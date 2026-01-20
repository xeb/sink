use crate::error::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
pub struct Followup {
    pub id: Option<i64>,
    pub transcript_id: i64,
    pub chat_guid: String,
    pub sender: String,
    pub original_message_guid: String,
    pub description: String,
    pub context_summary: Option<String>,
    pub trigger_type: String,
    pub notify_at: Option<i64>,
    pub notify_interval_mins: Option<i64>,
    pub notify_until: Option<i64>,
    pub recurrence_count: i32,
    pub status: String,
    pub created_at: i64,
    pub sent_at: Option<i64>,
    pub dismissed_at: Option<i64>,
    pub notification_guid: Option<String>,
    pub user_response: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UserPreference {
    pub id: Option<i64>,
    pub chat_guid: String,
    pub sender: String,
    pub notifications_enabled: bool,
    pub quiet_hours_start: Option<i32>,
    pub quiet_hours_end: Option<i32>,
    pub timezone: String,
    pub created_at: i64,
    pub updated_at: i64,
}

pub struct FollowupsDb {
    conn: Connection,
}

impl FollowupsDb {
    /// Get a reference to the underlying connection (for web queries)
    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

impl FollowupsDb {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        let db = FollowupsDb { conn };
        db.init()?;
        Ok(db)
    }

    fn init(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS followups (
                id INTEGER PRIMARY KEY,
                transcript_id INTEGER NOT NULL,
                chat_guid TEXT NOT NULL,
                sender TEXT NOT NULL,
                original_message_guid TEXT NOT NULL,
                description TEXT NOT NULL,
                context_summary TEXT,
                trigger_type TEXT NOT NULL,
                notify_at INTEGER,
                notify_interval_mins INTEGER,
                notify_until INTEGER,
                recurrence_count INTEGER DEFAULT 0,
                status TEXT DEFAULT 'pending',
                created_at INTEGER NOT NULL,
                sent_at INTEGER,
                dismissed_at INTEGER,
                notification_guid TEXT,
                user_response TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_followups_notify ON followups(notify_at) WHERE status = 'pending';
            CREATE INDEX IF NOT EXISTS idx_followups_chat ON followups(chat_guid);
            CREATE INDEX IF NOT EXISTS idx_followups_status ON followups(status);
            CREATE INDEX IF NOT EXISTS idx_followups_sender ON followups(sender);

            CREATE TABLE IF NOT EXISTS user_preferences (
                id INTEGER PRIMARY KEY,
                chat_guid TEXT UNIQUE NOT NULL,
                sender TEXT NOT NULL,
                notifications_enabled INTEGER DEFAULT 1,
                quiet_hours_start INTEGER,
                quiet_hours_end INTEGER,
                timezone TEXT DEFAULT 'America/Phoenix',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE UNIQUE INDEX IF NOT EXISTS idx_prefs_chat ON user_preferences(chat_guid);
            "#,
        )?;
        Ok(())
    }

    pub fn insert_followup(&self, followup: &Followup) -> Result<i64> {
        self.conn.execute(
            r#"
            INSERT INTO followups (
                transcript_id, chat_guid, sender, original_message_guid, description,
                context_summary, trigger_type, notify_at, notify_interval_mins,
                notify_until, recurrence_count, status, created_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
            params![
                followup.transcript_id,
                followup.chat_guid,
                followup.sender,
                followup.original_message_guid,
                followup.description,
                followup.context_summary,
                followup.trigger_type,
                followup.notify_at,
                followup.notify_interval_mins,
                followup.notify_until,
                followup.recurrence_count,
                followup.status,
                followup.created_at,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn get_due_followups(&self) -> Result<Vec<Followup>> {
        let now = chrono::Utc::now().timestamp_millis();
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, transcript_id, chat_guid, sender, original_message_guid, description,
                   context_summary, trigger_type, notify_at, notify_interval_mins, notify_until,
                   recurrence_count, status, created_at, sent_at, dismissed_at,
                   notification_guid, user_response
            FROM followups
            WHERE status = 'pending' AND notify_at IS NOT NULL AND notify_at <= ?
            ORDER BY notify_at ASC
            "#,
        )?;

        let followups = stmt
            .query_map([now], |row| {
                Ok(Followup {
                    id: Some(row.get(0)?),
                    transcript_id: row.get(1)?,
                    chat_guid: row.get(2)?,
                    sender: row.get(3)?,
                    original_message_guid: row.get(4)?,
                    description: row.get(5)?,
                    context_summary: row.get(6)?,
                    trigger_type: row.get(7)?,
                    notify_at: row.get(8)?,
                    notify_interval_mins: row.get(9)?,
                    notify_until: row.get(10)?,
                    recurrence_count: row.get(11)?,
                    status: row.get(12)?,
                    created_at: row.get(13)?,
                    sent_at: row.get(14)?,
                    dismissed_at: row.get(15)?,
                    notification_guid: row.get(16)?,
                    user_response: row.get(17)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(followups)
    }

    pub fn get_pending_for_chat(&self, chat_guid: &str) -> Result<Vec<Followup>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, transcript_id, chat_guid, sender, original_message_guid, description,
                   context_summary, trigger_type, notify_at, notify_interval_mins, notify_until,
                   recurrence_count, status, created_at, sent_at, dismissed_at,
                   notification_guid, user_response
            FROM followups
            WHERE chat_guid = ? AND status = 'pending'
            ORDER BY notify_at ASC
            "#,
        )?;

        let followups = stmt
            .query_map([chat_guid], |row| {
                Ok(Followup {
                    id: Some(row.get(0)?),
                    transcript_id: row.get(1)?,
                    chat_guid: row.get(2)?,
                    sender: row.get(3)?,
                    original_message_guid: row.get(4)?,
                    description: row.get(5)?,
                    context_summary: row.get(6)?,
                    trigger_type: row.get(7)?,
                    notify_at: row.get(8)?,
                    notify_interval_mins: row.get(9)?,
                    notify_until: row.get(10)?,
                    recurrence_count: row.get(11)?,
                    status: row.get(12)?,
                    created_at: row.get(13)?,
                    sent_at: row.get(14)?,
                    dismissed_at: row.get(15)?,
                    notification_guid: row.get(16)?,
                    user_response: row.get(17)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(followups)
    }

    pub fn get_pending_for_sender(&self, sender: &str) -> Result<Vec<Followup>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, transcript_id, chat_guid, sender, original_message_guid, description,
                   context_summary, trigger_type, notify_at, notify_interval_mins, notify_until,
                   recurrence_count, status, created_at, sent_at, dismissed_at,
                   notification_guid, user_response
            FROM followups
            WHERE sender = ? AND status = 'pending'
            ORDER BY notify_at ASC
            "#,
        )?;

        let followups = stmt
            .query_map([sender], |row| {
                Ok(Followup {
                    id: Some(row.get(0)?),
                    transcript_id: row.get(1)?,
                    chat_guid: row.get(2)?,
                    sender: row.get(3)?,
                    original_message_guid: row.get(4)?,
                    description: row.get(5)?,
                    context_summary: row.get(6)?,
                    trigger_type: row.get(7)?,
                    notify_at: row.get(8)?,
                    notify_interval_mins: row.get(9)?,
                    notify_until: row.get(10)?,
                    recurrence_count: row.get(11)?,
                    status: row.get(12)?,
                    created_at: row.get(13)?,
                    sent_at: row.get(14)?,
                    dismissed_at: row.get(15)?,
                    notification_guid: row.get(16)?,
                    user_response: row.get(17)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(followups)
    }

    pub fn get_latest_pending_for_chat(&self, chat_guid: &str) -> Result<Option<Followup>> {
        let followup = self
            .conn
            .query_row(
                r#"
                SELECT id, transcript_id, chat_guid, sender, original_message_guid, description,
                       context_summary, trigger_type, notify_at, notify_interval_mins, notify_until,
                       recurrence_count, status, created_at, sent_at, dismissed_at,
                       notification_guid, user_response
                FROM followups
                WHERE chat_guid = ? AND status = 'pending'
                ORDER BY created_at DESC
                LIMIT 1
                "#,
                [chat_guid],
                |row| {
                    Ok(Followup {
                        id: Some(row.get(0)?),
                        transcript_id: row.get(1)?,
                        chat_guid: row.get(2)?,
                        sender: row.get(3)?,
                        original_message_guid: row.get(4)?,
                        description: row.get(5)?,
                        context_summary: row.get(6)?,
                        trigger_type: row.get(7)?,
                        notify_at: row.get(8)?,
                        notify_interval_mins: row.get(9)?,
                        notify_until: row.get(10)?,
                        recurrence_count: row.get(11)?,
                        status: row.get(12)?,
                        created_at: row.get(13)?,
                        sent_at: row.get(14)?,
                        dismissed_at: row.get(15)?,
                        notification_guid: row.get(16)?,
                        user_response: row.get(17)?,
                    })
                },
            )
            .optional()?;
        Ok(followup)
    }

    pub fn mark_sent(&self, id: i64, notification_guid: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        self.conn.execute(
            "UPDATE followups SET status = 'sent', sent_at = ?, notification_guid = ? WHERE id = ?",
            params![now, notification_guid, id],
        )?;
        Ok(())
    }

    pub fn mark_dismissed(&self, id: i64) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        self.conn.execute(
            "UPDATE followups SET status = 'dismissed', dismissed_at = ? WHERE id = ?",
            params![now, id],
        )?;
        Ok(())
    }

    pub fn mark_expired(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE followups SET status = 'expired' WHERE id = ?",
            params![id],
        )?;
        Ok(())
    }

    pub fn snooze(&self, id: i64, new_notify_at: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE followups SET notify_at = ?, status = 'pending' WHERE id = ?",
            params![new_notify_at, id],
        )?;
        Ok(())
    }

    pub fn reschedule_recurring(&self, followup: &Followup) -> Result<()> {
        if let Some(interval_mins) = followup.notify_interval_mins {
            let now = chrono::Utc::now().timestamp_millis();
            let next_notify = now + (interval_mins * 60 * 1000);

            // Check if past until time
            if let Some(until) = followup.notify_until {
                if next_notify > until {
                    // Mark as expired instead of rescheduling
                    if let Some(id) = followup.id {
                        return self.mark_expired(id);
                    }
                    return Ok(());
                }
            }

            // Update the followup with new notify_at and increment recurrence_count
            if let Some(id) = followup.id {
                self.conn.execute(
                    "UPDATE followups SET notify_at = ?, recurrence_count = recurrence_count + 1, status = 'pending' WHERE id = ?",
                    params![next_notify, id],
                )?;
            }
        }
        Ok(())
    }

    // User preference operations

    pub fn get_preference(&self, chat_guid: &str) -> Result<Option<UserPreference>> {
        let pref = self
            .conn
            .query_row(
                r#"
                SELECT id, chat_guid, sender, notifications_enabled, quiet_hours_start,
                       quiet_hours_end, timezone, created_at, updated_at
                FROM user_preferences
                WHERE chat_guid = ?
                "#,
                [chat_guid],
                |row| {
                    Ok(UserPreference {
                        id: Some(row.get(0)?),
                        chat_guid: row.get(1)?,
                        sender: row.get(2)?,
                        notifications_enabled: row.get::<_, i32>(3)? != 0,
                        quiet_hours_start: row.get(4)?,
                        quiet_hours_end: row.get(5)?,
                        timezone: row.get(6)?,
                        created_at: row.get(7)?,
                        updated_at: row.get(8)?,
                    })
                },
            )
            .optional()?;
        Ok(pref)
    }

    pub fn upsert_preference(&self, pref: &UserPreference) -> Result<()> {
        self.conn.execute(
            r#"
            INSERT INTO user_preferences (
                chat_guid, sender, notifications_enabled, quiet_hours_start,
                quiet_hours_end, timezone, created_at, updated_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(chat_guid) DO UPDATE SET
                notifications_enabled = excluded.notifications_enabled,
                quiet_hours_start = excluded.quiet_hours_start,
                quiet_hours_end = excluded.quiet_hours_end,
                timezone = excluded.timezone,
                updated_at = excluded.updated_at
            "#,
            params![
                pref.chat_guid,
                pref.sender,
                pref.notifications_enabled as i32,
                pref.quiet_hours_start,
                pref.quiet_hours_end,
                pref.timezone,
                pref.created_at,
                pref.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn set_notifications_enabled(
        &self,
        chat_guid: &str,
        sender: &str,
        enabled: bool,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        self.conn.execute(
            r#"
            INSERT INTO user_preferences (chat_guid, sender, notifications_enabled, timezone, created_at, updated_at)
            VALUES (?, ?, ?, 'America/Phoenix', ?, ?)
            ON CONFLICT(chat_guid) DO UPDATE SET
                notifications_enabled = excluded.notifications_enabled,
                updated_at = excluded.updated_at
            "#,
            params![chat_guid, sender, enabled as i32, now, now],
        )?;
        Ok(())
    }

    pub fn set_quiet_hours(
        &self,
        chat_guid: &str,
        sender: &str,
        start: i32,
        end: i32,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp_millis();
        self.conn.execute(
            r#"
            INSERT INTO user_preferences (chat_guid, sender, notifications_enabled, quiet_hours_start, quiet_hours_end, timezone, created_at, updated_at)
            VALUES (?, ?, 1, ?, ?, 'America/Phoenix', ?, ?)
            ON CONFLICT(chat_guid) DO UPDATE SET
                quiet_hours_start = excluded.quiet_hours_start,
                quiet_hours_end = excluded.quiet_hours_end,
                updated_at = excluded.updated_at
            "#,
            params![chat_guid, sender, start, end, now, now],
        )?;
        Ok(())
    }

    /// Get a single followup by ID
    pub fn get_followup(&self, id: i64) -> Result<Option<Followup>> {
        let followup = self
            .conn
            .query_row(
                r#"
                SELECT id, transcript_id, chat_guid, sender, original_message_guid,
                       description, context_summary, trigger_type, notify_at,
                       notify_interval_mins, notify_until, recurrence_count, status,
                       created_at, sent_at, dismissed_at, notification_guid, user_response
                FROM followups WHERE id = ?
                "#,
                [id],
                |row| {
                    Ok(Followup {
                        id: Some(row.get(0)?),
                        transcript_id: row.get(1)?,
                        chat_guid: row.get(2)?,
                        sender: row.get(3)?,
                        original_message_guid: row.get(4)?,
                        description: row.get(5)?,
                        context_summary: row.get(6)?,
                        trigger_type: row.get(7)?,
                        notify_at: row.get(8)?,
                        notify_interval_mins: row.get(9)?,
                        notify_until: row.get(10)?,
                        recurrence_count: row.get(11)?,
                        status: row.get(12)?,
                        created_at: row.get(13)?,
                        sent_at: row.get(14)?,
                        dismissed_at: row.get(15)?,
                        notification_guid: row.get(16)?,
                        user_response: row.get(17)?,
                    })
                },
            )
            .optional()?;
        Ok(followup)
    }

    /// Get all followups (for admin panel)
    pub fn get_all_followups(&self) -> Result<Vec<Followup>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, transcript_id, chat_guid, sender, original_message_guid,
                   description, context_summary, trigger_type, notify_at,
                   notify_interval_mins, notify_until, recurrence_count, status,
                   created_at, sent_at, dismissed_at, notification_guid, user_response
            FROM followups ORDER BY created_at DESC
            "#,
        )?;
        let followups = stmt
            .query_map([], |row| {
                Ok(Followup {
                    id: Some(row.get(0)?),
                    transcript_id: row.get(1)?,
                    chat_guid: row.get(2)?,
                    sender: row.get(3)?,
                    original_message_guid: row.get(4)?,
                    description: row.get(5)?,
                    context_summary: row.get(6)?,
                    trigger_type: row.get(7)?,
                    notify_at: row.get(8)?,
                    notify_interval_mins: row.get(9)?,
                    notify_until: row.get(10)?,
                    recurrence_count: row.get(11)?,
                    status: row.get(12)?,
                    created_at: row.get(13)?,
                    sent_at: row.get(14)?,
                    dismissed_at: row.get(15)?,
                    notification_guid: row.get(16)?,
                    user_response: row.get(17)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(followups)
    }

    /// Update a followup (for admin panel)
    pub fn update_followup(
        &self,
        id: i64,
        description: Option<&str>,
        notify_at: Option<i64>,
        status: Option<&str>,
    ) -> Result<()> {
        if let Some(desc) = description {
            self.conn.execute(
                "UPDATE followups SET description = ? WHERE id = ?",
                params![desc, id],
            )?;
        }
        if let Some(at) = notify_at {
            self.conn.execute(
                "UPDATE followups SET notify_at = ? WHERE id = ?",
                params![at, id],
            )?;
        }
        if let Some(s) = status {
            self.conn.execute(
                "UPDATE followups SET status = ? WHERE id = ?",
                params![s, id],
            )?;
        }
        Ok(())
    }

    /// Delete a followup by ID
    pub fn delete_followup(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM followups WHERE id = ?", [id])?;
        Ok(())
    }
}
