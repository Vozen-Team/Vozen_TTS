//! Idempotent upgrades for production SQLite files created by older Node releases.
//!
//! The generated schema contract intentionally represents the current schema only. SQLite's
//! `CREATE TABLE IF NOT EXISTS` does not add missing columns, therefore a Rust process must run
//! these historical transformations before it can safely open an existing production database.

use std::collections::HashSet;

use rusqlite::{Connection, params};

use crate::{StoreError, utc_day_key_from_unix_millis};

const DAY_MS: i64 = 24 * 60 * 60 * 1_000;

const GUILD_CONFIG_COLUMNS: &[(&str, &str)] = &[
    ("tts_channel_id", "TEXT"),
    ("autoread", "INTEGER NOT NULL DEFAULT 0"),
    ("default_voice", "TEXT NOT NULL DEFAULT 'en_US-amy-medium'"),
    ("max_chars", "INTEGER NOT NULL DEFAULT 300"),
    ("rate_per_min", "INTEGER NOT NULL DEFAULT 8"),
    ("enabled", "INTEGER NOT NULL DEFAULT 1"),
    ("tts_role_id", "TEXT"),
    ("locale", "TEXT NOT NULL DEFAULT 'en'"),
    ("xsaid", "INTEGER NOT NULL DEFAULT 1"),
    ("autojoin", "INTEGER NOT NULL DEFAULT 0"),
    ("read_bots", "INTEGER NOT NULL DEFAULT 0"),
    ("text_in_voice", "INTEGER NOT NULL DEFAULT 1"),
    ("greet_on_join", "INTEGER NOT NULL DEFAULT 1"),
    ("greet_locale", "TEXT NOT NULL DEFAULT 'en'"),
    ("antispam", "INTEGER NOT NULL DEFAULT 0"),
    ("stay_in_call", "INTEGER NOT NULL DEFAULT 0"),
    ("streak_announce", "INTEGER NOT NULL DEFAULT 1"),
    ("soundboard", "INTEGER NOT NULL DEFAULT 1"),
    ("vote_promos", "INTEGER NOT NULL DEFAULT 0"),
    ("priority_role_id", "TEXT"),
    ("blocked_role_id", "TEXT"),
    ("translation_enabled", "INTEGER NOT NULL DEFAULT 0"),
    (
        "translation_daily_char_limit",
        "INTEGER NOT NULL DEFAULT 10000",
    ),
    (
        "translation_per_user_daily_char_limit",
        "INTEGER NOT NULL DEFAULT 2000",
    ),
];

const CHANNEL_PROFILE_COLUMNS: &[(&str, &str)] = &[
    ("engine", "TEXT"),
    ("speed", "REAL"),
    ("max_chars", "INTEGER"),
    ("read_bots", "INTEGER CHECK (read_bots IN (0, 1))"),
    ("voice_channel_id", "TEXT"),
    ("locale", "TEXT"),
    ("effect", "TEXT"),
];

const TOPGG_SYNC_STATE_COLUMNS: &[(&str, &str)] =
    &[("last_detail", "TEXT NOT NULL DEFAULT 'unknown'")];

