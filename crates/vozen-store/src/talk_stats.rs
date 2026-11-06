//! Durable message-speech counters and Duolingo-style streaks.
//!
//! This is deliberately a storage policy: callers supply the operator-local calendar day so the
//! Rust runtime can match the existing Node process regardless of where the bot is hosted.

use rusqlite::{OptionalExtension, params};
use time::{Date, Duration, format_description::well_known::Iso8601};

use crate::{SqliteStore, StoreError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TalkBump {
    pub first_of_day: bool,
    pub streak: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TalkRow {
    pub user_id: String,
    pub count: i64,
    pub streak: i64,
    pub best_streak: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuildTalkStreak {
    pub streak: i64,
    pub best_streak: i64,
}

#[derive(Debug)]
struct StoredTalkRow {
    spoken_count: i64,
    streak: i64,
    best_streak: i64,
    last_date: String,
}

#[derive(Debug)]
struct StoredGuildStreak {
    streak: i64,
    best_streak: i64,
    last_date: String,
}

impl SqliteStore {
    /// Records an accepted auto-read request for an operator-local calendar day.
    ///
    /// A one-day miss freezes a streak; two consecutive misses reset it. This exactly mirrors the
    /// existing Node semantics and never receives message content.
    pub fn bump_talk(
        &self,
        guild_id: &str,
        user_id: &str,
        local_day: &str,
    ) -> Result<TalkBump, StoreError> {
        let day = parse_day(local_day)?;
        let row = self
            .connection()
            .query_row(
                "SELECT spoken_count, streak, best_streak, last_date
                 FROM talk_stats WHERE guild_id = ?1 AND user_id = ?2",
                params![guild_id, user_id],
                |row| {
                    Ok(StoredTalkRow {
                        spoken_count: row.get(0)?,
                        streak: row.get(1)?,
                        best_streak: row.get(2)?,
                        last_date: row.get(3)?,
                    })
                },
            )
            .optional()?;

        let Some(row) = row else {
            self.connection().execute(
                "INSERT INTO talk_stats (guild_id, user_id, spoken_count, streak, best_streak, last_date)
                 VALUES (?1, ?2, 1, 1, 1, ?3)",
                params![guild_id, user_id, local_day],
            )?;
            return Ok(TalkBump {
                first_of_day: true,
                streak: 1,
            });
        };

        let first_of_day = row.last_date != local_day;
        let streak = next_streak(&row.last_date, row.streak, day);
        let best_streak = row.best_streak.max(streak);
        self.connection().execute(
            "UPDATE talk_stats
             SET spoken_count = ?1, streak = ?2, best_streak = ?3, last_date = ?4
             WHERE guild_id = ?5 AND user_id = ?6",
            params![
                row.spoken_count + 1,
                streak,
                best_streak,
                local_day,
                guild_id,
                user_id
            ],
        )?;
        Ok(TalkBump {
            first_of_day,
            streak,
        })
    }

    /// Records that at least one accepted request occurred in this guild on `local_day`.
    pub fn bump_guild_talk(&self, guild_id: &str, local_day: &str) -> Result<i64, StoreError> {
        let day = parse_day(local_day)?;
        let row = self
            .connection()
            .query_row(
                "SELECT streak, best_streak, last_date FROM guild_talk_streak WHERE guild_id = ?1",
                [guild_id],
                |row| {
                    Ok(StoredGuildStreak {
                        streak: row.get(0)?,
                        best_streak: row.get(1)?,
                        last_date: row.get(2)?,
                    })
                },
            )
            .optional()?;

        let Some(row) = row else {
            let seed = self.seed_guild_streak(guild_id, day)?;
            self.connection().execute(
                "INSERT INTO guild_talk_streak (guild_id, streak, best_streak, last_date)
                 VALUES (?1, ?2, ?2, ?3)",
                params![guild_id, seed, local_day],
            )?;
            return Ok(seed);
        };

        let streak = next_streak(&row.last_date, row.streak, day);
        self.connection().execute(
            "UPDATE guild_talk_streak SET streak = ?1, best_streak = ?2, last_date = ?3
             WHERE guild_id = ?4",
            params![streak, row.best_streak.max(streak), local_day, guild_id],
        )?;
        Ok(streak)
    }

    /// Returns the live streak and historical high-water mark for a guild.
    pub fn guild_talk_streak(
        &self,
        guild_id: &str,
        local_day: &str,
    ) -> Result<GuildTalkStreak, StoreError> {
        let day = parse_day(local_day)?;
        let result = self
            .connection()
            .query_row(
                "SELECT streak, best_streak, last_date FROM guild_talk_streak WHERE guild_id = ?1",
                [guild_id],
                |row| {
                    let streak: i64 = row.get(0)?;
                    let best_streak: i64 = row.get(1)?;
                    let last_date: String = row.get(2)?;
                    Ok(GuildTalkStreak {
                        streak: effective_streak(&last_date, streak, day),
                        best_streak,
                    })
                },
            )
            .optional()?
            .unwrap_or(GuildTalkStreak {
                streak: 0,
                best_streak: 0,
            });
        Ok(result)
    }

    /// Lists the highest accepted message counts for a guild, with the live streak as a tiebreak.
    pub fn top_speakers(
        &self,
        guild_id: &str,
        local_day: &str,
        limit: usize,
    ) -> Result<Vec<TalkRow>, StoreError> {
        let mut rows = self.talk_rows(guild_id, local_day)?;
        rows.sort_unstable_by(by_message_count);
        rows.truncate(limit);
        Ok(rows)
    }

    /// Returns the union of the top entries for messages and current streaks in one guild.
    pub fn talk_leaderboard(
        &self,
        guild_id: &str,
        local_day: &str,
        limit: usize,
    ) -> Result<Vec<TalkRow>, StoreError> {
        let mut rows = self.talk_rows(guild_id, local_day)?;
        rows.retain(|row| row.count > 0);
        rows.sort_unstable_by(by_message_count);
        let mut entries: Vec<_> = rows.iter().take(limit).cloned().collect();
        rows.sort_unstable_by(|left, right| {
            right
                .streak
                .cmp(&left.streak)
                .then_with(|| by_message_count(left, right))
        });
        for row in rows
            .into_iter()
            .take_while(|row| row.streak > 0)
            .take(limit)
        {
            if !entries.iter().any(|entry| entry.user_id == row.user_id) {
                entries.push(row);
            }
        }
        Ok(entries)
    }

    fn talk_rows(&self, guild_id: &str, local_day: &str) -> Result<Vec<TalkRow>, StoreError> {
        let day = parse_day(local_day)?;
        let mut statement = self.connection().prepare(
            "SELECT user_id, spoken_count, streak, best_streak, last_date
             FROM talk_stats WHERE guild_id = ?1",
        )?;
        let rows = statement
            .query_map([guild_id], |row| {
                let last_date: String = row.get(4)?;
                let stored_streak: i64 = row.get(2)?;
                Ok(TalkRow {
                    user_id: row.get(0)?,
                    count: row.get(1)?,
                    streak: effective_streak(&last_date, stored_streak, day),
                    best_streak: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn seed_guild_streak(&self, guild_id: &str, day: Date) -> Result<i64, StoreError> {
        let mut statement = self
            .connection()
            .prepare("SELECT streak, last_date FROM talk_stats WHERE guild_id = ?1")?;
        let rows = statement
            .query_map([guild_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .iter()
            .map(|(streak, last_date)| effective_streak(last_date, *streak, day))
            .max()
            .unwrap_or(1)
            .max(1))
    }
}

fn by_message_count(left: &TalkRow, right: &TalkRow) -> std::cmp::Ordering {
    right
        .count
        .cmp(&left.count)
        .then_with(|| right.streak.cmp(&left.streak))
        .then_with(|| left.user_id.cmp(&right.user_id))
}

fn parse_day(day: &str) -> Result<Date, StoreError> {
    Date::parse(day, &Iso8601::DATE).map_err(|_| StoreError::InvalidTalkStatsDay)
}

fn next_streak(last_date: &str, stored_streak: i64, today: Date) -> i64 {
    let yesterday = today - Duration::days(1);
    let two_days_ago = today - Duration::days(2);
    if last_date == day_key(today) {
        stored_streak
    } else if last_date == day_key(yesterday) || last_date == day_key(two_days_ago) {
        stored_streak + 1
    } else {
        1
    }
}

fn effective_streak(last_date: &str, stored_streak: i64, today: Date) -> i64 {
    let yesterday = today - Duration::days(1);
    let two_days_ago = today - Duration::days(2);
    if last_date == day_key(today)
        || last_date == day_key(yesterday)
        || last_date == day_key(two_days_ago)
    {
        stored_streak
    } else {
        0
    }
}

fn day_key(day: Date) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        day.year(),
        u8::from(day.month()),
        day.day()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_streak_uses_the_same_one_day_freeze_as_node() {
        let store = SqliteStore::open_in_memory().expect("store");
        assert_eq!(
            store
                .bump_talk("guild", "user", "2026-07-20")
                .expect("first"),
            TalkBump {
                first_of_day: true,
                streak: 1
            }
        );
        assert_eq!(
            store
                .bump_talk("guild", "user", "2026-07-20")
                .expect("same day"),
            TalkBump {
                first_of_day: false,
                streak: 1
            }
        );
        assert_eq!(
            store
                .bump_talk("guild", "user", "2026-07-22")
                .expect("freeze"),
            TalkBump {
                first_of_day: true,
                streak: 2
            }
        );
        assert_eq!(
            store
                .bump_talk("guild", "user", "2026-07-25")
                .expect("reset"),
            TalkBump {
                first_of_day: true,
                streak: 1
            }
        );
    }

    #[test]
    fn guild_seed_and_live_streak_do_not_inflate_activity() {
        let store = SqliteStore::open_in_memory().expect("store");
        store
            .bump_talk("guild", "user", "2026-07-20")
            .expect("user");
        assert_eq!(
            store.bump_guild_talk("guild", "2026-07-20").expect("first"),
            1
        );
        assert_eq!(
            store.bump_guild_talk("guild", "2026-07-20").expect("same"),
            1
        );
        assert_eq!(
            store
                .bump_guild_talk("guild", "2026-07-22")
                .expect("freeze"),
            2
        );
        assert_eq!(
            store
                .guild_talk_streak("guild", "2026-07-25")
                .expect("live"),
            GuildTalkStreak {
                streak: 0,
                best_streak: 2
            }
        );
    }

    #[test]
    fn leaderboard_uses_counts_then_live_streak_and_never_accepts_invalid_days() {
        let store = SqliteStore::open_in_memory().expect("store");
        store
            .bump_talk("guild", "anna", "2026-07-20")
            .expect("anna");
        store
            .bump_talk("guild", "anna", "2026-07-20")
            .expect("anna again");
        store
            .bump_talk("guild", "bruno", "2026-07-22")
            .expect("bruno");
        assert_eq!(
            store.top_speakers("guild", "2026-07-22", 10).expect("top"),
            vec![
                TalkRow {
                    user_id: "anna".into(),
                    count: 2,
                    streak: 1,
                    best_streak: 1
                },
                TalkRow {
                    user_id: "bruno".into(),
                    count: 1,
                    streak: 1,
                    best_streak: 1
                },
            ]
        );
        assert!(matches!(
            store.bump_talk("guild", "user", "not-a-day"),
            Err(StoreError::InvalidTalkStatsDay)
        ));
    }

    #[test]
    fn leaderboard_combines_distinct_winners_and_keeps_expired_streaks_at_zero() {
        let store = SqliteStore::open_in_memory().unwrap();
        for _ in 0..6 {
            store
                .bump_talk("guild", "most-messages", "2026-07-15")
                .unwrap();
            store.bump_talk("other", "outside", "2026-07-22").unwrap();
        }
        for day in ["2026-07-20", "2026-07-21", "2026-07-22"] {
            store.bump_talk("guild", "longest-streak", day).unwrap();
        }
        store.bump_talk("guild", "one-day", "2026-07-22").unwrap();
        let rows = store.talk_leaderboard("guild", "2026-07-22", 1).unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| (row.user_id.as_str(), row.count, row.streak))
                .collect::<Vec<_>>(),
            [("most-messages", 6, 0), ("longest-streak", 3, 3)]
        );
        let rows = store.talk_leaderboard("guild", "2026-07-22", 2).unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| row.user_id.as_str())
                .collect::<Vec<_>>(),
            ["most-messages", "longest-streak", "one-day"]
        );
    }
}
