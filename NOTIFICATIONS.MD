# Sink Notifications Spec

Proactive follow-up notifications via iMessage, powered by Gemini extraction.

## Overview

This upgrade adds two major capabilities to the sink daemon:

1. **Session Transcript Storage**: Capture and store full Claude session output (all messages, tool calls, reasoning) in a dedicated SQLite database
2. **Proactive Notifications**: Use Gemini Flash to extract follow-up actions from conversations, then send reminder texts at appropriate times

## Architecture

```
                                    ┌─────────────────────┐
                                    │   transcripts.db    │
                                    │  (full session data)│
                                    └──────────▲──────────┘
                                               │
BlueBubbles ◄──── Sink Daemon ────► Claude CLI │
     │                │                        │
     │                │         store full ────┘
     │                │         session output
     │                │
     │                ▼
     │         ┌─────────────┐      ┌─────────────────┐
     │         │ Gemini Flash│ ───► │  followups.db   │
     │         │ (extraction)│      │ (pending tasks) │
     │         └─────────────┘      └────────┬────────┘
     │                                       │
     │◄─────── Notification Scheduler ◄──────┘
     │         (sends reminders)
     │
```

## Phase 1: Session Transcript Storage

### New Database: `transcripts.db`

Location: `/var/lib/sink/transcripts.db`

This database stores the complete Claude session output for every interaction, enabling post-hoc analysis, debugging, and follow-up extraction.

#### Schema

```sql
-- Full transcript of each Claude invocation
CREATE TABLE transcripts (
    id INTEGER PRIMARY KEY,
    message_guid TEXT NOT NULL,           -- Links to messages.guid in messages.db
    chat_guid TEXT NOT NULL,              -- Denormalized for fast lookup
    session_id TEXT,                      -- Claude session ID

    -- Raw I/O
    prompt_sent TEXT NOT NULL,            -- Full prompt sent to Claude
    raw_stdout TEXT NOT NULL,             -- Complete stdout from Claude CLI
    raw_stderr TEXT,                      -- Any stderr output
    exit_code INTEGER NOT NULL,           -- Process exit code

    -- Parsed session data (extracted from JSON output)
    messages_json TEXT,                   -- Full messages array from Claude
    tool_calls_json TEXT,                 -- All tool invocations
    cost_usd REAL,                        -- API cost if available
    input_tokens INTEGER,                 -- Token usage
    output_tokens INTEGER,

    -- Metadata
    claude_model TEXT,                    -- Model used (opus, sonnet, etc.)
    working_dir TEXT,                     -- Working directory for context
    duration_ms INTEGER,                  -- How long Claude took to respond
    created_at INTEGER NOT NULL,          -- Unix timestamp (ms)

    FOREIGN KEY (message_guid) REFERENCES messages(guid)
);

CREATE INDEX idx_transcripts_chat ON transcripts(chat_guid);
CREATE INDEX idx_transcripts_session ON transcripts(session_id);
CREATE INDEX idx_transcripts_created ON transcripts(created_at);

-- Store individual messages within a session for granular access
CREATE TABLE transcript_messages (
    id INTEGER PRIMARY KEY,
    transcript_id INTEGER NOT NULL,
    role TEXT NOT NULL,                   -- 'user', 'assistant', 'system', 'tool'
    content TEXT NOT NULL,
    tool_name TEXT,                        -- If role='tool', the tool that was called
    tool_input_json TEXT,                  -- Tool input parameters
    tool_output_json TEXT,                 -- Tool output
    sequence_num INTEGER NOT NULL,        -- Order within the conversation

    FOREIGN KEY (transcript_id) REFERENCES transcripts(id)
);

CREATE INDEX idx_tmsg_transcript ON transcript_messages(transcript_id);
```

### Capturing Session Data

Claude CLI with `--output-format json` returns structured output. We need to capture:

