use regex::Regex;
use std::process::Command;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{debug, error, info};

#[derive(Debug, Clone)]
pub struct TmuxConfig {
    pub session: String,           // Session name (e.g., "main")
    pub window: String,            // Window name (e.g., "sink MASTER")
    pub prompt: String,            // Prompt string (e.g., "❯")
    pub timeout_secs: u64,         // Max wait time (default: 90)
    pub capture_lines: usize,      // Max lines to capture (default: 200)
    pub capture_interval_ms: u64,  // Poll interval (default: 200)
}

impl Default for TmuxConfig {
    fn default() -> Self {
        TmuxConfig {
            session: "main".to_string(),
            window: "sink MASTER".to_string(),
            prompt: "❯".to_string(),
            timeout_secs: 90,
            capture_lines: 200,
            capture_interval_ms: 200,
        }
    }
}

/// Send a command to tmux window and wait for Claude to finish processing
pub async fn execute_command(config: &TmuxConfig, command_text: &str) -> Result<String, String> {
    debug!("Sending command to {}:{}: {}", config.session, config.window, command_text);
    info!("TMUX: Starting command execution in {}:{}", config.session, config.window);

    let target = format!("{}:{}", config.session, config.window);

    // Step 0: Check if permission menu is open and dismiss it
    let content = capture_pane(&target)?;
    if content.contains("bypass permissions") {
        info!("TMUX: Detected permissions menu, dismissing with Tab");
        send_keys_key(&target, "Tab")?;
        // Wait a moment for the menu to process
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    // Step 1: Send command as literal text
    info!("TMUX: Sending literal text: {}", command_text);
    send_keys_literal(&target, command_text)?;
    info!("TMUX: Literal text sent successfully");

    // Step 2: Send Enter key
    info!("TMUX: Sending Enter key");
    send_keys_key(&target, "Enter")?;
    info!("TMUX: Enter key sent, waiting for prompt");

    // Step 3: Wait for [REPLY-ID] tags (Claude wraps response with matching ID)
    // Extract the ID from the command text (format: [CMD-XXXX]...[/CMD-XXXX])
    let cmd_id = if let Some(start) = command_text.find("[CMD-") {
        if let Some(end) = command_text[start + 5..].find("]") {
            command_text[start + 5..start + 5 + end].to_string()
        } else {
            return Err("Invalid command format".to_string());
        }
    } else {
        return Err("Command missing [CMD-ID] format".to_string());
    };

    info!("TMUX: Waiting for [REPLY-{}] tags (timeout: {}s, poll interval: {}ms)",
        cmd_id, config.timeout_secs, config.capture_interval_ms);
    let result = wait_for_reply_tags(&target, &cmd_id, config.timeout_secs, config.capture_interval_ms).await?;
    info!("TMUX: [REPLY-{}] tags detected, returning output: {} chars", cmd_id, result.len());

    // Return the raw output - caller extracts [REPLY-ID]...[/REPLY-ID] content
    Ok(result)
}

/// Send literal text to tmux window (via -l flag, prevents escape sequence interpretation)
fn send_keys_literal(target: &str, text: &str) -> Result<(), String> {
    let output = Command::new("tmux")
        .args(&["send-keys", "-t", target, "-l", text])
        .output()
        .map_err(|e| format!("Failed to execute tmux send-keys: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("tmux send-keys failed: {}", stderr));
    }

    Ok(())
}

/// Send a tmux key (like "Enter", "Escape", etc.)
fn send_keys_key(target: &str, key: &str) -> Result<(), String> {
    let output = Command::new("tmux")
        .args(&["send-keys", "-t", target, key])
        .output()
        .map_err(|e| format!("Failed to execute tmux send-keys: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("tmux send-keys key failed: {}", stderr));
    }

    Ok(())
}

/// Capture pane content from tmux window
fn capture_pane(target: &str) -> Result<String, String> {
    let output = Command::new("tmux")
        .args(&["capture-pane", "-t", target, "-p", "-S", "-100"])
        .output()
        .map_err(|e| format!("Failed to execute tmux capture-pane: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("tmux capture-pane failed: {}", stderr));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Wait for [REPLY-ID] tags in the output (Claude wraps response with matching ID)
async fn wait_for_reply_tags(target: &str, cmd_id: &str, timeout_secs: u64, poll_interval_ms: u64) -> Result<String, String> {
    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    let poll_interval = Duration::from_millis(poll_interval_ms);

    let reply_start_tag = format!("[REPLY-{}]", cmd_id);
    let reply_end_tag = format!("[/REPLY-{}]", cmd_id);

    let mut poll_count = 0;
    loop {
        poll_count += 1;
        let content = capture_pane(target)?;

        // Look for [REPLY-ID] tags (specific to this command)
        if content.contains(&reply_start_tag) && content.contains(&reply_end_tag) {
            info!("TMUX: Found [REPLY-{}] tags after {} polls", cmd_id, poll_count);
            return Ok(content);
        }

        // Check timeout
        if start.elapsed() > timeout {
            error!(
                "Timed out waiting for [REPLY-{}] tags after {} seconds ({} polls)",
                cmd_id, timeout_secs, poll_count
            );
            return Err(format!(
                "Timed out waiting for [REPLY-{}] tags after {} seconds",
                cmd_id, timeout_secs
            ));
        }

        if poll_count % 10 == 0 {
            debug!("TMUX: Still waiting for [REPLY-{}] tags (poll #{}, elapsed: {}s)",
                cmd_id, poll_count, start.elapsed().as_secs());
        }

        // Poll again
        sleep(poll_interval).await;
    }
}

/// Wait for the prompt to appear (indicating Claude finished processing)
/// The prompt should appear without any thinking indicator below it
async fn wait_for_prompt(config: &TmuxConfig, target: &str) -> Result<String, String> {
    let start = Instant::now();
    let timeout = Duration::from_secs(config.timeout_secs);
    let poll_interval = Duration::from_millis(config.capture_interval_ms);

    // Regex patterns for thinking indicators
    // Matches lines like: "✻ Scurrying… (thinking)", "✽ Bloviating…", etc.
    let thinking_pattern =
        Regex::new(r"^[\s]*[✻✽·⏳🔄💭🌀][^❯]*$").unwrap_or_else(|_| Regex::new("^$").unwrap());

    let mut poll_count = 0;
    loop {
        poll_count += 1;
        let content = capture_pane(target)?;
        let lines: Vec<&str> = content.lines().collect();

        // Look for prompt at the end of output
        let has_prompt = lines.iter().rev().take(3).any(|line| {
            line.trim() == config.prompt || line.trim().starts_with(&format!("{} ", config.prompt))
        });

        if has_prompt {
            // Check if there's a thinking indicator below the prompt
            let has_thinking = lines
                .iter()
                .rev()
                .take(5) // Look at last 5 lines
                .any(|line| thinking_pattern.is_match(line) && !line.contains(&config.prompt));

            if !has_thinking {
                info!("TMUX: Prompt detected after {} polls, {} lines captured", poll_count, lines.len());
                debug!("Prompt detected with no thinking indicator, command complete");
                return Ok(content);
            } else {
                debug!("Prompt found but thinking indicator still present, continue polling");
            }
        } else {
            if poll_count % 10 == 0 {
                debug!("TMUX: No prompt yet (poll #{}, {} lines, elapsed: {}s)",
                    poll_count, lines.len(), start.elapsed().as_secs());
            }
        }

        // Check timeout
        if start.elapsed() > timeout {
            error!(
                "Command timed out after {} seconds waiting for prompt ({} polls)",
                config.timeout_secs, poll_count
            );
            // Capture final state for debugging
            let final_capture = capture_pane(target)?;
            return Err(format!(
                "Command timed out after {} seconds (polls: {}, last capture: {} lines)",
                config.timeout_secs, poll_count, final_capture.lines().count()
            ));
        }

        // Poll again
        sleep(poll_interval).await;
    }
}

/// Strip ANSI escape codes from text
pub fn strip_ansi_codes(text: &str) -> String {
    // Pattern: ESC [ followed by digits and semicolons, then m
    // Matches: \x1b[0m, \x1b[38;5;123m, \x1b[1;2;3m, etc.
    let ansi_pattern = Regex::new(r"\x1b\[[0-9;]*m").unwrap_or_else(|_| Regex::new("").unwrap());
    ansi_pattern.replace_all(text, "").to_string()
}

/// Extract meaningful output from the captured pane
/// Removes: command echo (first line), Claude decorations, trailing prompt
fn extract_output(raw_output: &str, command_sent: &str, prompt: &str) -> Result<String, String> {
    info!("TMUX: Extracting output from {} chars, command was: {}", raw_output.len(), command_sent);

    // Strip ANSI codes first
    let clean = strip_ansi_codes(raw_output);
    info!("TMUX: After stripping ANSI: {} chars", clean.len());

    let mut lines: Vec<&str> = clean.lines().collect();

    // Remove first line if it's the echoed command (contains the command we sent)
    if let Some(first) = lines.first() {
        if first.contains(command_sent) {
            lines.remove(0);
            debug!("Removed echoed command line");
        }
    }

    // Remove trailing blank lines
    while lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        lines.pop();
    }

    // Remove trailing prompt lines (lines that are just the prompt or prompt with trailing content)
    while lines.last().map(|l| l.trim() == prompt || l.trim().starts_with(prompt)).unwrap_or(false)
    {
        lines.pop();
    }

    // Remove Claude Code UI decorations (●, ✻, ✽, ·, ⏳, etc.)
    // These appear at the start of response lines from Claude
    let lines: Vec<String> = lines
        .iter()
        .map(|line| {
            // Remove leading bullet/decoration and whitespace
            // Pattern: starts with ● or ✻ or similar, followed by optional space
            let trimmed = line.trim_start();
            if trimmed.starts_with('●') || trimmed.starts_with('✻') || trimmed.starts_with('✽')
                || trimmed.starts_with('·') || trimmed.starts_with('⏳')
            {
                let without_bullet = trimmed.trim_start_matches(|c| "●✻✽·⏳🔄💭🌀".contains(c));
                without_bullet.trim_start().to_string()
            } else {
                line.to_string()
            }
        })
        .collect();

    // Join and clean up
    let result = lines.join("\n").trim().to_string();

    if result.is_empty() {
        error!("TMUX: No output extracted from command response");
        return Err("No output extracted from command response".to_string());
    }

    // Limit to configured number of lines
    let limited = result
        .lines()
        .take(200)
        .collect::<Vec<_>>()
        .join("\n");

    info!("TMUX: Final extracted output: {} chars, {} lines", limited.len(), limited.lines().count());
    Ok(limited)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_ansi_codes() {
        let input = "\x1b[38;5;123mHello\x1b[0m \x1b[1mWorld\x1b[0m";
        let output = strip_ansi_codes(input);
        assert_eq!(output, "Hello World");
    }

    #[test]
    fn test_strip_ansi_various() {
        let input = "\x1b[1;31mRed\x1b[0m \x1b[32mGreen\x1b[0m";
        let output = strip_ansi_codes(input);
        assert_eq!(output, "Red Green");
    }

    #[test]
    fn test_extract_output_removes_echo() {
        let raw = "❯ echo test\n● Bash(echo test)\n  ⎿  test output\n● Done\n❯ ";
        let output = extract_output(raw, "echo test", "❯").unwrap();
        // Should not contain the echoed command
        assert!(!output.contains("❯ echo test"));
        // Should contain the response
        assert!(output.contains("Done"));
    }
}
