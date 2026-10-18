//! Owner-only premium overview and revocation helpers.
//!
//! These operations mirror the original admin-pass contract. They are deliberately kept in the store
//! crate so an eventual Rust HTTP handler cannot accidentally implement a second entitlement
//! policy. Listing is read-only; pass revocation removes the pass and its seat activations in one
//! transaction so no orphan activation can keep a guild premium.

use rusqlite::params;

use crate::{SqliteStore, StoreError};

const ACTIVE_SCAN_CAP: i64 = 2_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminPlusRow {
    pub user_id: String,
    pub expires_at: i64,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminPassRow {
    pub user_id: String,
    pub seats: i64,
    pub used: i64,
    pub expires_at: i64,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AdminPassesView {
    pub plus: Vec<AdminPlusRow>,
    pub passes: Vec<AdminPassRow>,
}

impl SqliteStore {
    /// Lists active direct Plus and multi-seat passes, newest expiry first.
    pub fn list_active_premium(&self, now: i64) -> Result<AdminPassesView, StoreError> {
        let mut plus_statement = self.connection().prepare(
            "WITH sources AS (
                SELECT user_id,expires_at,source FROM premium_user
                UNION ALL SELECT user_id,expires_at,'stripe' FROM stripe_paid_entitlement WHERE plan='plus'
             ) SELECT user_id,MAX(expires_at),source FROM sources
             WHERE expires_at > ?1 GROUP BY user_id ORDER BY MAX(expires_at) DESC LIMIT ?2",
        )?;
        let plus = plus_statement
            .query_map(params![now, ACTIVE_SCAN_CAP], |row| {
                Ok(AdminPlusRow {
                    user_id: row.get(0)?,
                    expires_at: row.get(1)?,
                    source: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut pass_statement = self.connection().prepare(
            &format!("{} SELECT p.user_id, p.seats, p.expires_at, p.source,
                    (SELECT COUNT(*) FROM premium_pass_activation a WHERE a.user_id = p.user_id)
             FROM pass_sources p
             WHERE p.expires_at > ?1 AND NOT EXISTS (
                SELECT 1 FROM pass_sources higher WHERE higher.user_id=p.user_id AND higher.expires_at > ?1
                AND (higher.seats > p.seats OR (higher.seats=p.seats AND higher.expires_at > p.expires_at))
             ) GROUP BY p.user_id ORDER BY p.expires_at DESC LIMIT ?2", crate::premium::PASS_SOURCES),
        )?;
        let passes = pass_statement
            .query_map(params![now, ACTIVE_SCAN_CAP], |row| {
                Ok(AdminPassRow {
                    user_id: row.get(0)?,
                    seats: row.get(1)?,
                    expires_at: row.get(2)?,
                    source: row.get(3)?,
                    used: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(AdminPassesView { plus, passes })
    }

    /// Removes a direct Plus row. Returns whether anything was revoked.
    pub fn revoke_user_premium(&self, user_id: &str) -> Result<bool, StoreError> {
        let transaction = self.connection().unchecked_transaction()?;
        let direct = transaction.execute("DELETE FROM premium_user WHERE user_id=?1", [user_id])?;
        let paid = transaction.execute("UPDATE stripe_paid_entitlement SET expires_at=0 WHERE user_id=?1 AND plan='plus' AND expires_at>0", [user_id])?;
        transaction.commit()?;
        Ok(direct + paid > 0)
    }

    /// Removes a pass and all activations atomically. Returns whether the pass existed.
    pub fn revoke_guild_pass(&self, user_id: &str) -> Result<bool, StoreError> {
        let transaction = self.connection().unchecked_transaction()?;
        let removed =
            transaction.execute("DELETE FROM premium_pass WHERE user_id = ?1", [user_id])?;
        let paid = transaction.execute("UPDATE stripe_paid_entitlement SET expires_at=0 WHERE user_id=?1 AND plan IN ('premium','max') AND expires_at>0", [user_id])?;
        transaction.execute(
            "DELETE FROM premium_pass_activation WHERE user_id = ?1",
            [user_id],
        )?;
        transaction.commit()?;
        Ok(removed + paid > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqliteStore;

    const NOW: i64 = 1_000_000;
    const DAY: i64 = 86_400_000;

    #[test]
    fn lists_active_plus_and_passes_with_used_seats() {
        let store = SqliteStore::open_in_memory().expect("store");
        store
            .grant_user_premium("plus-1", 30, "kofi", NOW)
            .expect("plus");
        store
            .grant_guild_pass("owner-1", 3, 30, "manual", NOW)
            .expect("pass");
        store
            .activate_seat("owner-1", "guild-a", NOW)
            .expect("seat a");
        store
            .activate_seat("owner-1", "guild-b", NOW)
            .expect("seat b");

        assert_eq!(
            store.list_active_premium(NOW).expect("view"),
            AdminPassesView {
                plus: vec![AdminPlusRow {
                    user_id: "plus-1".into(),
                    expires_at: NOW + 30 * DAY,
                    source: "kofi".into()
                }],
                passes: vec![AdminPassRow {
                    user_id: "owner-1".into(),
                    seats: 3,
                    used: 2,
                    expires_at: NOW + 30 * DAY,
                    source: "manual".into()
                }],
            }
        );
    }

    #[test]
    fn excludes_expired_rows_and_revokes_pass_activations() {
        let store = SqliteStore::open_in_memory().expect("store");
        store
            .grant_user_premium("old", 1, "kofi", NOW)
            .expect("plus");
        store
            .grant_guild_pass("owner", 1, 1, "kofi", NOW)
            .expect("pass");
        store.activate_seat("owner", "guild", NOW).expect("seat");
        assert!(
            store
                .list_active_premium(NOW + 5 * DAY)
                .expect("view")
                .plus
                .is_empty()
        );
        assert!(
            store
                .list_active_premium(NOW + 5 * DAY)
                .expect("view")
                .passes
                .is_empty()
        );
        assert!(store.revoke_guild_pass("owner").expect("revoke"));
        assert!(!store.is_guild_premium("guild", NOW).expect("premium"));
        assert!(!store.revoke_guild_pass("owner").expect("missing"));
        assert!(!store.revoke_user_premium("missing").expect("missing"));
        assert!(store.revoke_user_premium("old").expect("revoke plus"));
    }
}
