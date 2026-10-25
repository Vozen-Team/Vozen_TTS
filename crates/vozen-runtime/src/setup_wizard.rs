//! Shared, private setup flow for `/setup` and the welcome button.

use async_trait::async_trait;
use serenity::{
    builder::{
        CreateActionRow, CreateAllowedMentions, CreateButton, CreateChannel, CreateEmbed,
        CreateEmbedFooter, CreateInteractionResponse, CreateInteractionResponseMessage,
        CreateSelectMenu, CreateSelectMenuKind, EditInteractionResponse,
    },
    client::Context,
    model::{
        Permissions,
        application::{ButtonStyle, ComponentInteractionDataKind, Interaction},
        channel::{ChannelType, GuildChannel},
        id::{ChannelId, GuildId},
    },
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};
use vozen_discord::{GatewayEventDispatchError, GatewayEventSink, GatewayState};
use vozen_store::{GuildConfigPatch, SqliteStore};

pub const OPEN_ID: &str = "vozen:setup:open";
const PREFIX: &str = "vozen:setup:";
const SESSION_TTL_MS: i64 = 15 * 60 * 1_000;
const SESSION_LIMIT: usize = 2_048;

#[derive(Clone)]
struct PendingSetup {
    guild_id: u64,
    user_id: u64,
    issued_at: i64,
    channel: Option<ChannelId>,
    category: Option<ChannelId>,
    choose_category: bool,
    pt: bool,
}

impl PendingSetup {
    fn new(guild_id: u64, user_id: u64, now: i64) -> Self {
        Self {
            guild_id,
            user_id,
            issued_at: now,
            channel: None,
            category: None,
            choose_category: false,
            pt: false,
        }
    }
    fn permits(&self, guild: u64, user: u64, now: i64) -> bool {
        self.guild_id == guild
            && self.user_id == user
            && now >= self.issued_at
            && now - self.issued_at < SESSION_TTL_MS
    }
    fn text<'a>(&self, en: &'a str, pt: &'a str) -> &'a str {
        if self.pt { pt } else { en }
    }
}

pub fn welcome_components() -> Vec<CreateActionRow> {
    vec![
        CreateActionRow::Buttons(vec![
            CreateButton::new_link("https://vozen.org/account/").label("Dashboard"),
            CreateButton::new_link("https://vozen.org/terms").label("Terms of Service"),
            CreateButton::new_link("https://vozen.org/privacy").label("Privacy Policy"),
        ]),
        CreateActionRow::Buttons(vec![
            CreateButton::new(OPEN_ID)
                .style(ButtonStyle::Danger)
                .label("Setup Vozen"),
            CreateButton::new_link("https://vozen.org/docs/").label("Documentation"),
        ]),
    ]
}

