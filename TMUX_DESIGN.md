# Sink TMUX Design Specification

**Current Date**: 2026-03-25
**Status**: Design Phase - Updated after manual testing
**Version**: 2.0 (Updated to use Claude Code interactive terminal)

## Overview

Redesign `sink` daemon to use **tmux key injection** to pipe iMessage commands into Claude Code's interactive terminal window (`main:sink MASTER`) instead of spawning subprocess Claudes. The window is a live Claude Code session that processes commands with full tool access, bash execution, etc.

## Core Concept

```
BlueBubbles
    │
    ├─ New iMessage
    │
    ▼
┌─────────────────────┐
│      Sink Daemon    │
│   (tmux proxy)      │
└──────────┬──────────┘
           │
           ├─ Inject command via tmux send-keys
           │
           ▼
┌──────────────────────────────────────┐
│  Claude Code Interactive Terminal    │
│  (main:sink MASTER tmux window)      │
│                                      │
│  ❯ record weight at 191.2lbs         │
│  ● Bash(uv run record_weight.py ...) │
│  ✓ Updated inbody_2026-03-25.json    │
│  ● <Claude's response>               │
│  ❯                                   │
└──────────────────────────────────────┘
           │
           ├─ Wait for prompt (detect via polling)
           │
           ├─ Capture pane output
           │
           ▼
   Extract Claude's response text
   Strip UI decorations (●, ·, ✻)
           │
           ▼
   Send as iMessage reply
```

## Design Decisions

### 1. Command Execution Flow

**Input**: iMessage text (e.g., "record weight at 191.2lbs")

**Process**:
1. Poll BlueBubbles for new messages (unchanged)
2. Incoming text is sent to Claude Code interactive session (not raw shell)
3. Inject text into `main:sink MASTER` window via `tmux send-keys -t "main:sink MASTER" -l "<text>"`
4. Send Enter key: `tmux send-keys -t "main:sink MASTER" Enter`
5. **Wait for Claude Code to process** (with thinking indicator detection)
6. Poll for prompt `❯ ` without thinking indicator below it
7. Capture pane: `tmux capture-pane -t "main:sink MASTER" -p -S -100`
8. Extract response (lines between command and prompt)
9. Strip Claude Code UI decorations
10. Send clean text back as iMessage reply

### 2. Prompt Detection Strategy

**Challenge**: How to know when Claude Code is done processing?

**Solution**: Detect the prompt `❯` when Claude finishes (no thinking indicator below)

