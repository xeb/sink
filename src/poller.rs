use crate::config::Config;
use crate::db::{Database, Message};
use crate::error::{Result, SinkError};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    status: i32,
    message: String,
    data: T,
}

#[derive(Debug, Deserialize)]
struct BlueBubblesMessage {
    guid: String,
    text: Option<String>,
    #[serde(rename = "isFromMe")]
    is_from_me: bool,
    #[serde(rename = "dateCreated")]
    date_created: i64,
    chats: Vec<Chat>,
    handle: Option<Handle>,
}

#[derive(Debug, Deserialize)]
struct Chat {
    guid: String,
}

#[derive(Debug, Deserialize)]
struct Handle {
    address: String,
}

#[derive(Debug, Serialize)]
struct QueryRequest {
    limit: i32,
    sort: String,
    with: Vec<String>,
}

pub struct Poller {
    client: reqwest::Client,
    config: Config,
}

impl Poller {
    pub fn new(config: Config) -> Self {
        Poller {
            client: reqwest::Client::new(),
            config,
        }
    }

    pub async fn fetch_recent_messages(&self, limit: i32) -> Result<Vec<Message>> {
        let url = format!(
            "{}/api/v1/message/query?password={}",
            self.config.bluebubbles_url(),
            urlencoding::encode(&self.config.bluebubbles.password)
        );

        let request = QueryRequest {
            limit,
            sort: "DESC".to_string(),
            with: vec!["chat".to_string(), "handle".to_string()],
        };

        debug!("Fetching {} recent messages from BlueBubbles", limit);

        let response = self
            .client
            .post(&url)
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err(SinkError::BlueBubbles(format!(
                "API returned status: {}",
                response.status()
            )));
        }

        let api_response: ApiResponse<Vec<BlueBubblesMessage>> = response.json().await?;

        if api_response.status != 200 {
            return Err(SinkError::BlueBubbles(api_response.message));
        }

        let messages: Vec<Message> = api_response
            .data
            .into_iter()
            .filter_map(|m| {
                let text = m.text?;
                if text.is_empty() {
                    return None;
                }

                let chat_guid = m.chats.first()?.guid.clone();
                let sender = m
                    .handle
                    .map(|h| h.address)
                    .unwrap_or_else(|| "unknown".to_string());

                Some(Message {
                    id: None,
                    guid: m.guid,
                    chat_guid,
                    sender,
                    text,
                    date_received: m.date_created,
                    processed_at: None,
                    response_guid: None,
                    session_id: None,
                    status: "pending".to_string(),
                    is_from_me: m.is_from_me,
                })
            })
            .collect();

        debug!("Fetched {} messages with text content", messages.len());
        Ok(messages)
    }

    /// Poll for messages and store them. Only returns messages received after `startup_time`.
    pub async fn poll_and_store(&self, db: &Database, startup_time: i64) -> Result<Vec<Message>> {
        let messages = self.fetch_recent_messages(50).await?;
        let mut new_messages = Vec::new();

        for msg in messages {
            if !db.message_exists(&msg.guid)? {
                db.insert_message(&msg)?;

                // Only queue for processing if message arrived after daemon started
                if !msg.is_from_me && msg.date_received >= startup_time {
                    info!(
                        "New incoming message from {} in chat {}",
                        msg.sender, msg.chat_guid
                    );
                    new_messages.push(msg);
                } else if !msg.is_from_me {
                    // Mark historical messages as skipped so we don't process them later
                    db.update_status(&msg.guid, "skipped")?;
                    debug!("Skipped historical message from {}", msg.sender);
                } else {
                    debug!("Stored outgoing message {}", msg.guid);
                }
            }
        }

        Ok(new_messages)
    }
}
