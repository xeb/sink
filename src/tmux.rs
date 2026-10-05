use regex::Regex;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

const MASTER_SESSION: &str = "MASTER";

/// Consecutive idle polls (no live spinner) required before accepting a reply
/// whose `[REPLY-id]` opener is present but whose `[/REPLY-id]` closer Claude
/// dropped. At the default 200ms poll interval this is ~0.6s of confirmation,
/// enough to rule out a closer that is merely one frame behind the spinner.
const IDLE_CONFIRM_POLLS: u32 = 3;

#[derive(Debug, Clone)]
pub struct TmuxConfig {
    pub window: String,                  // Window name (e.g., "sink MASTER")
    pub restart_command: Option<String>, // Command used at startup and to recreate a missing window
    pub startup_command: Option<String>, // Typed after the primary agent is ready
    pub fallback_command: Option<String>, // Replaces the window when primary quota is exhausted
    pub prompt: String,                  // Prompt string (e.g., "❯")
    pub timeout_secs: u64,               // Primary wait before notifying the user (default: 90)
    pub extended_timeout_secs: u64, // Extra wait after the notice, for a slow reply (default: 600)
    pub capture_lines: usize,       // Max lines to capture (default: 200)
    pub capture_interval_ms: u64,   // Poll interval (default: 200)
    pub(crate) fallback_active: Arc<AtomicBool>,
}

impl Default for TmuxConfig {
    fn default() -> Self {
        TmuxConfig {
            window: "sink MASTER".to_string(),
            restart_command: None,
            startup_command: None,
            fallback_command: None,
            prompt: "❯".to_string(),
            timeout_secs: 90,
            extended_timeout_secs: 600,
            capture_lines: 200,
            capture_interval_ms: 200,
            fallback_active: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl TmuxConfig {
    /// Codex streams its final answer after removing the TUI's live-spinner text,
    /// so the Claude-specific idle fallback can truncate a reply mid-render.
    /// Sink owns the pane startup command, which gives us a reliable distinction
    /// for the supported `codex ...` and `claude ...` configurations.
    fn requires_closing_reply_tag(&self) -> bool {
        self.active_command()
            .as_deref()
            .and_then(configured_program)
            .is_some_and(|program| program == "codex")
    }

    fn active_command(&self) -> Option<&str> {
        if self.fallback_active.load(Ordering::SeqCst) {
            self.fallback_command.as_deref()
        } else {
            self.restart_command.as_deref()
        }
    }
}

/// Return the executable name from the configured interactive-shell command.
/// Allow leading `exec`, `env`, and environment assignments, which are common in
/// service configuration, while keeping the detection conservative.
fn configured_program(command: &str) -> Option<&str> {
    command
        .split_ascii_whitespace()
        .map(|token| token.trim_matches(['\'', '"']))
        .filter(|token| *token != "exec" && *token != "env" && !token.contains('='))
        .next()
        .and_then(|token| Path::new(token).file_name())
        .and_then(|name| name.to_str())
}

/// Quote one argument for a POSIX shell. tmux executes its `shell-command`
/// through a shell, so the command passed to Bash must survive that outer
/// parsing layer unchanged.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn interactive_bash_command(command: &str) -> String {
    format!("exec bash -ic {}", shell_quote(command))
}

fn master_target(window_name: &str) -> String {
    format!("{MASTER_SESSION}:{window_name}")
}

pub fn master_window_exists(window_name: &str) -> Result<bool, String> {
    let output = Command::new("tmux")
        .args(["list-windows", "-t", "=MASTER", "-F", "#{window_name}"])
        .output()
        .map_err(|e| format!("Failed to list tmux windows: {e}"))?;

    if !output.status.success() {
        return Ok(false);
    }

    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|window| window == window_name))
}

/// The polling fast path only checks for the window; leave existing agents and
/// their scrollback alone, including an active fallback agent.
pub async fn ensure_agent_window(config: &TmuxConfig, working_dir: &Path) -> Result<(), String> {
    if config.restart_command.is_none() || master_window_exists(&config.window)? {
        return Ok(());
    }

    warn!("TMUX: Window {} is missing; recreating it", master_target(&config.window));
    restart_agent(config, working_dir)?;
    run_startup_command(config).await
}