pub(crate) fn migrate_legacy_schema(connection: &Connection) -> Result<(), StoreError> {
    // Removed voice cloning stored biometric-consent metadata. This deletion is deliberately
    // irreversible and mirrors the privacy purge in Node before a Rust cutover is permitted.
    // `vote_redemption` was the old lifetime HMAC marker. The current promotion has a 30-day
    // rolling cap and `vote_reward_ledger` is purged on that same window, so retaining the
    // legacy marker would be unnecessary pseudonymous personal data.
    connection
        .execute_batch("DROP TABLE IF EXISTS user_clone; DROP TABLE IF EXISTS vote_redemption;")?;

    add_missing_columns(connection, "guild_config", GUILD_CONFIG_COLUMNS)?;
    add_missing_columns(
        connection,
        "user_voice",
        &[("engine", "TEXT NOT NULL DEFAULT 'google'")],
    )?;
    add_missing_columns(
        connection,
        "translation_preference",
        &[("speak_locale", "TEXT")],
    )?;
    add_missing_columns(connection, "channel_profile", CHANNEL_PROFILE_COLUMNS)?;
    add_missing_columns(
        connection,
        "vote_promo_state",
        &[(
            "last_kind",
            "TEXT NOT NULL DEFAULT 'vote' CHECK (last_kind IN ('vote', 'support'))",
        )],
    )?;
    add_missing_columns(
        connection,
        "kofi_pending",
        &[("is_subscription", "INTEGER NOT NULL DEFAULT 0")],
    )?;
    add_missing_columns(connection, "topgg_sync_state", TOPGG_SYNC_STATE_COLUMNS)?;
    add_missing_columns(
        connection,
        "giveaway_vote",
        &[("bot_id", "TEXT NOT NULL DEFAULT '1523826014935842997'")],
    )?;

    // Older releases retained provider delivery IDs for 30 days but did not expose an
    // identity-free vote series. Seed the durable aggregate from those existing rows. MAX makes
    // this safe on every startup and never reduces counts after raw replay rows are purged.
    connection.execute_batch(
        "INSERT INTO growth_daily_metric (day, product, source, event, value)
         SELECT strftime('%Y-%m-%d', processed_at / 1000, 'unixepoch'),
                'tts', 'topgg', 'vote', COUNT(*)
         FROM topgg_webhook_event
         WHERE processed_at >= 0
         GROUP BY strftime('%Y-%m-%d', processed_at / 1000, 'unixepoch')
         ON CONFLICT(day, product, source, event)
         DO UPDATE SET value = MAX(growth_daily_metric.value, excluded.value);",
    )?;

    backfill_growth_setup_from_first_value(connection)?;
    backfill_growth_retention(connection)?;

    let supporter_columns = table_columns(connection, "kofi_supporter")?;
    if supporter_columns.contains("email") && !supporter_columns.contains("email_hash") {
        // The values in old rows are intentionally no longer usable as an email lookup key; they
        // become inert until Ko-fi supplies a new HMACed payment email.
        connection
            .execute_batch("ALTER TABLE kofi_supporter RENAME COLUMN email TO email_hash;")?;
    }

    connection.execute_batch(
        "UPDATE user_voice SET voice_model = 'pt_PT-google-medium'
         WHERE voice_model = 'pt_PT-tugao-medium';
         UPDATE guild_config SET default_voice = 'pt_PT-google-medium'
         WHERE default_voice = 'pt_PT-tugao-medium';
         DROP TABLE IF EXISTS tts_lang_detect_off;
         CREATE TABLE IF NOT EXISTS tts_lang_detect_on (
           guild_id TEXT NOT NULL,
           user_id TEXT NOT NULL,
           PRIMARY KEY (guild_id, user_id)
         );",
    )?;
    // This table stores one aggregate total per UTC day for the private owner panel. It contains
    // no member, username or guild identifiers and is intentionally additive/idempotent.
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS admin_member_daily_total (
           day TEXT PRIMARY KEY,
           member_count INTEGER NOT NULL CHECK (member_count >= 0),
           captured_at INTEGER NOT NULL CHECK (captured_at >= 0)
         );",
    )?;
    Ok(())
}