fn voice_categories(channels: &HashMap<ChannelId, GuildChannel>) -> Vec<ChannelId> {
    let mut ids: Vec<_> = channels
        .values()
        .filter(|c| matches!(c.kind, ChannelType::Voice | ChannelType::Stage))
        .filter_map(|c| c.parent_id)
        .filter(|id| {
            channels
                .get(id)
                .is_some_and(|c| c.kind == ChannelType::Category)
        })
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn action_id(id: &str) -> Option<(&str, &str)> {
    let (nonce, action) = id.strip_prefix(PREFIX)?.split_once(':')?;
    (nonce.len() == 32
        && nonce.bytes().all(|c| c.is_ascii_hexdigit())
        && matches!(
            action,
            "channel" | "category" | "create" | "confirm" | "cancel"
        ))
    .then_some((nonce, action))
}

fn existing_chat(
    channels: &HashMap<ChannelId, GuildChannel>,
    category: Option<ChannelId>,
    can_read: impl Fn(&GuildChannel) -> bool,
) -> Option<ChannelId> {
    channels
        .values()
        .filter(|c| {
            c.parent_id == category
                && c.name.replace(' ', "-").eq_ignore_ascii_case("tts-chat")
                && can_read(c)
        })
        .min_by_key(|c| c.id)
        .map(|c| c.id)
}

fn wizard_components(nonce: &str, session: &PendingSetup) -> Vec<CreateActionRow> {
    let id = |action| format!("{PREFIX}{nonce}:{action}");
    let mut rows = vec![CreateActionRow::SelectMenu(
        CreateSelectMenu::new(
            id("channel"),
            CreateSelectMenuKind::Channel {
                channel_types: Some(vec![ChannelType::Text]),
                default_channels: session.channel.map(|c| vec![c]),
            },
        )
        .placeholder(session.text(
            "Choose the text channel to read",
            "Escolhe o canal de texto para leitura",
        ))
        .min_values(1)
        .max_values(1),
    )];
    if session.choose_category {
        rows.push(CreateActionRow::SelectMenu(
            CreateSelectMenu::new(
                id("category"),
                CreateSelectMenuKind::Channel {
                    channel_types: Some(vec![ChannelType::Category]),
                    default_channels: session.category.map(|c| vec![c]),
                },
            )
            .placeholder(session.text(
                "Choose a category containing voice channels",
                "Escolhe uma categoria com chamadas",
            ))
            .min_values(1)
            .max_values(1),
        ));
    }
    rows.push(CreateActionRow::Buttons(vec![
        CreateButton::new(id("create"))
            .style(ButtonStyle::Secondary)
            .label(session.text("Create TTS Chat", "Criar TTS Chat")),
        CreateButton::new(id("confirm"))
            .style(ButtonStyle::Success)
            .label(session.text("Enable reading", "Ativar leitura"))
            .disabled(session.channel.is_none()),
        CreateButton::new(id("cancel"))
            .style(ButtonStyle::Secondary)
            .label(session.text("Cancel", "Cancelar")),
    ]));
    rows
}

fn wizard_embed(session: &PendingSetup) -> CreateEmbed {
    CreateEmbed::new().color(0x5865F2)
        .title(session.text("Set up Vozen", "Configurar Vozen"))
        .description(session.text(
            "Choose a chat, or create **TTS Chat** in your voice category.\nConfirm to start reading.",
            "Escolhe um chat ou cria **TTS Chat** na categoria das chamadas.\nConfirma para começar a leitura."))
        .footer(CreateEmbedFooter::new(session.text(
            "Creating a channel happens immediately.",
            "A criação do canal é imediata.")))
}

fn no_mentions() -> CreateAllowedMentions {
    CreateAllowedMentions::new()
        .all_users(false)
        .all_roles(false)
        .everyone(false)
}

pub struct SetupGatewaySink {
    store: Arc<Mutex<SqliteStore>>,
    gateway: GatewayState,
    sessions: Mutex<BTreeMap<String, PendingSetup>>,
    // Serialize lookup/create/confirm so concurrent clicks cannot create duplicate channels.
    actions: tokio::sync::Mutex<()>,
}

impl SetupGatewaySink {
    pub fn new(store: Arc<Mutex<SqliteStore>>, gateway: GatewayState) -> Self {
        Self {
            store,
            gateway,
            sessions: Mutex::new(BTreeMap::new()),
            actions: tokio::sync::Mutex::new(()),
        }
    }

    async fn open(
        &self,
        context: &Context,
        guild_id: GuildId,
        user_id: serenity::model::id::UserId,
        locale: &str,
    ) -> Result<(String, PendingSetup), String> {
        let guild = guild_id
            .to_partial_guild(&context.http)
            .await
            .map_err(|_| "Could not load server permissions.")?;
        let member = guild_id
            .member(&context.http, user_id)
            .await
            .map_err(|_| "Could not verify your membership.")?;
        if !guild
            .member_permissions(&member)
            .contains(Permissions::MANAGE_GUILD)
        {
            return Err(
                "You need Manage Server to configure Vozen. / Precisas de Gerir Servidor.".into(),
            );
        }
        let channels = guild_id
            .channels(&context.http)
            .await
            .map_err(|_| "Could not load channels.")?;
        let categories = voice_categories(&channels);
        let mut session = PendingSetup::new(guild_id.get(), user_id.get(), crate::system_now_ms());
        session.pt = locale.starts_with("pt");
        session.channel = self
            .store
            .lock()
            .map_err(|_| "Setup storage unavailable.")?
            .guild_config(&guild_id.get().to_string())
            .map_err(|_| "Setup storage unavailable.")?
            .tts_channel_id
            .and_then(|id| id.parse::<u64>().ok())
            .map(ChannelId::new)
            .filter(|id| {
                channels
                    .get(id)
                    .is_some_and(|c| c.kind == ChannelType::Text)
            });
        session.category = self
            .gateway
            .voice_channel_id(&guild_id.get().to_string(), &user_id.get().to_string())
            .and_then(|id| id.parse::<u64>().ok())
            .and_then(|id| channels.get(&ChannelId::new(id)))
            .and_then(|c| c.parent_id)
            .filter(|id| categories.contains(id))
            .or_else(|| (categories.len() == 1).then(|| categories[0]));
        session.choose_category = categories.len() > 1;
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let mut sessions = self.sessions.lock().map_err(|_| "Setup unavailable.")?;
        sessions.retain(|_, s| crate::system_now_ms() - s.issued_at < SESSION_TTL_MS);
        if sessions.len() >= SESSION_LIMIT {
            return Err("Setup is busy. Try again shortly.".into());
        }
        sessions.insert(nonce.clone(), session.clone());
        Ok((nonce, session))
    }

    async fn apply_action(
        &self,
        context: &Context,
        component: &serenity::model::application::ComponentInteraction,
    ) -> Result<EditInteractionResponse, String> {
        let _guard = self.actions.lock().await;
        let (nonce, action) = action_id(&component.data.custom_id)
            .ok_or("Invalid setup action. Run /setup again.")?;
        if matches!(action, "create" | "confirm" | "cancel")
            && !matches!(component.data.kind, ComponentInteractionDataKind::Button)
        {
            return Err("Invalid setup button.".into());
        }
        let mut session = self
            .sessions
            .lock()
            .map_err(|_| "Setup unavailable.")?
            .get(nonce)
            .cloned()
            .ok_or("Setup expired. Run /setup again.")?;
        let guild_id = component.guild_id.ok_or("Use setup inside a server.")?;
        if !session.permits(
            guild_id.get(),
            component.user.id.get(),
            crate::system_now_ms(),
        ) {
            return Err(
                "This setup belongs to another user/server or has expired. Run /setup again."
                    .into(),
            );
        }
        let terminal = |text: String| {
            EditInteractionResponse::new()
                .content(text)
                .embeds(vec![])
                .components(vec![])
                .allowed_mentions(no_mentions())
        };
        if action == "cancel" {
            self.sessions
                .lock()
                .map_err(|_| "Setup unavailable.")?
                .remove(nonce);
            return Ok(terminal(
                session
                    .text(
                        "Setup cancelled. Reading settings were not changed.",
                        "Configuração cancelada. As definições de leitura não foram alteradas.",
                    )
                    .into(),
            ));
        }
        // Read current Discord permissions for every action, not the stale interaction snapshot.
        let guild = guild_id
            .to_partial_guild(&context.http)
            .await
            .map_err(|_| "Could not load permissions.")?;
        let member = guild_id
            .member(&context.http, component.user.id)
            .await
            .map_err(|_| "Could not verify membership.")?;
        if !guild
            .member_permissions(&member)
            .contains(Permissions::MANAGE_GUILD)
        {
            return Err(
                "You need Manage Server to configure Vozen. / Precisas de Gerir Servidor.".into(),
            );
        }
        let bot = context
            .http
            .get_current_user()
            .await
            .map_err(|_| "Could not verify bot permissions.")?;
        let bot_member = guild_id
            .member(&context.http, bot.id)
            .await
            .map_err(|_| "Could not verify bot membership.")?;
        let mut channels = guild_id
            .channels(&context.http)
            .await
            .map_err(|_| "Could not load channels.")?;
        let can_read = |c: &GuildChannel| {
            c.kind == ChannelType::Text
                && guild
                    .user_permissions_in(c, &bot_member)
                    .contains(Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES)
                && guild
                    .user_permissions_in(c, &member)
                    .contains(Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES)
        };
        match action {
            "channel" | "category" => {
                let ComponentInteractionDataKind::ChannelSelect { values } = &component.data.kind
                else {
                    return Err("Invalid channel selection.".into());
                };
                if values.len() != 1 {
                    return Err("Choose exactly one channel.".into());
                }
                let channel = channels
                    .get(&values[0])
                    .ok_or("That channel no longer exists.")?;
                if action == "channel" {
                    if !can_read(channel) {
                        return Err("You and Vozen need View Channel and Send Messages in that text channel.".into());
                    }
                    session.channel = Some(channel.id);
                } else {
                    if !voice_categories(&channels).contains(&channel.id)
                        || !guild
                            .user_permissions_in(channel, &member)
                            .contains(Permissions::VIEW_CHANNEL)
                    {
                        return Err("Choose a visible category containing voice channels.".into());
                    }
                    session.category = Some(channel.id);
                }
            }
            "create" => {
                let categories = voice_categories(&channels);
                if session.category.is_some_and(|id| !categories.contains(&id)) {
                    return Err("The voice category changed. Run /setup again.".into());
                }
                if !categories.is_empty() && session.category.is_none() {
                    return Err(session
                        .text(
                            "Choose the voice category first.",
                            "Escolhe primeiro a categoria das chamadas.",
                        )
                        .into());
                }
                let permissions = session
                    .category
                    .and_then(|id| channels.get(&id))
                    .map(|c| guild.user_permissions_in(c, &bot_member))
                    .unwrap_or_else(|| guild.member_permissions(&bot_member));
                if !permissions.contains(
                    Permissions::MANAGE_CHANNELS
                        | Permissions::VIEW_CHANNEL
                        | Permissions::SEND_MESSAGES,
                ) {
                    return Err("Vozen needs Manage Channels, View Channel and Send Messages there. Choose an existing channel instead.".into());
                }
                if let Some(category) = session.category.and_then(|id| channels.get(&id))
                    && !guild
                        .user_permissions_in(category, &member)
                        .contains(Permissions::VIEW_CHANNEL)
                {
                    return Err("You cannot access that category.".into());
                }
                let existing = existing_chat(&channels, session.category, can_read);
                session.channel = match existing {
                    Some(id) => Some(id),
                    None => {
                        let mut builder = CreateChannel::new("TTS Chat").kind(ChannelType::Text)
                            .topic("Write here for Vozen to read in your voice call. Use /join to connect.");
                        if let Some(parent) = session.category {
                            builder = builder.category(parent);
                        }
                        let channel = guild_id.create_channel(&context.http, builder).await
                            .map_err(|_| "Could not create TTS Chat. Check Manage Channels and try again.")?;
                        session.channel = Some(channel.id);
                        self.sessions
                            .lock()
                            .map_err(|_| "Setup unavailable.")?
                            .insert(nonce.into(), session.clone());
                        channels.insert(channel.id, channel.clone());
                        if !can_read(&channel) {
                            return Err("TTS Chat was created but category permissions block reading. Adjust permissions or select another channel.".into());
                        }
                        Some(channel.id)
                    }
                };
            }
            "confirm" => {
                let channel_id = session.channel.ok_or("Choose a reading channel first.")?;
                let channel = channels
                    .get(&channel_id)
                    .ok_or("The selected channel no longer exists.")?;
                if !can_read(channel) {
                    return Err("Channel permissions changed. You and Vozen need View Channel and Send Messages.".into());
                }
                let store = self
                    .store
                    .lock()
                    .map_err(|_| "Setup storage unavailable.")?;
                store
                    .update_guild_config(
                        &guild_id.get().to_string(),
                        GuildConfigPatch {
                            tts_channel_id: Some(Some(channel_id.get().to_string())),
                            autoread: Some(true),
                            ..Default::default()
                        },
                    )
                    .map_err(|_| "Could not save setup. Try again.")?;
                store.record_guild_setup_completed(&guild_id.get().to_string(), crate::system_now_ms())
                    .map_err(|_| "Reading channel saved, but setup reporting failed. Reopen /setup to verify.")?;
                self.sessions
                    .lock()
                    .map_err(|_| "Setup unavailable.")?
                    .remove(nonce);
                return Ok(terminal(format!("{} <#{}>\n{}", session.text("Reading enabled in", "Leitura ativada em"), channel_id,
                    session.text("Join a voice channel, run /join, and write there. No voice channel or category was created.",
                        "Entra numa chamada, usa /join e escreve nesse chat. Não foi criado nenhum canal de voz nem categoria."))));
            }
            _ => return Err("Unknown setup action. Run /setup again.".into()),
        }
        self.sessions
            .lock()
            .map_err(|_| "Setup unavailable.")?
            .insert(nonce.into(), session.clone());
        Ok(EditInteractionResponse::new()
            .content("")
            .embed(wizard_embed(&session))
            .components(wizard_components(nonce, &session))
            .allowed_mentions(no_mentions()))
    }
}

#[async_trait]
impl GatewayEventSink for SetupGatewaySink {
    async fn on_message(
        &self,
        _: Context,
        _: serenity::model::channel::Message,
    ) -> Result<(), GatewayEventDispatchError> {
        Ok(())
    }

    async fn on_interaction(
        &self,
        context: Context,
        interaction: Interaction,
    ) -> Result<(), GatewayEventDispatchError> {
        let (guild, user, locale) = match &interaction {
            Interaction::Command(c) if c.data.name == "setup" && c.data.options.is_empty() => {
                c.defer_ephemeral(&context)
                    .await
                    .map_err(|_| GatewayEventDispatchError)?;
                (c.guild_id, c.user.id, c.locale.as_str())
            }
            Interaction::Component(c) if c.data.custom_id == OPEN_ID => {
                c.create_response(
                    &context,
                    CreateInteractionResponse::Defer(
                        CreateInteractionResponseMessage::new().ephemeral(true),
                    ),
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
                (c.guild_id, c.user.id, c.locale.as_str())
            }
            Interaction::Component(c) if c.data.custom_id.starts_with(PREFIX) => {
                c.create_response(&context, CreateInteractionResponse::Acknowledge)
                    .await
                    .map_err(|_| GatewayEventDispatchError)?;
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(20),
                    self.apply_action(&context, c),
                )
                .await;
                match result {
                    Ok(Ok(edit)) => {
                        c.edit_response(&context, edit)
                            .await
                            .map_err(|_| GatewayEventDispatchError)?;
                    }
                    error => {
                        let text = match error {
                            Ok(Err(text)) => text,
                            _ => "Setup timed out. Reopen /setup to verify before retrying.".into(),
                        };
                        c.create_followup(
                            &context,
                            serenity::builder::CreateInteractionResponseFollowup::new()
                                .content(text)
                                .ephemeral(true)
                                .allowed_mentions(no_mentions()),
                        )
                        .await
                        .map_err(|_| GatewayEventDispatchError)?;
                    }
                }
                return Ok(());
            }
            _ => return Ok(()),
        };
        let result = if let Some(guild) = guild {
            match tokio::time::timeout(
                std::time::Duration::from_secs(15),
                self.open(&context, guild, user, locale),
            )
            .await
            {
                Ok(Ok((nonce, session))) => EditInteractionResponse::new()
                    .embed(wizard_embed(&session))
                    .components(wizard_components(&nonce, &session))
                    .allowed_mentions(no_mentions()),
                other => EditInteractionResponse::new()
                    .content(match other {
                        Ok(Err(text)) => text,
                        _ => "Setup could not load. Try /setup again.".into(),
                    })
                    .allowed_mentions(no_mentions()),
            }
        } else {
            EditInteractionResponse::new()
                .content("Use /setup inside a server.")
                .allowed_mentions(no_mentions())
        };
        match &interaction {
            Interaction::Command(c) => {
                c.edit_response(&context, result)
                    .await
                    .map_err(|_| GatewayEventDispatchError)?;
            }
            Interaction::Component(c) => {
                c.edit_response(&context, result)
                    .await
                    .map_err(|_| GatewayEventDispatchError)?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn on_guild_delete(&self, guild_id: &str) -> Result<(), GatewayEventDispatchError> {
        self.sessions
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .retain(|_, s| s.guild_id.to_string() != guild_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_bind_actor_guild_and_expiry() {
        let session = PendingSetup::new(7, 42, 100);
        assert!(session.permits(7, 42, 101));
        assert!(!session.permits(8, 42, 101));
        assert!(!session.permits(7, 43, 101));
        assert!(!session.permits(7, 42, 100 + SESSION_TTL_MS));
        assert!(!session.permits(7, 42, 99));
    }

    #[test]
    fn malformed_and_foreign_component_ids_are_rejected() {
        let nonce = "0123456789abcdef0123456789abcdef";
        for action in ["channel", "category", "create", "confirm", "cancel"] {
            let id = format!("{PREFIX}{nonce}:{action}");
            assert_eq!(action_id(&id), Some((nonce, action)));
        }
        for id in [
            OPEN_ID,
            "other:setup",
            "vozen:setup:short:create",
            "vozen:setup:0123456789abcdef0123456789abcdef:delete",
            "vozen:setup:0123456789abcdef0123456789abcdeg:create",
            "vozen:setup:0123456789abcdef0123456789abcdef:create:extra",
        ] {
            assert!(action_id(id).is_none(), "accepted {id}");
        }
    }

    #[test]
    fn existing_chat_is_reused_only_in_the_selected_category_with_permissions() {
        let category = Some(ChannelId::new(5));
        let mut channels = HashMap::new();
        for (id, name, parent) in [
            (20, "TTS Chat", category),
            (21, "tts-chat", category),
            (10, "tts-chat", None),
            (9, "general", category),
        ] {
            let mut c = GuildChannel::default();
            c.id = ChannelId::new(id);
            c.name = name.into();
            c.parent_id = parent;
            c.kind = ChannelType::Text;
            channels.insert(c.id, c);
        }
        assert_eq!(
            existing_chat(&channels, category, |_| true),
            Some(ChannelId::new(20))
        );
        assert_eq!(
            existing_chat(&channels, None, |_| true),
            Some(ChannelId::new(10))
        );
        assert_eq!(existing_chat(&channels, category, |_| false), None);
        assert_eq!(
            existing_chat(&channels, Some(ChannelId::new(99)), |_| true),
            None
        );
    }

    #[test]
    fn confirming_is_enabled_only_after_selection_and_category_picker_is_conditional() {
        let mut session = PendingSetup::new(7, 42, 100);
        session.pt = true;
        session.choose_category = true;
        session.channel = Some(ChannelId::new(77));
        let rows = serde_json::to_value(wizard_components("nonce", &session)).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 3);
        assert_eq!(
            rows[1]["components"][0]["channel_types"],
            serde_json::json!([4])
        );
        assert_eq!(rows[2]["components"][1]["disabled"], false);
        assert_eq!(rows[2]["components"][1]["label"], "Ativar leitura");
        let embed = serde_json::to_value(wizard_embed(&session)).unwrap();
        assert!(embed["fields"].is_null());
        assert_eq!(rows[0]["components"][0]["default_values"][0]["id"], 77);
    }

    #[test]
    fn guild_departure_clears_only_its_sessions() {
        let sink = SetupGatewaySink::new(
            Arc::new(Mutex::new(SqliteStore::open_in_memory().unwrap())),
            GatewayState::default(),
        );
        sink.sessions
            .lock()
            .unwrap()
            .insert("a".into(), PendingSetup::new(7, 42, 100));
        sink.sessions
            .lock()
            .unwrap()
            .insert("b".into(), PendingSetup::new(8, 42, 100));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(sink.on_guild_delete("7")).unwrap();
        assert!(!sink.sessions.lock().unwrap().contains_key("a"));
        assert!(sink.sessions.lock().unwrap().contains_key("b"));
    }

    #[test]
    fn only_categories_with_voice_channels_are_candidates() {
        let mut channels = std::collections::HashMap::new();
        for (id, kind, parent) in [
            (10, ChannelType::Category, None),
            (11, ChannelType::Category, None),
            (12, ChannelType::Voice, Some(10)),
            (13, ChannelType::Text, Some(11)),
            (14, ChannelType::Stage, Some(10)),
        ] {
            let mut channel = GuildChannel::default();
            channel.id = ChannelId::new(id);
            channel.kind = kind;
            channel.parent_id = parent.map(ChannelId::new);
            channels.insert(channel.id, channel);
        }
        assert_eq!(voice_categories(&channels), vec![ChannelId::new(10)]);
    }

    #[test]
    fn welcome_has_five_labelled_buttons_and_a_red_setup_action() {
        let rows = serde_json::to_value(welcome_components()).unwrap();
        let buttons: Vec<_> = rows
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|row| row["components"].as_array().unwrap())
            .collect();
        assert_eq!(buttons.len(), 5);
        let setup = buttons
            .iter()
            .find(|button| button["custom_id"] == OPEN_ID)
            .unwrap();
        assert_eq!(setup["style"], 4);
        assert_eq!(setup["label"], "Setup Vozen");
        assert_eq!(buttons[0]["url"], "https://vozen.org/account/");
        assert_eq!(buttons[1]["label"], "Terms of Service");
        assert_eq!(buttons[2]["label"], "Privacy Policy");
        assert_eq!(buttons[4]["label"], "Documentation");
        assert_eq!(
            buttons.iter().filter(|button| button["style"] == 5).count(),
            4
        );
    }

    #[test]
    fn wizard_never_saves_on_open_and_requires_a_channel_to_confirm() {
        let session = PendingSetup::new(7, 42, 100);
        let rows = serde_json::to_value(wizard_components("nonce", &session)).unwrap();
        assert_eq!(
            rows[0]["components"][0]["channel_types"],
            serde_json::json!([0])
        );
        assert_eq!(rows[1]["components"][1]["disabled"], true);
        assert_eq!(rows[1]["components"][0]["label"], "Create TTS Chat");
    }
}
