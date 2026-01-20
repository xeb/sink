use crate::config::Config;
use crate::db::Message;
use crate::error::{Result, SinkError};
use chrono::{TimeZone, Utc};
use std::process::Stdio;
use std::time::Instant;
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

    pub fn build_prompt(&self, current_message: &Message, history: &[Message]) -> String {
        let mut prompt = String::new();

        // System instructions
        prompt.push_str("[INSTRUCTIONS]\n\n");
        prompt.push_str("YOU ARE A NESTED AGENT. You are Claude, an assistant responding via iMessage.\n\n");
        prompt.push_str("- NEVER use tools to send messages, texts, or notifications. The system handles all message delivery automatically.\n");
        prompt.push_str("- Just provide your text response to help with whatever the user asks.\n");
        prompt.push_str("- Focus on research, answers, and assistance based on the user's request.\n\n");

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
    ) -> Result<ClaudeResult> {
        let start = Instant::now();

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
        let duration_ms = start.elapsed().as_millis() as i64;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code().unwrap_or(-1);

        if !output.status.success() {
            error!("Claude failed with status {}: {}", output.status, stderr);
            return Err(SinkError::Claude(format!(
                "Process exited with status {}: {}",
                output.status, stderr
            )));
        }

        debug!("Claude output length: {} chars", stdout.len());

        // Parse JSON output to extract all fields
        let result = self.parse_output(&stdout, &stderr, exit_code, duration_ms)?;

        info!("Claude response length: {} chars", result.response_text.len());
        Ok(result)
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
