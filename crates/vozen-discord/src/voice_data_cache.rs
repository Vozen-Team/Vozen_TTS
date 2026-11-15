//! Short-lived durable-data cache for the automatic voice path.
//!
//! The cache never holds Discord permission or presence state.  Those values remain input to
//! every gateway event.  Its only job is to keep repeated messages from waiting on the global
//! compatibility SQLite mutex while the PostgreSQL store rollout is being enabled.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use vozen_store::{SqliteStore, StoreError};

use crate::{MessageAdmissionData, VoicePreparationData};

const CACHE_TTL: Duration = Duration::from_secs(15);
const MAX_ENTRIES: usize = 4_096;

#[derive(Debug, Clone)]
pub(crate) struct VoiceDataSnapshot {
    pub admission: MessageAdmissionData,
    pub preparation: VoicePreparationData,
}

impl VoiceDataSnapshot {
    /// SQLite is authoritative until the explicit Postgres cutover. Replication lag must
    /// neither ignore a newly enabled reading option nor undo a disable/opt-out/role gate.
    pub(crate) fn refresh_live_settings(
        &mut self,
        store: &SqliteStore,
        guild_id: &str,
        channel_id: &str,
        user_id: &str,
    ) -> Result<(), StoreError> {
        let guild = store.guild_config(guild_id)?;
        let profile = store.channel_profile(guild_id, channel_id)?;
        self.admission = MessageAdmissionData {
            guild: guild.clone(),
            profile: profile.clone(),
            opted_out: store.is_opted_out(guild_id, user_id)?,
        };
        self.preparation.guild = guild;
        self.preparation.profile = profile;
        self.preparation.user_voice = store.get_user_voice(guild_id, user_id)?;
        // Moderation must not lag the replica: a newly blocked word applies to the next message.
        self.preparation.blocklist = store.get_blocklist(guild_id)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    guild_id: String,
    channel_id: String,
    user_id: String,
}

struct CachedSnapshot {
    expires_at: Instant,
    value: VoiceDataSnapshot,
}

#[derive(Default)]
pub(crate) struct VoiceDataCache {
    entries: Mutex<HashMap<CacheKey, CachedSnapshot>>,
}

impl VoiceDataCache {
    pub(crate) fn snapshot(
        &self,
        store: &Arc<Mutex<SqliteStore>>,
        guild_id: &str,
        channel_id: &str,
        user_id: &str,
        now_ms: i64,
    ) -> Result<VoiceDataSnapshot, StoreError> {
        let key = CacheKey {
            guild_id: guild_id.to_owned(),
            channel_id: channel_id.to_owned(),
            user_id: user_id.to_owned(),
        };
        if let Ok(entries) = self.entries.lock()
            && let Some(cached) = entries.get(&key)
            && cached.expires_at > Instant::now()
        {
            return Ok(cached.value.clone());
        }

        // A miss is the only time this path takes the legacy compatibility lock.  All queries
        // are collected under the same lock so a single message can never interleave a partial
        // configuration update with a preparation snapshot.
        let value = {
            let store = store.lock().map_err(|_| StoreError::CacheUnavailable)?;
            load_snapshot(&store, guild_id, channel_id, user_id, now_ms)?
        };
        if let Ok(mut entries) = self.entries.lock() {
            entries.retain(|_, cached| cached.expires_at > Instant::now());
            if entries.len() >= MAX_ENTRIES {
                entries.clear();
            }
            entries.insert(
                key,
                CachedSnapshot {
                    expires_at: Instant::now() + CACHE_TTL,
                    value: value.clone(),
                },
            );
        }
        Ok(value)
    }

    pub(crate) fn forget_guild(&self, guild_id: &str) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.retain(|key, _| key.guild_id != guild_id);
        }
    }
}

fn load_snapshot(
    store: &SqliteStore,
    guild_id: &str,
    channel_id: &str,
    user_id: &str,
    now_ms: i64,
) -> Result<VoiceDataSnapshot, StoreError> {
    let guild = store.guild_config(guild_id)?;
    let profile = store.channel_profile(guild_id, channel_id)?;
    let opted_out = store.is_opted_out(guild_id, user_id)?;
    let admission = MessageAdmissionData {
        guild: guild.clone(),
        profile: profile.clone(),
        opted_out,
    };
    let preparation = VoicePreparationData {
        guild,
        profile,
        blocklist: store.get_blocklist(guild_id)?,
        user_voice: store.get_user_voice(guild_id, user_id)?,
        user_pronunciations: store.get_user_pronunciations(user_id)?,
        server_pronunciations: store.get_server_pronunciations(guild_id)?,
        detection_enabled: store.is_detection_on(guild_id, user_id)?,
        personal_effect: store.voice_effect(guild_id, user_id)?,
        user_premium: store.is_user_premium(user_id, now_ms)?,
        guild_pass_owner: store.resolve_guild_pass_owner(guild_id, now_ms)?,
        guild_premium: store.is_guild_premium(guild_id, now_ms)?,
    };
    Ok(VoiceDataSnapshot {
        admission,
        preparation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use vozen_store::GuildConfigPatch;

    #[test]
    fn live_refresh_reads_blocklist_from_the_authoritative_store() {
        let replica = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("replica")));
        let authority = SqliteStore::open_in_memory().expect("authority");
        let cache = VoiceDataCache::default();
        let mut snapshot = cache
            .snapshot(&replica, "guild", "channel", "user", 1)
            .expect("replica snapshot");
        assert!(snapshot.preparation.blocklist.is_empty());
        authority.add_blockword("guild", "secret").expect("block");
        snapshot
            .refresh_live_settings(&authority, "guild", "channel", "user")
            .expect("refresh");
        assert_eq!(snapshot.preparation.blocklist, vec!["secret".to_owned()]);
    }

    #[test]
    fn returns_a_cached_snapshot_until_the_ttl_or_explicit_invalidation() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        let cache = VoiceDataCache::default();
        let first = cache
            .snapshot(&store, "guild", "channel", "user", 1)
            .expect("first");
        assert_eq!(first.preparation.guild.rate_per_min, 8);
        store
            .lock()
            .expect("store")
            .update_guild_config(
                "guild",
                GuildConfigPatch {
                    rate_per_min: Some(99),
                    ..GuildConfigPatch::default()
                },
            )
            .expect("update");
        assert_eq!(
            cache
                .snapshot(&store, "guild", "channel", "user", 1)
                .expect("cached")
                .preparation
                .guild
                .rate_per_min,
            8
        );
        cache.forget_guild("guild");
        assert_eq!(
            cache
                .snapshot(&store, "guild", "channel", "user", 1)
                .expect("refreshed")
                .preparation
                .guild
                .rate_per_min,
            99
        );
    }
}
