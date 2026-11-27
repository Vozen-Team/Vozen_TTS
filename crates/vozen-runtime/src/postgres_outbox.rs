//! Background-only delivery of durable SQLite outbox batches to Supabase.
//!
//! No Discord handler waits for this worker. A failed remote write leaves the local batch intact;
//! a successful idempotent insert is acknowledged by removing only that local batch.

use serde::Deserialize;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use sqlx::PgPool;
use vozen_store::{
    GUILD_PURGE_SPECS, MAX_RUNTIME_OUTBOX_PAYLOAD_BYTES, PrivacyPurgeKey, PrivacyPurgeSpec,
    RuntimeBatchBuffer, RuntimeOutboxEnqueue, SqliteStore, StoreError, USER_ERASE_SPECS,
};

const FLUSH_INTERVAL: Duration = Duration::from_secs(5);
const MAX_BATCHES_PER_FLUSH: usize = 100;

pub fn spawn(pool: PgPool, store: Arc<Mutex<SqliteStore>>, buffer: RuntimeBatchBuffer) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(FLUSH_INTERVAL);
        loop {
            interval.tick().await;
            persist_buffer(&store, &buffer).await;
            if let Err(error) = flush_once(&pool, store.clone()).await {
                // Do not include payloads or connection details: both may be sensitive. The
                // batch remains in SQLite and the next interval retries it.
                eprintln!("[postgres] outbox delivery deferred: {error}");
            }
        }
    });
}

async fn persist_buffer(store: &Arc<Mutex<SqliteStore>>, buffer: &RuntimeBatchBuffer) {
    let Some(event) = buffer.drain() else {
        return;
    };
    // An oversized drain is split to fit the outbox. Restoring a batch the outbox can never
    // accept would only make it larger on every interval and stall all later metrics.
    for part in event.split(MAX_RUNTIME_OUTBOX_PAYLOAD_BYTES) {
        let payload = part.payload().to_owned();
        let writer = store.clone();
        let persisted = tokio::task::spawn_blocking(move || {
            writer
                .lock()
                .map_err(|_| None)?
                .enqueue_runtime_outbox(RuntimeOutboxEnqueue {
                    batch_id: &format!("runtime-{}", uuid::Uuid::new_v4()),
                    created_at: unix_now_ms(),
                    payload: &payload,
                })
                .map_err(Some)
        })
        .await;
        match persisted {
            Ok(Ok(())) => {}
            Ok(Err(Some(StoreError::InvalidRuntimeOutboxPayload))) => {
                eprintln!("[postgres] dropped a runtime batch the outbox cannot accept");
            }
            _ => buffer.restore(part),
        }
    }
}

/// Outbox timestamps are Unix milliseconds, matching the replica triggers.
fn unix_now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

async fn flush_once(pool: &PgPool, store: Arc<Mutex<SqliteStore>>) -> Result<(), String> {
    let reader = store.clone();
    let batches = tokio::task::spawn_blocking(move || {
        reader
            .lock()
            .map_err(|_| "SQLite outbox lock was poisoned".to_owned())?
            .list_runtime_outbox(MAX_BATCHES_PER_FLUSH)
            .map_err(|_| "SQLite outbox read failed".to_owned())
    })
    .await
    .map_err(|_| "SQLite outbox worker stopped".to_owned())??;

    for batch in batches {
        deliver_batch(pool, &batch.batch_id, batch.created_at, &batch.payload).await?;

        let acknowledgement = store.clone();
        let batch_id = batch.batch_id;
        tokio::task::spawn_blocking(move || {
            acknowledgement
                .lock()
                .map_err(|_| "SQLite outbox lock was poisoned".to_owned())?
                .delete_runtime_outbox(&batch_id)
                .map_err(|_| "SQLite outbox acknowledgement failed".to_owned())
        })
        .await
        .map_err(|_| "SQLite outbox worker stopped".to_owned())??;
    }
    Ok(())
}

