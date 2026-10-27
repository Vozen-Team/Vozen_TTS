//! Discord OAuth adapter for the dashboard authorization boundary.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::dashboard_api::{DashboardAccess, DashboardAuthorizer, ManageableGuild};

const DISCORD_OAUTH_ME: &str = "https://discord.com/api/v10/oauth2/@me";
const DISCORD_GUILDS: &str = "https://discord.com/api/v10/users/@me/guilds";
const DASHBOARD_TIMEOUT: Duration = Duration::from_secs(5);
const CACHE_TTL_MS: i64 = 60_000;
const CACHE_MAX_ENTRIES: usize = 512;
const MANAGE_GUILD: u64 = 0x20;
const ADMINISTRATOR: u64 = 0x8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashboardDiscordError {
    InvalidToken,
    Unavailable,
}

#[async_trait]
pub trait DashboardDiscordHttp: Send + Sync {
    async fn get_json(
        &self,
        url: &'static str,
        bearer: &str,
    ) -> Result<Value, DashboardDiscordError>;
}

#[derive(Clone)]
pub struct ReqwestDashboardDiscordHttp {
    client: reqwest::Client,
}

impl ReqwestDashboardDiscordHttp {
    pub fn new() -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(DASHBOARD_TIMEOUT)
                .build()?,
        })
    }
}

#[async_trait]
impl DashboardDiscordHttp for ReqwestDashboardDiscordHttp {
    async fn get_json(
        &self,
        url: &'static str,
        bearer: &str,
    ) -> Result<Value, DashboardDiscordError> {
        // Bound the complete exchange, including the single rate-limit retry.
        tokio::time::timeout(DASHBOARD_TIMEOUT, async {
            for attempt in 0..2 {
                let response = self
                    .client
                    .get(url)
                    .bearer_auth(bearer)
                    .send()
                    .await
                    .map_err(|_| DashboardDiscordError::Unavailable)?;
                match response.status().as_u16() {
                    401 | 403 => return Err(DashboardDiscordError::InvalidToken),
                    429 if attempt == 0 => {
                        let seconds = response
                            .headers()
                            .get(reqwest::header::RETRY_AFTER)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|v| v.parse::<f64>().ok());
                        let seconds = match seconds {
                            Some(value) => Some(value),
                            None => response
                                .json::<Value>()
                                .await
                                .ok()
                                .and_then(|v| v.get("retry_after").and_then(Value::as_f64)),
                        };
                        let delay = seconds
                            .filter(|v| v.is_finite() && *v >= 0.0 && *v <= 2.0)
                            .map(|v| Duration::from_secs_f64(v.max(0.001)))
                            .ok_or(DashboardDiscordError::Unavailable)?;
                        tokio::time::sleep(delay).await;
                    }
                    _ if response.status().is_success() => {
                        return response
                            .json()
                            .await
                            .map_err(|_| DashboardDiscordError::Unavailable);
                    }
                    _ => return Err(DashboardDiscordError::Unavailable),
                }
            }
            Err(DashboardDiscordError::Unavailable)
        })
        .await
        .map_err(|_| DashboardDiscordError::Unavailable)?
    }
}

type BotPresence = Arc<dyn Fn(&str) -> bool + Send + Sync>;

#[derive(Clone)]
pub struct DiscordDashboardAuthorizer {
    // Dashboard tokens may come from the Ecosystem login app or from a
    // product app used by an "Add server" invite. Both IDs are explicit
    // operator configuration; no arbitrary Discord application is trusted.
    expected_client_ids: Arc<Vec<Arc<str>>>,
    http: Arc<dyn DashboardDiscordHttp>,
    bot_has_guild: BotPresence,
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
    cache: Arc<Mutex<HashMap<[u8; 32], CachedGuilds>>>,
}

#[derive(Clone)]
struct CachedGuilds {
    guilds: Option<Vec<ManageableGuild>>,
    expires_at: i64,
}

