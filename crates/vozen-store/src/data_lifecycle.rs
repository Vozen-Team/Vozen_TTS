//! Explicit data-retention boundaries shared with the Node `dataLifecycle` contract.
//!
//! These arrays are deliberately hand-reviewed rather than inferred from column names: paid
//! entitlements and financial/idempotency evidence may carry a Discord identifier but are not
//! erased by a privacy request. Adding a new user/guild table requires an explicit decision.

use crate::{SqliteStore, StoreError};

pub const GUILD_PURGE_TABLES: &[&str] = &[
    "user_voice",
    "guild_config",
    "blocklist",
    "pronunciation",
    "tts_optout",
    "tts_lang_detect_on",
    "user_nickname",
    "game_score",
    "user_birthday",
    "talk_stats",
    "talk_usage",
    "guild_talk_streak",
    "vote_promo_state",
    "user_effect",
    "voice_presence",
    "stt_consent",
    "guild_departed",
    "translation_mapping",
    "translation_preference",
    "translation_daily_usage",
    "translation_user_daily_usage",
    "channel_profile",
];

pub const USER_ERASE_TABLES: &[&str] = &[
    "user_voice",
    "user_voice_favorite",
    "user_voice_recent",
    "tts_optout",
    "tts_lang_detect_on",
    "user_nickname",
    "game_score",
    "user_birthday",
    "talk_stats",
    "talk_usage",
    "user_effect",
    "user_abbreviation",
    "pronunciation_user",
    "stt_consent",
    "vote_reward",
    "translation_preference",
    "translation_user_daily_usage",
];

impl SqliteStore {
    /// Removes a departed server's configuration/content/statistics in one transaction. Paid
    /// memberships/pass activations deliberately remain: deleting them would revoke a purchase
    /// or free a paid seat.
    pub fn purge_guild_data(&self, guild_id: &str) -> Result<(), StoreError> {
        let transaction = self.connection().unchecked_transaction()?;
        for table in GUILD_PURGE_TABLES {
            transaction.execute(
                &format!("DELETE FROM {table} WHERE guild_id = ?1"),
                [guild_id],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Erases a user's personal data in every guild, atomically. It intentionally retains
    /// premium entitlement/payment evidence and HMAC-only anti-abuse ledgers. `talk_usage` is
    /// included despite being aggregate because it is scoped to a user and a guild.
    pub fn erase_user_data(&self, user_id: &str) -> Result<(), StoreError> {
        let transaction = self.connection().unchecked_transaction()?;
        for table in USER_ERASE_TABLES {
            transaction.execute(
                &format!("DELETE FROM {table} WHERE user_id = ?1"),
                [user_id],
            )?;
        }
        // Identifier columns which intentionally differ from `user_id`.
        transaction.execute(
            "DELETE FROM kofi_supporter WHERE discord_id = ?1",
            [user_id],
        )?;
        self.erase_personal_gcloud_on(
            &transaction,
            user_id,
            &crate::month_key_utc(crate::gcloud_usage::current_time_ms()),
        )?;
        transaction.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privacy_erase_removes_personal_rows_but_retains_paid_rights() {
        let store = SqliteStore::open_in_memory().expect("store");
        store
            .set_nickname("guild", "user", "Rexy")
            .expect("nickname");
        store
            .remember_voice_presence("guild", "voice", 1)
            .expect("presence");
        store.connection().execute(
            "INSERT INTO talk_usage (guild_id, user_id, language, engine, spoken_count) VALUES ('guild', 'user', 'en', 'piper', 1)",
            [],
        ).expect("usage");
        store
            .connection()
            .execute(
                "INSERT INTO premium_user (user_id, expires_at) VALUES ('user', 999)",
                [],
            )
            .expect("paid entitlement");

        store.erase_user_data("user").expect("erase");
        assert_eq!(store.nickname("guild", "user").expect("nickname"), None);
        let usage: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM talk_usage WHERE user_id = 'user'",
                [],
                |row| row.get(0),
            )
            .expect("usage count");
        assert_eq!(usage, 0);
        let entitlement: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM premium_user WHERE user_id = 'user'",
                [],
                |row| row.get(0),
            )
            .expect("entitlement count");
        assert_eq!(entitlement, 1);
        // Voice presence has no user identifier and remains until the server/session lifecycle
        // removes it; a privacy erase must not disconnect every member of a shared call.
        assert_eq!(store.voice_presences().expect("presence").len(), 1);
    }

    #[test]
    fn guild_purge_removes_shared_content_but_keeps_paid_pass_state() {
        let store = SqliteStore::open_in_memory().expect("store");
        store
            .set_nickname("guild", "user", "Rexy")
            .expect("nickname");
        store
            .connection()
            .execute(
                "INSERT INTO premium_guild (guild_id, expires_at) VALUES ('guild', 999)",
                [],
            )
            .expect("paid guild entitlement");
        store.purge_guild_data("guild").expect("purge");
        assert_eq!(store.nickname("guild", "user").expect("nickname"), None);
        let entitlement: i64 = store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM premium_guild WHERE guild_id = 'guild'",
                [],
                |row| row.get(0),
            )
            .expect("entitlement count");
        assert_eq!(entitlement, 1);
    }
}
