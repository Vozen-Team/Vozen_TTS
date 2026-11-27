//! Opt-in adapter for the direct `/pronunciation` and `/server-pronunciation` leaves.
//!
//! The modal path is deliberately session-bound: Discord can send a modal interaction later, so
//! Rust keeps only a short-lived user/guild binding and never trusts the custom id by itself.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use serde_json::{Value, json};
use serenity::{
    builder::{CreateActionRow, CreateInputText, CreateInteractionResponse, CreateModal},
    client::Context,
    model::{
        Permissions,
        application::{ActionRowComponent, InputTextStyle, Interaction, ModalInteraction},
    },
};
use uuid::Uuid;
use vozen_discord::{
    GatewayEventDispatchError, GatewayEventSink, PronunciationCommand, PronunciationInvocation,
    PronunciationOutcome, PronunciationScope, PronunciationService, VoiceResponseLocalizer,
    parse_pronunciation_command,
};
use vozen_store::SqliteStore;

use crate::system_now_ms;

const PRON_FORM_TTL_MS: i64 = 5 * 60_000;
const PRON_FORM_MAX: usize = 256;

#[derive(Debug, Clone)]
struct PendingPronunciationForm {
    user_id: String,
    guild_id: Option<String>,
    scope: PronunciationScope,
    locale: String,
    guild_locale: Option<String>,
    issued_at_ms: i64,
}

pub struct PronunciationGatewaySink {
    service: PronunciationService,
    localizer: VoiceResponseLocalizer,
    premium_url: String,
    payments_enabled: bool,
    pending_forms: Mutex<BTreeMap<String, PendingPronunciationForm>>,
}

impl PronunciationGatewaySink {
    pub fn new(
        store: Arc<Mutex<SqliteStore>>,
        premium_url: String,
        payments_enabled: bool,
    ) -> Result<Self, GatewayEventDispatchError> {
        Ok(Self {
            service: PronunciationService::new(store),
            localizer: VoiceResponseLocalizer::from_generated_contract()
                .map_err(|_| GatewayEventDispatchError)?,
            premium_url,
            payments_enabled,
            pending_forms: Mutex::new(BTreeMap::new()),
        })
    }

    fn prune_forms(&self, now_ms: i64) {
        if let Ok(mut forms) = self.pending_forms.lock() {
            forms.retain(|_, form| now_ms.saturating_sub(form.issued_at_ms) <= PRON_FORM_TTL_MS);
            if forms.len() > PRON_FORM_MAX {
                let mut oldest = forms
                    .iter()
                    .map(|(id, form)| (id.clone(), form.issued_at_ms))
                    .collect::<Vec<_>>();
                oldest.sort_by_key(|(_, issued_at)| *issued_at);
                for (id, _) in oldest.into_iter().take(forms.len() - PRON_FORM_MAX) {
                    forms.remove(&id);
                }
            }
        }
    }

    fn localized(
        &self,
        key: &str,
        locale: Option<&str>,
        guild_locale: Option<&str>,
        parameters: &BTreeMap<&str, String>,
    ) -> Result<String, GatewayEventDispatchError> {
        self.localizer
            .render_key(key, locale, guild_locale, parameters)
            .ok_or(GatewayEventDispatchError)
    }

    /// Matches the TypeScript `replyCard` presentation used by the real Vozen bot. Serenity 0.12
    /// does not expose Components V2 containers yet, so keep the small envelope as raw JSON just
    /// like the cast sink does. The response remains ephemeral and mention-safe.
    fn card_response_payload(content: &str) -> Value {
        let accent_color = if content.trim_start().starts_with('❌') {
            0xED4245u32
        } else if content.trim_start().starts_with('⚠') {
            0xFEE75Cu32
        } else {
            0x5865F2u32
        };
        json!({
            "type": 4,
            "data": {
                "flags": 32832,
                "components": [{
                    "type": 17,
                    "accent_color": accent_color,
                    "components": [{"type": 10, "content": content}]
                }],
                "allowed_mentions": {"parse": []}
            }
        })
    }

