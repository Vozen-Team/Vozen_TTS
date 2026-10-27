//! Contract-backed Discord command registration.
//!
//! The optional state path must be Rust-owned (for example `rust-commands-state.json`), never
//! Node's state file. This prevents a shadow runtime from suppressing a Node REST update.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use vozen_contracts::{ContractError, DiscordCommandCatalog};

const COMMANDS: &str = include_str!("../../../contracts/discord-commands.json");
const DISCORD_API: &str = "https://discord.com/api/v10";

#[derive(Debug, Clone)]
pub struct CommandRegistrationConfig {
    pub application_id: String,
    /// Optional guild-only scope for R4 staging. When set, public commands are replaced only in
    /// this guild instead of being published globally. Production leaves this unset.
    pub public_guild_id: Option<String>,
    pub state_path: Option<PathBuf>,
    pub owner_guild_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandRegistrationOutcome {
    pub public_registered: bool,
    pub owner_registered: bool,
    pub state_saved: bool,
}

#[derive(Debug, Error)]
pub enum CommandRegistrationError {
    #[error("Discord application ID must be a 17-20 digit snowflake")]
    InvalidApplicationId,
    #[error("Discord public command guild ID must be a 17-20 digit snowflake")]
    InvalidPublicGuildId,
    #[error("Discord owner guild ID must be a 17-20 digit snowflake")]
    InvalidOwnerGuildId,
    #[error("Discord bot token is required for command registration")]
    MissingToken,
    #[error("command contract error: {0}")]
    Contract(#[from] ContractError),
    #[error("Discord command registration request failed")]
    Transport,
    #[error("Discord command registration returned an unsuccessful status")]
    Rejected,
}

#[async_trait]
pub trait CommandRegistrationClient: Send + Sync {
    async fn replace_global(
        &self,
        application_id: &str,
        commands: Vec<Value>,
    ) -> Result<(), CommandRegistrationError>;
    async fn replace_guild(
        &self,
        application_id: &str,
        guild_id: &str,
        commands: Vec<Value>,
    ) -> Result<(), CommandRegistrationError>;
}

/// Production REST client. Errors intentionally do not include a token or response body.
pub struct DiscordHttpCommandRegistrationClient {
    client: reqwest::Client,
    token: String,
}

impl DiscordHttpCommandRegistrationClient {
    pub fn new(token: String) -> Result<Self, CommandRegistrationError> {
        if token.trim().is_empty() {
            return Err(CommandRegistrationError::MissingToken);
        }
        Ok(Self {
            client: reqwest::Client::new(),
            token,
        })
    }

    async fn replace(
        &self,
        url: String,
        commands: Vec<Value>,
    ) -> Result<(), CommandRegistrationError> {
        let response = self
            .client
            .put(url)
            .header("Authorization", bot_authorization(&self.token))
            .json(&commands)
            .send()
            .await
            .map_err(|_| CommandRegistrationError::Transport)?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(CommandRegistrationError::Rejected)
        }
    }
}

fn bot_authorization(token: &str) -> String {
    format!("Bot {token}")
}

#[async_trait]
impl CommandRegistrationClient for DiscordHttpCommandRegistrationClient {
    async fn replace_global(
        &self,
        application_id: &str,
        commands: Vec<Value>,
    ) -> Result<(), CommandRegistrationError> {
        self.replace(
            format!("{DISCORD_API}/applications/{application_id}/commands"),
            commands,
        )
        .await
    }