1. **Raw output**: Store complete stdout/stderr for debugging
2. **Parsed messages**: Extract the `messages` array containing the full conversation
3. **Tool calls**: Extract tool invocations (Read, Edit, Bash, etc.) with inputs/outputs
4. **Metadata**: Session ID, cost, tokens, duration

#### Implementation Changes to `claude.rs`

```rust
pub struct ClaudeResult {
    pub response_text: String,
    pub session_id: Option<String>,
    // New fields for transcript storage
    pub raw_stdout: String,
    pub raw_stderr: String,
    pub exit_code: i32,
    pub messages: Option<Vec<serde_json::Value>>,
    pub tool_calls: Option<Vec<serde_json::Value>>,
    pub cost_usd: Option<f64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub model: Option<String>,
    pub duration_ms: u64,
}
```

## Phase 2: Gemini Follow-up Extraction

### New Database: `followups.db`

Location: `/var/lib/sink/followups.db`

Stores extracted follow-up items with their scheduled notification times.

#### Schema

```sql
CREATE TABLE followups (
    id INTEGER PRIMARY KEY,
    transcript_id INTEGER NOT NULL,       -- Source transcript
    chat_guid TEXT NOT NULL,              -- Chat to notify
    sender TEXT NOT NULL,                 -- User to notify (for cross-chat lookup)
    original_message_guid TEXT NOT NULL,  -- Message to reply to (for threading)

    -- Follow-up details
    description TEXT NOT NULL,            -- What the follow-up is about
    context_summary TEXT,                 -- Brief context from conversation
    trigger_type TEXT NOT NULL,           -- 'explicit' or 'ai_detected'

    -- Scheduling
    notify_at INTEGER,                    -- Unix timestamp (ms) when to send
    notify_interval_mins INTEGER,         -- For recurring: interval in minutes
    notify_until INTEGER,                 -- For recurring: stop after this time
    recurrence_count INTEGER DEFAULT 0,   -- How many times this has recurred

    -- State
    status TEXT DEFAULT 'pending',        -- pending, sent, dismissed, expired
    created_at INTEGER NOT NULL,
    sent_at INTEGER,
    dismissed_at INTEGER,

    -- Response tracking
    notification_guid TEXT,               -- GUID of the notification message we sent
    user_response TEXT,                   -- What user replied (if anything)

    FOREIGN KEY (transcript_id) REFERENCES transcripts(id)
);

CREATE INDEX idx_followups_notify ON followups(notify_at) WHERE status = 'pending';
CREATE INDEX idx_followups_chat ON followups(chat_guid);
CREATE INDEX idx_followups_status ON followups(status);
CREATE INDEX idx_followups_sender ON followups(sender);  -- For cross-chat lookups

-- User preferences for notifications
CREATE TABLE user_preferences (
    id INTEGER PRIMARY KEY,
    chat_guid TEXT UNIQUE NOT NULL,       -- Per-chat preferences
    sender TEXT NOT NULL,                 -- Phone/email

    notifications_enabled INTEGER DEFAULT 1,  -- 1 = on, 0 = opted out
    quiet_hours_start INTEGER,            -- e.g., 2200 for 10pm
    quiet_hours_end INTEGER,              -- e.g., 0800 for 8am
    timezone TEXT DEFAULT 'America/Phoenix',

    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE UNIQUE INDEX idx_prefs_chat ON user_preferences(chat_guid);
```

### Gemini Integration

#### Configuration Addition