impl DiscordDashboardAuthorizer {
    pub fn production(
        expected_client_id: impl Into<String>,
        bot_has_guild: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self::new(
            expected_client_id,
            Arc::new(ReqwestDashboardDiscordHttp::new()?),
            Arc::new(bot_has_guild),
            Arc::new(system_now_ms),
        ))
    }

    pub fn production_with_client_ids(
        expected_client_ids: impl IntoIterator<Item = String>,
        bot_has_guild: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self::new_with_client_ids(
            expected_client_ids,
            Arc::new(ReqwestDashboardDiscordHttp::new()?),
            Arc::new(bot_has_guild),
            Arc::new(system_now_ms),
        ))
    }

    pub fn new(
        expected_client_id: impl Into<String>,
        http: Arc<dyn DashboardDiscordHttp>,
        bot_has_guild: BotPresence,
        now: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        Self::new_with_client_ids([expected_client_id.into()], http, bot_has_guild, now)
    }

    pub fn new_with_client_ids(
        expected_client_ids: impl IntoIterator<Item = String>,
        http: Arc<dyn DashboardDiscordHttp>,
        bot_has_guild: BotPresence,
        now: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        let expected_client_ids = expected_client_ids
            .into_iter()
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty())
            .map(Arc::<str>::from)
            .collect();
        Self {
            expected_client_ids: Arc::new(expected_client_ids),
            http,
            bot_has_guild,
            now,
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn fetch_manageable(
        &self,
        bearer: &str,
    ) -> Result<Vec<ManageableGuild>, DashboardDiscordError> {
        let now = (self.now)();
        let key = token_hash(bearer);
        if let Some(cached) = self.cache_get(key, now) {
            return cached.ok_or(DashboardDiscordError::InvalidToken);
        }
        self.refresh_manageable(bearer).await
    }

    async fn refresh_manageable(
        &self,
        bearer: &str,
    ) -> Result<Vec<ManageableGuild>, DashboardDiscordError> {
        let now = (self.now)();
        let key = token_hash(bearer);
        let guilds = async {
            let oauth = self.http.get_json(DISCORD_OAUTH_ME, bearer).await?;
            if !oauth_authorized(&oauth, &self.expected_client_ids) {
                return Err(DashboardDiscordError::InvalidToken);
            }
            let guilds = self.http.get_json(DISCORD_GUILDS, bearer).await?;
            parse_manageable_guilds(guilds, &self.bot_has_guild)
                .ok_or(DashboardDiscordError::Unavailable)
        }
        .await;
        match &guilds {
            Ok(rows) => self.cache_put(key, Some(rows.clone()), now),
            Err(DashboardDiscordError::InvalidToken) => self.cache_put(key, None, now),
            Err(DashboardDiscordError::Unavailable) => {
                if let Ok(mut cache) = self.cache.lock() {
                    cache.remove(&key);
                }
            }
        }
        guilds
    }

    fn cache_get(&self, key: [u8; 32], now: i64) -> Option<Option<Vec<ManageableGuild>>> {
        let Ok(mut cache) = self.cache.lock() else {
            return None;
        };
        cache.retain(|_, item| item.expires_at > now);
        cache.get(&key).map(|item| item.guilds.clone())
    }

    fn cache_put(&self, key: [u8; 32], guilds: Option<Vec<ManageableGuild>>, now: i64) {
        let Ok(mut cache) = self.cache.lock() else {
            return;
        };
        cache.retain(|_, item| item.expires_at > now);
        while cache.len() >= CACHE_MAX_ENTRIES && !cache.contains_key(&key) {
            let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, value)| value.expires_at)
                .map(|(key, _)| *key)
            else {
                break;
            };
            cache.remove(&oldest);
        }
        cache.insert(
            key,
            CachedGuilds {
                guilds,
                expires_at: now.saturating_add(CACHE_TTL_MS),
            },
        );
    }
}

#[async_trait]
impl DashboardAuthorizer for DiscordDashboardAuthorizer {
    async fn manageable_guilds(&self, bearer: &str) -> DashboardAccess<Vec<ManageableGuild>> {
        match self.fetch_manageable(bearer).await {
            Ok(rows) => DashboardAccess::Allowed(rows),
            Err(DashboardDiscordError::InvalidToken) => DashboardAccess::Unauthenticated,
            Err(DashboardDiscordError::Unavailable) => DashboardAccess::Unavailable,
        }
    }

