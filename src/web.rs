use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json},
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info};

use crate::db::Database;

#[derive(Clone)]
pub struct AppState {
    pub db_path: PathBuf,
}

pub async fn run_server(state: AppState, host: &str, port: u16) {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/", get(index_html))
        .route("/api/messages", get(list_messages))
        .layer(cors)
        .with_state(Arc::new(state));

    let addr = format!("{}:{}", host, port);
    info!("Admin panel starting on http://{}", addr);

    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to bind web server to {}: {}", addr, e);
            return;
        }
    };

    if let Err(e) = axum::serve(listener, app).await {
        error!("Web server error: {}", e);
    }
}

// ============ API ============

/// One inbound message plus the reply that went back out, if any.
#[derive(Serialize)]
struct Row {
    id: i64,
    sender: String,
    text: String,
    date_received: i64,
    processed_at: Option<i64>,
    status: String,
    /// Text of Claude's reply, resolved through `response_guid`.
    reply: Option<String>,
    /// Why a message failed, or why it was judged not for Claude.
    reason: Option<String>,
}

#[derive(Serialize)]
struct MessagesResponse {
    data: Vec<Row>,
    total: i64,
    limit: i64,
    offset: i64,
    /// Per-filter totals for the whole log, independent of the current filter.
    counts: HashMap<String, i64>,
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
    /// One of: replied, failed, waiting, ignored. Anything else means "all".
    status: Option<String>,
    /// Substring match against the message text.
    q: Option<String>,
}

fn default_limit() -> i64 {
    100
}

/// Map a UI filter onto the raw `status` values stored in the database.
fn statuses_for_filter(filter: &str) -> Option<&'static [&'static str]> {
    match filter {
        "replied" => Some(&["replied"]),
        "failed" => Some(&["failed"]),
        "waiting" => Some(&["pending", "processing"]),
        "ignored" => Some(&["not_for_claude", "skipped"]),
        _ => None,
    }
}

/// Bucket a raw status into the same groups the filter pills use.
fn bucket_for_status(status: &str) -> &'static str {
    match status {
        "replied" => "replied",
        "failed" => "failed",
        "pending" | "processing" => "waiting",
        _ => "ignored",
    }
}