/// A successful TTS playback cannot happen before the server is configured. Early lifecycle
/// releases recorded the playback but did not persist the corresponding setup event, which made
/// the aggregate funnel look impossible. Infer only this mechanical prerequisite and use the
/// existing playback timestamp/source; the NULL guard makes this repair safe on every startup.
fn backfill_growth_setup_from_first_value(connection: &Connection) -> Result<(), StoreError> {
    let candidates = {
        let mut statement = connection.prepare(
            "SELECT guild_id, first_value_at, install_source
             FROM guild_growth_lifecycle
             WHERE setup_completed_at IS NULL AND first_value_at IS NOT NULL",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };

    for (guild_id, first_value_at, source) in candidates {
        let changed = connection.execute(
            "UPDATE guild_growth_lifecycle
             SET setup_completed_at = ?2
             WHERE guild_id = ?1 AND setup_completed_at IS NULL",
            params![guild_id, first_value_at],
        )?;
        if changed == 0 {
            continue;
        }
        connection.execute(
            "INSERT INTO growth_daily_metric(day, product, source, event, value)
             VALUES (?1, 'tts', ?2, 'setup_completed', 1)
             ON CONFLICT(day, product, source, event)
             DO UPDATE SET value = value + 1",
            params![utc_day_key_from_unix_millis(first_value_at), source],
        )?;
    }
    Ok(())
}

fn backfill_growth_retention(connection: &Connection) -> Result<(), StoreError> {
    let candidates = {
        let mut statement = connection.prepare(
            "SELECT guild_id, first_value_at, last_active_at, install_source
             FROM guild_growth_lifecycle
             WHERE install_source <> 'baseline'
               AND first_value_at IS NOT NULL
               AND last_active_at IS NOT NULL",
        )?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };

    for (guild_id, first_value_at, last_active_at, source) in candidates {
        for (window_days, event) in [(7_i64, "retained_w7"), (30_i64, "retained_w30")] {
            let threshold = first_value_at.saturating_add(window_days.saturating_mul(DAY_MS));
            if last_active_at < threshold {
                continue;
            }
            let inserted = connection.execute(
                "INSERT OR IGNORE INTO guild_growth_retention_record (guild_id, window_days)
                 VALUES (?1, ?2)",
                params![guild_id, window_days],
            )?;
            if inserted != 0 {
                connection.execute(
                    "INSERT INTO growth_daily_metric (day, product, source, event, value)
                     VALUES (?1, 'tts', ?2, ?3, 1)
                     ON CONFLICT(day, product, source, event)
                     DO UPDATE SET value = value + 1",
                    params![utc_day_key_from_unix_millis(last_active_at), source, event],
                )?;
            }
        }
    }
    Ok(())
}

fn add_missing_columns(
    connection: &Connection,
    table: &str,
    definitions: &[(&str, &str)],
) -> Result<(), StoreError> {
    let mut existing = table_columns(connection, table)?;
    for (column, definition) in definitions {
        if !existing.contains(*column) {
            // Both values are Rust constants above, never user input.
            connection.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN {column} {definition};"
            ))?;
            existing.insert((*column).to_owned());
        }
    }
    Ok(())
}