**Prompt Format**: `❯ ` (Claude Code's interactive prompt)
- Appears on its own line when Claude is ready for next command
- Preceded by "thinking indicators" while Claude processes:
  - `✽ Scurrying…`, `✻ Bloviating…`, `· Photosynthesizing…`, `⏳ Unravelling…`, etc.
  - These rotate/change while Claude thinks
  - When thinking stops, only the prompt `❯` remains

**Daemon's job** (pure proxy):
- Send command to `main:sink MASTER` via `tmux send-keys -t "main:sink MASTER" -l "..."`
- Send Enter key: `tmux send-keys -t "main:sink MASTER" Enter`
- After `send-keys Enter`, poll `capture-pane` every 200ms
- Look for a line that is exactly `❯ ` (the prompt) with NO thinking indicator visible below
- Pattern: capture ends with `❯` and no active thinking animation present
- Timeout after 90 seconds (Claude can think for a while)
- If timeout → return "Command timed out after 90s"

**Why this approach?**
- Reliable: waiting for the actual prompt is deterministic
- Accounts for variable Claude processing time (simple commands → fast, complex → slow)
- Works with all command types (simple echo, bash execution, complex reasoning)
- Thinking indicators rotate but eventually disappear

**No window validation**:
- Daemon assumes window exists and Claude Code is running
- If window doesn't exist or Claude crashed: tmux send-keys will fail, logged in errors
- User is responsible for keeping Claude Code interactive running in that window

### 3. Output Extraction

**Problem**: `capture-pane` returns full visible pane including:
- The command we just sent (echoed by shell)
- Tool output
- The new prompt at the bottom

**Solution**: Multi-step cleanup (STDOUT + STDERR combined)

```
Raw capture output:
─────────────────────────────
[SINK] $ record weight at 191.2lbs
Weight recorded: 191.2 lbs
New daily average: 190.8 lbs
[SINK] $
─────────────────────────────

After stripping ANSI codes:
─────────────────────────────
[SINK] $ record weight at 191.2lbs
Weight recorded: 191.2 lbs
New daily average: 190.8 lbs
[SINK] $
─────────────────────────────

After removing command echo (first line) + trailing prompt:
─────────────────────────────
Weight recorded: 191.2 lbs
New daily average: 190.8 lbs
─────────────────────────────
```

**Algorithm**:
1. Call `tmux capture-pane -t session:window -p` to get full pane
2. Strip ANSI codes (`\x1b[0-9;]*m` patterns)
3. Split into lines
4. Remove first line if it matches the command we sent (echo)
5. Remove trailing blank lines and prompt (exact match: `[SINK] $ `)
6. Limit output to last 200 lines (safety for large outputs)
7. Return remaining lines as plain text (STDOUT + STDERR combined)

### 4. Configuration Changes

**New config section** (`config.toml`):

```toml
[tmux]
enabled = true
session = "main"           # Session name (default: "main")
window = "sink"            # Window name (default: "sink")
full_target = "main:sink"  # Or auto-derived from session:window
prompt_timeout_secs = 30   # Max wait for prompt after command
capture_lines = 100        # Lines of history to capture
capture_interval_ms = 100  # Poll interval while waiting for prompt
```

**Removed** from config (if present):
- `claude.api_key` / `claude.timeout` (no longer used)
- `notifications.enabled` (no Claude → no followup extraction)
- `transcripts` database config

### 5. Database Changes

**Still track messages in `messages.db`** for audit trail:

```sql
CREATE TABLE messages (
    id INTEGER PRIMARY KEY,
    guid TEXT UNIQUE NOT NULL,
    chat_guid TEXT NOT NULL,
    sender TEXT NOT NULL,
    text TEXT NOT NULL,             -- Original iMessage text
    date_received INTEGER NOT NULL,
    processed_at INTEGER,
    response_guid TEXT,
    response_text TEXT,             -- NEW: Raw tmux output instead of Claude JSON
    session_id TEXT,
    status TEXT DEFAULT 'pending',  -- pending, processing, replied, failed, skipped, sent
    is_from_me INTEGER NOT NULL DEFAULT 0
);
```

**Removed**:
- `transcripts.db` (no Claude sessions)
- `followups.db` (no notification extraction, but keep user prefs if needed)
- `gemini_reason` column (not doing group chat filtering)

### 6. Features to Remove/Simplify

| Feature | Status | Reason |
|---------|--------|--------|
| Claude CLI spawning | ❌ Removed | Core change: using tmux instead |
| Session/resume tracking | ❌ Removed | No Claude sessions |
| Transcript storage | ❌ Removed | No Claude output to store |
| Notification extraction | ❌ Removed | No Gemini-powered followup detection |
| Group chat filtering | ❌ Removed | No Claude reasoning about intent |
| Tool use | ❌ Removed | Raw shell, no Claude tools |
| Attachments/images | ⚠️ Reduced | May still support for shell input, but not Claude vision |
| Control commands | ✅ Kept | (snooze, dismiss, etc.) but simplified |
| Web panel | ⚠️ Retooled | Show tmux session status instead of transcripts |

### 7. Web Admin Panel Redesign

**Simplified dashboard** (no transcript details):

```
┌─ Sink Admin ─────────────────────┐
│                                   │
│ Status: RUNNING                  │
│ Uptime: 2h 34m                   │
│ Messages processed: 45           │
│ Average response time: 1.2s      │
│                                   │
│ ┌─ Tmux Session Status ──────┐  │
│ │ Session: main              │  │
│ │ Window: sink MASTER        │  │
│ │ Status: Ready              │  │
│ │                            │  │
│ │ Recent commands (last 10): │  │
│ │ - record weight...         │  │
│ │ - what time is it          │  │
│ │ - check weather            │  │
│ └────────────────────────────┘  │
│                                   │
│ ┌─ Message Queue ────────────┐   │
│ │ Pending: 0                 │   │
│ │ Replied today: 45          │   │
│ │ Failed: 2                  │   │
│ └────────────────────────────┘   │
│                                   │
└─────────────────────────────────┘
```

**Removed**:
- Transcript viewer (no transcripts)
- Followup manager (no followups)
- Token/cost stats (no Claude billing)
- Full message content viewer (optional: keep simple list)

### 8. Control Commands

**Still supported** (but simplified):

| Command | Action | Notes |
|---------|--------|-------|
| `help` | Show available commands | Updated help text |
| `status` | Show daemon status | Simpler: just tmux check |
| `clear` / `reset` | Clear message queue | Still useful for debugging |
| (Future) `tmux-session <name>` | Change target session/window | Dynamic retargeting |

**Removed**:
- `snooze`, `dismiss`, `enable reminders` (no notifications)

## Tmux Module API (src/tmux.rs)

The core `tmux` module will expose these functions (pure proxy, no validation):

```rust
/// Configuration for tmux execution
pub struct TmuxConfig {
    pub session: String,       // Session name (e.g., "main")
    pub window: String,        // Window name (e.g., "sink MASTER")
    pub prompt: String,        // Prompt string to detect (e.g., "[SINK] $ ")
    pub timeout_secs: u64,     // Max wait time (default: 30)
    pub capture_lines: usize,  // Max lines to capture (default: 200)
    pub poll_interval_ms: u64, // Polling interval (default: 100)
}

/// Send a command to tmux window and wait for output
/// ASSUMES: tmux window already exists and is ready
/// (no validation, pure proxy)
pub async fn execute_command(
    config: &TmuxConfig,
    command_text: &str,
) -> Result<String> {
    // 1. Send command (literal text via -l flag)
    tmux_send_keys(&config.session, &config.window, "-l", command_text)?;

    // 2. Send Enter key
    tmux_send_keys(&config.session, &config.window, "", "Enter")?;

    // 3. Wait for prompt (poll until seen or timeout)
    let start = Instant::now();
    loop {
        let output = tmux_capture_pane(&config.session, &config.window)?;
        if output.contains(&format!("{}\n", config.prompt))
            || output.ends_with(&config.prompt) {
            // Prompt detected, extract meaningful output
            return extract_output(&output, command_text, &config.prompt);
        }
        if start.elapsed().as_secs() > config.timeout_secs {
            return Err(format!("Command timed out after {} seconds", config.timeout_secs));
        }
        tokio::time::sleep(Duration::from_millis(config.poll_interval_ms)).await;
    }
}

/// Strip ANSI escape codes from string
fn strip_ansi_codes(text: &str) -> String {
    // Regex: \x1b\[[0-9;]*m
    // Removes color codes, formatting, etc.
}

/// Extract meaningful output (remove echo + prompt)
fn extract_output(raw_output: &str, command: &str, prompt: &str) -> Result<String> {
    let mut lines: Vec<&str> = raw_output.lines().collect();

    // Remove first line if it's the echoed command
    if lines.first().map(|l| l.contains(command)).unwrap_or(false) {
        lines.remove(0);
    }

    // Remove trailing blank lines and prompt lines
    while lines.last().map(|l| l.is_empty() || l == prompt).unwrap_or(false) {
        lines.pop();
    }

    // Limit to 200 lines
    lines.truncate(200);

    Ok(lines.join("\n"))
}
```

## Implementation Roadmap

### Phase 1: Core Tmux Integration
- [ ] Add `tmux` module with:
  - `send_command(session, window, text)` → injects via send-keys
  - `wait_for_prompt(session, window, timeout)` → polls for shell prompt
  - `capture_output(session, window)` → gets pane content
  - `strip_ansi(text)` → removes escape codes
  - `extract_meaningful_output(raw, command_sent)` → cleanup

- [ ] Replace `claude.rs` invocation with tmux calls in main loop
- [ ] Update message status handling (no more transcripts)
- [ ] Test with manual commands in actual tmux window

### Phase 2: Configuration & Migration
- [ ] Add `[tmux]` section to config schema
- [ ] Migrate config loading (keep fallback for old Claude config)
- [ ] Remove unused database migrations
- [ ] Document setup (user must create `sink MASTER` window, set prompt)

### Phase 3: Web Panel
- [ ] Simplify web panel to show tmux status
- [ ] Remove transcript/followup pages
- [ ] Add simple command log view
- [ ] Test web dashboard

### Phase 4: Testing & Polish
- [ ] Manual testing: send various commands via iMessage
- [ ] Edge cases: multiline output, timeouts, special characters
- [ ] Error handling: prompt not found, tmux session crash
- [ ] Performance: measure response times vs Claude

## Design Decisions Finalized ✅

1. **Shell and prompt configuration**
   - User manually creates tmux window: `tmux new-window -t main -n "sink MASTER"`
   - User manually sets prompt: `export PS1='[SINK] $ '` (exact string, required)
   - User keeps window open while daemon runs
   - ✅ Daemon is a pure proxy: send keys → capture output → return text
   - ✅ No validation, no window creation, no setup help — just execute commands

2. **Prompt detection approach**
   - ✅ **Exact match only**: Look for lines ending with exactly `[SINK] $ `
   - Avoids false positives from command output
   - Fails fast if not configured properly

3. **Interactive programs timeout**
   - ⚠️ **Known limitation**: Programs like `vim`, `python -i`, `htop` won't return prompt
   - Solution: Document that `sink MASTER` must stay as a simple shell
   - Timeout after 30s with clear error message
   - User can manually interrupt (`Ctrl+C` in that window)

4. **Multiline commands**
   - ✅ **Supported**: Send entire text via `-l` flag, shell handles line breaks
   - Example: Paste bash function, script block, etc.

5. **Prompt in command output edge case**
   - ⚠️ **Risk**: User runs `echo "[SINK] $ "` → we might detect it as prompt
   - Current decision: Accept this edge case (rare, acceptable)
   - Alternative: Could add "seen 2+ consecutive prompt matches" logic (future enhancement)

6. **Input sanitization**
   - ✅ **NO sanitization applied** — trust the shell
   - iMessage text sent directly to tmux (via `-l` flag for safety)
   - Shell is the boundary; user is responsible for what they type
   - This is consistent with normal terminal usage

7. **Error output handling**
   - ✅ **Combine STDOUT + STDERR** in response
   - No separate error channels
   - Both appear on screen → both returned in iMessage reply

8. **Long output (>200 lines)**
   - ✅ Limit captured output to 200 lines
   - Prevents huge iMessage replies
   - Reasonable for shell output, can be adjusted in config

9. **Gemini group chat filtering**
   - ✅ **Removed** entirely
   - No Claude reasoning → no smart filtering needed
   - Keeps dependencies minimal (no external API calls)

10. **Thread structure / Reply targeting**
    - ✅ **Unchanged**: Continue sending responses as threaded replies
    - No changes to iMessage protocol or `sender.rs`

## File Structure Changes

**Current**:
```
src/
├── main.rs           # Daemon loop
├── claude.rs         # Claude CLI invocation
├── db.rs
├── poller.rs
├── sender.rs
├── gemini.rs         # Gemini filtering
├── transcripts.rs    # Stores Claude output
├── followups.rs
├── scheduler.rs
├── commands.rs
└── web.rs
```

**New** (after refactor):
```
src/
├── main.rs           # Daemon loop (simplified)
├── tmux.rs           # ✨ NEW: tmux integration module
├── db.rs             # Simplified (no transcripts)
├── poller.rs         # Unchanged
├── sender.rs         # Unchanged
├── commands.rs       # Simplified (remove notification commands)
├── web.rs            # Retooled (simpler dashboard)
└── DELETED:
    ├── claude.rs
    ├── gemini.rs
    ├── transcripts.rs
    ├── followups.rs
    ├── scheduler.rs
```

## Sample Test Cases (Verified)

### Test 1: Simple Echo
```
iMessage: "echo hello world"
↓
Claude Code processes via tmux
↓
❯ echo hello world
● Bash(echo hello world)
  ⎿  hello world
● Echo executed successfully.
❯
↓
iMessage reply: "Echo executed successfully."
```

### Test 2: Date Command
```
iMessage: "what time is it"
↓
Claude Code executes `date` command
↓
❯ what time is it
● Bash(date)
  ⎿  Wed Mar 25 12:01:36 PM PDT 2026
● It's 12:01 PM PDT on Wednesday, March 25, 2026.
❯
↓
iMessage reply: "It's 12:01 PM PDT on Wednesday, March 25, 2026."
```

### Test 3: Timeout (Interactive/Hung Command)
```
iMessage: "vim"  (or any interactive program)
↓
Tmux sends: vim
↓
[Claude sits waiting for response, thinking indicator shows]
[No prompt ❯ appears, hits 60-90 second timeout]
↓
iMessage reply: "Command timed out after 60s."
```

### Test 4: Complex Command with Tool Use
```
iMessage: "list the python files in /tmp"
↓
Claude executes bash command, may call multiple tools
↓
[Claude shows bash output, then synthesized response]
❯
↓
iMessage reply: [Clean extracted response]
```

## Deployment Plan

### Step 1: Setup Claude Code Interactive Terminal (One-time, Before Starting Daemon)

The `main:sink MASTER` window must already be running Claude Code's interactive mode:

```bash
# User has already created this window and started Claude Code interactive:
tmux list-windows -t main  # Should show "sink MASTER" window running Claude

# The window should show:
# ❯ [prompt waiting for input]

# This is Claude Code's interactive terminal, NOT a plain shell
```

**Why this setup?** The daemon is a proxy to an existing Claude Code session. It assumes:
- The window exists and is running Claude Code interactive
- The prompt is `❯ ` (Claude Code's interactive prompt)
- Commands will be processed through Claude with full tool access

### Step 2: Update Configuration

```bash
# Edit /etc/sink/config.toml
# Add [tmux] section:

[tmux]
session = "main"              # Tmux session (where sink MASTER window is)
window = "sink MASTER"        # Exact window name
prompt = "❯"                  # Claude Code interactive prompt (no trailing space check needed)
timeout_secs = 90             # Max wait for response (Claude can think long)
capture_lines = 200           # Max lines to capture
capture_interval_ms = 200     # Poll frequency (slower, Claude takes time)
```

### Step 3: Start Sink Daemon

```bash
# Build and install
cargo build --release
cp target/release/sink ~/.local/bin/sink

# Ensure Claude Code interactive is running in main:sink MASTER window
tmux send-keys -t "main:sink MASTER" -l "# Waiting for iMessage commands..."
tmux send-keys -t "main:sink MASTER" Enter

# Start sink service
systemctl --user restart sink
journalctl --user -u sink -f

# Verify logs show:
# - "Sink daemon starting"
# - "Configured tmux target: main:sink MASTER"
```

### Step 4: Test via iMessage

```bash
# Send test message to Claude's iMessage account:
# "echo hello"

# Watch the daemon logs:
journalctl --user -u sink -f

# In tmux window you should see:
# ❯ echo hello
# ✻ Scurrying… (thinking)  [Claude processes]
# [Claude response]
# ❯

# Check iMessage for reply (should arrive after Claude finishes)
```

**If it doesn't work:**
- Check daemon logs for tmux connection errors
- Verify window name is exactly `sink MASTER` (case-sensitive)
- Verify Claude Code interactive is running in that window (prompt shows `❯`)
- Send a simple test command like `echo test` first
- Check timeout isn't too short for Claude's thinking time

## Performance Expectations

| Aspect | Current (Claude) | New (Tmux) | Change |
|--------|------------------|-----------|--------|
| E2E latency | 2-5s (Claude processing) | 0.5-2s (shell cmd) | **2-5x faster** |
| CPU | Moderate (Claude process) | Minimal (tmux queries) | **Much lower** |
| Memory | ~100MB (Claude) | ~10MB (daemon only) | **10x lower** |
| Network | ✅ Minimal | ✅ Minimal | No change |
| Cost | $$ (Claude API calls) | Free (local shell) | **Much cheaper** |

## Known Limitations & Future Work

1. **No AI reasoning**: Raw shell output only. No Claude summarization or context awareness.
2. **No multimodal**: Images/files still need shell-native handling (not vision).
3. **No tool use**: Can't invoke Claude tools; limited to shell builtins + installed commands.
4. **No proactive actions**: No background reminders/notifications (could add with cron separately).
5. **Simple filtering**: No intelligent group chat detection (could add with Gemini separately).

**These are intentional trade-offs** for simplicity and performance. If needed later, can be added back.

## Rollback Plan

- Keep `claude.rs` in git history (don't delete)
- Keep `config.example.toml` with both `[claude]` and `[tmux]` sections
- If needed, revert to commit before tmux refactor
- **No data loss**: `messages.db` unchanged in structure

---

## Next Steps

1. **Clarify any ambiguities** in this design (ask below)
2. **Review implementation strategy** and file changes
3. **Proceed with Phase 1** (tmux module + core integration)
4. **Test with real iMessages**
5. **Collect feedback** and iterate

**Ready to start implementation?** Or are there design questions first?
