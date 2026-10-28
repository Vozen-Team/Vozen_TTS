#![forbid(unsafe_code)]

//! SQLite compatibility boundary for the Rust runtime.
//!
//! The Node implementation remains authoritative while the migration is in progress. This crate
//! deliberately consumes its generated schema contract rather than maintaining a hand-copied DDL
//! list. Existing-database data migrations are added before cutover; this first boundary proves
//! that a newly created Rust database has the same durable objects as today's Node runtime.

use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;
use serde::Deserialize;
use thiserror::Error;

mod admin_passes;
mod admin_stats;
mod blocklist;
mod channel_profile;
mod data_lifecycle;
mod game_score;
mod gcloud_usage;
mod guild_config;
mod guild_departed;
mod kofi_claim;
mod kofi_delivery;
mod kofi_pending;
mod lang_detect;
mod migration;
mod optout;
mod premium;
mod premium_code;
mod pronunciation;
mod stripe;
mod stt_consent;
mod talk_stats;
mod telemetry;
mod translation;
mod user_profile;
mod user_voice;
mod voice_effect;
mod voice_presence;
mod vote_promo;
mod vote_reward;

pub use admin_passes::{AdminPassRow, AdminPassesView, AdminPlusRow};
pub use admin_stats::{
    AdminGuildStats, AdminTopTalkerRow, GameScoreRow, GameUserStats, GuildGamePlayerRow,
    GuildGameStats,
};
pub use blocklist::{AddBlockwordResult, MAX_BLOCKWORDS};
pub use channel_profile::{ChannelProfile, ChannelProfilePatch, MAX_CHANNEL_PROFILES_PER_GUILD};
pub use data_lifecycle::{GUILD_PURGE_TABLES, USER_ERASE_TABLES};
pub use gcloud_usage::{GcloudUsageScope, day_key_utc, month_key_utc};
pub use guild_config::{GuildConfig, GuildConfigPatch};
pub use guild_departed::DEPARTURE_GRACE_MS;
pub use kofi_claim::{
    ACTIVATION_TERMS_VERSION, ActivationConfirmation, ActivationOutcome, ClaimOutcome,
    ClaimedKofiItem, activate_kofi_by_email_hash, claim_kofi_pending_grant,
    extract_kofi_receipt_code,
};
pub use kofi_delivery::{KofiDelivery, KofiDeliveryOutcome, process_kofi_delivery};
pub use kofi_pending::{
    KofiPendingGrant, KofiPendingGrantInput, KofiPendingPlan, PENDING_RETENTION_MS,
};
pub use premium::{
    ActivateResult, ActivateStatus, EntitlementGrant, EntitlementSyncResult, GuildPassOwner,
    PremiumKind, PremiumPass, PremiumPassStatus, PremiumStatusView,
};
pub use premium_code::{
    PremiumCode, PremiumCodeInput, PremiumCodePlan, RedeemCodeResult, RedeemCodeStatus,
};
pub use pronunciation::{
    AddPronunciationResult, SERVER_PRON_LIMIT, SERVER_PRON_LIMIT_PREMIUM, USER_PRON_LIMIT_FREE,
    USER_PRON_LIMIT_PREMIUM,
};
pub use stripe::{StripeSubscription, StripeSubscriptionInput};
pub use stt_consent::SttConsent;
pub use talk_stats::{GuildTalkStreak, TalkBump, TalkRow};
pub use telemetry::{
    ConfiguredEngineResolver, DailyOperationalMetric, DominantTalkUsage, DominantTalkUsageOptions,
    OperationalMetric, OperationalProvider, ProviderHealth, ProviderHealthSnapshot,
    TalkUsageSource, provider_for_engine, utc_day_key, utc_day_key_from_unix_millis,
};
pub use translation::{
    TranslationMapping, TranslationPreference, TranslationPreferencePatch, TranslationReservation,
    TranslationReservationDenial,
};
pub use user_profile::{Birthday, is_valid_birthday};
pub use user_voice::{MAX_RECENT_VOICES, MAX_VOICE_FAVORITES, UserEngine, UserVoice};
pub use voice_effect::VoiceEffect;
pub use voice_presence::VoicePresence;
pub use vote_promo::{CommunityPromoKind, PROMO_SLOT_COOLDOWN_MS};
pub use vote_reward::{
    TOPGG_EVENT_RETENTION_MS, TopggVoteRewardResult, VOTE_REDEMPTION_SECRET_MIN_LENGTH,
    VOTE_REWARD_MS, VoteRewardResult, VoteRewardStatus,
};

pub const SQLITE_SCHEMA_CONTRACT_VERSION: u16 = 1;
const SQLITE_SCHEMA: &str = include_str!("../../../contracts/sqlite-schema.json");

