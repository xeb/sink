mod claude;
mod config;
mod db;
mod error;
mod poller;
mod sender;

use crate::claude::ClaudeInvoker;
use crate::config::Config;
use crate::db::Database;
use crate::error::Result;
use crate::poller::Poller;
use crate::sender::Sender;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::{error, info};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("sink=info".parse().unwrap()),
        )
        .init();

    info!("Sink daemon starting");

    // Record startup time - only process messages after this
    let startup_time = chrono::Utc::now().timestamp_millis();
    info!("Startup time: {} (will ignore older messages)", startup_time);

    // Load config
    let config_path = std::env::var("SINK_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/etc/sink/config.toml"));

    let config = if config_path.exists() {
        info!("Loading config from {:?}", config_path);
        Config::load(&config_path)?
    } else {
        info!("Using default config (no config file found at {:?})", config_path);
        Config::default()
    };

    // Open database
    info!("Opening database at {:?}", config.database.path);
    let db = Database::open(&config.database.path)?;

    // Initialize components
    let poller = Poller::new(config.clone());
    let invoker = ClaudeInvoker::new(config.clone());
    let sender = Sender::new(config.clone());

    // Set up signal handling for graceful shutdown
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        info!("Received shutdown signal");
        r.store(false, Ordering::SeqCst);
    });

    info!(
        "Starting poll loop (interval: {}s)",
        config.polling.interval_secs
    );

    let mut poll_interval = interval(Duration::from_secs(config.polling.interval_secs));

    while running.load(Ordering::SeqCst) {
        poll_interval.tick().await;

        // Poll for new messages
        match poller.poll_and_store(&db, startup_time).await {
            Ok(new_messages) => {
                if !new_messages.is_empty() {
                    info!("Found {} new incoming messages", new_messages.len());
                }
            }
            Err(e) => {
                error!("Polling error: {}", e);
                continue;
            }
        }

        // Process pending messages if not already processing
        let is_processing = match db.is_processing() {
            Ok(v) => v,
            Err(e) => {
                error!("Database error checking processing status: {}", e);
                continue;
            }
        };
        if is_processing {
            continue;
        }

        let pending = match db.get_pending_messages() {
            Ok(v) => v,
            Err(e) => {
                error!("Database error getting pending messages: {}", e);
                continue;
            }
        };
        if pending.is_empty() {
            continue;
        }

        // Process the oldest pending message
        let msg = &pending[0];
        info!(
            "Processing message from {} in {}",
            msg.sender, msg.chat_guid
        );

        if let Err(e) = db.update_status(&msg.guid, "processing") {
            error!("Failed to update status to processing: {}", e);
            continue;
        }

        // Get conversation history for context
        let history = match db.get_recent_messages_for_chat(&msg.chat_guid, config.context.message_history_count) {
            Ok(v) => v,
            Err(e) => {
                error!("Failed to get chat history: {}", e);
                let _ = db.mark_failed(&msg.guid);
                continue;
            }
        };

        // Filter out the current message from history
        let history: Vec<_> = history
            .into_iter()
            .filter(|m| m.guid != msg.guid)
            .collect();

        // Get existing session for this chat
        let session_id = match db.get_session_for_chat(&msg.chat_guid) {
            Ok(v) => v,
            Err(e) => {
                error!("Failed to get session: {}", e);
                None
            }
        };

        // Build prompt and invoke Claude
        let prompt = invoker.build_prompt(msg, &history);

        match invoker.invoke(&prompt, session_id.as_deref()).await {
            Ok((response, new_session_id)) => {
                // Send response
                match sender.send_with_retry(&msg.chat_guid, &response, 3).await {
                    Ok(response_guid) => {
                        if let Err(e) = db.mark_processed(
                            &msg.guid,
                            response_guid.as_deref(),
                            new_session_id.as_deref(),
                        ) {
                            error!("Failed to mark as processed: {}", e);
                        }
                        info!("Successfully processed and responded to message");
                    }
                    Err(e) => {
                        error!("Failed to send response: {}", e);
                        let _ = db.mark_failed(&msg.guid);
                    }
                }
            }
            Err(e) => {
                error!("Claude invocation failed: {}", e);
                let _ = db.mark_failed(&msg.guid);
            }
        }
    }

    info!("Sink daemon shutting down");
    Ok(())
}
