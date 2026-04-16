use crate::config::GmailConfig;
use crate::error::Result;
use crate::sender::Sender;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tracing::{error, info, warn};

const CHECK_EVERY_N: u64 = 4;

/// Holds a pending gmail-cli login subprocess waiting for the auth callback URL
pub struct GmailAuthPending {
    child: Child,
}

impl GmailAuthPending {
    /// Feed the localhost callback URL to the waiting login process
    pub async fn complete(mut self, callback_url: &str) -> bool {
        if let Some(ref mut stdin) = self.child.stdin {
            let input = format!("{}\n", callback_url);
            if let Err(e) = stdin.write_all(input.as_bytes()).await {
                error!("Failed to write callback URL to gmail-cli stdin: {}", e);
                return false;
            }
            // Wait for process to finish
            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                self.child.wait(),
            )
            .await
            {
                Ok(Ok(status)) => {
                    if status.success() {
                        info!("Gmail re-authentication completed successfully");
                        true
                    } else {
                        error!("gmail-cli login exited with status: {}", status);
                        false
                    }
                }
                Ok(Err(e)) => {
                    error!("gmail-cli login process error: {}", e);
                    false
                }
                Err(_) => {
                    error!("gmail-cli login timed out after sending callback URL");
                    let _ = self.child.kill().await;
                    false
                }
            }
        } else {
            error!("gmail-cli login process has no stdin handle");
            false
        }
    }
}

impl Drop for GmailAuthPending {
    fn drop(&mut self) {
        // Kill the process if it's still running when dropped
        let _ = self.child.start_kill();
    }
}

/// Check if the message counter warrants a gmail auth check
pub fn should_check(_message_count: u64) -> bool {
    // TEMPORARILY DISABLED - gmail auth feature needs debugging
    false
    // message_count > 0 && message_count % CHECK_EVERY_N == 0
}

/// Returns true if the message text is a localhost callback URL (for gmail auth)
pub fn is_callback_url(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.starts_with("http://localhost:8085")
}

/// Run `gmail-cli status` to check if authenticated.
/// Returns true if authenticated, false otherwise.
pub async fn check_auth(cfg: &GmailConfig) -> bool {
    match Command::new(&cfg.cli_path)
        .args(["status", "--account", &cfg.account])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
    {
        Ok(output) => {
            if output.status.success() {
                info!("Gmail auth check: authenticated");
                true
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn!("Gmail auth check: not authenticated ({})", stderr.trim());
                false
            }
        }
        Err(e) => {
            error!("Failed to run gmail-cli status: {}", e);
            // Don't trigger login on binary-not-found errors
            true
        }
    }
}

/// Start the gmail-cli login process and extract the auth URL.
/// Returns the auth URL and the pending child process, or None on failure.
pub async fn start_login(cfg: &GmailConfig) -> Option<(String, GmailAuthPending)> {
    let mut child = match Command::new(&cfg.cli_path)
        .args(["login", "--account", &cfg.account])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to spawn gmail-cli login: {}", e);
            return None;
        }
    };

    // Read stdout until we find the auth URL
    // gmail-cli prints: "Visit this URL to authorize:\n<URL>\n"
    // Then prints: "Waiting (or paste URL): " and blocks on stdin
    let stdout = child.stdout.take()?;
    let mut reader = tokio::io::BufReader::new(stdout);
    let mut auth_url: Option<String> = None;

    // Read lines looking for the URL (with timeout)
    let read_result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        use tokio::io::AsyncBufReadExt;
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.starts_with("https://accounts.google.com/") {
                        return Some(trimmed.to_string());
                    }
                }
                Err(e) => {
                    error!("Error reading gmail-cli stdout: {}", e);
                    break;
                }
            }
        }
        None
    })
    .await;

    match read_result {
        Ok(Some(url)) => {
            info!("Got Gmail auth URL: {}...", &url[..url.len().min(80)]);
            Some((url, GmailAuthPending { child }))
        }
        Ok(None) => {
            error!("gmail-cli login did not produce an auth URL");
            let _ = child.kill().await;
            None
        }
        Err(_) => {
            error!("Timed out waiting for gmail-cli login auth URL");
            let _ = child.kill().await;
            None
        }
    }
}

/// Send the auth URL and a request to re-authenticate to the owner
pub async fn send_auth_request(sender: &Sender, cfg: &GmailConfig, auth_url: &str) {
    let msg = format!(
        "Gmail authentication has expired for {}. Please re-authenticate by clicking the link below, then text back the localhost URL it redirects to.",
        cfg.account
    );
    if let Err(e) = sender.send_with_retry(&cfg.owner_chat_guid, &msg, 3).await {
        error!("Failed to send gmail auth request: {}", e);
    }

    if let Err(e) = sender.send_with_retry(&cfg.owner_chat_guid, auth_url, 3).await {
        error!("Failed to send gmail auth URL: {}", e);
    }
}
