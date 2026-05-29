mod attachments;
mod claude;
mod commands;
mod config;
mod contacts;
mod db;
mod error;
mod followups;
mod gemini;
mod gmail;
mod poller;
mod scheduler;
mod sender;
mod tmux;
mod transcripts;
mod web;

use crate::attachments::AttachmentDownloader;
use crate::claude::ClaudeInvoker;
use crate::commands::{CommandHandler, CommandParser};
use crate::config::Config;
use crate::contacts::ContactResolver;
use crate::db::{Database, Message};
use crate::error::Result;
use crate::followups::{Followup, FollowupsDb};
use crate::gemini::GeminiExtractor;
use crate::poller::Poller;
use crate::scheduler::NotificationScheduler;
use crate::sender::Sender;
use crate::tmux::TmuxConfig;
use crate::transcripts::{Transcript, TranscriptsDb};
use regex::Regex;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, warn};

/// True if an attachment is viewable media (image/video) worth downloading and
/// handing to Claude. Skips link-preview payloads (.pluginPayloadAttachment),
/// vcards, and other non-media attachment types.
fn is_media_attachment(a: &poller::Attachment) -> bool {
    if let Some(mime) = a.mime_type.as_deref() {
        return mime.starts_with("image/") || mime.starts_with("video/");
    }
    // Fall back to the transfer filename extension when mime type is absent.
    if let Some(name) = a.transfer_name.as_deref() {
        let lower = name.to_ascii_lowercase();
        return [
            ".jpg", ".jpeg", ".png", ".gif", ".heic", ".heif", ".webp", ".mov", ".mp4",
        ]
        .iter()
        .any(|ext| lower.ends_with(ext));
    }
    false
}

