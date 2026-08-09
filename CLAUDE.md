# Sink - iMessage to Claude Daemon

A Rust daemon that bridges iMessage (via BlueBubbles) with Claude Code. When someone texts Claude's iMessage account, this daemon processes the message through Claude and sends back the response. Supports proactive follow-up notifications and intelligent group chat filtering.

## Quick Reference

```bash
# Build and update (no sudo required)
cargo build --release
# Plain `cp` over the running binary fails with "Text file busy" — write beside
# it and rename, which swaps the directory entry without touching the live inode.
cp target/release/sink ~/.local/bin/sink.new && mv -f ~/.local/bin/sink.new ~/.local/bin/sink
systemctl --user restart sink

# Service control (no sudo required)
systemctl --user status sink     # Check status
systemctl --user restart sink    # Restart
systemctl --user stop sink       # Stop
systemctl --user start sink      # Start
journalctl --user -u sink -f     # Tail logs
```

## Architecture

```
BlueBubbles Server
         │
         │ HTTP polling every 5s
         ▼
   ┌─────────────┐
   │    Sink     │ Daemon
   │             │
   └──────┬──────┘
          │
          ├──► Gemini Flash (group chat filtering)
          │
          │ spawns: claude -p "..." --output-format json
          ▼
   ┌─────────────┐
   │   Claude    │
   │    Code     │
   └─────────────┘
          │
          ▼
   ┌─────────────┐
   │ Admin Panel │ (port 1111)
   └─────────────┘
```

## Key Behaviors

- **Only processes new messages**: Messages received before daemon startup are marked "skipped"
- **Sequential processing**: One message at a time; new messages queue in SQLite
- **Message batching**: Waits 30s for additional messages before processing (handles iOS message splitting)
- **Context aware**: Last 20 messages in conversation passed to Claude as context
- **Full history access**: Claude can query the SQLite database for complete conversation history
- **Session continuity**: Uses `--resume` to maintain Claude's memory per chat
- **Group chat filtering**: Uses Gemini to detect if messages in group chats are directed at Claude
- **Image handling**: Downloads attachments to `/tmp/sink/`, converts HEIC to JPG, passes paths to Claude
- **Proactive notifications**: Extracts follow-ups from conversations and sends reminder texts
- **Threaded replies**: Notifications are sent as replies to the original message
- **60-minute timeout**: Claude processes are killed after 60 minutes to prevent hangs

## Group Chat Behavior

In group chats (>2 participants), Sink uses Gemini to determine if a message is directed at Claude:

- **Responds** when directly addressed ("Claude, ...", "Hey Claude", "@Claude")
- **Responds** when a question/request seems intended for Claude based on context
- **Ignores** conversations between other humans in the group
- **Ignores** third-person references ("Claude told me...", "What Claude said...")
- **When in doubt**: Stays silent (errs on the side of not responding)

Messages not directed at Claude are marked as `not_for_claude` with a reason stored in the database.

## Notifications System

Gemini analyzes conversations to extract follow-up actions:

- **Explicit reminders**: "remind me in 2 hours to check the build"
- **AI-detected follow-ups**: Implicit needs detected from conversation context
- **Recurring reminders**: "remind me every day at 9am"

When a follow-up is due, the scheduler:
1. Invokes Claude with the follow-up context
2. Claude generates a personalized reminder message
3. Message is sent as a threaded reply to the original conversation

### User Commands

Users can control notifications via text commands:

| Command | Description |
|---------|-------------|
| `snooze 1h` / `snooze 30m` | Snooze pending reminders |
| `dismiss` | Dismiss all pending reminders |
| `disable reminders` | Turn off all notifications |
| `enable reminders` | Turn notifications back on |
| `quiet hours 10pm-8am` | Set do-not-disturb window |
| `list reminders` | Show pending follow-ups |

## Files

