use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json},
    routing::{delete, get, put},
    Router,
};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info};

use crate::db::Database;
use crate::followups::FollowupsDb;
use crate::transcripts::TranscriptsDb;

#[derive(Clone)]
pub struct AppState {
    pub db_path: PathBuf,
    pub transcripts_db_path: PathBuf,
    pub followups_db_path: PathBuf,
}

pub async fn run_server(state: AppState, host: &str, port: u16) {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/", get(index_html))
        .route("/api/stats", get(get_stats))
        .route("/api/messages", get(list_messages))
        .route("/api/messages/:id", get(get_message))
        .route("/api/transcripts", get(list_transcripts))
        .route("/api/transcripts/:id", get(get_transcript))
        .route("/api/followups", get(list_followups))
        .route(
            "/api/followups/:id",
            get(get_followup).put(update_followup).delete(delete_followup),
        )
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

// ============ API Response Types ============

#[derive(Serialize)]
struct StatsResponse {
    messages: MessageStats,
    followups: FollowupStats,
    transcripts: TranscriptStats,
    costs: CostStats,
}

#[derive(Serialize)]
struct MessageStats {
    pending: i64,
    processing: i64,
    replied: i64,
    failed: i64,
    skipped: i64,
}

#[derive(Serialize)]
struct FollowupStats {
    pending: i64,
    sent: i64,
    dismissed: i64,
}

#[derive(Serialize)]
struct TranscriptStats {
    total: i64,
    today: i64,
}

#[derive(Serialize)]
struct CostStats {
    total_usd: f64,
    today_usd: f64,
}

#[derive(Serialize)]
struct PaginatedResponse<T> {
    data: T,
    total: i64,
    limit: i64,
    offset: i64,
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
    status: Option<String>,
    chat_guid: Option<String>,
}

fn default_limit() -> i64 {
    50
}

#[derive(Deserialize)]
struct UpdateFollowupRequest {
    description: Option<String>,
    notify_at: Option<i64>,
    status: Option<String>,
}

// ============ API Handlers ============

async fn get_stats(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let db_path = state.db_path.clone();
    let transcripts_path = state.transcripts_db_path.clone();
    let followups_path = state.followups_db_path.clone();

    let result = tokio::task::spawn_blocking(move || {
        // Get message stats
        let db = Database::open(&db_path)?;
        let msg_stats = get_message_stats(&db)?;

        // Get transcript stats
        let transcripts_db = TranscriptsDb::open(&transcripts_path)?;
        let transcript_stats = get_transcript_stats(&transcripts_db)?;

        // Get followup stats
        let followups_db = FollowupsDb::open(&followups_path)?;
        let followup_stats = get_followup_stats(&followups_db)?;

        Ok::<_, crate::error::SinkError>(StatsResponse {
            messages: msg_stats,
            followups: followup_stats,
            transcripts: transcript_stats.0,
            costs: transcript_stats.1,
        })
    })
    .await;

    match result {
        Ok(Ok(stats)) => Json(stats).into_response(),
        Ok(Err(e)) => {
            error!("Stats error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
        Err(e) => {
            error!("Stats task error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
    }
}

fn get_message_stats(db: &Database) -> crate::error::Result<MessageStats> {
    // Only count inbound messages (is_from_me = 0) for dashboard stats
    let count_status = |status: &str| -> crate::error::Result<i64> {
        let count: i64 = db.conn().query_row(
            "SELECT COUNT(*) FROM messages WHERE status = ? AND is_from_me = 0",
            [status],
            |row| row.get(0),
        )?;
        Ok(count)
    };

    Ok(MessageStats {
        pending: count_status("pending")?,
        processing: count_status("processing")?,
        replied: count_status("replied")?,
        failed: count_status("failed")?,
        skipped: count_status("skipped")?,
    })
}

fn get_transcript_stats(db: &TranscriptsDb) -> crate::error::Result<(TranscriptStats, CostStats)> {
    let total: i64 = db.conn().query_row(
        "SELECT COUNT(*) FROM transcripts",
        [],
        |row| row.get(0),
    )?;

    let today_start = chrono::Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
        .timestamp_millis();

    let today: i64 = db.conn().query_row(
        "SELECT COUNT(*) FROM transcripts WHERE created_at >= ?",
        [today_start],
        |row| row.get(0),
    )?;

    let total_cost: f64 = db.conn().query_row(
        "SELECT COALESCE(SUM(cost_usd), 0) FROM transcripts",
        [],
        |row| row.get(0),
    )?;

    let today_cost: f64 = db.conn().query_row(
        "SELECT COALESCE(SUM(cost_usd), 0) FROM transcripts WHERE created_at >= ?",
        [today_start],
        |row| row.get(0),
    )?;

    Ok((
        TranscriptStats { total, today },
        CostStats {
            total_usd: total_cost,
            today_usd: today_cost,
        },
    ))
}

fn get_followup_stats(db: &FollowupsDb) -> crate::error::Result<FollowupStats> {
    let count_status = |status: &str| -> crate::error::Result<i64> {
        let count: i64 = db.conn().query_row(
            "SELECT COUNT(*) FROM followups WHERE status = ?",
            [status],
            |row| row.get(0),
        )?;
        Ok(count)
    };

    Ok(FollowupStats {
        pending: count_status("pending")?,
        sent: count_status("sent")?,
        dismissed: count_status("dismissed")?,
    })
}

async fn list_messages(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> impl IntoResponse {
    let db_path = state.db_path.clone();
    let limit = query.limit;
    let offset = query.offset;
    let status = query.status;
    let chat_guid = query.chat_guid;

    let result = tokio::task::spawn_blocking(move || {
        let db = Database::open(&db_path)?;

        let (where_clause, params): (String, Vec<Box<dyn rusqlite::ToSql>>) = {
            let mut clauses = Vec::new();
            let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

            // Only show inbound messages by default (hide Claude's outbound responses)
            clauses.push("is_from_me = 0");

            if let Some(ref s) = status {
                clauses.push("status = ?");
                params.push(Box::new(s.clone()));
            }
            if let Some(ref c) = chat_guid {
                clauses.push("chat_guid = ?");
                params.push(Box::new(c.clone()));
            }

            (format!("WHERE {}", clauses.join(" AND ")), params)
        };

        // Get total count
        let count_sql = format!("SELECT COUNT(*) FROM messages {}", where_clause);
        let total: i64 = if params.is_empty() {
            db.conn().query_row(&count_sql, [], |row| row.get(0))?
        } else {
            let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
            db.conn().query_row(&count_sql, param_refs.as_slice(), |row| row.get(0))?
        };

        // Get messages
        let sql = format!(
            r#"SELECT id, guid, chat_guid, sender, text, date_received,
                      processed_at, response_guid, session_id, status, is_from_me, gemini_reason
               FROM messages {} ORDER BY date_received DESC LIMIT ? OFFSET ?"#,
            where_clause
        );

        let mut all_params: Vec<Box<dyn rusqlite::ToSql>> = params;
        all_params.push(Box::new(limit));
        all_params.push(Box::new(offset));
        let param_refs: Vec<&dyn rusqlite::ToSql> = all_params.iter().map(|p| p.as_ref()).collect();

        let mut stmt = db.conn().prepare(&sql)?;
        let messages = stmt
            .query_map(param_refs.as_slice(), |row| {
                Ok(crate::db::Message {
                    id: Some(row.get(0)?),
                    guid: row.get(1)?,
                    chat_guid: row.get(2)?,
                    sender: row.get(3)?,
                    text: row.get(4)?,
                    date_received: row.get(5)?,
                    processed_at: row.get(6)?,
                    response_guid: row.get(7)?,
                    session_id: row.get(8)?,
                    status: row.get(9)?,
                    is_from_me: row.get::<_, i32>(10)? != 0,
                    gemini_reason: row.get(11)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok::<_, crate::error::SinkError>(PaginatedResponse {
            data: messages,
            total,
            limit,
            offset,
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

async fn get_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let db_path = state.db_path.clone();

    let result = tokio::task::spawn_blocking(move || {
        let db = Database::open(&db_path)?;
        let msg: Option<crate::db::Message> = db.conn().query_row(
            r#"SELECT id, guid, chat_guid, sender, text, date_received,
                      processed_at, response_guid, session_id, status, is_from_me, gemini_reason
               FROM messages WHERE id = ?"#,
            [id],
            |row| {
                Ok(crate::db::Message {
                    id: Some(row.get(0)?),
                    guid: row.get(1)?,
                    chat_guid: row.get(2)?,
                    sender: row.get(3)?,
                    text: row.get(4)?,
                    date_received: row.get(5)?,
                    processed_at: row.get(6)?,
                    response_guid: row.get(7)?,
                    session_id: row.get(8)?,
                    status: row.get(9)?,
                    is_from_me: row.get::<_, i32>(10)? != 0,
                    gemini_reason: row.get(11)?,
                })
            },
        ).optional()?;
        Ok::<_, crate::error::SinkError>(msg)
    })
    .await;

    match result {
        Ok(Ok(Some(msg))) => Json::<crate::db::Message>(msg).into_response(),
        Ok(Ok(None)) => (StatusCode::NOT_FOUND, "Message not found").into_response(),
        Ok(Err(e)) => {
            error!("Get message error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
        Err(e) => {
            error!("Get message task error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
    }
}

async fn list_transcripts(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> impl IntoResponse {
    let db_path = state.transcripts_db_path.clone();
    let limit = query.limit;
    let offset = query.offset;
    let chat_guid = query.chat_guid;

    let result = tokio::task::spawn_blocking(move || {
        let db = TranscriptsDb::open(&db_path)?;

        let (where_clause, chat_param) = if let Some(ref c) = chat_guid {
            ("WHERE chat_guid = ?", Some(c.clone()))
        } else {
            ("", None)
        };

        // Get total count
        let count_sql = format!("SELECT COUNT(*) FROM transcripts {}", where_clause);
        let total: i64 = if let Some(ref c) = chat_param {
            db.conn().query_row(&count_sql, [c], |row| row.get(0))?
        } else {
            db.conn().query_row(&count_sql, [], |row| row.get(0))?
        };

        // Get transcripts using a helper to avoid borrow issues
        fn map_transcript_row(row: &rusqlite::Row) -> rusqlite::Result<crate::transcripts::Transcript> {
            Ok(crate::transcripts::Transcript {
                id: Some(row.get(0)?),
                message_guid: row.get(1)?,
                chat_guid: row.get(2)?,
                session_id: row.get(3)?,
                prompt_sent: row.get(4)?,
                raw_stdout: row.get(5)?,
                raw_stderr: row.get(6)?,
                exit_code: row.get(7)?,
                messages_json: row.get(8)?,
                tool_calls_json: row.get(9)?,
                cost_usd: row.get(10)?,
                input_tokens: row.get(11)?,
                output_tokens: row.get(12)?,
                claude_model: row.get(13)?,
                working_dir: row.get(14)?,
                duration_ms: row.get(15)?,
                created_at: row.get(16)?,
            })
        }

        let sql = format!(
            r#"SELECT id, message_guid, chat_guid, session_id, prompt_sent, raw_stdout,
                      raw_stderr, exit_code, messages_json, tool_calls_json, cost_usd,
                      input_tokens, output_tokens, claude_model, working_dir, duration_ms, created_at
               FROM transcripts {} ORDER BY created_at DESC LIMIT ? OFFSET ?"#,
            where_clause
        );

        let transcripts: Vec<crate::transcripts::Transcript> = if let Some(ref c) = chat_param {
            let mut stmt = db.conn().prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params![c, limit, offset], map_transcript_row)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            let mut stmt = db.conn().prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params![limit, offset], map_transcript_row)?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };

        Ok::<_, crate::error::SinkError>(PaginatedResponse {
            data: transcripts,
            total,
            limit,
            offset,
        })
    })
    .await;

    match result {
        Ok(Ok(resp)) => Json(resp).into_response(),
        Ok(Err(e)) => {
            error!("List transcripts error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
        Err(e) => {
            error!("List transcripts task error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
    }
}

async fn get_transcript(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let db_path = state.transcripts_db_path.clone();

    let result = tokio::task::spawn_blocking(move || {
        let db = TranscriptsDb::open(&db_path)?;
        let transcript: Option<crate::transcripts::Transcript> = db.conn().query_row(
            r#"SELECT id, message_guid, chat_guid, session_id, prompt_sent, raw_stdout,
                      raw_stderr, exit_code, messages_json, tool_calls_json, cost_usd,
                      input_tokens, output_tokens, claude_model, working_dir, duration_ms, created_at
               FROM transcripts WHERE id = ?"#,
            [id],
            |row| {
                Ok(crate::transcripts::Transcript {
                    id: Some(row.get(0)?),
                    message_guid: row.get(1)?,
                    chat_guid: row.get(2)?,
                    session_id: row.get(3)?,
                    prompt_sent: row.get(4)?,
                    raw_stdout: row.get(5)?,
                    raw_stderr: row.get(6)?,
                    exit_code: row.get(7)?,
                    messages_json: row.get(8)?,
                    tool_calls_json: row.get(9)?,
                    cost_usd: row.get(10)?,
                    input_tokens: row.get(11)?,
                    output_tokens: row.get(12)?,
                    claude_model: row.get(13)?,
                    working_dir: row.get(14)?,
                    duration_ms: row.get(15)?,
                    created_at: row.get(16)?,
                })
            },
        ).optional()?;
        Ok::<_, crate::error::SinkError>(transcript)
    })
    .await;

    match result {
        Ok(Ok(Some(t))) => Json::<crate::transcripts::Transcript>(t).into_response(),
        Ok(Ok(None)) => (StatusCode::NOT_FOUND, "Transcript not found").into_response(),
        Ok(Err(e)) => {
            error!("Get transcript error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
        Err(e) => {
            error!("Get transcript task error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
    }
}

async fn list_followups(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ListQuery>,
) -> impl IntoResponse {
    let db_path = state.followups_db_path.clone();
    let status = query.status;

    let result = tokio::task::spawn_blocking(move || {
        let db = FollowupsDb::open(&db_path)?;
        let all = db.get_all_followups()?;

        let filtered: Vec<_> = if let Some(ref s) = status {
            all.into_iter().filter(|f| f.status == *s).collect()
        } else {
            all
        };

        Ok::<_, crate::error::SinkError>(filtered)
    })
    .await;

    match result {
        Ok(Ok(followups)) => Json(followups).into_response(),
        Ok(Err(e)) => {
            error!("List followups error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
        Err(e) => {
            error!("List followups task error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
    }
}

async fn get_followup(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let db_path = state.followups_db_path.clone();

    let result = tokio::task::spawn_blocking(move || {
        let db = FollowupsDb::open(&db_path)?;
        db.get_followup(id)
    })
    .await;

    match result {
        Ok(Ok(Some(f))) => Json(f).into_response(),
        Ok(Ok(None)) => (StatusCode::NOT_FOUND, "Followup not found").into_response(),
        Ok(Err(e)) => {
            error!("Get followup error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
        Err(e) => {
            error!("Get followup task error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
    }
}

async fn update_followup(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateFollowupRequest>,
) -> impl IntoResponse {
    let db_path = state.followups_db_path.clone();

    let result = tokio::task::spawn_blocking(move || {
        let db = FollowupsDb::open(&db_path)?;
        db.update_followup(
            id,
            req.description.as_deref(),
            req.notify_at,
            req.status.as_deref(),
        )?;
        db.get_followup(id)
    })
    .await;

    match result {
        Ok(Ok(Some(f))) => Json(f).into_response(),
        Ok(Ok(None)) => (StatusCode::NOT_FOUND, "Followup not found").into_response(),
        Ok(Err(e)) => {
            error!("Update followup error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
        Err(e) => {
            error!("Update followup task error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
    }
}

async fn delete_followup(
    State(state): State<Arc<AppState>>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let db_path = state.followups_db_path.clone();

    let result = tokio::task::spawn_blocking(move || {
        let db = FollowupsDb::open(&db_path)?;
        db.delete_followup(id)
    })
    .await;

    match result {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => {
            error!("Delete followup error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
        Err(e) => {
            error!("Delete followup task error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
    }
}

// ============ Frontend HTML ============

async fn index_html() -> Html<&'static str> {
    Html(ADMIN_HTML)
}

const ADMIN_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0, maximum-scale=1.0, user-scalable=no">
    <title>SINK://TERMINAL</title>
    <link rel="preconnect" href="https://fonts.googleapis.com">
    <link href="https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@400;500;700&display=swap" rel="stylesheet">
    <style>
        :root {
            --terminal-green: #00ff41;
            --terminal-green-dim: #00aa2a;
            --terminal-green-bright: #39ff14;
            --terminal-amber: #ffb000;
            --terminal-red: #ff0040;
            --terminal-cyan: #00ffff;
            --bg-black: #0a0a0a;
            --bg-dark: #0d1117;
            --bg-panel: #111518;
            --border-green: #003d10;
            --glow: 0 0 10px rgba(0,255,65,0.5), 0 0 20px rgba(0,255,65,0.3), 0 0 30px rgba(0,255,65,0.1);
            --glow-text: 0 0 8px rgba(0,255,65,0.8);
        }

        * { box-sizing: border-box; margin: 0; padding: 0; }

        html, body {
            font-family: 'JetBrains Mono', monospace;
            background: var(--bg-black);
            color: var(--terminal-green);
            min-height: 100vh;
            overflow-x: hidden;
            font-size: 14px;
        }

        /* Scanline overlay */
        body::before {
            content: '';
            position: fixed;
            top: 0; left: 0; right: 0; bottom: 0;
            background: repeating-linear-gradient(
                0deg,
                rgba(0,0,0,0.15) 0px,
                rgba(0,0,0,0.15) 1px,
                transparent 1px,
                transparent 2px
            );
            pointer-events: none;
            z-index: 9999;
        }

        /* CRT flicker animation */
        @keyframes flicker {
            0%, 100% { opacity: 1; }
            92% { opacity: 1; }
            93% { opacity: 0.8; }
            94% { opacity: 1; }
        }

        @keyframes pulse-glow {
            0%, 100% { box-shadow: var(--glow); }
            50% { box-shadow: 0 0 15px rgba(0,255,65,0.6), 0 0 30px rgba(0,255,65,0.4); }
        }

        @keyframes blink {
            0%, 50% { opacity: 1; }
            51%, 100% { opacity: 0; }
        }

        .container {
            max-width: 1400px;
            margin: 0 auto;
            padding: 12px;
            animation: flicker 8s infinite;
        }

        /* Header */
        .header {
            border: 1px solid var(--terminal-green-dim);
            background: var(--bg-panel);
            padding: 12px 16px;
            margin-bottom: 12px;
            display: flex;
            justify-content: space-between;
            align-items: center;
            flex-wrap: wrap;
            gap: 8px;
        }

        .logo {
            font-size: 1.1em;
            font-weight: 700;
            text-shadow: var(--glow-text);
            letter-spacing: 2px;
        }

        .logo::before { content: '> '; color: var(--terminal-green-dim); }

        .status-line {
            font-size: 0.75em;
            color: var(--terminal-green-dim);
        }

        .status-line .blink {
            display: inline-block;
            width: 8px;
            height: 14px;
            background: var(--terminal-green);
            margin-left: 4px;
            animation: blink 1s infinite;
        }

        /* Tabs */
        .tabs {
            display: flex;
            gap: 0;
            margin-bottom: 12px;
            border: 1px solid var(--border-green);
            background: var(--bg-panel);
            overflow-x: auto;
            -webkit-overflow-scrolling: touch;
        }

        .tab {
            flex: 1;
            min-width: 80px;
            padding: 14px 12px;
            background: transparent;
            border: none;
            border-right: 1px solid var(--border-green);
            color: var(--terminal-green-dim);
            font-family: inherit;
            font-size: 0.85em;
            cursor: pointer;
            transition: all 0.2s;
            text-transform: uppercase;
            letter-spacing: 1px;
            white-space: nowrap;
        }

        .tab:last-child { border-right: none; }

        .tab:hover {
            background: rgba(0,255,65,0.05);
            color: var(--terminal-green);
        }

        .tab.active {
            background: rgba(0,255,65,0.1);
            color: var(--terminal-green-bright);
            text-shadow: var(--glow-text);
            border-bottom: 2px solid var(--terminal-green);
        }

        /* Panels */
        .panel { display: none; }
        .panel.active { display: block; }

        /* Stats Grid */
        .stats-grid {
            display: grid;
            grid-template-columns: repeat(2, 1fr);
            gap: 12px;
            margin-bottom: 12px;
        }

        @media (min-width: 768px) {
            .stats-grid { grid-template-columns: repeat(4, 1fr); }
        }

        .stat-card {
            border: 1px solid var(--border-green);
            background: var(--bg-panel);
            padding: 16px;
            position: relative;
            overflow: hidden;
        }

        .stat-card::before {
            content: '';
            position: absolute;
            top: 0; left: 0;
            width: 100%; height: 2px;
            background: linear-gradient(90deg, transparent, var(--terminal-green), transparent);
        }

        .stat-label {
            font-size: 0.7em;
            color: var(--terminal-green-dim);
            text-transform: uppercase;
            letter-spacing: 1px;
            margin-bottom: 8px;
        }

        .stat-value {
            font-size: 1.8em;
            font-weight: 700;
            text-shadow: var(--glow-text);
        }

        .stat-value.warning { color: var(--terminal-amber); text-shadow: 0 0 8px rgba(255,176,0,0.8); }
        .stat-value.danger { color: var(--terminal-red); text-shadow: 0 0 8px rgba(255,0,64,0.8); }
        .stat-value.info { color: var(--terminal-cyan); text-shadow: 0 0 8px rgba(0,255,255,0.8); }

        /* Info Panels */
        .info-grid {
            display: grid;
            grid-template-columns: 1fr;
            gap: 12px;
        }

        @media (min-width: 768px) {
            .info-grid { grid-template-columns: repeat(2, 1fr); }
        }

        .info-panel {
            border: 1px solid var(--border-green);
            background: var(--bg-panel);
            padding: 16px;
        }

        .info-title {
            font-size: 0.8em;
            color: var(--terminal-green);
            text-transform: uppercase;
            letter-spacing: 1px;
            margin-bottom: 12px;
            padding-bottom: 8px;
            border-bottom: 1px solid var(--border-green);
        }

        .info-title::before { content: '// '; color: var(--terminal-green-dim); }

        .info-row {
            display: flex;
            justify-content: space-between;
            padding: 6px 0;
            font-size: 0.85em;
        }

        .info-row span:first-child { color: var(--terminal-green-dim); }
        .info-row span:last-child { text-shadow: var(--glow-text); }
        .info-row .success { color: var(--terminal-green-bright); }
        .info-row .error { color: var(--terminal-red); }
        .info-row .muted { color: #555; }

        /* Filter Bar */
        .filter-bar {
            margin-bottom: 12px;
            display: flex;
            gap: 12px;
            flex-wrap: wrap;
        }

        select, input[type="text"] {
            background: var(--bg-black);
            border: 1px solid var(--border-green);
            color: var(--terminal-green);
            padding: 12px 16px;
            font-family: inherit;
            font-size: 0.9em;
            min-width: 150px;
            appearance: none;
            cursor: pointer;
        }

        select:focus, input[type="text"]:focus {
            outline: none;
            border-color: var(--terminal-green);
            box-shadow: 0 0 10px rgba(0,255,65,0.3);
        }

        /* Tables */
        .table-wrap {
            border: 1px solid var(--border-green);
            background: var(--bg-panel);
            overflow-x: auto;
            -webkit-overflow-scrolling: touch;
        }

        table {
            width: 100%;
            min-width: 600px;
            border-collapse: collapse;
        }

        th {
            background: var(--bg-black);
            color: var(--terminal-green-dim);
            font-size: 0.7em;
            font-weight: 500;
            text-transform: uppercase;
            letter-spacing: 1px;
            padding: 14px 12px;
            text-align: left;
            border-bottom: 1px solid var(--border-green);
            white-space: nowrap;
        }

        td {
            padding: 12px;
            border-bottom: 1px solid rgba(0,61,16,0.5);
            font-size: 0.85em;
            vertical-align: top;
        }

        tr:hover td { background: rgba(0,255,65,0.03); }

        .cell-id { color: var(--terminal-green-dim); font-size: 0.8em; }
        .cell-text {
            max-width: 200px;
            overflow: hidden;
            text-overflow: ellipsis;
            white-space: nowrap;
        }
        @media (min-width: 768px) {
            .cell-text { max-width: 400px; }
        }
        .cell-time { color: var(--terminal-green-dim); font-size: 0.8em; white-space: nowrap; }
        .cell-cost { color: var(--terminal-green-bright); text-shadow: var(--glow-text); }

        /* Badges */
        .badge {
            display: inline-block;
            padding: 4px 10px;
            font-size: 0.7em;
            font-weight: 500;
            text-transform: uppercase;
            letter-spacing: 1px;
            border: 1px solid;
        }

        .badge-pending { color: var(--terminal-amber); border-color: var(--terminal-amber); background: rgba(255,176,0,0.1); }
        .badge-processing { color: var(--terminal-cyan); border-color: var(--terminal-cyan); background: rgba(0,255,255,0.1); animation: pulse-glow 2s infinite; }
        .badge-replied { color: var(--terminal-green); border-color: var(--terminal-green); background: rgba(0,255,65,0.1); }
        .badge-failed { color: var(--terminal-red); border-color: var(--terminal-red); background: rgba(255,0,64,0.1); }
        .badge-skipped, .badge-dismissed { color: #555; border-color: #333; background: rgba(50,50,50,0.3); }
        .badge-sent { color: var(--terminal-green); border-color: var(--terminal-green); background: rgba(0,255,65,0.1); }

        /* Buttons */
        .btn {
            background: transparent;
            border: 1px solid var(--terminal-green-dim);
            color: var(--terminal-green);
            padding: 12px 20px;
            font-family: inherit;
            font-size: 0.85em;
            cursor: pointer;
            transition: all 0.2s;
            text-transform: uppercase;
            letter-spacing: 1px;
            min-height: 48px;
            min-width: 100px;
        }

        .btn:hover {
            background: rgba(0,255,65,0.1);
            border-color: var(--terminal-green);
            box-shadow: var(--glow);
        }

        .btn:active {
            transform: scale(0.98);
        }

        .btn-danger {
            border-color: var(--terminal-red);
            color: var(--terminal-red);
        }

        .btn-danger:hover {
            background: rgba(255,0,64,0.1);
            box-shadow: 0 0 10px rgba(255,0,64,0.5);
        }

        .btn-primary {
            background: rgba(0,255,65,0.15);
            border-color: var(--terminal-green);
        }

        /* Action Buttons (Touch-friendly) */
        .actions {
            display: flex;
            gap: 8px;
            flex-wrap: wrap;
        }

        .action-btn {
            background: transparent;
            border: 1px solid;
            padding: 10px 16px;
            font-family: inherit;
            font-size: 0.75em;
            cursor: pointer;
            transition: all 0.2s;
            text-transform: uppercase;
            letter-spacing: 1px;
            min-height: 44px;
            min-width: 70px;
            display: inline-flex;
            align-items: center;
            justify-content: center;
        }

        .action-btn.edit {
            color: var(--terminal-cyan);
            border-color: var(--terminal-cyan);
        }

        .action-btn.edit:hover {
            background: rgba(0,255,255,0.1);
            box-shadow: 0 0 10px rgba(0,255,255,0.5);
        }

        .action-btn.delete {
            color: var(--terminal-red);
            border-color: var(--terminal-red);
        }

        .action-btn.delete:hover {
            background: rgba(255,0,64,0.1);
            box-shadow: 0 0 10px rgba(255,0,64,0.5);
        }

        .action-btn.view {
            color: var(--terminal-green);
            border-color: var(--terminal-green);
        }

        .action-btn.view:hover {
            background: rgba(0,255,65,0.1);
            box-shadow: var(--glow);
        }

        /* Pagination */
        .pagination {
            display: flex;
            justify-content: space-between;
            align-items: center;
            padding: 12px 0;
            flex-wrap: wrap;
            gap: 12px;
        }

        .pagination-info {
            font-size: 0.8em;
            color: var(--terminal-green-dim);
        }

        .pagination-btns {
            display: flex;
            gap: 8px;
        }

        /* Modal */
        .modal {
            position: fixed;
            top: 0; left: 0; right: 0; bottom: 0;
            background: rgba(0,0,0,0.9);
            display: none;
            align-items: flex-start;
            justify-content: center;
            z-index: 1000;
            padding: 20px;
            overflow-y: auto;
        }

        .modal.open {
            display: flex;
        }

        .modal-content {
            background: var(--bg-dark);
            border: 1px solid var(--terminal-green);
            box-shadow: var(--glow);
            width: 100%;
            max-width: 800px;
            margin: 20px 0;
            animation: modalIn 0.2s ease;
        }

        @keyframes modalIn {
            from { opacity: 0; transform: translateY(-20px); }
            to { opacity: 1; transform: translateY(0); }
        }

        .modal-header {
            display: flex;
            justify-content: space-between;
            align-items: center;
            padding: 16px;
            border-bottom: 1px solid var(--border-green);
            background: var(--bg-black);
        }

        .modal-title {
            font-size: 0.9em;
            text-transform: uppercase;
            letter-spacing: 2px;
            text-shadow: var(--glow-text);
        }

        .modal-title::before { content: '[ '; color: var(--terminal-green-dim); }
        .modal-title::after { content: ' ]'; color: var(--terminal-green-dim); }

        .modal-close {
            background: none;
            border: 1px solid var(--terminal-red);
            color: var(--terminal-red);
            width: 40px;
            height: 40px;
            font-size: 1.2em;
            cursor: pointer;
            transition: all 0.2s;
        }

        .modal-close:hover {
            background: rgba(255,0,64,0.2);
            box-shadow: 0 0 10px rgba(255,0,64,0.5);
        }

        .modal-body {
            padding: 20px;
            max-height: 70vh;
            overflow-y: auto;
        }

        .modal-footer {
            display: flex;
            justify-content: flex-end;
            gap: 12px;
            padding: 16px;
            border-top: 1px solid var(--border-green);
            background: var(--bg-black);
        }

        /* Form elements in modal */
        .form-group {
            margin-bottom: 20px;
        }

        .form-label {
            display: block;
            font-size: 0.75em;
            color: var(--terminal-green-dim);
            text-transform: uppercase;
            letter-spacing: 1px;
            margin-bottom: 8px;
        }

        .form-label::before { content: '> '; }

        .form-input {
            width: 100%;
            background: var(--bg-black);
            border: 1px solid var(--border-green);
            color: var(--terminal-green);
            padding: 14px;
            font-family: inherit;
            font-size: 1em;
        }

        .form-input:focus {
            outline: none;
            border-color: var(--terminal-green);
            box-shadow: 0 0 10px rgba(0,255,65,0.3);
        }

        /* Pre/Code blocks */
        pre {
            background: var(--bg-black);
            border: 1px solid var(--border-green);
            padding: 16px;
            overflow-x: auto;
            font-size: 0.85em;
            line-height: 1.5;
            white-space: pre-wrap;
            word-break: break-word;
        }

        .detail-row {
            padding: 10px 0;
            border-bottom: 1px solid var(--border-green);
        }

        .detail-label {
            font-size: 0.75em;
            color: var(--terminal-green-dim);
            text-transform: uppercase;
            letter-spacing: 1px;
        }

        .detail-value {
            margin-top: 4px;
            word-break: break-all;
        }

        .detail-section {
            margin-top: 20px;
        }

        .detail-section-title {
            font-size: 0.8em;
            color: var(--terminal-green);
            text-transform: uppercase;
            letter-spacing: 1px;
            margin-bottom: 10px;
            padding-bottom: 6px;
            border-bottom: 1px solid var(--border-green);
        }

        .detail-section-title::before { content: '## '; color: var(--terminal-green-dim); }

        /* Empty state */
        .empty-state {
            text-align: center;
            padding: 40px 20px;
            color: var(--terminal-green-dim);
        }

        .empty-state::before {
            content: '> NO_DATA_FOUND';
            display: block;
            font-size: 0.9em;
            margin-bottom: 8px;
        }

        /* Loading */
        .loading::after {
            content: '_';
            animation: blink 0.5s infinite;
        }

        /* Mobile adjustments */
        @media (max-width: 480px) {
            .container { padding: 8px; }
            .header { padding: 10px 12px; }
            .logo { font-size: 0.95em; }
            .stat-card { padding: 12px; }
            .stat-value { font-size: 1.5em; }
            .tab { padding: 12px 8px; font-size: 0.75em; }
            td, th { padding: 10px 8px; }
            .btn, .action-btn { padding: 10px 14px; }
        }
    </style>
</head>
<body>
    <div class="container">
        <header class="header">
            <div class="logo">SINK://ADMIN</div>
            <div class="status-line">
                SYS.TIME: <span id="last-update">--:--:--</span><span class="blink"></span>
            </div>
        </header>

        <nav class="tabs">
            <button class="tab active" onclick="showTab('dashboard')" id="tab-dashboard">Dashboard</button>
            <button class="tab" onclick="showTab('messages')" id="tab-messages">Messages</button>
            <button class="tab" onclick="showTab('transcripts')" id="tab-transcripts">Transcripts</button>
            <button class="tab" onclick="showTab('followups')" id="tab-followups">Followups</button>
        </nav>

        <!-- Dashboard Panel -->
        <div id="panel-dashboard" class="panel active">
            <div class="stats-grid">
                <div class="stat-card">
                    <div class="stat-label">Queue/Pending</div>
                    <div class="stat-value warning" id="stat-pending">-</div>
                </div>
                <div class="stat-card">
                    <div class="stat-label">Processing</div>
                    <div class="stat-value info" id="stat-processing">-</div>
                </div>
                <div class="stat-card">
                    <div class="stat-label">Cost Today</div>
                    <div class="stat-value" id="stat-cost-today">-</div>
                </div>
                <div class="stat-card">
                    <div class="stat-label">Followups</div>
                    <div class="stat-value warning" id="stat-followups">-</div>
                </div>
            </div>

            <div class="info-grid">
                <div class="info-panel">
                    <div class="info-title">Message Stats</div>
                    <div id="message-stats" class="loading">LOADING</div>
                </div>
                <div class="info-panel">
                    <div class="info-title">Cost Summary</div>
                    <div id="cost-stats" class="loading">LOADING</div>
                </div>
            </div>
        </div>

        <!-- Messages Panel -->
        <div id="panel-messages" class="panel">
            <div class="filter-bar">
                <select id="msg-status-filter" onchange="loadMessages()">
                    <option value="">ALL STATUS</option>
                    <option value="pending">PENDING</option>
                    <option value="processing">PROCESSING</option>
                    <option value="replied">REPLIED</option>
                    <option value="failed">FAILED</option>
                    <option value="skipped">SKIPPED</option>
                </select>
            </div>
            <div class="table-wrap">
                <table>
                    <thead>
                        <tr>
                            <th>ID</th>
                            <th>Sender</th>
                            <th>Message</th>
                            <th>Status</th>
                            <th>Time</th>
                        </tr>
                    </thead>
                    <tbody id="messages-table">
                        <tr><td colspan="5" class="empty-state">INITIALIZING...</td></tr>
                    </tbody>
                </table>
            </div>
            <div class="pagination">
                <div class="pagination-info" id="msg-pagination-info">--</div>
                <div class="pagination-btns">
                    <button class="btn" onclick="prevMessagesPage()">&lt; PREV</button>
                    <button class="btn" onclick="nextMessagesPage()">NEXT &gt;</button>
                </div>
            </div>
        </div>

        <!-- Transcripts Panel -->
        <div id="panel-transcripts" class="panel">
            <div class="table-wrap">
                <table>
                    <thead>
                        <tr>
                            <th>ID</th>
                            <th>Chat</th>
                            <th>Model</th>
                            <th>Cost</th>
                            <th>Tokens</th>
                            <th>Time</th>
                            <th>Action</th>
                        </tr>
                    </thead>
                    <tbody id="transcripts-table">
                        <tr><td colspan="7" class="empty-state">INITIALIZING...</td></tr>
                    </tbody>
                </table>
            </div>
            <div class="pagination">
                <div class="pagination-info" id="trans-pagination-info">--</div>
                <div class="pagination-btns">
                    <button class="btn" onclick="prevTranscriptsPage()">&lt; PREV</button>
                    <button class="btn" onclick="nextTranscriptsPage()">NEXT &gt;</button>
                </div>
            </div>
        </div>

        <!-- Followups Panel -->
        <div id="panel-followups" class="panel">
            <div class="filter-bar">
                <select id="followup-status-filter" onchange="loadFollowups()">
                    <option value="">ALL STATUS</option>
                    <option value="pending">PENDING</option>
                    <option value="sent">SENT</option>
                    <option value="dismissed">DISMISSED</option>
                </select>
            </div>
            <div class="table-wrap">
                <table>
                    <thead>
                        <tr>
                            <th>ID</th>
                            <th>Description</th>
                            <th>Status</th>
                            <th>Notify</th>
                            <th>Actions</th>
                        </tr>
                    </thead>
                    <tbody id="followups-table">
                        <tr><td colspan="5" class="empty-state">INITIALIZING...</td></tr>
                    </tbody>
                </table>
            </div>
        </div>
    </div>

    <!-- Transcript Detail Modal -->
    <div id="transcript-modal" class="modal">
        <div class="modal-content">
            <div class="modal-header">
                <div class="modal-title">Transcript Data</div>
                <button class="modal-close" onclick="closeTranscriptModal()">&times;</button>
            </div>
            <div class="modal-body" id="transcript-detail"></div>
        </div>
    </div>

    <!-- Edit Followup Modal -->
    <div id="edit-modal" class="modal">
        <div class="modal-content" style="max-width: 500px;">
            <div class="modal-header">
                <div class="modal-title">Edit Followup</div>
                <button class="modal-close" onclick="closeEditModal()">&times;</button>
            </div>
            <form onsubmit="saveFollowup(event)">
                <input type="hidden" id="edit-id">
                <div class="modal-body">
                    <div class="form-group">
                        <label class="form-label">Description</label>
                        <input type="text" id="edit-description" class="form-input">
                    </div>
                    <div class="form-group">
                        <label class="form-label">Status</label>
                        <select id="edit-status" class="form-input">
                            <option value="pending">PENDING</option>
                            <option value="sent">SENT</option>
                            <option value="dismissed">DISMISSED</option>
                        </select>
                    </div>
                </div>
                <div class="modal-footer">
                    <button type="button" class="btn" onclick="closeEditModal()">CANCEL</button>
                    <button type="submit" class="btn btn-primary">SAVE</button>
                </div>
            </form>
        </div>
    </div>

    <script>
        let currentTab = 'dashboard';
        let messagesOffset = 0;
        let transcriptsOffset = 0;
        const limit = 20;

        function showTab(tab) {
            document.querySelectorAll('.panel').forEach(p => p.classList.remove('active'));
            document.querySelectorAll('.tab').forEach(t => t.classList.remove('active'));
            document.getElementById('panel-' + tab).classList.add('active');
            document.getElementById('tab-' + tab).classList.add('active');
            currentTab = tab;
            if (tab === 'messages') loadMessages();
            if (tab === 'transcripts') loadTranscripts();
            if (tab === 'followups') loadFollowups();
        }

        async function loadStats() {
            try {
                const resp = await fetch('/api/stats');
                const data = await resp.json();
                document.getElementById('stat-pending').textContent = data.messages.pending;
                document.getElementById('stat-processing').textContent = data.messages.processing;
                document.getElementById('stat-cost-today').textContent = '$' + data.costs.today_usd.toFixed(2);
                document.getElementById('stat-followups').textContent = data.followups.pending;

                document.getElementById('message-stats').innerHTML = `
                    <div class="info-row"><span>Replied</span><span class="success">${data.messages.replied}</span></div>
                    <div class="info-row"><span>Failed</span><span class="error">${data.messages.failed}</span></div>
                    <div class="info-row"><span>Skipped</span><span class="muted">${data.messages.skipped}</span></div>
                `;

                document.getElementById('cost-stats').innerHTML = `
                    <div class="info-row"><span>Today</span><span class="success">$${data.costs.today_usd.toFixed(4)}</span></div>
                    <div class="info-row"><span>Total</span><span>$${data.costs.total_usd.toFixed(4)}</span></div>
                    <div class="info-row"><span>Transcripts Today</span><span>${data.transcripts.today}</span></div>
                    <div class="info-row"><span>Total Transcripts</span><span>${data.transcripts.total}</span></div>
                `;

                document.getElementById('last-update').textContent = new Date().toLocaleTimeString();
            } catch (e) { console.error('Stats error:', e); }
        }

        async function loadMessages() {
            const status = document.getElementById('msg-status-filter').value;
            const url = `/api/messages?limit=${limit}&offset=${messagesOffset}` + (status ? `&status=${status}` : '');
            try {
                const resp = await fetch(url);
                const data = await resp.json();
                const tbody = document.getElementById('messages-table');
                if (data.data.length === 0) {
                    tbody.innerHTML = '<tr><td colspan="5" class="empty-state">No messages found</td></tr>';
                } else {
                    tbody.innerHTML = data.data.map(m => `
                        <tr>
                            <td class="cell-id">${m.id}</td>
                            <td>${esc(m.sender)}</td>
                            <td class="cell-text">${esc(m.text.substring(0, 80))}${m.text.length > 80 ? '...' : ''}</td>
                            <td><span class="badge badge-${m.status}">${m.status}</span></td>
                            <td class="cell-time">${fmtTime(m.date_received)}</td>
                        </tr>
                    `).join('');
                }
                document.getElementById('msg-pagination-info').textContent =
                    `[${messagesOffset + 1}-${Math.min(messagesOffset + limit, data.total)} of ${data.total}]`;
            } catch (e) { console.error('Messages error:', e); }
        }

        async function loadTranscripts() {
            const url = `/api/transcripts?limit=${limit}&offset=${transcriptsOffset}`;
            try {
                const resp = await fetch(url);
                const data = await resp.json();
                const tbody = document.getElementById('transcripts-table');
                if (data.data.length === 0) {
                    tbody.innerHTML = '<tr><td colspan="7" class="empty-state">No transcripts found</td></tr>';
                } else {
                    tbody.innerHTML = data.data.map(t => `
                        <tr>
                            <td class="cell-id">${t.id}</td>
                            <td class="cell-text">${esc(t.chat_guid)}</td>
                            <td>${t.claude_model || '-'}</td>
                            <td class="cell-cost">$${(t.cost_usd || 0).toFixed(4)}</td>
                            <td>${(t.input_tokens || 0).toLocaleString()}/${(t.output_tokens || 0).toLocaleString()}</td>
                            <td class="cell-time">${fmtTime(t.created_at)}</td>
                            <td><button class="action-btn view" onclick="showTranscript(${t.id})">VIEW</button></td>
                        </tr>
                    `).join('');
                }
                document.getElementById('trans-pagination-info').textContent =
                    `[${transcriptsOffset + 1}-${Math.min(transcriptsOffset + limit, data.total)} of ${data.total}]`;
            } catch (e) { console.error('Transcripts error:', e); }
        }

        async function loadFollowups() {
            const status = document.getElementById('followup-status-filter').value;
            const url = '/api/followups' + (status ? `?status=${status}` : '');
            try {
                const resp = await fetch(url);
                const data = await resp.json();
                const tbody = document.getElementById('followups-table');
                if (data.length === 0) {
                    tbody.innerHTML = '<tr><td colspan="5" class="empty-state">No followups found</td></tr>';
                } else {
                    tbody.innerHTML = data.map(f => `
                        <tr>
                            <td class="cell-id">${f.id}</td>
                            <td class="cell-text">${esc(f.description)}</td>
                            <td><span class="badge badge-${f.status}">${f.status}</span></td>
                            <td class="cell-time">${f.notify_at ? fmtTime(f.notify_at) : '-'}</td>
                            <td>
                                <div class="actions">
                                    <button class="action-btn edit" onclick="editFollowup(${f.id}, '${esc(f.description).replace(/'/g, "\\'")}', '${f.status}')">EDIT</button>
                                    <button class="action-btn delete" onclick="deleteFollowup(${f.id})">DEL</button>
                                </div>
                            </td>
                        </tr>
                    `).join('');
                }
            } catch (e) { console.error('Followups error:', e); }
        }

        async function showTranscript(id) {
            try {
                const resp = await fetch('/api/transcripts/' + id);
                if (!resp.ok) {
                    alert('Failed to load transcript: ' + resp.status);
                    return;
                }
                const t = await resp.json();
                let result = '';
                try { result = JSON.parse(t.raw_stdout).result || t.raw_stdout; } catch { result = t.raw_stdout; }

                document.getElementById('transcript-detail').innerHTML = `
                    <div class="detail-row">
                        <div class="detail-label">Chat GUID</div>
                        <div class="detail-value">${esc(t.chat_guid)}</div>
                    </div>
                    <div class="detail-row">
                        <div class="detail-label">Model</div>
                        <div class="detail-value">${t.claude_model || '-'}</div>
                    </div>
                    <div class="detail-row">
                        <div class="detail-label">Cost</div>
                        <div class="detail-value cell-cost">$${(t.cost_usd || 0).toFixed(4)}</div>
                    </div>
                    <div class="detail-row">
                        <div class="detail-label">Tokens (in/out)</div>
                        <div class="detail-value">${(t.input_tokens || 0).toLocaleString()} / ${(t.output_tokens || 0).toLocaleString()}</div>
                    </div>
                    <div class="detail-row">
                        <div class="detail-label">Duration</div>
                        <div class="detail-value">${((t.duration_ms || 0) / 1000).toFixed(2)}s</div>
                    </div>
                    <div class="detail-section">
                        <div class="detail-section-title">Prompt</div>
                        <pre>${esc(t.prompt_sent)}</pre>
                    </div>
                    <div class="detail-section">
                        <div class="detail-section-title">Response</div>
                        <pre>${esc(result)}</pre>
                    </div>
                `;
                document.getElementById('transcript-modal').classList.add('open');
            } catch (e) { console.error('Transcript error:', e); }
        }

        function closeTranscriptModal() {
            document.getElementById('transcript-modal').classList.remove('open');
        }

        function editFollowup(id, description, status) {
            document.getElementById('edit-id').value = id;
            document.getElementById('edit-description').value = description;
            document.getElementById('edit-status').value = status;
            document.getElementById('edit-modal').classList.add('open');
        }

        function closeEditModal() {
            document.getElementById('edit-modal').classList.remove('open');
        }

        async function saveFollowup(e) {
            e.preventDefault();
            const id = document.getElementById('edit-id').value;
            const payload = {
                description: document.getElementById('edit-description').value,
                status: document.getElementById('edit-status').value
            };
            try {
                await fetch('/api/followups/' + id, {
                    method: 'PUT',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify(payload)
                });
                closeEditModal();
                loadFollowups();
            } catch (e) { console.error('Save error:', e); }
        }

        async function deleteFollowup(id) {
            if (!confirm('DELETE FOLLOWUP #' + id + '?')) return;
            try {
                const resp = await fetch('/api/followups/' + id, { method: 'DELETE' });
                if (!resp.ok) {
                    alert('Delete failed: ' + resp.status);
                    return;
                }
                loadFollowups();
            } catch (e) {
                console.error('Delete error:', e);
                alert('Delete failed: ' + e.message);
            }
        }

        function prevMessagesPage() { if (messagesOffset >= limit) { messagesOffset -= limit; loadMessages(); } }
        function nextMessagesPage() { messagesOffset += limit; loadMessages(); }
        function prevTranscriptsPage() { if (transcriptsOffset >= limit) { transcriptsOffset -= limit; loadTranscripts(); } }
        function nextTranscriptsPage() { transcriptsOffset += limit; loadTranscripts(); }

        function fmtTime(ts) {
            if (!ts) return '-';
            const d = new Date(ts);
            const now = new Date();
            const diff = (now - d) / 1000;
            if (diff < 60) return 'NOW';
            if (diff < 3600) return Math.floor(diff / 60) + 'm';
            if (diff < 86400) return Math.floor(diff / 3600) + 'h';
            return d.toLocaleDateString();
        }

        function esc(str) {
            if (!str) return '';
            return str.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
        }

        // Initialize
        loadStats();
        setInterval(loadStats, 5000);
    </script>
</body>
</html>
"##;
