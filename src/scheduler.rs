use crate::claude::ClaudeInvoker;
use crate::config::{Config, NotificationsConfig};
use crate::error::Result;
use crate::followups::{Followup, FollowupsDb};
use crate::sender::Sender;
use crate::transcripts::{Transcript, TranscriptsDb};
use chrono::Local;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::time::{interval, Duration};
use tracing::{debug, error, info, warn};

pub struct NotificationScheduler {
    followups_db_path: std::path::PathBuf,
    transcripts_db_path: std::path::PathBuf,
    sender: Sender,
    invoker: ClaudeInvoker,
    config: Config,
    notif_config: NotificationsConfig,
    running: Arc<AtomicBool>,
}

impl NotificationScheduler {
    pub fn new(
        followups_db_path: std::path::PathBuf,
        transcripts_db_path: std::path::PathBuf,
        config: Config,
        notif_config: NotificationsConfig,
        running: Arc<AtomicBool>,
    ) -> Self {
        let sender = Sender::new(config.clone());
        let invoker = ClaudeInvoker::new(config.clone());

        NotificationScheduler {
            followups_db_path,
            transcripts_db_path,
            sender,
            invoker,
            config,
            notif_config,
            running,
        }
    }

    /// Run the scheduler loop. This should be spawned as a background task.
    pub async fn run(&self) {
        info!(
            "Notification scheduler started (interval: {}s)",
            self.notif_config.scheduler_interval_secs
        );

        let mut poll_interval =
            interval(Duration::from_secs(self.notif_config.scheduler_interval_secs));

        while self.running.load(Ordering::SeqCst) {
            poll_interval.tick().await;

            if let Err(e) = self.check_and_send().await {
                error!("Scheduler error: {}", e);
            }
        }

        info!("Notification scheduler stopped");
    }

    async fn check_and_send(&self) -> Result<()> {
        // Open fresh connections for this check cycle
        let db = FollowupsDb::open(&self.followups_db_path)?;
        let transcripts_db = TranscriptsDb::open(&self.transcripts_db_path)?;
        let due = db.get_due_followups()?;

        if !due.is_empty() {
            debug!("Found {} due followups", due.len());
        }

        for followup in due {
            // Check user preferences
            if !self.should_notify(&db, &followup)? {
                debug!(
                    "Skipping notification {} due to user preferences",
                    followup.id.unwrap_or(0)
                );
                continue;
            }

            info!(
                "Processing followup {}: {}",
                followup.id.unwrap_or(0),
                followup.description
            );

            // Build prompt for Claude with followup context
            let prompt = self.build_followup_prompt(&followup);

            // Invoke Claude to generate the follow-up message
            match self.invoker.invoke(&prompt, None).await {
                Ok(result) => {
                    // Store transcript of the followup invocation
                    let transcript = Transcript {
                        id: None,
                        message_guid: format!("followup-{}", followup.id.unwrap_or(0)),
                        chat_guid: followup.chat_guid.clone(),
                        session_id: result.session_id.clone(),
                        prompt_sent: prompt.clone(),
                        raw_stdout: result.raw_stdout.clone(),
                        raw_stderr: Some(result.raw_stderr.clone()),
                        exit_code: result.exit_code,
                        messages_json: result.messages_json.clone(),
                        tool_calls_json: result.tool_calls_json.clone(),
                        cost_usd: result.cost_usd,
                        input_tokens: result.input_tokens,
                        output_tokens: result.output_tokens,
                        claude_model: result.model.clone(),
                        working_dir: self.config.claude.working_dir.to_string_lossy().to_string(),
                        duration_ms: result.duration_ms,
                        created_at: chrono::Utc::now().timestamp_millis(),
                    };

                    if let Err(e) = transcripts_db.insert_transcript(&transcript) {
                        warn!("Failed to store followup transcript: {}", e);
                    }

                    // Send Claude's response as a threaded reply
                    match self
                        .sender
                        .send_reply_with_retry(
                            &followup.chat_guid,
                            &result.response_text,
                            &followup.original_message_guid,
                            self.notif_config.max_retries,
                        )
                        .await
                    {
                        Ok(guid) => {
                            info!(
                                "Sent followup notification {}: {}",
                                followup.id.unwrap_or(0),
                                followup.description
                            );
                            db.mark_sent(followup.id.unwrap(), &guid)?;

                            // Handle recurring
                            if followup.notify_interval_mins.is_some() {
                                db.reschedule_recurring(&followup)?;
                                info!(
                                    "Rescheduled recurring followup {}",
                                    followup.id.unwrap_or(0)
                                );
                            }
                        }
                        Err(e) => {
                            warn!(
                                "Failed to send followup notification {}: {}",
                                followup.id.unwrap_or(0),
                                e
                            );
                        }
                    }
                }
                Err(e) => {
                    error!(
                        "Claude invocation failed for followup {}: {}",
                        followup.id.unwrap_or(0),
                        e
                    );
                    // Don't mark as sent - will retry next cycle
                }
            }
        }

        Ok(())
    }

