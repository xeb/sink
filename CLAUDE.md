# Sink - iMessage to Claude Daemon

A Rust daemon that bridges iMessage (via BlueBubbles) with Claude Code. When someone texts Claude's iMessage account, this daemon processes the message through Claude and sends back the response.

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
BlueBubbles (your-bluebubbles-host:1235)
         │
         │ HTTP polling every 15s
         ▼
   ┌─────────────┐
   │    Sink     │ (runs on not-invented-here)
   │   Daemon    │
   └──────┬──────┘
          │
          │ spawns: claude -p "..." --output-format json
          │ working dir: /path/to/working/directory
          ▼
   ┌─────────────┐
   │   Claude    │
   │    Code     │
   └─────────────┘
```

## Key Behaviors

- **Only processes new messages**: Messages received before daemon startup are marked "skipped"
- **Sequential processing**: One message at a time; new messages queue in SQLite
- **Context aware**: Last 10 messages in conversation passed to Claude as context
- **Session continuity**: Uses `--resume` to maintain Claude's memory per chat
- **Group chat support**: Responds to both 1:1 and group chats

## Files

| Path | Purpose |
|------|---------|
| `/usr/local/bin/sink` | Installed binary |
| `/etc/sink/config.toml` | Configuration (owned by xeb, mode 600) |
| `/var/lib/sink/messages.db` | SQLite database |
| `/etc/systemd/system/sink.service` | Systemd unit |

## Configuration

```toml
# /etc/sink/config.toml
[bluebubbles]
host = "your-bluebubbles-host"
port = 1235
password = "CHANGEME_PASSWORD"

[claude]
working_dir = "/path/to/working/directory"
binary = "/path/to/claude"

[polling]
interval_secs = 15

[database]
path = "/var/lib/sink/messages.db"

[context]
message_history_count = 10
```

## Database Schema

```sql
CREATE TABLE messages (
    id INTEGER PRIMARY KEY,
    guid TEXT UNIQUE NOT NULL,      -- BlueBubbles message GUID
    chat_guid TEXT NOT NULL,        -- e.g., "iMessage;-;+1XXXXXXXXXX"
    sender TEXT NOT NULL,           -- Phone/email of sender
    text TEXT NOT NULL,
    date_received INTEGER NOT NULL, -- Unix timestamp (ms)
    processed_at INTEGER,
    response_guid TEXT,
    session_id TEXT,                -- Claude session ID for --resume
    status TEXT DEFAULT 'pending',  -- pending, processing, completed, failed, skipped
    is_from_me INTEGER NOT NULL DEFAULT 0
);
```

## Source Structure

```
src/
├── main.rs      # Daemon loop, startup time tracking
├── config.rs    # TOML config loading
├── db.rs        # SQLite operations
├── poller.rs    # BlueBubbles API polling
├── claude.rs    # Claude CLI invocation
├── sender.rs    # Response sending
└── error.rs     # Error types
```

## Debugging

```bash
# Check service status
systemctl status sink

# Tail logs
journalctl -u sink -f

# Query the database
sqlite3 /var/lib/sink/messages.db "SELECT * FROM messages ORDER BY date_received DESC LIMIT 10;"

# Test BlueBubbles connection
curl -s "http://your-bluebubbles-host:1235/api/v1/server/info?password=CHANGEME_PASSWORD"
```

## Common Issues

| Issue | Solution |
|-------|----------|
| Permission denied on config | `sudo chown youruser:youruser /etc/sink/config.toml` |
| Node not found | Ensure PATH in sink.service includes nvm node path |
| Historical messages processing | This is fixed; daemon only processes messages after startup |
