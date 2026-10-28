//! Durable Top.gg reward storage.
//!
//! The temporary entitlement keeps a Discord ID for exactly as long as the 48-hour Plus reward
//! needs it. The lifetime one-claim guard keeps only a keyed HMAC, so `/privacy erase` cannot
//! turn into a way to reclaim the promotion.

use hmac::{Hmac, Mac};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{SqliteStore, StoreError};

type HmacSha256 = Hmac<Sha256>;

/// One Top.gg vote reward lasts 48 hours, matching the Node entitlement calculation.
pub const VOTE_REWARD_MS: i64 = 48 * 60 * 60 * 1_000;
pub const VOTE_REDEMPTION_SECRET_MIN_LENGTH: usize = 32;
/// Delivery IDs are only provider replay protection; permanent reward idempotency is separate.
pub const TOPGG_EVENT_RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1_000;
const MAX_TOPGG_EVENT_ID_LENGTH: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoteRewardResult {
    pub granted: bool,
    pub expires_at: Option<i64>,
    pub already_redeemed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoteRewardStatus {
    pub eligible: bool,
    pub already_redeemed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopggVoteRewardResult {
    Granted { expires_at: i64 },
    AlreadyRedeemed,
    DuplicateEvent,
}

impl SqliteStore {
    /// Pins the stable HMAC key and converts pre-ledger temporary rewards into permanent
    /// pseudonymous claim markers. A key change fails closed instead of reopening eligibility.
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
                "INSERT OR IGNORE INTO vote_redemption (user_hash, redeemed_at) VALUES (?1, ?2)",
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
    ) -> Result<VoteRewardStatus, StoreError> {
        let user_hash = vote_redemption_hash(redemption_secret, user_id)?;
        assert_stable_redemption_secret(self.connection(), redemption_secret)?;
        let already_redeemed = self.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM vote_redemption WHERE user_hash = ?1)",
            [user_hash],
            |row| row.get::<_, i64>(0),
        )? != 0;
        Ok(VoteRewardStatus {
            eligible: !already_redeemed,
            already_redeemed,
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
                already_redeemed: false,
            }),
            TopggVoteRewardResult::AlreadyRedeemed => Ok(VoteRewardResult {
                granted: false,
                expires_at: None,
                already_redeemed: true,
            }),
            TopggVoteRewardResult::DuplicateEvent => unreachable!("legacy claims have no event id"),
        }
    }

    /// Atomically claims a Top.gg delivery ID and its single lifetime reward. If storing the
    /// reward fails, the transaction rolls back the event marker so a legitimate retry remains
    /// possible. This removes the Node implementation's separate claim/release failure window.
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
        }

        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO vote_redemption (user_hash, redeemed_at) VALUES (?1, ?2)",
            params![user_hash, now],
        )?;
        let result = if inserted == 0 {
            TopggVoteRewardResult::AlreadyRedeemed
        } else {
            transaction.execute(
                "INSERT INTO vote_reward (user_id, rewarded_at) VALUES (?1, ?2)
                 ON CONFLICT(user_id) DO UPDATE SET rewarded_at = excluded.rewarded_at",
                params![user_id, now],
            )?;
            TopggVoteRewardResult::Granted {
                expires_at: now + VOTE_REWARD_MS,
            }
        };
        transaction.commit()?;
        Ok(result)
    }

    /// Removes only expired raw-ID entitlement records. The keyed one-time markers stay.
    pub fn purge_expired_vote_rewards(&self, now: i64) -> Result<usize, StoreError> {
        Ok(self.connection().execute(
            "DELETE FROM vote_reward WHERE rewarded_at <= ?1",
            [now - VOTE_REWARD_MS],
        )?)
    }

    pub fn purge_expired_topgg_events(&self, now: i64) -> Result<usize, StoreError> {
        Ok(self.connection().execute(
            "DELETE FROM topgg_webhook_event WHERE processed_at < ?1",
            [now - TOPGG_EVENT_RETENTION_MS],
        )?)
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
    fn reward_is_lifetime_idempotent_but_temporary_premium_is_erasable() {
        let store = SqliteStore::open_in_memory().expect("store");
        assert_eq!(
            store.claim_vote_reward(USER, NOW, SECRET).expect("grant"),
            VoteRewardResult {
                granted: true,
                expires_at: Some(NOW + VOTE_REWARD_MS),
                already_redeemed: false,
            }
        );
        assert!(store.is_user_premium(USER, NOW + 1).expect("premium"));
        store.erase_user_data(USER).expect("privacy erase");
        assert_eq!(store.vote_reward_at(USER).expect("reward"), None);
        assert_eq!(
            store
                .claim_vote_reward(USER, NOW + 2, SECRET)
                .expect("repeat"),
            VoteRewardResult {
                granted: false,
                expires_at: None,
                already_redeemed: true,
            }
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
        assert_eq!(
            store
                .claim_topgg_vote_reward(Some("evt-2"), USER, NOW + 2, SECRET)
                .expect("other event"),
            TopggVoteRewardResult::AlreadyRedeemed
        );
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
    fn purge_keeps_the_permanent_marker_but_expires_raw_and_delivery_rows() {
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
            1
        );
        assert_eq!(
            store.vote_reward_status(USER, SECRET).expect("status"),
            VoteRewardStatus {
                eligible: false,
                already_redeemed: true
            }
        );
    }
}