| Path | Purpose |
|------|---------|
| `~/.local/bin/sink` | Installed binary |
| `~/.config/systemd/user/sink.service` | User systemd unit |
| `/etc/sink/config.toml` | Configuration (mode 600) |
| `/var/lib/sink/messages.db` | Messages database |
| `/var/lib/sink/transcripts.db` | Full Claude session transcripts |
| `/var/lib/sink/followups.db` | Pending notifications & user preferences |

## Configuration

See `config.example.toml` for all options. Copy to `/etc/sink/config.toml` and fill in your values.

## Database Schemas

### messages.db
```sql
CREATE TABLE messages (
    id INTEGER PRIMARY KEY,
    guid TEXT UNIQUE NOT NULL,
    chat_guid TEXT NOT NULL,
    sender TEXT NOT NULL,
    text TEXT NOT NULL,
    date_received INTEGER NOT NULL,
    processed_at INTEGER,
    response_guid TEXT,
    session_id TEXT,
    status TEXT DEFAULT 'pending',  -- pending, processing, replied, failed, skipped, sent, not_for_claude
    is_from_me INTEGER NOT NULL DEFAULT 0,
    gemini_reason TEXT              -- Why message was marked not_for_claude
);
```

### transcripts.db
```sql
CREATE TABLE transcripts (
    id INTEGER PRIMARY KEY,
    message_guid TEXT NOT NULL,
    chat_guid TEXT NOT NULL,
    session_id TEXT,
    prompt_sent TEXT NOT NULL,
    raw_stdout TEXT,
    raw_stderr TEXT,
    exit_code INTEGER,
    messages_json TEXT,
    tool_calls_json TEXT,
    cost_usd REAL,
    input_tokens INTEGER,
    output_tokens INTEGER,
    claude_model TEXT,
    working_dir TEXT,
    duration_ms INTEGER,
    created_at INTEGER NOT NULL
);
```

### followups.db
```sql
CREATE TABLE followups (
    id INTEGER PRIMARY KEY,
    transcript_id INTEGER NOT NULL,
    chat_guid TEXT NOT NULL,
    sender TEXT NOT NULL,
    original_message_guid TEXT NOT NULL,
    description TEXT NOT NULL,
    context_summary TEXT,
    trigger_type TEXT NOT NULL,      -- explicit, ai_detected
    notify_at INTEGER,
    notify_interval_mins INTEGER,    -- For recurring
    notify_until INTEGER,
    recurrence_count INTEGER DEFAULT 0,
    status TEXT DEFAULT 'pending',   -- pending, sent, dismissed
    created_at INTEGER NOT NULL,
    sent_at INTEGER,
    dismissed_at INTEGER,
    notification_guid TEXT,
    user_response TEXT
);

CREATE TABLE user_preferences (
    id INTEGER PRIMARY KEY,
    chat_guid TEXT NOT NULL,
    sender TEXT NOT NULL,
    notifications_enabled INTEGER DEFAULT 1,
    quiet_hours_start INTEGER,       -- HHMM format (e.g., 2200)
    quiet_hours_end INTEGER,
    timezone TEXT DEFAULT 'America/Phoenix',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(chat_guid, sender)
);
```

## Source Structure

```
src/
├── main.rs        # Daemon loop, message processing, integration
├── config.rs      # TOML config loading
├── db.rs          # Messages database operations
├── poller.rs      # BlueBubbles API polling + participant count + attachments
├── claude.rs      # Claude CLI invocation
├── sender.rs      # Response sending (regular + threaded replies)
├── error.rs       # Error types
├── gemini.rs      # Gemini API (followup extraction + addressed check)
├── transcripts.rs # Transcript storage
├── followups.rs   # Followups database + user preferences
├── scheduler.rs   # Background notification scheduler
├── commands.rs    # User command parsing + handling
├── attachments.rs # Image/attachment downloading + HEIC conversion
└── web.rs         # Web admin panel (axum server)
```

## Web Admin Panel