#[derive(Debug, Deserialize)]
struct SqliteSchemaContract {
    schema_version: u16,
    generated_from: String,
    objects: Vec<SqliteSchemaObject>,
}

#[derive(Debug, Deserialize)]
struct SqliteSchemaObject {
    #[serde(rename = "type")]
    kind: String,
    name: String,
    sql: String,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("invalid SQLite schema contract: {0}")]
    InvalidContract(#[from] serde_json::Error),
    #[error("unsupported SQLite schema contract version {0}")]
    UnsupportedContractVersion(u16),
    #[error("schema object is invalid: {0}")]
    InvalidSchemaObject(String),
    #[error("premium code has an unsupported plan: {0}")]
    InvalidPremiumCodePlan(String),
    #[error("operational metric value must be finite and non-negative")]
    InvalidOperationalMetricValue,
    #[error("operational metric day must be a UTC YYYY-MM-DD value: {0}")]
    InvalidOperationalMetricDay(String),
    #[error("database contains an unsupported operational telemetry value: {0}")]
    InvalidOperationalTelemetry(String),
    #[error("invalid birthday month/day")]
    InvalidBirthday,
    #[error("invalid Google HD usage scope key")]
    InvalidGcloudKey,
    #[error("Google personal quota requires an initialized stable redemption secret")]
    GcloudQuotaSecretUnavailable,
    #[error("invalid Google HD usage month")]
    InvalidGcloudMonth,
    #[error("invalid Google HD usage day")]
    InvalidGcloudDay,
    #[error("Google HD usage characters must be positive")]
    InvalidGcloudChars,
    #[error("Google HD usage limits must be non-negative")]
    InvalidGcloudLimit,
    #[error("invalid Discord guild id")]
    InvalidGuildId,
    #[error("invalid STT consent identity")]
    InvalidSttIdentity,
    #[error("guild departure grace period must be non-negative")]
    InvalidDepartureGrace,
    #[error("invalid translation mapping")]
    InvalidTranslationMapping,
    #[error("translation mapping would create a direct cycle")]
    TranslationCycle,
    #[error("translation chars must be a positive integer")]
    InvalidTranslationChars,
    #[error("translation limits must be non-negative integers")]
    InvalidTranslationLimit,
    #[error("translation day must be UTC YYYY-MM-DD")]
    InvalidTranslationDay,
    #[error("talk statistics day must be YYYY-MM-DD")]
    InvalidTalkStatsDay,
    #[error(
        "VOTE_REDEMPTION_SECRET must contain at least {VOTE_REDEMPTION_SECRET_MIN_LENGTH} characters"
    )]
    InvalidVoteRedemptionSecret,
    #[error("VOTE_REDEMPTION_SECRET does not match the key pinned to this database")]
    VoteRedemptionSecretMismatch,
    #[error("invalid Discord user id for vote reward")]
    InvalidVoteUserId,
    #[error("invalid Top.gg webhook event id")]
    InvalidTopggEventId,
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("SQLite integrity check failed: {0}")]
    IntegrityCheck(String),
    #[error("SQLite foreign-key check failed: {0}")]
    ForeignKeyCheck(String),
}

/// A Rust-owned SQLite connection configured with the same durable schema as the live bot.
pub struct SqliteStore {
    connection: Connection,
    gcloud_quota_secret: Option<String>,
}

impl SqliteStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let connection = Connection::open(path)?;
        Self::from_connection(connection)
    }

    pub fn open_in_memory() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;
        Self::from_connection(connection)
    }

    fn from_connection(mut connection: Connection) -> Result<Self, StoreError> {
        connection.busy_timeout(Duration::from_secs(5))?;
        // Foreign-key enforcement is connection-local and is safe to enable before the
        // read-only preflight. Delay WAL/synchronous changes until after that preflight so a
        // rejected database is not modified merely by being opened.
        connection.execute_batch("PRAGMA foreign_keys = ON;")?;
        // Reject an already-corrupt copy before any schema installation or historical migration
        // can touch it. The post-migration check below protects the newly-added objects too.
        verify_connection_integrity(&connection)?;
        connection.execute_batch("PRAGMA journal_mode = WAL;\nPRAGMA synchronous = NORMAL;")?;
        // Schema installation and historical upgrades must commit together. A failed ALTER or
        // privacy cleanup must leave the database exactly as it was before Rust opened it.
        let transaction = connection.transaction()?;
        install_current_schema(&transaction)?;
        migration::migrate_legacy_schema(&transaction)?;
        verify_connection_integrity(&transaction)?;
        transaction.commit()?;
        Ok(Self {
            connection,
            gcloud_quota_secret: None,
        })
    }

    pub fn has_schema_object(&self, name: &str) -> Result<bool, StoreError> {
        let found = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = ?1)",
            [name],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(found != 0)
    }

    /// Verifies that an existing production copy is safe to serve before Discord or HTTP starts.
    ///
    /// SQLite can open a file whose pages are readable while still reporting corruption or
    /// orphaned foreign-key rows. The Node process remains authoritative during migration, so
    /// this is an explicit gate rather than a silent repair: a failed check must stop a Rust
    /// cutover and preserve the original file for rollback investigation.
    pub fn verify_integrity(&self) -> Result<(), StoreError> {
        verify_connection_integrity(&self.connection)
    }
}

