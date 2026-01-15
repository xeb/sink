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
}

#[derive(Debug, Deserialize, Clone)]
pub struct DatabaseConfig {
    pub path: PathBuf,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ContextConfig {
    pub message_history_count: usize,
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
                host: "your-bluebubbles-host".to_string(),
                port: 1235,
                password: "CHANGEME_PASSWORD".to_string(),
            },
            claude: ClaudeConfig {
                working_dir: PathBuf::from("/path/to/working/directory"),
                binary: "claude".to_string(),
            },
            polling: PollingConfig {
                interval_secs: 5,
            },
            database: DatabaseConfig {
                path: PathBuf::from("/var/lib/sink/messages.db"),
            },
            context: ContextConfig {
                message_history_count: 10,
            },
        }
    }
}
