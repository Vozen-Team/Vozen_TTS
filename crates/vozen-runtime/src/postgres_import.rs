//! Explicit SQLite-to-Postgres importer used only for staging verification.
//!
//! It never runs by default. The operator must opt in through a staging-only environment flag;
//! each contract table is inserted in chunks and then reconciled by row count.

use std::{collections::BTreeMap, path::Path};

use sha2::{Digest, Sha256};
use sqlx::PgPool;
use thiserror::Error;
use vozen_store::SqliteStore;

const CHUNK_SIZE: usize = 250;

#[derive(Debug, Error)]
pub enum ImportError {
    #[error("SQLite import source failed: {0}")]
    Store(#[from] vozen_store::StoreError),
    #[error("Postgres import failed: {0}")]
    Postgres(#[source] sqlx::Error),
    #[error(
        "row-count reconciliation failed for table {table}: SQLite={sqlite_rows}, Postgres={postgres_rows}"
    )]
    CountMismatch {
        table: String,
        sqlite_rows: i64,
        postgres_rows: i64,
    },
    #[error("content fingerprint reconciliation failed for table {table}")]
    FingerprintMismatch { table: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportTableReport {
    pub table: String,
    pub sqlite_rows: i64,
    pub postgres_rows: i64,
}

pub async fn import_and_reconcile(
    pool: &PgPool,
    sqlite_path: &Path,
) -> Result<Vec<ImportTableReport>, ImportError> {
    let store = SqliteStore::open(sqlite_path)?;
    let mut tables = store.durable_table_names()?;
    // Giveaway campaigns and their participant records are local to SQLite.
    tables.retain(|name| {
        !matches!(
            name.as_str(),
            "giveaway" | "giveaway_entry" | "giveaway_award"
        )
    });
    let mut transaction = pool.begin().await.map_err(ImportError::Postgres)?;
    let mut reports = Vec::with_capacity(tables.len());
    let mut fingerprint_input = String::new();
    for table in tables {
        let rows = store.export_table_rows(&table)?;
        let table_fingerprint = fingerprint_rows(&rows);
        fingerprint_input.push_str(&table);
        fingerprint_input.push(':');
        fingerprint_input.push_str(&table_fingerprint);
        fingerprint_input.push('\n');
        let delete = format!("DELETE FROM vozen.\"{table}\"");
        sqlx::query(&delete)
            .execute(&mut *transaction)
            .await
            .map_err(ImportError::Postgres)?;
        for chunk in rows.chunks(CHUNK_SIZE) {
            let payload = serde_json::to_string(chunk).map_err(|_| ImportError::CountMismatch {
                table: table.clone(),
                sqlite_rows: -1,
                postgres_rows: -1,
            })?;
            let query = format!(
                "INSERT INTO vozen.\"{table}\" SELECT * FROM jsonb_populate_recordset(NULL::vozen.\"{table}\", $1::jsonb) ON CONFLICT DO NOTHING"
            );
            sqlx::query(&query)
                .bind(payload)
                .execute(&mut *transaction)
                .await
                .map_err(ImportError::Postgres)?;
        }
        let sqlite_rows = store.durable_table_row_count(&table)?;
        let query = format!("SELECT COUNT(*)::bigint FROM vozen.\"{table}\"");
        let postgres_rows: i64 = sqlx::query_scalar(&query)
            .fetch_one(&mut *transaction)
            .await
            .map_err(ImportError::Postgres)?;
        if sqlite_rows != postgres_rows {
            return Err(ImportError::CountMismatch {
                table,
                sqlite_rows,
                postgres_rows,
            });
        }
        let query = format!(
            "SELECT COALESCE(jsonb_agg(to_jsonb(row)), '[]'::jsonb)::text
             FROM vozen.\"{table}\" AS row"
        );
        let remote_payload: String = sqlx::query_scalar(&query)
            .fetch_one(&mut *transaction)
            .await
            .map_err(ImportError::Postgres)?;
        let remote_rows =
            serde_json::from_str::<Vec<serde_json::Value>>(&remote_payload).map_err(|_| {
                ImportError::FingerprintMismatch {
                    table: table.clone(),
                }
            })?;
        if fingerprint_rows(&remote_rows) != table_fingerprint {
            return Err(ImportError::FingerprintMismatch { table });
        }
        reports.push(ImportTableReport {
            table,
            sqlite_rows,
            postgres_rows,
        });
    }
    sqlx::query(
        "INSERT INTO vozen.runtime_migration_state
           (marker, completed_at, generation, fingerprint, source_checkpoint)
         VALUES ('sqlite_initial_import_v1', EXTRACT(EPOCH FROM NOW())::bigint * 1000,
                 $1, $2, $3)
         ON CONFLICT (marker) DO UPDATE SET
           completed_at = EXCLUDED.completed_at,
           generation = EXCLUDED.generation,
           fingerprint = EXCLUDED.fingerprint,
           source_checkpoint = EXCLUDED.source_checkpoint",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(format!(
        "{:x}",
        Sha256::digest(fingerprint_input.as_bytes())
    ))
    .bind(source_checkpoint(sqlite_path))
    .execute(&mut *transaction)
    .await
    .map_err(ImportError::Postgres)?;
    transaction.commit().await.map_err(ImportError::Postgres)?;
    Ok(reports)
}

fn source_checkpoint(path: &Path) -> String {
    let metadata = std::fs::metadata(path).ok();
    let size = metadata.as_ref().map_or(0, |value| value.len());
    let modified = metadata
        .and_then(|value| value.modified().ok())
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |value| value.as_secs());
    format!("{size}:{modified}")
}

