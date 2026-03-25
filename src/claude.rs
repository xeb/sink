use crate::config::Config;
use crate::db::Message;
use crate::error::{Result, SinkError};
use chrono::{TimeZone, Utc};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;
use tracing::{debug, error, info};

#[derive(Debug, Clone)]
pub struct ClaudeResult {
    pub response_text: String,
    pub session_id: Option<String>,
    pub raw_stdout: String,
    pub raw_stderr: String,
    pub exit_code: i32,
    pub messages_json: Option<String>,
    pub tool_calls_json: Option<String>,
    pub cost_usd: Option<f64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub model: Option<String>,
    pub duration_ms: i64,
}

pub struct ClaudeInvoker {
    config: Config,
}

impl ClaudeInvoker {
    pub fn new(config: Config) -> Self {
        ClaudeInvoker { config }
    }

    pub fn build_prompt(
        &self,
        current_message: &Message,
        history: &[Message],
        attachment_paths: &[String],
    ) -> String {
        let mut prompt = String::new();

        // System instructions
        prompt.push_str("[INSTRUCTIONS]\n\n");
        prompt.push_str("You are Claude, responding to a user via iMessage.\n\n");
        prompt.push_str("You have FULL tool access: Bash, Read, Write, Edit, WebFetch, WebSearch, and all other tools.\n");
        prompt.push_str("Use tools freely to fulfill requests — create files, run commands, search the web, query databases, deploy code, etc.\n");
        prompt.push_str("The ONLY restriction: Do NOT use tools to send iMessages/texts/notifications. Message delivery is handled automatically by the system.\n");
        prompt.push_str("Your final text output will be sent as an iMessage reply, so keep it concise and suitable for iMessage.\n\n");

        prompt.push_str("[FULL MESSAGE HISTORY ACCESS]\n\n");
        prompt.push_str("The recent conversation is shown below, but you can access the FULL message history for any chat using sqlite3.\n");
        prompt.push_str("Database: /var/lib/sink/messages.db\n");
        prompt.push_str("Table: messages (id, guid, chat_guid, sender, text, date_received, status, is_from_me)\n");
        prompt.push_str("Example queries:\n");
        prompt.push_str(&format!(
            "  sqlite3 /var/lib/sink/messages.db \"SELECT sender, text, datetime(date_received/1000, 'unixepoch') as time FROM messages WHERE chat_guid='{}' ORDER BY date_received DESC LIMIT 50;\"\n",
            current_message.chat_guid
        ));
        prompt.push_str("  sqlite3 /var/lib/sink/messages.db \"SELECT COUNT(*) FROM messages WHERE chat_guid='...';\"\n");
        prompt.push_str("Use this when you need more context than what's provided below, e.g. if the user references something from earlier in the conversation.\n\n");

        if !history.is_empty() {
            prompt.push_str("[Previous messages in this conversation]\n\n");

            for msg in history {
                let timestamp = Utc
                    .timestamp_millis_opt(msg.date_received)
                    .single()
                    .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_else(|| "unknown time".to_string());

                let sender = if msg.is_from_me {
                    "Claude".to_string()
                } else {
                    msg.sender.clone()
                };

                prompt.push_str(&format!("From {} ({}):\n{}\n\n", sender, timestamp, msg.text));
            }
        }

        prompt.push_str("[Current message - please respond to this]\n\n");

        let timestamp = Utc
            .timestamp_millis_opt(current_message.date_received)
            .single()
            .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "unknown time".to_string());

        prompt.push_str(&format!(
            "From {} ({}):\n{}",
            current_message.sender, timestamp, current_message.text
        ));

        // Add attachment file paths if present
        if !attachment_paths.is_empty() {
            prompt.push_str("\n\n[Attachments sent with this message - use the Read tool to view these files]\n");
            for path in attachment_paths {
                prompt.push_str(&format!("- {}\n", path));
            }
        }

