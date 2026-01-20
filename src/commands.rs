use crate::error::Result;
use crate::followups::FollowupsDb;
use chrono::Utc;
use regex::Regex;
use tracing::info;

#[derive(Debug, Clone)]
pub enum UserCommand {
    DisableReminders,
    EnableReminders,
    Snooze { duration_mins: i64 },
    Dismiss,
    SetQuietHours { start: i32, end: i32 },
    ListReminders,
}

pub struct CommandParser;

impl CommandParser {
    /// Attempts to parse a user message as a control command.
    /// Returns None if not a command.
    pub fn parse(text: &str) -> Option<UserCommand> {
        let text = text.trim().to_lowercase();

        // Disable reminders
        if text == "disable reminders"
            || text == "turn off reminders"
            || text == "stop reminders"
            || text == "no more reminders"
        {
            return Some(UserCommand::DisableReminders);
        }

        // Enable reminders
        if text == "enable reminders"
            || text == "turn on reminders"
            || text == "start reminders"
        {
            return Some(UserCommand::EnableReminders);
        }

        // Dismiss
        if text == "dismiss" || text == "dismiss reminder" || text == "cancel reminder" {
            return Some(UserCommand::Dismiss);
        }

        // List reminders
        if text == "list reminders"
            || text == "show reminders"
            || text == "my reminders"
            || text == "pending reminders"
        {
            return Some(UserCommand::ListReminders);
        }

        // Parse "snooze 1h", "snooze 30m", "snooze 1d"
        if let Some(duration) = Self::parse_snooze(&text) {
            return Some(UserCommand::Snooze {
                duration_mins: duration,
            });
        }

        // Parse "quiet hours 10pm-8am"
        if let Some((start, end)) = Self::parse_quiet_hours(&text) {
            return Some(UserCommand::SetQuietHours { start, end });
        }

        None
    }

    fn parse_snooze(text: &str) -> Option<i64> {
        let re = Regex::new(r"^snooze\s+(\d+)\s*(h|hr|hour|m|min|minute|d|day)s?$").ok()?;
        let caps = re.captures(text)?;

        let amount: i64 = caps.get(1)?.as_str().parse().ok()?;
        let unit = caps.get(2)?.as_str();

        match unit {
            "h" | "hr" | "hour" => Some(amount * 60),
            "m" | "min" | "minute" => Some(amount),
            "d" | "day" => Some(amount * 24 * 60),
            _ => None,
        }
    }

    fn parse_quiet_hours(text: &str) -> Option<(i32, i32)> {
        // Match "quiet hours 10pm-8am" or "quiet hours 22:00-08:00"
        let re = Regex::new(r"^quiet\s+hours?\s+(\d{1,2})(am|pm|:\d{2})\s*-\s*(\d{1,2})(am|pm|:\d{2})$").ok()?;
        let caps = re.captures(text)?;

        let start_hour: i32 = caps.get(1)?.as_str().parse().ok()?;
        let start_suffix = caps.get(2)?.as_str();
        let end_hour: i32 = caps.get(3)?.as_str().parse().ok()?;
        let end_suffix = caps.get(4)?.as_str();

        let start = Self::parse_hour(start_hour, start_suffix)?;
        let end = Self::parse_hour(end_hour, end_suffix)?;

        Some((start, end))
    }

    fn parse_hour(hour: i32, suffix: &str) -> Option<i32> {
        if suffix == "am" {
            if hour == 12 {
                Some(0) // 12am = 0:00
            } else {
                Some(hour * 100)
            }
        } else if suffix == "pm" {
            if hour == 12 {
                Some(1200) // 12pm = 12:00
            } else {
                Some((hour + 12) * 100)
            }
        } else if suffix.starts_with(':') {
            // 24-hour format like ":00"
            let minutes: i32 = suffix[1..].parse().ok()?;
            Some(hour * 100 + minutes)
        } else {
            None
        }
    }
}

pub struct CommandHandler<'a> {
    db: &'a FollowupsDb,
    default_snooze_mins: u64,
}

