use crate::config::Config;
use crate::db::Message;
use crate::error::{Result, SinkError};
use chrono::{TimeZone, Utc};
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, error, info};

pub struct ClaudeInvoker {
    config: Config,
}

impl ClaudeInvoker {
    pub fn new(config: Config) -> Self {
        ClaudeInvoker { config }
    }

    pub fn build_prompt(&self, current_message: &Message, history: &[Message]) -> String {
        let mut prompt = String::new();

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

        prompt
    }

    pub async fn invoke(
        &self,
        prompt: &str,
        session_id: Option<&str>,
    ) -> Result<(String, Option<String>)> {
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

        info!("Invoking Claude in {:?}", self.config.claude.working_dir);
        debug!("Prompt length: {} chars", prompt.len());

        let output = cmd.output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!("Claude failed with status {}: {}", output.status, stderr);
            return Err(SinkError::Claude(format!(
                "Process exited with status {}: {}",
                output.status, stderr
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        debug!("Claude output length: {} chars", stdout.len());

        // Parse JSON output to extract response and session_id
        let (response, new_session_id) = self.parse_output(&stdout)?;

        info!("Claude response length: {} chars", response.len());
        Ok((response, new_session_id))
    }

    fn parse_output(&self, output: &str) -> Result<(String, Option<String>)> {
        // Try to parse as JSON
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(output) {
            let session_id = json
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(String::from);

            // The response text is in the "result" field
            let response = json
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
                        .unwrap_or_else(|| output.to_string())
                });

            return Ok((response, session_id));
        }

        // If not JSON, return raw output (text mode fallback)
        Ok((output.to_string(), None))
    }
}