    async fn authorize_guild(&self, bearer: &str, guild_id: &str) -> DashboardAccess<()> {
        // A cached server list is only a navigation hint, never authority for a guild operation.
        let guilds = match self.refresh_manageable(bearer).await {
            Ok(rows) => rows,
            Err(DashboardDiscordError::InvalidToken) => return DashboardAccess::Unauthenticated,
            Err(DashboardDiscordError::Unavailable) => return DashboardAccess::Unavailable,
        };
        if !(self.bot_has_guild)(guild_id) || !guilds.iter().any(|guild| guild.id == guild_id) {
            DashboardAccess::Forbidden
        } else {
            DashboardAccess::Allowed(())
        }
    }
}

fn oauth_authorized(value: &Value, expected_client_ids: &[Arc<str>]) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    let application_id = object
        .get("application")
        .and_then(Value::as_object)
        .and_then(|application| application.get("id"))
        .and_then(Value::as_str);
    let scopes: Vec<_> = object
        .get("scopes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    expected_client_ids
        .iter()
        .any(|expected| application_id == Some(expected.as_ref()))
        && scopes.contains(&"identify")
        && scopes.contains(&"guilds")
}

fn parse_manageable_guilds(
    value: Value,
    bot_has_guild: &BotPresence,
) -> Option<Vec<ManageableGuild>> {
    let rows = value.as_array()?;
    Some(
        rows.iter()
            .filter_map(|row| {
                let object = row.as_object()?;
                let id = object.get("id")?.as_str()?;
                if !(bot_has_guild)(id)
                    || !can_manage(object.get("permissions"), object.get("owner"))
                {
                    return None;
                }
                Some(ManageableGuild {
                    id: id.to_owned(),
                    name: object
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_owned(),
                    icon: object
                        .get("icon")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                })
            })
            .collect(),
    )
}

fn can_manage(permissions: Option<&Value>, owner: Option<&Value>) -> bool {
    if owner == Some(&Value::Bool(true)) {
        return true;
    }
    let bits = match permissions {
        Some(Value::String(value)) => value.parse::<u64>().ok(),
        Some(Value::Number(value)) => value.as_u64(),
        _ => None,
    };
    bits.is_some_and(|bits| bits & MANAGE_GUILD != 0 || bits & ADMINISTRATOR != 0)
}