    async fn replace_guild(
        &self,
        application_id: &str,
        guild_id: &str,
        commands: Vec<Value>,
    ) -> Result<(), CommandRegistrationError> {
        self.replace(
            format!("{DISCORD_API}/applications/{application_id}/guilds/{guild_id}/commands"),
            commands,
        )
        .await
    }
}

/// Registers public commands only when the exact contract payload changed. Owner commands follow
/// Node's behaviour: they are guild-only and refreshed on each boot when a control guild exists.
pub async fn register_commands<C: CommandRegistrationClient>(
    client: &C,
    config: &CommandRegistrationConfig,
) -> Result<CommandRegistrationOutcome, CommandRegistrationError> {
    validate_config(config)?;
    let catalog = DiscordCommandCatalog::from_json(COMMANDS)?;
    let public = catalog.public_registration_payload()?;
    let fingerprint = payload_fingerprint(&public);
    let previous = match &config.state_path {
        Some(path) => read_state(path).await,
        None => None,
    };
    let public_registered = previous.as_ref().is_none_or(|state| {
        state.application_id != config.application_id
            || state.public_fingerprint != fingerprint
            || state.public_guild_id != config.public_guild_id
    });
    let owner_same_as_public_guild = config
        .public_guild_id
        .as_deref()
        .zip(config.owner_guild_id.as_deref())
        .is_some_and(|(public, owner)| public == owner);
    let owner_registered = if owner_same_as_public_guild {
        // Discord's PUT /guilds/{guild}/commands replaces the complete list. Merge both scopes
        // into one request or a later owner-only PUT would silently erase the public commands.
        let mut commands = public.clone();
        commands.extend(catalog.owner_registration_payload()?);
        client
            .replace_guild(
                &config.application_id,
                config
                    .public_guild_id
                    .as_deref()
                    .expect("same public guild"),
                commands,
            )
            .await?;
        true
    } else if public_registered {
        if let Some(guild_id) = &config.public_guild_id {
            client
                .replace_guild(&config.application_id, guild_id, public.clone())
                .await?;
        } else {
            client
                .replace_global(&config.application_id, public.clone())
                .await?;
        }
        false
    } else {
        false
    };
    let state_saved = if public_registered {
        match &config.state_path {
            Some(path) => {
                write_state(
                    path,
                    &CommandRegistrationState {
                        application_id: config.application_id.clone(),
                        public_fingerprint: fingerprint,
                        public_guild_id: config.public_guild_id.clone(),
                    },
                )
                .await
            }
            None => false,
        }
    } else {
        false
    };
    let owner_registered = if owner_same_as_public_guild {
        owner_registered
    } else if let Some(guild_id) = &config.owner_guild_id {
        client
            .replace_guild(
                &config.application_id,
                guild_id,
                catalog.owner_registration_payload()?,
            )
            .await?;
        true
    } else {
        false
    };
    Ok(CommandRegistrationOutcome {
        public_registered,
        owner_registered,
        state_saved,
    })
}

#[derive(Debug, Serialize, Deserialize)]
struct CommandRegistrationState {
    application_id: String,
    public_fingerprint: String,
    #[serde(default)]
    public_guild_id: Option<String>,
}

fn validate_config(config: &CommandRegistrationConfig) -> Result<(), CommandRegistrationError> {
    if !valid_snowflake(&config.application_id) {
        return Err(CommandRegistrationError::InvalidApplicationId);
    }
    if config
        .public_guild_id
        .as_deref()
        .is_some_and(|value| !valid_snowflake(value))
    {
        return Err(CommandRegistrationError::InvalidPublicGuildId);
    }
    if config
        .owner_guild_id
        .as_deref()
        .is_some_and(|value| !valid_snowflake(value))
    {
        return Err(CommandRegistrationError::InvalidOwnerGuildId);
    }
    Ok(())
}

fn valid_snowflake(value: &str) -> bool {
    (17..=20).contains(&value.len()) && value.bytes().all(|value| value.is_ascii_digit())
}

fn payload_fingerprint(payload: &[Value]) -> String {
    let bytes = serde_json::to_vec(payload).expect("the contract payload is serializable");
    format!("{:x}", Sha256::digest(bytes))
}

async fn read_state(path: &Path) -> Option<CommandRegistrationState> {
    serde_json::from_slice(&tokio::fs::read(path).await.ok()?).ok()
}

// State is a cache only: failure safely means the next Rust boot repeats the scoped PUT.
async fn write_state(path: &Path, state: &CommandRegistrationState) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    if tokio::fs::create_dir_all(parent).await.is_err() {
        return false;
    }
    let Ok(bytes) = serde_json::to_vec(state) else {
        return false;
    };
    tokio::fs::write(path, bytes).await.is_ok()
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    #[derive(Default)]
    struct MockClient {
        global: Mutex<Vec<Vec<Value>>>,
        guild: Mutex<Vec<(String, Vec<Value>)>>,
    }

