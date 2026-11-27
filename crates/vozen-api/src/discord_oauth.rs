//! Discord OAuth verifier for the browser account API.
//!
//! Access tokens are treated as credentials: this module never logs or stores a token, and only
//! caches the SHA-256 digest of a token for the short identity-only lookup used by account calls.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::admin_api::{AdminAuthorization, AdminAuthorizationResolver};
use crate::premium_api::{DiscordIdentity, DiscordIdentityVerifier};

const DISCORD_ME: &str = "https://discord.com/api/v10/users/@me";
const DISCORD_OAUTH_ME: &str = "https://discord.com/api/v10/oauth2/@me";
const DISCORD_USERS: &str = "https://discord.com/api/v10/users/";
const DISCORD_FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const IDENTITY_TTL_MS: i64 = 60_000;
const IDENTITY_CACHE_MAX_ENTRIES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscordHttpError {
    InvalidToken,
    Unavailable,
}

/// Small HTTP boundary so OAuth semantics can be tested without a real Discord request.
#[async_trait]
pub trait DiscordOAuthHttp: Send + Sync {
    async fn get_json(&self, url: &str, bearer: &str) -> Result<Value, DiscordHttpError>;
    async fn get_json_as_bot(&self, url: &str, bot_token: &str) -> Result<Value, DiscordHttpError>;
}

#[derive(Clone)]
pub struct ReqwestDiscordOAuthHttp {
    client: reqwest::Client,
}

impl ReqwestDiscordOAuthHttp {
    pub fn new() -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(DISCORD_FETCH_TIMEOUT)
                .build()?,
        })
    }
}

#[async_trait]
impl DiscordOAuthHttp for ReqwestDiscordOAuthHttp {
    async fn get_json(&self, url: &str, bearer: &str) -> Result<Value, DiscordHttpError> {
        let response = self
            .client
            .get(url)
            .bearer_auth(bearer)
            .send()
            .await
            .map_err(|_| DiscordHttpError::Unavailable)?;
        let status = response.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(DiscordHttpError::InvalidToken);
        }
        if !status.is_success() {
            return Err(DiscordHttpError::Unavailable);
        }
        response
            .json()
            .await
            .map_err(|_| DiscordHttpError::Unavailable)
    }

    async fn get_json_as_bot(&self, url: &str, bot_token: &str) -> Result<Value, DiscordHttpError> {
        let response = self
            .client
            .get(url)
            .header(reqwest::header::AUTHORIZATION, format!("Bot {bot_token}"))
            .send()
            .await
            .map_err(|_| DiscordHttpError::Unavailable)?;
        let status = response.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(DiscordHttpError::InvalidToken);
        }
        if !status.is_success() {
            return Err(DiscordHttpError::Unavailable);
        }
        response
            .json()
            .await
            .map_err(|_| DiscordHttpError::Unavailable)
    }
}

#[derive(Clone)]
pub struct DiscordOAuthVerifier {
    expected_client_id: Arc<str>,
    bot_token: Option<Arc<str>>,
    http: Arc<dyn DiscordOAuthHttp>,
    now: Arc<dyn Fn() -> i64 + Send + Sync>,
    cache: Arc<Mutex<HashMap<[u8; 32], CachedIdentity>>>,
}

#[derive(Clone)]
struct CachedIdentity {
    identity: Option<DiscordIdentity>,
    expires_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdentityResolutionError {
    CachedRejection,
    OAuthInvalidToken,
    OAuthUnavailable,
    WrongAudience,
    MissingIdentifyScope,
    UserInvalidToken,
    UserUnavailable,
    UserMismatch,
}

impl IdentityResolutionError {
    fn as_str(self) -> &'static str {
        match self {
            Self::CachedRejection => "cached_rejection",
            Self::OAuthInvalidToken => "oauth_invalid_token",
            Self::OAuthUnavailable => "oauth_unavailable",
            Self::WrongAudience => "wrong_audience",
            Self::MissingIdentifyScope => "missing_identify_scope",
            Self::UserInvalidToken => "user_invalid_token",
            Self::UserUnavailable => "user_unavailable",
            Self::UserMismatch => "user_mismatch",
        }
    }
}