fn token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}
fn system_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn temporary_upstream_failure_is_503_without_mutating_configuration() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;
        let (auth, http) = mutable_authorizer();
        let store = Arc::new(Mutex::new(
            vozen_store::SqliteStore::open_in_memory().unwrap(),
        ));
        let guild = "100000000000000010";
        let before = store.lock().unwrap().guild_config(guild).unwrap();
        let app =
            crate::dashboard_api::dashboard_router(crate::dashboard_api::DashboardApiConfig {
                origin: "http://127.0.0.1:18765".into(),
                store: store.clone(),
                authorizer: Arc::new(auth),
                options: Arc::new(EmptyOptions),
            })
            .unwrap();
        *http.oauth.lock().unwrap() = Err(DashboardDiscordError::Unavailable);
        for (method, path, body) in [
            ("GET", "/api/dashboard/guilds".to_owned(), ""),
            (
                "POST",
                format!("/api/dashboard/guild/{guild}"),
                r#"{"maxChars":7}"#,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("Authorization", "Bearer synthetic")
                        .header("Content-Type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                axum::http::StatusCode::SERVICE_UNAVAILABLE
            );
            assert_eq!(store.lock().unwrap().guild_config(guild).unwrap(), before);
        }
    }

    #[tokio::test]
    async fn rate_limit_retry_waits_and_is_bounded() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let app = axum::Router::new().route(
            "/limited",
            axum::routing::get(move || {
                let index = counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    if index == 0 {
                        (
                            axum::http::StatusCode::TOO_MANY_REQUESTS,
                            [("Retry-After", "0.05")],
                            axum::Json(json!({"retry_after":0.05})),
                        )
                    } else {
                        (
                            axum::http::StatusCode::OK,
                            [("Retry-After", "0")],
                            axum::Json(json!({"ok":true})),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url: &'static str = Box::leak(
            format!("http://{}/limited", listener.local_addr().unwrap()).into_boxed_str(),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let started = tokio::time::Instant::now();
        let result = ReqwestDashboardDiscordHttp::new()
            .unwrap()
            .get_json(url, "synthetic")
            .await;
        server.abort();
        assert_eq!(result, Ok(json!({"ok":true})));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(started.elapsed() >= Duration::from_millis(50));
    }

    #[tokio::test]
    async fn upstream_errors_do_not_retry_forever_or_retry_before_long_limits() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for (status, retry, expected_calls, expected_error) in [
            (401, "0", 1, DashboardDiscordError::InvalidToken),
            (403, "0", 1, DashboardDiscordError::InvalidToken),
            (500, "0", 1, DashboardDiscordError::Unavailable),
            (429, "2.1", 1, DashboardDiscordError::Unavailable),
            (429, "NaN", 1, DashboardDiscordError::Unavailable),
            (429, "-1", 1, DashboardDiscordError::Unavailable),
            (429, "0", 2, DashboardDiscordError::Unavailable),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let counter = calls.clone();
            let app = axum::Router::new().route(
                "/limited",
                axum::routing::get(move || {
                    counter.fetch_add(1, Ordering::SeqCst);
                    async move {
                        (
                            axum::http::StatusCode::from_u16(status).unwrap(),
                            [("Retry-After", retry)],
                            axum::Json(json!({"error":"synthetic"})),
                        )
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url: &'static str = Box::leak(
                format!("http://{}/limited", listener.local_addr().unwrap()).into_boxed_str(),
            );
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let result = ReqwestDashboardDiscordHttp::new()
                .unwrap()
                .get_json(url, "synthetic")
                .await;
            server.abort();
            assert_eq!(result, Err(expected_error));
            assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
        }
    }

    #[tokio::test]
    async fn temporary_failure_invalidates_navigation_cache_and_allows_fresh_recovery() {
        let (auth, http) = mutable_authorizer();
        let guild = "100000000000000010";
        assert!(matches!(
            auth.manageable_guilds("synthetic").await,
            DashboardAccess::Allowed(_)
        ));
        *http.oauth.lock().unwrap() = Err(DashboardDiscordError::Unavailable);
        assert_eq!(
            auth.authorize_guild("synthetic", guild).await,
            DashboardAccess::Unavailable
        );
        *http.oauth.lock().unwrap() =
            Ok(json!({"application":{"id":"our-app"},"scopes":["identify","guilds"]}));
        *http.guilds.lock().unwrap() = json!([{"id":guild,"permissions":"0","owner":false}]);
        assert_eq!(
            auth.manageable_guilds("synthetic").await,
            DashboardAccess::Allowed(vec![])
        );
        assert_eq!(
            auth.authorize_guild("synthetic", guild).await,
            DashboardAccess::Forbidden
        );
        *http.guilds.lock().unwrap() = json!({"invalid":"upstream-response"});
        assert_eq!(
            auth.authorize_guild("synthetic", guild).await,
            DashboardAccess::Unavailable
        );
    }

    struct MutableHttp {
        oauth: Mutex<Result<Value, DashboardDiscordError>>,
        guilds: Mutex<Value>,
    }
    #[async_trait]
    impl DashboardDiscordHttp for MutableHttp {
        async fn get_json(
            &self,
            url: &'static str,
            _bearer: &str,
        ) -> Result<Value, DashboardDiscordError> {
            match url {
                DISCORD_OAUTH_ME => self.oauth.lock().unwrap().clone(),
                DISCORD_GUILDS => Ok(self.guilds.lock().unwrap().clone()),
                _ => unreachable!(),
            }
        }
    }
    fn mutable_authorizer() -> (DiscordDashboardAuthorizer, Arc<MutableHttp>) {
        let http = Arc::new(MutableHttp {
            oauth: Mutex::new(Ok(
                json!({"application":{"id":"our-app"},"scopes":["identify","guilds"]}),
            )),
            guilds: Mutex::new(
                json!([{"id":"100000000000000010","permissions":"32","owner":false}]),
            ),
        });
        let auth = DiscordDashboardAuthorizer::new(
            "our-app",
            http.clone(),
            Arc::new(|_| true),
            Arc::new(|| 1_000),
        );
        (auth, http)
    }
    #[tokio::test]
    async fn guild_access_revalidates_permissions_while_the_list_cache_is_warm() {
        let (auth, http) = mutable_authorizer();
        let guild = "100000000000000010";
        assert!(
            matches!(auth.manageable_guilds("synthetic").await,DashboardAccess::Allowed(rows) if rows.len()==1)
        );
        *http.guilds.lock().unwrap() = json!([{"id":guild,"permissions":"0","owner":false}]);
        assert_eq!(
            auth.authorize_guild("synthetic", guild).await,
            DashboardAccess::Forbidden
        );
        assert_eq!(
            auth.manageable_guilds("synthetic").await,
            DashboardAccess::Allowed(vec![])
        );
        *http.guilds.lock().unwrap() = json!([{"id":guild,"permissions":"32","owner":false}]);
        assert_eq!(
            auth.authorize_guild("synthetic", guild).await,
            DashboardAccess::Allowed(())
        );
    }
    #[tokio::test]
    async fn guild_access_revalidates_revocation_and_unavailability_with_a_warm_cache() {
        for failure in [
            DashboardDiscordError::InvalidToken,
            DashboardDiscordError::Unavailable,
        ] {
            let (auth, http) = mutable_authorizer();
            assert!(matches!(
                auth.manageable_guilds("synthetic").await,
                DashboardAccess::Allowed(_)
            ));
            *http.oauth.lock().unwrap() = Err(failure);
            let expected = match failure {
                DashboardDiscordError::InvalidToken => DashboardAccess::Unauthenticated,
                DashboardDiscordError::Unavailable => DashboardAccess::Unavailable,
            };
            assert_eq!(
                auth.authorize_guild("synthetic", "100000000000000010")
                    .await,
                expected
            );
            assert_eq!(
                auth.manageable_guilds("synthetic").await,
                match failure {
                    DashboardDiscordError::InvalidToken => DashboardAccess::Unauthenticated,
                    DashboardDiscordError::Unavailable => DashboardAccess::Unavailable,
                }
            );
        }
    }
    struct EmptyOptions;
    #[async_trait]
    impl crate::dashboard_api::DashboardOptionsProvider for EmptyOptions {
        async fn options_for_guild(
            &self,
            _guild: &str,
        ) -> Result<
            crate::dashboard_api::DashboardOptions,
            crate::dashboard_api::DashboardOptionsError,
        > {
            Ok(crate::dashboard_api::DashboardOptions::default())
        }
    }
    #[tokio::test]
    async fn guild_access_revalidation_blocks_http_mutations_without_changing_stored_data() {
        use crate::dashboard_api::{DashboardApiConfig, dashboard_router};
        use axum::{
            body::Body,
            http::{Request, StatusCode},
        };
        use tower::ServiceExt;
        use vozen_store::{ChannelProfilePatch, SqliteStore};
        let guild = "100000000000000010";
        let channel = "100000000000000011";
        let (auth, http) = mutable_authorizer();
        assert!(matches!(
            auth.manageable_guilds("synthetic").await,
            DashboardAccess::Allowed(_)
        ));
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().unwrap()));
        let profile = ChannelProfilePatch {
            max_chars: Some(211),
            ..Default::default()
        };
        assert!(
            store
                .lock()
                .unwrap()
                .save_channel_profile(guild, channel, &profile)
                .unwrap()
        );
        let before_profile = store
            .lock()
            .unwrap()
            .channel_profile(guild, channel)
            .unwrap();
        let before_config = store.lock().unwrap().guild_config(guild).unwrap();
        let app = dashboard_router(DashboardApiConfig {
            origin: "http://127.0.0.1:18765".into(),
            store: store.clone(),
            authorizer: Arc::new(auth),
            options: Arc::new(EmptyOptions),
        })
        .unwrap();
        *http.guilds.lock().unwrap() = json!([{"id":guild,"permissions":"0","owner":false}]);
        for (method, uri, body) in [
            (
                "POST",
                format!("/api/dashboard/guild/{guild}"),
                r#"{"maxChars":212}"#,
            ),
            (
                "POST",
                format!("/api/dashboard/guild/{guild}/profile/{channel}"),
                r#"{"maxChars":212}"#,
            ),
            (
                "DELETE",
                format!("/api/dashboard/guild/{guild}/profile/{channel}"),
                "",
            ),
            ("GET", format!("/api/dashboard/guild/{guild}"), ""),
        ] {
            let request = Request::builder()
                .method(method)
                .uri(uri)
                .header("Authorization", "Bearer synthetic")
                .header("Content-Type", "application/json")
                .body(Body::from(body))
                .unwrap();
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                StatusCode::FORBIDDEN
            );
            assert_eq!(
                store
                    .lock()
                    .unwrap()
                    .channel_profile(guild, channel)
                    .unwrap(),
                before_profile
            );
            assert_eq!(
                store.lock().unwrap().guild_config(guild).unwrap(),
                before_config
            );
        }
    }

    struct Fake {
        oauth: Value,
        guilds: Value,
    }
    #[async_trait]
    impl DashboardDiscordHttp for Fake {
        async fn get_json(
            &self,
            url: &'static str,
            _bearer: &str,
        ) -> Result<Value, DashboardDiscordError> {
            Ok(match url {
                DISCORD_OAUTH_ME => self.oauth.clone(),
                DISCORD_GUILDS => self.guilds.clone(),
                _ => unreachable!(),
            })
        }
    }
    fn authorizer(oauth: Value, guilds: Value, present: bool) -> DiscordDashboardAuthorizer {
        DiscordDashboardAuthorizer::new(
            "our-app",
            Arc::new(Fake { oauth, guilds }),
            Arc::new(move |_| present),
            Arc::new(|| 1_000),
        )
    }

    #[tokio::test]
    async fn accepts_only_our_guild_scoped_tokens_and_manageable_live_guilds() {
        let auth = authorizer(
            json!({"application":{"id":"our-app"},"scopes":["identify","guilds"]}),
            json!([
                {"id":"guild-a","name":"Mine","permissions":"32"},
                {"id":"guild-b","name":"No access","permissions":"0"}
            ]),
            true,
        );
        assert_eq!(
            auth.manageable_guilds("secret").await,
            DashboardAccess::Allowed(vec![ManageableGuild {
                id: "guild-a".into(),
                name: "Mine".into(),
                icon: None
            }])
        );
        assert_eq!(
            auth.authorize_guild("secret", "guild-a").await,
            DashboardAccess::Allowed(())
        );
        assert_eq!(
            auth.authorize_guild("secret", "guild-b").await,
            DashboardAccess::Forbidden
        );
    }

    #[tokio::test]
    async fn wrong_audience_or_scope_fails_closed_before_guild_data_is_used() {
        let auth = authorizer(
            json!({"application":{"id":"other"},"scopes":["identify","guilds"]}),
            json!([]),
            true,
        );
        assert_eq!(
            auth.manageable_guilds("secret").await,
            DashboardAccess::Unauthenticated
        );
        let no_scope = authorizer(
            json!({"application":{"id":"our-app"},"scopes":["identify"]}),
            json!([]),
            true,
        );
        assert_eq!(
            no_scope.authorize_guild("secret", "guild-a").await,
            DashboardAccess::Unauthenticated
        );
    }

    #[tokio::test]
    async fn accepts_ecosystem_and_product_audiences_explicitly() {
        let auth = DiscordDashboardAuthorizer::new_with_client_ids(
            ["ecosystem-app".into(), "tts-app".into()],
            Arc::new(Fake {
                oauth: json!({
                    "application":{"id":"ecosystem-app"},
                    "scopes":["identify","guilds"]
                }),
                guilds: json!([]),
            }),
            Arc::new(|_| true),
            Arc::new(|| 1_000),
        );
        assert!(matches!(
            auth.manageable_guilds("secret").await,
            DashboardAccess::Allowed(_)
        ));

        let product = DiscordDashboardAuthorizer::new_with_client_ids(
            ["ecosystem-app".into(), "tts-app".into()],
            Arc::new(Fake {
                oauth: json!({
                    "application":{"id":"tts-app"},
                    "scopes":["identify","guilds"]
                }),
                guilds: json!([]),
            }),
            Arc::new(|_| true),
            Arc::new(|| 1_000),
        );
        assert!(matches!(
            product.manageable_guilds("secret").await,
            DashboardAccess::Allowed(_)
        ));
    }
}
