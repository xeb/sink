use crate::error::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct Transcript {
    pub id: Option<i64>,
    pub message_guid: String,
    pub chat_guid: String,
    pub session_id: Option<String>,
    pub prompt_sent: String,
    pub raw_stdout: String,
    pub raw_stderr: Option<String>,
    pub exit_code: i32,
    pub messages_json: Option<String>,
    pub tool_calls_json: Option<String>,
    pub cost_usd: Option<f64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub claude_model: Option<String>,
    pub working_dir: String,
    pub duration_ms: i64,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct TranscriptMessage {
    pub id: Option<i64>,
    pub transcript_id: i64,
    pub role: String,
    pub content: String,
    pub tool_name: Option<String>,
    pub tool_input_json: Option<String>,
    pub tool_output_json: Option<String>,
    pub sequence_num: i32,
}

pub struct TranscriptsDb {
    conn: Connection,
}

impl TranscriptsDb {
    /// Get a reference to the underlying connection (for web queries)
    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

impl TranscriptsDb {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        let db = TranscriptsDb { conn };
        db.init()?;
        Ok(db)
    }

    fn init(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS transcripts (
                id INTEGER PRIMARY KEY,
                message_guid TEXT NOT NULL,
                chat_guid TEXT NOT NULL,
                session_id TEXT,
                prompt_sent TEXT NOT NULL,
                raw_stdout TEXT NOT NULL,
                raw_stderr TEXT,
                exit_code INTEGER NOT NULL,
                messages_json TEXT,
                tool_calls_json TEXT,
                cost_usd REAL,
                input_tokens INTEGER,
                output_tokens INTEGER,
                claude_model TEXT,
                working_dir TEXT NOT NULL,
                duration_ms INTEGER NOT NULL,
                created_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_transcripts_chat ON transcripts(chat_guid);
            CREATE INDEX IF NOT EXISTS idx_transcripts_session ON transcripts(session_id);
            CREATE INDEX IF NOT EXISTS idx_transcripts_created ON transcripts(created_at);
            CREATE INDEX IF NOT EXISTS idx_transcripts_message ON transcripts(message_guid);

            CREATE TABLE IF NOT EXISTS transcript_messages (
                id INTEGER PRIMARY KEY,
                transcript_id INTEGER NOT NULL,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                tool_name TEXT,
                tool_input_json TEXT,
                tool_output_json TEXT,
                sequence_num INTEGER NOT NULL,
                FOREIGN KEY (transcript_id) REFERENCES transcripts(id)
            );

            CREATE INDEX IF NOT EXISTS idx_tmsg_transcript ON transcript_messages(transcript_id);
            "#,
        )?;
        Ok(())
    }

    pub fn insert_transcript(&self, transcript: &Transcript) -> Result<i64> {
        self.conn.execute(
            r#"
            INSERT INTO transcripts (
                message_guid, chat_guid, session_id, prompt_sent, raw_stdout, raw_stderr,
                exit_code, messages_json, tool_calls_json, cost_usd, input_tokens,
                output_tokens, claude_model, working_dir, duration_ms, created_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
            params![
                transcript.message_guid,
                transcript.chat_guid,
                transcript.session_id,
                transcript.prompt_sent,
                transcript.raw_stdout,
                transcript.raw_stderr,
                transcript.exit_code,
                transcript.messages_json,
                transcript.tool_calls_json,
                transcript.cost_usd,
                transcript.input_tokens,
                transcript.output_tokens,
                transcript.claude_model,
                transcript.working_dir,
                transcript.duration_ms,
                transcript.created_at,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn insert_transcript_message(&self, msg: &TranscriptMessage) -> Result<i64> {
        self.conn.execute(
            r#"
            INSERT INTO transcript_messages (
                transcript_id, role, content, tool_name, tool_input_json,
                tool_output_json, sequence_num
            ) VALUES (?, ?, ?, ?, ?, ?, ?)
            "#,
            params![
                msg.transcript_id,
                msg.role,
                msg.content,
                msg.tool_name,
                msg.tool_input_json,
                msg.tool_output_json,
                msg.sequence_num,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn get_transcript_by_message_guid(&self, guid: &str) -> Result<Option<Transcript>> {
        let transcript = self
            .conn
            .query_row(
                r#"
                SELECT id, message_guid, chat_guid, session_id, prompt_sent, raw_stdout,
                       raw_stderr, exit_code, messages_json, tool_calls_json, cost_usd,
                       input_tokens, output_tokens, claude_model, working_dir, duration_ms, created_at
                FROM transcripts
                WHERE message_guid = ?
                ORDER BY created_at DESC
                LIMIT 1
                "#,
                [guid],
                |row| {
                    Ok(Transcript {
                        id: Some(row.get(0)?),
                        message_guid: row.get(1)?,
                        chat_guid: row.get(2)?,
                        session_id: row.get(3)?,
                        prompt_sent: row.get(4)?,
                        raw_stdout: row.get(5)?,
                        raw_stderr: row.get(6)?,
                        exit_code: row.get(7)?,
                        messages_json: row.get(8)?,
                        tool_calls_json: row.get(9)?,
                        cost_usd: row.get(10)?,
                        input_tokens: row.get(11)?,
                        output_tokens: row.get(12)?,
                        claude_model: row.get(13)?,
                        working_dir: row.get(14)?,
                        duration_ms: row.get(15)?,
                        created_at: row.get(16)?,
                    })
                },
            )
            .optional()?;
        Ok(transcript)
    }

    pub fn get_transcript_messages(&self, transcript_id: i64) -> Result<Vec<TranscriptMessage>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, transcript_id, role, content, tool_name, tool_input_json,
                   tool_output_json, sequence_num
            FROM transcript_messages
            WHERE transcript_id = ?
            ORDER BY sequence_num ASC
            "#,
        )?;

        let messages = stmt
            .query_map([transcript_id], |row| {
                Ok(TranscriptMessage {
                    id: Some(row.get(0)?),
                    transcript_id: row.get(1)?,
                    role: row.get(2)?,
                    content: row.get(3)?,
                    tool_name: row.get(4)?,
                    tool_input_json: row.get(5)?,
                    tool_output_json: row.get(6)?,
                    sequence_num: row.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(messages)
    }

    pub fn get_recent_for_chat(&self, chat_guid: &str, limit: usize) -> Result<Vec<Transcript>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, message_guid, chat_guid, session_id, prompt_sent, raw_stdout,
                   raw_stderr, exit_code, messages_json, tool_calls_json, cost_usd,
                   input_tokens, output_tokens, claude_model, working_dir, duration_ms, created_at
            FROM transcripts
            WHERE chat_guid = ?
            ORDER BY created_at DESC
            LIMIT ?
            "#,
        )?;

        let transcripts = stmt
            .query_map(params![chat_guid, limit as i64], |row| {
                Ok(Transcript {
                    id: Some(row.get(0)?),
                    message_guid: row.get(1)?,
                    chat_guid: row.get(2)?,
                    session_id: row.get(3)?,
                    prompt_sent: row.get(4)?,
                    raw_stdout: row.get(5)?,
                    raw_stderr: row.get(6)?,
                    exit_code: row.get(7)?,
                    messages_json: row.get(8)?,
                    tool_calls_json: row.get(9)?,
                    cost_usd: row.get(10)?,
                    input_tokens: row.get(11)?,
                    output_tokens: row.get(12)?,
                    claude_model: row.get(13)?,
                    working_dir: row.get(14)?,
                    duration_ms: row.get(15)?,
                    created_at: row.get(16)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(transcripts)
    }
}
