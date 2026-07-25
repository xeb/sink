# 🚰 Sink

> **Requires [BlueBubbles](https://bluebubbles.app/) running on a Mac with iMessage access**

A Rust daemon that bridges iMessage (via BlueBubbles) with Claude Code. When someone texts Claude's iMessage account, the daemon injects commands into an interactive Claude Code session (running in tmux), captures the response, and sends it back via iMessage.

## Key Innovation: Interactive Claude Code via tmux

Instead of spawning new Claude subprocess instances for each message, **Sink injects commands directly into a persistent Claude Code interactive session**. This approach offers several advantages:

### Why This Is Better

✅ **Real-time Visibility**: Watch what Claude is doing in real time—see tool usage, file access, command execution
✅ **Persistent Context**: Claude maintains session state across messages, enabling more sophisticated workflows
✅ **No Subprocess Overhead**: Much faster response times with minimal resource usage
✅ **Guaranteed Output Format**: Commands are wrapped with unique IDs—Claude responds with matching tags, ensuring correct reply extraction
✅ **Debuggable**: You can manually interact with Claude in the same terminal for testing and debugging

### How It Works

1. **Setup**: You run Claude Code in an interactive tmux window (e.g., `tmux new-session -s main -n "sink MASTER"`)
2. **Message Arrives**: Daemon receives iMessage from BlueBubbles
3. **Tag Wrapping**: Daemon wraps the message in structured tags with a unique ID:
   ```
   [CMD-a1b2]What files are in this directory?[/CMD-a1b2]
   ```
4. **Injection**: Daemon sends the wrapped command to the tmux window via `tmux send-keys`
5. **Claude Processes**: Claude Code reads the command and executes it in real time (you see it happening!)
6. **Response Tags**: Claude responds with matching ID tags:
   ```
   [REPLY-a1b2]There are 42 files in the current directory[/REPLY-a1b2]
   ```
7. **Extraction & Send**: Daemon extracts the reply and sends it back as an iMessage

## Features

- **iMessage Bridge**: Receives messages via BlueBubbles API, sends responses back
- **Interactive Claude Code**: Commands execute in real Claude Code session (not subprocess spawning)
- **Unique ID Tagging**: Each command gets a 4-character ID—responses are matched by ID, never by buffer position
- **Group Chat Filtering**: Uses Gemini to detect if messages are directed at Claude
- **Web Admin Panel**: Monitor messages, transcripts, and costs at port 1111
- **Context Awareness**: Maintains conversation history for better responses
- **Visible Execution**: Watch all tool use, file operations, and command execution in real time

## Requirements

- [BlueBubbles](https://bluebubbles.app/) server running on macOS with iMessage access
- [Claude Code](https://claude.ai/claude-code) CLI installed and authenticated
- Rust toolchain (for building)
- tmux (for running the persistent Claude session)
- (Optional) Gemini API key for group chat filtering

## Installation

### 1. Start Claude Code in a tmux Window

First, set up the interactive Claude Code session that Sink will use:

```bash
# Create or attach to a tmux session
tmux new-session -s main -n "sink MASTER"

# In the tmux window, start Claude Code
claude -p "You are Claude, an AI assistant."
```

Keep this window open—it's where Sink will inject commands.

### 2. Clone and Build Sink

```bash
git clone https://github.com/xeb/sink.git
cd sink
cargo build --release
```

### 3. Create Configuration

```bash
sudo mkdir -p /etc/sink
sudo cp config.example.toml /etc/sink/config.toml
sudo chown $USER:$USER /etc/sink/config.toml
chmod 600 /etc/sink/config.toml
```

### 4. Edit Configuration

```bash
$EDITOR /etc/sink/config.toml
```

Key settings:

```toml
[bluebubbles]
host = "YOUR_BB_HOST"
port = 1234
password = "YOUR_BB_PASSWORD"

[tmux]
session = "main"                 # Your tmux session name
window = "sink MASTER"           # Your tmux window name
prompt = "❯"                     # Claude Code's prompt character
timeout_secs = 300              # Max wait time for responses
capture_interval_ms = 200       # Poll frequency

[polling]
interval_secs = 5               # How often to check for new messages
batch_window_secs = 2           # Wait 2s for message batching

[database]
path = "/var/lib/sink/messages.db"

[context]
message_history_count = 20      # Messages to send as context
```

### 5. Install and Enable Service

```bash
make install
```

Or manually:

```bash
cargo build --release
cp target/release/sink ~/.local/bin/sink
systemctl --user enable sink
systemctl --user start sink
```

## Usage

Check the daemon status and logs:

```bash
# Check status
systemctl --user status sink

# Watch logs in real time
journalctl --user -u sink -f

# Restart
systemctl --user restart sink
```

## How It Works

### Message Flow

```
iMessage arrives
       ↓
BlueBubbles API polls daemon
       ↓
Daemon generates unique ID (e.g., "a1b2")
       ↓
Wraps message: [CMD-a1b2]message[/CMD-a1b2]
       ↓
Injects into tmux window via send-keys
       ↓
Claude Code reads and executes in real time
(YOU CAN WATCH IT IN THE TERMINAL!)
       ↓
Claude responds with: [REPLY-a1b2]answer[/REPLY-a1b2]
       ↓
Daemon extracts answer and sends via BlueBubbles
       ↓
User receives response as iMessage
```

### Why ID Tagging?

Without unique IDs, the daemon might extract an old response from the tmux buffer. With IDs:

```
Old message:  [CMD-xyz1]Old question[/CMD-xyz1]
              [REPLY-xyz1]Old answer[/REPLY-xyz1]  ← Daemon ignores this

New message:  [CMD-a1b2]New question[/CMD-a1b2]
              [REPLY-a1b2]New answer[/REPLY-a1b2]  ← Daemon extracts THIS
```

## Admin Panel

Access the web admin panel at `http://localhost:1111`.

It is a single page: a flat, newest-first list of every inbound message with its outcome
(`replied` / `failed` / `waiting` / `ignored`). Click any row to expand it in place and read
Claude's reply along with the round-trip time; failed messages show the recorded reason instead.
Filter by outcome or search the message text.

The panel exposes one endpoint, `GET /api/messages?limit&offset&status&q`, and reads only
`messages.db`.

The panel has no authentication of its own — put it behind a reverse proxy or an
identity-aware proxy before exposing it beyond your machine.

## Configuration Reference

| Setting | Description | Default |
|---------|-------------|---------|
| `tmux.session` | tmux session name | `main` |
| `tmux.window` | tmux window name | `sink MASTER` |
| `tmux.prompt` | Claude Code prompt character | `❯` |
| `tmux.timeout_secs` | Max wait for response | `300` |
| `tmux.capture_interval_ms` | Poll frequency | `200` |
| `polling.interval_secs` | Check messages every N seconds | `5` |
| `polling.batch_window_secs` | Wait for message batching | `2` |
| `context.message_history_count` | Messages to send as context | `20` |

## Data Storage

| Path | Content |
|------|---------|
| `/var/lib/sink/messages.db` | Message history and status |
| `/var/lib/sink/transcripts.db` | Full session transcripts (if enabled) |
| `/var/lib/sink/followups.db` | Scheduled notifications |

## Debugging

### Check if tmux window is set up correctly

```bash
tmux list-windows -t main
# Should show: 0: sink MASTER (attached)
```

### Watch the tmux window in real time

```bash
# Attach to the window
tmux attach-session -t main:sink\ MASTER
```

### Check daemon logs

```bash
journalctl --user -u sink -f

# Look for:
# - "TMUX: Sending wrapped command with ID: ..."
# - "TMUX: Found [REPLY-...] tags after X polls"
# - "Successfully processed and responded to message"
```

### Manual command injection (for testing)

```bash
# Inject a command directly
tmux send-keys -t main:sink\ MASTER -l "[CMD-test]echo hello[/CMD-test]"
tmux send-keys -t main:sink\ MASTER Enter
```

## Design Philosophy

- **Visibility First**: You can see what Claude is doing at all times
- **Simplicity**: No complex subprocess management or transcript parsing
- **Reliability**: Unique ID tagging ensures correct response extraction every time
- **Real-time**: Commands execute immediately in a persistent session
- **Debuggable**: Manual testing and inspection is straightforward

## License

MIT
