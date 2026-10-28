use rusqlite::{Connection, OptionalExtension, params};

use crate::{SqliteStore, StoreError};

/// Milliseconds in a billing day. This matches the Node subscription contract exactly.
pub const DAY_MS: i64 = 86_400_000;
const RANKED_PASS_ACTIVATIONS: &str = "WITH ranked_pass_activations AS (
    SELECT user_id, guild_id, activated_at,
           ROW_NUMBER() OVER (PARTITION BY user_id ORDER BY activated_at, guild_id) AS seat_number
    FROM premium_pass_activation
)";

/// The current Top.gg Plus reward duration. It is an existing entitlement source, not a payment.
pub const VOTE_REWARD_MS: i64 = 48 * 60 * 60 * 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PremiumKind {
    Guild,
    User,
}

impl PremiumKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Guild => "guild",
            Self::User => "user",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PremiumPass {
    pub seats: i64,
    pub expires_at: i64,
    pub source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivateStatus {
    Ok,
    Already,
    NoPass,
    Expired,
    NoSeats,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivateResult {
    pub status: ActivateStatus,
    pub seats: Option<i64>,
    pub used: Option<i64>,
    pub expires_at: Option<i64>,
}

impl ActivateResult {
    fn without_pass(status: ActivateStatus) -> Self {
        Self {
            status,
            seats: None,
            used: None,
            expires_at: None,
        }
    }

    fn with_pass(status: ActivateStatus, pass: &PremiumPass, used: i64) -> Self {
        Self {
            status,
            seats: Some(pass.seats),
            used: Some(used),
            expires_at: Some(pass.expires_at),
        }
    }

    fn expired(pass: &PremiumPass) -> Self {
        Self {
            status: ActivateStatus::Expired,
            seats: None,
            used: None,
            expires_at: Some(pass.expires_at),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntitlementGrant {
    pub kind: PremiumKind,
    pub id: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntitlementSyncResult {
    pub guilds_active: usize,
    pub users_active: usize,
    pub revoked: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PremiumStatusView {
    pub plus_active: bool,
    pub plus_expires_at: Option<i64>,
    pub pass: Option<PremiumPassStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PremiumPassStatus {
    pub seats: i64,
    pub used: i64,
    pub expires_at: i64,
    pub active: bool,
    pub guilds: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuildPassOwner {
    pub owner_id: String,
    pub seats: i64,
}

impl SqliteStore {
    /// Direct guild Premium, current Discord entitlement, or an unexpired activated pass.
    pub fn is_guild_premium(&self, guild_id: &str, now: i64) -> Result<bool, StoreError> {
        let direct_or_discord: Option<i64> = self.connection().query_row(
            "SELECT MAX(expires_at) FROM (
                SELECT expires_at FROM premium_guild WHERE guild_id = ?1
                UNION ALL
                SELECT expires_at FROM discord_premium_entitlement
                WHERE kind = 'guild' AND target_id = ?1
             )",
            [guild_id],
            |row| row.get(0),
        )?;
        if direct_or_discord.is_some_and(|expiry| expiry > now) {
            return Ok(true);
        }
        let has_active_pass: bool = self.connection().query_row(
            &format!("{RANKED_PASS_ACTIVATIONS} SELECT EXISTS(
                SELECT 1 FROM ranked_pass_activations activation
                JOIN premium_pass pass ON pass.user_id = activation.user_id
                WHERE activation.guild_id = ?1 AND pass.expires_at > ?2 AND activation.seat_number <= pass.seats
             )"),
            params![guild_id, now],
            |row| row.get::<_, i64>(0),
        )? != 0;
        Ok(has_active_pass)
    }

    /// User Plus comes from a direct grant, the 48-hour vote reward, or Discord entitlement.
    pub fn is_user_premium(&self, user_id: &str, now: i64) -> Result<bool, StoreError> {
        Ok(self
            .user_premium_expiry(user_id)?
            .is_some_and(|expiry| expiry > now))
    }

    /// The direct guild grant only; callers renewing a direct purchase must not extend a pass.
    pub fn guild_premium_expiry(&self, guild_id: &str) -> Result<Option<i64>, StoreError> {
        self.connection()
            .query_row(
                "SELECT expires_at FROM premium_guild WHERE guild_id = ?1",
                [guild_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Display-only effective guild expiry across all currently active sources.
    pub fn effective_guild_premium_expiry(
        &self,
        guild_id: &str,
        now: i64,
    ) -> Result<Option<i64>, StoreError> {
        self.connection()
            .query_row(
                &format!("{RANKED_PASS_ACTIVATIONS} SELECT MAX(expiry) FROM (
                SELECT expires_at AS expiry FROM premium_guild
                WHERE guild_id = ?1 AND expires_at > ?2
                UNION ALL
                SELECT expires_at FROM discord_premium_entitlement
                WHERE kind = 'guild' AND target_id = ?1 AND expires_at > ?2
                UNION ALL
                SELECT pass.expires_at FROM ranked_pass_activations activation
                JOIN premium_pass pass ON pass.user_id = activation.user_id
                WHERE activation.guild_id = ?1 AND pass.expires_at > ?2 AND activation.seat_number <= pass.seats
             )"),
                params![guild_id, now],
                |row| row.get(0),
            )
            .map_err(StoreError::from)
    }

    /// Full effective Plus expiry, including the short-lived vote reward and Discord entitlement.
    pub fn user_premium_expiry(&self, user_id: &str) -> Result<Option<i64>, StoreError> {
        self.connection()
            .query_row(
                "SELECT MAX(expires_at) FROM (
                SELECT expires_at FROM premium_user WHERE user_id = ?1
                UNION ALL
                SELECT rewarded_at + ?2 AS expires_at FROM vote_reward WHERE user_id = ?1
                UNION ALL
                SELECT expires_at FROM discord_premium_entitlement
                WHERE kind = 'user' AND target_id = ?1
             )",
                params![user_id, VOTE_REWARD_MS],
                |row| row.get(0),
            )
            .map_err(StoreError::from)
    }

    pub fn grant_guild_premium(
        &self,
        guild_id: &str,
        days: i64,
        source: &str,
        now: i64,
    ) -> Result<i64, StoreError> {
        grant_guild_premium_on(self.connection(), guild_id, days, source, now)
    }

    pub fn grant_user_premium(
        &self,
        user_id: &str,
        days: i64,
        source: &str,
        now: i64,
    ) -> Result<i64, StoreError> {
        grant_user_premium_on(self.connection(), user_id, days, source, now)
    }

    pub fn premium_pass(&self, user_id: &str) -> Result<Option<PremiumPass>, StoreError> {
        self.connection()
            .query_row(
                "SELECT seats, expires_at, source FROM premium_pass WHERE user_id = ?1",
                [user_id],
                |row| {
                    Ok(PremiumPass {
                        seats: row.get(0)?,
                        expires_at: row.get(1)?,
                        source: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn pass_activations(&self, user_id: &str) -> Result<Vec<String>, StoreError> {
        let mut statement = self.connection().prepare(
            "SELECT guild_id FROM premium_pass_activation WHERE user_id = ?1 ORDER BY activated_at",
        )?;
        statement
            .query_map([user_id], |row| row.get(0))?
            .collect::<Result<_, _>>()
            .map_err(StoreError::from)
    }

    pub fn active_seat_count(&self, user_id: &str) -> Result<i64, StoreError> {
        self.connection()
            .query_row(
                "SELECT COUNT(*) FROM premium_pass_activation WHERE user_id = ?1",
                [user_id],
                |row| row.get(0),
            )
            .map_err(StoreError::from)
    }

    /// The oldest still-active pass covering a guild. Legacy data permits two owners for one
    /// guild, so this stable ordering decides whose shared Google HD allowance is used.
    pub fn resolve_guild_pass_owner(
        &self,
        guild_id: &str,
        now: i64,
    ) -> Result<Option<GuildPassOwner>, StoreError> {
        self.connection()
            .query_row(
                &format!("{RANKED_PASS_ACTIVATIONS} SELECT activation.user_id, pass.seats
                 FROM ranked_pass_activations activation
                 JOIN premium_pass pass ON pass.user_id = activation.user_id
                 WHERE activation.guild_id = ?1 AND pass.expires_at > ?2 AND activation.seat_number <= pass.seats
                 ORDER BY activation.activated_at ASC
                 LIMIT 1"),
                params![guild_id, now],
                |row| {
                    Ok(GuildPassOwner {
                        owner_id: row.get(0)?,
                        seats: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    pub fn grant_guild_pass(
        &self,
        user_id: &str,
        seats: i64,
        days: i64,
        source: &str,
        now: i64,
    ) -> Result<i64, StoreError> {
        grant_guild_pass_on(self.connection(), user_id, seats, days, source, now)
    }

    /// Atomically consumes a pass seat. The transaction preserves Node's count-and-insert
    /// invariant when two dashboard requests arrive together.
    pub fn activate_seat(
        &self,
        user_id: &str,
        guild_id: &str,
        now: i64,
    ) -> Result<ActivateResult, StoreError> {
        let transaction = self.connection().unchecked_transaction()?;
        let pass = premium_pass_from(&transaction, user_id)?;
        let result = match pass {
            None => ActivateResult::without_pass(ActivateStatus::NoPass),
            Some(pass) if pass.expires_at <= now => ActivateResult::expired(&pass),
            Some(pass) => {
                let used = active_seat_count_from(&transaction, user_id)?;
                let already: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM premium_pass_activation WHERE user_id = ?1 AND guild_id = ?2)",
                    params![user_id, guild_id], |row| row.get::<_, i64>(0),
                )? != 0;
                if already {
                    ActivateResult::with_pass(ActivateStatus::Already, &pass, used)
                } else if used >= pass.seats {
                    ActivateResult::with_pass(ActivateStatus::NoSeats, &pass, used)
                } else {
                    transaction.execute(
                        "INSERT INTO premium_pass_activation (user_id, guild_id, activated_at) VALUES (?1, ?2, ?3)",
                        params![user_id, guild_id, now],
                    )?;
                    ActivateResult::with_pass(ActivateStatus::Ok, &pass, used + 1)
                }
            }
        };
        transaction.commit()?;
        Ok(result)
    }

    pub fn deactivate_seat(&self, user_id: &str, guild_id: &str) -> Result<bool, StoreError> {
        Ok(self.connection().execute(
            "DELETE FROM premium_pass_activation WHERE user_id = ?1 AND guild_id = ?2",
            params![user_id, guild_id],
        )? > 0)
    }

    pub fn premium_status(&self, user_id: &str, now: i64) -> Result<PremiumStatusView, StoreError> {
        let plus_expires_at = self.user_premium_expiry(user_id)?;
        let pass = self
            .premium_pass(user_id)?
            .map(|pass| {
                let used = self.active_seat_count(user_id)?;
                let guilds = self.pass_activations(user_id)?;
                Ok::<PremiumPassStatus, StoreError>(PremiumPassStatus {
                    seats: pass.seats,
                    used,
                    expires_at: pass.expires_at,
                    active: pass.expires_at > now,
                    guilds,
                })
            })
            .transpose()?;
        Ok(PremiumStatusView {
            plus_active: plus_expires_at.is_some_and(|expiry| expiry > now),
            plus_expires_at,
            pass,
        })
    }

    /// Stores only an already-HMACed Ko-fi email key, never a clear-text email address.
    pub fn remember_kofi_supporter(
        &self,
        email_hash: &str,
        discord_id: &str,
        now: i64,
    ) -> Result<(), StoreError> {
        remember_kofi_supporter_on(self.connection(), email_hash, discord_id, now)
    }

    pub fn kofi_supporter(&self, email_hash: &str) -> Result<Option<String>, StoreError> {
        self.connection()
            .query_row(
                "SELECT discord_id FROM kofi_supporter WHERE email_hash = ?1",
                [email_hash],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Returns true exactly once for an external transaction ID. The future webhook handler must
    /// perform this and a grant in the same transaction before it is allowed to serve traffic.
    pub fn record_kofi_transaction(
        &self,
        transaction_id: &str,
        now: i64,
    ) -> Result<bool, StoreError> {
        record_kofi_transaction_on(self.connection(), transaction_id, now)
    }

    /// Reconciles the complete current Discord entitlement set. It deliberately never mutates
    /// durable Ko-fi, code-redemption or manual grants.
    pub fn sync_discord_entitlements(
        &self,
        grants: &[EntitlementGrant],
    ) -> Result<EntitlementSyncResult, StoreError> {
        let mut active = std::collections::BTreeMap::<(String, String), i64>::new();
        for grant in grants {
            let key = (grant.kind.as_str().to_owned(), grant.id.clone());
            active
                .entry(key)
                .and_modify(|expiry| *expiry = (*expiry).max(grant.expires_at))
                .or_insert(grant.expires_at);
        }
        let transaction = self.connection().unchecked_transaction()?;
        let stored = {
            let mut statement =
                transaction.prepare("SELECT kind, target_id FROM discord_premium_entitlement")?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        let stale = stored
            .into_iter()
            .filter(|key| !active.contains_key(key))
            .collect::<Vec<_>>();
        for (kind, target_id) in &stale {
            transaction.execute(
                "DELETE FROM discord_premium_entitlement WHERE kind = ?1 AND target_id = ?2",
                params![kind, target_id],
            )?;
        }
        for ((kind, target_id), expiry) in &active {
            transaction.execute(
                "INSERT INTO discord_premium_entitlement (kind, target_id, expires_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(kind, target_id) DO UPDATE SET expires_at = excluded.expires_at",
                params![kind, target_id, expiry],
            )?;
        }
        transaction.commit()?;
        Ok(EntitlementSyncResult {
            guilds_active: active.keys().filter(|(kind, _)| kind == "guild").count(),
            users_active: active.keys().filter(|(kind, _)| kind == "user").count(),
            revoked: stale.len(),
        })
    }
}

pub(crate) fn record_kofi_transaction_on(
    connection: &Connection,
    transaction_id: &str,
    now: i64,
) -> Result<bool, StoreError> {
    Ok(connection.execute(
        "INSERT OR IGNORE INTO kofi_transaction (transaction_id, processed_at) VALUES (?1, ?2)",
        params![transaction_id, now],
    )? > 0)
}

pub(crate) fn remember_kofi_supporter_on(
    connection: &Connection,
    email_hash: &str,
    discord_id: &str,
    now: i64,
) -> Result<(), StoreError> {
    connection.execute(
            "INSERT INTO kofi_supporter (email_hash, discord_id, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(email_hash) DO UPDATE SET discord_id = excluded.discord_id, updated_at = excluded.updated_at",
            params![email_hash, discord_id, now],
        )?;
    Ok(())
}

pub(crate) fn premium_pass_from(
    connection: &Connection,
    user_id: &str,
) -> Result<Option<PremiumPass>, StoreError> {
    connection
        .query_row(
            "SELECT seats, expires_at, source FROM premium_pass WHERE user_id = ?1",
            [user_id],
            |row| {
                Ok(PremiumPass {
                    seats: row.get(0)?,
                    expires_at: row.get(1)?,
                    source: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::from)
}

fn active_seat_count_from(connection: &Connection, user_id: &str) -> Result<i64, StoreError> {
    connection
        .query_row(
            "SELECT COUNT(*) FROM premium_pass_activation WHERE user_id = ?1",
            [user_id],
            |row| row.get(0),
        )
        .map_err(StoreError::from)
}

pub(crate) fn grant_guild_premium_on(
    connection: &Connection,
    guild_id: &str,
    days: i64,
    source: &str,
    now: i64,
) -> Result<i64, StoreError> {
    let current: Option<i64> = connection
        .query_row(
            "SELECT expires_at FROM premium_guild WHERE guild_id = ?1",
            [guild_id],
            |row| row.get(0),
        )
        .optional()?;
    let expiry = current.filter(|value| *value > now).unwrap_or(now) + days * DAY_MS;
    connection.execute(
        "INSERT INTO premium_guild (guild_id, expires_at, source) VALUES (?1, ?2, ?3)
         ON CONFLICT(guild_id) DO UPDATE SET expires_at = excluded.expires_at, source = excluded.source",
        params![guild_id, expiry, source],
    )?;
    Ok(expiry)
}

pub(crate) fn grant_user_premium_on(
    connection: &Connection,
    user_id: &str,
    days: i64,
    source: &str,
    now: i64,
) -> Result<i64, StoreError> {
    let current: Option<i64> = connection
        .query_row(
            "SELECT expires_at FROM premium_user WHERE user_id = ?1",
            [user_id],
            |row| row.get(0),
        )
        .optional()?;
    let expiry = current.filter(|value| *value > now).unwrap_or(now) + days * DAY_MS;
    connection.execute(
        "INSERT INTO premium_user (user_id, expires_at, source) VALUES (?1, ?2, ?3)
         ON CONFLICT(user_id) DO UPDATE SET expires_at = excluded.expires_at, source = excluded.source",
        params![user_id, expiry, source],
    )?;
    Ok(expiry)
}

pub(crate) fn grant_guild_pass_on(
    connection: &Connection,
    user_id: &str,
    seats: i64,
    days: i64,
    source: &str,
    now: i64,
) -> Result<i64, StoreError> {
    let current = premium_pass_from(connection, user_id)?;
    let expiry = current
        .as_ref()
        .filter(|pass| pass.expires_at > now)
        .map_or(now, |pass| pass.expires_at)
        + days * DAY_MS;
    let final_seats = current
        .filter(|pass| pass.expires_at > now)
        .map_or(seats, |pass| pass.seats.max(seats));
    connection.execute(
        "INSERT INTO premium_pass (user_id, seats, expires_at, source) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(user_id) DO UPDATE SET seats = excluded.seats, expires_at = excluded.expires_at, source = excluded.source",
        params![user_id, final_seats, expiry, source],
    )?;
    Ok(expiry)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;

    #[test]
    fn direct_grants_accumulate_then_restart_after_expiry() {
        let store = SqliteStore::open_in_memory().expect("store");
        let first = store
            .grant_guild_premium("guild", 30, "kofi", NOW)
            .expect("grant");
        assert_eq!(first, NOW + 30 * DAY_MS);
        assert!(store.is_guild_premium("guild", NOW + 1).expect("read"));
        let renewed = store
            .grant_guild_premium("guild", 30, "kofi", NOW + DAY_MS)
            .expect("renew");
        assert_eq!(renewed, NOW + 60 * DAY_MS);
        let restarted = store
            .grant_guild_premium("guild", 30, "kofi", renewed + DAY_MS)
            .expect("restart");
        assert_eq!(restarted, renewed + DAY_MS + 30 * DAY_MS);
    }

    #[test]
    fn plus_considers_direct_vote_and_discord_sources() {
        let store = SqliteStore::open_in_memory().expect("store");
        store
            .connection()
            .execute(
                "INSERT INTO vote_reward (user_id, rewarded_at) VALUES ('user', ?1)",
                [NOW],
            )
            .expect("vote");
        assert!(
            store
                .is_user_premium("user", NOW + 1)
                .expect("vote applies")
        );
        store
            .grant_user_premium("user", 1, "manual", NOW)
            .expect("direct");
        store
            .sync_discord_entitlements(&[EntitlementGrant {
                kind: PremiumKind::User,
                id: "user".into(),
                expires_at: NOW + 10 * DAY_MS,
            }])
            .expect("sync");
        assert_eq!(
            store.user_premium_expiry("user").expect("expiry"),
            Some(NOW + 10 * DAY_MS)
        );
    }

    #[test]
    fn lower_capacity_grants_only_carry_forward_unexpired_seats() {
        for (offset, expected_seats) in [(-1, 5), (0, 2), (1, 2)] {
            let store = SqliteStore::open_in_memory().expect("store");
            let expiry = store
                .grant_guild_pass("owner", 5, 1, "kofi", NOW)
                .expect("old pass");
            for guild in ["a", "b", "c"] {
                assert_eq!(
                    store
                        .activate_seat("owner", guild, NOW)
                        .expect("activate")
                        .status,
                    ActivateStatus::Ok
                );
            }
            let renewal_at = expiry + offset;
            store
                .grant_guild_pass("owner", 2, 1, "redeem", renewal_at)
                .expect("new lower-capacity pass");
            assert_eq!(
                store.premium_pass("owner").expect("pass").unwrap().seats,
                expected_seats
            );
            assert!(store.is_guild_premium("a", renewal_at).expect("first seat"));
            assert!(
                store
                    .is_guild_premium("b", renewal_at)
                    .expect("second seat")
            );
            assert_eq!(
                store
                    .is_guild_premium("c", renewal_at)
                    .expect("old excess seat"),
                expected_seats == 5
            );
            assert_eq!(
                store
                    .activate_seat("owner", "d", renewal_at)
                    .expect("capacity")
                    .status,
                if expected_seats == 5 {
                    ActivateStatus::Ok
                } else {
                    ActivateStatus::NoSeats
                }
            );
        }
    }

    #[test]
    fn pass_seats_are_atomic_idempotent_and_reversible() {
        let store = SqliteStore::open_in_memory().expect("store");
        let expiry = store
            .grant_guild_pass("owner", 2, 30, "kofi", NOW)
            .expect("pass");
        assert_eq!(
            store
                .activate_seat("owner", "a", NOW)
                .expect("activate")
                .status,
            ActivateStatus::Ok
        );
        assert_eq!(
            store
                .activate_seat("owner", "a", NOW)
                .expect("repeat")
                .status,
            ActivateStatus::Already
        );
        assert_eq!(
            store
                .activate_seat("owner", "b", NOW)
                .expect("activate")
                .status,
            ActivateStatus::Ok
        );
        assert_eq!(
            store
                .activate_seat("owner", "c", NOW)
                .expect("limit")
                .status,
            ActivateStatus::NoSeats
        );
        assert!(store.is_guild_premium("a", NOW + 1).expect("premium"));
        assert!(store.deactivate_seat("owner", "a").expect("release"));
        assert!(!store.is_guild_premium("a", NOW + 1).expect("released"));
        assert_eq!(
            store
                .activate_seat("owner", "c", NOW + DAY_MS)
                .expect("reuse")
                .status,
            ActivateStatus::Ok
        );
        assert!(!store.is_guild_premium("c", expiry + 1).expect("expired"));
    }

    #[test]
    fn entitlement_sync_deduplicates_and_only_revokes_discord_rows() {
        let store = SqliteStore::open_in_memory().expect("store");
        let direct = store
            .grant_guild_premium("paid", 1, "redeem", NOW)
            .expect("direct");
        let synced = store
            .sync_discord_entitlements(&[
                EntitlementGrant {
                    kind: PremiumKind::Guild,
                    id: "discord".into(),
                    expires_at: NOW + 10,
                },
                EntitlementGrant {
                    kind: PremiumKind::Guild,
                    id: "discord".into(),
                    expires_at: NOW + 100,
                },
                EntitlementGrant {
                    kind: PremiumKind::User,
                    id: "user".into(),
                    expires_at: NOW + 50,
                },
            ])
            .expect("sync");
        assert_eq!(
            synced,
            EntitlementSyncResult {
                guilds_active: 1,
                users_active: 1,
                revoked: 0
            }
        );
        assert_eq!(
            store
                .effective_guild_premium_expiry("discord", NOW)
                .expect("expiry"),
            Some(NOW + 100)
        );
        let revoked = store.sync_discord_entitlements(&[]).expect("revoke");
        assert_eq!(revoked.revoked, 2);
        assert!(store.is_guild_premium("paid", NOW).expect("direct stays"));
        assert_eq!(
            store
                .effective_guild_premium_expiry("paid", NOW)
                .expect("direct expiry"),
            Some(direct)
        );
    }

    #[test]
    fn selects_the_oldest_active_pass_owner_and_preserves_webhook_idempotency() {
        let store = SqliteStore::open_in_memory().expect("store");
        store
            .grant_guild_pass("first", 3, 30, "kofi", NOW)
            .expect("first pass");
        store
            .grant_guild_pass("second", 3, 30, "kofi", NOW)
            .expect("second pass");
        store
            .activate_seat("first", "shared", NOW)
            .expect("first activation");
        store
            .activate_seat("second", "shared", NOW + 1)
            .expect("second activation");
        assert_eq!(
            store
                .resolve_guild_pass_owner("shared", NOW + 2)
                .expect("owner"),
            Some(GuildPassOwner {
                owner_id: "first".into(),
                seats: 3
            })
        );

        store
            .remember_kofi_supporter("opaque-hmac", "discord-user", NOW)
            .expect("remember");
        assert_eq!(
            store.kofi_supporter("opaque-hmac").expect("lookup"),
            Some("discord-user".into())
        );
        assert!(
            store
                .record_kofi_transaction("transaction", NOW)
                .expect("first delivery")
        );
        assert!(
            !store
                .record_kofi_transaction("transaction", NOW + 1)
                .expect("retry")
        );
    }
}
