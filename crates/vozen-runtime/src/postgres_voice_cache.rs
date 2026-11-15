//! Staging-only Postgres-backed read cache for the automatic voice path.
//!
//! The Discord handler reads a local in-memory SQLite snapshot. A background task refreshes that
//! snapshot from the private Postgres schema; it never performs network I/O from a message task.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use sqlx::PgPool;
use thiserror::Error;
use vozen_store::SqliteStore;

const REFRESH_INTERVAL: Duration = Duration::from_secs(15);
const MAX_SNAPSHOT_AGE: Duration = Duration::from_secs(60);
const MAX_CONSECUTIVE_FAILURES: u32 = 3;
const IMPORT_MARKER: &str = "sqlite_initial_import_v1";
const VOICE_CACHE_TABLES: &[&str] = &[
    "blocklist",
    "channel_profile",
    "discord_premium_entitlement",
    "guild_config",
    "premium_guild",
    "premium_pass",
    "premium_pass_activation",
    "premium_user",
    "pronunciation",
    "pronunciation_user",
    "stt_daily_usage",
    "tts_lang_detect_on",
    "tts_optout",
    "user_effect",
    "user_voice",
];

#[derive(Debug, Error)]
pub enum PostgresVoiceCacheError {
    #[error("Postgres initial import marker is missing")]
    MissingInitialImport,
    #[error("Postgres initial import marker is incomplete")]
    InvalidImportMarker,
    #[error("Postgres voice-cache generation changed during refresh")]
    GenerationMismatch,
    #[error("Postgres voice-cache query failed")]
    Postgres(#[source] sqlx::Error),
    #[error("Postgres voice-cache payload is invalid")]
    Payload(#[source] serde_json::Error),
    #[error("in-memory voice-cache store failed: {0}")]
    Store(#[from] vozen_store::StoreError),
    #[error("in-memory voice-cache lock was poisoned")]
    Lock,
}

/// Builds a fully populated, local-only cache. It fails closed unless the explicit staging import
/// marker exists, avoiding a silent switch to an empty remote snapshot.
pub async fn load(pool: &PgPool) -> Result<Arc<Mutex<SqliteStore>>, PostgresVoiceCacheError> {
    let marker: Option<(Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT generation, fingerprint
           FROM vozen.runtime_migration_state
          WHERE marker = $1",
    )
    .bind(IMPORT_MARKER)
    .fetch_optional(pool)
    .await
    .map_err(PostgresVoiceCacheError::Postgres)?;
    let Some((generation, fingerprint)) = marker else {
        return Err(PostgresVoiceCacheError::MissingInitialImport);
    };
    // The immutable fingerprint is an attestation written only by a complete import. It is
    // deliberately not compared with live rows: mirror batches are expected to change them.
    require_import_marker(generation.as_deref(), fingerprint.as_deref())?;
    let store = Arc::new(Mutex::new(SqliteStore::open_in_memory()?));
    refresh_once_with_generation(pool, &store, generation.unwrap()).await?;
    Ok(store)
}

/// Runs one consistent remote read, then replaces local tables while holding no network resource.
pub async fn refresh_once(
    pool: &PgPool,
    store: &Arc<Mutex<SqliteStore>>,
) -> Result<(), PostgresVoiceCacheError> {
    let generation: Option<Option<String>> = sqlx::query_scalar(
        "SELECT generation FROM vozen.runtime_migration_state WHERE marker = $1",
    )
    .bind(IMPORT_MARKER)
    .fetch_optional(pool)
    .await
    .map_err(PostgresVoiceCacheError::Postgres)?;
    let Some(generation) = generation else {
        return Err(PostgresVoiceCacheError::MissingInitialImport);
    };
    let Some(generation) = generation else {
        return Err(PostgresVoiceCacheError::InvalidImportMarker);
    };
    if generation.is_empty() {
        return Err(PostgresVoiceCacheError::InvalidImportMarker);
    }
    refresh_once_with_generation(pool, store, generation).await
}

async fn refresh_once_with_generation(
    pool: &PgPool,
    store: &Arc<Mutex<SqliteStore>>,
    expected_generation: String,
) -> Result<(), PostgresVoiceCacheError> {
    let mut transaction = pool
        .begin()
        .await
        .map_err(PostgresVoiceCacheError::Postgres)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *transaction)
        .await
        .map_err(PostgresVoiceCacheError::Postgres)?;
    let snapshot_generation: Option<String> = sqlx::query_scalar(
        "SELECT generation FROM vozen.runtime_migration_state WHERE marker = $1",
    )
    .bind(IMPORT_MARKER)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(PostgresVoiceCacheError::Postgres)?;
    require_generation(&expected_generation, snapshot_generation.as_deref())?;
    let mut snapshots = Vec::with_capacity(VOICE_CACHE_TABLES.len());
    for table in VOICE_CACHE_TABLES {
        let query = format!(
            "SELECT COALESCE(jsonb_agg(to_jsonb(row)), '[]'::jsonb)::text
             FROM vozen.\"{table}\" AS row"
        );
        let payload: String = sqlx::query_scalar(&query)
            .fetch_one(&mut *transaction)
            .await
            .map_err(PostgresVoiceCacheError::Postgres)?;
        let rows = serde_json::from_str::<Vec<serde_json::Value>>(&payload)
            .map_err(PostgresVoiceCacheError::Payload)?;
        snapshots.push(((*table).to_owned(), rows));
    }
    // A repeatable-read transaction gives all tables one snapshot. Compare the marker again
    // after commit so a batch committed while the read was in flight cannot replace local state.
    transaction
        .commit()
        .await
        .map_err(PostgresVoiceCacheError::Postgres)?;

    let current_generation: Option<String> = sqlx::query_scalar(
        "SELECT generation FROM vozen.runtime_migration_state WHERE marker = $1",
    )
    .bind(IMPORT_MARKER)
    .fetch_optional(pool)
    .await
    .map_err(PostgresVoiceCacheError::Postgres)?;
    replace_snapshot_if_fresh(
        store,
        snapshots,
        &expected_generation,
        current_generation.as_deref(),
    )
}

fn require_import_marker(
    generation: Option<&str>,
    fingerprint: Option<&str>,
) -> Result<(), PostgresVoiceCacheError> {
    if generation.is_none_or(str::is_empty) || fingerprint.is_none_or(str::is_empty) {
        return Err(PostgresVoiceCacheError::InvalidImportMarker);
    }
    Ok(())
}

fn require_generation(
    expected: &str,
    observed: Option<&str>,
) -> Result<(), PostgresVoiceCacheError> {
    if observed != Some(expected) {
        return Err(PostgresVoiceCacheError::GenerationMismatch);
    }
    Ok(())
}

fn replace_snapshot_if_fresh(
    store: &Arc<Mutex<SqliteStore>>,
    snapshots: Vec<(String, Vec<serde_json::Value>)>,
    expected_generation: &str,
    observed_generation: Option<&str>,
) -> Result<(), PostgresVoiceCacheError> {
    // Validate before taking the lock or deleting any local rows. A failed/racing refresh therefore
    // leaves the last known-good cache untouched.
    require_generation(expected_generation, observed_generation)?;
    let store = store.lock().map_err(|_| PostgresVoiceCacheError::Lock)?;
    for (table, rows) in snapshots {
        store.replace_contract_table_rows(&table, &rows)?;
    }
    Ok(())
}

/// Tracks whether the local Postgres snapshot is recent enough to serve voice reads. Once refreshes
/// keep failing or the snapshot ages past the bound, callers read the authoritative SQLite store
/// instead, so blocklist and premium edits cannot be served stale indefinitely.
#[derive(Clone)]
pub struct VoiceCacheFreshness {
    state: Arc<Mutex<FreshnessState>>,
}

struct FreshnessState {
    last_success: Instant,
    consecutive_failures: u32,
}

impl Default for VoiceCacheFreshness {
    /// Starts fresh: callers create this right after a successful initial `load`.
    fn default() -> Self {
        Self::new_at(Instant::now())
    }
}

impl VoiceCacheFreshness {
    fn new_at(now: Instant) -> Self {
        Self {
            state: Arc::new(Mutex::new(FreshnessState {
                last_success: now,
                consecutive_failures: 0,
            })),
        }
    }

    fn record_success_at(&self, now: Instant) {
        if let Ok(mut state) = self.state.lock() {
            state.last_success = now;
            state.consecutive_failures = 0;
        }
    }

    fn record_failure(&self) -> u32 {
        self.state
            .lock()
            .map(|mut state| {
                state.consecutive_failures = state.consecutive_failures.saturating_add(1);
                state.consecutive_failures
            })
            .unwrap_or(u32::MAX)
    }

    pub fn is_fresh(&self) -> bool {
        self.is_fresh_at(Instant::now())
    }

    fn is_fresh_at(&self, now: Instant) -> bool {
        // A poisoned lock fails closed to the authoritative store.
        self.state.lock().is_ok_and(|state| {
            state.consecutive_failures < MAX_CONSECUTIVE_FAILURES
                && now.saturating_duration_since(state.last_success) <= MAX_SNAPSHOT_AGE
        })
    }
}

pub fn spawn(pool: PgPool, store: Arc<Mutex<SqliteStore>>, freshness: VoiceCacheFreshness) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(REFRESH_INTERVAL);
        loop {
            interval.tick().await;
            match refresh_once(&pool, &store).await {
                Ok(()) => {
                    if !freshness.is_fresh() {
                        eprintln!("[postgres] voice-cache refreshed; serving Postgres snapshot");
                    }
                    freshness.record_success_at(Instant::now());
                }
                Err(error) => {
                    // Keep the last known-good local cache for a bounded time; past that bound,
                    // voice reads fall back to the authoritative SQLite store.
                    let failures = freshness.record_failure();
                    eprintln!("[postgres] voice-cache refresh deferred: {error}");
                    if failures == MAX_CONSECUTIVE_FAILURES {
                        eprintln!("[postgres] voice-cache stale; voice reads use local SQLite");
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use vozen_store::GuildConfigPatch;

    #[test]
    fn snapshot_goes_stale_after_repeated_failures_or_age_and_recovers() {
        let start = Instant::now();
        let freshness = VoiceCacheFreshness::new_at(start);
        assert!(freshness.is_fresh_at(start + MAX_SNAPSHOT_AGE));
        assert!(!freshness.is_fresh_at(start + MAX_SNAPSHOT_AGE + Duration::from_secs(1)));
        for _ in 1..MAX_CONSECUTIVE_FAILURES {
            freshness.record_failure();
            assert!(freshness.is_fresh_at(start));
        }
        freshness.record_failure();
        assert!(!freshness.is_fresh_at(start));
        let later = start + Duration::from_secs(120);
        freshness.record_success_at(later);
        assert!(freshness.is_fresh_at(later));
    }

    #[test]
    fn malformed_import_markers_fail_closed() {
        assert!(matches!(
            require_import_marker(None, Some("fingerprint")),
            Err(PostgresVoiceCacheError::InvalidImportMarker)
        ));
        assert!(matches!(
            require_import_marker(Some("generation"), Some("")),
            Err(PostgresVoiceCacheError::InvalidImportMarker)
        ));
    }

    #[test]
    fn generation_races_fail_closed_and_preserve_last_known_good_cache() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("cache")));
        let guild_id = "known-good";
        store
            .lock()
            .expect("cache lock")
            .update_guild_config(
                guild_id,
                GuildConfigPatch {
                    locale: Some("pt".into()),
                    rate_per_min: Some(11),
                    ..GuildConfigPatch::default()
                },
            )
            .expect("seed cache");
        let mut changed = store
            .lock()
            .expect("cache lock")
            .export_table_rows("guild_config")
            .expect("export cache");
        changed[0]["rate_per_min"] = serde_json::json!(99);
        let result = replace_snapshot_if_fresh(
            &store,
            vec![("guild_config".into(), changed)],
            "generation-a",
            Some("generation-b"),
        );
        assert!(matches!(
            result,
            Err(PostgresVoiceCacheError::GenerationMismatch)
        ));
        assert_eq!(
            store
                .lock()
                .expect("cache lock")
                .guild_config(guild_id)
                .expect("known-good config")
                .rate_per_min,
            11
        );
    }

    #[tokio::test]
    async fn staging_voice_cache_loads_only_after_explicit_import_when_requested() {
        let Ok(database_url) = std::env::var("VOZEN_POSTGRES_INTEGRATION_URL") else {
            return;
        };
        let pool = PgPool::connect(&database_url).await.expect("staging pool");
        let guild_id = format!("staging-cache-guild-{}", uuid::Uuid::new_v4());
        sqlx::query(
            "INSERT INTO vozen.guild_config (guild_id, locale, rate_per_min)
             VALUES ($1, 'pt', 11)",
        )
        .bind(&guild_id)
        .execute(&pool)
        .await
        .expect("stage cache fixture");
        sqlx::query(
            "INSERT INTO vozen.runtime_migration_state (marker, completed_at, generation, fingerprint)
             VALUES ('sqlite_initial_import_v1', 1, 'staging-fixture', 'initial-attestation')
             ON CONFLICT (marker) DO UPDATE SET completed_at = EXCLUDED.completed_at,
                                                generation = EXCLUDED.generation,
                                                fingerprint = EXCLUDED.fingerprint",
        )
        .execute(&pool)
        .await
        .expect("stage marker");
        let verification = async {
            let cache = load(&pool).await.map_err(|error| error.to_string())?;
            {
                let cache = cache.lock().map_err(|_| "cache lock".to_owned())?;
                let config = cache
                    .guild_config(&guild_id)
                    .map_err(|error| error.to_string())?;
                if config.locale != "pt" || config.rate_per_min != 11 {
                    return Err("initial Postgres voice-cache value did not match".to_owned());
                }
                if !cache
                    .has_schema_object("premium_user")
                    .map_err(|error| error.to_string())?
                {
                    return Err("voice-cache schema missing premium_user".to_owned());
                }
            }
            sqlx::query("UPDATE vozen.guild_config SET rate_per_min = 12 WHERE guild_id = $1")
                .bind(&guild_id)
                .execute(&pool)
                .await
                .map_err(|error| error.to_string())?;
            refresh_once(&pool, &cache)
                .await
                .map_err(|error| error.to_string())?;
            let cache = cache.lock().map_err(|_| "cache lock".to_owned())?;
            if cache
                .guild_config(&guild_id)
                .map_err(|error| error.to_string())?
                .rate_per_min
                != 12
            {
                return Err("refreshed Postgres voice-cache value did not match".to_owned());
            }
            Ok::<(), String>(())
        }
        .await;
        sqlx::query("DELETE FROM vozen.guild_config WHERE guild_id = $1")
            .bind(&guild_id)
            .execute(&pool)
            .await
            .expect("remove cache fixture");
        sqlx::query(
            "DELETE FROM vozen.runtime_migration_state WHERE marker = 'sqlite_initial_import_v1'",
        )
        .execute(&pool)
        .await
        .expect("remove fixture marker");
        verification.expect("verify Postgres voice-cache refresh");
    }
}