fn verify_connection_integrity(connection: &Connection) -> Result<(), StoreError> {
    let result =
        connection.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))?;
    if result != "ok" {
        return Err(StoreError::IntegrityCheck(result));
    }

    let mut statement = connection.prepare("PRAGMA foreign_key_check")?;
    let mut rows = statement.query([])?;
    if let Some(row) = rows.next()? {
        let table = row.get::<_, String>(0).unwrap_or_else(|_| "?".into());
        let rowid = row.get::<_, i64>(1).unwrap_or(-1);
        let parent = row.get::<_, String>(2).unwrap_or_else(|_| "?".into());
        return Err(StoreError::ForeignKeyCheck(format!(
            "table={table} rowid={rowid} parent={parent}"
        )));
    }
    Ok(())
}

impl SqliteStore {
    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }
}

fn install_current_schema(connection: &Connection) -> Result<(), StoreError> {
    let contract: SqliteSchemaContract = serde_json::from_str(SQLITE_SCHEMA)?;
    if contract.schema_version != SQLITE_SCHEMA_CONTRACT_VERSION {
        return Err(StoreError::UnsupportedContractVersion(
            contract.schema_version,
        ));
    }
    if contract.generated_from != "crates/vozen-store/src/schema.rs" {
        return Err(StoreError::InvalidSchemaObject(
            "schema contract source is not the Node database migrator".into(),
        ));
    }

    for object in contract.objects {
        if !matches!(object.kind.as_str(), "table" | "index")
            || object.name.trim().is_empty()
            || !object.sql.trim_start().starts_with("CREATE ")
        {
            return Err(StoreError::InvalidSchemaObject(object.name));
        }
        connection.execute_batch(&idempotent_create_sql(&object.sql)?)?;
    }
    Ok(())
}

/// `sqlite_master` normalizes away `IF NOT EXISTS` when it returns a table/index definition.
/// The generated contract is therefore structurally exact but must be made idempotent before a
/// Rust process opens a pre-existing production database.
fn idempotent_create_sql(sql: &str) -> Result<String, StoreError> {
    if let Some(rest) = sql.strip_prefix("CREATE TABLE ") {
        return Ok(format!("CREATE TABLE IF NOT EXISTS {rest}"));
    }
    if let Some(rest) = sql.strip_prefix("CREATE INDEX ") {
        return Ok(format!("CREATE INDEX IF NOT EXISTS {rest}"));
    }
    Err(StoreError::InvalidSchemaObject(sql.into()))
}

#[cfg(test)]
mod tests {
    use std::fs::remove_file;
    use std::time::{SystemTime, UNIX_EPOCH};

    use rusqlite::Connection;

    use super::*;