```toml
[gemini]
api_key = "your-gemini-api-key"
model = "gemini-2.0-flash"
extraction_prompt_template = """
Analyze this conversation between a user and Claude. Extract any follow-up actions needed.

For each follow-up, identify:
1. Type: "explicit" (user said "remind me", "follow up", etc.) or "ai_detected" (implicit need)
2. Description: What needs to be followed up on
3. Timing: When the notification should be sent (parse relative times like "in 2 hours", "tomorrow", "next week")
4. Recurring: If the user wants repeated reminders, specify interval and optional end condition
5. Context: Brief summary of why this follow-up matters

Return JSON array of follow-ups, or empty array if none found.

Example output:
[
  {
    "type": "explicit",
    "description": "Check if the deployment completed successfully",
    "notify_at_relative": "2 hours",
    "recurring": null,
    "context": "User deployed new feature and asked for reminder to verify"
  },
  {
    "type": "explicit",
    "description": "Daily standup reminder",
    "notify_at_relative": "tomorrow 9am",
    "recurring": {
      "interval_mins": 1440,
      "until_relative": "2 weeks"
    },
    "context": "User requested daily morning reminder for standup prep"
  },
  {
    "type": "ai_detected",
    "description": "Follow up on database migration decision",
    "notify_at_relative": "1 day",
    "recurring": null,
    "context": "User was weighing options between PostgreSQL and SQLite, seemed uncertain"
  }
]

Conversation to analyze (last 10 messages):
{conversation}
"""
```

#### Extraction Flow

```rust
pub struct GeminiExtractor {
    api_key: String,
    model: String,
    client: reqwest::Client,
    max_messages: usize,  // Hard cap: 10
}

impl GeminiExtractor {
    pub async fn extract_followups(
        &self,
        transcript: &Transcript,
        recent_messages: &[TranscriptMessage],  // Last 10 messages max
    ) -> Result<Vec<FollowUp>, Error> {
        // 1. Take last 10 messages from recent_messages (hard cap)
        // 2. Build conversation text with role labels
        // 3. Call Gemini API with extraction prompt
        // 4. Parse JSON response into FollowUp structs
        // 5. Calculate absolute notify_at times from relative times
        // 6. For recurring: parse interval and end conditions
    }
}
```

#### Gemini API Call

```
POST https://generativelanguage.googleapis.com/v1beta/models/gemini-2.0-flash:generateContent
Authorization: Bearer {api_key}

{
  "contents": [{
    "parts": [{"text": "{extraction_prompt}"}]
  }],
  "generationConfig": {
    "responseMimeType": "application/json"
  }
}
```

## Phase 3: Notification Scheduler

### Scheduler Design

A background task that:
1. Queries `followups.db` for pending items where `notify_at <= now()`
2. Respects quiet hours from `user_preferences`
3. Sends notification via BlueBubbles **as a reply to the original message**
4. Updates followup status to 'sent'
5. For recurring: reschedules next notification if `notify_interval_mins` is set

#### Notification Threading

Notifications are sent as **threaded replies** to the original message that created the follow-up. This requires:
- Storing `original_message_guid` in the followups table (the message that spawned the follow-up)
- Using BlueBubbles reply API: `POST /api/v1/message/text` with `selectedMessageGuid` parameter

#### Notification Message Format

```
Hey! This is a follow-up from our earlier conversation:

"{description}"

Context: {context_summary}

Reply with anything to acknowledge, or "snooze 1h" / "dismiss" to manage this reminder.
```

#### Recurring Reminder Handling

When a recurring reminder is sent:
1. Send the notification
2. Check if `notify_until` has passed → if so, mark as `expired`
3. Otherwise, calculate `next_notify_at = now() + notify_interval_mins`
4. Create a new followup entry with the updated time (or update in place)

### New Module: `scheduler.rs`

```rust
pub struct NotificationScheduler {
    db: FollowupsDb,
    sender: Sender,
    poll_interval: Duration,
}

impl NotificationScheduler {
    pub async fn run(&self) {
        loop {
            // 1. Get due notifications
            let due = self.db.get_due_followups().await?;

            for followup in due {
                // 2. Check user preferences (opted out? quiet hours?)
                if !self.should_notify(&followup).await {
                    continue;
                }

                // 3. Send notification
                let guid = self.sender.send_notification(&followup).await?;

                // 4. Mark as sent
                self.db.mark_sent(followup.id, guid).await?;
            }

            tokio::time::sleep(self.poll_interval).await;
        }
    }
}
```

