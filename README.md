# Sink

A Rust daemon that bridges iMessage (via BlueBubbles) with Claude Code. When someone texts Claude's iMessage account, this daemon processes the message through Claude and sends back the response.

## Features

- **iMessage Bridge**: Receives messages via BlueBubbles API, sends responses back
- **Claude Code Integration**: Spawns Claude CLI for each conversation with session continuity
- **Group Chat Filtering**: Uses Gemini to detect if messages are directed at Claude
- **Web Admin Panel**: Monitor messages, transcripts, and costs at port 1111
- **Context Awareness**: Maintains conversation history for better responses

## Requirements

- [BlueBubbles](https://bluebubbles.app/) server running on macOS with iMessage access
- [Claude Code CLI](https://claude.ai/claude-code) installed and authenticated
- Rust toolchain
- (Optional) Gemini API key for group chat filtering

## Installation

1. **Clone and build**
   ```bash
   git clone https://github.com/yourusername/sink.git
   cd sink
   cargo build --release
   ```

2. **Create configuration**
   ```bash
   sudo mkdir -p /etc/sink
   sudo cp config.example.toml /etc/sink/config.toml
   sudo chown $USER:$USER /etc/sink/config.toml
   chmod 600 /etc/sink/config.toml
   ```

3. **Edit configuration**
   ```bash
   $EDITOR /etc/sink/config.toml
   ```

   Configure:
   - BlueBubbles server host, port, and password
   - Claude binary path (find with `which claude`)
   - Working directory for Claude sessions
   - (Optional) Gemini API key for group chat filtering

4. **Install and enable service**
   ```bash
   make install
   ```

   This will:
   - Copy binary to `/usr/local/bin/sink`
   - Install systemd service
   - Enable and start the daemon

## Usage

```bash
make status     # Check service status
make logs       # Tail service logs
make restart    # Restart service
make update     # Rebuild and restart after code changes
make uninstall  # Remove service (preserves data)
```

## Admin Panel

Access the web admin panel at `http://localhost:1111`:

- **Dashboard**: Message stats, costs, queue status
- **Messages**: View/filter all processed messages
- **Transcripts**: Full Claude session logs with token counts
- **Followups**: Manage scheduled notifications (if enabled)

For production, configure a reverse proxy with authentication.

## How It Works

1. Daemon polls BlueBubbles for new messages every 5 seconds
2. New messages are stored in SQLite database
3. For group chats, Gemini checks if message is directed at Claude
4. Messages directed at Claude are processed:
   - Build prompt with conversation history
   - Spawn Claude CLI with `--resume` for session continuity
   - Parse response and send back via BlueBubbles
5. Full transcript (prompt, response, cost, tokens) is logged

## Configuration

See `config.example.toml` for all options. Key settings:

| Setting | Description |
|---------|-------------|
| `bluebubbles.*` | BlueBubbles server connection |
| `claude.binary` | Path to Claude CLI |
| `claude.working_dir` | Working directory for Claude sessions |
| `polling.interval_secs` | How often to check for new messages |
| `gemini.api_key` | For group chat filtering (optional) |
| `web_server.port` | Admin panel port (default: 1111) |

## Data Storage

| Path | Content |
|------|---------|
| `/var/lib/sink/messages.db` | Message history and status |
| `/var/lib/sink/transcripts.db` | Full Claude session transcripts |
| `/var/lib/sink/followups.db` | Scheduled notifications |

## License

MIT