fn create_master_window(
    window_name: &str,
    working_dir: &str,
    shell_command: &str,
) -> Result<String, String> {
    let session = Command::new("tmux")
        .args(["has-session", "-t", "=MASTER"])
        .output()
        .map_err(|e| format!("Failed to check tmux session {MASTER_SESSION}: {e}"))?;

    let mut tmux = Command::new("tmux");
    if session.status.success() {
        tmux.args([
            "new-window",
            "-d",
            "-t",
            "MASTER:",
            "-n",
            window_name,
            "-c",
            working_dir,
        ]);
    } else {
        tmux.args([
            "new-session",
            "-d",
            "-s",
            MASTER_SESSION,
            "-n",
            window_name,
            "-c",
            working_dir,
        ]);
    }

    let output = tmux
        .arg(shell_command)
        .output()
        .map_err(|e| format!("Failed to create tmux window {window_name}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "tmux window creation failed for {}: {}",
            master_target(window_name),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(master_target(window_name))
}

/// Restart the target pane with the configured interactive agent command.
///
/// Bash is deliberately interactive here: the local `codex --yolo` wrapper is
/// defined in ~/.bash_aliases, which ~/.bashrc sources only for interactive
/// shells. `remain-on-exit` keeps the named pane available for a later daemon
/// restart even if the agent exits immediately.
pub fn restart_agent(config: &TmuxConfig, working_dir: &Path) -> Result<(), String> {
    config.fallback_active.store(false, Ordering::SeqCst);
    let command = config
        .restart_command
        .as_deref()
        .ok_or_else(|| "No tmux restart command configured".to_string())?;
    if command.trim().is_empty() {
        return Err("Tmux restart command cannot be empty".to_string());
    }

    let working_dir = working_dir.to_str().ok_or_else(|| {
        format!(
            "Tmux working directory is not valid UTF-8: {:?}",
            working_dir
        )
    })?;
    let shell_command = interactive_bash_command(command);
    let window_exists = master_window_exists(&config.window)?;
    let target = if window_exists {
        master_target(&config.window)
    } else {
        create_master_window(&config.window, working_dir, &shell_command)?
    };
    info!(
        "TMUX: Starting {} in {:?} with interactive command: {}",
        target, working_dir, command
    );

    let remain = Command::new("tmux")
        .args(["set-option", "-p", "-t", &target, "remain-on-exit", "on"])
        .output()
        .map_err(|e| format!("Failed to configure tmux pane {}: {}", target, e))?;
    if !remain.status.success() {
        return Err(format!(
            "tmux set-option failed for {}: {}",
            target,
            String::from_utf8_lossy(&remain.stderr).trim()
        ));
    }

    if !window_exists {
        info!("TMUX: Created {} successfully", target);
        return Ok(());
    }

    let output = Command::new("tmux")
        .args([
            "respawn-pane",
            "-k",
            "-t",
            &target,
            "-c",
            working_dir,
            &shell_command,
        ])
        .output()
        .map_err(|e| format!("Failed to respawn tmux pane {}: {}", target, e))?;

    if !output.status.success() {
        return Err(format!(
            "tmux respawn-pane failed for {}: {}",
            target,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    info!("TMUX: Respawned {} successfully", target);
    Ok(())
}

/// Type the configured initialization command once the freshly started primary
/// agent has drawn its UI. The fallback deliberately skips this step.
pub async fn run_startup_command(config: &TmuxConfig) -> Result<(), String> {
    let Some(command) = config.startup_command.as_deref() else {
        return Ok(());
    };
    if command.trim().is_empty() {
        return Err("Tmux startup command cannot be empty".to_string());
    }

    let target = find_target(&config.window)?;
    wait_for_agent_ui(&target, 15).await?;
    info!("TMUX: Sending primary-agent startup command: {}", command);
    exit_pane_mode(&target);
    send_keys_literal(&target, command)?;
    send_keys_key(&target, "Enter")?;
    sleep(Duration::from_millis(500)).await;
    Ok(())
}

/// Find the named window in the dedicated MASTER session.
fn find_target(window_name: &str) -> Result<String, String> {
    if master_window_exists(window_name)? {
        return Ok(master_target(window_name));
    }

    Err(format!(
        "No tmux window named '{}' found in session {}",
        window_name, MASTER_SESSION
    ))
}

/// Send a command to the tmux window and wait for the interactive agent.
pub async fn execute_command(config: &TmuxConfig, command_text: &str) -> Result<String, String> {
    execute_or_fallback(config, command_text, None).await
}

async fn execute_or_fallback(
    config: &TmuxConfig,
    command_text: &str,
    cmd_id: Option<&str>,
) -> Result<String, String> {
    // A call from the extended-wait path has already notified the user, so give
    // a newly launched fallback the full extended interval for its retry.
    let mut retry_after_fallback =
        cmd_id.is_some() && config.fallback_active.load(Ordering::SeqCst);
    loop {
    let target = find_target(&config.window)?;
    info!("TMUX: Starting command execution in {}", target);

    // Step -2: Leave any tmux pane mode (copy-mode / tree-mode / …) first.
    // While a pane is in a mode, tmux routes send-keys to the mode's key table
    // instead of the program underneath, so every keystroke we send is silently
    // swallowed and the reply never arrives. A detached client can strand a pane
    // in tree-mode indefinitely (seen 2026-08-09), which wedged the daemon.
    exit_pane_mode(&target);

    // Step -1: Send 3 Enters with 100ms pause to prepare prompt
    info!("TMUX: Sending 3 Enter keys to prepare prompt");
    for _ in 0..3 {
        send_keys_key(&target, "Enter")?;
        sleep(Duration::from_millis(100)).await;
    }

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

    // Step 2: Send Enter key 3 times to ensure submission
    info!("TMUX: Sending Enter key 3 times to submit");
    for _ in 0..3 {
        send_keys_key(&target, "Enter")?;
        sleep(Duration::from_millis(100)).await;
    }
    info!("TMUX: Enter keys sent, waiting for prompt");

    // Step 3: Wait for [REPLY-ID] tags (the agent wraps its response with the matching ID)
    // Extract the ID from the command text (format: [CMD-XXXX]...[/CMD-XXXX])
    let parsed_cmd_id = if let Some(start) = command_text.find("[CMD-") {
        if let Some(end) = command_text[start + 5..].find("]") {
            command_text[start + 5..start + 5 + end].to_string()
        } else {
            return Err("Invalid command format".to_string());
        }
    } else {
        return Err("Command missing [CMD-ID] format".to_string());
    };

    let cmd_id = cmd_id.unwrap_or(&parsed_cmd_id);
    let timeout_secs = if retry_after_fallback {
        config.extended_timeout_secs
    } else {
        config.timeout_secs
    };
    info!("TMUX: Waiting for [REPLY-{}] tags (timeout: {}s, poll interval: {}ms)",
        cmd_id, timeout_secs, config.capture_interval_ms);
    match wait_for_reply_tags(
        &target,
        &cmd_id,
        timeout_secs,
        config.capture_interval_ms,
        config.requires_closing_reply_tag(),
    )
    .await {
        Ok(result) => {
            info!("TMUX: [REPLY-{}] tags detected, returning output: {} chars", cmd_id, result.len());
            return Ok(result);
        }
        Err(WaitError::UsageLimit) if !config.fallback_active.load(Ordering::SeqCst) => {
            replace_with_fallback(config)?;
            retry_after_fallback = true;
            wait_for_agent_ui(&find_target(&config.window)?, 15).await?;
            info!("TMUX: Retrying [CMD-{}] in fallback agent", cmd_id);
            continue;
        }
        Err(WaitError::UsageLimit) => {
            return Err("Fallback agent also reported a usage limit".to_string());
        }
        Err(WaitError::Other(error)) => return Err(error),
    }

    }
}

/// Keep listening for [REPLY-ID] tags after the command was already sent and the
/// primary wait timed out. Does NOT resend the command — it just continues polling
/// the pane so a slow reply can still be delivered after the user has been notified.
pub async fn wait_for_reply(
    config: &TmuxConfig,
    cmd_id: &str,
    command_text: &str,
    timeout_secs: u64,
) -> Result<String, String> {
    let target = find_target(&config.window)?;
    info!(
        "TMUX: Extended wait for [REPLY-{}] (up to {}s more)",
        cmd_id, timeout_secs
    );
    match wait_for_reply_tags(
        &target,
        cmd_id,
        timeout_secs,
        config.capture_interval_ms,
        config.requires_closing_reply_tag(),
    )
    .await {
        Ok(output) => Ok(output),
        Err(WaitError::UsageLimit) if !config.fallback_active.load(Ordering::SeqCst) => {
            replace_with_fallback(config)?;
            wait_for_agent_ui(&find_target(&config.window)?, 15).await?;
            execute_or_fallback(config, command_text, Some(cmd_id)).await
        }
        Err(WaitError::UsageLimit) => Err("Fallback agent also reported a usage limit".to_string()),
        Err(WaitError::Other(error)) => Err(error),
    }
}

#[derive(Debug)]
enum WaitError {
    UsageLimit,
    Other(String),
}

fn usage_limit_hit(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    (lower.contains("individual quota reached")
        && (lower.contains("please upgrade your subscription") || lower.contains("resets in")))
        || lower.contains("you've hit your usage limit")
        || lower.contains("you have hit your usage limit")
        || lower.contains("usage quota exceeded")
}

fn pane_working_dir(target: &str) -> Result<String, String> {
    let output = Command::new("tmux")
        .args(["display-message", "-p", "-t", target, "#{pane_current_path}"])
        .output()
        .map_err(|e| format!("Failed to read tmux pane working directory: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "tmux display-message failed for {}: {}",
            target,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn replace_with_fallback(config: &TmuxConfig) -> Result<(), String> {
    let command = config
        .fallback_command
        .as_deref()
        .ok_or_else(|| "Primary agent hit its usage limit and no fallback command is configured".to_string())?;
    let target = find_target(&config.window)?;
    let working_dir = pane_working_dir(&target)?;
    warn!("TMUX: Primary agent usage limit detected; replacing {} with fallback: {}", target, command);

    let killed = Command::new("tmux")
        .args(["kill-window", "-t", &target])
        .output()
        .map_err(|e| format!("Failed to close exhausted tmux window {}: {e}", target))?;
    if !killed.status.success() {
        return Err(format!(
            "tmux kill-window failed for {}: {}",
            target,
            String::from_utf8_lossy(&killed.stderr).trim()
        ));
    }

    create_master_window(
        &config.window,
        &working_dir,
        &interactive_bash_command(command),
    )?;
    config.fallback_active.store(true, Ordering::SeqCst);
    info!("TMUX: Created fallback window {}", master_target(&config.window));
    Ok(())
}

async fn wait_for_agent_ui(target: &str, timeout_secs: u64) -> Result<(), String> {
    let start = Instant::now();
    let mut trust_confirmed = false;
    while start.elapsed() < Duration::from_secs(timeout_secs) {
        let content = capture_visible_pane(target)?;
        if !trust_confirmed && content.contains("Do you trust the contents of this project?") {
            info!("TMUX: Confirming configured working directory for unattended agent");
            send_keys_key(target, "Enter")?;
            trust_confirmed = true;
            sleep(Duration::from_millis(200)).await;
            continue;
        }
        if content.contains("for shortcuts") || content.contains("OpenAI Codex") {
            return Ok(());
        }
        sleep(Duration::from_millis(200)).await;
    }
    Err(format!("Timed out waiting for agent UI in {}", target))
}

fn capture_visible_pane(target: &str) -> Result<String, String> {
    let output = Command::new("tmux")
        .args(["capture-pane", "-t", target, "-p", "-J"])
        .output()
        .map_err(|e| format!("Failed to capture visible tmux pane: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "tmux capture-pane failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Drop the target pane out of any tmux mode so send-keys reaches the program.
///
/// `copy-mode -q` cancels whichever mode is active (it is a no-op on a pane that
/// isn't in one), and unlike `send-keys -X cancel` it still works when the mode
/// was orphaned by a detached client. Best-effort: a failure here shouldn't stop
/// us from trying to send the command.
fn exit_pane_mode(target: &str) {
    let in_mode = Command::new("tmux")
        .args(&["display-message", "-p", "-t", target, "#{pane_in_mode}"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "1")
        .unwrap_or(false);

    if !in_mode {
        return;
    }

    warn!("TMUX: Pane is in a tmux mode (keys would be swallowed) — cancelling it");
    match Command::new("tmux")
        .args(&["copy-mode", "-q", "-t", target])
        .output()
    {
        Ok(o) if o.status.success() => info!("TMUX: Pane mode cancelled"),
        Ok(o) => warn!(
            "TMUX: Failed to cancel pane mode: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => warn!("TMUX: Failed to run tmux copy-mode -q: {}", e),
    }
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

/// Heuristic for "the agent has finished rendering this turn". The TUI shows
/// "(esc to interrupt)" next to its live spinner while a turn is running; its
/// absence means the turn is done. Glyph-independent, so it survives spinner
/// wording changes (Crunched/Churned/Brewed/…).
fn pane_is_idle(content: &str) -> bool {
    !content.contains("esc to interrupt")
}

/// Capture pane content from tmux window.
///
/// `-J` joins wrapped lines (so a `[/REPLY-id]` closer appended to a long line
/// can't be split across rows and miss a `contains` check) and `-S -1000` widens
/// the scrollback window so both tags of a long reply fit in one capture.
fn capture_pane(target: &str) -> Result<String, String> {
    let output = Command::new("tmux")
        .args(&["capture-pane", "-t", target, "-p", "-J", "-S", "-1000"])
        .output()
        .map_err(|e| format!("Failed to execute tmux capture-pane: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("tmux capture-pane failed: {}", stderr));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Read-only capture of all retained history for the five-minute recovery scan.
pub fn capture_reply_history(config: &TmuxConfig) -> Result<String, String> {
    let target = find_target(&config.window)?;
    let output = Command::new("tmux")
        .args(["capture-pane", "-t", &target, "-p", "-J", "-S", "-"])
        .output()
        .map_err(|e| format!("Failed to capture reply history: {e}"))?;
    if !output.status.success() {
        return Err(format!("Reply history capture failed: {}", String::from_utf8_lossy(&output.stderr)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Wait for [REPLY-ID] tags in the output.
async fn wait_for_reply_tags(
    target: &str,
    cmd_id: &str,
    timeout_secs: u64,
    poll_interval_ms: u64,
    require_closing_tag: bool,
) -> Result<String, WaitError> {
    let start = Instant::now();
    let timeout = Duration::from_secs(timeout_secs);
    let poll_interval = Duration::from_millis(poll_interval_ms);

    let reply_start_tag = format!("[REPLY-{}]", cmd_id);
    let reply_end_tag = format!("[/REPLY-{}]", cmd_id);

    let mut poll_count = 0;
    let mut idle_with_opener = 0u32;
    if require_closing_tag {
        info!(
            "TMUX: Codex reply policy active for [REPLY-{}]; matching closing tag is required",
            cmd_id
        );
    }
    loop {
        poll_count += 1;
        let content = capture_pane(target).map_err(WaitError::Other)?;

        if usage_limit_hit(&capture_visible_pane(target).map_err(WaitError::Other)?) {
            return Err(WaitError::UsageLimit);
        }

        let has_open = content.contains(&reply_start_tag);
        let has_close = content.contains(&reply_end_tag);

        // Clean path: both tags present.
        if has_open && has_close {
            info!("TMUX: Found [REPLY-{}] tags after {} polls", cmd_id, poll_count);
            return Ok(content);
        }

        // Claude recovery path: opener present, closer missing. If Claude has gone idle
        // (no live "esc to interrupt" spinner) for several consecutive polls, the
        // model almost certainly dropped the closing tag — accept the response
        // now rather than waiting out the full timeout on a complete, on-screen
        // answer. extract_reply() recovers the body up to the first TUI boundary.
        if has_open && !require_closing_tag {
            if pane_is_idle(&content) {
                idle_with_opener += 1;
                if idle_with_opener >= IDLE_CONFIRM_POLLS {
                    warn!(
                        "TMUX: [REPLY-{}] opener present but closer missing and Claude idle for {} polls — accepting reply without closing tag",
                        cmd_id, idle_with_opener
                    );
                    return Ok(content);
                }
            } else {
                idle_with_opener = 0; // still streaming; reset confirmation
            }
        }

        // Check timeout
        if start.elapsed() > timeout {
            error!(
                "Timed out waiting for [REPLY-{}] tags after {} seconds ({} polls)",
                cmd_id, timeout_secs, poll_count
            );
            return Err(WaitError::Other(format!(
                "Timed out waiting for [REPLY-{}] tags after {} seconds",
                cmd_id, timeout_secs
            )));
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
    fn interactive_command_loads_bash_aliases() {
        assert_eq!(
            interactive_bash_command("codex --yolo"),
            "exec bash -ic 'codex --yolo'"
        );
    }

    #[test]
    fn interactive_command_preserves_single_quotes() {
        assert_eq!(
            interactive_bash_command("agent --name 'sink master'"),
            "exec bash -ic 'agent --name '\"'\"'sink master'\"'\"''"
        );
    }

    #[test]
    fn codex_requires_a_closing_reply_tag() {
        let config = TmuxConfig {
            restart_command: Some("codex --yolo".to_string()),
            ..TmuxConfig::default()
        };

        assert!(config.requires_closing_reply_tag());
    }

    #[test]
    fn codex_path_and_shell_prefix_require_a_closing_reply_tag() {
        let config = TmuxConfig {
            restart_command: Some(
                "exec env CODEX_MODE=sink /usr/local/bin/codex --yolo".to_string(),
            ),
            ..TmuxConfig::default()
        };

        assert!(config.requires_closing_reply_tag());
    }

    #[test]
    fn claude_keeps_the_missing_closer_fallback() {
        for restart_command in [
            None,
            Some("claude --dangerously-skip-permissions".to_string()),
        ] {
            let config = TmuxConfig {
                restart_command,
                ..TmuxConfig::default()
            };

            assert!(!config.requires_closing_reply_tag());
        }
    }

    #[test]
    fn recognizes_antigravity_quota_banner_without_matching_limit_conversation() {
        assert!(usage_limit_hit(
            "⚠ Individual quota reached. Please upgrade your subscription to increase your limits. Resets in 146h."
        ));
        assert!(!usage_limit_hit(
            "[CMD-abcd]Can you explain individual quota limits?[/CMD-abcd]"
        ));
        assert!(!usage_limit_hit(
            "A checked bag may avoid the carry-on liquid limit."
        ));
    }

    #[test]
    fn fallback_switches_reply_policy_without_changing_startup_command() {
        let config = TmuxConfig {
            restart_command: Some("agy --yolo".to_string()),
            startup_command: Some("/effort low".to_string()),
            fallback_command: Some("codex --yolo".to_string()),
            ..TmuxConfig::default()
        };

        assert!(!config.requires_closing_reply_tag());
        assert_eq!(config.startup_command.as_deref(), Some("/effort low"));
        config.fallback_active.store(true, Ordering::SeqCst);
        assert!(config.requires_closing_reply_tag());
        assert_eq!(config.startup_command.as_deref(), Some("/effort low"));
    }

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