async fn list_messages(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> impl IntoResponse {
    let db_path = state.db_path.clone();
    let limit = query.limit.clamp(1, 500);
    let offset = query.offset.max(0);
    let filter = query.status.unwrap_or_default();
    let search = query.q.unwrap_or_default();

    let result = tokio::task::spawn_blocking(move || {
        let db = Database::open(&db_path)?;

        // Only inbound messages. Claude's own outbound rows are reached
        // through response_guid, never listed on their own.
        let mut clauses = vec!["m.is_from_me = 0".to_string()];
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(statuses) = statuses_for_filter(&filter) {
            let placeholders = vec!["?"; statuses.len()].join(", ");
            clauses.push(format!("m.status IN ({})", placeholders));
            for s in statuses {
                params.push(Box::new(s.to_string()));
            }
        }

        let trimmed = search.trim();
        if !trimmed.is_empty() {
            // ESCAPE so a literal % or _ in the query doesn't act as a wildcard.
            clauses.push("m.text LIKE ? ESCAPE '\\'".to_string());
            let escaped = trimmed
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            params.push(Box::new(format!("%{}%", escaped)));
        }

        let where_clause = format!("WHERE {}", clauses.join(" AND "));

        let count_sql = format!("SELECT COUNT(*) FROM messages m {}", where_clause);
        let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let total: i64 = db
            .conn()
            .query_row(&count_sql, param_refs.as_slice(), |row| row.get(0))?;

        // LEFT JOIN pulls the reply text straight from the outbound row that
        // response_guid points at, so the panel never needs the transcripts DB.
        let sql = format!(
            r#"SELECT m.id, m.sender, m.text, m.date_received, m.processed_at, m.status,
                      m.gemini_reason, m.error_reason, r.text
               FROM messages m
               LEFT JOIN messages r ON r.guid = m.response_guid
               {}
               ORDER BY m.date_received DESC
               LIMIT ? OFFSET ?"#,
            where_clause
        );

        let mut all_params: Vec<Box<dyn rusqlite::ToSql>> = params;
        all_params.push(Box::new(limit));
        all_params.push(Box::new(offset));
        let param_refs: Vec<&dyn rusqlite::ToSql> = all_params.iter().map(|p| p.as_ref()).collect();

        let mut stmt = db.conn().prepare(&sql)?;
        let rows = stmt
            .query_map(param_refs.as_slice(), |row| {
                let status: String = row.get(5)?;
                let gemini_reason: Option<String> = row.get(6)?;
                let error_reason: Option<String> = row.get(7)?;

                // A failure explains itself with error_reason; a message Gemini
                // held back explains itself with gemini_reason.
                let reason = match status.as_str() {
                    "failed" => error_reason,
                    "not_for_claude" => gemini_reason,
                    _ => None,
                };

                Ok(Row {
                    id: row.get(0)?,
                    sender: row.get(1)?,
                    text: row.get(2)?,
                    date_received: row.get(3)?,
                    processed_at: row.get(4)?,
                    status,
                    reply: row.get(8)?,
                    reason,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        // Whole-log tallies, so the pills keep their numbers while a filter is on.
        let mut counts: HashMap<String, i64> = HashMap::new();
        counts.insert("all".to_string(), 0);
        for key in ["replied", "failed", "waiting", "ignored"] {
            counts.insert(key.to_string(), 0);
        }
        let mut count_stmt = db
            .conn()
            .prepare("SELECT status, COUNT(*) FROM messages WHERE is_from_me = 0 GROUP BY status")?;
        let tallies = count_stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for tally in tallies {
            let (status, n) = tally?;
            *counts.entry(bucket_for_status(&status).to_string()).or_insert(0) += n;
            *counts.entry("all".to_string()).or_insert(0) += n;
        }

        Ok::<_, crate::error::SinkError>(MessagesResponse {
            data: rows,
            total,
            limit,
            offset,
            counts,
        })
    })
    .await;

    match result {
        Ok(Ok(resp)) => Json(resp).into_response(),
        Ok(Err(e)) => {
            error!("List messages error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
        Err(e) => {
            error!("List messages task error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
    }
}

async fn index_html() -> Html<&'static str> {
    Html(ADMIN_HTML)
}

const ADMIN_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>sink</title>
<style>
  :root {
    --paper:  #FCFBF9;
    --card:   #FFFFFF;
    --ink:    #141821;
    --muted:  #69707D;
    --rule:   #E4E6EB;
    --signal: #0B84FF;
    --alert:  #C0392B;
    --hold:   #B07A12;
    --quiet:  #9AA1AC;

    --sans: ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
    --mono: ui-monospace, "SF Mono", "JetBrains Mono", "Cascadia Code", Menlo, Consolas, monospace;
  }

  @media (prefers-color-scheme: dark) {
    :root {
      --paper:  #0E1116;
      --card:   #151A21;
      --ink:    #E7EAEF;
      --muted:  #98A0AC;
      --rule:   #242B35;
      --signal: #3B9DFF;
      --alert:  #E5705F;
      --hold:   #D9A441;
      --quiet:  #6B7381;
    }
  }

  * { box-sizing: border-box; }

  body {
    margin: 0;
    background: var(--paper);
    color: var(--ink);
    font-family: var(--sans);
    font-size: 15px;
    line-height: 1.5;
    -webkit-font-smoothing: antialiased;
  }

  .shell { max-width: 1000px; margin: 0 auto; padding: 0 20px 96px; }

  /* ---- header ---------------------------------------------------- */

  header {
    display: flex;
    align-items: baseline;
    gap: 14px;
    padding: 34px 0 20px;
    flex-wrap: wrap;
  }

  h1 {
    font-family: var(--mono);
    font-size: 19px;
    font-weight: 600;
    letter-spacing: -0.02em;
    margin: 0;
  }
  /* The daemon pipes iMessage into Claude; the arrow says so without a tagline. */
  h1::after {
    content: " ⟶ claude";
    color: var(--quiet);
    font-weight: 400;
  }

  .tally {
    font-family: var(--mono);
    font-size: 12.5px;
    color: var(--muted);
    margin-right: auto;
  }

  button.ghost {
    font-family: var(--mono);
    font-size: 12.5px;
    color: var(--muted);
    background: none;
    border: 1px solid var(--rule);
    border-radius: 7px;
    padding: 5px 11px;
    cursor: pointer;
  }
  button.ghost:hover { color: var(--ink); border-color: var(--quiet); }

  /* ---- controls -------------------------------------------------- */

  .controls {
    display: flex;
    gap: 10px;
    align-items: center;
    flex-wrap: wrap;
    padding-bottom: 16px;
  }

  #search {
    flex: 1 1 220px;
    min-width: 0;
    font-family: var(--sans);
    font-size: 14px;
    color: var(--ink);
    background: var(--card);
    border: 1px solid var(--rule);
    border-radius: 8px;
    padding: 9px 12px;
  }
  #search::placeholder { color: var(--quiet); }
  #search:focus-visible { outline: 2px solid var(--signal); outline-offset: 1px; border-color: transparent; }

  .pills { display: flex; gap: 6px; flex-wrap: wrap; }

  .pill {
    font-family: var(--mono);
    font-size: 12.5px;
    color: var(--muted);
    background: none;
    border: 1px solid var(--rule);
    border-radius: 999px;
    padding: 6px 13px;
    cursor: pointer;
    white-space: nowrap;
  }
  .pill:hover { color: var(--ink); }
  .pill[aria-pressed="true"] {
    color: var(--paper);
    background: var(--ink);
    border-color: var(--ink);
  }
  .pill .n { opacity: 0.6; margin-left: 5px; }

  /* ---- the list -------------------------------------------------- */

  .log {
    border: 1px solid var(--rule);
    border-radius: 12px;
    background: var(--card);
    overflow: hidden;
  }

  .row + .row { border-top: 1px solid var(--rule); }

  .row-head {
    display: grid;
    grid-template-columns: 3px 112px 152px 1fr auto;
    align-items: center;
    gap: 14px;
    width: 100%;
    padding: 0 16px 0 0;
    background: none;
    border: 0;
    font: inherit;
    color: inherit;
    text-align: left;
    cursor: pointer;
  }
  .row-head:hover { background: color-mix(in srgb, var(--signal) 5%, transparent); }
  .row-head:focus-visible { outline: 2px solid var(--signal); outline-offset: -2px; }

  /* The signature: a status edge running the length of the log, so the
     health of every message reads in one vertical scan. */
  .edge { align-self: stretch; background: var(--quiet); }
  .row[data-tone="replied"] .edge { background: var(--signal); }
  .row[data-tone="failed"]  .edge { background: var(--alert); }
  .row[data-tone="waiting"] .edge { background: var(--hold); }

  .ts, .who {
    font-family: var(--mono);
    font-size: 12px;
    color: var(--muted);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .ts { padding: 14px 0 14px 13px; }

  /* Human words get the prose face; machine metadata stays monospaced. */
  .what {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    padding: 14px 0;
  }
  .row.open .what { white-space: normal; }
  .what:empty::before { content: "(no text — attachment or link)"; color: var(--quiet); }

  .chip {
    font-family: var(--mono);
    font-size: 11px;
    letter-spacing: 0.02em;
    color: var(--quiet);
    white-space: nowrap;
  }
  .row[data-tone="replied"] .chip { color: var(--signal); }
  .row[data-tone="failed"]  .chip { color: var(--alert); }
  .row[data-tone="waiting"] .chip { color: var(--hold); }

  /* ---- expanded body --------------------------------------------- */

  .row-body { padding: 0 16px 18px 32px; }

  /* The reply hangs off a blue gutter — the same blue it left as. */
  .reply {
    border-left: 2px solid var(--signal);
    padding-left: 14px;
  }
  .reply .lat {
    display: block;
    font-family: var(--mono);
    font-size: 11.5px;
    color: var(--muted);
    margin-bottom: 6px;
  }
  .reply p {
    margin: 0;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }

  .note {
    border-left: 2px solid var(--rule);
    padding-left: 14px;
    color: var(--muted);
    font-size: 14px;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .note.bad { border-left-color: var(--alert); }
  .note .lat {
    display: block;
    font-family: var(--mono);
    font-size: 11.5px;
    color: var(--muted);
    margin-bottom: 6px;
  }
  .note code {
    font-family: var(--mono);
    font-size: 12.5px;
  }

  /* ---- states ---------------------------------------------------- */

  .empty, .status-line {
    text-align: center;
    color: var(--muted);
    font-family: var(--mono);
    font-size: 13px;
    padding: 44px 20px;
  }
  .empty button {
    display: block;
    margin: 12px auto 0;
    font-family: var(--mono);
    font-size: 13px;
    color: var(--signal);
    background: none;
    border: 0;
    cursor: pointer;
    text-decoration: underline;
  }

  .more { text-align: center; padding: 18px 0 0; }

  @media (max-width: 720px) {
    .row-head {
      grid-template-columns: 3px 1fr auto;
      grid-template-areas:
        "edge ts   chip"
        "edge who  who"
        "edge what what";
      gap: 0 12px;
      padding-bottom: 12px;
    }
    .edge { grid-area: edge; }
    .ts   { grid-area: ts;   padding: 12px 0 0 13px; }
    .who  { grid-area: who;  padding-left: 13px; }
    .what { grid-area: what; padding: 4px 0 0 13px; white-space: normal; }
    .chip { grid-area: chip; padding-top: 12px; }
    .row-body { padding-left: 16px; }
  }

  @media (prefers-reduced-motion: no-preference) {
    .row-head { transition: background 120ms ease; }
  }
</style>
</head>
<body>
<div class="shell">

  <header>
    <h1>sink</h1>
    <span class="tally" id="tally">loading…</span>
    <button class="ghost" id="refresh">refresh</button>
  </header>

  <div class="controls">
    <input id="search" type="search" placeholder="Search message text" autocomplete="off" aria-label="Search message text">
    <div class="pills" id="pills" role="group" aria-label="Filter by outcome"></div>
  </div>

  <div class="log" id="log">
    <div class="status-line">Loading messages…</div>
  </div>

  <div class="more" id="more"></div>

</div>

<script>
const FILTERS = [
  { key: '',        label: 'all' },
  { key: 'replied', label: 'replied' },
  { key: 'failed',  label: 'failed' },
  { key: 'waiting', label: 'waiting' },
  { key: 'ignored', label: 'ignored' },
];

// Raw db status -> what the operator calls it, and which edge colour it gets.
const STATUS = {
  replied:        { label: 'replied', tone: 'replied' },
  sent:           { label: 'sent',    tone: 'replied' },
  failed:         { label: 'failed',  tone: 'failed'  },
  pending:        { label: 'waiting', tone: 'waiting' },
  processing:     { label: 'running', tone: 'waiting' },
  skipped:        { label: 'skipped', tone: 'ignored' },
  not_for_claude: { label: 'ignored', tone: 'ignored' },
};

const PAGE = 100;

let filter = '';
let search = '';
let offset = 0;
let total  = 0;
let rows   = [];

const $log    = document.getElementById('log');
const $more   = document.getElementById('more');
const $tally  = document.getElementById('tally');
const $pills  = document.getElementById('pills');
const $search = document.getElementById('search');

const esc = (s) => String(s ?? '').replace(/[&<>"']/g, c => (
  { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]
));

// date_received / processed_at are milliseconds since epoch (BlueBubbles convention).
function when(ms) {
  const d = new Date(ms);
  const now = new Date();
  const time = d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', hour12: false });
  const sameDay = d.toDateString() === now.toDateString();
  if (sameDay) return time;
  return d.toLocaleDateString([], { month: 'short', day: '2-digit' }) + ' ' + time;
}

function took(ms) {
  if (ms < 0) return null;
  if (ms < 1000) return ms + 'ms';
  const sec = Math.round(ms / 1000);
  if (sec < 60) return sec + 's';
  if (sec < 3600) return Math.floor(sec / 60) + 'm ' + (sec % 60) + 's';
  return Math.floor(sec / 3600) + 'h ' + Math.floor((sec % 3600) / 60) + 'm';
}

function body(r) {
  const meta = STATUS[r.status] || { label: r.status, tone: 'ignored' };
  const lat = (r.processed_at && r.date_received) ? took(r.processed_at - r.date_received) : null;
  const stamp = lat ? `${meta.label} in ${lat}` : meta.label;

  // The row header un-truncates on open, so the inbound text is already in
  // full view above — the body carries only what came back.
  if (r.reply) {
    return `<div class="reply"><span class="lat">${esc(stamp)}</span><p>${esc(r.reply)}</p></div>`;
  }
  if (r.reason) {
    const bad = r.status === 'failed' ? ' bad' : '';
    return `<div class="note${bad}"><span class="lat">${esc(stamp)}</span>${esc(r.reason)}</div>`;
  }
  if (r.status === 'failed') {
    return `<div class="note bad"><span class="lat">${esc(stamp)}</span>No reason recorded — this one failed before error tracking existed. Check <code>journalctl --user -u sink</code>.</div>`;
  }
  if (r.status === 'pending' || r.status === 'processing') {
    return `<div class="note"><span class="lat">${esc(meta.label)}</span>No reply yet.</div>`;
  }
  return `<div class="note"><span class="lat">${esc(meta.label)}</span>No reply was sent.</div>`;
}

function render() {
  if (!rows.length) {
    $log.innerHTML = `<div class="empty">No messages match this filter.` +
      (filter || search ? `<button id="clear">Show everything</button>` : '') + `</div>`;
    const clear = document.getElementById('clear');
    if (clear) clear.onclick = () => { filter = ''; search = ''; $search.value = ''; paint(); load(true); };
    $more.innerHTML = '';
    return;
  }

  $log.innerHTML = rows.map(r => {
    const meta = STATUS[r.status] || { label: r.status, tone: 'ignored' };
    return `<article class="row" data-tone="${meta.tone}">
      <button class="row-head" aria-expanded="false">
        <span class="edge" aria-hidden="true"></span>
        <time class="ts">${esc(when(r.date_received))}</time>
        <span class="who">${esc(r.sender)}</span>
        <span class="what">${esc(r.text)}</span>
        <span class="chip">${esc(meta.label)}</span>
      </button>
    </article>`;
  }).join('');

  [...$log.querySelectorAll('.row')].forEach((el, i) => {
    el.querySelector('.row-head').onclick = () => toggle(el, rows[i]);
  });

  $more.innerHTML = rows.length < total
    ? `<button class="ghost" id="load-more">Load ${Math.min(PAGE, total - rows.length)} more · ${rows.length} of ${total}</button>`
    : (total > PAGE ? `<span class="status-line">All ${total} shown.</span>` : '');
  const btn = document.getElementById('load-more');
  if (btn) btn.onclick = () => { offset += PAGE; load(false); };
}

function toggle(el, r) {
  const head = el.querySelector('.row-head');
  const open = el.classList.toggle('open');
  head.setAttribute('aria-expanded', open ? 'true' : 'false');
  if (open) {
    const div = document.createElement('div');
    div.className = 'row-body';
    div.innerHTML = body(r);
    el.appendChild(div);
  } else {
    const b = el.querySelector('.row-body');
    if (b) b.remove();
  }
}

function paint(counts) {
  if (counts) paint.counts = counts;
  const c = paint.counts || {};
  $pills.innerHTML = FILTERS.map(f => {
    const n = c[f.key || 'all'];
    return `<button class="pill" data-key="${f.key}" aria-pressed="${filter === f.key}">` +
      `${f.label}${n != null ? `<span class="n">${n}</span>` : ''}</button>`;
  }).join('');
  [...$pills.querySelectorAll('.pill')].forEach(p => {
    p.onclick = () => { filter = p.dataset.key; paint(); load(true); };
  });
}

async function load(reset) {
  if (reset) { offset = 0; rows = []; }
  const params = new URLSearchParams({ limit: PAGE, offset });
  if (filter) params.set('status', filter);
  if (search) params.set('q', search);

  try {
    const resp = await fetch('/api/messages?' + params);
    if (!resp.ok) throw new Error('HTTP ' + resp.status);
    const json = await resp.json();
    rows = reset ? json.data : rows.concat(json.data);
    total = json.total;
    paint(json.counts);
    const c = json.counts || {};
    $tally.textContent = `${c.all ?? 0} messages · ${c.replied ?? 0} replied · ${c.failed ?? 0} failed`;
    render();
  } catch (e) {
    $log.innerHTML = `<div class="empty">Could not reach the daemon (${esc(e.message)}).<br>` +
      `Check <span style="font-family:var(--mono)">systemctl --user status sink</span>.</div>`;
    $more.innerHTML = '';
  }
}

let timer;
$search.addEventListener('input', e => {
  clearTimeout(timer);
  timer = setTimeout(() => { search = e.target.value.trim(); load(true); }, 220);
});

document.getElementById('refresh').onclick = () => load(true);

paint();
load(true);
</script>
</body>
</html>
"##;