    fn build_followup_prompt(&self, followup: &Followup) -> String {
        let mut prompt = String::new();

        prompt.push_str("[FOLLOW-UP REMINDER]\n\n");
        prompt.push_str("You previously had a conversation with this user and a follow-up was scheduled.\n\n");

        prompt.push_str(&format!("Follow-up type: {}\n", followup.trigger_type));
        prompt.push_str(&format!("Description: {}\n", followup.description));

        if let Some(ctx) = &followup.context_summary {
            prompt.push_str(&format!("Context: {}\n", ctx));
        }

        prompt.push_str(&format!("\nUser: {}\n", followup.sender));
        prompt.push_str(&format!("Chat: {}\n", followup.chat_guid));

        if followup.notify_interval_mins.is_some() {
            prompt.push_str("\nThis is a recurring reminder.\n");
        }

        prompt.push_str("\n[INSTRUCTIONS]\n");
        prompt.push_str("Generate a friendly follow-up message to send to the user about this topic.\n\n");
        prompt.push_str("If the follow-up requires checking on something, you CAN use tools to investigate ");
        prompt.push_str("(check files, run commands, etc) BEFORE composing your message.\n\n");
        prompt.push_str("CRITICAL OUTPUT RULES:\n");
        prompt.push_str("- Do NOT output ANY text until you are ready to output the FINAL message\n");
        prompt.push_str("- Do NOT narrate what you're doing (no 'Let me check...', 'I found...', etc)\n");
        prompt.push_str("- Do NOT output status updates or intermediate findings\n");
        prompt.push_str("- Your ONLY text output should be the message itself - nothing else\n");
        prompt.push_str("- The system sends whatever you output as the text message, so keep it clean\n\n");
        prompt.push_str("SENDING RULES:\n");
        prompt.push_str("- Do NOT send this reminder yourself - the system delivers it automatically\n");
        prompt.push_str("- EXCEPTION: If the user previously asked you to text someone ELSE, you may do that\n");

        prompt
    }

    fn should_notify(&self, db: &FollowupsDb, followup: &Followup) -> Result<bool> {
        let pref = db.get_preference(&followup.chat_guid)?;

        if let Some(pref) = pref {
            // Check if notifications are disabled
            if !pref.notifications_enabled {
                return Ok(false);
            }

            // Check quiet hours
            if let (Some(start), Some(end)) = (pref.quiet_hours_start, pref.quiet_hours_end) {
                let now = Local::now();
                let current_time = (now.format("%H").to_string().parse::<i32>().unwrap_or(0) * 100)
                    + now.format("%M").to_string().parse::<i32>().unwrap_or(0);

                if Self::is_in_quiet_hours(current_time, start, end) {
                    return Ok(false);
                }
            }
        }

        Ok(true)
    }

    fn is_in_quiet_hours(current: i32, start: i32, end: i32) -> bool {
        // Handle wrapping (e.g., 2200-0800 means 10pm to 8am)
        if start > end {
            // Quiet hours span midnight
            current >= start || current <= end
        } else {
            // Quiet hours within same day
            current >= start && current <= end
        }
    }
}
