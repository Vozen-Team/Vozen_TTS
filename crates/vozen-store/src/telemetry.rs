//! Privacy-minimised operational and speech-usage aggregates.
//!
//! These tables deliberately contain neither text, audio, tokens, raw provider errors nor
//! Discord identities in the operational path. `talk_usage` is limited to the existing
//! `(guild, user, voice locale, engine)` counter which is required by the owner dashboard.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, params_from_iter};

use crate::{SqliteStore, StoreError, UserEngine};

const DEFAULT_MODEL: &str = "en_US-amy-medium";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationalProvider {
    Internal,
    Piper,
    Kokoro,
    Gtts,
    Gcloud,
    AzureTts,
    AzureTranslation,
    Neural,
}

impl OperationalProvider {
    pub(crate) fn as_database(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::Piper => "piper",
            Self::Kokoro => "kokoro",
            Self::Gtts => "gtts",
            Self::Gcloud => "gcloud",
            Self::AzureTts => "azure_tts",
            Self::AzureTranslation => "azure_translation",
            Self::Neural => "neural",
        }
    }

    fn from_database(value: &str) -> Result<Self, StoreError> {
        match value {
            "internal" => Ok(Self::Internal),
            "piper" => Ok(Self::Piper),
            "kokoro" => Ok(Self::Kokoro),
            "gtts" => Ok(Self::Gtts),
            "gcloud" => Ok(Self::Gcloud),
            "azure_tts" => Ok(Self::AzureTts),
            "azure_translation" => Ok(Self::AzureTranslation),
            "neural" => Ok(Self::Neural),
            _ => Err(StoreError::InvalidOperationalTelemetry(value.to_owned())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationalMetric {
    CommandInvoked,
    GuildJoin,
    GuildLeave,
    SynthSuccess,
    SynthFailure,
    SynthFallback,
    SynthLatencyMs,
    TtfaMs,
    QueueDrop,
    SttAudioMs,
    TranslationSuccess,
    TranslationFailure,
    TranslationChars,
    ProviderChargedChars,
    ReadDisabled,
    ReadWrongCall,
    ReadRoleMissing,
    ReadOptedOut,
    ReadBlocked,
    ReadProfileMismatch,
    ReadRateLimited,
    ReadQueueBusy,
    ReadSpamSuppressed,
    ReadContentBlocked,
    ReadSynthesisFailed,
    ReadPlaybackFailed,
    ReadStoreUnavailable,
    TtsQueued,
    PlaybackReady,
    PlaybackStarted,
    PlaybackCompleted,
    PlaybackStopped,
    PlaybackFailed,
}

impl OperationalMetric {
    pub(crate) fn as_database(self) -> &'static str {
        match self {
            Self::CommandInvoked => "command_invoked",
            Self::GuildJoin => "guild_join",
            Self::GuildLeave => "guild_leave",
            Self::SynthSuccess => "synth_success",
            Self::SynthFailure => "synth_failure",
            Self::SynthFallback => "synth_fallback",
            Self::SynthLatencyMs => "synth_latency_ms",
            Self::TtfaMs => "ttfa_ms",
            Self::QueueDrop => "queue_drop",
            Self::SttAudioMs => "stt_audio_ms",
            Self::TranslationSuccess => "translation_success",
            Self::TranslationFailure => "translation_failure",
            Self::TranslationChars => "translation_chars",
            Self::ProviderChargedChars => "provider_charged_chars",
            Self::ReadDisabled => "read_disabled",
            Self::ReadWrongCall => "read_wrong_call",
            Self::ReadRoleMissing => "read_role_missing",
            Self::ReadOptedOut => "read_opted_out",
            Self::ReadBlocked => "read_blocked",
            Self::ReadProfileMismatch => "read_profile_mismatch",
            Self::ReadRateLimited => "read_rate_limited",
            Self::ReadQueueBusy => "read_queue_busy",
            Self::ReadSpamSuppressed => "read_spam_suppressed",
            Self::ReadContentBlocked => "read_content_blocked",
            Self::ReadSynthesisFailed => "read_synthesis_failed",
            Self::ReadPlaybackFailed => "read_playback_failed",
            Self::ReadStoreUnavailable => "read_store_unavailable",
            Self::TtsQueued => "tts_queued",
            Self::PlaybackReady => "playback_ready",
            Self::PlaybackStarted => "playback_started",
            Self::PlaybackCompleted => "playback_completed",
            Self::PlaybackStopped => "playback_stopped",
            Self::PlaybackFailed => "playback_failed",
        }
    }

    fn from_database(value: &str) -> Result<Self, StoreError> {
        match value {
            "command_invoked" => Ok(Self::CommandInvoked),
            "guild_join" => Ok(Self::GuildJoin),
            "guild_leave" => Ok(Self::GuildLeave),
            "synth_success" => Ok(Self::SynthSuccess),
            "synth_failure" => Ok(Self::SynthFailure),
            "synth_fallback" => Ok(Self::SynthFallback),
            "synth_latency_ms" => Ok(Self::SynthLatencyMs),
            "ttfa_ms" => Ok(Self::TtfaMs),
            "queue_drop" => Ok(Self::QueueDrop),
            "stt_audio_ms" => Ok(Self::SttAudioMs),
            "translation_success" => Ok(Self::TranslationSuccess),
            "translation_failure" => Ok(Self::TranslationFailure),
            "translation_chars" => Ok(Self::TranslationChars),
            "provider_charged_chars" => Ok(Self::ProviderChargedChars),
            "read_disabled" => Ok(Self::ReadDisabled),
            "read_wrong_call" => Ok(Self::ReadWrongCall),
            "read_role_missing" => Ok(Self::ReadRoleMissing),
            "read_opted_out" => Ok(Self::ReadOptedOut),
            "read_blocked" => Ok(Self::ReadBlocked),
            "read_profile_mismatch" => Ok(Self::ReadProfileMismatch),
            "read_rate_limited" => Ok(Self::ReadRateLimited),
            "read_queue_busy" => Ok(Self::ReadQueueBusy),
            "read_spam_suppressed" => Ok(Self::ReadSpamSuppressed),
            "read_content_blocked" => Ok(Self::ReadContentBlocked),
            "read_synthesis_failed" => Ok(Self::ReadSynthesisFailed),
            "read_playback_failed" => Ok(Self::ReadPlaybackFailed),
            "read_store_unavailable" => Ok(Self::ReadStoreUnavailable),
            "tts_queued" => Ok(Self::TtsQueued),
            "playback_ready" => Ok(Self::PlaybackReady),
            "playback_started" => Ok(Self::PlaybackStarted),
            "playback_completed" => Ok(Self::PlaybackCompleted),
            "playback_stopped" => Ok(Self::PlaybackStopped),
            "playback_failed" => Ok(Self::PlaybackFailed),

            _ => Err(StoreError::InvalidOperationalTelemetry(value.to_owned())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderHealth {
    Healthy,
    Degraded,
}

impl ProviderHealth {
    fn as_database(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
        }
    }

    fn from_database(value: &str) -> Result<Self, StoreError> {
        match value {
            "healthy" => Ok(Self::Healthy),
            "degraded" => Ok(Self::Degraded),
            _ => Err(StoreError::InvalidOperationalTelemetry(value.to_owned())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyOperationalMetric {
    pub day: String,
    pub metric: OperationalMetric,
    pub provider: OperationalProvider,
    pub value: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderHealthSnapshot {
    pub provider: OperationalProvider,
    pub health: ProviderHealth,
}

/// Whether the owner-dashboard usage summary comes from actual recorded speech or only the
/// current configuration of a user with no samples yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TalkUsageSource {
    Measured,
    Configured,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DominantTalkUsage {
    pub language: Option<String>,
    pub engine: Option<UserEngine>,
    pub samples: i64,
    pub source: TalkUsageSource,
}

/// Runtime inputs used only to choose an honest configured fallback. They never modify stored
/// usage or get combined with measured samples.
#[derive(Default)]
pub struct DominantTalkUsageOptions<'a> {
    pub default_model: Option<&'a str>,
    pub available_models: Option<&'a [String]>,
    pub resolve_configured_engine: Option<ConfiguredEngineResolver<'a>>,
}

pub type ConfiguredEngineResolver<'a> = &'a dyn Fn(&str, &str, UserEngine) -> UserEngine;

impl SqliteStore {
    /// Records one accepted speech message using only its resolved voice locale and engine.
    pub fn bump_talk_usage(
        &self,
        guild_id: &str,
        user_id: &str,
        model: &str,
        engine: UserEngine,
    ) -> Result<(), StoreError> {
        self.connection().execute(
            "INSERT INTO talk_usage (guild_id, user_id, language, engine, spoken_count)
             VALUES (?1, ?2, ?3, ?4, 1)
             ON CONFLICT(guild_id, user_id, language, engine)
             DO UPDATE SET spoken_count = spoken_count + 1",
            params![guild_id, user_id, voice_locale(model), engine_key(engine)],
        )?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(i64::MAX as u128) as i64;
        // This touches only a server lifecycle row and an identity-free daily aggregate. The
        // existing user-level talk counter above remains the source for the private top-talker
        // view, while growth does not consume the user ID at all.
        self.record_guild_first_value(guild_id, now)?;
        Ok(())
    }

    /// Returns each requested user's dominant usage across all guilds.
    ///
    /// Exact recorded samples win completely. In their absence, current settings are weighted by
    /// existing `talk_stats` counts and explicitly labelled `Configured`; historical messages are
    /// never retrospectively attributed to a current voice choice.
    pub fn dominant_talk_usage(
        &self,
        user_ids: &[String],
        options: DominantTalkUsageOptions<'_>,
    ) -> Result<HashMap<String, DominantTalkUsage>, StoreError> {
        let ids = unique_non_empty_ids(user_ids);
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let usage_sql = format!(
            "SELECT user_id, language, engine, spoken_count
             FROM talk_usage WHERE user_id IN ({placeholders})"
        );
        let configured_sql = format!(
            "SELECT ts.guild_id, ts.user_id, ts.spoken_count,
                    uv.voice_model, gc.default_voice, uv.engine
             FROM talk_stats ts
             LEFT JOIN user_voice uv ON uv.guild_id = ts.guild_id AND uv.user_id = ts.user_id
             LEFT JOIN guild_config gc ON gc.guild_id = ts.guild_id
             WHERE ts.user_id IN ({placeholders})"
        );

        let id_params = || params_from_iter(ids.iter().map(String::as_str));
        let usage_rows = {
            let mut statement = self.connection().prepare(&usage_sql)?;
            statement
                .query_map(id_params(), |row| {
                    Ok(UsageRow {
                        user_id: row.get(0)?,
                        language: row.get(1)?,
                        engine: row.get(2)?,
                        spoken_count: row.get(3)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        let configured_rows = {
            let mut statement = self.connection().prepare(&configured_sql)?;
            statement
                .query_map(id_params(), |row| {
                    Ok(ConfiguredRow {
                        guild_id: row.get(0)?,
                        user_id: row.get(1)?,
                        spoken_count: row.get(2)?,
                        user_model: row.get(3)?,
                        guild_model: row.get(4)?,
                        engine: row.get(5)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?
        };

        let mut measured_languages: HashMap<String, HashMap<String, i64>> = HashMap::new();
        let mut measured_engines: HashMap<String, HashMap<UserEngine, i64>> = HashMap::new();
        let mut samples = HashMap::<String, i64>::new();
        for row in usage_rows {
            let count = row.spoken_count.max(0);
            if count == 0 {
                continue;
            }
            *measured_languages
                .entry(row.user_id.clone())
                .or_default()
                .entry(row.language)
                .or_default() += count;
            *measured_engines
                .entry(row.user_id.clone())
                .or_default()
                .entry(engine_from_database(&row.engine))
                .or_default() += count;
            *samples.entry(row.user_id).or_default() += count;
        }

        let mut configured_languages: HashMap<String, HashMap<String, i64>> = HashMap::new();
        let mut configured_engines: HashMap<String, HashMap<UserEngine, i64>> = HashMap::new();
        let default_model = options.default_model.unwrap_or(DEFAULT_MODEL).trim();
        for row in configured_rows {
            if samples.get(&row.user_id).copied().unwrap_or_default() > 0 {
                continue;
            }
            let count = row.spoken_count.max(0);
            if count == 0 {
                continue;
            }
            let model = configured_model(
                row.user_model.as_deref(),
                row.guild_model.as_deref(),
                default_model,
                options.available_models,
            );
            let stored_engine = row
                .engine
                .as_deref()
                .map(engine_from_database)
                .unwrap_or(UserEngine::Google);
            let engine = options
                .resolve_configured_engine
                .map(|resolve| resolve(&row.guild_id, &row.user_id, stored_engine))
                .unwrap_or(stored_engine);
            *configured_languages
                .entry(row.user_id.clone())
                .or_default()
                .entry(voice_locale(model).to_owned())
                .or_default() += count;
            *configured_engines
                .entry(row.user_id)
                .or_default()
                .entry(engine)
                .or_default() += count;
        }

        let mut result = HashMap::new();
        for user_id in ids {
            let sample_count = samples.get(&user_id).copied().unwrap_or_default();
            if sample_count > 0 {
                result.insert(
                    user_id.clone(),
                    DominantTalkUsage {
                        language: winner_string(measured_languages.get(&user_id)),
                        engine: winner_engine(measured_engines.get(&user_id)),
                        samples: sample_count,
                        source: TalkUsageSource::Measured,
                    },
                );
                continue;
            }
            let language = winner_string(configured_languages.get(&user_id));
            let engine = winner_engine(configured_engines.get(&user_id));
            let source = if language.is_some() && engine.is_some() {
                TalkUsageSource::Configured
            } else {
                TalkUsageSource::None
            };
            result.insert(
                user_id,
                DominantTalkUsage {
                    language,
                    engine,
                    samples: 0,
                    source,
                },
            );
        }
        Ok(result)
    }

    /// Adds an identity-free daily operator metric. The caller cannot attach content or Discord
    /// IDs because this API accepts only the fixed metric/provider enums and a number.
    pub fn add_operational_metric(
        &self,
        metric: OperationalMetric,
        provider: OperationalProvider,
        value: f64,
        day: Option<&str>,
    ) -> Result<(), StoreError> {
        if !value.is_finite() || value < 0.0 || value > i64::MAX as f64 {
            return Err(StoreError::InvalidOperationalMetricValue);
        }
        let day = day.map(str::to_owned).unwrap_or_else(utc_day_key);
        validate_day(&day)?;
        self.connection().execute(
            "INSERT INTO operational_daily_metric (day, metric, provider, value)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(day, metric, provider) DO UPDATE SET value = value + excluded.value",
            params![
                day,
                metric.as_database(),
                provider.as_database(),
                value.round() as i64
            ],
        )?;
        Ok(())
    }

    pub fn list_daily_operational_metrics(
        &self,
        day: Option<&str>,
    ) -> Result<Vec<DailyOperationalMetric>, StoreError> {
        let day = day.map(str::to_owned).unwrap_or_else(utc_day_key);
        validate_day(&day)?;
        let mut statement = self.connection().prepare(
            "SELECT day, metric, provider, value FROM operational_daily_metric
             WHERE day = ?1 ORDER BY metric ASC, provider ASC",
        )?;
        statement
            .query_map([day], |row| {
                let metric: String = row.get(1)?;
                let provider: String = row.get(2)?;
                Ok((
                    row.get::<_, String>(0)?,
                    metric,
                    provider,
                    row.get::<_, i64>(3)?,
                ))
            })?
            .map(|row| {
                let (day, metric, provider, value) = row?;
                Ok(DailyOperationalMetric {
                    day,
                    metric: OperationalMetric::from_database(&metric)?,
                    provider: OperationalProvider::from_database(&provider)?,
                    value,
                })
            })
            .collect()
    }

    pub fn list_provider_health(&self) -> Result<Vec<ProviderHealthSnapshot>, StoreError> {
        let mut statement = self
            .connection()
            .prepare("SELECT provider, health FROM provider_health_state ORDER BY provider ASC")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (provider, health) = row?;
                Ok(ProviderHealthSnapshot {
                    provider: OperationalProvider::from_database(&provider)?,
                    health: ProviderHealth::from_database(&health)?,
                })
            })
            .collect()
    }

    pub fn set_provider_health(
        &self,
        provider: OperationalProvider,
        health: ProviderHealth,
        changed_at: i64,
    ) -> Result<(), StoreError> {
        self.connection().execute(
            "INSERT INTO provider_health_state
               (provider, health, changed_at, last_healthy_at, last_degraded_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(provider) DO UPDATE SET
               health = excluded.health,
               changed_at = CASE WHEN provider_health_state.health = excluded.health
                 THEN provider_health_state.changed_at ELSE excluded.changed_at END,
               last_healthy_at = CASE WHEN excluded.health = 'healthy'
                 THEN excluded.last_healthy_at ELSE provider_health_state.last_healthy_at END,
               last_degraded_at = CASE WHEN excluded.health = 'degraded'
                 THEN excluded.last_degraded_at ELSE provider_health_state.last_degraded_at END",
            params![
                provider.as_database(),
                health.as_database(),
                changed_at,
                (health == ProviderHealth::Healthy).then_some(changed_at),
                (health == ProviderHealth::Degraded).then_some(changed_at),
            ],
        )?;
        Ok(())
    }
}

pub fn provider_for_engine(engine: Option<UserEngine>) -> OperationalProvider {
    match engine.unwrap_or(UserEngine::Google) {
        UserEngine::Google => OperationalProvider::Gtts,
        UserEngine::Piper => OperationalProvider::Piper,
        UserEngine::Kokoro => OperationalProvider::Kokoro,
        UserEngine::Gcloud => OperationalProvider::Gcloud,
    }
}

/// UTC YYYY-MM-DD based on the current system clock, independently testable via
/// `utc_day_key_from_unix_millis`.
pub fn utc_day_key() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64;
    utc_day_key_from_unix_millis(millis)
}

pub fn utc_day_key_from_unix_millis(millis: i64) -> String {
    let days = millis.div_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

fn voice_locale(model: &str) -> &str {
    model.split_once('-').map_or_else(
        || {
            if model.trim().is_empty() {
                "unknown"
            } else {
                model.trim()
            }
        },
        |(locale, _)| {
            if locale.trim().is_empty() {
                "unknown"
            } else {
                locale.trim()
            }
        },
    )
}

fn engine_key(engine: UserEngine) -> &'static str {
    match engine {
        UserEngine::Google => "google",
        UserEngine::Piper => "piper",
        UserEngine::Kokoro => "kokoro",
        UserEngine::Gcloud => "gcloud",
    }
}

fn engine_from_database(value: &str) -> UserEngine {
    match value {
        "piper" => UserEngine::Piper,
        "kokoro" => UserEngine::Kokoro,
        "gcloud" => UserEngine::Gcloud,
        _ => UserEngine::Google,
    }
}

fn unique_non_empty_ids(ids: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    ids.iter()
        .filter(|id| !id.is_empty() && seen.insert((*id).clone()))
        .cloned()
        .collect()
}

fn configured_model<'a>(
    user_model: Option<&'a str>,
    guild_model: Option<&'a str>,
    default_model: &'a str,
    available_models: Option<&'a [String]>,
) -> &'a str {
    let candidates = [user_model, guild_model, Some(default_model)];
    let first_non_empty = candidates
        .into_iter()
        .flatten()
        .find(|model| !model.trim().is_empty())
        .unwrap_or(DEFAULT_MODEL);
    let Some(available_models) = available_models else {
        return first_non_empty;
    };
    candidates
        .into_iter()
        .flatten()
        .find(|candidate| available_models.iter().any(|model| model == *candidate))
        .map_or_else(
            || {
                available_models
                    .first()
                    .map(String::as_str)
                    .unwrap_or(first_non_empty)
            },
            |model| model,
        )
}

fn winner_string(counts: Option<&HashMap<String, i64>>) -> Option<String> {
    counts.and_then(|counts| {
        counts
            .iter()
            .max_by(|(left_key, left_count), (right_key, right_count)| {
                left_count
                    .cmp(right_count)
                    .then_with(|| right_key.cmp(left_key))
            })
            .map(|(key, _)| key.clone())
    })
}

fn winner_engine(counts: Option<&HashMap<UserEngine, i64>>) -> Option<UserEngine> {
    counts.and_then(|counts| {
        counts
            .iter()
            .max_by(|(left_engine, left_count), (right_engine, right_count)| {
                left_count
                    .cmp(right_count)
                    .then_with(|| engine_key(**right_engine).cmp(engine_key(**left_engine)))
            })
            .map(|(engine, _)| *engine)
    })
}

fn validate_day(day: &str) -> Result<(), StoreError> {
    let valid = day.len() == 10
        && day.as_bytes().get(4) == Some(&b'-')
        && day.as_bytes().get(7) == Some(&b'-')
        && day
            .bytes()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit());
    if valid {
        Ok(())
    } else {
        Err(StoreError::InvalidOperationalMetricDay(day.to_owned()))
    }
}

// Howard Hinnant's public-domain civil calendar conversion, with day zero at 1970-01-01.
fn civil_from_days(days_since_unix_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month as u32, day as u32)
}

struct UsageRow {
    user_id: String,
    language: String,
    engine: String,
    spoken_count: i64,
}

struct ConfiguredRow {
    guild_id: String,
    user_id: String,
    spoken_count: i64,
    user_model: Option<String>,
    guild_model: Option<String>,
    engine: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GuildConfigPatch, UserVoice};

    #[test]
    fn usage_is_aggregated_by_voice_locale_and_engine() {
        let store = SqliteStore::open_in_memory().expect("store");
        store
            .bump_talk_usage("guild", "user", "pt_PT-tugao-medium", UserEngine::Piper)
            .expect("first message");
        store
            .bump_talk_usage("guild", "user", "pt_PT-tugao-medium", UserEngine::Piper)
            .expect("second message");
        store
            .bump_talk_usage("guild", "user", "en_US-amy-medium", UserEngine::Google)
            .expect("english message");

        let usage = store
            .dominant_talk_usage(&["user".into()], DominantTalkUsageOptions::default())
            .expect("usage");
        assert_eq!(
            usage.get("user"),
            Some(&DominantTalkUsage {
                language: Some("pt_PT".into()),
                engine: Some(UserEngine::Piper),
                samples: 3,
                source: TalkUsageSource::Measured,
            })
        );
    }

    #[test]
    fn configured_fallback_never_mixes_with_measured_history() {
        let store = SqliteStore::open_in_memory().expect("store");
        store
            .connection()
            .execute(
                "INSERT INTO talk_stats (guild_id, user_id, spoken_count) VALUES ('guild', 'user', 90)",
                [],
            )
            .expect("stats");
        store
            .update_guild_config(
                "guild",
                GuildConfigPatch {
                    default_voice: Some("de_DE-thorsten-medium".into()),
                    ..GuildConfigPatch::default()
                },
            )
            .expect("config");
        store
            .set_user_voice(
                "guild",
                "user",
                &UserVoice {
                    model: "fr_FR-siwis-medium".into(),
                    speed: 1.0,
                    engine: UserEngine::Kokoro,
                },
            )
            .expect("voice");

        let fallback = store
            .dominant_talk_usage(&["user".into()], DominantTalkUsageOptions::default())
            .expect("fallback");
        assert_eq!(fallback["user"].source, TalkUsageSource::Configured);
        assert_eq!(fallback["user"].language.as_deref(), Some("fr_FR"));
        assert_eq!(fallback["user"].engine, Some(UserEngine::Kokoro));

        store
            .bump_talk_usage("guild", "user", "es_ES-davefx-medium", UserEngine::Google)
            .expect("measured");
        let measured = store
            .dominant_talk_usage(&["user".into()], DominantTalkUsageOptions::default())
            .expect("measured");
        assert_eq!(measured["user"].source, TalkUsageSource::Measured);
        assert_eq!(measured["user"].language.as_deref(), Some("es_ES"));
        assert_eq!(measured["user"].engine, Some(UserEngine::Google));
        assert_eq!(measured["user"].samples, 1);
    }

    #[test]
    fn operational_metrics_are_identity_free_and_health_keeps_transition_times() {
        let store = SqliteStore::open_in_memory().expect("store");
        store
            .add_operational_metric(
                OperationalMetric::SynthSuccess,
                OperationalProvider::Piper,
                1.4,
                Some("2026-07-22"),
            )
            .expect("first");
        store
            .add_operational_metric(
                OperationalMetric::SynthSuccess,
                OperationalProvider::Piper,
                2.0,
                Some("2026-07-22"),
            )
            .expect("second");
        assert_eq!(
            store
                .list_daily_operational_metrics(Some("2026-07-22"))
                .expect("metrics"),
            vec![DailyOperationalMetric {
                day: "2026-07-22".into(),
                metric: OperationalMetric::SynthSuccess,
                provider: OperationalProvider::Piper,
                value: 3,
            }]
        );
        assert!(
            store
                .add_operational_metric(
                    OperationalMetric::SynthFailure,
                    OperationalProvider::Piper,
                    -1.0,
                    Some("2026-07-22"),
                )
                .is_err()
        );

        store
            .set_provider_health(OperationalProvider::Piper, ProviderHealth::Healthy, 10)
            .expect("healthy");
        store
            .set_provider_health(OperationalProvider::Piper, ProviderHealth::Healthy, 20)
            .expect("same health");
        store
            .set_provider_health(OperationalProvider::Piper, ProviderHealth::Degraded, 30)
            .expect("degraded");
        assert_eq!(
            store.list_provider_health().expect("health"),
            vec![ProviderHealthSnapshot {
                provider: OperationalProvider::Piper,
                health: ProviderHealth::Degraded,
            }]
        );
        let times: (i64, Option<i64>, Option<i64>) = store
            .connection()
            .query_row(
                "SELECT changed_at, last_healthy_at, last_degraded_at FROM provider_health_state WHERE provider = 'piper'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("times");
        // Node records the most recent healthy probe even if the coarse health state did not
        // transition, while `changed_at` remains the time of the actual state transition.
        assert_eq!(times, (30, Some(20), Some(30)));
    }

    #[test]
    fn utc_day_keys_and_engine_providers_match_node_contract() {
        assert_eq!(utc_day_key_from_unix_millis(0), "1970-01-01");
        assert_eq!(utc_day_key_from_unix_millis(-1), "1969-12-31");
        assert_eq!(
            utc_day_key_from_unix_millis(1_709_251_200_000),
            "2024-03-01"
        );
        assert_eq!(
            provider_for_engine(Some(UserEngine::Google)),
            OperationalProvider::Gtts
        );
        assert_eq!(provider_for_engine(None), OperationalProvider::Gtts);
    }
}
