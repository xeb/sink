# Sink - iMessage to Claude Daemon

A Rust daemon that bridges iMessage (via BlueBubbles) with Claude Code. When someone texts Claude's iMessage account, this daemon processes the message through Claude and sends back the response. Supports proactive follow-up notifications and intelligent group chat filtering.

## Quick Reference

```bash
make install    # First-time install (builds, installs, enables service)
make update     # Rebuild and restart after code changes
make uninstall  # Remove service (preserves data)
make status     # Check service status
make logs       # Tail service logs
make restart    # Restart service
make stop       # Stop service
make start      # Start service
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
- **Context aware**: Last 10 messages in conversation passed to Claude as context
- **Session continuity**: Uses `--resume` to maintain Claude's memory per chat
- **Group chat filtering**: Uses Gemini to detect if messages in group chats are directed at Claude
- **Proactive notifications**: Extracts follow-ups from conversations and sends reminder texts
- **Threaded replies**: Notifications are sent as replies to the original message

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
| `/usr/local/bin/sink` | Installed binary |
| `/etc/sink/config.toml` | Configuration (owned by xeb, mode 600) |
| `/var/lib/sink/messages.db` | Messages database |
| `/var/lib/sink/transcripts.db` | Full Claude session transcripts |
| `/var/lib/sink/followups.db` | Pending notifications & user preferences |
| `/etc/systemd/system/sink.service` | Systemd unit |

## Configuration

```toml
# /etc/sink/config.toml
[bluebubbles]
host = "your-bluebubbles-host"
port = 1234
password = "your-password"

[claude]
working_dir = "/path/to/working/directory"
binary = "/path/to/claude"

[polling]
interval_secs = 5

[database]
path = "/var/lib/sink/messages.db"

[context]
message_history_count = 10

[gemini]
api_key = "..."  # Or set GEMINI_API_KEY env var
model = "gemini-2.0-flash"

[notifications]
enabled = false
scheduler_interval_secs = 60

[web_server]
enabled = true
port = 1111
host = "0.0.0.0"
```

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
├── poller.rs      # BlueBubbles API polling + participant count
├── claude.rs      # Claude CLI invocation
├── sender.rs      # Response sending (regular + threaded replies)
├── error.rs       # Error types
├── gemini.rs      # Gemini API (followup extraction + addressed check)
├── transcripts.rs # Transcript storage
├── followups.rs   # Followups database + user preferences
├── scheduler.rs   # Background notification scheduler
├── commands.rs    # User command parsing + handling
└── web.rs         # Web admin panel (axum server)
```

## Web Admin Panel

The daemon includes a web-based admin panel on port 1111 with:

- **Dashboard**: Stats overview (pending/replied/failed counts, costs)
- **Messages**: View and filter all processed messages
- **Transcripts**: Full Claude session transcripts with cost/token data
- **Followups**: Manage scheduled notifications (currently disabled)

Access at `http://localhost:1111` or configure reverse proxy with auth.

## Debugging

```bash
# Check service status
systemctl status sink

# Tail logs
journalctl -u sink -f

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
| Permission denied on config | `sudo chown youruser:youruser /etc/sink/config.toml` |
| Node not found | Ensure PATH in sink.service includes nvm node path |
| Historical messages processing | Fixed; daemon only processes messages after startup |
| Claude responds to group chat side conversations | Check Gemini API key is set; verify with logs |
| Double messages from followups | Fixed; scheduler tells Claude not to send messages directly |