A single page on port 1111: a flat, reverse-chronological list of every inbound message, its
outcome, and the reply that went back. Rewritten 2026-07-24 — the old Dashboard / Messages /
Transcripts / Followups tabs are gone, along with their endpoints.

- **The list** — one row per inbound message: time, sender handle, text, outcome. A coloured edge
  runs down the left of the log so the health of the whole thing reads in one vertical scan.
- **Click a row** — it expands in place to show Claude's reply and the round-trip time. Failed
  messages show `error_reason`; ignored ones show `gemini_reason`; anything that failed before
  reason-tracking existed says so and points at `journalctl`.
- **Filter** — `all / replied / failed / waiting / ignored` pills (counts are whole-log totals,
  not page totals), plus substring search over message text.

Only two routes exist: `GET /` and `GET /api/messages?limit&offset&status&q`. The reply text is
resolved by a `LEFT JOIN` from `response_guid` back to the outbound row in the *same* table, so the
panel never touches `transcripts.db` — sessions and cost data are deliberately not shown.

Access locally at `http://localhost:1111`, or remotely at **https://sink.xeb.ai** (Cloudflare
tunnel → Access gate, `xebxeb@gmail.com` only; set up 2026-07-24).

The panel has **no authentication of its own** and serves the full message history and every reply
over `/api/messages`. What that means in practice:

- **From the internet** — protected by the Cloudflare Access gate on `sink.xeb.ai` (domain-level,
  so `/api/*` is covered too; `xebxeb@gmail.com` only). That gate is the only auth; never add a
  tunnel hostname for port 1111 without one. App IDs in `~/p/master/docs/cloudflare.md`.
- **From the LAN** — deliberately wide open. It binds `0.0.0.0:1111` (`[web_server]` in
  `/etc/sink/config.toml`), so anyone on the local network reads the full panel with no login.
  This is intentional per Mark (2026-07-24); don't "harden" it to `127.0.0.1`.

## Debugging

```bash
# Check service status
systemctl --user status sink

# Tail logs
journalctl --user -u sink -f

# Query messages
sqlite3 /var/lib/sink/messages.db "SELECT * FROM messages ORDER BY date_received DESC LIMIT 10;"

# Check filtered group chat messages
sqlite3 /var/lib/sink/messages.db "SELECT sender, text, gemini_reason FROM messages WHERE status = 'not_for_claude' ORDER BY date_received DESC LIMIT 10;"

# Check pending followups
sqlite3 /var/lib/sink/followups.db "SELECT * FROM followups WHERE status = 'pending';"

# Check transcripts
sqlite3 /var/lib/sink/transcripts.db "SELECT id, chat_guid, cost_usd, duration_ms FROM transcripts ORDER BY created_at DESC LIMIT 5;"

# Test BlueBubbles connection
curl -s "http://YOUR_BLUEBUBBLES_HOST:PORT/api/v1/server/info?password=YOUR_PASSWORD"
```

## Common Issues

| Issue | Solution |
|-------|----------|
| Permission denied on config | `sudo chown $USER:$USER /etc/sink/config.toml` |
| Node not found | Ensure PATH in service file includes path to node/claude binary |
| Historical messages processing | Fixed; daemon only processes messages after startup |
| Claude responds to group chat side conversations | Check Gemini API key is set; verify with logs |
| Double messages from followups | Fixed; scheduler tells Claude not to send messages directly |
| Message stuck in "processing" | Check `journalctl --user -u sink -f` for errors; reset with `sqlite3 /var/lib/sink/messages.db "UPDATE messages SET status='pending' WHERE status='processing'"` |
| Commands sent to tmux but no reply ever arrives | The target pane was left in a tmux mode (copy-mode / tree-mode), which swallows `send-keys`. Sink now clears it automatically before each send; check with `tmux display-message -p -t "0:sink MASTER" '#{pane_mode}'` and clear manually with `tmux copy-mode -q -t "0:sink MASTER"` (`send-keys -X cancel` fails when a detached client orphaned the mode). |