    fn add_modal(
        &self,
        command: &serenity::model::application::CommandInteraction,
        scope: PronunciationScope,
    ) -> Result<CreateInteractionResponse, GatewayEventDispatchError> {
        let session_id = Uuid::new_v4().to_string();
        let prefix = match scope {
            PronunciationScope::Personal => "pronAdd",
            PronunciationScope::Server => "spronAdd",
        };
        let session = PendingPronunciationForm {
            user_id: command.user.id.get().to_string(),
            guild_id: command.guild_id.map(|id| id.get().to_string()),
            scope,
            locale: command.locale.clone(),
            guild_locale: command.guild_locale.clone(),
            issued_at_ms: system_now_ms(),
        };
        self.prune_forms(session.issued_at_ms);
        self.pending_forms
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .insert(session_id.clone(), session);

        let title_key = match scope {
            PronunciationScope::Personal => "pron.modalTitle",
            PronunciationScope::Server => "spron.modalTitle",
        };
        let say_key = match scope {
            PronunciationScope::Personal => "pron.modalSay",
            PronunciationScope::Server => "spron.modalSay",
        };
        let title = self.localized(
            title_key,
            Some(&command.locale),
            command.guild_locale.as_deref(),
            &BTreeMap::new(),
        )?;
        let term_label = self.localized(
            "pron.modalTerm",
            Some(&command.locale),
            command.guild_locale.as_deref(),
            &BTreeMap::new(),
        )?;
        let say_label = self.localized(
            say_key,
            Some(&command.locale),
            command.guild_locale.as_deref(),
            &BTreeMap::new(),
        )?;
        Ok(CreateInteractionResponse::Modal(
            CreateModal::new(format!("{prefix}:{session_id}"), title).components(vec![
                CreateActionRow::InputText(
                    CreateInputText::new(InputTextStyle::Short, term_label, "term")
                        .max_length(100)
                        .required(true),
                ),
                CreateActionRow::InputText(
                    CreateInputText::new(InputTextStyle::Short, say_label, "say")
                        .max_length(200)
                        .required(true),
                ),
            ]),
        ))
    }

    async fn handle_modal(
        &self,
        context: &Context,
        modal: ModalInteraction,
    ) -> Result<bool, GatewayEventDispatchError> {
        let Some((prefix, session_id)) = parse_pronunciation_modal_id(&modal.data.custom_id) else {
            return Ok(false);
        };
        let now_ms = system_now_ms();
        self.prune_forms(now_ms);
        let session = self
            .pending_forms
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .remove(session_id);
        let Some(session) = session else {
            return Ok(true);
        };
        let guild_id = modal.guild_id.map(|id| id.get().to_string());
        if session.user_id != modal.user.id.get().to_string()
            || session.guild_id != guild_id
            || session.scope != prefix
            || now_ms.saturating_sub(session.issued_at_ms) > PRON_FORM_TTL_MS
        {
            return Ok(true);
        }
        let value = |custom_id: &str| {
            modal
                .data
                .components
                .iter()
                .flat_map(|row| row.components.iter())
                .find_map(|component| match component {
                    ActionRowComponent::InputText(input) if input.custom_id == custom_id => {
                        input.value.clone()
                    }
                    _ => None,
                })
                .map(|value| value.trim().to_owned())
        };
        let term = value("term").unwrap_or_default();
        let replacement = value("say").unwrap_or_default();
        let parameters = BTreeMap::new();
        let content = if term.is_empty() || replacement.is_empty() {
            self.localized(
                "pron.empty",
                Some(&session.locale),
                session.guild_locale.as_deref(),
                &parameters,
            )?
        } else {
            let can_manage_guild = modal
                .member
                .as_ref()
                .and_then(|member| member.permissions)
                .is_some_and(|permissions| permissions.contains(Permissions::MANAGE_GUILD));
            let outcome = self.service.execute(
                PronunciationInvocation {
                    user_id: &session.user_id,
                    guild_id: session.guild_id.as_deref(),
                    can_manage_guild,
                    now_ms,
                },
                PronunciationCommand::Add {
                    scope: session.scope,
                    term,
                    replacement,
                },
            );
            self.response_localized(&session.locale, session.guild_locale.as_deref(), outcome)?
        };
        context
            .http
            .create_interaction_response(
                modal.id,
                &modal.token,
                &Self::card_response_payload(&content),
                Vec::new(),
            )
            .await
            .map_err(|error| {
                eprintln!("[pronunciation] modal card response failed: {error}");
                GatewayEventDispatchError
            })?;
        Ok(true)
    }

