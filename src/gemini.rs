use crate::config::GeminiConfig;
use crate::error::{Result, SinkError};
use chrono::{Duration, Local, NaiveTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedFollowup {
    #[serde(rename = "type")]
    pub followup_type: String,
    pub description: String,
    pub notify_at_relative: String,
    pub recurring: Option<RecurringConfig>,
    pub context: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecurringConfig {
    pub interval_mins: i64,
    pub until_relative: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddressedCheck {
    pub addressed_to_claude: bool,
    pub reason: String,
}

#[derive(Debug, Serialize)]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(rename = "generationConfig")]
    generation_config: GenerationConfig,
}

#[derive(Debug, Serialize)]
struct GeminiContent {
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Serialize)]
struct GeminiPart {
    text: String,
}

#[derive(Debug, Serialize)]
struct GenerationConfig {
    #[serde(rename = "responseMimeType")]
    response_mime_type: String,
}

#[derive(Debug, Deserialize)]
struct GeminiResponse {
    candidates: Option<Vec<GeminiCandidate>>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidate {
    content: Option<GeminiContentResponse>,
}

#[derive(Debug, Deserialize)]
struct GeminiContentResponse {
    parts: Option<Vec<GeminiPartResponse>>,
}

#[derive(Debug, Deserialize)]
struct GeminiPartResponse {
    text: Option<String>,
}

pub struct GeminiExtractor {
    config: GeminiConfig,
    client: reqwest::Client,
    api_key: String,
}

impl GeminiExtractor {
    pub fn new(config: GeminiConfig) -> Option<Self> {
        let api_key = config.get_api_key()?;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .unwrap_or_default();

        Some(GeminiExtractor { config, client, api_key })
    }

    pub async fn extract_followups(
        &self,
        conversation: &str,
    ) -> Result<Vec<ExtractedFollowup>> {
        let prompt = self.build_prompt(conversation);

        let request = GeminiRequest {
            contents: vec![GeminiContent {
                parts: vec![GeminiPart { text: prompt }],
            }],
            generation_config: GenerationConfig {
                response_mime_type: "application/json".to_string(),
            },
        };

        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            self.config.model, self.api_key
        );

        debug!("Calling Gemini API for follow-up extraction");

        let response = self
            .client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| SinkError::Gemini(format!("HTTP request failed: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(SinkError::Gemini(format!(
                "API returned status {}: {}",
                status, body
            )));
        }

        let gemini_response: GeminiResponse = response
            .json()
            .await
            .map_err(|e| SinkError::Gemini(format!("Failed to parse response: {}", e)))?;

        // Extract text from response
        let text = gemini_response
            .candidates
            .and_then(|c| c.into_iter().next())
            .and_then(|c| c.content)
            .and_then(|c| c.parts)
            .and_then(|p| p.into_iter().next())
            .and_then(|p| p.text)
            .ok_or_else(|| SinkError::Gemini("No text in response".to_string()))?;

        debug!("Gemini response: {}", text);

        // Parse JSON array of followups
        let followups: Vec<ExtractedFollowup> = serde_json::from_str(&text)
            .map_err(|e| SinkError::Gemini(format!("Failed to parse followups JSON: {}", e)))?;

        Ok(followups)
    }

    /// Check if a message in a group chat is addressed to Claude
    pub async fn check_if_addressed_to_claude(
        &self,
        conversation: &str,
        current_message: &str,
        sender: &str,
    ) -> Result<AddressedCheck> {
        let prompt = self.build_addressed_check_prompt(conversation, current_message, sender);

        let request = GeminiRequest {
            contents: vec![GeminiContent {
                parts: vec![GeminiPart { text: prompt }],
            }],
            generation_config: GenerationConfig {
                response_mime_type: "application/json".to_string(),
            },
        };

        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            self.config.model, self.api_key
        );

        debug!("Calling Gemini API to check if message is addressed to Claude");

        let response = self
            .client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| SinkError::Gemini(format!("HTTP request failed: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(SinkError::Gemini(format!(
                "API returned status {}: {}",
                status, body
            )));
        }

        let gemini_response: GeminiResponse = response
            .json()
            .await
            .map_err(|e| SinkError::Gemini(format!("Failed to parse response: {}", e)))?;

        let text = gemini_response
            .candidates
            .and_then(|c| c.into_iter().next())
            .and_then(|c| c.content)
            .and_then(|c| c.parts)
            .and_then(|p| p.into_iter().next())
            .and_then(|p| p.text)
            .ok_or_else(|| SinkError::Gemini("No text in response".to_string()))?;

        debug!("Gemini addressed check response: {}", text);

        let check: AddressedCheck = serde_json::from_str(&text)
            .map_err(|e| SinkError::Gemini(format!("Failed to parse addressed check JSON: {}", e)))?;

        Ok(check)
    }

    fn build_addressed_check_prompt(&self, conversation: &str, current_message: &str, sender: &str) -> String {
        format!(
            r#"You are analyzing a group chat that includes an AI assistant named "Claude".
Determine if the CURRENT MESSAGE is directed at Claude and requires a response.

RULES:
1. If the message directly addresses Claude (e.g., "Claude, ...", "Hey Claude", "@Claude"), it IS addressed to Claude.
2. If the message asks a question or makes a request that seems intended for Claude (based on context), it IS addressed to Claude.
3. If the message is part of a conversation between other humans in the group, it is NOT addressed to Claude.
4. If Claude is mentioned in THIRD PERSON (e.g., "Claude told me...", "What Claude said was...", "Tell Claude that..."), it is NOT addressed to Claude.
5. When in doubt, assume the message is NOT for Claude. Err on the side of staying silent.

Return JSON with:
- addressed_to_claude: true/false
- reason: Brief explanation of why

CONVERSATION HISTORY (last 10 messages):
{}

CURRENT MESSAGE from {}:
"{}"

Analyze the current message and return JSON:
{{"addressed_to_claude": true/false, "reason": "..."}}"#,
            conversation,
            sender,
            current_message
        )
    }

    fn build_prompt(&self, conversation: &str) -> String {
        format!(
            r#"Analyze this conversation between a user and Claude. Extract any follow-up actions needed.

For each follow-up, identify:
1. Type: "explicit" (user said "remind me", "follow up", etc.) or "ai_detected" (implicit need)
2. Description: What needs to be followed up on
3. Timing: When the notification should be sent (parse relative times like "in 2 hours", "tomorrow", "next week")
4. Recurring: If the user wants repeated reminders, specify interval and optional end condition
5. Context: Brief summary of why this follow-up matters

Return JSON array of follow-ups, or empty array if none found.

Example output:
[
  {{
    "type": "explicit",
    "description": "Check if the deployment completed successfully",
    "notify_at_relative": "2 hours",
    "recurring": null,
    "context": "User deployed new feature and asked for reminder to verify"
  }},
  {{
    "type": "explicit",
    "description": "Daily standup reminder",
    "notify_at_relative": "tomorrow 9am",
    "recurring": {{
      "interval_mins": 1440,
      "until_relative": "2 weeks"
    }},
    "context": "User requested daily morning reminder for standup prep"
  }},
  {{
    "type": "ai_detected",
    "description": "Follow up on database migration decision",
    "notify_at_relative": "1 day",
    "recurring": null,
    "context": "User was weighing options between PostgreSQL and SQLite, seemed uncertain"
  }}
]

Conversation to analyze (last 10 messages):
{}
"#,
            conversation
        )
    }

    /// Parse a relative time string into an absolute Unix timestamp (ms)
    pub fn parse_relative_time(&self, relative: &str) -> Option<i64> {
        let relative = relative.trim().to_lowercase();
        let now = Utc::now();

        // Match patterns like "2 hours", "30 minutes", "1 day", "3 days"
        let duration_re = Regex::new(r"^(\d+)\s*(hour|hr|h|minute|min|m|day|d|week|w)s?$").ok()?;
        if let Some(caps) = duration_re.captures(&relative) {
            let amount: i64 = caps.get(1)?.as_str().parse().ok()?;
            let unit = caps.get(2)?.as_str();

            let duration = match unit {
                "hour" | "hr" | "h" => Duration::hours(amount),
                "minute" | "min" | "m" => Duration::minutes(amount),
                "day" | "d" => Duration::days(amount),
                "week" | "w" => Duration::weeks(amount),
                _ => return None,
            };

            return Some((now + duration).timestamp_millis());
        }

        // Match "in X hours/minutes/days"
        let in_duration_re =
            Regex::new(r"^in\s+(\d+)\s*(hour|hr|h|minute|min|m|day|d|week|w)s?$").ok()?;
        if let Some(caps) = in_duration_re.captures(&relative) {
            let amount: i64 = caps.get(1)?.as_str().parse().ok()?;
            let unit = caps.get(2)?.as_str();

            let duration = match unit {
                "hour" | "hr" | "h" => Duration::hours(amount),
                "minute" | "min" | "m" => Duration::minutes(amount),
                "day" | "d" => Duration::days(amount),
                "week" | "w" => Duration::weeks(amount),
                _ => return None,
            };

            return Some((now + duration).timestamp_millis());
        }

        // Match "tomorrow" with optional time
        if relative.starts_with("tomorrow") {
            let tomorrow = (now + Duration::days(1)).date_naive();
            let time = self.parse_time_from_str(&relative).unwrap_or(NaiveTime::from_hms_opt(9, 0, 0)?);
            let datetime = tomorrow.and_time(time);
            return Some(datetime.and_utc().timestamp_millis());
        }

        // Match "next week"
        if relative == "next week" {
            return Some((now + Duration::weeks(1)).timestamp_millis());
        }

        // Match specific times like "9am", "10pm", "14:00"
        if let Some(time) = self.parse_time_from_str(&relative) {
            let today = Local::now().date_naive();
            let datetime = today.and_time(time);
            let mut result = datetime.and_utc();

            // If the time has already passed today, schedule for tomorrow
            if result < now {
                result = result + Duration::days(1);
            }

            return Some(result.timestamp_millis());
        }

        warn!("Could not parse relative time: {}", relative);
        None
    }

    fn parse_time_from_str(&self, s: &str) -> Option<NaiveTime> {
        // Match "9am", "10pm", "9 am", "10 pm"
        let ampm_re = Regex::new(r"(\d{1,2})\s*(am|pm)").ok()?;
        if let Some(caps) = ampm_re.captures(s) {
            let hour: u32 = caps.get(1)?.as_str().parse().ok()?;
            let ampm = caps.get(2)?.as_str();

            let hour_24 = match ampm {
                "am" => {
                    if hour == 12 {
                        0
                    } else {
                        hour
                    }
                }
                "pm" => {
                    if hour == 12 {
                        12
                    } else {
                        hour + 12
                    }
                }
                _ => return None,
            };

            return NaiveTime::from_hms_opt(hour_24, 0, 0);
        }

        // Match "14:00", "9:30"
        let time_re = Regex::new(r"(\d{1,2}):(\d{2})").ok()?;
        if let Some(caps) = time_re.captures(s) {
            let hour: u32 = caps.get(1)?.as_str().parse().ok()?;
            let minute: u32 = caps.get(2)?.as_str().parse().ok()?;
            return NaiveTime::from_hms_opt(hour, minute, 0);
        }

        None
    }

    /// Parse a relative duration string into minutes for recurring intervals
    pub fn parse_relative_duration_mins(&self, relative: &str) -> Option<i64> {
        let relative = relative.trim().to_lowercase();

        let duration_re = Regex::new(r"^(\d+)\s*(hour|hr|h|minute|min|m|day|d|week|w)s?$").ok()?;
        if let Some(caps) = duration_re.captures(&relative) {
            let amount: i64 = caps.get(1)?.as_str().parse().ok()?;
            let unit = caps.get(2)?.as_str();

            return match unit {
                "hour" | "hr" | "h" => Some(amount * 60),
                "minute" | "min" | "m" => Some(amount),
                "day" | "d" => Some(amount * 24 * 60),
                "week" | "w" => Some(amount * 7 * 24 * 60),
                _ => None,
            };
        }

        None
    }
}
