//! Persistent Google HD character budgets.
//!
//! The Node implementation keeps monthly pools in `gcloud_usage` and one service-wide daily
//! backstop in `gcloud_daily_usage`. This module preserves those scopes and makes a reservation
//! all-or-nothing: a failed daily check rolls the monthly increment back.

use hmac::{Hmac, Mac};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::Sha256;

use crate::{SqliteStore, StoreError};

const DAY_MS: i64 = 86_400_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcloudUsageScope {
    User,
    Pass,
    Guild,
    Global,
}

impl GcloudUsageScope {
    fn as_database(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Pass => "pass",
            Self::Guild => "guild",
            Self::Global => "global",
        }
    }
}

#[must_use]
pub fn month_key_utc(unix_millis: i64) -> String {
    utc_day_key(unix_millis)[..7].to_owned()
}

#[must_use]
pub fn day_key_utc(unix_millis: i64) -> String {
    utc_day_key(unix_millis)
}

impl SqliteStore {
    /// Pins the existing stable key without rewriting counters or backfilling the vote ledger.
    pub fn initialize_gcloud_quota_secret(&mut self, secret: &str) -> Result<(), StoreError> {
        crate::vote_reward::assert_stable_redemption_secret(self.connection(), secret)?;
        self.gcloud_quota_secret = Some(secret.to_owned());
        Ok(())
    }

    fn gcloud_storage_key(
        &self,
        scope: GcloudUsageScope,
        key: &str,
        month: &str,
    ) -> Result<String, StoreError> {
        if !matches!(scope, GcloudUsageScope::User | GcloudUsageScope::Pass) {
            return Ok(key.to_owned());
        }
        let secret = self
            .gcloud_quota_secret
            .as_deref()
            .ok_or(StoreError::GcloudQuotaSecretUnavailable)?;
        crate::vote_reward::assert_stable_redemption_secret(self.connection(), secret)?;
        let mut mac =
            Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key");
        mac.update(
            format!(
                "vozen-gcloud-quota:v1:{}:{month}:{key}",
                scope.as_database()
            )
            .as_bytes(),
        );
        let digest = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Ok(format!("gcloud:v1:{digest}"))
    }

    pub fn gcloud_monthly_chars(
        &self,
        scope: GcloudUsageScope,
        key: &str,
        month: &str,
    ) -> Result<i64, StoreError> {
        validate_pool(scope, key, month)?;
        let storage_key = self.gcloud_storage_key(scope, key, month)?;
        monthly_chars_on(self.connection(), scope, key, &storage_key, month)
    }

    /// Adds to a pool without applying a limit. Paid synthesis should use the reservation API.
    pub fn add_gcloud_monthly_chars(
        &self,
        scope: GcloudUsageScope,
        key: &str,
        month: &str,
        chars: i64,
    ) -> Result<(), StoreError> {
        validate_pool(scope, key, month)?;
        if chars < 0 {
            return Err(StoreError::InvalidGcloudChars);
        }
        let storage_key = self.gcloud_storage_key(scope, key, month)?;
        self.connection().execute(
            "INSERT INTO gcloud_usage (scope, key, month, chars)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(scope, key, month)
             DO UPDATE SET chars = chars + excluded.chars",
            params![scope.as_database(), storage_key, month, chars],
        )?;
        Ok(())
    }

