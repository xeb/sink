use crate::error::{Result, SinkError};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub bluebubbles: BlueBubblesConfig,
    pub claude: ClaudeConfig,
    pub polling: PollingConfig,
    pub database: DatabaseConfig,
    pub context: ContextConfig,
    #[serde(default)]
    pub databases: Option<DatabasesConfig>,
    #[serde(default)]
    pub gemini: Option<GeminiConfig>,
    #[serde(default)]
    pub notifications: Option<NotificationsConfig>,
    #[serde(default)]
    pub web_server: Option<WebServerConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct BlueBubblesConfig {
    pub host: String,
    pub port: u16,
    pub password: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ClaudeConfig {
    pub working_dir: PathBuf,
    pub binary: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PollingConfig {
    pub interval_secs: u64,
    #[serde(default = "default_batch_window")]
    pub batch_window_secs: u64,
}

fn default_batch_window() -> u64 {
    30
}

#[derive(Debug, Deserialize, Clone)]
pub struct DatabaseConfig {
    pub path: PathBuf,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ContextConfig {
    pub message_history_count: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub struct DatabasesConfig {
    pub messages: PathBuf,
    pub transcripts: PathBuf,
    pub followups: PathBuf,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GeminiConfig {
    #[serde(default)]
    pub api_key: String,  // Can be overridden by GEMINI_API_KEY env var
    #[serde(default = "default_gemini_model")]
    pub model: String,
    #[serde(default = "default_gemini_timeout")]
    pub timeout_secs: u64,
}

impl GeminiConfig {
    /// Get the API key, preferring environment variable over config file
    pub fn get_api_key(&self) -> Option<String> {
        std::env::var("GEMINI_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                if self.api_key.is_empty() {
                    None
                } else {
                    Some(self.api_key.clone())
                }
            })
    }
}

fn default_gemini_model() -> String {
    "gemini-3-flash-preview".to_string()
}

fn default_gemini_timeout() -> u64 {
    30
}

#[derive(Debug, Deserialize, Clone)]
pub struct NotificationsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_scheduler_interval")]
    pub scheduler_interval_secs: u64,
    #[serde(default = "default_snooze_mins")]
    pub default_snooze_mins: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
}

fn default_true() -> bool {
    true
}

fn default_scheduler_interval() -> u64 {
    60
}

fn default_snooze_mins() -> u64 {
    60
}

fn default_max_retries() -> u32 {
    3
}

#[derive(Debug, Deserialize, Clone)]
pub struct WebServerConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_web_port")]
    pub port: u16,
    #[serde(default = "default_web_host")]
    pub host: String,
}

fn default_web_port() -> u16 {
    1111
}

fn default_web_host() -> String {
    "0.0.0.0".to_string()
}

impl Default for WebServerConfig {
    fn default() -> Self {
        WebServerConfig {
            enabled: true,
            port: 1111,
            host: "0.0.0.0".to_string(),
        }
    }
}

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| SinkError::Config(format!("Failed to read config file: {}", e)))?;

        toml::from_str(&content)
            .map_err(|e| SinkError::Config(format!("Failed to parse config: {}", e)))
    }

    pub fn bluebubbles_url(&self) -> String {
        format!("http://{}:{}", self.bluebubbles.host, self.bluebubbles.port)
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            bluebubbles: BlueBubblesConfig {
                host: "localhost".to_string(),
                port: 1234,
                password: "changeme".to_string(),
            },
            claude: ClaudeConfig {
                working_dir: PathBuf::from("."),
                binary: "claude".to_string(),
            },
            polling: PollingConfig {
                interval_secs: 5,
                batch_window_secs: 30,
            },
            database: DatabaseConfig {
                path: PathBuf::from("/var/lib/sink/messages.db"),
            },
            context: ContextConfig {
                message_history_count: 20,
            },
            databases: None,
            gemini: None,
            notifications: None,
            web_server: None,
        }
    }
}