impl DiscordOAuthVerifier {
    pub fn production(
        expected_client_id: impl Into<String>,
        bot_token: Option<String>,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self::new(
            expected_client_id,
            Arc::new(ReqwestDiscordOAuthHttp::new()?),
            Arc::new(system_now_ms),
        )
        .with_bot_token(bot_token))
    }

    pub fn new(
        expected_client_id: impl Into<String>,
        http: Arc<dyn DiscordOAuthHttp>,
        now: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        Self {
            expected_client_id: Arc::from(expected_client_id.into()),
            bot_token: None,
            http,
            now,
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn with_bot_token(mut self, bot_token: Option<String>) -> Self {
        self.bot_token = bot_token
            .filter(|token| !token.trim().is_empty())
            .map(Arc::from);
        self
    }

    async fn resolve_claim_identity(
        &self,
        bearer: &str,
    ) -> Result<DiscordIdentity, IdentityResolutionError> {
        let now = (self.now)();
        let cache_key = token_cache_key(bearer);
        if let Some(cached) = self.cache_get(cache_key, now) {
            return cached.ok_or(IdentityResolutionError::CachedRejection);
        }

        let identity = self.resolve_claim_identity_uncached(bearer).await;
        match &identity {
            Ok(found) => self.cache_put(cache_key, Some(found.clone()), now),
            // A temporary Discord failure (429, 5xx, network) says nothing about the token.
            // Caching it would turn one hiccup into a minute of "invalid token" answers.
            Err(
                IdentityResolutionError::OAuthUnavailable
                | IdentityResolutionError::UserUnavailable,
            ) => self.cache_remove(cache_key),
            Err(_) => self.cache_put(cache_key, None, now),
        }
        identity
    }

    async fn resolve_claim_identity_uncached(
        &self,
        bearer: &str,
    ) -> Result<DiscordIdentity, IdentityResolutionError> {
        let oauth = self
            .fetch_oauth(bearer)
            .await
            .map_err(|error| match error {
                DiscordHttpError::InvalidToken => IdentityResolutionError::OAuthInvalidToken,
                DiscordHttpError::Unavailable => IdentityResolutionError::OAuthUnavailable,
            })?;
        if oauth.application_id != self.expected_client_id.as_ref() {
            return Err(IdentityResolutionError::WrongAudience);
        }
        if !oauth.scopes.iter().any(|scope| scope == "identify") {
            return Err(IdentityResolutionError::MissingIdentifyScope);
        }
        let user = self.fetch_user(bearer).await.map_err(|error| match error {
            DiscordHttpError::InvalidToken => IdentityResolutionError::UserInvalidToken,
            DiscordHttpError::Unavailable => IdentityResolutionError::UserUnavailable,
        })?;
        if user.id != oauth.user_id {
            return Err(IdentityResolutionError::UserMismatch);
        }
        let fallback_decoration = if user.avatar_decoration_asset.is_none() {
            self.fetch_user_with_bot(&user.id)
                .await
                .ok()
                .and_then(|bot_user| bot_user.avatar_decoration_asset)
        } else {
            None
        };
        Ok(DiscordIdentity {
            id: user.id,
            username: user.username,
            avatar: user.avatar,
            avatar_decoration_asset: user.avatar_decoration_asset.or(fallback_decoration),
        })
    }

    async fn fetch_oauth(&self, bearer: &str) -> Result<OAuthInfo, DiscordHttpError> {
        let value = self.http.get_json(DISCORD_OAUTH_ME, bearer).await?;
        parse_oauth(value).ok_or(DiscordHttpError::InvalidToken)
    }

    async fn fetch_user(&self, bearer: &str) -> Result<DiscordUser, DiscordHttpError> {
        let value = self.http.get_json(DISCORD_ME, bearer).await?;
        parse_user(value).ok_or(DiscordHttpError::InvalidToken)
    }

    async fn fetch_user_with_bot(&self, user_id: &str) -> Result<DiscordUser, DiscordHttpError> {
        let Some(bot_token) = self.bot_token.as_deref() else {
            return Err(DiscordHttpError::Unavailable);
        };
        let url = format!("{DISCORD_USERS}{user_id}");
        let value = self.http.get_json_as_bot(&url, bot_token).await?;
        parse_user(value).ok_or(DiscordHttpError::InvalidToken)
    }

    fn cache_get(&self, cache_key: [u8; 32], now: i64) -> Option<Option<DiscordIdentity>> {
        let Ok(mut cache) = self.cache.lock() else {
            return None;
        };
        cache.retain(|_, item| item.expires_at > now);
        cache.get(&cache_key).map(|item| item.identity.clone())
    }

    fn cache_remove(&self, cache_key: [u8; 32]) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.remove(&cache_key);
        }
    }

    fn cache_put(&self, cache_key: [u8; 32], identity: Option<DiscordIdentity>, now: i64) {
        let Ok(mut cache) = self.cache.lock() else {
            return;
        };
        cache.retain(|_, item| item.expires_at > now);
        while cache.len() >= IDENTITY_CACHE_MAX_ENTRIES && !cache.contains_key(&cache_key) {
            let oldest = cache
                .iter()
                .min_by_key(|(_, item)| item.expires_at)
                .map(|(key, _)| *key);
            if let Some(oldest) = oldest {
                cache.remove(&oldest);
            } else {
                break;
            }
        }
        cache.insert(
            cache_key,
            CachedIdentity {
                identity,
                expires_at: now.saturating_add(IDENTITY_TTL_MS),
            },
        );
    }
}