/// Records the raw batch and applies its aggregates exactly once. A process retry after the
/// remote commit sees the id in `runtime_applied_batch` and only acknowledges SQLite locally.
async fn deliver_batch(
    pool: &PgPool,
    batch_id: &str,
    created_at: i64,
    payload: &str,
) -> Result<(), String> {
    let mut transaction = pool
        .begin()
        .await
        .map_err(|_| "Supabase transaction failed".to_owned())?;
    sqlx::query(
        "INSERT INTO vozen.runtime_outbox_batch (batch_id, created_at, payload)
         VALUES ($1, $2, $3::jsonb)
         ON CONFLICT (batch_id) DO NOTHING",
    )
    .bind(batch_id)
    .bind(created_at)
    .bind(payload)
    .execute(&mut *transaction)
    .await
    .map_err(|_| "Supabase outbox write failed".to_owned())?;
    let applied = sqlx::query_scalar::<_, String>(
        "INSERT INTO vozen.runtime_applied_batch (batch_id, applied_at)
         VALUES ($1, $2)
         ON CONFLICT (batch_id) DO NOTHING
         RETURNING batch_id",
    )
    .bind(batch_id)
    .bind(created_at)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(|_| "Supabase batch idempotency write failed".to_owned())?
    .is_some();
    if applied {
        materialize_aggregates(&mut transaction, payload).await?;
        advance_live_generation(&mut transaction).await?;
    }
    // The applied marker is the idempotency evidence. The raw payload is transient so personal
    // rows cannot remain indefinitely in Supabase after materialization or privacy erasure.
    sqlx::query("DELETE FROM vozen.runtime_outbox_batch WHERE batch_id = $1")
        .bind(batch_id)
        .execute(&mut *transaction)
        .await
        .map_err(|_| "Supabase outbox retention cleanup failed".to_owned())?;
    transaction
        .commit()
        .await
        .map_err(|_| "Supabase batch commit failed".to_owned())
}

