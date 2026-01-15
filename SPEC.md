# Sink - iMessage Daemon for Claude

## Overview

**Sink** is a Rust daemon that bridges iMessage (via BlueBubbles API) with Claude Code. It listens for incoming text messages to Claude's iMessage account, invokes the `claude` CLI to process them, and sends Claude's response back to the sender.

The daemon is a pure data mover—it does not inject system prompts or modify Claude's behavior. All personality and instruction handling is delegated to Claude via the `CLAUDE.md` in the working directory.

## Architecture

```
┌─────────────────┐     poll/15s      ┌──────────────────────┐
│  BlueBubbles    │◄─────────────────►│       Sink           │
│  (reasonable-   │                   │   (not-invented-     │
│   excuse:1235)  │                   │    here)             │
│                 │    send response  │                      │
│  Claude's       │◄──────────────────│  - Poll for messages │
│  iMessage       │                   │  - Track in SQLite   │
│  Account        │                   │  - Invoke claude CLI │
└─────────────────┘                   │  - Queue if busy     │
                                      └──────────┬───────────┘
                                                 │
                                                 │ spawn process
                                                 ▼
                                      ┌──────────────────────┐
                                      │   claude -p ...      │
                                      │   (in /path/to/    │
                                      │   GreyArea/projects/ │
                                      │   master/)           │
                                      └──────────────────────┘
```

## Components

### 1. Message Poller

- Polls BlueBubbles API every **15 seconds**
- Endpoint: `POST http://your-bluebubbles-host:1235/api/v1/message/query`
- Fetches recent messages sorted DESC
- Compares against SQLite to identify unprocessed incoming messages
- Only processes messages where `is_from_me = false`

### 2. SQLite Database

**Location**: `/var/lib/sink/messages.db`

**Schema**:

```sql
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY,
    guid TEXT UNIQUE NOT NULL,           -- BlueBubbles message GUID
    chat_guid TEXT NOT NULL,             -- Chat identifier (e.g., iMessage;-;+1XXXXXXXXXX)
    sender TEXT NOT NULL,                -- Sender handle (phone/email)
    text TEXT NOT NULL,                  -- Message content
    date_received INTEGER NOT NULL,      -- Unix timestamp (ms)
    processed_at INTEGER,                -- When we processed it (NULL = pending)
    response_guid TEXT,                  -- GUID of our response message
    session_id TEXT,                     -- Claude session ID for --continue
    status TEXT DEFAULT 'pending'        -- pending, processing, completed, failed
);

CREATE INDEX idx_messages_chat_guid ON messages(chat_guid);
CREATE INDEX idx_messages_status ON messages(status);
CREATE INDEX idx_messages_date ON messages(date_received);
```

### 3. Message Queue

When Claude is actively processing a message, new incoming messages are:
1. Stored in SQLite with `status = 'pending'`
2. Processed sequentially after current task completes

The queue is per-conversation (by `chat_guid`), allowing parallel processing of different conversations if desired in the future.

### 4. Claude Invoker

**Working Directory**: `/path/to/working/directory/`

**Invocation**:
```bash
claude -p "<prompt>" --continue
```

Or for first message in a conversation:
```bash
claude -p "<prompt>"
```

**Prompt Construction**:

The prompt sent to Claude includes the last 10 messages for context:

```
[Previous messages in this conversation]

From +1XXXXXXXXXX (2026-01-14 10:30:15):
Hey Claude, what's on my todo list?

From Claude (2026-01-14 10:30:45):
Hey, it's Claude (the intern). Here are your top items...

From +1XXXXXXXXXX (2026-01-14 10:31:00):
Thanks! Can you also check my calendar?

[Current message - please respond to this]

From +1XXXXXXXXXX (2026-01-14 10:35:22):
What about the Japan trip planning?
```

**Session Management**:
- Store `session_id` from Claude's output in SQLite
- Use `--continue` for subsequent messages in the same conversation
- This allows Claude to maintain its own memory across the conversation

**Response Extraction**:
- Claude's final output (stdout) is the response to send
- Use `--output-format text` (default) for clean text output

### 5. Response Sender

After Claude completes:
1. Extract the response text from stdout
2. Send via BlueBubbles API (direct HTTP, no SSH tunnel needed):
   ```
   POST http://your-bluebubbles-host:1235/api/v1/message/text
   {
     "chatGuid": "iMessage;-;+1XXXXXXXXXX",
     "message": "<response>",
     "method": "private-api",
     "tempGuid": "sink-<uuid>"
   }
   ```
