//! Discord identity boundary shared by the authenticated browser APIs.
//!
//! OAuth verification is injected. The production adapter (`discord_oauth`) validates the
//! `/oauth2/@me` audience and the `/users/@me` identity before returning a Discord account to
//! the account, Stripe and dashboard routes.

use async_trait::async_trait;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordIdentity {
    pub id: String,
    pub username: String,
    pub avatar: Option<String>,
    /// Public avatar-decoration asset returned by Discord's `identify` scope.
    /// This is only carried through the current authenticated request; it is not persisted.
    pub avatar_decoration_asset: Option<String>,
}

/// Boundary for Discord OAuth. Implementations must reject tokens issued to another application
/// and tokens whose user does not match the OAuth grant.
#[async_trait]
pub trait DiscordIdentityVerifier: Send + Sync {
    async fn resolve_identity(&self, bearer: &str) -> Result<DiscordIdentity, ()>;
}