## User Interaction Commands

Users can control notifications via text:

| Command | Action |
|---------|--------|
| "disable reminders" | Sets `notifications_enabled = 0` for this chat |
| "enable reminders" | Sets `notifications_enabled = 1` for this chat |
| "snooze 1h" | Reschedules latest notification +1 hour |
| "snooze 1d" | Reschedules latest notification +1 day |
| "dismiss" | Marks latest notification as dismissed |
| "quiet hours 10pm-8am" | Sets quiet hours for this chat |
| "list reminders" | Shows pending follow-ups for this chat |

These commands should be detected in `main.rs` before invoking Claude, handled locally.

## Configuration Changes

```toml
[notifications]
enabled = true
scheduler_interval_secs = 60              # How often to check for due notifications
default_snooze_mins = 60                  # Default snooze duration
max_retries = 3                           # Retry failed notification sends

[gemini]
api_key = "your-gemini-api-key"
model = "gemini-2.0-flash"
timeout_secs = 30

[databases]
messages = "/var/lib/sink/messages.db"    # Existing
transcripts = "/var/lib/sink/transcripts.db"
followups = "/var/lib/sink/followups.db"
```

## Implementation Order

### Step 1: Transcript Storage (Foundation)
- [ ] Create `transcripts.db` schema and Rust structs
- [ ] Modify `ClaudeInvoker` to capture full output
- [ ] Add `TranscriptsDb` module for storage operations
- [ ] Store transcript after each Claude invocation
- [ ] Add CLI command to query transcripts for debugging

### Step 2: Gemini Extraction
- [ ] Add Gemini API client (`gemini.rs`)
- [ ] Create extraction prompt template
- [ ] Create `followups.db` schema and Rust structs
- [ ] Extract follow-ups after storing transcript
- [ ] Parse relative times into absolute timestamps

### Step 3: Notification Scheduler
- [ ] Implement `NotificationScheduler` background task
- [ ] Add user preference detection (opt-out, quiet hours)
- [ ] Format and send notification messages
- [ ] Track notification responses

### Step 4: User Commands
- [ ] Detect control commands before Claude invocation
- [ ] Implement snooze/dismiss/enable/disable handlers
- [ ] Add "list reminders" query support

## File Structure After Implementation

```
src/
├── main.rs           # Daemon loop (modified)
├── config.rs         # Config loading (modified)
├── db.rs             # messages.db operations (existing)
├── transcripts.rs    # NEW: transcripts.db operations
├── followups.rs      # NEW: followups.db operations
├── gemini.rs         # NEW: Gemini API client
├── scheduler.rs      # NEW: Notification scheduler
├── commands.rs       # NEW: User command parser
├── poller.rs         # BlueBubbles polling (existing)
├── claude.rs         # Claude invocation (modified)
├── sender.rs         # Response sending (existing)
└── error.rs          # Error types (modified)
```

## Design Decisions

1. **Session History Depth**: Hard cap of **10 messages** sent to Gemini for extraction. This matches the existing context window used for Claude and keeps extraction costs low.

2. **Notification Threading**: Notifications are sent as **replies to the original message** that spawned the follow-up. This keeps context visible and avoids cluttering the chat with disconnected messages.

3. **Recurring Follow-ups**: **Supported**. Users can request recurring reminders (e.g., "remind me every day at 9am"). The scheduler checks `notify_interval_mins` and reschedules after each send until `notify_until` is reached.

4. **Cross-Chat Context**: Follow-ups are **cross-chat aware**. A user identified by phone/email can have follow-ups tracked across all their conversations. The scheduler looks up the user by `sender` field, not just `chat_guid`, enabling unified reminder management.

## Security Considerations

- Gemini API key stored in config file (already mode 600)
- Transcripts contain full conversation history - ensure proper file permissions on `transcripts.db`
- User phone numbers stored in multiple places - already handled by existing security model
- Gemini sees conversation content - consider data retention policies