#[async_trait]
impl DiscordIdentityVerifier for DiscordOAuthVerifier {
    async fn resolve_identity(&self, bearer: &str) -> Result<DiscordIdentity, ()> {
        self.resolve_claim_identity(bearer).await.map_err(|error| {
            eprintln!(
                "[oauth] account identity rejected reason={}",
                error.as_str()
            );
        })
    }
}

#[async_trait]
impl AdminAuthorizationResolver for DiscordOAuthVerifier {
    /// Admin login intentionally revalidates `/oauth2/@me` on every attempt. In particular, this
    /// does not reuse the short identity cache: a stale OAuth audience must never mint a session.
    async fn resolve_authorization(&self, bearer: &str) -> Option<AdminAuthorization> {
        let oauth = self.fetch_oauth(bearer).await.ok()?;
        Some(AdminAuthorization {
            user_id: oauth.user_id,
            application_id: oauth.application_id,
        })
    }
}

struct OAuthInfo {
    application_id: String,
    user_id: String,
    scopes: Vec<String>,
}

struct DiscordUser {
    id: String,
    username: String,
    avatar: Option<String>,
    avatar_decoration_asset: Option<String>,
}

fn parse_oauth(value: Value) -> Option<OAuthInfo> {
    let object = value.as_object()?;
    let application_id = object
        .get("application")?
        .as_object()?
        .get("id")?
        .as_str()?;
    let user_id = object.get("user")?.as_object()?.get("id")?.as_str()?;
    let scopes = object
        .get("scopes")
        .and_then(Value::as_array)
        .map(|scopes| {
            scopes
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Some(OAuthInfo {
        application_id: application_id.to_owned(),
        user_id: user_id.to_owned(),
        scopes,
    })
}

fn parse_user(value: Value) -> Option<DiscordUser> {
    let object = value.as_object()?;
    let id = object.get("id")?.as_str()?.to_owned();
    let username = object
        .get("global_name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .or_else(|| object.get("username").and_then(Value::as_str))
        .unwrap_or(&id)
        .to_owned();
    Some(DiscordUser {
        id,
        username,
        avatar: object
            .get("avatar")
            .and_then(Value::as_str)
            .map(str::to_owned),
        avatar_decoration_asset: object
            .get("avatar_decoration_data")
            .and_then(Value::as_object)
            .and_then(|decoration| decoration.get("asset"))
            .and_then(Value::as_str)
            .filter(|asset| is_safe_discord_asset(asset))
            .map(str::to_owned),
    })
}

fn is_safe_discord_asset(asset: &str) -> bool {
    !asset.is_empty()
        && asset.len() <= 128
        && asset
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn token_cache_key(token: &str) -> [u8; 32] {
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
    use std::sync::Mutex;

    use super::*;
    use serde_json::json;

    struct FakeHttp {
        calls: Mutex<Vec<String>>,
        oauth: Result<Value, DiscordHttpError>,
        user: Result<Value, DiscordHttpError>,
        bot_user: Result<Value, DiscordHttpError>,
    }

    #[async_trait]
    impl DiscordOAuthHttp for FakeHttp {
        async fn get_json(&self, url: &str, _bearer: &str) -> Result<Value, DiscordHttpError> {
            self.calls.lock().unwrap().push(url.to_owned());
            match url {
                DISCORD_OAUTH_ME => self.oauth.clone(),
                DISCORD_ME => self.user.clone(),
                _ => unreachable!(),
            }
        }

        async fn get_json_as_bot(
            &self,
            url: &str,
            _bot_token: &str,
        ) -> Result<Value, DiscordHttpError> {
            self.calls.lock().unwrap().push(url.to_owned());
            self.bot_user.clone()
        }
    }

    fn oauth(app: &str, user: &str, scopes: &[&str]) -> Value {
        json!({"application":{"id":app},"user":{"id":user},"scopes":scopes})
    }

    fn user(id: &str, email: Option<&str>, verified: bool) -> Value {
        json!({"id":id,"email":email,"verified":verified})
    }

    fn verifier(http: Arc<FakeHttp>) -> DiscordOAuthVerifier {
        DiscordOAuthVerifier::new("vozen-client", http, Arc::new(|| 1_000))
    }

    #[tokio::test]
    async fn requires_our_audience_and_the_same_discord_identity_for_receipts() {
        let http = Arc::new(FakeHttp {
            calls: Mutex::new(Vec::new()),
            oauth: Ok(oauth("other-client", "user-a", &["identify"])),
            user: Ok(user("user-a", None, false)),
            bot_user: Err(DiscordHttpError::Unavailable),
        });
        assert!(
            verifier(http.clone())
                .resolve_identity("token")
                .await
                .is_err()
        );
        assert_eq!(http.calls.lock().unwrap().as_slice(), [DISCORD_OAUTH_ME]);

        let mismatch = Arc::new(FakeHttp {
            calls: Mutex::new(Vec::new()),
            oauth: Ok(oauth("vozen-client", "user-a", &["identify"])),
            user: Ok(user("user-b", None, false)),
            bot_user: Err(DiscordHttpError::Unavailable),
        });
        assert!(verifier(mismatch).resolve_identity("token").await.is_err());
    }

    #[tokio::test]
    async fn carries_a_valid_avatar_decoration_from_discord_identity() {
        let http = Arc::new(FakeHttp {
            calls: Mutex::new(Vec::new()),
            oauth: Ok(oauth("vozen-client", "user-a", &["identify"])),
            user: Ok(json!({
                "id": "user-a",
                "username": "Rexy",
                "avatar": "avatar-hash",
                "avatar_decoration_data": {"asset": "a_fed43ab12698df65902ba06727e20c0e", "sku_id": "1"}
            })),
            bot_user: Err(DiscordHttpError::Unavailable),
        });

        let identity = verifier(http).resolve_identity("token").await.unwrap();
        assert_eq!(identity.username, "Rexy");
        assert_eq!(identity.avatar.as_deref(), Some("avatar-hash"));
        assert_eq!(
            identity.avatar_decoration_asset.as_deref(),
            Some("a_fed43ab12698df65902ba06727e20c0e")
        );
    }

    #[tokio::test]
    async fn uses_the_bot_profile_only_when_oauth_omits_the_avatar_decoration() {
        let http = Arc::new(FakeHttp {
            calls: Mutex::new(Vec::new()),
            oauth: Ok(oauth("vozen-client", "user-a", &["identify"])),
            user: Ok(json!({"id":"user-a", "username":"Rexy", "avatar":"avatar-hash"})),
            bot_user: Ok(json!({
                "id": "user-a",
                "avatar_decoration_data": {"asset": "a_fed43ab12698df65902ba06727e20c0e"}
            })),
        });
        let verifier = verifier(http.clone()).with_bot_token(Some("bot-token".into()));

        let identity = verifier.resolve_identity("token").await.unwrap();
        assert_eq!(
            identity.avatar_decoration_asset.as_deref(),
            Some("a_fed43ab12698df65902ba06727e20c0e")
        );
        assert_eq!(
            http.calls.lock().unwrap().as_slice(),
            [
                DISCORD_OAUTH_ME,
                DISCORD_ME,
                "https://discord.com/api/v10/users/user-a"
            ]
        );
    }

    #[test]
    fn ignores_an_unsafe_avatar_decoration_asset() {
        let parsed = parse_user(json!({
            "id": "user-a",
            "username": "Rexy",
            "avatar_decoration_data": {"asset": "not/a-safe-asset"}
        }))
        .unwrap();
        assert_eq!(parsed.avatar_decoration_asset, None);
    }

    #[tokio::test]
    async fn only_the_digest_is_cached_and_invalid_tokens_fail_closed() {
        let http = Arc::new(FakeHttp {
            calls: Mutex::new(Vec::new()),
            oauth: Err(DiscordHttpError::InvalidToken),
            user: Ok(user("user-a", None, false)),
            bot_user: Err(DiscordHttpError::Unavailable),
        });
        let verifier = verifier(http.clone());
        assert!(verifier.resolve_identity("sensitive-token").await.is_err());
        assert!(verifier.resolve_identity("sensitive-token").await.is_err());
        assert_eq!(http.calls.lock().unwrap().as_slice(), [DISCORD_OAUTH_ME]);
        let cache = verifier.cache.lock().unwrap();
        assert!(cache.contains_key(&token_cache_key("sensitive-token")));
    }

    #[tokio::test]
    async fn temporary_discord_failures_are_not_cached_as_invalid_tokens() {
        for (oauth_result, user_result) in [
            (
                Err(DiscordHttpError::Unavailable),
                Ok(user("user-a", None, false)),
            ),
            (
                Ok(oauth("vozen-client", "user-a", &["identify"])),
                Err(DiscordHttpError::Unavailable),
            ),
        ] {
            let http = Arc::new(FakeHttp {
                calls: Mutex::new(Vec::new()),
                oauth: oauth_result,
                user: user_result,
                bot_user: Err(DiscordHttpError::Unavailable),
            });
            let verifier = verifier(http.clone());
            assert!(verifier.resolve_identity("sensitive-token").await.is_err());
            assert!(verifier.resolve_identity("sensitive-token").await.is_err());
            // Both attempts reached Discord: the outage was retried, not served from the cache.
            let oauth_calls = http
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|url| url.as_str() == DISCORD_OAUTH_ME)
                .count();
            assert_eq!(oauth_calls, 2);
            assert!(
                !verifier
                    .cache
                    .lock()
                    .unwrap()
                    .contains_key(&token_cache_key("sensitive-token"))
            );
        }
    }

    #[tokio::test]
    async fn identity_rejections_keep_a_safe_operational_reason() {
        let wrong_audience = Arc::new(FakeHttp {
            calls: Mutex::new(Vec::new()),
            oauth: Ok(oauth("other-client", "user-a", &["identify"])),
            user: Ok(user("user-a", None, false)),
            bot_user: Err(DiscordHttpError::Unavailable),
        });
        assert_eq!(
            verifier(wrong_audience)
                .resolve_claim_identity("sensitive-token")
                .await,
            Err(IdentityResolutionError::WrongAudience)
        );

        let missing_scope = Arc::new(FakeHttp {
            calls: Mutex::new(Vec::new()),
            oauth: Ok(oauth("vozen-client", "user-a", &["guilds"])),
            user: Ok(user("user-a", None, false)),
            bot_user: Err(DiscordHttpError::Unavailable),
        });
        assert_eq!(
            verifier(missing_scope)
                .resolve_claim_identity("sensitive-token")
                .await,
            Err(IdentityResolutionError::MissingIdentifyScope)
        );
        assert_eq!(
            IdentityResolutionError::OAuthInvalidToken.as_str(),
            "oauth_invalid_token"
        );
    }

    #[tokio::test]
    async fn admin_authorization_rechecks_audience_payload_without_needing_user_scope() {
        let http = Arc::new(FakeHttp {
            calls: Mutex::new(Vec::new()),
            oauth: Ok(oauth("vozen-client", "owner", &[])),
            user: Ok(user("owner", None, false)),
            bot_user: Err(DiscordHttpError::Unavailable),
        });
        assert_eq!(
            verifier(http.clone()).resolve_authorization("token").await,
            Some(AdminAuthorization {
                user_id: "owner".into(),
                application_id: "vozen-client".into(),
            })
        );
        assert_eq!(http.calls.lock().unwrap().as_slice(), [DISCORD_OAUTH_ME]);
    }
}
