//! One-time state records for the server-side Discord installation flow.

use rusqlite::params;

use crate::{SqliteStore, StoreError};

impl SqliteStore {
    /// Registers only a digest of the signed browser state. The raw state and
    /// Discord code never enter SQLite or logs.
    pub fn register_install_oauth_state(
        &self,
        state_hash: &str,
        expires_at: i64,
    ) -> Result<(), StoreError> {
        self.connection().execute(
            "INSERT INTO install_oauth_state(state_hash,expires_at,used_at) VALUES(?1,?2,NULL)",
            params![state_hash, expires_at],
        )?;
        Ok(())
    }

    /// Atomically consumes a valid one-time state. Replays and expired states
    /// are indistinguishable to callers and therefore safe to report.
    pub fn consume_install_oauth_state(
        &self,
        state_hash: &str,
        now: i64,
    ) -> Result<bool, StoreError> {
        let changed = self.connection().execute(
            "UPDATE install_oauth_state SET used_at=?2 WHERE state_hash=?1 AND used_at IS NULL AND expires_at>=?2",
            params![state_hash, now],
        )?;
        Ok(changed == 1)
    }

    /// Counts unconsumed states that can still be redeemed, so the public start
    /// route can refuse new rows once the replay table reaches its bound.
    pub fn count_pending_install_oauth_states(&self, now: i64) -> Result<usize, StoreError> {
        let count: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM install_oauth_state WHERE used_at IS NULL AND expires_at >= ?1",
            [now],
            |row| row.get(0),
        )?;
        Ok(usize::try_from(count).unwrap_or(usize::MAX))
    }

    pub fn purge_install_oauth_states(&self, now: i64) -> Result<usize, StoreError> {
        Ok(self.connection().execute(
            "DELETE FROM install_oauth_state WHERE expires_at < ?1 OR used_at IS NOT NULL",
            [now],
        )?)
    }
}

#[cfg(test)]
mod tests {
    use crate::SqliteStore;

    #[test]
    fn state_is_single_use_and_expires() {
        let store = SqliteStore::open_in_memory().expect("store");
        store
            .register_install_oauth_state("digest", 2_000)
            .expect("register");
        assert!(
            store
                .consume_install_oauth_state("digest", 1_000)
                .expect("consume")
        );
        assert!(
            !store
                .consume_install_oauth_state("digest", 1_001)
                .expect("replay")
        );
        store
            .register_install_oauth_state("expired", 999)
            .expect("register");
        assert!(
            !store
                .consume_install_oauth_state("expired", 1_000)
                .expect("expired")
        );
    }

    #[test]
    fn pending_count_excludes_used_and_expired_states() {
        let store = SqliteStore::open_in_memory().expect("store");
        for (hash, expires_at) in [("live", 2_000), ("used", 2_000), ("expired", 999)] {
            store
                .register_install_oauth_state(hash, expires_at)
                .expect("register");
        }
        assert!(
            store
                .consume_install_oauth_state("used", 1_000)
                .expect("consume")
        );
        assert_eq!(
            store
                .count_pending_install_oauth_states(1_000)
                .expect("count"),
            1
        );
    }
}