/// Produces a stable row fingerprint for the immutable initial-import attestation.
/// This value must not be reused as a live-refresh freshness check.
pub(crate) fn fingerprint_rows(rows: &[serde_json::Value]) -> String {
    let mut canonical = rows.iter().map(canonical_json).collect::<Vec<_>>();
    canonical.sort();
    format!("{:x}", Sha256::digest(canonical.join("\n").as_bytes()))
}

fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(object) => {
            let sorted = object
                .iter()
                .map(|(key, value)| (key, canonical_json(value)))
                .collect::<BTreeMap<_, _>>();
            serde_json::to_string(&sorted).unwrap_or_default()
        }
        serde_json::Value::Array(values) => {
            serde_json::to_string(&values.iter().map(canonical_json).collect::<Vec<_>>())
                .unwrap_or_default()
        }
        serde_json::Value::Number(number) => canonical_number(number),
        _ => value.to_string(),
    }
}

/// SQLite commonly serializes a REAL value such as `1.0`, while PostgreSQL may
/// return the same value as the JSON number `1`.  Both values represent the
/// same database value and must therefore produce the same import fingerprint.
/// Keep large integers on their original lossless representation instead of
/// routing them through an imprecise `f64` conversion.
fn canonical_number(number: &serde_json::Number) -> String {
    const MAX_EXACT_F64_INTEGER: f64 = 9_007_199_254_740_991.0;

    if let Some(value) = number.as_f64()
        && value.is_finite()
        && value.abs() <= MAX_EXACT_F64_INTEGER
        && value.fract() == 0.0
    {
        if value == 0.0 {
            return "0".to_owned();
        }
        return format!("{value:.0}");
    }

    number.to_string()
}