/// Extract the first http(s) URL from message text, if any. Used to surface a
/// `link=` field for blank/link-only messages so the session can fetch it.
fn extract_first_url(text: &str) -> Option<String> {
    let re = Regex::new(r"https?://[^\s]+").ok()?;
    re.find(text).map(|m| m.as_str().to_string())
}

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
    info!(
        "Startup time: {} (will ignore older messages)",
        startup_time
    );

    // Load config
    let config_path = std::env::var("SINK_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/etc/sink/config.toml"));

    let config = if config_path.exists() {
        info!("Loading config from {:?}", config_path);
        Config::load(&config_path)?
    } else {
        info!(
            "Using default config (no config file found at {:?})",
            config_path
        );
        Config::default()
    };

    // Open main database
    info!("Opening database at {:?}", config.database.path);
    let db = Database::open(&config.database.path)?;

    // Recover any messages stuck in "processing" from a previous crash
    match db.recover_stuck_processing() {
        Ok(count) if count > 0 => {
            warn!("Recovered {} message(s) stuck in 'processing' status", count);
        }
        Ok(_) => {}
        Err(e) => {
            error!("Failed to recover stuck messages: {}", e);
        }
    }

    // Open transcripts database (if configured via databases section)
    let transcripts_db_path = config
        .databases
        .as_ref()
        .map(|d| d.transcripts.clone())
        .unwrap_or_else(|| PathBuf::from("/var/lib/sink/transcripts.db"));
    let transcripts_db = TranscriptsDb::open(&transcripts_db_path)?;
    info!("Transcripts database at {:?}", transcripts_db_path);

    // Open followups database (if configured)
    let followups_db_path = config
        .databases
        .as_ref()
        .map(|d| d.followups.clone())
        .unwrap_or_else(|| PathBuf::from("/var/lib/sink/followups.db"));
    let followups_db = FollowupsDb::open(&followups_db_path)?;
    info!("Followups database at {:?}", followups_db_path);

    // Initialize Gemini extractor (if configured and API key available)
    let gemini = config.gemini.as_ref().and_then(|g| {
        match GeminiExtractor::new(g.clone()) {
            Some(extractor) => {
                info!("Gemini extractor enabled (model: {})", g.model);
                Some(extractor)
            }
            None => {
                warn!("Gemini configured but no API key found (set GEMINI_API_KEY env var or api_key in config)");
                None
            }
        }
    });

    // Determine execution mode (tmux vs Claude)
    let use_tmux = config.tmux.is_some();
    let tmux_config = config.tmux.as_ref().map(|t| TmuxConfig {
        window: t.window.clone(),
        prompt: t.prompt.clone(),
        timeout_secs: t.timeout_secs,
        capture_lines: t.capture_lines,
        capture_interval_ms: t.capture_interval_ms,
    });

    if use_tmux {
        info!("TMUX mode enabled, window: {}",
            tmux_config.as_ref().map(|c| c.window.as_str()).unwrap_or("?")
        );
    } else {
        info!("Claude mode enabled (no [tmux] config section found)");
    }

    // Initialize components
    let poller = Poller::new(config.clone());
    let invoker = ClaudeInvoker::new(config.clone());
    let sender = Sender::new(config.clone());
    let attachment_downloader = AttachmentDownloader::new(config.clone());
    let contact_resolver = ContactResolver::new(&config);
    if contact_resolver.enabled() {
        info!("Contact name resolution enabled");
    }

    // Set up signal handling for graceful shutdown
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        info!("Received shutdown signal");
        r.store(false, Ordering::SeqCst);
    });

    // Spawn notification scheduler (if notifications enabled)
    if let Some(ref notif_config) = config.notifications {
        if notif_config.enabled {
            let scheduler = NotificationScheduler::new(
                followups_db_path.clone(),
                transcripts_db_path.clone(),
                config.clone(),
                notif_config.clone(),
                running.clone(),
            );
            tokio::spawn(async move {
                scheduler.run().await;
            });
            info!("Notification scheduler spawned");
        }
    } else {
        // Default: enable scheduler if gemini is configured
        if config.gemini.is_some() {
            let default_notif_config = crate::config::NotificationsConfig {
                enabled: true,
                scheduler_interval_secs: 60,
                default_snooze_mins: 60,
                max_retries: 3,
            };
            let scheduler = NotificationScheduler::new(
                followups_db_path.clone(),
                transcripts_db_path.clone(),
                config.clone(),
                default_notif_config,
                running.clone(),
            );
            tokio::spawn(async move {
                scheduler.run().await;
            });
            info!("Notification scheduler spawned (default config)");
        }
    }

    // Spawn web server (if enabled)
    let web_enabled = config.web_server.as_ref().map(|w| w.enabled).unwrap_or(true);
    if web_enabled {
        let web_config = config.web_server.clone().unwrap_or_default();
        let state = web::AppState {
            db_path: config.database.path.clone(),
            transcripts_db_path: transcripts_db_path.clone(),
            followups_db_path: followups_db_path.clone(),
        };
        let host = web_config.host.clone();
        let port = web_config.port;
        tokio::spawn(async move {
            web::run_server(state, &host, port).await;
        });
        info!("Admin panel at http://{}:{}", web_config.host, web_config.port);
    }

    info!(
        "Starting poll loop (interval: {}s)",
        config.polling.interval_secs
    );

    let mut poll_interval = interval(Duration::from_secs(config.polling.interval_secs));
    let mut message_count: u64 = 0;
    let mut gmail_auth_pending: Option<gmail::GmailAuthPending> = None;

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

        // Check if the batch window has elapsed for this chat
        // (wait until no new messages have arrived for batch_window_secs)
        let oldest_pending = &pending[0];
        let batch_window_ms = (config.polling.batch_window_secs * 1000) as i64;
        let now_ms = chrono::Utc::now().timestamp_millis();

        let newest_ts = match db.newest_pending_timestamp_for_chat(&oldest_pending.chat_guid) {
            Ok(Some(ts)) => ts,
            Ok(None) => continue,
            Err(e) => {
                error!("Database error checking batch window: {}", e);
                continue;
            }
        };

        if now_ms - newest_ts < batch_window_ms {
            debug!(
                "Batch window not elapsed for chat {} ({:.1}s remaining)",
                oldest_pending.chat_guid,
                (batch_window_ms - (now_ms - newest_ts)) as f64 / 1000.0
            );
            continue;
        }

        // Batch all pending messages from this chat
        let batch = match db.get_pending_messages_for_chat(&oldest_pending.chat_guid) {
            Ok(v) => v,
            Err(e) => {
                error!("Database error getting batch: {}", e);
                continue;
            }
        };

        // Build a combined message from the batch
        let msg = if batch.len() == 1 {
            batch[0].clone()
        } else {
            info!("Batching {} messages from chat {}", batch.len(), batch[0].chat_guid);
            let combined_text = batch
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let combined_attachments = batch
                .iter()
                .flat_map(|m| m.attachments.clone())
                .collect::<Vec<_>>();
            Message {
                text: combined_text,
                attachments: combined_attachments,
                // Use the first message's metadata (guid, sender, etc.)
                ..batch[0].clone()
            }
        };
        // Track all guids in the batch for marking status later
        let batch_guids: Vec<String> = batch.iter().map(|m| m.guid.clone()).collect();

        info!(
            "Processing message from {} in {}",
            msg.sender, msg.chat_guid
        );

        // Check if this is a gmail auth callback URL (intercept before anything else)
        if gmail_auth_pending.is_some() && gmail::is_callback_url(&msg.text) {
            info!("Received Gmail auth callback URL, completing authentication");
            if let Some(pending) = gmail_auth_pending.take() {
                let success = pending.complete(msg.text.trim()).await;
                if success {
                    info!("Gmail re-authentication succeeded");
                } else {
                    warn!("Gmail re-authentication failed");
                }
            }
            // Swallow the message silently
            for guid in &batch_guids {
                if let Err(e) = db.mark_processed(guid, None, None) {
                    error!("Failed to mark gmail callback as processed: {}", e);
                }
            }
            continue;
        }

        // Check if this is a control command (before Claude invocation)
        if let Some(command) = CommandParser::parse(&msg.text) {
            info!("Detected control command: {:?}", command);

            let default_snooze = config
                .notifications
                .as_ref()
                .map(|n| n.default_snooze_mins)
                .unwrap_or(60);

            let handler = CommandHandler::new(&followups_db, default_snooze);
            match handler.handle(command, &msg.chat_guid, &msg.sender) {
                Ok(response) => {
                    if let Err(e) = sender.send_with_retry(&msg.chat_guid, &response, 3).await {
                        error!("Failed to send command response: {}", e);
                    }
                    for guid in &batch_guids {
                        if let Err(e) = db.mark_processed(guid, None, None) {
                            error!("Failed to mark command as processed: {}", e);
                        }
                    }
                }
                Err(e) => {
                    error!("Command handler error: {}", e);
                    for guid in &batch_guids {
                        let _ = db.mark_failed(guid);
                    }
                }
            }
            continue; // Skip Claude invocation for commands
        }

        // For group chats, check if message is addressed to Claude
        if let Some(ref gemini_client) = gemini {
            match poller.get_chat_participant_count(&msg.chat_guid).await {
                Ok(participant_count) => {
                    if participant_count > 2 {
                        debug!(
                            "Group chat detected ({} participants), checking if addressed to Claude",
                            participant_count
                        );

                        // Get conversation history for context
                        let history_for_check = db
                            .get_recent_messages_for_chat(&msg.chat_guid, 10)
                            .unwrap_or_default();

                        // Build conversation string for Gemini
                        let conversation: String = history_for_check
                            .iter()
                            .filter(|m| m.guid != msg.guid)
                            .map(|m| {
                                let sender_name = if m.is_from_me { "Claude" } else { &m.sender };
                                format!("{}: {}", sender_name, m.text)
                            })
                            .collect::<Vec<_>>()
                            .join("\n");

                        match gemini_client
                            .check_if_addressed_to_claude(&conversation, &msg.text, &msg.sender)
                            .await
                        {
                            Ok(check) => {
                                if !check.addressed_to_claude {
                                    info!(
                                        "Message not addressed to Claude: {}",
                                        check.reason
                                    );
                                    for guid in &batch_guids {
                                        if let Err(e) = db.mark_not_for_claude(guid, &check.reason) {
                                            error!("Failed to mark as not_for_claude: {}", e);
                                        }
                                    }
                                    continue; // Skip Claude invocation
                                }
                                debug!("Message IS addressed to Claude: {}", check.reason);
                            }
                            Err(e) => {
                                // On Gemini error, default to responding (fail-open for 1:1-like behavior)
                                warn!("Gemini addressed check failed, defaulting to respond: {}", e);
                            }
                        }
                    }
                }
                Err(e) => {
                    // On error getting participant count, default to responding
                    warn!("Failed to get participant count, defaulting to respond: {}", e);
                }
            }
        }

        for guid in &batch_guids {
            if let Err(e) = db.update_status(guid, "processing") {
                error!("Failed to update status to processing: {}", e);
            }
        }

        // Get conversation history for context
        let history = match db.get_recent_messages_for_chat(
            &msg.chat_guid,
            config.context.message_history_count,
        ) {
            Ok(v) => v,
            Err(e) => {
                error!("Failed to get chat history: {}", e);
                for guid in &batch_guids {
                    let _ = db.mark_failed(guid);
                }
                continue;
            }
        };

        // Filter out the batched messages from history
        let history: Vec<_> = history
            .into_iter()
            .filter(|m| !batch_guids.contains(&m.guid))
            .collect();

        // Download media attachments (images/videos) — BOTH tmux and Claude modes need
        // these. Non-media payloads (e.g. link-preview .pluginPayloadAttachment) are skipped
        // so the session is never handed a non-image to Read.
        let attachment_paths = if msg.attachments.is_empty() {
            Vec::new()
        } else {
            let media: Vec<_> = msg
                .attachments
                .iter()
                .filter(|a| is_media_attachment(a))
                .cloned()
                .collect();
            if media.is_empty() {
                Vec::new()
            } else {
                info!("Downloading {} media attachment(s)", media.len());
                attachment_downloader.download_all(&media, &msg.guid).await
            }
        };

        // Surface the first URL in the message text (covers blank/link-only messages).
        let link = extract_first_url(&msg.text);

        // Resolve the sender handle to a contact display name (read-only, via the
        // contacts instance). Cached in-memory across messages.
        let from_name = contact_resolver.resolve(&msg.sender).await;

        // Execute command via tmux or Claude depending on configuration
        let execution_result = if use_tmux {
            // TMUX MODE: Wrap message with unique ID and extract matching [REPLY-ID] from output
            if let Some(ref tc) = tmux_config {
                debug!("Using TMUX mode for message: {}", msg.text);

                // Generate a 4-character random ID to uniquely identify this command/reply pair
                let id = uuid::Uuid::new_v4().to_string()[0..4].to_string();

                // Daemon metadata goes as bracketed tokens AFTER the pristine [CMD-id]
                // opening tag (and before the user text), so id extraction — the first ']'
                // following "[CMD-" — is unaffected. from= is always present; attachment=
                // (paths joined by '|') and link= appear only when relevant.
                let mut meta = format!("[from={}]", msg.sender);
                if let Some(ref name) = from_name {
                    meta.push_str(&format!("[name={}]", name));
                }
                if !attachment_paths.is_empty() {
                    meta.push_str(&format!("[attachment={}]", attachment_paths.join("|")));
                }
                if let Some(ref url) = link {
                    meta.push_str(&format!("[link={}]", url));
                }
                let wrapped = format!("[CMD-{}]{}{}[/CMD-{}]", id, meta, msg.text, id);
                info!("TMUX: Sending wrapped command with ID: {} (from={}, name={:?}, {} attachment(s), link={})",
                    id, msg.sender, from_name, attachment_paths.len(), link.is_some());

                match crate::tmux::execute_command(tc, &wrapped).await {
                    Ok(output) => {
                        info!("TMUX: Raw output received, {} chars", output.len());
                        // Extract content between [REPLY-ID] and [/REPLY-ID] tags for this specific command
                        let reply_start_tag = format!("[REPLY-{}]", id);
                        let reply_end_tag = format!("[/REPLY-{}]", id);

                        if let Some(start) = output.find(&reply_start_tag) {
                            if let Some(end) = output[start..].find(&reply_end_tag) {
                                let reply_content = output[start + reply_start_tag.len()..start + end].to_string();
                                info!("TMUX: Extracted reply for ID {}: {} chars", id, reply_content.len());
                                Ok((reply_content, None))
                            } else {
                                error!("TMUX: Found [REPLY-{}] but no [/REPLY-{}] tag", id, id);
                                Err(format!("Response missing [/REPLY-{}] tag", id))
                            }
                        } else {
                            error!("TMUX: No [REPLY-{}] tag found in output", id);
                            Err(format!("Response missing [REPLY-{}] tags", id))
                        }
                    }
                    Err(e) => {
                        error!("TMUX command failed: {}", e);
                        Err(format!("TMUX execution error: {}", e))
                    }
                }
            } else {
                Err("TMUX mode enabled but config missing".to_string())
            }
        } else {
            // CLAUDE MODE: Traditional subprocess invocation
            let session_id = match db.get_session_for_chat(&msg.chat_guid) {
                Ok(v) => v,
                Err(e) => {
                    error!("Failed to get session: {}", e);
                    None
                }
            };

            // Build prompt and invoke Claude (attachments already downloaded above).
            let prompt = invoker.build_prompt(&msg, &history, &attachment_paths);

            match invoker.invoke(&prompt, session_id.as_deref()).await {
                Ok(result) => Ok((result.response_text, result.session_id)),
                Err(e) => Err(e.to_string()),
            }
        };

        match execution_result {
            Ok((response_text, session_id_opt)) => {
                info!("Message execution succeeded, response: {} chars", response_text.len());
                // Store/track transcript (varies by mode)
                let transcript_id: Option<i64> = if !use_tmux {
                    // Store transcript only in Claude mode
                    let transcript = Transcript {
                        id: None,
                        message_guid: msg.guid.clone(),
                        chat_guid: msg.chat_guid.clone(),
                        session_id: session_id_opt.clone(),
                        prompt_sent: String::new(), // Would be filled from Claude result in full mode
                        raw_stdout: String::new(),
                        raw_stderr: None,
                        exit_code: 0,
                        messages_json: None,
                        tool_calls_json: None,
                        cost_usd: None,
                        input_tokens: None,
                        output_tokens: None,
                        claude_model: None,
                        working_dir: config
                            .claude
                            .working_dir
                            .to_string_lossy()
                            .to_string(),
                        duration_ms: 0,
                        created_at: chrono::Utc::now().timestamp_millis(),
                    };

                    match transcripts_db.insert_transcript(&transcript) {
                        Ok(id) => {
                            debug!("Stored transcript with id {}", id);
                            Some(id)
                        }
                        Err(e) => {
                            warn!("Failed to store transcript: {}", e);
                            None
                        }
                    }
                } else {
                    // TMUX mode: no transcript storage
                    None
                };

                // Send response
                info!("Sending response via iMessage: {} chars", response_text.len());
                match sender
                    .send_with_retry(&msg.chat_guid, &response_text, 3)
                    .await
                {
                    Ok(response_guid) => {
                        for guid in &batch_guids {
                            if let Err(e) = db.mark_processed(
                                guid,
                                response_guid.as_deref(),
                                session_id_opt.as_deref(),
                            ) {
                                error!("Failed to mark as processed: {}", e);
                            }
                        }
                        info!("Successfully processed and responded to message");
                        message_count += 1;

                        // Every 4th message, check gmail auth (if not already pending)
                        if gmail::should_check(message_count) && gmail_auth_pending.is_none() {
                            if let Some(ref gmail_cfg) = config.gmail {
                                info!("Checking Gmail authentication (message #{})", message_count);
                                if !gmail::check_auth(gmail_cfg).await {
                                    info!("Gmail not authenticated, starting login flow");
                                    if let Some((auth_url, pending)) = gmail::start_login(gmail_cfg).await {
                                        gmail::send_auth_request(&sender, gmail_cfg, &auth_url).await;
                                        gmail_auth_pending = Some(pending);
                                        info!("Gmail auth URL sent, waiting for callback");
                                    }
                                }
                            }
                        }

                        // Extract followups using Gemini (if configured)
                        // DISABLED: Followup extraction is turned off to reduce noise
                        // To re-enable, change `false` to `true` below
                        let _followup_extraction_enabled = false;
                        if _followup_extraction_enabled {
                            if let (Some(ref gemini_client), Some(tid)) = (&gemini, transcript_id) {
                                let conversation = build_conversation_for_extraction(
                                    &history,
                                    &msg,
                                    &response_text,
                                );

                                match gemini_client.extract_followups(&conversation).await {
                                    Ok(extracted) => {
                                        if !extracted.is_empty() {
                                            info!(
                                                "Extracted {} followup(s) from conversation",
                                                extracted.len()
                                            );
                                        }

                                        for ef in extracted {
                                            let notify_at =
                                                gemini_client.parse_relative_time(&ef.notify_at_relative);

                                            let (notify_interval_mins, notify_until) =
                                                if let Some(recurring) = &ef.recurring {
                                                    let until = recurring.until_relative.as_ref().and_then(
                                                        |u| gemini_client.parse_relative_time(u),
                                                    );
                                                    (Some(recurring.interval_mins), until)
                                                } else {
                                                    (None, None)
                                                };

                                            let followup = Followup {
                                                id: None,
                                                transcript_id: tid,
                                                chat_guid: msg.chat_guid.clone(),
                                                sender: msg.sender.clone(),
                                                original_message_guid: msg.guid.clone(),
                                                description: ef.description,
                                                context_summary: Some(ef.context),
                                                trigger_type: ef.followup_type,
                                                notify_at,
                                                notify_interval_mins,
                                                notify_until,
                                                recurrence_count: 0,
                                                status: "pending".to_string(),
                                                created_at: chrono::Utc::now().timestamp_millis(),
                                                sent_at: None,
                                                dismissed_at: None,
                                                notification_guid: None,
                                                user_response: None,
                                            };

                                            if let Err(e) = followups_db.insert_followup(&followup) {
                                                warn!("Failed to insert followup: {}", e);
                                            } else {
                                                info!(
                                                    "Created followup: {} (notify_at: {:?})",
                                                    followup.description, followup.notify_at
                                                );
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        warn!("Gemini extraction failed: {}", e);
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to send response: {}", e);
                        for guid in &batch_guids {
                            let _ = db.mark_failed(guid);
                        }
                    }
                }
            }
            Err(e) => {
                error!("Command execution failed: {}", e);

                // Send error response to user
                let error_response = format!("⚠️ Error processing command: {}", e);
                match sender
                    .send_with_retry(&msg.chat_guid, &error_response, 3)
                    .await
                {
                    Ok(response_guid) => {
                        info!("Sent error response to user");
                        for guid in &batch_guids {
                            if let Err(err) = db.mark_processed(
                                guid,
                                response_guid.as_deref(),
                                None,
                            ) {
                                error!("Failed to mark as processed: {}", err);
                            }
                        }
                    }
                    Err(send_err) => {
                        error!("Failed to send error response: {}", send_err);
                        for guid in &batch_guids {
                            let _ = db.mark_failed(guid);
                        }
                    }
                }
            }
        }
    }

    info!("Sink daemon shutting down");
    Ok(())
}

/// Build a conversation string for Gemini extraction (hard cap: 10 messages)
fn build_conversation_for_extraction(
    history: &[crate::db::Message],
    current_msg: &crate::db::Message,
    claude_response: &str,
) -> String {
    let mut conversation = String::new();

    // Take last 9 messages from history (to leave room for current + response = 10 total)
    let history_slice = if history.len() > 9 {
        &history[history.len() - 9..]
    } else {
        history
    };

    for msg in history_slice {
        let sender = if msg.is_from_me {
            "Claude"
        } else {
            &msg.sender
        };
        conversation.push_str(&format!("{}: {}\n\n", sender, msg.text));
    }

    // Add current message
    conversation.push_str(&format!("{}: {}\n\n", current_msg.sender, current_msg.text));

    // Add Claude's response
    conversation.push_str(&format!("Claude: {}\n", claude_response));

    conversation
}
