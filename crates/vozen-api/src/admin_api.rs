//! Owner-only admin console logic.
//!
//! This is the Rust premium admin API. HTTP wiring is intentionally kept
//! separate so the authentication and money-surface decisions can be tested without opening a
//! listener. A caller must still install this service behind an owner-only route.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::Serialize;
use vozen_store::{
    AdminPassRow, AdminPassesView, AdminPlusRow, DominantTalkUsageOptions, KofiPendingGrant,
    SqliteStore, TalkUsageSource, UserEngine,
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
    local_day: Arc<dyn Fn() -> String + Send + Sync>,
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
    pub local_day: Arc<dyn Fn() -> String + Send + Sync>,
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
            local_day: config.local_day,
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
        Ok(AdminPasses {
            plus: plus.into_iter().map(Into::into).collect(),
            passes: passes.into_iter().map(Into::into).collect(),
            pending: pending.into_iter().map(Into::into).collect(),
        })
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

    pub fn list_top_talkers(&self) -> Result<Vec<AdminTopTalker>, AdminGrantError> {
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
        Ok(rows
            .into_iter()
            .map(|row| {
                let dominant = usage.get(&row.user_id);
                AdminTopTalker {
                    id: row.user_id,
                    total: row.total,
                    username: None,
                    avatar: None,
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

    const OWNER: &str = "100000000000000001";
    const CLIENT: &str = "100000000000000003";
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
            local_day: Arc::new(|| "2026-07-23".into()),
        })
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

    #[test]
    fn guilds_and_top_talkers_use_only_stored_aggregates() {
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
            local_day: Arc::new(|| "2026-07-23".into()),
        });
        assert_eq!(api.list_guilds().expect("guilds")[0].messages, 2);
        assert_eq!(api.list_top_talkers().expect("talkers")[0].total, 2);
    }
}