impl<'a> CommandHandler<'a> {
    pub fn new(db: &'a FollowupsDb, default_snooze_mins: u64) -> Self {
        CommandHandler {
            db,
            default_snooze_mins,
        }
    }

    pub fn handle(&self, command: UserCommand, chat_guid: &str, sender: &str) -> Result<String> {
        match command {
            UserCommand::DisableReminders => {
                self.db.set_notifications_enabled(chat_guid, sender, false)?;
                info!("Disabled reminders for chat {}", chat_guid);
                Ok("Reminders disabled for this chat. Send \"enable reminders\" to turn them back on.".to_string())
            }

            UserCommand::EnableReminders => {
                self.db.set_notifications_enabled(chat_guid, sender, true)?;
                info!("Enabled reminders for chat {}", chat_guid);
                Ok("Reminders enabled for this chat.".to_string())
            }

            UserCommand::Snooze { duration_mins } => {
                // Get most recent pending followup for this chat
                if let Some(followup) = self.db.get_latest_pending_for_chat(chat_guid)? {
                    let new_notify_at =
                        Utc::now().timestamp_millis() + (duration_mins * 60 * 1000);
                    self.db.snooze(followup.id.unwrap(), new_notify_at)?;

                    let hours = duration_mins / 60;
                    let mins = duration_mins % 60;
                    let time_str = if hours > 0 && mins > 0 {
                        format!("{} hour(s) and {} minute(s)", hours, mins)
                    } else if hours > 0 {
                        format!("{} hour(s)", hours)
                    } else {
                        format!("{} minute(s)", mins)
                    };

                    info!(
                        "Snoozed followup {} for {} minutes",
                        followup.id.unwrap(),
                        duration_mins
                    );
                    Ok(format!(
                        "Snoozed reminder \"{}\" for {}.",
                        followup.description, time_str
                    ))
                } else {
                    Ok("No pending reminders to snooze.".to_string())
                }
            }

            UserCommand::Dismiss => {
                if let Some(followup) = self.db.get_latest_pending_for_chat(chat_guid)? {
                    self.db.mark_dismissed(followup.id.unwrap())?;
                    info!("Dismissed followup {}", followup.id.unwrap());
                    Ok(format!("Dismissed reminder: \"{}\"", followup.description))
                } else {
                    Ok("No pending reminders to dismiss.".to_string())
                }
            }

            UserCommand::SetQuietHours { start, end } => {
                self.db.set_quiet_hours(chat_guid, sender, start, end)?;

                let format_time = |t: i32| -> String {
                    let hour = t / 100;
                    let minute = t % 100;
                    if hour == 0 {
                        format!("12:{:02}am", minute)
                    } else if hour < 12 {
                        format!("{}:{:02}am", hour, minute)
                    } else if hour == 12 {
                        format!("12:{:02}pm", minute)
                    } else {
                        format!("{}:{:02}pm", hour - 12, minute)
                    }
                };

                info!(
                    "Set quiet hours for chat {}: {}-{}",
                    chat_guid, start, end
                );
                Ok(format!(
                    "Quiet hours set from {} to {}. No reminders will be sent during this time.",
                    format_time(start),
                    format_time(end)
                ))
            }

            UserCommand::ListReminders => {
                let pending = self.db.get_pending_for_chat(chat_guid)?;

                if pending.is_empty() {
                    return Ok("You have no pending reminders.".to_string());
                }

                let mut response = format!("You have {} pending reminder(s):\n", pending.len());

                for (i, f) in pending.iter().enumerate() {
                    let time_str = if let Some(notify_at) = f.notify_at {
                        let dt = chrono::DateTime::from_timestamp_millis(notify_at)
                            .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                            .unwrap_or_else(|| "unknown".to_string());
                        format!(" (scheduled: {})", dt)
                    } else {
                        String::new()
                    };

                    let recurring_str = if f.notify_interval_mins.is_some() {
                        " [recurring]"
                    } else {
                        ""
                    };

                    response.push_str(&format!(
                        "\n{}. \"{}\"{}{}\n",
                        i + 1,
                        f.description,
                        time_str,
                        recurring_str
                    ));
                }

                Ok(response)
            }
        }
    }
}