    fn temporary_database_path() -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("vozen-rust-current-schema-{nonce}.sqlite"))
    }

    #[test]
    fn new_rust_database_contains_the_node_schema_contract() {
        let store = SqliteStore::open_in_memory().expect("create in-memory store");
        store.verify_integrity().expect("new store is valid");
        assert!(
            store
                .has_schema_object("guild_config")
                .expect("query guild config")
        );
        assert!(
            store
                .has_schema_object("premium_user")
                .expect("query premium user")
        );
        assert!(
            store
                .has_schema_object("kofi_pending")
                .expect("query Ko-fi pending")
        );
        assert!(
            store
                .has_schema_object("tts_lang_detect_on")
                .expect("query detection opt-in")
        );
        assert!(
            store
                .has_schema_object("idx_pass_activation_guild")
                .expect("query index")
        );
    }

    #[test]
    fn schema_sql_is_made_idempotent_without_accepting_non_create_sql() {
        assert_eq!(
            idempotent_create_sql("CREATE TABLE guild_config (guild_id TEXT)").expect("table"),
            "CREATE TABLE IF NOT EXISTS guild_config (guild_id TEXT)"
        );
        assert_eq!(
            idempotent_create_sql("CREATE INDEX idx ON table_name (column_name)").expect("index"),
            "CREATE INDEX IF NOT EXISTS idx ON table_name (column_name)"
        );
        assert!(idempotent_create_sql("DROP TABLE nope").is_err());
    }

    #[test]
    fn integrity_gate_rejects_orphaned_foreign_keys_without_repairing_them() {
        let store = SqliteStore::open_in_memory().expect("create in-memory store");
        store
            .connection
            .execute_batch(
                "CREATE TABLE integrity_parent (id INTEGER PRIMARY KEY);
                 CREATE TABLE integrity_child (
                   id INTEGER PRIMARY KEY,
                   parent_id INTEGER REFERENCES integrity_parent(id)
                 );
                 PRAGMA foreign_keys = OFF;
                 INSERT INTO integrity_child (id, parent_id) VALUES (1, 999);
                 PRAGMA foreign_keys = ON;",
            )
            .expect("seed invalid fixture");

        let error = store
            .verify_integrity()
            .expect_err("orphan must fail closed");
        assert!(matches!(error, StoreError::ForeignKeyCheck(_)));
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT parent_id FROM integrity_child WHERE id = 1",
                    [],
                    |row| { row.get::<_, i64>(0) }
                )
                .expect("fixture remains intact"),
            999
        );
    }

    #[test]
    fn opens_a_copy_created_by_the_current_node_schema_and_preserves_rows() {
        let path = temporary_database_path();
        let legacy = Connection::open(&path).expect("node schema copy");
        let contract: SqliteSchemaContract =
            serde_json::from_str(SQLITE_SCHEMA).expect("schema contract");
        for object in &contract.objects {
            legacy
                .execute_batch(&object.sql)
                .unwrap_or_else(|error| panic!("create {}: {error}", object.name));
        }
        legacy
            .execute(
                "INSERT INTO guild_config (guild_id, default_voice, locale) VALUES (?1, ?2, ?3)",
                ("guild-copy", "pt_PT-google-medium", "pt"),
            )
            .expect("seed node row");
        legacy
            .execute(
                "INSERT INTO user_voice (guild_id, user_id, voice_model, speed, engine)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                (
                    "guild-copy",
                    "user-copy",
                    "en_US-amy-medium",
                    1.0_f64,
                    "google",
                ),
            )
            .expect("seed user voice");
        drop(legacy);

        let store = SqliteStore::open(&path).expect("open current node copy");
        store.verify_integrity().expect("current copy is valid");
        for object in &contract.objects {
            assert!(
                store
                    .has_schema_object(&object.name)
                    .expect("query schema object"),
                "missing schema object {}",
                object.name
            );
        }
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT default_voice FROM guild_config WHERE guild_id = 'guild-copy'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("preserved guild row"),
            "pt_PT-google-medium"
        );
        drop(store);
        let _ = remove_file(&path);
        let _ = remove_file(format!("{}-wal", path.display()));
        let _ = remove_file(format!("{}-shm", path.display()));
    }

    #[test]
    fn schema_and_legacy_migration_roll_back_together_on_open_failure() {
        let path = temporary_database_path();
        let legacy = Connection::open(&path).expect("legacy database");
        legacy
            .execute_batch("CREATE VIEW guild_config AS SELECT 'guild' AS guild_id;")
            .expect("incompatible legacy object");
        drop(legacy);

        assert!(SqliteStore::open(&path).is_err());

        let reopened = Connection::open(&path).expect("reopen original database");
        let object_type: String = reopened
            .query_row(
                "SELECT type FROM sqlite_master WHERE name = 'guild_config'",
                [],
                |row| row.get(0),
            )
            .expect("original view remains");
        assert_eq!(object_type, "view");
        assert_eq!(
            reopened
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name = 'blocklist'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("schema object query"),
            0
        );
        drop(reopened);
        let _ = remove_file(&path);
        let _ = remove_file(format!("{}-wal", path.display()));
        let _ = remove_file(format!("{}-shm", path.display()));
    }

    #[test]
    fn integrity_failure_is_rejected_before_schema_mutation() {
        let path = temporary_database_path();
        let legacy = Connection::open(&path).expect("legacy database");
        legacy
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE integrity_parent (id INTEGER PRIMARY KEY);
                 CREATE TABLE integrity_child (
                   id INTEGER PRIMARY KEY,
                   parent_id INTEGER NOT NULL REFERENCES integrity_parent(id)
                 );
                 PRAGMA foreign_keys = OFF;
                 INSERT INTO integrity_child (id, parent_id) VALUES (1, 999);",
            )
            .expect("incompatible legacy data");
        drop(legacy);

        assert!(matches!(
            SqliteStore::open(&path),
            Err(StoreError::ForeignKeyCheck(_))
        ));

        let reopened = Connection::open(&path).expect("reopen original database");
        assert_eq!(
            reopened
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE name = 'guild_config'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("schema object query"),
            0
        );
        assert_eq!(
            reopened
                .query_row(
                    "SELECT parent_id FROM integrity_child WHERE id = 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("orphan remains untouched"),
            999
        );
        drop(reopened);
        let _ = remove_file(&path);
        let _ = remove_file(format!("{}-wal", path.display()));
        let _ = remove_file(format!("{}-shm", path.display()));
    }
}