    /// Reserves a paid Google HD call from its monthly pool and, when non-zero, the service-wide
    /// daily pool. No counter is changed when either budget would be exceeded.
    #[allow(clippy::too_many_arguments)]
    pub fn reserve_gcloud_chars(
        &self,
        scope: GcloudUsageScope,
        key: &str,
        month: &str,
        monthly_limit: i64,
        day: &str,
        daily_limit: i64,
        chars: i64,
    ) -> Result<bool, StoreError> {
        validate_pool(scope, key, month)?;
        validate_day(day)?;
        validate_non_negative_limit(monthly_limit)?;
        validate_non_negative_limit(daily_limit)?;
        if chars <= 0 {
            return Err(StoreError::InvalidGcloudChars);
        }

        let storage_key = self.gcloud_storage_key(scope, key, month)?;
        let transaction = self.connection().unchecked_transaction()?;
        let used = monthly_chars_on(&transaction, scope, key, &storage_key, month)?;
        if used
            .checked_add(chars)
            .is_none_or(|total| total > monthly_limit)
        {
            return Ok(false);
        }
        transaction.execute(
            "INSERT INTO gcloud_usage(scope,key,month,chars) VALUES(?1,?2,?3,?4)
             ON CONFLICT(scope,key,month) DO UPDATE SET chars=chars+excluded.chars",
            params![scope.as_database(), storage_key, month, chars],
        )?;

        if daily_limit > 0 {
            let changed = transaction.execute(
                "UPDATE gcloud_daily_usage
                    SET chars = chars + ?1
                  WHERE day = ?2 AND chars + ?1 <= ?3",
                params![chars, day, daily_limit],
            )?;
            if changed == 0 {
                let exists: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM gcloud_daily_usage WHERE day = ?1)",
                    [day],
                    |row| row.get(0),
                )?;
                if exists {
                    transaction.rollback()?;
                    return Ok(false);
                }
                let inserted = transaction.execute(
                    "INSERT INTO gcloud_daily_usage (day, chars)
                     SELECT ?1, ?2 WHERE ?2 <= ?3",
                    params![day, chars, daily_limit],
                )?;
                if inserted == 0 {
                    transaction.rollback()?;
                    return Ok(false);
                }
            }
        }
        transaction.commit()?;
        Ok(true)
    }

    pub fn refund_gcloud_chars(
        &self,
        scope: GcloudUsageScope,
        key: &str,
        month: &str,
        day: &str,
        daily_limit: i64,
        chars: i64,
    ) -> Result<(), StoreError> {
        validate_pool(scope, key, month)?;
        validate_day(day)?;
        validate_non_negative_limit(daily_limit)?;
        if chars <= 0 {
            return Err(StoreError::InvalidGcloudChars);
        }
        let storage_key = self.gcloud_storage_key(scope, key, month)?;
        let transaction = self.connection().unchecked_transaction()?;
        // Refund once across both representations, including a reservation moved by erasure.
        let mut remaining = chars;
        for refund_key in
            std::iter::once(storage_key.as_str()).chain((storage_key != key).then_some(key))
        {
            let available: i64 = transaction
                .query_row(
                    "SELECT chars FROM gcloud_usage WHERE scope=?1 AND key=?2 AND month=?3",
                    params![scope.as_database(), refund_key, month],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(0);
            let refunded = available.max(0).min(remaining);
            transaction.execute(
                "UPDATE gcloud_usage SET chars=chars-?4 WHERE scope=?1 AND key=?2 AND month=?3",
                params![scope.as_database(), refund_key, month, refunded],
            )?;
            remaining -= refunded;
        }
        if daily_limit > 0 {
            transaction.execute(
                "UPDATE gcloud_daily_usage SET chars=MAX(0,chars-?1) WHERE day=?2",
                params![chars - remaining, day],
            )?;
        }

        transaction.commit()?;
        Ok(())
    }

    pub fn delete_user_gcloud_usage(&self, user_id: &str) -> Result<(), StoreError> {
        if user_id.trim().is_empty() {
            return Err(StoreError::InvalidGcloudKey);
        }
        let transaction = self.connection().unchecked_transaction()?;
        self.erase_personal_gcloud_on(&transaction, user_id, &month_key_utc(current_time_ms()))?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn erase_personal_gcloud_on(
        &self,
        connection: &Connection,
        user_id: &str,
        month: &str,
    ) -> Result<(), StoreError> {
        for scope in [GcloudUsageScope::User, GcloudUsageScope::Pass] {
            let chars: i64 = connection
                .query_row(
                    "SELECT chars FROM gcloud_usage WHERE scope=?1 AND key=?2 AND month=?3",
                    params![scope.as_database(), user_id, month],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(0);
            if chars > 0 {
                let storage_key = self.gcloud_storage_key(scope, user_id, month)?;
                connection.execute(
                    "INSERT INTO gcloud_usage(scope,key,month,chars) VALUES(?1,?2,?3,?4)
                     ON CONFLICT(scope,key,month) DO UPDATE SET chars=chars+excluded.chars",
                    params![scope.as_database(), storage_key, month, chars],
                )?;
            }
        }
        connection.execute(
            "DELETE FROM gcloud_usage WHERE key=?1 AND scope IN ('user','pass')",
            [user_id],
        )?;
        Ok(())
    }

    /// HMAC counters are needed only during their UTC month; legacy raw retention is separate.
    pub fn purge_expired_gcloud_quota(&self, current_month: &str) -> Result<usize, StoreError> {
        validate_month(current_month)?;
        Ok(self.connection().execute("DELETE FROM gcloud_usage WHERE key LIKE 'gcloud:v1:%' AND scope IN ('user','pass') AND month < ?1",[current_month])?)
    }

    pub fn purge_old_gcloud_usage(&self, cutoff_month: &str) -> Result<usize, StoreError> {
        validate_month(cutoff_month)?;
        Ok(self
            .connection()
            .execute("DELETE FROM gcloud_usage WHERE month < ?1", [cutoff_month])?)
    }
}

fn monthly_chars_on(
    connection: &Connection,
    scope: GcloudUsageScope,
    raw_key: &str,
    storage_key: &str,
    month: &str,
) -> Result<i64, StoreError> {
    connection.query_row(
        "SELECT COALESCE(SUM(chars),0) FROM gcloud_usage WHERE scope=?1 AND key IN (?2,?3) AND month=?4",
        params![scope.as_database(),raw_key,storage_key,month], |row| row.get(0),
    ).map_err(StoreError::from)
}

pub(crate) fn current_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn validate_pool(_scope: GcloudUsageScope, key: &str, month: &str) -> Result<(), StoreError> {
    if key.trim().is_empty() {
        return Err(StoreError::InvalidGcloudKey);
    }
    validate_month(month)
}

fn validate_month(month: &str) -> Result<(), StoreError> {
    let bytes = month.as_bytes();
    if bytes.len() != 7
        || bytes[4] != b'-'
        || !bytes[..4].iter().all(u8::is_ascii_digit)
        || !bytes[5..].iter().all(u8::is_ascii_digit)
        || !(1..=12).contains(&month[5..].parse::<u8>().unwrap_or_default())
    {
        return Err(StoreError::InvalidGcloudMonth);
    }
    Ok(())
}

fn validate_day(day: &str) -> Result<(), StoreError> {
    let bytes = day.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes[..4].iter().all(u8::is_ascii_digit)
        || !bytes[5..7].iter().all(u8::is_ascii_digit)
        || !bytes[8..].iter().all(u8::is_ascii_digit)
    {
        return Err(StoreError::InvalidGcloudDay);
    }
    let year = day[..4].parse::<i32>().unwrap_or_default();
    let month = day[5..7].parse::<u8>().unwrap_or_default();
    let date = day[8..].parse::<u8>().unwrap_or_default();
    if year < 1 || !(1..=12).contains(&month) || !(1..=31).contains(&date) {
        return Err(StoreError::InvalidGcloudDay);
    }
    let max_day = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][usize::from(month - 1)];
    if date > max_day {
        return Err(StoreError::InvalidGcloudDay);
    }
    Ok(())
}

fn validate_non_negative_limit(value: i64) -> Result<(), StoreError> {
    if value < 0 {
        return Err(StoreError::InvalidGcloudLimit);
    }
    Ok(())
}

fn utc_day_key(unix_millis: i64) -> String {
    let days = unix_millis.div_euclid(DAY_MS);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

// Howard Hinnant's proleptic Gregorian conversion, matching the telemetry module.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096).div_euclid(365);
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2).div_euclid(153);
    let d = doy - (153 * mp + 2).div_euclid(5) + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let y = y + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    fn configured() -> SqliteStore {
        let mut store = SqliteStore::open_in_memory().unwrap();
        store.initialize_gcloud_quota_secret(SECRET).unwrap();
        store
    }

    #[test]
    fn spending_survives_erasure_for_user_and_pass_without_touching_other_pools() {
        let store = configured();
        let now = current_time_ms();
        let month = month_key_utc(now);
        let day = day_key_utc(now);
        store.grant_user_premium("u", 30, "manual", now).unwrap();
        store.set_nickname("guild", "u", "personal").unwrap();
        for scope in [GcloudUsageScope::User, GcloudUsageScope::Pass] {
            assert!(
                store
                    .reserve_gcloud_chars(scope, "u", &month, 10, &day, 100, 10)
                    .unwrap()
            );
            store
                .add_gcloud_monthly_chars(scope, "other", &month, 3)
                .unwrap();
        }
        for scope in [GcloudUsageScope::Guild, GcloudUsageScope::Global] {
            store
                .add_gcloud_monthly_chars(scope, "u", &month, 4)
                .unwrap();
        }
        store.erase_user_data("u").unwrap();
        store.erase_user_data("u").unwrap();
        assert!(store.is_user_premium("u", now).unwrap());
        assert_eq!(store.nickname("guild", "u").unwrap(), None);
        for scope in [GcloudUsageScope::User, GcloudUsageScope::Pass] {
            assert_eq!(store.gcloud_monthly_chars(scope, "u", &month).unwrap(), 10);
            assert!(
                !store
                    .reserve_gcloud_chars(scope, "u", &month, 10, &day, 100, 1)
                    .unwrap()
            );
            assert_eq!(
                store.gcloud_monthly_chars(scope, "other", &month).unwrap(),
                3
            );
        }
        for scope in [GcloudUsageScope::Guild, GcloudUsageScope::Global] {
            assert_eq!(store.gcloud_monthly_chars(scope, "u", &month).unwrap(), 4);
        }
        let daily: i64 = store
            .connection()
            .query_row(
                "SELECT chars FROM gcloud_daily_usage WHERE day=?1",
                [&day],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(daily, 20);
        let raw:i64=store.connection().query_row("SELECT COUNT(*) FROM gcloud_usage WHERE scope IN ('user','pass') AND key IN ('u','other')",[],|r|r.get(0)).unwrap();
        assert_eq!(raw, 0);
    }

    #[test]
    fn legacy_and_hash_counts_sum_without_startup_migration_and_refund_once() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let now = current_time_ms();
        let month = month_key_utc(now);
        let day = day_key_utc(now);
        store
            .connection()
            .execute(
                "INSERT INTO gcloud_usage VALUES ('user','u',?1,6)",
                [&month],
            )
            .unwrap();
        store
            .connection()
            .execute(
                "INSERT INTO vote_reward VALUES ('12345678901234567',?1)",
                [now],
            )
            .unwrap();
        store.initialize_gcloud_quota_secret(SECRET).unwrap();
        let raw: i64 = store
            .connection()
            .query_row("SELECT chars FROM gcloud_usage WHERE key='u'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let ledger: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM vote_reward_ledger", [], |r| r.get(0))
            .unwrap();
        assert_eq!((raw, ledger), (6, 0));
        assert!(
            store
                .reserve_gcloud_chars(GcloudUsageScope::User, "u", &month, 10, &day, 100, 4)
                .unwrap()
        );
        assert_eq!(
            store
                .gcloud_monthly_chars(GcloudUsageScope::User, "u", &month)
                .unwrap(),
            10
        );
        assert!(
            !store
                .reserve_gcloud_chars(GcloudUsageScope::User, "u", &month, 10, &day, 100, 1)
                .unwrap()
        );
        assert!(
            !store
                .reserve_gcloud_chars(
                    GcloudUsageScope::User,
                    "u",
                    &month,
                    i64::MAX,
                    &day,
                    0,
                    i64::MAX
                )
                .unwrap()
        );
        store
            .refund_gcloud_chars(GcloudUsageScope::User, "u", &month, &day, 100, 3)
            .unwrap();
        assert_eq!(
            store
                .gcloud_monthly_chars(GcloudUsageScope::User, "u", &month)
                .unwrap(),
            7
        );
        store.delete_user_gcloud_usage("u").unwrap();
        assert_eq!(
            store
                .gcloud_monthly_chars(GcloudUsageScope::User, "u", &month)
                .unwrap(),
            7
        );
        let raw: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM gcloud_usage WHERE key='u'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(raw, 0);
        store
            .refund_gcloud_chars(GcloudUsageScope::User, "u", &month, &day, 100, 2)
            .unwrap();
        assert_eq!(
            store
                .gcloud_monthly_chars(GcloudUsageScope::User, "u", &month)
                .unwrap(),
            5
        );
    }

    #[test]
    fn missing_or_changed_key_fails_closed_and_erase_failure_rolls_back() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let now = current_time_ms();
        let month = month_key_utc(now);
        let day = day_key_utc(now);
        assert!(matches!(
            store.reserve_gcloud_chars(GcloudUsageScope::User, "u", &month, 10, &day, 0, 1),
            Err(StoreError::GcloudQuotaSecretUnavailable)
        ));
        assert!(
            store
                .reserve_gcloud_chars(GcloudUsageScope::Guild, "g", &month, 10, &day, 0, 1)
                .unwrap()
        );
        store
            .connection()
            .execute(
                "INSERT INTO gcloud_usage VALUES ('user','u',?1,6)",
                [&month],
            )
            .unwrap();
        store.set_nickname("guild", "u", "keep").unwrap();
        assert!(store.erase_user_data("u").is_err());
        assert_eq!(
            store.nickname("guild", "u").unwrap().as_deref(),
            Some("keep")
        );
        assert!(store.initialize_gcloud_quota_secret("short").is_err());
        store.initialize_gcloud_quota_secret(SECRET).unwrap();
        assert!(matches!(
            store.initialize_gcloud_quota_secret("different-secret-abcdefghijklmnopqrstuvwxyz"),
            Err(StoreError::VoteRedemptionSecretMismatch)
        ));
        store.connection().execute_batch("CREATE TEMP TRIGGER reject_erase BEFORE DELETE ON gcloud_usage BEGIN SELECT RAISE(ABORT,'fixture'); END;").unwrap();
        assert!(store.erase_user_data("u").is_err());
        assert_eq!(
            store.nickname("guild", "u").unwrap().as_deref(),
            Some("keep")
        );
        let (raw,hashed):(i64,i64)=store.connection().query_row("SELECT SUM(key='u'),SUM(key LIKE 'gcloud:v1:%') FROM gcloud_usage WHERE scope='user'",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!((raw, hashed), (1, 0));
        assert!(store.list_runtime_outbox(10).unwrap().is_empty());
    }

    #[test]
    fn month_boundary_cleanup_keeps_legacy_retention_and_refund_uses_reserved_period() {
        let store = configured();
        store
            .connection()
            .execute_batch("INSERT INTO gcloud_usage VALUES ('user','legacy','2026-01',6);")
            .unwrap();
        assert!(
            store
                .reserve_gcloud_chars(
                    GcloudUsageScope::User,
                    "u",
                    "2026-01",
                    10,
                    "2026-01-31",
                    100,
                    8
                )
                .unwrap()
        );
        assert!(
            store
                .reserve_gcloud_chars(
                    GcloudUsageScope::User,
                    "u",
                    "2026-02",
                    10,
                    "2026-02-01",
                    100,
                    2
                )
                .unwrap()
        );
        store
            .refund_gcloud_chars(GcloudUsageScope::User, "u", "2026-01", "2026-01-31", 100, 3)
            .unwrap();
        assert_eq!(
            store
                .gcloud_monthly_chars(GcloudUsageScope::User, "u", "2026-01")
                .unwrap(),
            5
        );
        assert_eq!(
            store
                .gcloud_monthly_chars(GcloudUsageScope::User, "u", "2026-02")
                .unwrap(),
            2
        );
        assert_eq!(store.purge_expired_gcloud_quota("2026-02").unwrap(), 1);
        assert_eq!(
            store
                .gcloud_monthly_chars(GcloudUsageScope::User, "legacy", "2026-01")
                .unwrap(),
            6
        );
        assert_eq!(
            store
                .gcloud_monthly_chars(GcloudUsageScope::User, "u", "2026-02")
                .unwrap(),
            2
        );
        let daily: i64 = store
            .connection()
            .query_row(
                "SELECT chars FROM gcloud_daily_usage WHERE day='2026-01-31'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(daily, 5);
    }

    #[test]
    fn restart_reuses_the_pinned_key_without_resetting_or_rewriting_counters() {
        let path = std::env::temp_dir().join(format!(
            "vozen-gcloud-quota-{}.sqlite",
            uuid::Uuid::new_v4()
        ));
        {
            let mut store = SqliteStore::open(&path).unwrap();
            store.initialize_gcloud_quota_secret(SECRET).unwrap();
            assert!(
                store
                    .reserve_gcloud_chars(
                        GcloudUsageScope::Pass,
                        "u",
                        "2026-01",
                        10,
                        "2026-01-01",
                        0,
                        10
                    )
                    .unwrap()
            );
        }
        {
            let mut store = SqliteStore::open(&path).unwrap();
            assert!(
                store
                    .gcloud_monthly_chars(GcloudUsageScope::Pass, "u", "2026-01")
                    .is_err()
            );
            assert!(
                store
                    .initialize_gcloud_quota_secret("different-secret-abcdefghijklmnopqrstuvwxyz")
                    .is_err()
            );
            store.initialize_gcloud_quota_secret(SECRET).unwrap();
            assert!(
                !store
                    .reserve_gcloud_chars(
                        GcloudUsageScope::Pass,
                        "u",
                        "2026-01",
                        10,
                        "2026-01-01",
                        0,
                        1
                    )
                    .unwrap()
            );
            assert_eq!(
                store
                    .gcloud_monthly_chars(GcloudUsageScope::Pass, "u", "2026-01")
                    .unwrap(),
                10
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn utc_keys_match_node_boundaries() {
        assert_eq!(month_key_utc(1_767_225_600_000), "2026-01");
        assert_eq!(day_key_utc(1_767_225_600_000), "2026-01-01");
        assert_eq!(month_key_utc(1_798_761_600_000), "2027-01");
    }

    #[test]
    fn monthly_pool_accumulates_by_scope_key_and_month() {
        let store = configured();
        assert_eq!(
            store
                .gcloud_monthly_chars(GcloudUsageScope::User, "u1", "2026-07")
                .expect("empty"),
            0
        );
        store
            .add_gcloud_monthly_chars(GcloudUsageScope::User, "u1", "2026-07", 100)
            .expect("first");
        store
            .add_gcloud_monthly_chars(GcloudUsageScope::User, "u1", "2026-07", 250)
            .expect("second");
        store
            .add_gcloud_monthly_chars(GcloudUsageScope::Pass, "u1", "2026-07", 50)
            .expect("scope");
        assert_eq!(
            store
                .gcloud_monthly_chars(GcloudUsageScope::User, "u1", "2026-07")
                .expect("sum"),
            350
        );
        assert_eq!(
            store
                .gcloud_monthly_chars(GcloudUsageScope::Pass, "u1", "2026-07")
                .expect("separate"),
            50
        );
    }

    #[test]
    fn reservation_is_all_or_nothing_across_month_and_day_limits() {
        let store = configured();
        assert!(
            store
                .reserve_gcloud_chars(
                    GcloudUsageScope::User,
                    "u",
                    "2026-07",
                    5,
                    "2026-07-15",
                    3,
                    3
                )
                .expect("reserve")
        );
        assert!(
            !store
                .reserve_gcloud_chars(
                    GcloudUsageScope::User,
                    "u",
                    "2026-07",
                    5,
                    "2026-07-15",
                    3,
                    3
                )
                .expect("monthly denial")
        );
        assert_eq!(
            store
                .gcloud_monthly_chars(GcloudUsageScope::User, "u", "2026-07")
                .expect("monthly"),
            3
        );
        assert_eq!(
            store
                .connection()
                .query_row(
                    "SELECT chars FROM gcloud_daily_usage WHERE day = '2026-07-15'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .expect("daily"),
            3
        );
        let denied = store
            .reserve_gcloud_chars(
                GcloudUsageScope::Pass,
                "owner",
                "2026-07",
                100,
                "2026-07-15",
                3,
                1,
            )
            .expect("daily denial");
        assert!(!denied);
        assert_eq!(
            store
                .gcloud_monthly_chars(GcloudUsageScope::Pass, "owner", "2026-07")
                .expect("rollback"),
            0
        );
    }

    #[test]
    fn personal_delete_and_retention_do_not_touch_current_guild_pool() {
        let store = configured();
        store
            .add_gcloud_monthly_chars(GcloudUsageScope::User, "u", "2026-06", 1)
            .expect("user");
        store
            .add_gcloud_monthly_chars(GcloudUsageScope::Pass, "u", "2026-06", 2)
            .expect("pass");
        store
            .add_gcloud_monthly_chars(GcloudUsageScope::Guild, "g", "2026-06", 3)
            .expect("guild");
        store
            .add_gcloud_monthly_chars(GcloudUsageScope::Guild, "g", "2026-07", 4)
            .expect("current");
        store.delete_user_gcloud_usage("u").expect("erase");
        assert_eq!(store.purge_old_gcloud_usage("2026-07").expect("purge"), 3);
        assert_eq!(
            store
                .gcloud_monthly_chars(GcloudUsageScope::Guild, "g", "2026-07")
                .expect("current"),
            4
        );
    }
}