    fn response(
        &self,
        command: &serenity::model::application::CommandInteraction,
        outcome: PronunciationOutcome,
    ) -> Result<String, GatewayEventDispatchError> {
        self.response_localized(&command.locale, command.guild_locale.as_deref(), outcome)
    }

    fn response_localized(
        &self,
        locale: &str,
        guild_locale: Option<&str>,
        outcome: PronunciationOutcome,
    ) -> Result<String, GatewayEventDispatchError> {
        let mut parameters = BTreeMap::new();
        match outcome {
            PronunciationOutcome::List {
                scope,
                entries,
                limit,
            } => {
                parameters.insert("count", entries.len().to_string());
                parameters.insert("limit", limit.to_string());
                let key = match scope {
                    PronunciationScope::Personal => "pron.listHeader",
                    PronunciationScope::Server => "spron.listHeader",
                };
                let header = self.localized(key, Some(locale), guild_locale, &parameters)?;
                let empty_key = match scope {
                    PronunciationScope::Personal => "pron.listEmpty",
                    PronunciationScope::Server => "spron.listEmpty",
                };
                let body = if entries.is_empty() {
                    self.localized(empty_key, Some(locale), guild_locale, &BTreeMap::new())?
                } else {
                    entries
                        .iter()
                        .map(|entry| format!("- {} -> {}", entry.term, entry.replacement))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                Ok(format!("{header}\n{body}"))
            }
            PronunciationOutcome::OpenAddForm { .. } => Err(GatewayEventDispatchError),
            PronunciationOutcome::Added {
                scope,
                term,
                replacement,
                ..
            } => {
                parameters.insert("term", term);
                parameters.insert("replacement", replacement);
                self.localized(
                    match scope {
                        PronunciationScope::Personal => "pron.set",
                        PronunciationScope::Server => "spron.set",
                    },
                    Some(locale),
                    guild_locale,
                    &parameters,
                )
            }
            PronunciationOutcome::Limit { scope, limit } => {
                parameters.insert("limit", limit.to_string());
                let mut content = self.localized(
                    match scope {
                        PronunciationScope::Personal => "pron.limitHit",
                        PronunciationScope::Server => "spron.limitHit",
                    },
                    Some(locale),
                    guild_locale,
                    &parameters,
                )?;
                if self.payments_enabled && scope == PronunciationScope::Personal && limit == 3 {
                    let mut upsell = BTreeMap::new();
                    upsell.insert("url", self.premium_url.clone());
                    content.push('\n');
                    content.push_str(&self.localized(
                        "pron.limitUpsell",
                        Some(locale),
                        guild_locale,
                        &upsell,
                    )?);
                }
                Ok(content)
            }
            PronunciationOutcome::Removed { scope, term } => {
                parameters.insert("term", term);
                self.localized(
                    match scope {
                        PronunciationScope::Personal => "pron.removed",
                        PronunciationScope::Server => "spron.removed",
                    },
                    Some(locale),
                    guild_locale,
                    &parameters,
                )
            }
            PronunciationOutcome::NotFound { scope, term } => {
                parameters.insert("term", term);
                self.localized(
                    match scope {
                        PronunciationScope::Personal => "pron.notFound",
                        PronunciationScope::Server => "spron.notFound",
                    },
                    Some(locale),
                    guild_locale,
                    &parameters,
                )
            }
            PronunciationOutcome::NeedsManageGuild | PronunciationOutcome::GuildRequired => self
                .localized(
                    "error.needManageGuild",
                    Some(locale),
                    guild_locale,
                    &BTreeMap::new(),
                ),
            PronunciationOutcome::StoreUnavailable => self.localized(
                "error.generic",
                Some(locale),
                guild_locale,
                &BTreeMap::new(),
            ),
        }
    }
}

#[async_trait::async_trait]
impl GatewayEventSink for PronunciationGatewaySink {
    async fn on_message(
        &self,
        _context: Context,
        _message: serenity::model::channel::Message,
    ) -> Result<(), GatewayEventDispatchError> {
        Ok(())
    }