fn table_columns(connection: &Connection, table: &str) -> Result<HashSet<String>, StoreError> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    statement
        .query_map([], |row| row.get(1))?
        .collect::<Result<HashSet<String>, _>>()
        .map_err(StoreError::from)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs::remove_file;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    use rusqlite::Connection;

    use crate::SqliteStore;

    fn temporary_database_path() -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "vozen-rust-legacy-{}-{nonce}.sqlite",
            process::id()
        ))
    }

    fn columns(connection: &Connection, table: &str) -> HashSet<String> {
        super::table_columns(connection, table).expect("columns")
    }

    #[test]
    fn upgrades_legacy_columns_and_preserves_existing_values() {
        let path = temporary_database_path();
        let legacy = Connection::open(&path).expect("legacy DB");
        legacy.execute_batch(
            "CREATE TABLE guild_config (
                guild_id TEXT PRIMARY KEY,
                default_voice TEXT NOT NULL DEFAULT 'en_US-amy-medium'
             );
             INSERT INTO guild_config (guild_id, default_voice) VALUES ('guild', 'pt_PT-tugao-medium');
             CREATE TABLE user_voice (
                guild_id TEXT NOT NULL, user_id TEXT NOT NULL, voice_model TEXT NOT NULL,
                speed REAL NOT NULL, PRIMARY KEY (guild_id, user_id)
             );
             INSERT INTO user_voice VALUES ('guild', 'user', 'pt_PT-tugao-medium', 1.0);
             CREATE TABLE kofi_supporter (email TEXT PRIMARY KEY, discord_id TEXT NOT NULL, updated_at INTEGER NOT NULL);
             CREATE TABLE vote_promo_state (guild_id TEXT PRIMARY KEY, last_post_at INTEGER NOT NULL);
             INSERT INTO vote_promo_state VALUES ('guild', 1);
             CREATE TABLE topgg_sync_state (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                last_attempt_at INTEGER NOT NULL,
                last_success_at INTEGER,
                last_status INTEGER,
                last_server_count INTEGER,
                consecutive_failures INTEGER NOT NULL DEFAULT 0
             );
             INSERT INTO topgg_sync_state VALUES (1, 1, NULL, NULL, NULL, 1);
             CREATE TABLE user_clone (user_id TEXT PRIMARY KEY, sample_path TEXT NOT NULL, consent_at INTEGER NOT NULL);
             CREATE TABLE vote_redemption (user_hash TEXT PRIMARY KEY, redeemed_at INTEGER NOT NULL);
             CREATE TABLE tts_lang_detect_off (guild_id TEXT NOT NULL, user_id TEXT NOT NULL);",
        ).expect("legacy schema");
        drop(legacy);

        let store = SqliteStore::open(&path).expect("migrate");
        let config_columns = columns(store.connection(), "guild_config");
        assert!(config_columns.contains("translation_per_user_daily_char_limit"));
        assert!(columns(store.connection(), "user_voice").contains("engine"));
        assert!(columns(store.connection(), "kofi_supporter").contains("email_hash"));
        assert!(!columns(store.connection(), "kofi_supporter").contains("email"));
        assert!(columns(store.connection(), "vote_promo_state").contains("last_kind"));
        assert!(columns(store.connection(), "topgg_sync_state").contains("last_detail"));
        assert_eq!(
            store
                .connection()
                .query_row("SELECT last_detail FROM topgg_sync_state", [], |row| row
                    .get::<_, String>(
                    0
                ))
                .expect("topgg detail"),
            "unknown"
        );
        assert!(
            store
                .connection()
                .query_row("SELECT voice_model FROM user_voice", [], |row| row
                    .get::<_, String>(0))
                .expect("voice")
                == "pt_PT-google-medium"
        );
        assert!(
            store
                .connection()
                .query_row("SELECT default_voice FROM guild_config", [], |row| row
                    .get::<_, String>(
                    0
                ))
                .expect("default voice")
                == "pt_PT-google-medium"
        );
        assert!(!store.has_schema_object("user_clone").expect("clone purged"));
        assert!(
            !store
                .has_schema_object("vote_redemption")
                .expect("legacy vote marker purged")
        );
        assert!(
            !store
                .has_schema_object("tts_lang_detect_off")
                .expect("old optout purged")
        );
        assert!(
            store
                .has_schema_object("tts_lang_detect_on")
                .expect("optin present")
        );
        drop(store);
        let _ = remove_file(&path);
        let _ = remove_file(format!("{}-wal", path.display()));
        let _ = remove_file(format!("{}-shm", path.display()));
    }

    #[test]
    fn backfills_existing_topgg_deliveries_into_daily_votes_idempotently() {
        let path = temporary_database_path();
        let store = SqliteStore::open(&path).expect("initial database");
        store
            .connection()
            .execute(
                "INSERT INTO topgg_webhook_event (event_id, processed_at) VALUES ('old-1', 1699920000000), ('old-2', 1699920001000)",
                [],
            )
            .expect("historical events");
        drop(store);

        for _ in 0..2 {
            let reopened = SqliteStore::open(&path).expect("migrate");
            assert_eq!(
                reopened
                    .list_growth_daily_metrics("2023-11-14", "2023-11-14")
                    .expect("daily metrics")[0]
                    .votes,
                2
            );
            drop(reopened);
        }

        let _ = remove_file(&path);
        let _ = remove_file(format!("{}-wal", path.display()));
        let _ = remove_file(format!("{}-shm", path.display()));
    }

    #[test]
    fn backfills_existing_retention_outcomes_idempotently() {
        const DAY: i64 = 86_400_000;
        let path = temporary_database_path();
        let store = SqliteStore::open(&path).expect("initial database");
        store
            .connection()
            .execute_batch(
                "INSERT INTO guild_growth_lifecycle
                   (guild_id, product, first_joined_at, last_joined_at, install_source,
                    first_value_at, last_active_at)
                 VALUES
                   ('week', 'tts', 86400000, 86400000, 'home', 86400000, 777600000),
                   ('month', 'tts', 86400000, 86400000, 'home', 86400000, 2764800000);
                 INSERT INTO growth_daily_metric (day, product, source, event, value)
                 VALUES ('1970-01-02', 'tts', 'home', 'first_value', 2);",
            )
            .expect("historical lifecycle");
        drop(store);

        for _ in 0..2 {
            let reopened = SqliteStore::open(&path).expect("migrate");
            let overview = reopened
                .growth_overview(41 * DAY)
                .expect("retention overview");
            assert_eq!(overview.setup_completed, 2);
            assert_eq!(overview.first_value, 2);
            assert_eq!((overview.eligible_w7, overview.retained_w7), (2, 2));
            assert_eq!((overview.eligible_w30, overview.retained_w30), (2, 1));
            assert_eq!(
                reopened
                    .connection()
                    .query_row(
                        "SELECT COUNT(*) FROM guild_growth_retention_record",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .expect("retention record count"),
                3
            );
            drop(reopened);
        }

        let _ = remove_file(&path);
        let _ = remove_file(format!("{}-wal", path.display()));
        let _ = remove_file(format!("{}-shm", path.display()));
    }
}