        prompt
    }

    pub async fn invoke(
        &self,
        prompt: &str,
        session_id: Option<&str>,
    ) -> Result<ClaudeResult> {
        let start = Instant::now();

        let (output, _) = self.run_claude(prompt, session_id).await?;
        let duration_ms = start.elapsed().as_millis() as i64;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr_str = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code().unwrap_or(-1);

        if !output.status.success() {
            // If resume failed due to stale session, retry without it
            if session_id.is_some() && stderr_str.contains("No conversation found with session ID") {
                info!("Session expired, retrying without --resume");
                let (output2, _) = self.run_claude(prompt, None).await?;
                let retry_duration = start.elapsed().as_millis() as i64;

                let stdout2 = String::from_utf8_lossy(&output2.stdout).to_string();
                let stderr2 = String::from_utf8_lossy(&output2.stderr).to_string();
                let exit_code2 = output2.status.code().unwrap_or(-1);

                if !output2.status.success() {
                    error!("Claude retry failed with status {}: {}", output2.status, stderr2);
                    return Err(SinkError::Claude(format!(
                        "Process exited with status {}: {}",
                        output2.status, stderr2
                    )));
                }

                let result = self.parse_output(&stdout2, &stderr2, exit_code2, retry_duration)?;
                info!("Claude response length: {} chars (after session retry)", result.response_text.len());
                return Ok(result);
            }

            error!("Claude failed with status {}: {}", output.status, stderr_str);
            return Err(SinkError::Claude(format!(
                "Process exited with status {}: {}",
                output.status, stderr_str
            )));
        }

        debug!("Claude output length: {} chars", stdout.len());

        // Parse JSON output to extract all fields
        let result = self.parse_output(&stdout, &stderr_str, exit_code, duration_ms)?;

        info!("Claude response length: {} chars", result.response_text.len());
        Ok(result)
    }

    async fn run_claude(
        &self,
        prompt: &str,
        session_id: Option<&str>,
    ) -> Result<(std::process::Output, String)> {
        let mut cmd = Command::new(&self.config.claude.binary);

        cmd.arg("-p")
            .arg(prompt)
            .arg("--output-format")
            .arg("json")
            .arg("--dangerously-skip-permissions");

        if let Some(session) = session_id {
            cmd.arg("--resume").arg(session);
        }

        cmd.current_dir(&self.config.claude.working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        info!("Invoking Claude in {:?} (session: {:?})", self.config.claude.working_dir, session_id);
        debug!("Prompt length: {} chars", prompt.len());

        let timeout_duration = Duration::from_secs(30 * 60); // 30 minute hard limit
        let child = cmd.spawn()?;
        let pid = child.id();

        match tokio::time::timeout(timeout_duration, child.wait_with_output()).await {
            Ok(result) => {
                let output = result?;
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                Ok((output, stderr))
            }
            Err(_) => {
                error!("Claude process timed out after 30 minutes, killing pid {:?}", pid);
                if let Some(pid) = pid {
                    let _ = std::process::Command::new("kill").arg("-9").arg(pid.to_string()).output();
                }
                Err(SinkError::Claude("Process timed out after 30 minutes".to_string()))
            }
        }
    }

    fn parse_output(
        &self,
        stdout: &str,
        stderr: &str,
        exit_code: i32,
        duration_ms: i64,
    ) -> Result<ClaudeResult> {
        // Try to parse as JSON
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(stdout) {
            let session_id = json
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(String::from);

            // The response text is in the "result" field
            let response_text = json
                .get("result")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| {
                    // Fallback: try to get from messages array
                    json.get("messages")
                        .and_then(|m| m.as_array())
                        .and_then(|arr| arr.last())
                        .and_then(|msg| msg.get("content"))
                        .and_then(|c| c.as_str())
                        .map(String::from)
                        .unwrap_or_else(|| stdout.to_string())
                });

            // Extract messages array as JSON string
            let messages_json = json
                .get("messages")
                .map(|m| serde_json::to_string(m).unwrap_or_default());

            // Extract tool calls from messages
            let tool_calls_json = json.get("messages").and_then(|m| m.as_array()).map(|arr| {
                let tool_calls: Vec<&serde_json::Value> = arr
                    .iter()
                    .filter(|msg| {
                        msg.get("role")
                            .and_then(|r| r.as_str())
                            .map(|r| r == "tool" || r == "tool_use")
                            .unwrap_or(false)
                    })
                    .collect();
                serde_json::to_string(&tool_calls).unwrap_or_default()
            });

            // Extract cost and token info
            let cost_usd = json
                .get("cost_usd")
                .and_then(|v| v.as_f64())
                .or_else(|| json.get("total_cost").and_then(|v| v.as_f64()));

            let input_tokens = json
                .get("input_tokens")
                .and_then(|v| v.as_i64())
                .or_else(|| {
                    json.get("usage")
                        .and_then(|u| u.get("input_tokens"))
                        .and_then(|v| v.as_i64())
                });

            let output_tokens = json
                .get("output_tokens")
                .and_then(|v| v.as_i64())
                .or_else(|| {
                    json.get("usage")
                        .and_then(|u| u.get("output_tokens"))
                        .and_then(|v| v.as_i64())
                });

            let model = json
                .get("model")
                .and_then(|v| v.as_str())
                .map(String::from);

            return Ok(ClaudeResult {
                response_text,
                session_id,
                raw_stdout: stdout.to_string(),
                raw_stderr: stderr.to_string(),
                exit_code,
                messages_json,
                tool_calls_json,
                cost_usd,
                input_tokens,
                output_tokens,
                model,
                duration_ms,
            });
        }

        // If not JSON, return raw output (text mode fallback)
        Ok(ClaudeResult {
            response_text: stdout.to_string(),
            session_id: None,
            raw_stdout: stdout.to_string(),
            raw_stderr: stderr.to_string(),
            exit_code,
            messages_json: None,
            tool_calls_json: None,
            cost_usd: None,
            input_tokens: None,
            output_tokens: None,
            model: None,
            duration_ms,
        })
    }
}
