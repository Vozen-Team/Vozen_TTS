#![forbid(unsafe_code)]

//! Versioned, language-neutral contracts shared by the legacy Node runtime and Rust rewrite.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DISCORD_COMMAND_CONTRACT_VERSION: u16 = 1;
pub const VOICE_RESPONSE_I18N_CONTRACT_VERSION: u16 = 1;

/// Generated from the Node i18n catalogue. It contains only strings that a promoted Rust voice
/// interaction can currently emit, retaining the Node locale fallback semantics without a second
/// handwritten translation table.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct VoiceResponseCatalog {
    pub schema_version: u16,
    pub generated_from: String,
    pub default_locale: String,
    pub supported_locales: Vec<String>,
    pub keys: Vec<String>,
    pub messages: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum VoiceResponseContractError {
    #[error("invalid voice response i18n JSON: {0}")]
    InvalidJson(String),
    #[error("unsupported voice response schema {found}")]
    UnsupportedSchema { found: u16 },
    #[error("voice response default locale is not supported")]
    InvalidDefaultLocale,
    #[error("voice response contract contains duplicate {kind}: {value}")]
    Duplicate { kind: &'static str, value: String },
    #[error("voice response contract is missing locale messages for {locale}")]
    MissingLocale { locale: String },
    #[error("voice response contract locale {locale} has an invalid key set")]
    InvalidKeySet { locale: String },
    #[error("voice response contract has an empty message for {locale}:{key}")]
    EmptyMessage { locale: String, key: String },
}

impl VoiceResponseCatalog {
    pub fn from_json(json: &str) -> Result<Self, VoiceResponseContractError> {
        let catalog = serde_json::from_str::<Self>(json)
            .map_err(|error| VoiceResponseContractError::InvalidJson(error.to_string()))?;
        catalog.validate()?;
        Ok(catalog)
    }

    /// Private responses use the Discord client locale; public callers omit it and supply the
    /// configured guild locale. Unsupported locales fall back to English in either audience.
    #[must_use]
    pub fn resolve_locale<'a>(
        &'a self,
        interaction_locale: Option<&str>,
        guild_locale: Option<&str>,
    ) -> &'a str {
        interaction_locale
            .or(guild_locale)
            .and_then(|locale| self.normalize_locale(locale))
            .unwrap_or(&self.default_locale)
    }

    /// Resolves an existing key after locale selection. Validation ensures that every supported
    /// locale has every generated key, but the default fallback is retained defensively.
    #[must_use]
    pub fn message<'a>(&'a self, key: &str, locale: &str) -> Option<&'a str> {
        self.messages
            .get(locale)
            .and_then(|messages| messages.get(key))
            .or_else(|| {
                self.messages
                    .get(&self.default_locale)
                    .and_then(|messages| messages.get(key))
            })
            .map(String::as_str)
    }

    fn normalize_locale<'a>(&'a self, raw: &str) -> Option<&'a str> {
        let base = raw.split('-').next()?.to_ascii_lowercase();
        self.supported_locales
            .iter()
            .find(|locale| locale.as_str() == base)
            .map(String::as_str)
    }

    fn validate(&self) -> Result<(), VoiceResponseContractError> {
        if self.schema_version != VOICE_RESPONSE_I18N_CONTRACT_VERSION {
            return Err(VoiceResponseContractError::UnsupportedSchema {
                found: self.schema_version,
            });
        }
        let locales = self
            .supported_locales
            .iter()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        if locales.len() != self.supported_locales.len() {
            return Err(VoiceResponseContractError::Duplicate {
                kind: "locale",
                value: "duplicate".into(),
            });
        }
        if !locales.contains(self.default_locale.as_str()) {
            return Err(VoiceResponseContractError::InvalidDefaultLocale);
        }
        let keys = self
            .keys
            .iter()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        if keys.len() != self.keys.len() {
            return Err(VoiceResponseContractError::Duplicate {
                kind: "key",
                value: "duplicate".into(),
            });
        }
        for locale in &self.supported_locales {
            let messages = self.messages.get(locale).ok_or_else(|| {
                VoiceResponseContractError::MissingLocale {
                    locale: locale.clone(),
                }
            })?;
            if messages
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>()
                != keys
            {
                return Err(VoiceResponseContractError::InvalidKeySet {
                    locale: locale.clone(),
                });
            }
            if let Some((key, _)) = messages
                .iter()
                .find(|(_, message)| message.trim().is_empty())
            {
                return Err(VoiceResponseContractError::EmptyMessage {
                    locale: locale.clone(),
                    key: key.clone(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct DiscordCommandCatalog {
    pub schema_version: u16,
    pub generated_from: String,
    #[serde(default)]
    pub public_commands: Vec<DiscordCommand>,
    #[serde(default)]
    pub owner_commands: Vec<DiscordCommand>,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct DiscordCommand {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contexts: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_types: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<DiscordCommand>>,
    /// Fields that Discord may add or that only apply to particular option types.
    /// Keeping them losslessly prevents Rust registration from silently dropping limits,
    /// autocomplete flags, choices, channel types, or permission metadata.
    #[serde(flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ContractError {
    #[error("invalid JSON: {0}")]
    InvalidJson(String),
    #[error("unsupported command contract schema {found}")]
    UnsupportedSchema { found: u16 },
    #[error("command name is empty")]
    EmptyCommandName,
    #[error("duplicate root command name: {0}")]
    DuplicateRootCommand(String),
    #[error("unknown command path: {root} {segment}")]
    UnknownCommandPath { root: String, segment: String },
    #[error("command type mismatch for {name}: expected {expected}, received {received}")]
    CommandTypeMismatch {
        name: String,
        expected: u8,
        received: u8,
    },
}

impl DiscordCommandCatalog {
    pub fn from_json(json: &str) -> Result<Self, ContractError> {
        let catalog = serde_json::from_str::<Self>(json)
            .map_err(|error| ContractError::InvalidJson(error.to_string()))?;
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn root_commands(&self) -> impl Iterator<Item = &DiscordCommand> {
        self.public_commands.iter().chain(&self.owner_commands)
    }

    pub fn command_names(&self) -> Vec<&str> {
        self.root_commands()
            .map(|command| command.name.as_str())
            .collect()
    }

    /// JSON payloads ready for Discord command registration. They deliberately contain the
    /// original option metadata instead of a separately maintained Rust copy.
    pub fn public_registration_payload(&self) -> Result<Vec<serde_json::Value>, ContractError> {
        registration_payload(&self.public_commands)
    }

    /// Owner-only commands are registered in the configured control guild, never alongside the
    /// global public catalog. Keeping this payload here prevents a second Rust command list from
    /// drifting from the Node-generated contract.
    pub fn owner_registration_payload(&self) -> Result<Vec<serde_json::Value>, ContractError> {
        registration_payload(&self.owner_commands)
    }

    /// Resolves whether a root belongs to the owner-only catalog before a gateway adapter invokes
    /// any handler. Registration visibility is not an authorization boundary.
    pub fn is_owner_command(&self, root: &str) -> bool {
        self.owner_commands
            .iter()
            .any(|command| command.name == root)
    }

    /// Resolves the exact leaf selected by Discord before a handler is invoked.
    ///
    /// `path` contains only subcommand/subcommand-group names in order. Argument options are
    /// intentionally not accepted here: their validation belongs to the typed command handler.
    /// An unknown root, subcommand, or application-command type fails closed.
    pub fn resolve_command<'a>(
        &'a self,
        root: &str,
        command_type: u8,
        path: &[&str],
    ) -> Result<&'a DiscordCommand, ContractError> {
        let command = self
            .root_commands()
            .find(|command| command.name == root)
            .ok_or_else(|| ContractError::UnknownCommandPath {
                root: root.to_owned(),
                segment: root.to_owned(),
            })?;
        let expected = command.kind.unwrap_or(1);
        if expected != command_type {
            return Err(ContractError::CommandTypeMismatch {
                name: root.to_owned(),
                expected,
                received: command_type,
            });
        }

        path.iter().try_fold(command, |current, segment| {
            current
                .options
                .as_deref()
                .unwrap_or_default()
                .iter()
                .find(|option| option.name == *segment && matches!(option.kind, Some(1) | Some(2)))
                .ok_or_else(|| ContractError::UnknownCommandPath {
                    root: root.to_owned(),
                    segment: (*segment).to_owned(),
                })
        })
    }
}

fn registration_payload(
    commands: &[DiscordCommand],
) -> Result<Vec<serde_json::Value>, ContractError> {
    commands
        .iter()
        .map(|command| {
            serde_json::to_value(command)
                .map_err(|error| ContractError::InvalidJson(error.to_string()))
        })
        .collect()
}

impl DiscordCommandCatalog {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != DISCORD_COMMAND_CONTRACT_VERSION {
            return Err(ContractError::UnsupportedSchema {
                found: self.schema_version,
            });
        }

        let mut names = std::collections::BTreeSet::new();
        for command in self.root_commands() {
            validate_command(command)?;
            if !names.insert(command.name.as_str()) {
                return Err(ContractError::DuplicateRootCommand(command.name.clone()));
            }
        }
        Ok(())
    }
}

fn validate_command(command: &DiscordCommand) -> Result<(), ContractError> {
    if command.name.trim().is_empty() {
        return Err(ContractError::EmptyCommandName);
    }
    if let Some(options) = &command.options {
        for option in options {
            validate_command(option)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURRENT_COMMANDS: &str = include_str!("../../../contracts/discord-commands.json");
    const VOICE_RESPONSE_I18N: &str = include_str!("../../../contracts/voice-response-i18n.json");

    #[test]
    fn current_voice_response_i18n_contract_is_complete_and_uses_node_fallback_order() {
        let catalog = VoiceResponseCatalog::from_json(VOICE_RESPONSE_I18N).expect("valid i18n");
        assert_eq!(catalog.supported_locales.len(), 35);
        assert_eq!(catalog.resolve_locale(Some("fr-CA"), Some("pt")), "fr");
        assert_eq!(catalog.resolve_locale(Some("ko"), Some("pt-BR")), "en");
        assert_eq!(catalog.resolve_locale(None, Some("pt-BR")), "pt");
        assert_eq!(catalog.resolve_locale(None, None), "en");
        assert!(
            catalog
                .message("tts.notInVoice", "fr")
                .is_some_and(|message| !message.is_empty())
        );
    }

    #[test]
    fn current_discord_command_contract_is_valid() {
        let catalog = DiscordCommandCatalog::from_json(CURRENT_COMMANDS).expect("valid contract");
        assert!(catalog.command_names().contains(&"setup"));
        assert!(catalog.command_names().contains(&"server-pronunciation"));
        assert!(catalog.command_names().contains(&"vozen-grant"));
        assert_eq!(catalog.public_commands.len(), 40);
        assert_eq!(catalog.owner_commands.len(), 2);
    }

    #[test]
    fn preserves_every_public_registration_field() {
        let catalog = DiscordCommandCatalog::from_json(CURRENT_COMMANDS).expect("valid contract");
        let source: serde_json::Value =
            serde_json::from_str(CURRENT_COMMANDS).expect("source JSON");
        assert_eq!(
            serde_json::Value::Array(
                catalog
                    .public_registration_payload()
                    .expect("serializable registration payload")
            ),
            source["public_commands"]
        );
    }

    #[test]
    fn preserves_owner_registration_fields_and_marks_owner_roots() {
        let catalog = DiscordCommandCatalog::from_json(CURRENT_COMMANDS).expect("valid contract");
        let source: serde_json::Value =
            serde_json::from_str(CURRENT_COMMANDS).expect("source JSON");
        assert_eq!(
            serde_json::Value::Array(
                catalog
                    .owner_registration_payload()
                    .expect("serializable owner payload")
            ),
            source["owner_commands"]
        );
        assert!(catalog.is_owner_command("vozen-grant"));
        assert!(!catalog.is_owner_command("setup"));
    }

    #[test]
    fn rejects_duplicate_root_commands() {
        let contract = r#"{
          "schema_version": 1,
          "generated_from": "test",
          "public_commands": [{"name": "setup"}],
          "owner_commands": [{"name": "setup"}]
        }"#;
        assert_eq!(
            DiscordCommandCatalog::from_json(contract),
            Err(ContractError::DuplicateRootCommand("setup".into()))
        );
    }

    #[test]
    fn resolves_only_declared_command_paths_and_types() {
        let catalog = DiscordCommandCatalog::from_json(CURRENT_COMMANDS).expect("valid contract");
        assert_eq!(
            catalog
                .resolve_command("queue", 1, &["remove"])
                .expect("queue remove")
                .description
                .as_deref(),
            Some("Remove one of your queued items (admins may remove any item)")
        );
        assert_eq!(
            catalog
                .resolve_command("Speak", 3, &[])
                .expect("message command")
                .name,
            "Speak"
        );
        assert!(matches!(
            catalog.resolve_command("queue", 1, &["invented"]),
            Err(ContractError::UnknownCommandPath { .. })
        ));
        assert!(matches!(
            catalog.resolve_command("Speak", 1, &[]),
            Err(ContractError::CommandTypeMismatch { .. })
        ));
    }
}
