//! Opt-in adapter for explicit translation and server translation administration.
//!
//! Automatic message delivery and member preferences remain separate canaries. This sink owns
//! only the command leaves represented by the typed parsers and never claims them unless the
//! matching runtime flag is enabled.

use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use serenity::{
    builder::{
        CreateAllowedMentions, CreateInteractionResponse, CreateInteractionResponseMessage,
        CreateMessage, EditInteractionResponse,
    },
    client::Context,
    model::{
        Permissions,
        application::{CommandInteraction, CommandType, Interaction},
        channel::{ChannelType, Reaction, ReactionType},
        id::ChannelId,
    },
};
use vozen_discord::{
    ExplicitTranslationInvocation, ExplicitTranslationOutcome, ExplicitTranslationProvider,
    ExplicitTranslationService, GatewayEventDispatchError, GatewayEventSink,
    TranslationAdminCommand, VoiceResponseLocalizer, parse_translate_message_command,
    parse_translate_preview_command, parse_translate_text_command, parse_translation_admin_command,
};
use vozen_store::{GuildConfigPatch, SqliteStore, StoreError, TranslationMapping};

use crate::{system_now_ms, translation_provider::RuntimeTranslationProvider};

const USER_APP_TRANSLATION_SCOPE: &str = "@user-app";
const MAX_TRANSLATION_RESPONSE_UTF16: usize = 1_800;

pub struct TranslationTextGatewaySink {
    store: Arc<Mutex<SqliteStore>>,
    service: ExplicitTranslationService<RuntimeTranslationProvider>,
    localizer: VoiceResponseLocalizer,
    text_enabled: bool,
    admin_enabled: bool,
    context_enabled: bool,
    provider_enabled: bool,
}

impl TranslationTextGatewaySink {
    fn card_edit_payload(content: &str) -> Value {
        json!({
            "flags": 32768,
            "components": [{
                "type": 17,
                "accent_color": 0x5865F2u32,
                "components": [{"type": 10, "content": content}]
            }],
            "allowed_mentions": {"parse": []}
        })
    }

    pub fn new(
        store: Arc<Mutex<SqliteStore>>,
        provider: RuntimeTranslationProvider,
        text_enabled: bool,
        admin_enabled: bool,
        context_enabled: bool,
    ) -> Result<Self, GatewayEventDispatchError> {
        let localizer = VoiceResponseLocalizer::from_generated_contract()
            .map_err(|_| GatewayEventDispatchError)?;
        let provider_enabled = provider.is_enabled();
        Ok(Self {
            service: ExplicitTranslationService::new(
                Arc::clone(&store),
                provider,
                Arc::new(system_now_ms),
            ),
            store,
            localizer,
            text_enabled,
            admin_enabled,
            context_enabled,
            provider_enabled,
        })
    }

    fn message(
        &self,
        key: &str,
        interaction_locale: &str,
        guild_locale: Option<&str>,
        parameters: &BTreeMap<&str, String>,
    ) -> Result<String, GatewayEventDispatchError> {
        self.localizer
            .render_key(key, Some(interaction_locale), guild_locale, parameters)
            .ok_or(GatewayEventDispatchError)
    }

    fn target_locale(
        &self,
        explicit_locale: Option<String>,
        preference_scope: &str,
        user_id: &str,
        interaction_locale: &str,
    ) -> Result<String, GatewayEventDispatchError> {
        let configured_locale = if explicit_locale.is_none() {
            self.store
                .lock()
                .map_err(|_| GatewayEventDispatchError)?
                .translation_preference(preference_scope, user_id)
                .map_err(|_| GatewayEventDispatchError)?
                .locale
        } else {
            None
        };
        Ok(explicit_locale.or(configured_locale).unwrap_or_else(|| {
            self.localizer
                .default_for_discord_locale(Some(interaction_locale))
        }))
    }

