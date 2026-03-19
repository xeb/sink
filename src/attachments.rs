use crate::config::Config;
use crate::error::Result;
use crate::poller::Attachment;
use std::path::PathBuf;
use std::process::Command;
use tokio::fs;
use tracing::{debug, error, info, warn};

const ATTACHMENT_DIR: &str = "/tmp/sink";

pub struct AttachmentDownloader {
    client: reqwest::Client,
    config: Config,
}

impl AttachmentDownloader {
    pub fn new(config: Config) -> Self {
        Self {
            client: reqwest::Client::new(),
            config,
        }
    }

    /// Download attachment and return local file path
    pub async fn download(&self, attachment: &Attachment, message_guid: &str) -> Result<PathBuf> {
        // Ensure directory exists
        fs::create_dir_all(ATTACHMENT_DIR).await?;

        // Build unique filename: {message_guid_prefix}_{attachment_guid_prefix}_{original_name}
        let original_name = attachment
            .transfer_name
            .as_deref()
            .unwrap_or("attachment");
        let msg_prefix = if message_guid.len() >= 8 {
            &message_guid[..8]
        } else {
            message_guid
        };
        let att_prefix = if attachment.guid.len() >= 8 {
            &attachment.guid[..8]
        } else {
            &attachment.guid
        };
        let filename = format!("{}_{}_{}", msg_prefix, att_prefix, original_name);
        let file_path = PathBuf::from(ATTACHMENT_DIR).join(&filename);

        // Download from BlueBubbles
        let url = format!(
            "{}/api/v1/attachment/{}/download?password={}",
            self.config.bluebubbles_url(),
            attachment.guid,
            urlencoding::encode(&self.config.bluebubbles.password)
        );

        debug!("Downloading attachment {} to {:?}", attachment.guid, file_path);

        let response = self.client.get(&url).send().await?;
        if !response.status().is_success() {
            return Err(crate::error::SinkError::BlueBubbles(format!(
                "Failed to download attachment: {}",
                response.status()
            )));
        }

        let bytes = response.bytes().await?;
        fs::write(&file_path, &bytes).await?;

        info!(
            "Downloaded attachment {} ({} bytes)",
            original_name,
            bytes.len()
        );

        // Convert HEIC to JPG if needed
        let final_path = self.maybe_convert_heic(&file_path, attachment).await?;

        Ok(final_path)
    }

    /// Convert HEIC to JPG, returns new path or original if not HEIC
    async fn maybe_convert_heic(
        &self,
        path: &PathBuf,
        attachment: &Attachment,
    ) -> Result<PathBuf> {
        let is_heic = attachment.mime_type.as_deref() == Some("image/heic")
            || attachment.uti.as_deref() == Some("public.heic")
            || path
                .extension()
                .map(|e| e.to_ascii_lowercase().to_string_lossy() == "heic")
                .unwrap_or(false);

        if !is_heic {
            return Ok(path.clone());
        }

        // Create JPG path
        let jpg_path = path.with_extension("jpg");

        debug!("Converting HEIC to JPG: {:?} -> {:?}", path, jpg_path);

        // Use heif-convert (Linux) or sips (macOS)
        let result = if cfg!(target_os = "macos") {
            Command::new("sips")
                .args([
                    "-s",
                    "format",
                    "jpeg",
                    path.to_str().unwrap(),
                    "--out",
                    jpg_path.to_str().unwrap(),
                ])
                .output()
        } else {
            Command::new("heif-convert")
                .args([path.to_str().unwrap(), jpg_path.to_str().unwrap()])
                .output()
        };

        match result {
            Ok(output) if output.status.success() => {
                info!("Converted HEIC to JPG: {:?}", jpg_path);
                // Remove original HEIC
                if let Err(e) = fs::remove_file(path).await {
                    warn!("Failed to remove original HEIC: {}", e);
                }
                Ok(jpg_path)
            }
            Ok(output) => {
                warn!(
                    "HEIC conversion failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                Ok(path.clone())
            }
            Err(e) => {
                warn!("HEIC conversion command failed: {}", e);
                Ok(path.clone())
            }
        }
    }

    /// Download all attachments for a message, returns list of local file paths
    pub async fn download_all(
        &self,
        attachments: &[Attachment],
        message_guid: &str,
    ) -> Vec<String> {
        let mut paths = Vec::new();

        for att in attachments {
            match self.download(att, message_guid).await {
                Ok(path) => {
                    paths.push(path.to_string_lossy().to_string());
                }
                Err(e) => {
                    error!("Failed to download attachment {}: {}", att.guid, e);
                }
            }
        }

        paths
    }
}
