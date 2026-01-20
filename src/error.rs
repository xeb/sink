use thiserror::Error;

#[derive(Error, Debug)]
pub enum SinkError {
    #[error("BlueBubbles API error: {0}")]
    BlueBubbles(String),

    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Config error: {0}")]
    Config(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Claude invocation failed: {0}")]
    Claude(String),

    #[error("Gemini API error: {0}")]
    Gemini(String),

    #[error("Notification error: {0}")]
    Notification(String),

    #[error("Command parse error: {0}")]
    CommandParse(String),
}

pub type Result<T> = std::result::Result<T, SinkError>;