    async fn respond_ephemeral(
        &self,
        context: &Context,
        command: &CommandInteraction,
        content: impl Into<String>,
    ) -> Result<(), GatewayEventDispatchError> {
        command
            .create_response(
                context,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content(content)
                        .ephemeral(true)
                        .allowed_mentions(no_mentions()),
                ),
            )
            .await
            .map_err(|_| GatewayEventDispatchError)
    }

    fn can_manage_guild(command: &CommandInteraction) -> bool {
        command
            .member
            .as_ref()
            .and_then(|member| member.permissions)
            .is_some_and(|permissions| permissions.contains(Permissions::MANAGE_GUILD))
    }

    fn store_translation_admin(
        &self,
        command: &CommandInteraction,
        admin: &TranslationAdminCommand,
    ) -> Result<String, GatewayEventDispatchError> {
        let Some(guild_id) = command.guild_id.map(|id| id.get().to_string()) else {
            return Ok("This translation setting is available only in a server.".to_owned());
        };
        let store = self.store.lock().map_err(|_| GatewayEventDispatchError)?;
        let content = match admin {
            TranslationAdminCommand::Status => {
                let config = store
                    .guild_config(&guild_id)
                    .map_err(|_| GatewayEventDispatchError)?;
                let mappings = store
                    .translation_mappings(&guild_id)
                    .map_err(|_| GatewayEventDispatchError)?;
                let limits =
                    translation_limits(&store, &guild_id, &command.user.id.get().to_string())?;
                format!(
                    "Translation: **{}**\nProvider: {}\nMappings: {}\nRolling 30-day cap: {} characters (you: {})",
                    if config.translation_enabled {
                        "on"
                    } else {
                        "off"
                    },
                    if self.provider_enabled {
                        "Azure"
                    } else {
                        "not configured (disabled)"
                    },
                    mappings.len(),
                    format_number(limits.0),
                    format_number(limits.1),
                )
            }
            TranslationAdminCommand::Enable => {
                if !self.provider_enabled {
                    "Translation is disabled because the operator has not configured a provider. No messages will be sent externally.".to_owned()
                } else if store
                    .translation_mappings(&guild_id)
                    .map_err(|_| GatewayEventDispatchError)?
                    .is_empty()
                {
                    "Add a valid source-to-destination mapping before enabling translation."
                        .to_owned()
                } else {
                    store
                        .update_guild_config(
                            &guild_id,
                            GuildConfigPatch {
                                translation_enabled: Some(true),
                                ..Default::default()
                            },
                        )
                        .map_err(|_| GatewayEventDispatchError)?;
                    "Translation enabled for the configured channels. It never speaks translated text.".to_owned()
                }
            }
            TranslationAdminCommand::Disable => {
                store
                    .update_guild_config(
                        &guild_id,
                        GuildConfigPatch {
                            translation_enabled: Some(false),
                            ..Default::default()
                        },
                    )
                    .map_err(|_| GatewayEventDispatchError)?;
                "Translation disabled. Existing mappings remain saved until removed.".to_owned()
            }
            TranslationAdminCommand::Clear => {
                store
                    .clear_translation_config(&guild_id)
                    .map_err(|_| GatewayEventDispatchError)?;
                store
                    .update_guild_config(
                        &guild_id,
                        GuildConfigPatch {
                            translation_enabled: Some(false),
                            ..Default::default()
                        },
                    )
                    .map_err(|_| GatewayEventDispatchError)?;
                "Translation mappings and member opt-outs were deleted; translation remains disabled.".to_owned()
            }
            TranslationAdminCommand::MapList => {
                let mappings = store
                    .translation_mappings(&guild_id)
                    .map_err(|_| GatewayEventDispatchError)?;
                if mappings.is_empty() {
                    "No translation mappings are configured.".to_owned()
                } else {
                    mappings
                        .into_iter()
                        .map(|mapping| {
                            format!(
                                "<#{}> -> <#{}> ({})",
                                mapping.source_channel_id,
                                mapping.destination_channel_id,
                                mapping.target_locale
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
            TranslationAdminCommand::MapRemove { source_channel_id } => {
                if store
                    .remove_translation_mapping(&guild_id, &source_channel_id.to_string())
                    .map_err(|_| GatewayEventDispatchError)?
                {
                    "Translation mapping removed.".to_owned()
                } else {
                    "No translation mapping exists for that source channel.".to_owned()
                }
            }
            TranslationAdminCommand::MapAdd { .. } => {
                return Err(GatewayEventDispatchError);
            }
        };
        Ok(content)
    }

    async fn channels_can_be_mapped(
        &self,
        context: &Context,
        command: &CommandInteraction,
        source_channel_id: u64,
        destination_channel_id: u64,
    ) -> Result<bool, GatewayEventDispatchError> {
        let Some(guild_id) = command.guild_id else {
            return Ok(false);
        };
        if source_channel_id == destination_channel_id {
            return Ok(false);
        }
        let guild = guild_id
            .to_partial_guild(&context.http)
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        let channels = guild_id
            .channels(&context.http)
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        let Some(source) = channels.get(&ChannelId::new(source_channel_id)) else {
            return Ok(false);
        };
        let Some(destination) = channels.get(&ChannelId::new(destination_channel_id)) else {
            return Ok(false);
        };
        if source.kind != ChannelType::Text || destination.kind != ChannelType::Text {
            return Ok(false);
        }
        let bot = context
            .http
            .get_current_user()
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        let bot_member = guild_id
            .member(&context.http, bot.id)
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        let source_permissions = guild.user_permissions_in(source, &bot_member);
        let destination_permissions = guild.user_permissions_in(destination, &bot_member);
        Ok(source_permissions.contains(Permissions::VIEW_CHANNEL)
            && destination_permissions
                .contains(Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES))
    }

    async fn handle_message_context(
        &self,
        context: &Context,
        command: &CommandInteraction,
    ) -> Result<(), GatewayEventDispatchError> {
        let Some(parsed) = parse_translate_message_command(&command.data)
            .map_err(|_| GatewayEventDispatchError)?
        else {
            return Ok(());
        };
        command
            .defer_ephemeral(context)
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        let raw = parsed.message.content.trim();
        let guild_id = command.guild_id.map(|id| id.get().to_string());
        let preference_scope = guild_id.as_deref().unwrap_or(USER_APP_TRANSLATION_SCOPE);
        let user_id = command.user.id.get().to_string();
        let guild_locale = command.guild_locale.as_deref();
        let empty_parameters = BTreeMap::new();
        let content = if raw.is_empty() {
            self.message(
                "translation.empty",
                &command.locale,
                guild_locale,
                &empty_parameters,
            )?
        } else {
            match self.target_locale(None, preference_scope, &user_id, &command.locale) {
                Err(_) => self.message(
                    "translation.unavailable",
                    &command.locale,
                    guild_locale,
                    &empty_parameters,
                )?,
                Ok(target_locale) if !self.localizer.supports_explicit_locale(&target_locale) => {
                    self.message(
                        "translation.invalidLocale",
                        &command.locale,
                        guild_locale,
                        &empty_parameters,
                    )?
                }
                Ok(target_locale) => match self
                    .service
                    .execute(ExplicitTranslationInvocation {
                        guild_id: guild_id.as_deref(),
                        user_id: &user_id,
                        text: raw,
                        target_locale: &target_locale,
                    })
                    .await
                {
                    ExplicitTranslationOutcome::Ready { text, .. } => {
                        let mut parameters = BTreeMap::new();
                        parameters.insert("locale", target_locale);
                        parameters.insert(
                            "text",
                            truncate_utf16(&text, MAX_TRANSLATION_RESPONSE_UTF16),
                        );
                        self.message(
                            "translation.ready",
                            &command.locale,
                            guild_locale,
                            &parameters,
                        )?
                    }
                    ExplicitTranslationOutcome::Empty => self.message(
                        "translation.empty",
                        &command.locale,
                        guild_locale,
                        &empty_parameters,
                    )?,
                    ExplicitTranslationOutcome::Disabled => self.message(
                        "translation.disabled",
                        &command.locale,
                        guild_locale,
                        &empty_parameters,
                    )?,
                    ExplicitTranslationOutcome::QuotaExceeded => self.message(
                        "translation.quota",
                        &command.locale,
                        guild_locale,
                        &empty_parameters,
                    )?,
                    ExplicitTranslationOutcome::Unavailable
                    | ExplicitTranslationOutcome::StoreUnavailable => self.message(
                        "translation.unavailable",
                        &command.locale,
                        guild_locale,
                        &empty_parameters,
                    )?,
                },
            }
        };
        context
            .http
            .edit_original_interaction_response(
                &command.token,
                &Self::card_edit_payload(&content),
                Vec::new(),
            )
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        Ok(())
    }

    async fn handle_admin(
        &self,
        context: &Context,
        command: &CommandInteraction,
        admin: TranslationAdminCommand,
    ) -> Result<(), GatewayEventDispatchError> {
        if command.guild_id.is_none() {
            return self
                .respond_ephemeral(
                    context,
                    command,
                    "This translation setting is available only in a server.",
                )
                .await;
        }
        if !Self::can_manage_guild(command) {
            return self
                .respond_ephemeral(
                    context,
                    command,
                    "You need Manage Server to configure translation.",
                )
                .await;
        }
        if let TranslationAdminCommand::MapAdd {
            source_channel_id,
            destination_channel_id,
            target_locale,
        } = &admin
        {
            if !self.localizer.supports_explicit_locale(target_locale) {
                return self
                    .respond_ephemeral(context, command, "That locale is not supported.")
                    .await;
            }
            // The channel checks below make several sequential Discord requests; acknowledge
            // first so a slow API cannot miss the 3-second interaction deadline.
            command
                .defer_ephemeral(context)
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            let mappable = self
                .channels_can_be_mapped(
                    context,
                    command,
                    *source_channel_id,
                    *destination_channel_id,
                )
                .await;
            let content = match mappable {
                Err(_) => {
                    "Vozen could not check those channels right now. Please try again.".to_owned()
                }
                Ok(false) => "Both channels must be distinct text channels that Vozen can view; it must also be able to send in the destination.".to_owned(),
                Ok(true) => {
                    let guild_id = command
                        .guild_id
                        .ok_or(GatewayEventDispatchError)?
                        .get()
                        .to_string();
                    let result = self
                        .store
                        .lock()
                        .map_err(|_| GatewayEventDispatchError)?
                        .upsert_translation_mapping(&TranslationMapping {
                            guild_id,
                            source_channel_id: source_channel_id.to_string(),
                            destination_channel_id: destination_channel_id.to_string(),
                            target_locale: target_locale.clone(),
                        });
                    match result {
                        Ok(()) => format!(
                            "Mapping saved: <#{}> -> <#{}> ({}).",
                            source_channel_id, destination_channel_id, target_locale
                        ),
                        Err(StoreError::TranslationCycle) => {
                            "That mapping would create a translation loop and was rejected."
                                .to_owned()
                        }
                        Err(_) => "The mapping could not be saved. Please try again.".to_owned(),
                    }
                }
            };
            return command
                .edit_response(
                    context,
                    EditInteractionResponse::new()
                        .content(content)
                        .allowed_mentions(no_mentions()),
                )
                .await
                .map(|_| ())
                .map_err(|_| GatewayEventDispatchError);
        }
        let content = self.store_translation_admin(command, &admin)?;
        self.respond_ephemeral(context, command, content).await
    }
}

#[async_trait::async_trait]
impl GatewayEventSink for TranslationTextGatewaySink {
    async fn on_message(
        &self,
        _context: Context,
        _message: serenity::model::channel::Message,
    ) -> Result<(), GatewayEventDispatchError> {
        Ok(())
    }

    async fn on_reaction_add(
        &self,
        context: Context,
        reaction: Reaction,
    ) -> Result<(), GatewayEventDispatchError> {
        if !self.text_enabled || !self.provider_enabled || reaction.guild_id.is_none() {
            return Ok(());
        }
        let Some(user_id) = reaction.user_id else {
            return Ok(());
        };
        let ReactionType::Unicode(emoji) = &reaction.emoji else {
            return Ok(());
        };
        let Some(target_locale) = vozen_discord::reaction_target_locale(emoji) else {
            return Ok(());
        };

        // Gateway reactions normally include a partial member. If it is absent, fetch the user
        // and fail closed on an HTTP error so an unknown actor can never trigger translation.
        let reactor_is_bot = match reaction.member.as_ref() {
            Some(member) => member.user.bot,
            None => match user_id.to_user(&context.http).await {
                Ok(user) => user.bot,
                Err(_) => return Ok(()),
            },
        };
        if reactor_is_bot {
            return Ok(());
        }

        let message = match reaction
            .channel_id
            .message(&context.http, reaction.message_id)
            .await
        {
            Ok(message) => message,
            Err(_) => return Ok(()),
        };
        let Some(message_guild_id) = message.guild_id else {
            return Ok(());
        };
        if message.content.trim().is_empty() || message.author.bot || message.webhook_id.is_some() {
            return Ok(());
        }

        let guild_id = message_guild_id.get().to_string();
        let user_id = user_id.get().to_string();
        let outcome = self
            .service
            .execute(ExplicitTranslationInvocation {
                guild_id: Some(&guild_id),
                user_id: &user_id,
                text: &message.content,
                target_locale,
            })
            .await;
        let ExplicitTranslationOutcome::Ready { text, .. } = outcome else {
            return Ok(());
        };
        let content = format!(
            "**Translation · {target_locale}**\n{}",
            truncate_utf16(&text, MAX_TRANSLATION_RESPONSE_UTF16)
        );
        message
            .channel_id
            .send_message(
                &context.http,
                CreateMessage::new()
                    .content(content)
                    .reference_message(&message)
                    .allowed_mentions(no_mentions()),
            )
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        Ok(())
    }

    async fn on_interaction(
        &self,
        context: Context,
        interaction: Interaction,
    ) -> Result<(), GatewayEventDispatchError> {
        let Interaction::Command(command) = interaction else {
            return Ok(());
        };
        if self.context_enabled
            && command.data.kind == CommandType::Message
            && command.data.name == vozen_discord::TRANSLATE_MESSAGE_COMMAND
        {
            return self.handle_message_context(&context, &command).await;
        }
        if self.admin_enabled {
            let parsed_admin = parse_translation_admin_command(&command.data)
                .map_err(|_| GatewayEventDispatchError)?;
            if let Some(admin) = parsed_admin {
                return self.handle_admin(&context, &command, admin).await;
            }
        }
        if !self.text_enabled {
            return Ok(());
        }
        let parsed_text =
            parse_translate_text_command(&command.data).map_err(|_| GatewayEventDispatchError)?;
        let parsed_preview = parse_translate_preview_command(&command.data)
            .map_err(|_| GatewayEventDispatchError)?;
        let Some((text, explicit_locale, is_preview)) = parsed_text
            .map(|parsed| (parsed.text, parsed.target_locale, false))
            .or_else(|| {
                parsed_preview.map(|parsed| (parsed.text, Some(parsed.target_locale), true))
            })
        else {
            return Ok(());
        };
        let guild_id = command.guild_id.map(|id| id.get().to_string());
        if is_preview {
            let can_manage_guild = command
                .member
                .as_ref()
                .and_then(|member| member.permissions)
                .is_some_and(|permissions| {
                    permissions.contains(serenity::model::Permissions::MANAGE_GUILD)
                });
            if guild_id.is_none() || !can_manage_guild {
                command
                    .create_response(
                        &context,
                        serenity::builder::CreateInteractionResponse::Message(
                            serenity::builder::CreateInteractionResponseMessage::new()
                                .content("You need Manage Server to configure translation.")
                                .ephemeral(true),
                        ),
                    )
                    .await
                    .map_err(|_| GatewayEventDispatchError)?;
                return Ok(());
            }
        }
        // Parse before defer: every unpromoted `/translate` subcommand remains Node-owned.
        command
            .defer_ephemeral(&context)
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        let preference_scope = guild_id.as_deref().unwrap_or(USER_APP_TRANSLATION_SCOPE);
        let user_id = command.user.id.get().to_string();
        let guild_locale = command.guild_locale.as_deref();
        let parameters = BTreeMap::new();
        let content = match self.target_locale(
            explicit_locale,
            preference_scope,
            &user_id,
            &command.locale,
        ) {
            // The interaction has already been deferred, so a transient SQLite failure must
            // still receive a bounded public response instead of silently timing out.
            Err(_) => self.message(
                "translation.unavailable",
                &command.locale,
                guild_locale,
                &parameters,
            )?,
            Ok(target_locale) if !self.localizer.supports_explicit_locale(&target_locale) => self
                .message(
                "translation.invalidLocale",
                &command.locale,
                guild_locale,
                &parameters,
            )?,
            Ok(target_locale) => match self
                .service
                .execute(ExplicitTranslationInvocation {
                    guild_id: guild_id.as_deref(),
                    user_id: &user_id,
                    text: &text,
                    target_locale: &target_locale,
                })
                .await
            {
                ExplicitTranslationOutcome::Ready { text, .. } => {
                    if is_preview {
                        format!(
                            "Preview ({target_locale}):\n{}",
                            truncate_utf16(&text, MAX_TRANSLATION_RESPONSE_UTF16)
                        )
                    } else {
                        let mut parameters = BTreeMap::new();
                        parameters.insert("locale", target_locale);
                        parameters.insert(
                            "text",
                            truncate_utf16(&text, MAX_TRANSLATION_RESPONSE_UTF16),
                        );
                        self.message(
                            "translation.ready",
                            &command.locale,
                            guild_locale,
                            &parameters,
                        )?
                    }
                }
                ExplicitTranslationOutcome::Empty => {
                    if is_preview {
                        "Provide readable text and a supported target locale.".to_owned()
                    } else {
                        self.message(
                            "translation.empty",
                            &command.locale,
                            guild_locale,
                            &parameters,
                        )?
                    }
                }
                ExplicitTranslationOutcome::Disabled => {
                    if is_preview {
                        "Translation is currently disabled.".to_owned()
                    } else {
                        self.message(
                            "translation.disabled",
                            &command.locale,
                            guild_locale,
                            &parameters,
                        )?
                    }
                }
                ExplicitTranslationOutcome::QuotaExceeded => {
                    if is_preview {
                        "The rolling 30-day translation limit has been reached.".to_owned()
                    } else {
                        self.message(
                            "translation.quota",
                            &command.locale,
                            guild_locale,
                            &parameters,
                        )?
                    }
                }
                ExplicitTranslationOutcome::Unavailable
                | ExplicitTranslationOutcome::StoreUnavailable => {
                    if is_preview {
                        "Translation is temporarily unavailable.".to_owned()
                    } else {
                        self.message(
                            "translation.unavailable",
                            &command.locale,
                            guild_locale,
                            &parameters,
                        )?
                    }
                }
            },
        };
        // Provider text is never trusted to control mentions. The Node automatic-translation
        // sender also uses an empty parse set; keep the private response equally non-pinging.
        command
            .edit_response(
                &context,
                EditInteractionResponse::new()
                    .content(content)
                    .allowed_mentions(
                        CreateAllowedMentions::new()
                            .all_users(false)
                            .all_roles(false)
                            .everyone(false),
                    ),
            )
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        Ok(())
    }

    async fn on_guild_delete(&self, _guild_id: &str) -> Result<(), GatewayEventDispatchError> {
        Ok(())
    }
}

fn truncate_utf16(input: &str, max_units: usize) -> String {
    let mut units = 0;
    let mut end = input.len();
    for (index, character) in input.char_indices() {
        let character_units = character.len_utf16();
        if units + character_units > max_units {
            end = index;
            break;
        }
        units += character_units;
    }
    input[..end].to_owned()
}

fn no_mentions() -> CreateAllowedMentions {
    CreateAllowedMentions::new()
        .all_users(false)
        .all_roles(false)
        .everyone(false)
}

fn translation_limits(
    store: &SqliteStore,
    guild_id: &str,
    user_id: &str,
) -> Result<(i64, i64), GatewayEventDispatchError> {
    let now_ms = system_now_ms();
    let guild_premium = store
        .is_guild_premium(guild_id, now_ms)
        .map_err(|_| GatewayEventDispatchError)?;
    let user_premium = store
        .is_user_premium(user_id, now_ms)
        .map_err(|_| GatewayEventDispatchError)?;
    Ok((
        if guild_premium {
            vozen_discord::PREMIUM_GUILD_TRANSLATION_LIMIT
        } else {
            vozen_discord::FREE_GUILD_TRANSLATION_LIMIT
        },
        if guild_premium || user_premium {
            vozen_discord::PREMIUM_USER_TRANSLATION_LIMIT
        } else {
            vozen_discord::FREE_USER_TRANSLATION_LIMIT
        },
    ))
}

fn format_number(value: i64) -> String {
    let negative = value < 0;
    let digits = value.unsigned_abs().to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(character);
    }
    if negative {
        format!("-{grouped}")
    } else {
        grouped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command() -> CommandInteraction {
        serde_json::from_str(
            r#"{"id":"1","application_id":"1","data":{"id":"1","name":"translate","type":1,"options":[]},"guild_id":"2","channel_id":"3","member":null,"user":{"id":"4","username":"user","discriminator":"0"},"token":"token","version":1,"app_permissions":null,"locale":"en-US","guild_locale":null,"entitlements":[],"authorizing_integration_owners":{},"context":null,"attachment_size_limit":0}"#,
        )
        .expect("command")
    }

    fn sink() -> TranslationTextGatewaySink {
        TranslationTextGatewaySink::new(
            Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store"))),
            RuntimeTranslationProvider::Disabled,
            false,
            true,
            false,
        )
        .expect("sink")
    }

    #[test]
    fn truncates_provider_output_without_splitting_unicode() {
        assert_eq!(truncate_utf16("ab😀", 3), "ab");
        assert_eq!(truncate_utf16("ab😀", 4), "ab😀");
    }

    #[test]
    fn formats_status_limits_like_the_node_handler() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(100_000), "100,000");
        assert_eq!(format_number(10_000), "10,000");
        assert_eq!(format_number(-1_000), "-1,000");
        let sink = sink();
        let content = sink
            .store_translation_admin(&command(), &TranslationAdminCommand::Status)
            .expect("status");
        assert!(content.contains("Translation: **off**"));
        assert!(content.contains("Provider: not configured (disabled)"));
        assert!(content.contains("Rolling 30-day cap: 100,000 characters (you: 10,000)"));
    }

    #[test]
    fn admin_mutations_preserve_mapping_and_disable_semantics() {
        let sink = sink();
        sink.store
            .lock()
            .expect("store")
            .upsert_translation_mapping(&TranslationMapping {
                guild_id: "2".into(),
                source_channel_id: "123".into(),
                destination_channel_id: "456".into(),
                target_locale: "pt".into(),
            })
            .expect("mapping");
        let command = command();
        let list = sink
            .store_translation_admin(&command, &TranslationAdminCommand::MapList)
            .expect("list");
        assert_eq!(list, "<#123> -> <#456> (pt)");
        let enable = sink
            .store_translation_admin(&command, &TranslationAdminCommand::Enable)
            .expect("enable");
        assert_eq!(
            enable,
            "Translation is disabled because the operator has not configured a provider. No messages will be sent externally."
        );
        let remove = sink
            .store_translation_admin(
                &command,
                &TranslationAdminCommand::MapRemove {
                    source_channel_id: 123,
                },
            )
            .expect("remove");
        assert_eq!(remove, "Translation mapping removed.");
        assert!(
            sink.store
                .lock()
                .expect("store")
                .translation_mappings("2")
                .expect("mappings")
                .is_empty()
        );
    }
}
