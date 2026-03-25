# Image Handling Specification

This document specifies how Sink handles image and media attachments received via iMessage/BlueBubbles.

## Overview

When a user sends an image (or other media) via iMessage, the Sink daemon:
1. Detects attachments in the message
2. Downloads them from BlueBubbles
3. Saves them locally to `/tmp/sink/`
4. Converts HEIC images to JPG
5. Passes file paths to Claude Code as part of the prompt

## BlueBubbles API

### Server Requirements
- BlueBubbles Server v1.9.9+ (confirmed working)
- Private API enabled

### Fetching Attachments with Messages

Modify the message query to include attachments:

```rust
// In poller.rs - QueryRequest
#[derive(Debug, Serialize)]
struct QueryRequest {
    limit: i32,
    sort: String,
    with: Vec<String>,  // Add "attachment" here
}

// Usage
let request = QueryRequest {
    limit,
    sort: "DESC".to_string(),
    with: vec!["chat".to_string(), "handle".to_string(), "attachment".to_string()],
};
```

### Attachment Object Structure

```json
{
  "originalROWID": 6264,
  "guid": "D4145F9B-B61A-4745-89A1-AEAADD56F9D2",
  "uti": "public.heic",
  "mimeType": "image/heic",
  "transferName": "IMG_8926.HEIC",
  "totalBytes": 1510471,
  "transferState": 5,
  "isOutgoing": false,
  "hideAttachment": false,
  "isSticker": false,
  "originalGuid": "D4145F9B-B61A-4745-89A1-AEAADD56F9D2",
  "hasLivePhoto": true
}
```

### Download Endpoint

```
GET /api/v1/attachment/{guid}/download?password={password}
```

Returns raw file bytes with appropriate content-type header.

## Implementation

### 1. Add Attachment Struct

```rust
// In poller.rs or new attachments.rs
#[derive(Debug, Deserialize, Clone)]
pub struct Attachment {
    pub guid: String,
    #[serde(rename = "mimeType")]
    pub mime_type: Option<String>,
    pub uti: Option<String>,
    #[serde(rename = "transferName")]
    pub transfer_name: Option<String>,
    #[serde(rename = "totalBytes")]
    pub total_bytes: i64,
    #[serde(rename = "isOutgoing")]
    pub is_outgoing: bool,
    #[serde(rename = "hasLivePhoto")]
    pub has_live_photo: bool,
}
```

### 2. Update BlueBubblesMessage

```rust
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
    #[serde(default)]
    attachments: Vec<Attachment>,  // ADD THIS
}
```

### 3. Update Message Struct

```rust
// In db.rs
pub struct Message {
    // ... existing fields ...
    pub attachment_paths: Option<Vec<String>>,  // Local paths after download
}
```

### 4. Attachment Downloader

Create new module `src/attachments.rs`:

```rust
use std::path::PathBuf;
use std::process::Command;
use tokio::fs;
use crate::config::Config;
use crate::error::Result;

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

        // Build unique filename: {message_guid}_{attachment_guid}_{original_name}
        let original_name = attachment.transfer_name.as_deref().unwrap_or("attachment");
        let filename = format!("{}_{}_{}",
            &message_guid[..8],  // First 8 chars of message GUID
            &attachment.guid[..8],
            original_name
        );
        let file_path = PathBuf::from(ATTACHMENT_DIR).join(&filename);

        // Download from BlueBubbles
        let url = format!(
            "{}/api/v1/attachment/{}/download?password={}",
            self.config.bluebubbles_url(),
            attachment.guid,
            urlencoding::encode(&self.config.bluebubbles.password)
        );

        let response = self.client.get(&url).send().await?;
        let bytes = response.bytes().await?;
        fs::write(&file_path, &bytes).await?;

        // Convert HEIC to JPG if needed
        let final_path = self.maybe_convert_heic(&file_path, attachment).await?;

        Ok(final_path)
    }

    /// Convert HEIC to JPG, returns new path or original if not HEIC
    async fn maybe_convert_heic(&self, path: &PathBuf, attachment: &Attachment) -> Result<PathBuf> {
        let is_heic = attachment.mime_type.as_deref() == Some("image/heic")
            || attachment.uti.as_deref() == Some("public.heic")
            || path.extension().map(|e| e.to_ascii_lowercase()) == Some("heic".into());

        if !is_heic {
            return Ok(path.clone());
        }

        // Create JPG path
        let jpg_path = path.with_extension("jpg");

        // Use heif-convert (Linux) or sips (macOS)
        let result = if cfg!(target_os = "macos") {
            Command::new("sips")
                .args(["-s", "format", "jpeg", path.to_str().unwrap(), "--out", jpg_path.to_str().unwrap()])
                .output()
        } else {
            Command::new("heif-convert")
                .args([path.to_str().unwrap(), jpg_path.to_str().unwrap()])
                .output()
        };

        match result {
            Ok(output) if output.status.success() => {
                // Remove original HEIC
                let _ = fs::remove_file(path).await;
                Ok(jpg_path)
            }
            _ => {
                // Conversion failed, return original
                tracing::warn!("HEIC conversion failed for {:?}, using original", path);
                Ok(path.clone())
            }
        }
    }

    /// Download all attachments for a message
    pub async fn download_all(&self, attachments: &[Attachment], message_guid: &str) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for att in attachments {
            match self.download(att, message_guid).await {
                Ok(path) => paths.push(path),
                Err(e) => tracing::error!("Failed to download attachment {}: {}", att.guid, e),
            }
        }
        paths
    }
}
```