    async fn on_interaction(
        &self,
        context: Context,
        interaction: Interaction,
    ) -> Result<(), GatewayEventDispatchError> {
        if let Interaction::Modal(modal) = interaction {
            self.handle_modal(&context, modal).await?;
            return Ok(());
        }
        let Interaction::Command(command) = interaction else {
            return Ok(());
        };
        let Some(parsed) =
            parse_pronunciation_command(&command.data).map_err(|_| GatewayEventDispatchError)?
        else {
            return Ok(());
        };
        let guild_id = command.guild_id.map(|id| id.get().to_string());
        let user_id = command.user.id.get().to_string();
        let can_manage_guild = command
            .member
            .as_ref()
            .and_then(|member| member.permissions)
            .is_some_and(|permissions| permissions.contains(Permissions::MANAGE_GUILD));
        let outcome = self.service.execute(
            PronunciationInvocation {
                user_id: &user_id,
                guild_id: guild_id.as_deref(),
                can_manage_guild,
                now_ms: system_now_ms(),
            },
            parsed.clone(),
        );
        if let PronunciationOutcome::OpenAddForm { scope } = outcome {
            let modal = self.add_modal(&command, scope)?;
            command
                .create_response(&context, modal)
                .await
                .map_err(|error| {
                    eprintln!("[pronunciation] add modal response failed: {error}");
                    GatewayEventDispatchError
                })?;
            return Ok(());
        }
        let content = self.response(&command, outcome)?;
        context
            .http
            .create_interaction_response(
                command.id,
                &command.token,
                &Self::card_response_payload(&content),
                Vec::new(),
            )
            .await
            .map_err(|error| {
                eprintln!("[pronunciation] command card response failed: {error}");
                GatewayEventDispatchError
            })?;
        Ok(())
    }

    async fn on_guild_delete(&self, _guild_id: &str) -> Result<(), GatewayEventDispatchError> {
        Ok(())
    }
}

fn parse_pronunciation_modal_id(custom_id: &str) -> Option<(PronunciationScope, &str)> {
    let (prefix, id) = custom_id.split_once(':')?;
    if id.is_empty() || id.contains(':') || Uuid::parse_str(id).is_err() {
        return None;
    }
    let scope = match prefix {
        "pronAdd" => PronunciationScope::Personal,
        "spronAdd" => PronunciationScope::Server,
        _ => return None,
    };
    Some((scope, id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vozen_core::PronunciationEntry;

    #[test]
    fn list_response_keeps_node_shape_and_scope_copy() {
        let sink = PronunciationGatewaySink::new(
            Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store"))),
            "https://vozen.org/#premium".into(),
            true,
        )
        .expect("sink");
        let command: serenity::model::application::CommandInteraction = serde_json::from_str(
            r#"{"id":"1","application_id":"1","data":{"id":"1","name":"pronunciation","type":1,"options":[{"name":"list","type":1,"options":[]}]},"guild_id":"2","channel_id":"3","member":null,"user":{"id":"4","username":"user","discriminator":"0"},"token":"token","version":1,"app_permissions":null,"locale":"en-US","guild_locale":null,"entitlements":[],"authorizing_integration_owners":{},"context":null,"attachment_size_limit":0}"#,
        ).expect("command");
        let content = sink
            .response(
                &command,
                PronunciationOutcome::List {
                    scope: PronunciationScope::Personal,
                    entries: vec![PronunciationEntry {
                        term: "gg".into(),
                        replacement: "good game".into(),
                    }],
                    limit: 3,
                },
            )
            .expect("response");
        assert!(content.contains("Your pronunciations"));
        assert!(content.contains("- gg -> good game"));
    }

    #[test]
    fn modal_ids_are_strict_and_scope_bound() {
        let id = Uuid::new_v4().to_string();
        assert_eq!(
            parse_pronunciation_modal_id(&format!("pronAdd:{id}")),
            Some((PronunciationScope::Personal, id.as_str()))
        );
        assert_eq!(
            parse_pronunciation_modal_id(&format!("spronAdd:{id}")),
            Some((PronunciationScope::Server, id.as_str()))
        );
        assert!(parse_pronunciation_modal_id("pronAdd:1").is_none());
        assert!(parse_pronunciation_modal_id(&format!("pronAdd:{id}:extra")).is_none());
        assert!(
            parse_pronunciation_modal_id("other:00000000-0000-0000-0000-000000000000").is_none()
        );
    }
}
