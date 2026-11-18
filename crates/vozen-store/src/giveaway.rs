use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use crate::{
    SqliteStore, StoreError,
    premium::{grant_guild_pass_on, grant_user_premium_on},
};

const HOUR: i64 = 3_600_000;
const FIELDS: &str =
    "id,guild_id,channel_id,message_id,title,description,plan,days,seats,winner_count,
    starts_at,ends_at,claim_hours,status,publish_attempt_at,revision,synced,error,cancel_reason";
const PAID: &str =
    "SELECT 1 FROM stripe_paid_entitlement p WHERE p.user_id=a.user_id AND p.expires_at>?1
    AND (p.plan=g.plan OR (g.plan='premium' AND p.plan='max'))";

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum GiveawayAction {
    Save {
        id: Option<i64>,
        request_id: String,
        #[serde(flatten)]
        draft: GiveawayDraft,
    },
    Schedule {
        id: i64,
    },
    Cancel {
        id: i64,
        reason: String,
    },
    Replace {
        id: i64,
        award_id: i64,
    },
}

#[derive(Debug, Deserialize)]
pub struct GiveawayDraft {
    channel_id: String,
    title: String,
    description: String,
    plan: String,
    days: i64,
    seats: i64,
    winner_count: i64,
    starts_at: i64,
    ends_at: i64,
    claim_hours: i64,
}

impl GiveawayDraft {
    fn valid(&self, now: i64) -> bool {
        self.channel_id.parse::<u64>().is_ok_and(|id| id > 0)
            && self.channel_id.len() <= 20
            && self.channel_id.bytes().all(|c| c.is_ascii_digit())
            && self.title.encode_utf16().count() <= 100
            && self.description.encode_utf16().count() <= 500
            && matches!(
                (self.plan.as_str(), self.seats),
                ("plus", 0) | ("premium", 3 | 8)
            )
            && matches!(self.days, 30 | 365)
            && (1..=10).contains(&self.winner_count)
            && self.starts_at >= 0
            && self.ends_at > self.starts_at.max(now)
            && self.ends_at <= now + 90 * 24 * HOUR
            && (1..=168).contains(&self.claim_hours)
    }
}

#[derive(Debug, Serialize)]
pub struct Giveaway {
    pub id: i64,
    pub guild_id: String,
    pub channel_id: String,
    pub message_id: Option<String>,
    pub title: String,
    pub description: String,
    pub plan: String,
    pub days: i64,
    pub seats: i64,
    pub winner_count: i64,
    pub starts_at: i64,
    pub ends_at: i64,
    pub claim_hours: i64,
    pub status: String,
    pub publish_attempt_at: Option<i64>,
    pub revision: i64,
    pub synced: i64,
    pub error: Option<String>,
    pub cancel_reason: Option<String>,
    pub entries: i64,
    pub awards: Vec<GiveawayAward>,
}

