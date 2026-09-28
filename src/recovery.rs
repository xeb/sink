//! Durable delivery of tmux replies, including answers arriving after both waits.
use crate::config::Config;
use crate::db::Database;
use crate::error::{Result, SinkError};
use crate::sender::Sender;
use crate::tmux::TmuxConfig;
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{interval, Duration, MissedTickBehavior};
use tracing::{error, info, warn};

pub const CHECK_INTERVAL_SECS: u64 = 300;
pub type DeliveryLock = Arc<Mutex<()>>;

pub fn init(conn: &Connection) -> Result<()> {
    let has_id: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('messages') WHERE name = 'tmux_command_id')",
        [],
        |row| row.get(0),
    )?;
    if !has_id {
        conn.execute("ALTER TABLE messages ADD COLUMN tmux_command_id TEXT", [])?;
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_messages_tmux_command ON messages(tmux_command_id);
         CREATE TABLE IF NOT EXISTS tmux_replies (
             command_id TEXT PRIMARY KEY,
             window TEXT NOT NULL,
             chat_guid TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             response_text TEXT,
             response_guid TEXT,
             sent_at INTEGER,
             last_error TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_tmux_replies_pending ON tmux_replies(sent_at, created_at);",
    )?;
    Ok(())
}

/// Commit the ID and every member of the batch BEFORE injecting its command.
pub fn register(db: &Database, id: &str, window: &str, chat: &str, guids: &[String]) -> Result<()> {
    let tx = db.conn().unchecked_transaction()?;
    tx.execute(
        "INSERT INTO tmux_replies(command_id, window, chat_guid, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![id, window, chat, chrono::Utc::now().timestamp_millis()],
    )?;
    for guid in guids {
        let changed = tx.execute(
            "UPDATE messages SET tmux_command_id = ?1 WHERE guid = ?2 AND chat_guid = ?3
             AND is_from_me = 0 AND response_guid IS NULL AND tmux_command_id IS NULL",
            params![id, guid, chat],
        )?;
        if changed != 1 {
            return Err(SinkError::Config(format!(
                "Cannot queue tmux reply for message {guid}"
            )));
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn has_pending(db: &Database, window: &str) -> Result<bool> {
    Ok(db.conn().query_row(
        "SELECT EXISTS(SELECT 1 FROM tmux_replies WHERE window = ?1 AND sent_at IS NULL)",
        [window],
        |row| row.get(0),
    )?)
}

pub fn defer(db: &Database, id: &str, reason: &str) -> Result<()> {
    let tx = db.conn().unchecked_transaction()?;
    tx.execute(
        "UPDATE tmux_replies SET last_error = ?2 WHERE command_id = ?1 AND sent_at IS NULL",
        params![id, reason],
    )?;
    // A concurrent recovery may already have delivered this reply.
    tx.execute(
        "UPDATE messages SET status = 'awaiting_reply', processed_at = NULL, error_reason = ?2
         WHERE tmux_command_id = ?1 AND response_guid IS NULL
         AND EXISTS(SELECT 1 FROM tmux_replies WHERE command_id = ?1 AND sent_at IS NULL)",
        params![id, reason],
    )?;
    tx.commit()?;
    Ok(())
}

fn save_response(db: &Database, id: &str, text: &str) -> Result<()> {
    if text.trim().is_empty() {
        return Err(SinkError::Config("Refusing to queue an empty reply".into()));
    }
    db.conn().execute(
        "UPDATE tmux_replies SET response_text = ?2 WHERE command_id = ?1
         AND sent_at IS NULL AND response_text IS NULL",
        params![id, text],
    )?;
    Ok(())
}

fn complete(db: &Database, id: &str, chat: &str, text: &str, guid: &str) -> Result<()> {
    let now = chrono::Utc::now().timestamp_millis();
    let tx = db.conn().unchecked_transaction()?;
    // Save the outbound row immediately so the admin panel can resolve the reply
    // even if polling is occupied by another long-running command.
    tx.execute(
        "INSERT OR IGNORE INTO messages(guid, chat_guid, sender, text, date_received, status, is_from_me)
         VALUES (?1, ?2, 'sink', ?3, ?4, 'sent', 1)",
        params![guid, chat, text, now],
    )?;
    tx.execute(
        "UPDATE tmux_replies SET response_guid = ?2, sent_at = ?3, last_error = NULL WHERE command_id = ?1",
        params![id, guid, now],
    )?;
    tx.execute(
        "UPDATE messages SET status = 'replied', response_guid = ?2, processed_at = ?3, error_reason = NULL
         WHERE tmux_command_id = ?1",
        params![id, guid, now],
    )?;
    tx.commit()?;
    Ok(())
}

/// Shared by the foreground and recovery task. The lock covers check/send/commit
/// so a reply discovered by both paths is delivered only once during this run.
pub async fn deliver(
    path: &Path,
    id: &str,
    response: Option<&str>,
    sender: &Sender,
    lock: &DeliveryLock,
) -> Result<Option<String>> {
    let _guard = lock.lock().await;
    let db = Database::open(path)?;
    if let Some(text) = response {
        save_response(&db, id, text)?;
    }
    let (chat, text, guid, sent_at): (String, Option<String>, Option<String>, Option<i64>) = db.conn().query_row(
        "SELECT chat_guid, response_text, response_guid, sent_at FROM tmux_replies WHERE command_id = ?1",
        [id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    if sent_at.is_some() {
        return Ok(guid);
    }
    let Some(text) = text else { return Ok(None) };
    // One bounded HTTP attempt per check, with no overall retry/age limit.
    // Keep the same tempGuid across retries instead of inventing another ID.
    match sender.send_queued_reply(&chat, &text, id).await {
        Ok(guid) => {
            complete(&db, id, &chat, &text, &guid)?;
            info!("Delivered queued tmux reply {} ({})", id, guid);
            Ok(Some(guid))
        }
        Err(e) => {
            defer(&db, id, &format!("Reply delivery will retry: {e}"))?;
            Err(e)
        }
    }
}

/// Strict completion is required for background recovery: a live, partially
/// streamed answer (especially Codex) must never be sent as the final response.
fn completed_reply(output: &str, id: &str) -> Option<String> {
    let start = format!("[REPLY-{id}]");
    let end = format!("[/REPLY-{id}]");
    let body = &output[output.rfind(&start)? + start.len()..];
    let text = body[..body.find(&end)?].trim();
    (!text.is_empty()).then(|| text.to_string())
}

pub async fn check(
    config: &Config,
    tmux: &TmuxConfig,
    sender: &Sender,
    lock: &DeliveryLock,
) -> Result<()> {
    check_with_capture(config, tmux, sender, lock, || {
        crate::tmux::capture_reply_history(tmux)
    })
    .await
}

async fn check_with_capture(
    config: &Config,
    tmux: &TmuxConfig,
    sender: &Sender,
    lock: &DeliveryLock,
    capture: impl FnOnce() -> std::result::Result<String, String>,
) -> Result<()> {
    let db = Database::open(&config.database.path)?;
    let pending: Vec<(String, bool)> = {
        let mut stmt = db.conn().prepare(
            "SELECT command_id, response_text IS NOT NULL FROM tmux_replies
             WHERE window = ?1 AND sent_at IS NULL AND created_at <= ?2 ORDER BY created_at",
        )?;
        let rows = stmt
            .query_map(
                params![
                    tmux.window,
                    chrono::Utc::now().timestamp_millis() - CHECK_INTERVAL_SECS as i64 * 1000
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };
    if pending.is_empty() {
        return Ok(());
    }

    // Capture once per tick, and retain all available scrollback, not just the
    // last 1,000 lines used by the fast foreground polling path.
    if pending.iter().any(|(_, ready)| !ready) {
        match capture() {
            Ok(output) => {
                for (id, ready) in &pending {
                    if !ready {
                        if let Some(text) = completed_reply(&output, id) {
                            save_response(&db, id, &text)?;
                            info!("Recovered completed tmux reply {} from scrollback", id);
                        }
                    }
                }
            }
            Err(e) => warn!("Cannot capture late replies yet; will retry: {}", e),
        }
    }
    // Save all recovered bodies before any network I/O. A disappearing pane
    // cannot lose an answer that has already entered the outbox.
    for (id, _) in pending {
        if let Err(e) = deliver(&config.database.path, &id, None, sender, lock).await {
            warn!("Queued reply {} remains pending: {}", id, e);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Json, Router};
    use serde_json::{json, Value};
    use std::sync::atomic::AtomicUsize;

    struct Fixture {
        config: Config,
        requests: Arc<std::sync::Mutex<Vec<Value>>>,
        server: tokio::task::JoinHandle<()>,
    }

    impl Fixture {
        async fn new(receipts: Vec<Value>) -> Self {
            let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
            let seen = requests.clone();
            let attempts = Arc::new(AtomicUsize::new(0));
            let app = Router::new().route(
                "/api/v1/message/text",
                post(move |Json(body): Json<Value>| {
                    let seen = seen.clone();
                    let attempts = attempts.clone();
                    let receipts = receipts.clone();
                    async move {
                        seen.lock().unwrap().push(body);
                        let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                        Json(receipts[attempt.min(receipts.len() - 1)].clone())
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let mut config = Config::default();
            config.bluebubbles.host = "127.0.0.1".into();
            config.bluebubbles.port = port;
            config.database.path =
                std::env::temp_dir().join(format!("sink-recovery-{}.db", uuid::Uuid::new_v4()));
            Self {
                config,
                requests,
                server,
            }
        }

        fn queue(&self, id: &str, chat: &str, guids: &[&str], age_secs: i64) {
            let db = Database::open(&self.config.database.path).unwrap();
            for guid in guids {
                db.conn().execute(
                    "INSERT INTO messages(guid, chat_guid, sender, text, date_received, status) VALUES (?1, ?2, 'test', 'request', 0, 'processing')",
                    params![guid, chat],
                ).unwrap();
            }
            register(
                &db,
                id,
                "sink MASTER",
                chat,
                &guids.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            )
            .unwrap();
            db.conn()
                .execute(
                    "UPDATE tmux_replies SET created_at = ?2 WHERE command_id = ?1",
                    params![id, chrono::Utc::now().timestamp_millis() - age_secs * 1000],
                )
                .unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
            let _ = std::fs::remove_file(&self.config.database.path);
        }
    }

    fn success(guid: &str) -> Value {
        json!({"status": 200, "message": "ok", "data": {"guid": guid}})
    }

    #[test]
    fn recovery_requires_matching_complete_nonempty_tags() {
        assert_eq!(
            completed_reply("• [REPLY-a]late answer\n[/REPLY-a]", "a").as_deref(),
            Some("late answer")
        );
        assert!(completed_reply("[REPLY-a]still streaming", "a").is_none());
        assert!(completed_reply("[REPLY-a]wrong closer[/REPLY-b]", "a").is_none());
        assert!(completed_reply("[/REPLY-a][REPLY-a]wrong order", "a").is_none());
        assert!(completed_reply("[REPLY-a]  [/REPLY-a]", "a").is_none());
        assert!(completed_reply("[REPLY-b]another chat[/REPLY-b]", "a").is_none());
        assert_eq!(
            completed_reply("[REPLY-a]old[/REPLY-a]\n[REPLY-a]new[/REPLY-a]", "a").as_deref(),
            Some("new")
        );
    }

    #[tokio::test]
    async fn recovers_late_batched_reply_once_while_another_request_is_running() {
        let fixture = Fixture::new(vec![success("sent-a"), success("sent-b")]).await;
        fixture.queue("a", "chat-a", &["m1", "m2"], 7200);
        fixture.queue("b", "chat-b", &["m3"], 0);
        let db = Database::open(&fixture.config.database.path).unwrap();
        defer(&db, "a", "foreground timed out").unwrap();
        let tmux = TmuxConfig::default();
        let sender = Sender::new(fixture.config.clone());
        let lock = Arc::new(Mutex::new(()));
        let output = "[REPLY-b]fresh response[/REPLY-b]\n[REPLY-a]late response[/REPLY-a]";
        check_with_capture(&fixture.config, &tmux, &sender, &lock, || Ok(output.into()))
            .await
            .unwrap();
        assert!(db.is_processing().unwrap()); // unrelated foreground request still runs
        let matched: i64 = db.conn().query_row(
            "SELECT COUNT(*) FROM messages WHERE tmux_command_id = 'a' AND status = 'replied' AND response_guid = 'sent-a' AND error_reason IS NULL",
            [], |row| row.get(0)).unwrap();
        assert_eq!(matched, 2);
        let outbound: String = db
            .conn()
            .query_row(
                "SELECT text FROM messages WHERE guid = 'sent-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(outbound, "late response");
        check_with_capture(&fixture.config, &tmux, &sender, &lock, || {
            panic!("nothing stale remains")
        })
        .await
        .unwrap();
        // Late foreground completion and timeout bookkeeping must not resend or
        // change the now-delivered batch back to waiting.
        deliver(
            &fixture.config.database.path,
            "a",
            Some("late response"),
            &sender,
            &lock,
        )
        .await
        .unwrap();
        defer(&db, "a", "late foreground timeout").unwrap();
        assert_eq!(fixture.requests.lock().unwrap().len(), 1);
        assert_eq!(fixture.requests.lock().unwrap()[0]["chatGuid"], "chat-a");
        assert_eq!(
            fixture.requests.lock().unwrap()[0]["message"],
            "late response"
        );
        let status: String = db
            .conn()
            .query_row("SELECT status FROM messages WHERE guid = 'm1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(status, "replied");
    }

    #[tokio::test]
    async fn failures_retry_saved_body_after_restart_even_without_a_pane() {
        let fixture = Fixture::new(vec![
            json!({"status": 500, "message": "offline", "data": null}),
            json!({"status": 200, "message": "no receipt", "data": {}}),
            success("eventual-guid"),
        ])
        .await;
        fixture.queue("a", "chat-a", &["m1"], 86400 * 90);
        let sender = Sender::new(fixture.config.clone());
        let lock = Arc::new(Mutex::new(()));
        let tmux = TmuxConfig::default();
        check_with_capture(&fixture.config, &tmux, &sender, &lock, || {
            Ok("[REPLY-a]saved answer[/REPLY-a]".into())
        })
        .await
        .unwrap();
        let db = Database::open(&fixture.config.database.path).unwrap();
        assert!(has_pending(&db, &tmux.window).unwrap());
        let status: String = db
            .conn()
            .query_row("SELECT status FROM messages WHERE guid = 'm1'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(status, "awaiting_reply");
        drop(db);
        // Fresh connections and lock simulate a daemon restart. Captured bodies
        // are durable; no tmux access or command rerun is needed for retries.
        let new_lock = Arc::new(Mutex::new(()));
        for _ in 0..2 {
            check_with_capture(&fixture.config, &tmux, &sender, &new_lock, || {
                panic!("body already saved")
            })
            .await
            .unwrap();
        }
        let db = Database::open(&fixture.config.database.path).unwrap();
        assert!(!has_pending(&db, &tmux.window).unwrap());
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests
            .iter()
            .all(|r| r["message"] == "saved answer" && r["tempGuid"] == "sink-reply-a"));
    }

    #[tokio::test]
    async fn concurrent_foreground_and_recovery_send_only_once() {
        let fixture = Fixture::new(vec![success("once")]).await;
        fixture.queue("a", "chat-a", &["m1"], 600);
        let sender = Sender::new(fixture.config.clone());
        let lock = Arc::new(Mutex::new(()));
        let (first, second) = tokio::join!(
            deliver(
                &fixture.config.database.path,
                "a",
                Some("answer"),
                &sender,
                &lock
            ),
            deliver(
                &fixture.config.database.path,
                "a",
                Some("answer"),
                &sender,
                &lock
            ),
        );
        assert_eq!(first.unwrap().as_deref(), Some("once"));
        assert_eq!(second.unwrap().as_deref(), Some("once"));
        assert_eq!(fixture.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn restart_preserves_submitted_commands_without_requeuing_them() {
        let fixture = Fixture::new(vec![success("unused")]).await;
        fixture.queue("a", "chat-a", &["m1", "m2"], 600);
        let db = Database::open(&fixture.config.database.path).unwrap();
        db.conn().execute(
            "INSERT INTO messages(guid, chat_guid, sender, text, date_received, status) VALUES ('unsubmitted', 'chat-b', 'test', 'request', 0, 'processing')", [],
        ).unwrap();
        assert_eq!(db.recover_stuck_processing().unwrap(), 3);
        assert_eq!(db.get_pending_messages().unwrap().len(), 1);
        assert_eq!(db.get_pending_messages().unwrap()[0].guid, "unsubmitted");
        assert!(has_pending(&db, "sink MASTER").unwrap());
        let sender = Sender::new(fixture.config.clone());
        let lock = Arc::new(Mutex::new(()));
        let tmux = TmuxConfig::default();
        check_with_capture(&fixture.config, &tmux, &sender, &lock, || {
            Err("pane missing".into())
        })
        .await
        .unwrap();
        check_with_capture(&fixture.config, &tmux, &sender, &lock, || {
            Ok("[REPLY-a]still streaming".into())
        })
        .await
        .unwrap();
        assert!(fixture.requests.lock().unwrap().is_empty());
        assert!(has_pending(&db, "sink MASTER").unwrap());
    }
}

pub async fn run(config: Config, tmux: TmuxConfig, running: Arc<AtomicBool>, lock: DeliveryLock) {
    let sender = Sender::new(config.clone());
    let mut ticker = interval(Duration::from_secs(CHECK_INTERVAL_SECS));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    info!(
        "Late-reply recovery started (every {}s; no expiration)",
        CHECK_INTERVAL_SECS
    );
    while running.load(Ordering::SeqCst) {
        ticker.tick().await;
        if let Err(e) = check(&config, &tmux, &sender, &lock).await {
            error!("Late-reply recovery check failed; will retry: {}", e);
        }
    }
}