/// Advances the live mirror checkpoint in the same transaction as the applied batch. The
/// initial import fingerprint remains immutable; voice-cache refreshes use this marker to reject
/// snapshots that raced a mirror write.
async fn advance_live_generation(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), String> {
    sqlx::query(
        "UPDATE vozen.runtime_migration_state
            SET generation = $1,
                completed_at = EXTRACT(EPOCH FROM NOW())::bigint * 1000
          WHERE marker = 'sqlite_initial_import_v1'",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .execute(&mut **transaction)
    .await
    .map_err(|_| "Supabase live mirror checkpoint update failed".to_owned())?;
    Ok(())
}

async fn materialize_aggregates(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    payload: &str,
) -> Result<(), String> {
    let document: BatchDocument = serde_json::from_str(payload)
        .map_err(|_| "SQLite outbox contains invalid JSON".to_owned())?;
    if document.replica.is_some() {
        sqlx::query("SELECT vozen.apply_replica_event(($1::jsonb)->'replica')")
            .bind(payload)
            .execute(&mut **transaction)
            .await
            .map_err(|_| "Supabase durable replica write failed".to_owned())?;
        return Ok(());
    }
    if let Some(privacy) = document.privacy {
        apply_privacy_tombstone(transaction, &privacy).await?;
        return Ok(());
    }
    let mut metrics = BTreeMap::<(String, String, String), i64>::new();
    let mut usage = BTreeMap::<(String, String, String, String), i64>::new();
    let mut stats = BTreeMap::<(String, String, String), i64>::new();
    let mut guild_days = BTreeMap::<(String, String), i64>::new();
    for row in document.metrics {
        *metrics
            .entry((row.day, row.metric, row.provider))
            .or_default() += row.value;
    }
    for row in document.speech {
        let language = row.model.split('-').next().unwrap_or(&row.model).to_owned();
        *usage
            .entry((
                row.guild_id.clone(),
                row.user_id.clone(),
                language,
                row.engine,
            ))
            .or_default() += row.value;
        *stats
            .entry((row.guild_id.clone(), row.user_id.clone(), row.day.clone()))
            .or_default() += row.value;
        *guild_days.entry((row.guild_id, row.day)).or_default() += 1;
    }
    for ((day, metric, provider), value) in metrics {
        sqlx::query(
            "INSERT INTO vozen.operational_daily_metric (day, metric, provider, value)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (day, metric, provider) DO UPDATE
             SET value = vozen.operational_daily_metric.value + EXCLUDED.value",
        )
        .bind(day)
        .bind(metric)
        .bind(provider)
        .bind(value)
        .execute(&mut **transaction)
        .await
        .map_err(|_| "Supabase metric batch write failed".to_owned())?;
    }
    for ((guild_id, user_id, language, engine), value) in usage {
        sqlx::query(
            "INSERT INTO vozen.talk_usage (guild_id, user_id, language, engine, spoken_count)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (guild_id, user_id, language, engine) DO UPDATE
             SET spoken_count = vozen.talk_usage.spoken_count + EXCLUDED.spoken_count",
        )
        .bind(guild_id)
        .bind(user_id)
        .bind(language)
        .bind(engine)
        .bind(value)
        .execute(&mut **transaction)
        .await
        .map_err(|error| format!("Supabase usage batch write failed: {error}"))?;
    }
    for ((guild_id, user_id, day), value) in stats {
        sqlx::query(
            "INSERT INTO vozen.talk_stats (guild_id, user_id, spoken_count, streak, best_streak, last_date)
             VALUES ($1, $2, $3, 1, 1, $4)
             ON CONFLICT (guild_id, user_id) DO UPDATE SET
               spoken_count = vozen.talk_stats.spoken_count + EXCLUDED.spoken_count,
               streak = CASE
                 WHEN vozen.talk_stats.last_date > EXCLUDED.last_date THEN vozen.talk_stats.streak
                 WHEN vozen.talk_stats.last_date = EXCLUDED.last_date THEN vozen.talk_stats.streak
                 WHEN vozen.talk_stats.last_date::date >= EXCLUDED.last_date::date - 2
                  AND vozen.talk_stats.last_date::date < EXCLUDED.last_date::date
                   THEN vozen.talk_stats.streak + 1
                 ELSE 1 END,
               best_streak = GREATEST(vozen.talk_stats.best_streak,
                 CASE WHEN vozen.talk_stats.last_date > EXCLUDED.last_date THEN vozen.talk_stats.streak
                      WHEN vozen.talk_stats.last_date = EXCLUDED.last_date THEN vozen.talk_stats.streak
                      WHEN vozen.talk_stats.last_date::date >= EXCLUDED.last_date::date - 2
                       AND vozen.talk_stats.last_date::date < EXCLUDED.last_date::date
                        THEN vozen.talk_stats.streak + 1
                      ELSE 1 END),
               last_date = GREATEST(vozen.talk_stats.last_date, EXCLUDED.last_date)",
        )
        .bind(&guild_id).bind(&user_id).bind(value).bind(&day)
        .execute(&mut **transaction).await
        .map_err(|_| "Supabase talk statistics batch write failed".to_owned())?;
    }
    for ((guild_id, day), _) in guild_days {
        sqlx::query(
            "INSERT INTO vozen.guild_talk_streak (guild_id, streak, best_streak, last_date)
             VALUES ($1, 1, 1, $2)
             ON CONFLICT (guild_id) DO UPDATE SET
               streak = CASE
                 WHEN vozen.guild_talk_streak.last_date > EXCLUDED.last_date THEN vozen.guild_talk_streak.streak
                 WHEN vozen.guild_talk_streak.last_date = EXCLUDED.last_date THEN vozen.guild_talk_streak.streak
                 WHEN vozen.guild_talk_streak.last_date::date >= EXCLUDED.last_date::date - 2
                  AND vozen.guild_talk_streak.last_date::date < EXCLUDED.last_date::date
                   THEN vozen.guild_talk_streak.streak + 1 ELSE 1 END,
               best_streak = GREATEST(vozen.guild_talk_streak.best_streak,
                 CASE WHEN vozen.guild_talk_streak.last_date > EXCLUDED.last_date THEN vozen.guild_talk_streak.streak
                      WHEN vozen.guild_talk_streak.last_date = EXCLUDED.last_date THEN vozen.guild_talk_streak.streak
                      WHEN vozen.guild_talk_streak.last_date::date >= EXCLUDED.last_date::date - 2
                       AND vozen.guild_talk_streak.last_date::date < EXCLUDED.last_date::date
                        THEN vozen.guild_talk_streak.streak + 1 ELSE 1 END),
               last_date = GREATEST(vozen.guild_talk_streak.last_date, EXCLUDED.last_date)",
        )
        .bind(guild_id).bind(day)
        .execute(&mut **transaction).await
        .map_err(|_| "Supabase guild streak batch write failed".to_owned())?;
    }
    Ok(())
}

async fn apply_privacy_tombstone(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    privacy: &PrivacyRow,
) -> Result<(), String> {
    let specs: &[PrivacyPurgeSpec] = match privacy.scope.as_str() {
        "user" => USER_ERASE_SPECS,
        "guild" => GUILD_PURGE_SPECS,
        _ => return Err("invalid privacy tombstone scope".to_owned()),
    };
    for spec in specs {
        sqlx::query(&privacy_delete_statement(*spec))
            .bind(&privacy.id)
            .execute(&mut **transaction)
            .await
            .map_err(|_| "Supabase privacy deletion failed".to_owned())?;
    }
    sqlx::query(
        "DELETE FROM vozen.runtime_outbox_batch
         WHERE payload #>> '{privacy,id}' = $1
            OR payload #>> '{replica,row,user_id}' = $1
            OR payload #>> '{replica,row,guild_id}' = $1",
    )
    .bind(&privacy.id)
    .execute(&mut **transaction)
    .await
    .map_err(|_| "Supabase privacy outbox cleanup failed".to_owned())?;
    Ok(())
}

fn privacy_delete_statement(spec: PrivacyPurgeSpec) -> String {
    let predicate = match spec.key {
        PrivacyPurgeKey::UserId => "user_id = $1",
        PrivacyPurgeKey::DiscordId => "discord_id = $1",
        PrivacyPurgeKey::GcloudPersonalKey => "key = $1 AND scope IN ('user', 'pass')",
        PrivacyPurgeKey::GuildId => "guild_id = $1",
    };
    format!("DELETE FROM vozen.\"{}\" WHERE {predicate}", spec.table)
}

#[derive(Debug, Deserialize)]
struct BatchDocument {
    #[serde(default)]
    replica: Option<serde_json::Value>,
    #[serde(default)]
    privacy: Option<PrivacyRow>,
    #[serde(default)]
    metrics: Vec<MetricRow>,
    #[serde(default)]
    speech: Vec<SpeechRow>,
}

#[derive(Debug, Deserialize)]
struct PrivacyRow {
    scope: String,
    id: String,
}

#[derive(Debug, Deserialize)]
struct MetricRow {
    day: String,
    metric: String,
    provider: String,
    value: i64,
}

#[derive(Debug, Deserialize)]
struct SpeechRow {
    day: String,
    guild_id: String,
    user_id: String,
    model: String,
    engine: String,
    value: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use vozen_store::RuntimeOutboxEnqueue;

    #[test]
    fn runtime_batches_are_stamped_in_milliseconds_like_replica_rows() {
        let expected = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis() as i64;
        assert!((unix_now_ms() - expected).abs() < 5_000);
    }

    #[tokio::test]
    async fn staging_delivery_when_explicitly_requested() {
        let Ok(database_url) = std::env::var("VOZEN_POSTGRES_INTEGRATION_URL") else {
            return;
        };
        let pool = PgPool::connect(&database_url)
            .await
            .expect("staging pool must connect");
        let batch_id = format!("staging-outbox-{}", uuid::Uuid::new_v4());
        let guild_id = format!("staging-guild-{}", uuid::Uuid::new_v4());
        let user_id = format!("staging-user-{}", uuid::Uuid::new_v4());
        let store = Arc::new(Mutex::new(
            SqliteStore::open_in_memory().expect("local outbox"),
        ));
        store
            .lock()
            .expect("store")
            .enqueue_runtime_outbox(RuntimeOutboxEnqueue {
                batch_id: &batch_id,
                created_at: 1,
                payload: &format!(
                    r#"{{"version":1,"metrics":[{{"day":"2099-01-01","metric":"synth_success","provider":"piper","value":2}}],"speech":[{{"day":"2099-01-01","guild_id":"{guild_id}","user_id":"{user_id}","model":"pt_PT-test","engine":"piper","value":2}}]}}"#
                ),
            })
            .expect("enqueue");

        flush_once(&pool, store.clone()).await.expect("flush");
        assert!(
            store
                .lock()
                .expect("store")
                .list_runtime_outbox(1)
                .expect("outbox")
                .is_empty()
        );
        let found: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM vozen.runtime_outbox_batch WHERE batch_id = $1)",
        )
        .bind(&batch_id)
        .fetch_one(&pool)
        .await
        .expect("remote batch");
        assert!(found);
        let metric: i32 = sqlx::query_scalar(
            "SELECT value FROM vozen.operational_daily_metric
             WHERE day = '2099-01-01' AND metric = 'synth_success' AND provider = 'piper'",
        )
        .fetch_one(&pool)
        .await
        .expect("materialized metric");
        assert!(metric >= 2);
        let usage: i32 = sqlx::query_scalar(
            "SELECT spoken_count FROM vozen.talk_usage
             WHERE guild_id = $1 AND user_id = $2 AND language = 'pt_PT' AND engine = 'piper'",
        )
        .bind(&guild_id)
        .bind(&user_id)
        .fetch_one(&pool)
        .await
        .expect("materialized usage");
        assert_eq!(usage, 2);
        sqlx::query("DELETE FROM vozen.talk_stats WHERE guild_id = $1 AND user_id = $2")
            .bind(&guild_id)
            .bind(&user_id)
            .execute(&pool)
            .await
            .expect("cleanup talk stats");
        sqlx::query("DELETE FROM vozen.talk_usage WHERE guild_id = $1 AND user_id = $2")
            .bind(&guild_id)
            .bind(&user_id)
            .execute(&pool)
            .await
            .expect("cleanup talk usage");
        sqlx::query(
            "DELETE FROM vozen.operational_daily_metric
             WHERE day = '2099-01-01' AND metric = 'synth_success' AND provider = 'piper'",
        )
        .execute(&pool)
        .await
        .expect("cleanup metric");
        sqlx::query("DELETE FROM vozen.runtime_outbox_batch WHERE batch_id = $1")
            .bind(&batch_id)
            .execute(&pool)
            .await
            .expect("cleanup staging batch");
        sqlx::query("DELETE FROM vozen.runtime_applied_batch WHERE batch_id = $1")
            .bind(&batch_id)
            .execute(&pool)
            .await
            .expect("cleanup idempotency marker");
    }

    #[tokio::test]
    async fn staging_replica_events_apply_insert_update_and_delete_when_explicitly_requested() {
        let Ok(database_url) = std::env::var("VOZEN_POSTGRES_INTEGRATION_URL") else {
            return;
        };
        let pool = PgPool::connect(&database_url)
            .await
            .expect("staging pool must connect");
        let guild_id = format!("staging-replica-guild-{}", uuid::Uuid::new_v4());
        let store = Arc::new(Mutex::new(
            SqliteStore::open_in_memory().expect("local replica store"),
        ));
        store
            .lock()
            .expect("store")
            .enable_postgres_replica_outbox()
            .expect("enable local replica triggers");

        store
            .lock()
            .expect("store")
            .update_guild_config(
                &guild_id,
                vozen_store::GuildConfigPatch {
                    locale: Some("pt".into()),
                    rate_per_min: Some(11),
                    ..vozen_store::GuildConfigPatch::default()
                },
            )
            .expect("insert fixture");
        flush_once(&pool, store.clone())
            .await
            .expect("flush insert");
        let locale: String =
            sqlx::query_scalar("SELECT locale FROM vozen.guild_config WHERE guild_id = $1")
                .bind(&guild_id)
                .fetch_one(&pool)
                .await
                .expect("replicated insert");
        assert_eq!(locale, "pt");

        store
            .lock()
            .expect("store")
            .update_guild_config(
                &guild_id,
                vozen_store::GuildConfigPatch {
                    rate_per_min: Some(12),
                    ..vozen_store::GuildConfigPatch::default()
                },
            )
            .expect("update fixture");
        flush_once(&pool, store.clone())
            .await
            .expect("flush update");
        let rate_per_min: i32 =
            sqlx::query_scalar("SELECT rate_per_min FROM vozen.guild_config WHERE guild_id = $1")
                .bind(&guild_id)
                .fetch_one(&pool)
                .await
                .expect("replicated update");
        assert_eq!(rate_per_min, 12);

        store
            .lock()
            .expect("store")
            .reset_guild_config(&guild_id)
            .expect("delete fixture");
        flush_once(&pool, store.clone())
            .await
            .expect("flush delete");
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM vozen.guild_config WHERE guild_id = $1)",
        )
        .bind(&guild_id)
        .fetch_one(&pool)
        .await
        .expect("replicated delete");
        assert!(!exists);

        sqlx::query(
            "DELETE FROM vozen.runtime_applied_batch
             WHERE batch_id IN (
               SELECT batch_id FROM vozen.runtime_outbox_batch
               WHERE payload #>> '{replica,row,guild_id}' = $1
             )",
        )
        .bind(&guild_id)
        .execute(&pool)
        .await
        .expect("cleanup replica idempotency markers");
        sqlx::query(
            "DELETE FROM vozen.runtime_outbox_batch
             WHERE payload #>> '{replica,row,guild_id}' = $1",
        )
        .bind(&guild_id)
        .execute(&pool)
        .await
        .expect("cleanup replica batches");
    }
}
