//! Durable Top.gg reward storage.
//!
//! The temporary entitlement keeps a Discord ID only while the active reward needs it. A keyed
//! HMAC ledger enforces a short rolling vote limit and is purged with provider replay data.

use hmac::{Hmac, Mac};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{SqliteStore, StoreError};

type HmacSha256 = Hmac<Sha256>;

/// A valid Top.gg vote grants one day of Plus. Rewards may stack, but never more than two days
/// ahead of the current time.
pub const VOTE_REWARD_MS: i64 = 24 * 60 * 60 * 1_000;
pub const VOTE_REWARD_MAX_AHEAD_MS: i64 = 48 * 60 * 60 * 1_000;
pub const VOTE_REWARD_MAX_GRANTS_PER_30_DAYS: i64 = 4;
pub const VOTE_REDEMPTION_SECRET_MIN_LENGTH: usize = 32;
/// Delivery IDs are only provider replay protection; permanent reward idempotency is separate.
pub const TOPGG_EVENT_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1_000;
const MAX_TOPGG_EVENT_ID_LENGTH: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoteRewardResult {
    pub granted: bool,
    pub expires_at: Option<i64>,
    pub rate_limited: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoteRewardStatus {
    pub eligible: bool,
    pub grants_remaining: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopggVoteRewardResult {
    Granted { expires_at: i64 },
    RateLimited,
    DuplicateEvent,
}

impl SqliteStore {
    /// Pins the stable HMAC key and backfills active legacy rewards into the rolling ledger.
    /// A key change fails closed instead of making historical rows unmatchable.
    pub fn initialize_vote_redemption_ledger(
        &self,
        redemption_secret: &str,
    ) -> Result<usize, StoreError> {
        let transaction = self.connection().unchecked_transaction()?;
        assert_stable_redemption_secret(&transaction, redemption_secret)?;
        let legacy_rewards = {
            let mut statement =
                transaction.prepare("SELECT user_id, rewarded_at FROM vote_reward")?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut backfilled = 0;
        for (user_id, rewarded_at) in legacy_rewards {
            let user_hash = vote_redemption_hash(redemption_secret, &user_id)?;
            backfilled += transaction.execute(
                "INSERT OR IGNORE INTO vote_reward_ledger (user_hash, granted_at) VALUES (?1, ?2)",
                params![user_hash, rewarded_at],
            )?;
        }
        transaction.commit()?;
        Ok(backfilled)
    }

    pub fn vote_reward_at(&self, user_id: &str) -> Result<Option<i64>, StoreError> {
        self.connection()
            .query_row(
                "SELECT rewarded_at FROM vote_reward WHERE user_id = ?1",
                [user_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn vote_reward_status(
        &self,
        user_id: &str,
        redemption_secret: &str,
        now: i64,
    ) -> Result<VoteRewardStatus, StoreError> {
        let user_hash = vote_redemption_hash(redemption_secret, user_id)?;
        assert_stable_redemption_secret(self.connection(), redemption_secret)?;
        let claims: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM vote_reward_ledger WHERE user_hash = ?1 AND granted_at > ?2",
            params![user_hash, now - TOPGG_EVENT_RETENTION_MS],
            |row| row.get(0),
        )?;
        Ok(VoteRewardStatus {
            eligible: claims < VOTE_REWARD_MAX_GRANTS_PER_30_DAYS,
            grants_remaining: (VOTE_REWARD_MAX_GRANTS_PER_30_DAYS - claims).max(0),
        })
    }

    /// Claims a verified legacy Top.gg vote. Modern v1 deliveries should use
    /// [`Self::claim_topgg_vote_reward`] with their provider event ID.
    pub fn claim_vote_reward(
        &self,
        user_id: &str,
        now: i64,
        redemption_secret: &str,
    ) -> Result<VoteRewardResult, StoreError> {
        match self.claim_topgg_vote_reward(None, user_id, now, redemption_secret)? {
            TopggVoteRewardResult::Granted { expires_at } => Ok(VoteRewardResult {
                granted: true,
                expires_at: Some(expires_at),
                rate_limited: false,
            }),
            TopggVoteRewardResult::RateLimited => Ok(VoteRewardResult {
                granted: false,
                expires_at: None,
                rate_limited: true,
            }),
            TopggVoteRewardResult::DuplicateEvent => unreachable!("legacy claims have no event id"),
        }
    }

    /// Atomically claims a Top.gg delivery ID and, within the rolling limit, extends Plus. If
    /// storage fails, the transaction rolls back the event marker so a legitimate retry remains
    /// possible.
    pub fn claim_topgg_vote_reward(
        &self,
        event_id: Option<&str>,
        user_id: &str,
        now: i64,
        redemption_secret: &str,
    ) -> Result<TopggVoteRewardResult, StoreError> {
        if let Some(event_id) = event_id {
            validate_event_id(event_id)?;
        }
        let user_hash = vote_redemption_hash(redemption_secret, user_id)?;
        let transaction = self.connection().unchecked_transaction()?;
        assert_stable_redemption_secret(&transaction, redemption_secret)?;

        if let Some(event_id) = event_id {
            let inserted = transaction.execute(
                "INSERT OR IGNORE INTO topgg_webhook_event (event_id, processed_at) VALUES (?1, ?2)",
                params![event_id, now],
            )?;
            if inserted == 0 {
                transaction.commit()?;
                return Ok(TopggVoteRewardResult::DuplicateEvent);
            }
            crate::growth_lifecycle::add_topgg_vote_daily(&transaction, now)?;
        }

        let grants_in_window: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM vote_reward_ledger WHERE user_hash = ?1 AND granted_at > ?2",
            params![user_hash, now - TOPGG_EVENT_RETENTION_MS],
            |row| row.get(0),
        )?;
        let result = if grants_in_window >= VOTE_REWARD_MAX_GRANTS_PER_30_DAYS {
            TopggVoteRewardResult::RateLimited
        } else {
            let current_expires_at = transaction
                .query_row(
                    "SELECT rewarded_at + ?2 FROM vote_reward WHERE user_id = ?1",
                    params![user_id, VOTE_REWARD_MS],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            let base = current_expires_at.unwrap_or(now).max(now);
            let expires_at = base
                .saturating_add(VOTE_REWARD_MS)
                .min(now.saturating_add(VOTE_REWARD_MAX_AHEAD_MS));
            transaction.execute(
                "INSERT INTO vote_reward (user_id, rewarded_at) VALUES (?1, ?2)
                 ON CONFLICT(user_id) DO UPDATE SET rewarded_at = excluded.rewarded_at",
                params![user_id, expires_at - VOTE_REWARD_MS],
            )?;
            transaction.execute(
                "INSERT INTO vote_reward_ledger (user_hash, granted_at) VALUES (?1, ?2)",
                params![user_hash, now],
            )?;
            TopggVoteRewardResult::Granted { expires_at }
        };
        transaction.commit()?;
        Ok(result)
    }

    /// Removes expired raw-ID entitlements. Pseudonymous vote rows are retained only for the
    /// 30-day rolling eligibility window and are purged with provider delivery IDs.
    pub fn purge_expired_vote_rewards(&self, now: i64) -> Result<usize, StoreError> {
        Ok(self.connection().execute(
            "DELETE FROM vote_reward WHERE rewarded_at <= ?1",
            [now - VOTE_REWARD_MS],
        )?)
    }

    pub fn purge_expired_topgg_events(&self, now: i64) -> Result<usize, StoreError> {
        let events = self.connection().execute(
            "DELETE FROM topgg_webhook_event WHERE processed_at < ?1",
            [now - TOPGG_EVENT_RETENTION_MS],
        )?;
        let ledger = self.connection().execute(
            "DELETE FROM vote_reward_ledger WHERE granted_at < ?1",
            [now - TOPGG_EVENT_RETENTION_MS],
        )?;
        Ok(events + ledger)
    }
}

pub(crate) fn assert_stable_redemption_secret(
    connection: &Connection,
    redemption_secret: &str,
) -> Result<(), StoreError> {
    let fingerprint = redemption_secret_fingerprint(redemption_secret)?;
    connection.execute(
        "INSERT OR IGNORE INTO vote_redemption_meta (singleton, secret_fingerprint) VALUES (1, ?1)",
        [fingerprint.as_str()],
    )?;
    let stored: String = connection.query_row(
        "SELECT secret_fingerprint FROM vote_redemption_meta WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    if stored.len() != fingerprint.len()
        || !bool::from(stored.as_bytes().ct_eq(fingerprint.as_bytes()))
    {
        return Err(StoreError::VoteRedemptionSecretMismatch);
    }
    Ok(())
}

fn redemption_secret_fingerprint(redemption_secret: &str) -> Result<String, StoreError> {
    if redemption_secret.len() < VOTE_REDEMPTION_SECRET_MIN_LENGTH {
        return Err(StoreError::InvalidVoteRedemptionSecret);
    }
    Ok(hex_encode(Sha256::digest(format!(
        "vozen-vote-redemption:v1:{redemption_secret}"
    ))))
}

fn vote_redemption_hash(redemption_secret: &str, user_id: &str) -> Result<String, StoreError> {
    redemption_secret_fingerprint(redemption_secret)?;
    if !is_discord_user_id(user_id) {
        return Err(StoreError::InvalidVoteUserId);
    }
    let mut mac = HmacSha256::new_from_slice(redemption_secret.as_bytes())
        .expect("HMAC accepts keys of every size");
    mac.update(format!("discord:{user_id}").as_bytes());
    Ok(hex_encode(mac.finalize().into_bytes()))
}

fn validate_event_id(event_id: &str) -> Result<(), StoreError> {
    if event_id.is_empty() || event_id.len() > MAX_TOPGG_EVENT_ID_LENGTH {
        return Err(StoreError::InvalidTopggEventId);
    }
    Ok(())
}

fn is_discord_user_id(value: &str) -> bool {
    (5..=25).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn hex_encode(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;
    const SECRET: &str = "0123456789abcdef0123456789abcdef";
    const USER: &str = "12345678901234567";

    #[test]
    fn rewards_stack_for_four_votes_then_reset_after_the_rolling_window() {
        let store = SqliteStore::open_in_memory().expect("store");
        assert_eq!(
            store.claim_vote_reward(USER, NOW, SECRET).expect("grant"),
            VoteRewardResult {
                granted: true,
                expires_at: Some(NOW + VOTE_REWARD_MS),
                rate_limited: false,
            }
        );
        for vote in 1..VOTE_REWARD_MAX_GRANTS_PER_30_DAYS {
            assert!(matches!(
                store
                    .claim_vote_reward(USER, NOW + vote, SECRET)
                    .expect("stacked grant"),
                VoteRewardResult { granted: true, .. }
            ));
        }
        assert_eq!(
            store
                .claim_vote_reward(USER, NOW + 5, SECRET)
                .expect("limited"),
            VoteRewardResult {
                granted: false,
                expires_at: None,
                rate_limited: true,
            }
        );
        assert_eq!(
            store
                .vote_reward_status(USER, SECRET, NOW + 5)
                .expect("status"),
            VoteRewardStatus {
                eligible: false,
                grants_remaining: 0,
            }
        );
        assert!(
            store
                .claim_vote_reward(USER, NOW + TOPGG_EVENT_RETENTION_MS + 6, SECRET)
                .expect("new window")
                .granted
        );
    }

    #[test]
    fn initialization_backfills_existing_temporary_rewards_and_pins_secret() {
        let store = SqliteStore::open_in_memory().expect("store");
        store
            .connection()
            .execute(
                "INSERT INTO vote_reward (user_id, rewarded_at) VALUES (?1, ?2)",
                params![USER, NOW],
            )
            .expect("legacy reward");
        assert_eq!(
            store
                .initialize_vote_redemption_ledger(SECRET)
                .expect("backfill"),
            1
        );
        assert_eq!(
            store
                .initialize_vote_redemption_ledger(SECRET)
                .expect("repeat"),
            0
        );
        assert!(matches!(
            store.initialize_vote_redemption_ledger(
                "a different secret with at least 32 characters"
            ),
            Err(StoreError::VoteRedemptionSecretMismatch)
        ));
    }

    #[test]
    fn event_and_reward_are_one_transactional_delivery_gate() {
        let store = SqliteStore::open_in_memory().expect("store");
        assert_eq!(
            store
                .claim_topgg_vote_reward(Some("evt-1"), USER, NOW, SECRET)
                .expect("first"),
            TopggVoteRewardResult::Granted {
                expires_at: NOW + VOTE_REWARD_MS
            }
        );
        assert_eq!(
            store
                .claim_topgg_vote_reward(Some("evt-1"), USER, NOW + 1, SECRET)
                .expect("retry"),
            TopggVoteRewardResult::DuplicateEvent
        );
        assert!(matches!(
            store
                .claim_topgg_vote_reward(Some("evt-2"), USER, NOW + 2, SECRET)
                .expect("other event"),
            TopggVoteRewardResult::Granted { .. }
        ));
    }

    #[test]
    fn unique_provider_votes_create_identity_free_daily_metrics_once() {
        let store = SqliteStore::open_in_memory().expect("store");
        for index in 0..=VOTE_REWARD_MAX_GRANTS_PER_30_DAYS {
            store
                .claim_topgg_vote_reward(Some(&format!("vote-{index}")), USER, NOW + index, SECRET)
                .expect("valid vote");
        }
        store
            .claim_topgg_vote_reward(Some("vote-0"), USER, NOW + 100, SECRET)
            .expect("provider retry");

        let metrics = store
            .list_growth_daily_metrics("1970-01-01", "1970-01-01")
            .expect("daily growth");
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].source, "topgg");
        assert_eq!(metrics[0].votes, 5);
        assert_eq!(metrics[0].joins, 0);

        let user_identifiers: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM growth_daily_metric WHERE source = ?1 OR source = ?2",
                params![USER, vote_redemption_hash(SECRET, USER).expect("hash")],
                |row| row.get(0),
            )
            .expect("privacy check");
        assert_eq!(user_identifiers, 0);
    }

    #[test]
    fn invalid_inputs_fail_before_any_ledger_write() {
        let store = SqliteStore::open_in_memory().expect("store");
        assert!(matches!(
            store.claim_vote_reward("bad", NOW, SECRET),
            Err(StoreError::InvalidVoteUserId)
        ));
        assert!(matches!(
            store.claim_topgg_vote_reward(Some(""), USER, NOW, SECRET),
            Err(StoreError::InvalidTopggEventId)
        ));
        assert!(matches!(
            store.claim_vote_reward(USER, NOW, "short"),
            Err(StoreError::InvalidVoteRedemptionSecret)
        ));
        let rows: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM vote_redemption_meta", [], |row| {
                row.get(0)
            })
            .expect("count");
        assert_eq!(rows, 0);
    }

    #[test]
    fn purge_expires_raw_delivery_and_pseudonymous_rolling_rows() {
        let store = SqliteStore::open_in_memory().expect("store");
        store
            .claim_topgg_vote_reward(Some("old"), USER, NOW, SECRET)
            .expect("grant");
        assert_eq!(
            store
                .purge_expired_vote_rewards(NOW + VOTE_REWARD_MS)
                .expect("purge"),
            1
        );
        assert_eq!(
            store
                .purge_expired_topgg_events(NOW + TOPGG_EVENT_RETENTION_MS + 1)
                .expect("purge"),
            2
        );
        assert_eq!(
            store
                .vote_reward_status(USER, SECRET, NOW + TOPGG_EVENT_RETENTION_MS + 1)
                .expect("status"),
            VoteRewardStatus {
                eligible: true,
                grants_remaining: VOTE_REWARD_MAX_GRANTS_PER_30_DAYS,
            }
        );
    }
}
