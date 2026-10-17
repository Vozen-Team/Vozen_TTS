//! Owner-only admin console logic.
//!
//! This is the Rust premium admin API. HTTP wiring is intentionally kept
//! separate so the authentication and money-surface decisions can be tested without opening a
//! listener. A caller must still install this service behind an owner-only route.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use vozen_store::{
    AdminMemberHistoryPoint, AdminPassRow, AdminPassesView, AdminPlusRow, DominantTalkUsageOptions,
    KofiPendingGrant, SqliteStore, StripeSubscription, TalkUsageSource, TopggSyncDetail,
    TopggSyncStatus, UserEngine,
};

use crate::admin_auth::{
    DEFAULT_ADMIN_SESSION_TTL_SECONDS, sign_admin_session, verify_admin_session,
};

const MAX_DAYS: i64 = 3_650;
const MAX_SEATS: i64 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminAuthorization {
    pub user_id: String,
    pub application_id: String,
}

#[async_trait]
pub trait AdminAuthorizationResolver: Send + Sync {
    async fn resolve_authorization(&self, bearer: &str) -> Option<AdminAuthorization>;
}

/// Resolves the small amount of Discord profile data that the owner-only Top 10 view needs.
/// Implementations must not persist the result or expose it through a public route.
#[async_trait]
pub trait AdminTalkerProfileResolver: Send + Sync {
    async fn resolve_talker_profiles(
        &self,
        user_ids: &[String],
    ) -> HashMap<String, AdminTalkerProfile>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminTalkerProfile {
    pub username: String,
    pub avatar: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminLogin {
    pub token: String,
    #[serde(rename = "expiresAt")]
    pub expires_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminPlus {
    #[serde(rename = "userId")]
    pub user_id: String,
    #[serde(rename = "expiresAt")]
    pub expires_at: i64,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminPass {
    #[serde(rename = "userId")]
    pub user_id: String,
    pub seats: i64,
    pub used: i64,
    #[serde(rename = "expiresAt")]
    pub expires_at: i64,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminPurchase {
    #[serde(rename = "subscriptionId")]
    pub subscription_id: String,
    #[serde(rename = "userId")]
    pub user_id: String,
    pub plan: String,
    pub seats: i64,
    pub status: String,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminPending {
    #[serde(rename = "transactionId")]
    pub transaction_id: String,
    #[serde(rename = "emailHash")]
    pub email_hash: Option<String>,
    pub plan: String,
    pub days: i64,
    pub seats: i64,
    #[serde(rename = "createdAt")]
    pub created_at: i64,
    #[serde(rename = "claimedAt")]
    pub claimed_at: Option<i64>,
    #[serde(rename = "isSubscription")]
    pub is_subscription: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminPasses {
    pub plus: Vec<AdminPlus>,
    pub passes: Vec<AdminPass>,
    pub pending: Vec<AdminPending>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub purchases: Vec<AdminPurchase>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminGuildBrief {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
    #[serde(rename = "memberCount")]
    pub member_count: i64,
    #[serde(rename = "joinedTimestamp")]
    pub joined_timestamp: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminGuildTopSpeaker {
    #[serde(rename = "userId")]
    pub user_id: String,
    pub count: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminGuildRow {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
    #[serde(rename = "memberCount")]
    pub member_count: i64,
    #[serde(rename = "joinedTimestamp")]
    pub joined_timestamp: Option<i64>,
    pub messages: i64,
    pub speakers: i64,
    #[serde(rename = "topSpeakers")]
    pub top_speakers: Vec<AdminGuildTopSpeaker>,
    pub streak: i64,
    #[serde(rename = "bestStreak")]
    pub best_streak: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminGuildsView {
    pub guilds: Vec<AdminGuildRow>,
    #[serde(rename = "memberHistory")]
    pub member_history: Vec<AdminMemberHistoryPoint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminTopTalker {
    pub id: String,
    pub total: i64,
    pub username: Option<String>,
    pub avatar: Option<String>,
    pub language: Option<String>,
    pub engine: Option<String>,
    #[serde(rename = "usageSamples")]
    pub usage_samples: i64,
    #[serde(rename = "usageSource")]
    pub usage_source: String,
}

/// Private growth view. It contains aggregate server counts only, so the static operator panel
/// can diagnose the funnel without receiving guild IDs or OAuth tokens.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AdminGrowth {
    pub product: &'static str,
    #[serde(rename = "currentGuilds")]
    pub current_guilds: i64,
    #[serde(rename = "baselineGuilds")]
    pub baseline_guilds: i64,
    #[serde(rename = "measurementStartedOn")]
    pub measurement_started_on: Option<String>,
    #[serde(rename = "configuredGuilds")]
    pub configured_guilds: i64,
    #[serde(rename = "usedGuilds")]
    pub used_guilds: i64,
    pub joins: i64,
    pub leaves: i64,
    pub net: i64,
    #[serde(rename = "setupCompleted")]
    pub setup_completed: i64,
    #[serde(rename = "firstValue")]
    pub first_value: i64,
    /// Authenticated, unique Top.gg deliveries in the requested date range. This is an
    /// identity-free aggregate and includes valid votes even when the reward cap is reached.
    pub votes: i64,
    #[serde(rename = "setupRate")]
    pub setup_rate: f64,
    #[serde(rename = "activationRate")]
    pub activation_rate: f64,
    #[serde(rename = "retainedW7")]
    pub retained_w7: i64,
    #[serde(rename = "eligibleW7")]
    pub eligible_w7: i64,
    #[serde(rename = "retainedW30")]
    pub retained_w30: i64,
    #[serde(rename = "eligibleW30")]
    pub eligible_w30: i64,
    pub daily: Vec<AdminGrowthDaily>,
    #[serde(rename = "topgg", skip_serializing_if = "Option::is_none")]
    pub topgg: Option<AdminTopggSync>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminGrowthDaily {
    pub day: String,
    pub source: String,
    pub joins: i64,
    pub leaves: i64,
    #[serde(rename = "setupCompleted")]
    pub setup_completed: i64,
    #[serde(rename = "firstValue")]
    pub first_value: i64,
    pub active: i64,
    pub votes: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct AdminTopggSync {
    #[serde(rename = "lastAttemptAt")]
    pub last_attempt_at: i64,
    #[serde(rename = "lastSuccessAt")]
    pub last_success_at: Option<i64>,
    #[serde(rename = "lastStatus")]
    pub last_status: Option<u16>,
    #[serde(rename = "lastDetail")]
    pub last_detail: TopggSyncDetail,
    #[serde(rename = "lastServerCount")]
    pub last_server_count: Option<i64>,
    #[serde(rename = "currentServerCount")]
    pub current_server_count: i64,
    /// Difference between the current Discord gateway count and the last count
    /// that the runtime attempted to publish. A successful v1 response is the
    /// closest server-side confirmation available without exposing Top.gg data.
    #[serde(rename = "driftPercent", skip_serializing_if = "Option::is_none")]
    pub drift_percent: Option<f64>,
    #[serde(rename = "consecutiveFailures")]
    pub consecutive_failures: i64,
    pub stale: bool,
    pub alert: bool,
}

/// Coarse owner-only operational readings. These values are intentionally aggregate-only: no
/// database paths, query contents, Discord identifiers, or individual session data are exposed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminDatabaseUsageSample {
    /// UTC calendar day (`YYYY-MM-DD`) for a single aggregate storage reading.
    pub day: String,
    #[serde(rename = "databaseBytes")]
    pub database_bytes: u64,
    /// Product footprint at the time of the reading. Defaults to zero when
    /// loading history written by an older runtime.
    #[serde(rename = "productBytes", default)]
    pub product_bytes: u64,
    #[serde(rename = "volumeTotalBytes")]
    pub volume_total_bytes: Option<u64>,
    #[serde(rename = "volumeUsedBytes")]
    pub volume_used_bytes: Option<u64>,
}

/// One real Supabase/PostgreSQL storage reading for the owner-only history view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminSupabaseUsageSample {
    /// UTC calendar day (`YYYY-MM-DD`) for a single aggregate storage reading.
    pub day: String,
    #[serde(rename = "databaseBytes")]
    pub database_bytes: u64,
    #[serde(rename = "capacityBytes")]
    pub capacity_bytes: u64,
}

/// Aggregate usage reported by the optional Supabase/PostgreSQL mirror.
///
/// The capacity is configured by the runtime because PostgreSQL can report its current
/// database size, but it cannot report the project's plan quota through the database connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminSupabaseMetrics {
    #[serde(rename = "databaseBytes")]
    pub database_bytes: u64,
    #[serde(rename = "capacityBytes")]
    pub capacity_bytes: u64,
    /// Up to seven daily Supabase readings, oldest first. These are collected locally by the
    /// runtime; missing days remain absent rather than being estimated by the API.
    pub history: Vec<AdminSupabaseUsageSample>,
}

/// A guild where Vozen is connected to a voice channel right now. The owner console receives a
/// display name only: Discord IDs and channel IDs remain private to the gateway process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminActiveVoiceServer {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdminPostgresOutboxMetrics {
    #[serde(rename = "pendingRows")]
    pub pending_rows: u64,
    #[serde(rename = "pendingBytes")]
    pub pending_bytes: u64,
    #[serde(rename = "oldestCreatedAt")]
    pub oldest_created_at: Option<i64>,
    #[serde(rename = "mirrorState")]
    pub mirror_state: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AdminSystemMetrics {
    /// Total bytes occupied by the Vozen TTS runtime data and assets. This is
    /// deliberately separate from the host volume usage below.
    #[serde(rename = "productBytes")]
    pub product_bytes: u64,
    #[serde(rename = "databaseBytes")]
    pub database_bytes: u64,
    #[serde(rename = "volumeTotalBytes")]
    pub volume_total_bytes: Option<u64>,
    #[serde(rename = "volumeUsedBytes")]
    pub volume_used_bytes: Option<u64>,
    #[serde(rename = "volumeAvailableBytes")]
    pub volume_available_bytes: Option<u64>,
    #[serde(rename = "activeVoiceSessions")]
    pub active_voice_sessions: u64,
    #[serde(rename = "activeVoiceServers")]
    pub active_voice_servers: Vec<AdminActiveVoiceServer>,
    /// Up to seven daily readings, oldest first. Missing days are represented by the client so
    /// the API never invents measurements that were not actually collected.
    #[serde(rename = "databaseHistory")]
    pub database_history: Vec<AdminDatabaseUsageSample>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supabase: Option<AdminSupabaseMetrics>,
    #[serde(rename = "postgresOutbox", skip_serializing_if = "Option::is_none")]
    pub postgres_outbox: Option<AdminPostgresOutboxMetrics>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminGrant {
    Plus { id: String, days: i64 },
    Premium { id: String, days: i64, seats: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminGrantError {
    BadId,
    BadDays,
    BadSeats,
    Store,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminRevoke {
    Plus { id: String },
    Premium { id: String },
}

#[derive(Clone)]
pub struct AdminApi {
    store: Arc<Mutex<SqliteStore>>,
    resolver: Arc<dyn AdminAuthorizationResolver>,
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
    secret: Option<Arc<str>>,
    owner_id: Option<Arc<str>>,
    client_id: Option<Arc<str>>,
    ttl_seconds: i64,
    log: Arc<dyn Fn(&str) + Send + Sync>,
    resolve_guilds: Option<Arc<dyn Fn() -> Vec<AdminGuildBrief> + Send + Sync>>,
    resolve_talker_profiles: Option<Arc<dyn AdminTalkerProfileResolver>>,
    local_day: Arc<dyn Fn() -> String + Send + Sync>,
    system_metrics: Option<Arc<dyn Fn() -> AdminSystemMetrics + Send + Sync>>,
}

pub struct AdminApiConfig {
    pub store: Arc<Mutex<SqliteStore>>,
    pub resolver: Arc<dyn AdminAuthorizationResolver>,
    pub now: Arc<dyn Fn() -> i64 + Send + Sync>,
    pub admin_session_secret: Option<String>,
    pub owner_id: Option<String>,
    pub admin_client_id: Option<String>,
    pub session_ttl_seconds: Option<i64>,
    pub log: Arc<dyn Fn(&str) + Send + Sync>,
    pub resolve_guilds: Option<Arc<dyn Fn() -> Vec<AdminGuildBrief> + Send + Sync>>,
    pub resolve_talker_profiles: Option<Arc<dyn AdminTalkerProfileResolver>>,
    pub local_day: Arc<dyn Fn() -> String + Send + Sync>,
    pub system_metrics: Option<Arc<dyn Fn() -> AdminSystemMetrics + Send + Sync>>,
}

impl AdminApi {
    #[must_use]
    pub fn new(config: AdminApiConfig) -> Self {
        let strong_secret = config
            .admin_session_secret
            .as_deref()
            .is_some_and(|secret| secret.len() >= 32);
        if config.admin_session_secret.is_some() && !strong_secret {
            (config.log)("[admin] ADMIN_SESSION_SECRET is shorter than 32 chars — admin disabled");
        }
        Self {
            store: config.store,
            resolver: config.resolver,
            now: config.now,
            secret: strong_secret.then(|| Arc::<str>::from(config.admin_session_secret.unwrap())),
            owner_id: config
                .owner_id
                .filter(|value| !value.is_empty())
                .map(Arc::from),
            client_id: config
                .admin_client_id
                .filter(|value| !value.is_empty())
                .map(Arc::from),
            ttl_seconds: config
                .session_ttl_seconds
                .unwrap_or(DEFAULT_ADMIN_SESSION_TTL_SECONDS),
            log: config.log,
            resolve_guilds: config.resolve_guilds,
            resolve_talker_profiles: config.resolve_talker_profiles,
            local_day: config.local_day,
            system_metrics: config.system_metrics,
        }
    }

    #[must_use]
    pub fn enabled(&self) -> bool {
        self.secret.is_some() && self.owner_id.is_some() && self.client_id.is_some()
    }

    pub async fn login(&self, discord_token: Option<&str>) -> Option<AdminLogin> {
        if !self.enabled() {
            return None;
        }
        let token = discord_token?;
        let auth = self.resolver.resolve_authorization(token).await?;
        if Some(auth.user_id.as_str()) != self.owner_id.as_deref()
            || Some(auth.application_id.as_str()) != self.client_id.as_deref()
        {
            return None;
        }
        let now = (self.now)();
        let ttl = self.ttl_seconds;
        let signed = sign_admin_session(&auth.user_id, self.secret.as_deref()?, now, ttl);
        Some(AdminLogin {
            token: signed,
            expires_at: (now.div_euclid(1_000) + ttl) * 1_000,
        })
    }

    #[must_use]
    pub fn authorize(&self, session_token: Option<&str>) -> Option<String> {
        if !self.enabled() {
            return None;
        }
        let user_id = verify_admin_session(session_token, self.secret.as_deref()?, (self.now)())?;
        (Some(user_id.as_str()) == self.owner_id.as_deref()).then_some(user_id)
    }

    pub fn list_passes(&self) -> Result<AdminPasses, AdminGrantError> {
        let now = (self.now)();
        let store = self.store.lock().map_err(|_| AdminGrantError::Store)?;
        let AdminPassesView { plus, passes } = store
            .list_active_premium(now)
            .map_err(|_| AdminGrantError::Store)?;
        let pending = store
            .all_unclaimed_kofi_pending(500)
            .map_err(|_| AdminGrantError::Store)?;
        let purchases = store
            .list_stripe_subscriptions(200)
            .map_err(|_| AdminGrantError::Store)?;
        Ok(AdminPasses {
            plus: plus.into_iter().map(Into::into).collect(),
            passes: passes.into_iter().map(Into::into).collect(),
            pending: pending.into_iter().map(Into::into).collect(),
            purchases: purchases.into_iter().map(Into::into).collect(),
        })
    }

    /// Same owner-only projection as `list_passes`, enriched with Discord display profiles.
    /// Profiles are resolved through the already authenticated gateway client and are never
    /// persisted in SQLite or exposed by a public route.
    pub async fn list_passes_with_profiles(&self) -> Result<AdminPasses, AdminGrantError> {
        let mut result = self.list_passes()?;
        let mut ids =
            Vec::with_capacity(result.plus.len() + result.passes.len() + result.purchases.len());
        ids.extend(result.plus.iter().map(|row| row.user_id.clone()));
        ids.extend(result.passes.iter().map(|row| row.user_id.clone()));
        ids.extend(result.purchases.iter().map(|row| row.user_id.clone()));
        ids.sort_unstable();
        ids.dedup();
        let profiles = match &self.resolve_talker_profiles {
            Some(resolver) => resolver.resolve_talker_profiles(&ids).await,
            None => HashMap::new(),
        };
        for row in &mut result.plus {
            if let Some(profile) = profiles.get(&row.user_id) {
                row.username = Some(profile.username.clone());
                row.avatar = profile.avatar.clone();
            }
        }
        for row in &mut result.passes {
            if let Some(profile) = profiles.get(&row.user_id) {
                row.username = Some(profile.username.clone());
                row.avatar = profile.avatar.clone();
            }
        }
        for row in &mut result.purchases {
            if let Some(profile) = profiles.get(&row.user_id) {
                row.username = Some(profile.username.clone());
                row.avatar = profile.avatar.clone();
            }
        }
        Ok(result)
    }

    pub fn list_guilds(&self) -> Result<Vec<AdminGuildRow>, AdminGrantError> {
        let Some(resolve_guilds) = &self.resolve_guilds else {
            return Ok(Vec::new());
        };
        let local_day = (self.local_day)();
        let store = self.store.lock().map_err(|_| AdminGrantError::Store)?;
        let mut rows = resolve_guilds()
            .into_iter()
            .map(|guild| {
                let stats = store
                    .admin_guild_stats(&guild.id, &local_day, 5)
                    .map_err(|_| AdminGrantError::Store)?;
                Ok(AdminGuildRow {
                    id: guild.id,
                    name: guild.name,
                    icon: guild.icon,
                    member_count: guild.member_count,
                    joined_timestamp: guild.joined_timestamp,
                    messages: stats.messages,
                    speakers: stats.speakers,
                    top_speakers: stats
                        .top_speakers
                        .into_iter()
                        .map(|speaker| AdminGuildTopSpeaker {
                            user_id: speaker.user_id,
                            count: speaker.count,
                        })
                        .collect(),
                    streak: stats.streak,
                    best_streak: stats.best_streak,
                })
            })
            .collect::<Result<Vec<_>, AdminGrantError>>()?;
        rows.sort_by(|left, right| {
            right
                .messages
                .cmp(&left.messages)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(rows)
    }

    pub fn list_guilds_with_history(&self) -> Result<AdminGuildsView, AdminGrantError> {
        let guilds = self.list_guilds()?;
        let store = self.store.lock().map_err(|_| AdminGrantError::Store)?;
        if !guilds.is_empty() {
            let member_count = guilds.iter().fold(0_i64, |total, guild| {
                total.saturating_add(guild.member_count.max(0))
            });
            store
                .record_admin_member_total(&(self.local_day)(), member_count, (self.now)())
                .map_err(|_| AdminGrantError::Store)?;
        }
        let member_history = store
            .list_admin_member_history(usize::MAX)
            .map_err(|_| AdminGrantError::Store)?;
        Ok(AdminGuildsView {
            guilds,
            member_history,
        })
    }

    pub fn growth(&self, from_day: &str, to_day: &str) -> Result<AdminGrowth, AdminGrantError> {
        let now = (self.now)();
        let store = self.store.lock().map_err(|_| AdminGrantError::Store)?;
        let overview = store
            .growth_overview(now)
            .map_err(|_| AdminGrantError::Store)?;
        let daily = store
            .list_growth_daily_metrics(from_day, to_day)
            .map_err(|_| AdminGrantError::Store)?;
        let joins = daily.iter().map(|point| point.joins).sum::<i64>();
        let leaves = daily.iter().map(|point| point.leaves).sum::<i64>();
        let setup_completed = daily.iter().map(|point| point.setup_completed).sum::<i64>();
        let first_value = daily.iter().map(|point| point.first_value).sum::<i64>();
        let votes = daily.iter().map(|point| point.votes).sum::<i64>();
        let rate = |numerator: i64| {
            if joins == 0 {
                0.0
            } else {
                numerator as f64 / joins as f64
            }
        };
        Ok(AdminGrowth {
            product: "tts",
            current_guilds: overview.current_guilds,
            baseline_guilds: overview.baseline_guilds,
            measurement_started_on: store
                .growth_measurement_started_on()
                .map_err(|_| AdminGrantError::Store)?,
            configured_guilds: overview.configured_guilds,
            used_guilds: overview.used_guilds,
            joins,
            leaves,
            net: joins - leaves,
            setup_completed,
            first_value,
            votes,
            setup_rate: rate(setup_completed),
            activation_rate: rate(first_value),
            retained_w7: overview.retained_w7,
            eligible_w7: overview.eligible_w7,
            retained_w30: overview.retained_w30,
            eligible_w30: overview.eligible_w30,
            daily: daily
                .into_iter()
                .map(|point| AdminGrowthDaily {
                    day: point.day,
                    source: point.source,
                    joins: point.joins,
                    leaves: point.leaves,
                    setup_completed: point.setup_completed,
                    first_value: point.first_value,
                    active: point.active,
                    votes: point.votes,
                })
                .collect(),
            topgg: store
                .topgg_sync_status(now)
                .map_err(|_| AdminGrantError::Store)?
                .map(|status| AdminTopggSync::from_status(status, overview.current_guilds)),
        })
    }

    pub async fn list_top_talkers(&self) -> Result<Vec<AdminTopTalker>, AdminGrantError> {
        let (rows, usage) = {
            let store = self.store.lock().map_err(|_| AdminGrantError::Store)?;
            let rows = store
                .admin_top_talkers(10)
                .map_err(|_| AdminGrantError::Store)?;
            let ids = rows
                .iter()
                .map(|row| row.user_id.clone())
                .collect::<Vec<_>>();
            let usage = store
                .dominant_talk_usage(&ids, DominantTalkUsageOptions::default())
                .map_err(|_| AdminGrantError::Store)?;
            (rows, usage)
        };
        let ids = rows
            .iter()
            .map(|row| row.user_id.clone())
            .collect::<Vec<_>>();
        let profiles = match &self.resolve_talker_profiles {
            Some(resolver) => resolver.resolve_talker_profiles(&ids).await,
            None => HashMap::new(),
        };
        Ok(rows
            .into_iter()
            .map(|row| {
                let dominant = usage.get(&row.user_id);
                let profile = profiles.get(&row.user_id);
                AdminTopTalker {
                    id: row.user_id,
                    total: row.total,
                    username: profile.map(|value| value.username.clone()),
                    avatar: profile.and_then(|value| value.avatar.clone()),
                    language: dominant.and_then(|value| value.language.clone()),
                    engine: dominant
                        .and_then(|value| value.engine.map(|engine| engine_key(engine).to_owned())),
                    usage_samples: dominant.map_or(0, |value| value.samples),
                    usage_source: dominant
                        .map_or("none", |value| match value.source {
                            TalkUsageSource::Measured => "measured",
                            TalkUsageSource::Configured => "configured",
                            TalkUsageSource::None => "none",
                        })
                        .to_owned(),
                }
            })
            .collect())
    }

    #[must_use]
    pub fn system_metrics(&self) -> AdminSystemMetrics {
        self.system_metrics
            .as_ref()
            .map(|provider| provider())
            .unwrap_or_default()
    }

    pub fn grant(&self, grant: AdminGrant) -> Result<i64, AdminGrantError> {
        let now = (self.now)();
        let (kind, id, days, seats) = match grant {
            AdminGrant::Plus { id, days } => ("plus", id, days, None),
            AdminGrant::Premium { id, days, seats } => ("premium", id, days, Some(seats)),
        };
        if !valid_snowflake(&id) {
            return Err(AdminGrantError::BadId);
        }
        if !(1..=MAX_DAYS).contains(&days) {
            return Err(AdminGrantError::BadDays);
        }
        if seats.is_some_and(|seats| !(1..=MAX_SEATS).contains(&seats)) {
            return Err(AdminGrantError::BadSeats);
        }
        let store = self.store.lock().map_err(|_| AdminGrantError::Store)?;
        let expires_at = if kind == "plus" {
            store
                .grant_user_premium(&id, days, "manual", now)
                .map_err(|_| AdminGrantError::Store)?
        } else {
            store
                .grant_guild_pass(
                    &id,
                    seats.expect("validated premium seats"),
                    days,
                    "manual",
                    now,
                )
                .map_err(|_| AdminGrantError::Store)?
        };
        (self.log)(&format!("[admin] grant {kind} {id} {days}d"));
        Ok(expires_at)
    }

    pub fn revoke(&self, revoke: AdminRevoke) -> Result<bool, AdminGrantError> {
        let (kind, id) = match revoke {
            AdminRevoke::Plus { id } => ("plus", id),
            AdminRevoke::Premium { id } => ("premium", id),
        };
        if !valid_snowflake(&id) {
            return Ok(false);
        }
        let store = self.store.lock().map_err(|_| AdminGrantError::Store)?;
        let ok = if kind == "plus" {
            store.revoke_user_premium(&id)
        } else {
            store.revoke_guild_pass(&id)
        }
        .map_err(|_| AdminGrantError::Store)?;
        (self.log)(&format!("[admin] revoke {kind} {id} -> {ok}"));
        Ok(ok)
    }
}

impl AdminTopggSync {
    fn from_status(value: TopggSyncStatus, current_server_count: i64) -> Self {
        let drift_percent = value.last_server_count.map(|reported| {
            if reported == 0 {
                if current_server_count == 0 { 0.0 } else { 1.0 }
            } else {
                (current_server_count.saturating_sub(reported).unsigned_abs() as f64)
                    / reported.unsigned_abs().max(1) as f64
            }
        });
        let alert = value.stale || drift_percent.is_some_and(|value| value > 0.05);
        Self {
            last_attempt_at: value.last_attempt_at,
            last_success_at: value.last_success_at,
            last_status: value.last_status,
            last_detail: value.last_detail,
            last_server_count: value.last_server_count,
            current_server_count,
            drift_percent,
            consecutive_failures: value.consecutive_failures,
            stale: value.stale,
            alert,
        }
    }
}

fn valid_snowflake(value: &str) -> bool {
    !value.is_empty() && value.len() <= 20 && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn engine_key(engine: UserEngine) -> &'static str {
    match engine {
        UserEngine::Google => "google",
        UserEngine::Piper => "piper",
        UserEngine::Kokoro => "kokoro",
        UserEngine::Gcloud => "gcloud",
    }
}

impl From<AdminPlusRow> for AdminPlus {
    fn from(value: AdminPlusRow) -> Self {
        Self {
            user_id: value.user_id,
            expires_at: value.expires_at,
            source: value.source,
            username: None,
            avatar: None,
        }
    }
}

impl From<AdminPassRow> for AdminPass {
    fn from(value: AdminPassRow) -> Self {
        Self {
            user_id: value.user_id,
            seats: value.seats,
            used: value.used,
            expires_at: value.expires_at,
            source: value.source,
            username: None,
            avatar: None,
        }
    }
}

impl From<StripeSubscription> for AdminPurchase {
    fn from(value: StripeSubscription) -> Self {
        Self {
            subscription_id: value.subscription_id,
            user_id: value.user_id,
            plan: value.plan,
            seats: value.seats,
            status: value.status,
            created_at: value.updated_at,
            username: None,
            avatar: None,
        }
    }
}

impl From<KofiPendingGrant> for AdminPending {
    fn from(value: KofiPendingGrant) -> Self {
        Self {
            transaction_id: value.input.transaction_id,
            email_hash: value.input.email_hash,
            plan: value.input.plan.as_str().to_owned(),
            days: value.input.days,
            seats: value.input.seats,
            created_at: value.created_at,
            claimed_at: value.claimed_at,
            is_subscription: value.input.is_subscription,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use vozen_store::StripeSubscriptionInput;

    const OWNER: &str = "1523489275155583056";
    const CLIENT: &str = "1526211106081734666";
    const SECRET: &str = "sess-secret-abcdefghijklmnopqrstuvwxyz";
    const NOW: i64 = 1_700_000_000_000;

    struct Resolver;
    #[async_trait]
    impl AdminAuthorizationResolver for Resolver {
        async fn resolve_authorization(&self, bearer: &str) -> Option<AdminAuthorization> {
            match bearer {
                "owner-token" => Some(AdminAuthorization {
                    user_id: OWNER.into(),
                    application_id: CLIENT.into(),
                }),
                "wrong-user" => Some(AdminAuthorization {
                    user_id: "999999999999999999".into(),
                    application_id: CLIENT.into(),
                }),
                "wrong-app" => Some(AdminAuthorization {
                    user_id: OWNER.into(),
                    application_id: "999000999000999000".into(),
                }),
                _ => None,
            }
        }
    }

    struct TalkerProfiles;
    #[async_trait]
    impl AdminTalkerProfileResolver for TalkerProfiles {
        async fn resolve_talker_profiles(
            &self,
            user_ids: &[String],
        ) -> HashMap<String, AdminTalkerProfile> {
            user_ids
                .iter()
                .filter(|user_id| user_id.as_str() == "user")
                .map(|user_id| {
                    (
                        user_id.clone(),
                        AdminTalkerProfile {
                            username: "Ana".into(),
                            avatar: Some("https://cdn.discordapp.com/avatars/user/hash.png".into()),
                        },
                    )
                })
                .collect()
        }
    }

    fn api() -> AdminApi {
        AdminApi::new(AdminApiConfig {
            store: Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store"))),
            resolver: Arc::new(Resolver),
            now: Arc::new(|| NOW),
            admin_session_secret: Some(SECRET.into()),
            owner_id: Some(OWNER.into()),
            admin_client_id: Some(CLIENT.into()),
            session_ttl_seconds: None,
            log: Arc::new(|_| {}),
            resolve_guilds: None,
            resolve_talker_profiles: None,
            local_day: Arc::new(|| "2026-07-23".into()),
            system_metrics: None,
        })
    }

    #[test]
    fn guild_member_history_includes_the_first_reading_beyond_seven_days() {
        let api = api();
        {
            let store = api.store.lock().expect("store");
            for day in 1..=12 {
                store
                    .record_admin_member_total(&format!("2026-07-{day:02}"), day * 10, NOW)
                    .expect("daily reading");
            }
        }
        let view = api.list_guilds_with_history().expect("guild history");
        assert_eq!(view.member_history.len(), 12);
        assert_eq!(view.member_history[0].day, "2026-07-01");
        assert_eq!(view.member_history[0].count, 10);
        assert_eq!(view.member_history[11].day, "2026-07-12");
    }

    #[tokio::test]
    async fn login_binds_owner_and_oauth_application_and_authorize_is_session_only() {
        let api = api();
        let login = api.login(Some("owner-token")).await.expect("login");
        assert_eq!(api.authorize(Some(&login.token)).as_deref(), Some(OWNER));
        assert!(api.login(Some("wrong-user")).await.is_none());
        assert!(api.login(Some("wrong-app")).await.is_none());
        assert!(api.authorize(Some("owner-token")).is_none());
    }

    #[test]
    fn wide_growth_window_separates_current_inventory_from_measured_net_change() {
        let api = api();
        {
            let store = api.store.lock().expect("store");
            for index in 0..168 {
                store
                    .record_guild_join(&format!("baseline-{index}"), Some("baseline"), NOW)
                    .expect("baseline join");
            }
            for index in 0..7 {
                store
                    .record_guild_join(&format!("new-{index}"), Some("tts-hero"), NOW)
                    .expect("new join");
            }
            for index in 0..3 {
                store
                    .record_guild_departure(&format!("new-{index}"), NOW + 1)
                    .expect("departure");
            }
        }

        let growth = api.growth("2023-08-17", "2023-11-14").expect("growth");
        assert_eq!(growth.current_guilds, 172);
        assert_eq!(growth.baseline_guilds, 168);
        assert_eq!(growth.measurement_started_on.as_deref(), Some("2023-11-14"));
        assert_eq!(growth.joins, 7);
        assert_eq!(growth.leaves, 3);
        assert_eq!(growth.net, 4);
    }

    #[test]
    fn growth_contract_exposes_unique_valid_votes_for_the_requested_window() {
        let api = api();
        {
            let store = api.store.lock().expect("store");
            store
                .claim_topgg_vote_reward(
                    Some("vote-event"),
                    "12345678901234567",
                    NOW,
                    "0123456789abcdef0123456789abcdef",
                )
                .expect("vote");
        }

        let growth = api.growth("2023-11-14", "2023-11-14").expect("growth");
        assert_eq!(growth.votes, 1);
        assert_eq!(growth.daily.len(), 1);
        assert_eq!(growth.daily[0].votes, 1);
    }

    #[test]
    fn topgg_alerts_after_ninety_minutes_or_more_than_five_percent_drift() {
        let status = TopggSyncStatus {
            last_attempt_at: NOW,
            last_success_at: Some(NOW),
            last_status: Some(204),
            last_server_count: Some(100),
            last_detail: TopggSyncDetail::Delivered,
            consecutive_failures: 0,
            stale: false,
        };
        let boundary = AdminTopggSync::from_status(status, 105);
        assert_eq!(boundary.drift_percent, Some(0.05));
        assert!(
            !boundary.alert,
            "the threshold is strictly greater than five percent"
        );
        let drifted = AdminTopggSync::from_status(status, 106);
        assert!(drifted.alert);
        assert_eq!(drifted.current_server_count, 106);
        let stale = AdminTopggSync::from_status(
            TopggSyncStatus {
                stale: true,
                ..status
            },
            100,
        );
        assert!(stale.alert);
    }

    #[test]
    fn grants_reject_bad_ids_and_lists_active_rows() {
        let api = api();
        assert_eq!(
            api.grant(AdminGrant::Plus {
                id: "not-id".into(),
                days: 30
            }),
            Err(AdminGrantError::BadId)
        );
        assert_eq!(
            api.grant(AdminGrant::Plus {
                id: "111".into(),
                days: 30
            })
            .expect("grant"),
            NOW + 30 * 86_400_000
        );
        assert!(
            api.list_passes()
                .expect("list")
                .plus
                .iter()
                .any(|row| row.user_id == "111")
        );
        assert!(
            api.revoke(AdminRevoke::Plus { id: "111".into() })
                .expect("revoke")
        );
        assert!(
            !api.revoke(AdminRevoke::Plus {
                id: "111\nforged".into()
            })
            .expect("invalid")
        );
    }

    #[tokio::test]
    async fn guilds_and_top_talkers_use_only_stored_aggregates() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        {
            let guard = store.lock().unwrap();
            guard
                .bump_talk("guild", "user", "2026-07-23")
                .expect("talk");
            guard
                .bump_talk("guild", "user", "2026-07-23")
                .expect("talk");
        }
        let api = AdminApi::new(AdminApiConfig {
            store,
            resolver: Arc::new(Resolver),
            now: Arc::new(|| NOW),
            admin_session_secret: Some(SECRET.into()),
            owner_id: Some(OWNER.into()),
            admin_client_id: Some(CLIENT.into()),
            session_ttl_seconds: None,
            log: Arc::new(|_| {}),
            resolve_guilds: Some(Arc::new(|| {
                vec![AdminGuildBrief {
                    id: "guild".into(),
                    name: "Test".into(),
                    icon: None,
                    member_count: 4,
                    joined_timestamp: None,
                }]
            })),
            resolve_talker_profiles: Some(Arc::new(TalkerProfiles)),
            local_day: Arc::new(|| "2026-07-23".into()),
            system_metrics: None,
        });
        assert_eq!(api.list_guilds().expect("guilds")[0].messages, 2);
        let guilds = api.list_guilds_with_history().expect("guilds with history");
        assert_eq!(guilds.guilds.len(), 1);
        assert_eq!(
            guilds.member_history.last().map(|point| point.count),
            Some(4)
        );
        let talker = api.list_top_talkers().await.expect("talkers").remove(0);
        assert_eq!(talker.total, 2);
        assert_eq!(talker.username.as_deref(), Some("Ana"));
        assert_eq!(
            talker.avatar.as_deref(),
            Some("https://cdn.discordapp.com/avatars/user/hash.png")
        );
    }

    #[tokio::test]
    async fn passes_enrich_stripe_purchases_and_active_grants_with_discord_profiles() {
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        {
            let guard = store.lock().unwrap();
            guard
                .upsert_stripe_subscription(&StripeSubscriptionInput {
                    subscription_id: "sub_profiled".into(),
                    customer_id: "cus_profiled".into(),
                    user_id: "user".into(),
                    plan: "plus".into(),
                    seats: 1,
                    current_period_end: 0,
                    status: "active".into(),
                    updated_at: NOW,
                })
                .expect("stripe subscription");
            guard
                .grant_user_premium("user", 30, "stripe", NOW)
                .expect("active grant");
        }
        let api = AdminApi::new(AdminApiConfig {
            store,
            resolver: Arc::new(Resolver),
            now: Arc::new(|| NOW),
            admin_session_secret: Some(SECRET.into()),
            owner_id: Some(OWNER.into()),
            admin_client_id: Some(CLIENT.into()),
            session_ttl_seconds: None,
            log: Arc::new(|_| {}),
            resolve_guilds: None,
            resolve_talker_profiles: Some(Arc::new(TalkerProfiles)),
            local_day: Arc::new(|| "2026-07-23".into()),
            system_metrics: None,
        });

        let result = api
            .list_passes_with_profiles()
            .await
            .expect("passes projection");
        let plus = result
            .plus
            .iter()
            .find(|row| row.user_id == "user")
            .expect("active plus");
        assert_eq!(plus.username.as_deref(), Some("Ana"));
        assert_eq!(
            plus.avatar.as_deref(),
            Some("https://cdn.discordapp.com/avatars/user/hash.png")
        );
        let purchase = result
            .purchases
            .iter()
            .find(|row| row.user_id == "user")
            .expect("stripe purchase");
        assert_eq!(purchase.username.as_deref(), Some("Ana"));
        assert_eq!(purchase.plan, "plus");
    }
}