#[derive(Debug, Serialize)]
pub struct GiveawayAward {
    pub id: i64,
    pub user_id: String,
    pub deadline: i64,
    pub accepted_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub replaced: bool,
    pub replace_requested: bool,
    pub delivery_error: bool,
    pub entries: i64,
    pub dm_channel: Option<String>,
    pub dm_message: Option<String>,
    pub dm_attempt: Option<i64>,
    pub dm_blocked: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum GiveawayClick {
    VoteRequired,
    Left,
    Closed,
    NotWinner,
    Expired,
    Waiting,
    Delivered(i64),
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<Giveaway> {
    Ok(Giveaway {
        id: row.get(0)?,
        guild_id: row.get(1)?,
        channel_id: row.get(2)?,
        message_id: row.get(3)?,
        title: row.get(4)?,
        description: row.get(5)?,
        plan: row.get(6)?,
        days: row.get(7)?,
        seats: row.get(8)?,
        winner_count: row.get(9)?,
        starts_at: row.get(10)?,
        ends_at: row.get(11)?,
        claim_hours: row.get(12)?,
        status: row.get(13)?,
        publish_attempt_at: row.get(14)?,
        revision: row.get(15)?,
        synced: row.get(16)?,
        error: row.get(17)?,
        cancel_reason: row.get(18)?,
        entries: 0,
        awards: Vec::new(),
    })
}

fn details(db: &Connection, g: &mut Giveaway) -> Result<(), StoreError> {
    g.entries = db.query_row(
        "SELECT (SELECT COUNT(*) FROM giveaway_entry WHERE giveaway_id=?1) + (SELECT COUNT(*) FROM giveaway_vote WHERE giveaway_id=?1)",
        [g.id],
        |r| r.get(0),
    )?;
    let mut stmt = db.prepare("SELECT a.id,a.user_id,a.deadline,a.accepted_at,a.expires_at,a.replaced,a.replace_requested,a.delivery_error,
        (SELECT COUNT(*) FROM giveaway_entry e WHERE e.giveaway_id=a.giveaway_id AND e.user_id=a.user_id)
        + (SELECT COUNT(*) FROM giveaway_vote v WHERE v.giveaway_id=a.giveaway_id AND v.user_id=a.user_id),
        d.channel_id,d.message_id,d.last_attempt,COALESCE(d.blocked,0)
        FROM giveaway_award a LEFT JOIN giveaway_dm d ON d.award_id=a.id WHERE a.giveaway_id=?1 ORDER BY a.id")?;
    g.awards = stmt
        .query_map([g.id], |r| {
            Ok(GiveawayAward {
                id: r.get(0)?,
                user_id: r.get(1)?,
                deadline: r.get(2)?,
                accepted_at: r.get(3)?,
                expires_at: r.get(4)?,
                replaced: r.get(5)?,
                replace_requested: r.get(6)?,
                delivery_error: r.get(7)?,
                entries: r.get(8)?,
                dm_channel: r.get(9)?,
                dm_message: r.get(10)?,
                dm_attempt: r.get(11)?,
                dm_blocked: r.get(12)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(())
}

impl SqliteStore {
    pub fn giveaways(&self, guild: &str) -> Result<Vec<Giveaway>, StoreError> {
        let db = self.connection();
        let mut stmt = db.prepare(&format!(
            "SELECT {FIELDS} FROM giveaway WHERE guild_id=?1
            ORDER BY status IN ('draft','scheduled','open') DESC,id DESC LIMIT 100"
        ))?;
        let mut rows = stmt
            .query_map([guild], from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        for giveaway in &mut rows {
            details(db, giveaway)?;
        }
        Ok(rows)
    }

    pub fn giveaway_admin(
        &self,
        guild: &str,
        action: GiveawayAction,
        now: i64,
    ) -> Result<Option<i64>, StoreError> {
        let db = self.connection();
        let id = match action {
            GiveawayAction::Save {
                id,
                request_id,
                draft: d,
            } if d.valid(now) && uuid::Uuid::parse_str(&request_id).is_ok() => {
                if let Some(id) = id {
                    let changed = db.execute(
                        "UPDATE giveaway SET channel_id=?1,title=?2,description=?3,plan=?4,days=?5,
                        seats=?6,winner_count=?7,starts_at=?8,ends_at=?9,claim_hours=?10
                        WHERE id=?11 AND guild_id=?12 AND status='draft'",
                        params![
                            d.channel_id,
                            d.title.trim(),
                            d.description.trim(),
                            d.plan,
                            d.days,
                            d.seats,
                            d.winner_count,
                            d.starts_at,
                            d.ends_at,
                            d.claim_hours,
                            id,
                            guild
                        ],
                    )?;
                    return Ok((changed == 1).then_some(id));
                }
                db.execute("INSERT INTO giveaway (request_id,guild_id,channel_id,title,description,plan,days,seats,
                    winner_count,starts_at,ends_at,claim_hours) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
                    ON CONFLICT(request_id) DO NOTHING",
                    params![request_id,guild,d.channel_id,d.title.trim(),d.description.trim(),d.plan,d.days,
                        d.seats,d.winner_count,d.starts_at,d.ends_at,d.claim_hours])?;
                return Ok(db
                    .query_row(
                        "SELECT id FROM giveaway WHERE request_id=?1 AND guild_id=?2",
                        params![request_id, guild],
                        |r| r.get(0),
                    )
                    .optional()?);
            }
            GiveawayAction::Schedule { id } => {
                let changed = db.execute(
                    "UPDATE giveaway SET status='scheduled',starts_at=MAX(starts_at,?3)
                    WHERE id=?1 AND guild_id=?2 AND status='draft' AND ends_at>MAX(starts_at,?3)",
                    params![id, guild, now],
                )?;
                (changed == 1).then_some(id)
            }
            GiveawayAction::Cancel { id, reason }
                if !reason.trim().is_empty() && reason.encode_utf16().count() <= 180 =>
            {
                let changed = db.execute("UPDATE giveaway SET status='cancelled',cancel_reason=?3,revision=revision+1,error=NULL
                    WHERE id=?1 AND guild_id=?2 AND status IN ('draft','scheduled','open')", params![id,guild,reason.trim()])?;
                (changed == 1).then_some(id)
            }
            GiveawayAction::Replace { id, award_id } => {
                let tx = db.unchecked_transaction()?;
                let changed = tx.execute("UPDATE giveaway_award SET replace_requested=1
                    WHERE id=?1 AND giveaway_id IN (SELECT id FROM giveaway WHERE id=?2 AND guild_id=?3 AND status='closed')
                    AND deadline<=?4 AND accepted_at IS NULL AND replaced=0", params![award_id,id,guild,now])?;
                if changed == 1 {
                    tx.execute("UPDATE giveaway SET error=NULL WHERE id=?1", [id])?;
                }
                tx.commit()?;
                (changed == 1).then_some(id)
            }
            _ => None,
        };
        Ok(id)
    }

    pub fn giveaway_jobs(&self, guild: &str, now: i64) -> Result<Vec<Giveaway>, StoreError> {
        let db = self.connection();
        let mut stmt = db.prepare(&format!("SELECT {FIELDS} FROM giveaway g WHERE guild_id=?2 AND (
            (status='scheduled' AND starts_at<=?1 AND error IS NULL) OR (status='open' AND ends_at<=?1)
            OR (message_id IS NOT NULL AND synced<revision)
            OR EXISTS (SELECT 1 FROM giveaway_award a LEFT JOIN giveaway_dm d ON d.award_id=a.id
                WHERE a.giveaway_id=g.id AND a.replaced=0 AND a.accepted_at IS NULL AND a.deadline>?1
                AND d.message_id IS NULL AND COALESCE(d.blocked,0)=0 AND COALESCE(d.last_attempt,0)<=?1-60000)
            OR EXISTS(SELECT 1 FROM giveaway_award a WHERE a.giveaway_id=g.id AND (a.replace_requested=1
              OR (a.accepted_at IS NOT NULL AND a.expires_at IS NULL AND NOT EXISTS ({PAID}))))
            )
            ORDER BY id LIMIT 100"))?;
        let mut jobs = stmt
            .query_map(params![now, guild], from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        for giveaway in &mut jobs {
            details(db, giveaway)?;
        }
        Ok(jobs)
    }

    pub fn giveaway_publish_attempt(&self, id: i64, now: i64) -> Result<bool, StoreError> {
        Ok(self.connection().execute(
            "UPDATE giveaway SET publish_attempt_at=COALESCE(publish_attempt_at,?2)
            WHERE id=?1 AND status='scheduled' AND ends_at>?2
            AND (publish_attempt_at IS NULL OR publish_attempt_at>?2-60000)",
            params![id, now],
        )? == 1)
    }

    pub fn giveaway_published(&self, id: i64, message: &str) -> Result<(), StoreError> {
        self.connection().execute(
            "UPDATE giveaway SET message_id=?2,error=NULL,
            status=CASE WHEN status='scheduled' THEN 'open' ELSE status END
            WHERE id=?1 AND message_id IS NULL",
            params![id, message],
        )?;
        Ok(())
    }

    pub fn giveaway_sync(&self, id: i64, revision: i64) -> Result<(), StoreError> {
        self.connection().execute(
            "UPDATE giveaway SET synced=MAX(synced,?2) WHERE id=?1",
            params![id, revision],
        )?;
        Ok(())
    }

    pub fn giveaway_error(&self, id: i64, error: &str) -> Result<(), StoreError> {
        self.connection().execute(
            "UPDATE giveaway SET error=?2 WHERE id=?1",
            params![id, error],
        )?;
        Ok(())
    }

    pub fn giveaway_candidates(&self, id: i64) -> Result<Vec<String>, StoreError> {
        // Shuffle tickets, then the runtime skips users already selected for this draw.
        let mut stmt = self.connection().prepare("SELECT user_id FROM (
            SELECT user_id,giveaway_id FROM giveaway_entry
            UNION ALL SELECT user_id,giveaway_id FROM giveaway_vote
        ) e WHERE giveaway_id=?1
        AND NOT EXISTS(SELECT 1 FROM giveaway_award a WHERE a.giveaway_id=e.giveaway_id AND a.user_id=e.user_id)
        ORDER BY random()")?;
        Ok(stmt
            .query_map([id], |r| r.get(0))?
            .collect::<Result<_, _>>()?)
    }

    pub fn giveaway_draw(
        &self,
        id: i64,
        replace: Option<i64>,
        candidates: &[String],
        now: i64,
    ) -> Result<(), StoreError> {
        let tx = self.connection().unchecked_transaction()?;
        let mut eligible = Vec::with_capacity(candidates.len());
        for user in candidates {
            let exists: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM giveaway_entry WHERE giveaway_id=?1 AND user_id=?2
                    UNION ALL SELECT 1 FROM giveaway_vote WHERE giveaway_id=?1 AND user_id=?2)",
                params![id, user],
                |r| r.get(0),
            )?;
            if exists {
                eligible.push(user);
            }
        }
        let candidates = eligible;
        let count = if let Some(award) = replace {
            tx.execute(
                "UPDATE giveaway_award SET replace_requested=0,replaced=?3
                WHERE id=?1 AND giveaway_id=?2 AND replace_requested=1 AND accepted_at IS NULL",
                params![award, id, !candidates.is_empty()],
            )?
        } else {
            let changed = tx.execute(
                "UPDATE giveaway SET status='closed' WHERE id=?1 AND status='open' AND ends_at<=?2",
                params![id, now],
            )?;
            if changed == 0 {
                return Ok(());
            }
            candidates.len()
        };
        for user in candidates.iter().take(count) {
            tx.execute(
                "INSERT INTO giveaway_award (giveaway_id,user_id,deadline)
                SELECT g.id,?2,?3+g.claim_hours*3600000 FROM giveaway g
                WHERE g.id=?1",
                params![id, user, now],
            )?;
        }
        let error = (replace.is_some() && candidates.is_empty())
            .then_some("There are no eligible participants to replace this winner.");
        tx.execute(
            "UPDATE giveaway SET revision=revision+1,error=?2 WHERE id=?1",
            params![id, error],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn giveaway_click(
        &self,
        guild: &str,
        message: &str,
        id: i64,
        user: &str,
        action: &str,
        now: i64,
    ) -> Result<GiveawayClick, StoreError> {
        let db = self.connection();
        let context = db
            .query_row(
                "SELECT status,ends_at FROM giveaway WHERE id=?1 AND guild_id=?2 AND message_id=?3",
                params![id, guild, message],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?;
        let Some((status, ends_at)) = context else {
            return Ok(GiveawayClick::Closed);
        };
        if action == "claim" && status == "closed" {
            let award = db
                .query_row(
                    "SELECT id,deadline,accepted_at,expires_at FROM giveaway_award
                WHERE giveaway_id=?1 AND user_id=?2 AND replaced=0",
                    params![id, user],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, Option<i64>>(2)?,
                            r.get::<_, Option<i64>>(3)?,
                        ))
                    },
                )
                .optional()?;
            let Some((award, deadline, accepted, delivered)) = award else {
                return Ok(GiveawayClick::NotWinner);
            };
            if let Some(expiry) = delivered {
                return Ok(GiveawayClick::Delivered(expiry));
            }
            if accepted.is_none() && deadline <= now {
                return Ok(GiveawayClick::Expired);
            }
            db.execute(
                "UPDATE giveaway_award SET accepted_at=COALESCE(accepted_at,?2) WHERE id=?1",
                params![award, now],
            )?;
            return self.giveaway_deliver(award, now);
        }
        if status != "open" || ends_at <= now {
            return Ok(GiveawayClick::Closed);
        }
        match action {
            "join" => Ok(GiveawayClick::VoteRequired),
            "leave" => {
                db.execute(
                    "DELETE FROM giveaway_vote WHERE giveaway_id=?1 AND user_id=?2",
                    params![id, user],
                )?;
                db.execute(
                    "DELETE FROM giveaway_entry WHERE giveaway_id=?1 AND user_id=?2",
                    params![id, user],
                )?;
                Ok(GiveawayClick::Left)
            }
            _ => Ok(GiveawayClick::Closed),
        }
    }

    pub fn giveaway_dm_attempt(&self, award: i64, now: i64) -> Result<(), StoreError> {
        self.connection().execute(
            "INSERT INTO giveaway_dm (award_id,last_attempt) VALUES (?1,?2)
             ON CONFLICT(award_id) DO UPDATE SET last_attempt=excluded.last_attempt",
            params![award, now],
        )?;
        Ok(())
    }

    pub fn giveaway_dm_sent(
        &self,
        award: i64,
        channel: &str,
        message: &str,
    ) -> Result<(), StoreError> {
        self.connection().execute(
            "UPDATE giveaway_dm SET channel_id=?2,message_id=?3 WHERE award_id=?1",
            params![award, channel, message],
        )?;
        Ok(())
    }

    pub fn giveaway_dm_blocked(&self, id: i64, award: i64) -> Result<(), StoreError> {
        let tx = self.connection().unchecked_transaction()?;
        tx.execute(
            "UPDATE giveaway_dm SET blocked=1 WHERE award_id=?1",
            [award],
        )?;
        tx.execute("UPDATE giveaway SET revision=revision+1 WHERE id=?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn giveaway_dm_original(
        &self,
        id: i64,
        user: &str,
        message: &str,
    ) -> Result<Option<String>, StoreError> {
        Ok(self.connection().query_row(
            "SELECT g.message_id FROM giveaway g JOIN giveaway_award a ON a.giveaway_id=g.id
             JOIN giveaway_dm d ON d.award_id=a.id WHERE g.id=?1 AND a.user_id=?2 AND d.message_id=?3",
            params![id,user,message],|r|r.get(0),
        ).optional()?)
    }

    pub fn giveaway_deliver(&self, award: i64, now: i64) -> Result<GiveawayClick, StoreError> {
        let db = self.connection();
        let result = (|| -> Result<GiveawayClick, StoreError> {
            let tx = db.unchecked_transaction()?;
            let pending = tx
                .query_row(
                    &format!(
                        "SELECT a.user_id,g.plan,g.days,g.seats FROM giveaway_award a
                JOIN giveaway g ON g.id=a.giveaway_id WHERE a.id=?2 AND a.accepted_at IS NOT NULL
                AND a.replaced=0 AND a.expires_at IS NULL AND NOT EXISTS ({PAID})"
                    ),
                    params![now, award],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, i64>(3)?,
                        ))
                    },
                )
                .optional()?;
            let Some((user, plan, days, seats)) = pending else {
                return Ok(GiveawayClick::Waiting);
            };
            let expires = if plan == "plus" {
                grant_user_premium_on(&tx, &user, days, "giveaway", now)?
            } else {
                grant_guild_pass_on(&tx, &user, seats, days, "giveaway", now)?
            };
            tx.execute(
                "UPDATE giveaway_award SET expires_at=?2,delivery_error=0 WHERE id=?1",
                params![award, expires],
            )?;
            tx.commit()?;
            Ok(GiveawayClick::Delivered(expires))
        })();
        if result.is_err() {
            let _ = db.execute("UPDATE giveaway_award SET delivery_error=1 WHERE id=?1 AND accepted_at IS NOT NULL", [award]);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::premium::DAY_MS;

    const NOW: i64 = 1_800_000_000_000;

    fn setup(plan: &str) -> (SqliteStore, i64) {
        let db = SqliteStore::open_in_memory().unwrap();
        let draft = serde_json::from_value::<GiveawayAction>(serde_json::json!({
            "action": "save", "request_id": "ea01298f-a728-489a-9639-856befd711cc",
            "channel_id": "200", "title": "Test", "description": "", "plan": plan,
            "days": 30, "seats": if plan=="premium" { 3 } else { 0 },
            "winner_count": 2, "starts_at": 0, "ends_at": NOW+HOUR, "claim_hours": 48
        }))
        .unwrap();
        let id = db.giveaway_admin("100", draft, NOW).unwrap().unwrap();
        assert!(
            db.giveaway_admin("999", GiveawayAction::Schedule { id }, NOW)
                .unwrap()
                .is_none()
        );
        db.giveaway_admin("100", GiveawayAction::Schedule { id }, NOW)
            .unwrap()
            .unwrap();
        assert_eq!(db.giveaway_jobs("100", NOW).unwrap().len(), 1);
        assert!(db.giveaway_publish_attempt(id, NOW).unwrap());
        assert!(!db.giveaway_publish_attempt(id, NOW + 60_000).unwrap());
        db.giveaway_published(id, "300").unwrap();
        assert_eq!(
            db.giveaway_click("100", "999", id, "501", "join", NOW)
                .unwrap(),
            GiveawayClick::Closed
        );
        assert_eq!(
            db.giveaway_click("100", "300", id, "501", "join", NOW)
                .unwrap(),
            GiveawayClick::VoteRequired
        );
        for user in ["501", "502", "503"] {
            db.connection()
                .execute(
                    "INSERT INTO giveaway_entry VALUES (?1,?2)",
                    params![id, user],
                )
                .unwrap();
        }
        assert_eq!(
            db.giveaway_click("100", "300", id, "504", "join", NOW + HOUR)
                .unwrap(),
            GiveawayClick::Closed
        );
        db.giveaway_draw(id, None, &["501".into(), "502".into()], NOW + HOUR)
            .unwrap();
        db.giveaway_draw(id, None, &["503".into()], NOW + HOUR)
            .unwrap();
        assert_eq!(db.giveaways("100").unwrap()[0].awards.len(), 2);
        for award in &db.giveaways("100").unwrap()[0].awards {
            db.giveaway_dm_attempt(award.id, NOW + HOUR).unwrap();
            db.giveaway_dm_sent(award.id, "test-dm", &format!("dm-{}", award.id))
                .unwrap();
        }
        (db, id)
    }

    #[test]
    fn active_plus_extends_once_and_only_the_winner_can_claim() {
        let (db, id) = setup("plus");
        let current = db.grant_user_premium("501", 30, "manual", NOW).unwrap();
        let expiry = current + 30 * DAY_MS;
        assert_eq!(
            db.giveaway_click("100", "300", id, "503", "claim", NOW + HOUR)
                .unwrap(),
            GiveawayClick::NotWinner
        );
        for _ in 0..2 {
            assert_eq!(
                db.giveaway_click("100", "300", id, "501", "claim", NOW + HOUR)
                    .unwrap(),
                GiveawayClick::Delivered(expiry)
            );
        }
        assert_eq!(db.user_premium_expiry("501").unwrap(), Some(expiry));
    }

    #[test]
    fn active_premium_keeps_its_capacity_and_replaces_only_the_expired_prize() {
        let (db, id) = setup("premium");
        let current = db.grant_guild_pass("501", 8, 30, "manual", NOW).unwrap();
        let expiry = current + 30 * DAY_MS;
        assert_eq!(
            db.giveaway_click("100", "300", id, "501", "claim", NOW + HOUR)
                .unwrap(),
            GiveawayClick::Delivered(expiry)
        );
        assert_eq!(db.premium_pass("501").unwrap().unwrap().seats, 8);
        let expired = &db.giveaways("100").unwrap()[0].awards[1];
        assert_eq!(
            db.giveaway_click("100", "300", id, "502", "claim", expired.deadline)
                .unwrap(),
            GiveawayClick::Expired
        );
        let action = GiveawayAction::Replace {
            id,
            award_id: expired.id,
        };
        db.giveaway_admin("100", action, expired.deadline)
            .unwrap()
            .unwrap();
        assert_eq!(db.giveaway_candidates(id).unwrap(), vec!["503"]);
        db.giveaway_draw(id, Some(expired.id), &["503".into()], expired.deadline)
            .unwrap();
        let campaigns = db.giveaways("100").unwrap();
        let awards = &campaigns[0].awards;
        assert_eq!(awards[0].expires_at, Some(expiry));
        assert!(awards[1].replaced);
        assert_eq!(awards[2].deadline, expired.deadline + 48 * HOUR);
    }

    #[test]
    fn paid_period_reserves_prize_then_delivers_when_renewals_end() {
        for (campaign, paid) in [("plus", "plus"), ("premium", "premium"), ("premium", "max")] {
            let (db, id) = setup(campaign);
            db.connection()
                .execute(
                    "INSERT INTO stripe_paid_entitlement VALUES
                ('sub','501','invoice',?1,?1,?2,?3,5)",
                    params![NOW, NOW + 30 * DAY_MS, paid],
                )
                .unwrap();
            assert_eq!(
                db.giveaway_click("100", "300", id, "501", "claim", NOW + HOUR)
                    .unwrap(),
                GiveawayClick::Waiting
            );
            let campaigns = db.giveaways("100").unwrap();
            let g = &campaigns[0];
            let prize = &g.awards[0];
            db.giveaway_sync(id, g.revision).unwrap();
            assert_eq!(prize.accepted_at, Some(NOW + HOUR));
            assert!(prize.expires_at.is_none());
            assert!(
                db.giveaway_jobs("100", NOW + 29 * DAY_MS)
                    .unwrap()
                    .is_empty()
            );
            db.connection()
                .execute(
                    "UPDATE stripe_paid_entitlement SET expires_at=?1",
                    [NOW + 60 * DAY_MS],
                )
                .unwrap();
            assert!(
                db.giveaway_jobs("100", NOW + 30 * DAY_MS)
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(db.giveaway_jobs("100", NOW + 60 * DAY_MS).unwrap().len(), 1);
            assert_eq!(
                db.giveaway_deliver(prize.id, NOW + 60 * DAY_MS).unwrap(),
                GiveawayClick::Delivered(NOW + 90 * DAY_MS)
            );
            assert_eq!(
                db.giveaway_click("100", "300", id, "501", "claim", NOW + 61 * DAY_MS)
                    .unwrap(),
                GiveawayClick::Delivered(NOW + 90 * DAY_MS)
            );
            let paid_expiry: i64 = db
                .connection()
                .query_row(
                    "SELECT expires_at FROM stripe_paid_entitlement WHERE subscription_id='sub'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(paid_expiry, NOW + 60 * DAY_MS);
        }
    }

    #[test]
    fn delivery_failure_preserves_claim_and_recovers() {
        let (db, id) = setup("plus");
        db.connection()
            .execute_batch(
                "CREATE TEMP TRIGGER fail_grant BEFORE INSERT ON premium_user
            BEGIN SELECT RAISE(ABORT,'fixture'); END;",
            )
            .unwrap();
        assert!(
            db.giveaway_click("100", "300", id, "501", "claim", NOW + HOUR)
                .is_err()
        );
        let campaigns = db.giveaways("100").unwrap();
        let award = &campaigns[0].awards[0];
        assert!(award.accepted_at.is_some() && award.delivery_error && award.expires_at.is_none());
        db.connection()
            .execute_batch("DROP TRIGGER fail_grant")
            .unwrap();
        let later = award.deadline + 1;
        assert_eq!(
            db.giveaway_deliver(award.id, later).unwrap(),
            GiveawayClick::Delivered(later + 30 * DAY_MS)
        );
    }
    #[test]
    fn dm_claim_context_is_bound_to_the_campaign_winner_and_message() {
        let (db, id) = setup("plus");
        let awards = &db.giveaways("100").unwrap()[0].awards;
        let award = awards[0].id;
        db.giveaway_dm_attempt(award, NOW).unwrap();
        db.giveaway_dm_sent(award, "dm-channel", "dm-message")
            .unwrap();
        assert_eq!(
            db.giveaway_dm_original(id, "501", "dm-message").unwrap(),
            Some("300".into())
        );
        assert_eq!(
            db.giveaway_dm_original(id, "502", "dm-message").unwrap(),
            None
        );
        assert_eq!(
            db.giveaway_dm_original(id + 1, "501", "dm-message")
                .unwrap(),
            None
        );
        assert_eq!(db.giveaway_dm_original(id, "501", "forged").unwrap(), None);
        db.giveaway_dm_blocked(id, award).unwrap();
        assert!(db.giveaways("100").unwrap()[0].awards[0].dm_blocked);
        db.erase_user_data("501").unwrap();
        assert_eq!(
            db.giveaway_dm_original(id, "501", "dm-message").unwrap(),
            None
        );
    }

    #[test]
    fn vote_tickets_respect_the_open_window_and_fail_atomically() {
        const USER: &str = "12345678901234567";
        const SECRET: &str = "0123456789abcdef0123456789abcdef";
        let (db, id) = setup("plus");
        db.connection()
            .execute(
                "UPDATE giveaway SET status='open',starts_at=?2 WHERE id=?1",
                params![id, NOW],
            )
            .unwrap();
        db.claim_topgg_vote_reward_at(Some("old"), USER, NOW + 1, SECRET, Some(NOW - 1))
            .unwrap();
        assert_eq!(db.giveaways("100").unwrap()[0].entries, 3);
        db.connection().execute_batch("CREATE TEMP TRIGGER fail_vote BEFORE INSERT ON giveaway_vote BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(
            db.claim_topgg_vote_reward_at(Some("retry"), USER, NOW + 2, SECRET, Some(NOW + 1))
                .is_err()
        );
        db.connection()
            .execute_batch("DROP TRIGGER fail_vote")
            .unwrap();
        db.claim_topgg_vote_reward_at(Some("retry"), USER, NOW + 2, SECRET, Some(NOW + 1))
            .unwrap();
        assert_eq!(db.giveaways("100").unwrap()[0].entries, 4);
        db.claim_topgg_vote_reward_at(
            Some("after-end"),
            USER,
            NOW + HOUR,
            SECRET,
            Some(NOW + HOUR),
        )
        .unwrap();
        assert_eq!(db.giveaways("100").unwrap()[0].entries, 4);
    }
    #[test]
    fn three_bot_entry_cap_survives_legacy_schema_upgrade() {
        let (db, id) = setup("plus");
        db.connection()
            .execute(
                "UPDATE giveaway SET status='open',starts_at=?2 WHERE id=?1",
                params![id, NOW],
            )
            .unwrap();
        db.connection()
            .execute_batch("ALTER TABLE giveaway_vote DROP COLUMN bot_id")
            .unwrap();
        db.connection().execute("INSERT INTO giveaway_vote (giveaway_id,event_id,user_id,voted_at) VALUES (?1,'old-tts','501',?2)",params![id,NOW]).unwrap();
        crate::migration::migrate_legacy_schema(db.connection()).unwrap();
        crate::migration::migrate_legacy_schema(db.connection()).unwrap();
        for (_, bot) in vozen_core::GIVEAWAY_VOTE_BOTS {
            for event in ["new", "retry"] {
                db.record_giveaway_vote(bot, &format!("{bot}-{event}"), "501", NOW + 1, NOW + 2)
                    .unwrap();
            }
        }
        let count: i64 = db
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM giveaway_vote WHERE giveaway_id=?1 AND user_id='501'",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 3);
        let helper = vozen_core::GIVEAWAY_VOTE_BOTS[1].1;
        assert_eq!(
            db.record_giveaway_vote(helper, "old", "502", NOW - 1, NOW + 1)
                .unwrap(),
            0
        );
        assert_eq!(
            db.record_giveaway_vote(helper, "late", "502", NOW + HOUR, NOW + HOUR)
                .unwrap(),
            0
        );
        assert_eq!(
            db.record_giveaway_vote("unknown", "x", "502", NOW, NOW)
                .unwrap(),
            0
        );
        db.erase_user_data("501").unwrap();
        assert_eq!(
            db.connection()
                .query_row(
                    "SELECT COUNT(*) FROM giveaway_vote WHERE user_id='501'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }
}
