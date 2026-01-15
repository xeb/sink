use crate::error::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Message {
    pub id: Option<i64>,
    pub guid: String,
    pub chat_guid: String,
    pub sender: String,
    pub text: String,
    pub date_received: i64,
    pub processed_at: Option<i64>,
    pub response_guid: Option<String>,
    pub session_id: Option<String>,
    pub status: String,
    pub is_from_me: bool,
}

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path)?;
        let db = Database { conn };
        db.init()?;
        Ok(db)
    }

    fn init(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY,
                guid TEXT UNIQUE NOT NULL,
                chat_guid TEXT NOT NULL,
                sender TEXT NOT NULL,
                text TEXT NOT NULL,
                date_received INTEGER NOT NULL,
                processed_at INTEGER,
                response_guid TEXT,
                session_id TEXT,
                status TEXT DEFAULT 'pending',
                is_from_me INTEGER NOT NULL DEFAULT 0
            );

            CREATE INDEX IF NOT EXISTS idx_messages_chat_guid ON messages(chat_guid);
            CREATE INDEX IF NOT EXISTS idx_messages_status ON messages(status);
            CREATE INDEX IF NOT EXISTS idx_messages_date ON messages(date_received);
            "#,
        )?;
        Ok(())
    }

    pub fn message_exists(&self, guid: &str) -> Result<bool> {
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE guid = ?)",
            [guid],
            |row| row.get(0),
        )?;
        Ok(exists)
    }

    pub fn insert_message(&self, msg: &Message) -> Result<i64> {
        self.conn.execute(
            r#"
            INSERT OR IGNORE INTO messages
                (guid, chat_guid, sender, text, date_received, status, is_from_me)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            params![
                msg.guid,
                msg.chat_guid,
                msg.sender,
                msg.text,
                msg.date_received,
                msg.status,
                msg.is_from_me as i32,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn get_pending_messages(&self) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, guid, chat_guid, sender, text, date_received,
                   processed_at, response_guid, session_id, status, is_from_me
            FROM messages
            WHERE status = 'pending' AND is_from_me = 0
            ORDER BY date_received ASC
            "#,
        )?;

        let messages = stmt
            .query_map([], |row| {
                Ok(Message {
                    id: Some(row.get(0)?),
                    guid: row.get(1)?,
                    chat_guid: row.get(2)?,
                    sender: row.get(3)?,
                    text: row.get(4)?,
                    date_received: row.get(5)?,
                    processed_at: row.get(6)?,
                    response_guid: row.get(7)?,
                    session_id: row.get(8)?,
                    status: row.get(9)?,
                    is_from_me: row.get::<_, i32>(10)? != 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(messages)
    }

    pub fn get_recent_messages_for_chat(&self, chat_guid: &str, limit: usize) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, guid, chat_guid, sender, text, date_received,
                   processed_at, response_guid, session_id, status, is_from_me
            FROM messages
            WHERE chat_guid = ?
            ORDER BY date_received DESC
            LIMIT ?
            "#,
        )?;

        let messages = stmt
            .query_map(params![chat_guid, limit as i64], |row| {
                Ok(Message {
                    id: Some(row.get(0)?),
                    guid: row.get(1)?,
                    chat_guid: row.get(2)?,
                    sender: row.get(3)?,
                    text: row.get(4)?,
                    date_received: row.get(5)?,
                    processed_at: row.get(6)?,
                    response_guid: row.get(7)?,
                    session_id: row.get(8)?,
                    status: row.get(9)?,
                    is_from_me: row.get::<_, i32>(10)? != 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        // Reverse to get chronological order
        let mut messages = messages;
        messages.reverse();
        Ok(messages)
    }

    pub fn update_status(&self, guid: &str, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE messages SET status = ? WHERE guid = ?",
            params![status, guid],
        )?;
        Ok(())
    }

    pub fn mark_processed(&self, guid: &str, response_guid: Option<&str>, session_id: Option<&str>) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        self.conn.execute(
            r#"
            UPDATE messages
            SET status = 'completed', processed_at = ?, response_guid = ?, session_id = ?
            WHERE guid = ?
            "#,
            params![now, response_guid, session_id, guid],
        )?;
        Ok(())
    }

    pub fn mark_failed(&self, guid: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        self.conn.execute(
            "UPDATE messages SET status = 'failed', processed_at = ? WHERE guid = ?",
            params![now, guid],
        )?;
        Ok(())
    }

    pub fn get_session_for_chat(&self, chat_guid: &str) -> Result<Option<String>> {
        let session: Option<String> = self.conn.query_row(
            r#"
            SELECT session_id FROM messages
            WHERE chat_guid = ? AND session_id IS NOT NULL
            ORDER BY date_received DESC
            LIMIT 1
            "#,
            [chat_guid],
            |row| row.get(0),
        ).optional()?;
        Ok(session)
    }

    pub fn is_processing(&self) -> Result<bool> {
        let processing: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE status = 'processing')",
            [],
            |row| row.get(0),
        )?;
        Ok(processing)
    }
}
