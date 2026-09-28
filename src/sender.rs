use crate::config::Config;
use crate::error::{Result, SinkError};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};
use uuid::Uuid;

#[derive(Debug, Serialize)]
struct SendMessageRequest {
    #[serde(rename = "chatGuid")]
    chat_guid: String,
    message: String,
    method: String,
    #[serde(rename = "tempGuid")]
    temp_guid: String,
}

#[derive(Debug, Serialize)]
struct SendReplyRequest {
    #[serde(rename = "chatGuid")]
    chat_guid: String,
    message: String,
    method: String,
    #[serde(rename = "tempGuid")]
    temp_guid: String,
    #[serde(rename = "selectedMessageGuid")]
    selected_message_guid: String,
}

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    status: i32,
    message: String,
    data: Option<T>,
}

#[derive(Debug, Deserialize)]
struct SentMessage {
    guid: Option<String>,
}

pub struct Sender {
    client: reqwest::Client,
    config: Config,
}

impl Sender {
    pub fn new(config: Config) -> Self {
        Sender {
            client: reqwest::Client::new(),
            config,
        }
    }

    /// Durable outbox sends require an actual successful API receipt. A timeout,
    /// error status, or missing GUID leaves the saved reply eligible for retry.
    pub async fn send_queued_reply(&self, chat_guid: &str, message: &str, command_id: &str) -> Result<String> {
        let request = SendMessageRequest {
            chat_guid: chat_guid.to_string(),
            message: message.to_string(),
            method: "private-api".to_string(),
            temp_guid: format!("sink-reply-{command_id}"),
        };
        let response = self.client
            .post(format!("{}/api/v1/message/text", self.config.bluebubbles_url()))
            .query(&[("password", &self.config.bluebubbles.password)])
            .timeout(std::time::Duration::from_secs(30))
            .json(&request)
            .send().await
            .map_err(|e| SinkError::Http(e.without_url()))?;
        if !response.status().is_success() {
            return Err(SinkError::BlueBubbles(format!("Reply send failed with status {}", response.status())));
        }
        let receipt: ApiResponse<SentMessage> = response.json().await
            .map_err(|e| SinkError::Http(e.without_url()))?;
        if receipt.status != 200 {
            return Err(SinkError::BlueBubbles(format!("Reply send failed ({}): {}", receipt.status, receipt.message)));
        }
        receipt.data.and_then(|data| data.guid).filter(|guid| !guid.is_empty())
            .ok_or_else(|| SinkError::BlueBubbles("Reply send returned no message GUID".into()))
    }

    pub async fn send_message(&self, chat_guid: &str, message: &str) -> Result<Option<String>> {
        let url = format!(
            "{}/api/v1/message/text?password={}",
            self.config.bluebubbles_url(),
            urlencoding::encode(&self.config.bluebubbles.password)
        );

        let temp_guid = format!("sink-{}", Uuid::new_v4());

        let request = SendMessageRequest {
            chat_guid: chat_guid.to_string(),
            message: message.to_string(),
            method: "private-api".to_string(),
            temp_guid,
        };

        info!("Sending response to chat {}", chat_guid);
        debug!("Message length: {} chars", message.len());

        let response = self.client.post(&url).json(&request).send().await?;

        if !response.status().is_success() {
            return Err(SinkError::BlueBubbles(format!(
                "Send failed with status: {}",
                response.status()
            )));
        }

        let api_response: ApiResponse<SentMessage> = response.json().await?;

        if api_response.status != 200 {
            warn!("BlueBubbles returned status {}: {}", api_response.status, api_response.message);
            // Don't fail - message might still have been sent
        }

        let guid = api_response.data.and_then(|d| d.guid);
        info!("Message sent successfully, guid: {:?}", guid);

        Ok(guid)
    }

    pub async fn send_with_retry(
        &self,
        chat_guid: &str,
        message: &str,
        max_retries: u32,
    ) -> Result<Option<String>> {
        let mut last_error = None;

        for attempt in 1..=max_retries {
            match self.send_message(chat_guid, message).await {
                Ok(guid) => return Ok(guid),
                Err(e) => {
                    warn!("Send attempt {} failed: {}", attempt, e);
                    last_error = Some(e);

                    if attempt < max_retries {
                        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| SinkError::BlueBubbles("Send failed".to_string())))
    }

    /// Send a message as a reply to a specific message (threaded)
    pub async fn send_reply(
        &self,
        chat_guid: &str,
        message: &str,
        reply_to_guid: &str,
    ) -> Result<String> {
        let url = format!(
            "{}/api/v1/message/text?password={}",
            self.config.bluebubbles_url(),
            urlencoding::encode(&self.config.bluebubbles.password)
        );

        let temp_guid = format!("sink-notif-{}", Uuid::new_v4());

        let request = SendReplyRequest {
            chat_guid: chat_guid.to_string(),
            message: message.to_string(),
            method: "private-api".to_string(),
            temp_guid: temp_guid.clone(),
            selected_message_guid: reply_to_guid.to_string(),
        };

        info!(
            "Sending reply to chat {} (reply to: {})",
            chat_guid, reply_to_guid
        );
        debug!("Message length: {} chars", message.len());

        let response = self.client.post(&url).json(&request).send().await?;

        if !response.status().is_success() {
            return Err(SinkError::BlueBubbles(format!(
                "Send reply failed with status: {}",
                response.status()
            )));
        }

        let api_response: ApiResponse<SentMessage> = response.json().await?;

        if api_response.status != 200 {
            warn!(
                "BlueBubbles returned status {}: {}",
                api_response.status, api_response.message
            );
        }

        let guid = api_response
            .data
            .and_then(|d| d.guid)
            .unwrap_or_else(|| temp_guid);

        info!("Reply sent successfully, guid: {}", guid);
        Ok(guid)
    }

    pub async fn send_reply_with_retry(
        &self,
        chat_guid: &str,
        message: &str,
        reply_to_guid: &str,
        max_retries: u32,
    ) -> Result<String> {
        let mut last_error = None;

        for attempt in 1..=max_retries {
            match self.send_reply(chat_guid, message, reply_to_guid).await {
                Ok(guid) => return Ok(guid),
                Err(e) => {
                    warn!("Send reply attempt {} failed: {}", attempt, e);
                    last_error = Some(e);

                    if attempt < max_retries {
                        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| SinkError::BlueBubbles("Send reply failed".to_string())))
    }
}
