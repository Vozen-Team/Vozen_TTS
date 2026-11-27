//! Legacy Ko-fi purchases that were never attributed to a Discord account.
//!
//! Vozen no longer sells through Ko-fi, so nothing writes or claims these rows any more. The
//! table stays readable for the owner's admin overview (an unclaimed purchase can still be
//! granted by hand) and keeps its retention sweep so the remaining rows expire on schedule.
//! The email value is an opaque HMAC key and is never returned in clear text.

use crate::{SqliteStore, StoreError};

pub const ADMIN_PENDING_SCAN_CAP: i64 = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KofiPendingPlan {
    Premium,
    Plus,
}

impl KofiPendingPlan {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Premium => "premium",
            Self::Plus => "plus",
        }
    }

    fn from_database(value: String) -> Result<Self, StoreError> {
        match value.as_str() {
            "premium" => Ok(Self::Premium),
            "plus" => Ok(Self::Plus),
            _ => Err(StoreError::InvalidPremiumCodePlan(value)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KofiPendingGrantInput {
    pub transaction_id: String,
    /// HMAC(email), never a clear-text email.
    pub email_hash: Option<String>,
    pub plan: KofiPendingPlan,
    pub days: i64,
    pub seats: i64,
    /// Shop purchases are false; membership payments are true.
    pub is_subscription: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KofiPendingGrant {
    pub input: KofiPendingGrantInput,
    pub created_at: i64,
    pub claimed_at: Option<i64>,
}

type KofiPendingRow = (
    String,
    Option<String>,
    String,
    i64,
    i64,
    i64,
    Option<i64>,
    i64,
);

impl SqliteStore {
    /// Bounded owner-only overview, ordered newest first. It never returns a clear-text email.
    pub fn all_unclaimed_kofi_pending(
        &self,
        cap: i64,
    ) -> Result<Vec<KofiPendingGrant>, StoreError> {
        let cap = cap.clamp(1, ADMIN_PENDING_SCAN_CAP);
        let mut statement = self.connection().prepare(
            "SELECT transaction_id, email_hash, plan, days, seats, created_at, claimed_at, is_subscription
             FROM kofi_pending WHERE claimed_at IS NULL ORDER BY created_at DESC LIMIT ?1",
        )?;
        statement
            .query_map([cap], pending_from_row)?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(pending_from_raw)
            .collect()
    }

    /// Removes claimed and unclaimed pending records before the caller-provided cutoff.
    pub fn purge_old_kofi_pending(&self, cutoff: i64) -> Result<usize, StoreError> {
        Ok(self
            .connection()
            .execute("DELETE FROM kofi_pending WHERE created_at < ?1", [cutoff])?)
    }
}

fn pending_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<KofiPendingRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

fn pending_from_raw(
    (transaction_id, email_hash, plan, days, seats, created_at, claimed_at, is_subscription): KofiPendingRow,
) -> Result<KofiPendingGrant, StoreError> {
    Ok(KofiPendingGrant {
        input: KofiPendingGrantInput {
            transaction_id,
            email_hash,
            plan: KofiPendingPlan::from_database(plan)?,
            days,
            seats,
            is_subscription: is_subscription == 1,
        },
        created_at,
        claimed_at,
    })
}

#[cfg(test)]
mod tests {
    use rusqlite::params;

    use super::*;

    const NOW: i64 = 1_000_000;

    fn insert(store: &SqliteStore, transaction_id: &str, created_at: i64, claimed: bool) {
        store
            .connection()
            .execute(
                "INSERT INTO kofi_pending
                 (transaction_id, email_hash, plan, days, seats, created_at, claimed_at, is_subscription)
                 VALUES (?1, 'opaque-hmac', 'plus', 30, 0, ?2, ?3, 0)",
                params![transaction_id, created_at, claimed.then_some(created_at + 1)],
            )
            .expect("insert legacy pending row");
    }

    #[test]
    fn overview_lists_only_unclaimed_rows_newest_first() {
        let store = SqliteStore::open_in_memory().expect("store");
        insert(&store, "old", NOW, false);
        insert(&store, "claimed", NOW + 1, true);
        insert(&store, "new", NOW + 2, false);
        let pending = store.all_unclaimed_kofi_pending(10).expect("overview");
        assert_eq!(
            pending
                .iter()
                .map(|grant| grant.input.transaction_id.as_str())
                .collect::<Vec<_>>(),
            vec!["new", "old"]
        );
        assert_eq!(pending[0].input.plan, KofiPendingPlan::Plus);
        assert_eq!(pending[0].claimed_at, None);
    }

    #[test]
    fn retention_sweep_removes_claimed_and_unclaimed_rows_before_the_cutoff() {
        let store = SqliteStore::open_in_memory().expect("store");
        insert(&store, "one", NOW, false);
        insert(&store, "two", NOW + 1, true);
        insert(&store, "three", NOW + 2, false);
        assert_eq!(store.purge_old_kofi_pending(NOW + 2).expect("purge"), 2);
        assert_eq!(
            store
                .all_unclaimed_kofi_pending(10)
                .expect("remaining")
                .into_iter()
                .map(|grant| grant.input.transaction_id)
                .collect::<Vec<_>>(),
            vec!["three"]
        );
    }
}