### 5. Update Claude Prompt

When building the prompt for Claude, include attachment paths:

```rust
// In claude.rs or wherever prompt is built
fn build_prompt(message: &Message, history: &[Message]) -> String {
    let mut prompt = String::new();

    // Add attachment context if present
    if let Some(paths) = &message.attachment_paths {
        if !paths.is_empty() {
            prompt.push_str("\n[Attachments sent with this message - use Read tool to view them]\n");
            for path in paths {
                prompt.push_str(&format!("- {}\n", path));
            }
            prompt.push_str("\n");
        }
    }

    // Rest of prompt building...
    prompt
}
```

### 6. Integration in Main Loop

```rust
// In main.rs or processor
async fn process_message(msg: Message, downloader: &AttachmentDownloader) -> Result<()> {
    // Download attachments if present
    let attachment_paths = if !msg.attachments.is_empty() {
        Some(downloader.download_all(&msg.attachments, &msg.guid).await
            .into_iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect())
    } else {
        None
    };

    // Update message with paths
    let msg = Message { attachment_paths, ..msg };

    // Process with Claude...
}
```

## File Storage

### Location
- Base directory: `/tmp/sink/`
- Files are stored with unique names to avoid collisions
- Pattern: `{msg_guid_prefix}_{att_guid_prefix}_{original_filename}`

### Cleanup
- Files in `/tmp/sink/` can be cleaned up periodically
- Consider: delete after Claude processes, or keep for N hours
- System temp directory typically clears on reboot

### Example
```
/tmp/sink/CEC6B5AD_D4145F9B_IMG_8926.jpg
/tmp/sink/ABC12345_DEF67890_screenshot.png
/tmp/sink/XYZ99999_AAA11111_document.pdf
```

## Supported File Types

### Images (converted if needed)
- HEIC → JPG (auto-converted)
- JPEG/JPG (passed directly)
- PNG (passed directly)
- GIF (passed directly)
- WEBP (passed directly)

### Videos
- MP4, MOV, etc. (passed directly, Claude can analyze frames)

### Documents
- PDF (passed directly)
- Other documents (passed directly)

### Audio
- Audio files (passed directly, transcription may be limited)

## Dependencies

### Linux (not-invented-here)
```bash
# For HEIC conversion
sudo apt install libheif-examples  # provides heif-convert
```

### macOS
- Uses built-in `sips` command (no additional dependencies)

## Error Handling

1. **Download failure**: Log error, continue processing message without that attachment
2. **Conversion failure**: Use original file, log warning
3. **Disk space**: Monitor `/tmp/sink/` usage, implement cleanup if needed
4. **Large files**: No size limits initially, but log file sizes for monitoring

## Future Enhancements

1. **Persistent storage**: Option to save attachments to `/var/lib/sink/attachments/`
2. **Thumbnail generation**: Create smaller previews for large images
3. **Video frame extraction**: Extract key frames for better analysis
4. **Compression**: Optional compression for very large images
5. **Deduplication**: Skip re-downloading identical attachments