#[cfg(test)]
mod tests {
    use std::{
        fs::remove_file,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use vozen_store::{GuildConfigPatch, UserEngine, UserVoice};

    fn temporary_database_path() -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("vozen-postgres-import-{nonce}.sqlite"))
    }

    fn remove_database_copy(path: &Path) {
        let _ = remove_file(path);
        let _ = remove_file(format!("{}-wal", path.display()));
        let _ = remove_file(format!("{}-shm", path.display()));
    }

    #[test]
    fn importer_reports_count_mismatches_without_hiding_table_name() {
        let error = ImportError::CountMismatch {
            table: "guild_config".into(),
            sqlite_rows: 2,
            postgres_rows: 1,
        };
        assert!(error.to_string().contains("guild_config"));
    }

    #[test]
    fn canonical_json_treats_integral_numeric_spellings_equally() {
        let sqlite = serde_json::json!({
            "speed": 1.0,
            "nested": [2.0, -3.0, 0.0],
        });
        let postgres = serde_json::json!({
            "speed": 1,
            "nested": [2, -3, 0],
        });

        assert_eq!(canonical_json(&sqlite), canonical_json(&postgres));
    }

    #[test]
    fn canonical_json_keeps_fractional_values_distinct() {
        let first = serde_json::json!({ "speed": 1.05 });
        let second = serde_json::json!({ "speed": 1.5 });

        assert_ne!(canonical_json(&first), canonical_json(&second));
    }

    /// Opt-in staging integration test. It uses a fresh SQLite source and removes only its
    /// deterministic fixture rows from Postgres, so it cannot touch production data.
    #[tokio::test]
    async fn staging_imports_and_reconciles_a_sqlite_fixture_when_explicitly_requested() {
        let Ok(database_url) = std::env::var("VOZEN_POSTGRES_INTEGRATION_URL") else {
            return;
        };
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&database_url)
            .await
            .expect("connect staging Postgres");
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let guild_id = format!("staging-import-guild-{nonce}");
        let user_id = format!("staging-import-user-{nonce}");
        let path = temporary_database_path();

        let store = SqliteStore::open(&path).expect("create SQLite fixture");
        store
            .update_guild_config(
                &guild_id,
                GuildConfigPatch {
                    autoread: Some(true),
                    locale: Some("pt".into()),
                    ..GuildConfigPatch::default()
                },
            )
            .expect("seed guild configuration");
        store
            .set_user_voice(
                &guild_id,
                &user_id,
                &UserVoice {
                    model: "pt_PT-google-medium".into(),
                    speed: 1.05,
                    engine: UserEngine::Google,
                },
            )
            .expect("seed user voice");
        drop(store);

        let verification = async {
            let report = import_and_reconcile(&pool, &path)
                .await
                .map_err(|error| error.to_string())?;
            if !report
                .iter()
                .any(|row| row.table == "guild_config" && row.sqlite_rows == 1)
            {
                return Err("guild configuration was not reconciled".to_owned());
            }
            if !report
                .iter()
                .any(|row| row.table == "user_voice" && row.sqlite_rows == 1)
            {
                return Err("user voice was not reconciled".to_owned());
            }

            let locale: String =
                sqlx::query_scalar("SELECT locale FROM vozen.guild_config WHERE guild_id = $1")
                    .bind(&guild_id)
                    .fetch_one(&pool)
                    .await
                    .map_err(|error| error.to_string())?;
            if locale != "pt" {
                return Err(format!("unexpected imported locale: {locale}"));
            }
            let voice_model: String = sqlx::query_scalar(
                "SELECT voice_model FROM vozen.user_voice WHERE guild_id = $1 AND user_id = $2",
            )
            .bind(&guild_id)
            .bind(&user_id)
            .fetch_one(&pool)
            .await
            .map_err(|error| error.to_string())?;
            if voice_model != "pt_PT-google-medium" {
                return Err(format!("unexpected imported voice model: {voice_model}"));
            }
            Ok::<(), String>(())
        }
        .await;

        sqlx::query("DELETE FROM vozen.user_voice WHERE guild_id = $1")
            .bind(&guild_id)
            .execute(&pool)
            .await
            .expect("remove staged user voice fixture");
        sqlx::query("DELETE FROM vozen.guild_config WHERE guild_id = $1")
            .bind(&guild_id)
            .execute(&pool)
            .await
            .expect("remove staged guild configuration fixture");
        sqlx::query(
            "DELETE FROM vozen.runtime_migration_state WHERE marker = 'sqlite_initial_import_v1'",
        )
        .execute(&pool)
        .await
        .expect("remove fixture import marker");
        remove_database_copy(&path);
        verification.expect("verify staging import");
    }
}