    #[async_trait]
    impl CommandRegistrationClient for MockClient {
        async fn replace_global(
            &self,
            _application_id: &str,
            commands: Vec<Value>,
        ) -> Result<(), CommandRegistrationError> {
            self.global.lock().expect("global").push(commands);
            Ok(())
        }

        async fn replace_guild(
            &self,
            _application_id: &str,
            guild_id: &str,
            commands: Vec<Value>,
        ) -> Result<(), CommandRegistrationError> {
            self.guild
                .lock()
                .expect("guild")
                .push((guild_id.to_owned(), commands));
            Ok(())
        }
    }

    fn config(state_path: Option<PathBuf>) -> CommandRegistrationConfig {
        CommandRegistrationConfig {
            application_id: "100000000000000004".into(),
            public_guild_id: None,
            state_path,
            owner_guild_id: Some("123456789012345678".into()),
        }
    }

    fn temporary_state_path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "vozen-rust-command-state-{}.json",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ))
    }

    #[test]
    fn discord_bot_registration_uses_bot_authorization_scheme() {
        assert_eq!(bot_authorization("test-token"), "Bot test-token");
    }

    #[tokio::test]
    async fn public_and_owner_commands_stay_in_separate_scopes() {
        let client = MockClient::default();
        let outcome = register_commands(&client, &config(None))
            .await
            .expect("register");
        assert!(outcome.public_registered && outcome.owner_registered);
        assert_eq!(client.global.lock().expect("global")[0].len(), 40);
        assert_eq!(client.guild.lock().expect("guild")[0].1.len(), 2);
    }

    #[tokio::test]
    async fn rust_state_skips_the_public_put_but_not_owner_guild_refresh() {
        let path = temporary_state_path();
        let client = MockClient::default();
        let first = register_commands(&client, &config(Some(path.clone())))
            .await
            .expect("first");
        let second = register_commands(&client, &config(Some(path.clone())))
            .await
            .expect("second");
        assert!(first.public_registered && first.state_saved);
        assert!(!second.public_registered);
        assert_eq!(client.global.lock().expect("global").len(), 1);
        assert_eq!(client.guild.lock().expect("guild").len(), 2);
        let _ = tokio::fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn staging_scope_uses_one_guild_put_without_erasing_owner_or_public_commands() {
        let client = MockClient::default();
        let mut staging = config(None);
        staging.public_guild_id = Some("123456789012345678".into());

        let outcome = register_commands(&client, &staging)
            .await
            .expect("register staging commands");

        assert!(outcome.public_registered && outcome.owner_registered);
        assert!(client.global.lock().expect("global").is_empty());
        let guild = client.guild.lock().expect("guild");
        assert_eq!(
            guild.len(),
            1,
            "same-guild scopes must use one replacing PUT"
        );
        assert_eq!(guild[0].0, "123456789012345678");
        assert_eq!(guild[0].1.len(), 42, "40 public + 2 owner commands");
    }

    #[tokio::test]
    async fn staging_scope_change_invalidates_a_global_registration_cache() {
        let path = temporary_state_path();
        let client = MockClient::default();
        register_commands(&client, &config(Some(path.clone())))
            .await
            .expect("global register");

        let mut staging = config(Some(path.clone()));
        staging.public_guild_id = Some("123456789012345678".into());
        let outcome = register_commands(&client, &staging)
            .await
            .expect("staging register");

        assert!(outcome.public_registered);
        assert_eq!(client.global.lock().expect("global").len(), 1);
        assert_eq!(client.guild.lock().expect("guild").len(), 2);
        let _ = tokio::fs::remove_file(path).await;
    }

    #[tokio::test]
    async fn malformed_snowflakes_fail_before_any_rest_call() {
        let client = MockClient::default();
        let mut invalid = config(None);
        invalid.application_id = "wrong".into();
        assert!(matches!(
            register_commands(&client, &invalid).await,
            Err(CommandRegistrationError::InvalidApplicationId)
        ));
        assert!(client.global.lock().expect("global").is_empty());

        let mut invalid_staging = config(None);
        invalid_staging.public_guild_id = Some("not-a-guild".into());
        assert!(matches!(
            register_commands(&client, &invalid_staging).await,
            Err(CommandRegistrationError::InvalidPublicGuildId)
        ));
    }
}