3. Update SQLite: `status = 'completed'`, `response_guid = <new_guid>`

## Configuration

**Config file**: `/etc/sink/config.toml`

```toml
[bluebubbles]
host = "your-bluebubbles-host"
port = 1235
password = "CHANGEME_PASSWORD"

[claude]
working_dir = "/path/to/working/directory"
binary = "claude"

[polling]
interval_secs = 15

[database]
path = "/var/lib/sink/messages.db"

[context]
message_history_count = 10
```

## Systemd Service

**Unit file**: `/etc/systemd/system/sink.service`

```ini
[Unit]
Description=Sink - iMessage to Claude daemon
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/sink
Restart=always
RestartSec=10
User=youruser
Group=youruser
WorkingDirectory=/path/to/working/directory

# Logging
StandardOutput=journal
StandardError=journal
SyslogIdentifier=sink

# Security
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=/var/lib/sink

[Install]
WantedBy=multi-user.target
```

## Message Flow

1. **Poll** → Fetch messages from BlueBubbles
2. **Filter** → Identify new incoming messages not in SQLite
3. **Store** → Insert into SQLite with `status = 'pending'`
4. **Check Lock** → If Claude is running, wait
5. **Acquire Lock** → Mark message as `status = 'processing'`
6. **Build Context** → Fetch last 10 messages from same chat
7. **Invoke Claude** → Run `claude -p <prompt>` in master directory
8. **Capture Response** → Read stdout
9. **Send Response** → POST to BlueBubbles API
10. **Update Status** → Mark as `status = 'completed'`
11. **Release Lock** → Process next pending message

## Error Handling

| Error | Handling |
|-------|----------|
| BlueBubbles unreachable | Log, retry next poll cycle |
| Claude process fails | Mark `status = 'failed'`, log error, continue to next |
| Response send fails | Retry up to 3 times, then mark failed |
| Malformed message JSON | Skip message, log warning |

## Logging

- Use `tracing` crate with structured logging
- Log levels: ERROR, WARN, INFO, DEBUG, TRACE
- Journal integration via systemd
- Key events to log:
  - New message detected
  - Claude invocation start/end
  - Response sent
  - Errors and retries

## Dependencies (Cargo.toml)

```toml
[dependencies]
tokio = { version = "1", features = ["full"] }
reqwest = { version = "0.11", features = ["json"] }
rusqlite = { version = "0.31", features = ["bundled"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
tracing-journald = "0.3"
chrono = { version = "0.4", features = ["serde"] }
uuid = { version = "1", features = ["v4"] }
thiserror = "1"
```

## Directory Structure

```
sink/
├── Cargo.toml
├── SPEC.md
├── MESSAGING.md
├── src/
│   ├── main.rs           # Entry point, service setup
│   ├── config.rs         # Configuration loading
│   ├── db.rs             # SQLite operations
│   ├── poller.rs         # BlueBubbles polling logic
│   ├── claude.rs         # Claude CLI invocation
│   ├── sender.rs         # Response sending
│   └── error.rs          # Error types
└── config/
    └── sink.toml.example
```

## Security Considerations

1. **Network Access**: Daemon connects directly to BlueBubbles over LAN (no SSH required)
2. **Password Storage**: BlueBubbles password in config file; ensure file permissions `600`
3. **No Secrets in Logs**: Redact message content in DEBUG logs, never log passwords
4. **Systemd Hardening**: Use `ProtectSystem`, `NoNewPrivileges`, etc.

## Future Enhancements (Out of Scope)

- [ ] Parallel conversation processing
- [ ] Rate limiting per sender
- [ ] Message filtering/blocklist
- [ ] Webhook mode (push instead of poll)
- [ ] Web dashboard for monitoring
- [ ] Multiple Claude account support

## Resolved Design Decisions

1. **Direct HTTP**: BlueBubbles port 1235 is directly accessible on LAN. No SSH tunnel needed.

2. **Message Deduplication**: The `guid` UNIQUE constraint handles duplicates via `INSERT OR IGNORE`.

3. **Long-running Claude Sessions**: The lock mechanism (`status = 'processing'`) prevents duplicate processing across poll cycles.

4. **Group Chat Handling**: Yes - daemon responds to all chats (1:1 and group) where messages arrive at Claude's account.

## Installation

```bash
# Build and install
make install

# Check status
make status

# View logs
make logs

# Update after code changes
make update

# Uninstall
make uninstall
```
