//! Private setup flow with completion checks posted in the selected reading channel.

use async_trait::async_trait;
use serenity::{
    builder::{
        CreateActionRow, CreateAllowedMentions, CreateButton, CreateChannel, CreateEmbed,
        CreateEmbedFooter, CreateInteractionResponse, CreateInteractionResponseMessage,
        CreateMessage, CreateSelectMenu, CreateSelectMenuKind, EditInteractionResponse,
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
use vozen_discord::{
    GatewayEventDispatchError, GatewayEventSink, GatewayState, VoiceResponseLocalizer,
};
use vozen_store::{GuildConfigPatch, SqliteStore};

pub const OPEN_ID: &str = "vozen:setup:open";
const PREFIX: &str = "vozen:setup:";
const SESSION_TTL_MS: i64 = 15 * 60 * 1_000;
const SESSION_LIMIT: usize = 2_048;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetupPage {
    Start,
    Existing,
    Create,
}

#[derive(Clone)]
struct PendingSetup {
    guild_id: u64,
    user_id: u64,
    issued_at: i64,
    channel: Option<ChannelId>,
    category: Option<ChannelId>,
    choose_category: bool,
    locale: String,
    autojoin: bool,
    quiet: bool,
    confirmed: bool,
    page: SetupPage,
    options_open: bool,
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
            locale: "en".into(),
            autojoin: false,
            quiet: false,
            confirmed: false,
            page: SetupPage::Start,
            options_open: false,
        }
    }
    fn permits(&self, guild: u64, user: u64, now: i64) -> bool {
        self.guild_id == guild
            && self.user_id == user
            && now >= self.issued_at
            && now - self.issued_at < SESSION_TTL_MS
    }
    fn text(&self, localizer: &VoiceResponseLocalizer, key: &str) -> String {
        setup_text(localizer, &self.locale, key)
    }

    fn allows_action(&self, action: &str) -> bool {
        if self.confirmed {
            return matches!(action, "test" | "diagnose");
        }
        match action {
            "cancel" => true,
            "existing" | "new" => self.page == SetupPage::Start && !self.options_open,
            "back" | "options" => self.page != SetupPage::Start,
            "autojoin" | "quiet" => self.page != SetupPage::Start && self.options_open,
            "channel" | "confirm" => self.page == SetupPage::Existing && !self.options_open,
            "category" | "create" => self.page == SetupPage::Create && !self.options_open,
            _ => false,
        }
    }

    fn config_patch(&self, channel_id: ChannelId) -> GuildConfigPatch {
        GuildConfigPatch {
            tts_channel_id: Some(Some(channel_id.to_string())),
            autoread: Some(true),
            autojoin: Some(self.autojoin),
            xsaid: self.quiet.then_some(false),
            greet_on_join: self.quiet.then_some(false),
            streak_announce: self.quiet.then_some(false),
            vote_promos: self.quiet.then_some(false),
            ..Default::default()
        }
    }
}

fn setup_text(localizer: &VoiceResponseLocalizer, locale: &str, key: &str) -> String {
    localizer
        .render_key(key, Some(locale), None, &BTreeMap::new())
        .expect("setup keys exist in the checked translation contract")
}

pub fn welcome_components(
    localizer: &VoiceResponseLocalizer,
    locale: &str,
) -> Vec<CreateActionRow> {
    vec![
        CreateActionRow::Buttons(vec![
            CreateButton::new_link("https://vozen.org/account/").label("Dashboard"),
            CreateButton::new_link("https://vozen.org/terms").label(setup_text(
                localizer,
                locale,
                "welcome.terms",
            )),
            CreateButton::new_link("https://vozen.org/privacy").label(setup_text(
                localizer,
                locale,
                "welcome.privacy",
            )),
        ]),
        CreateActionRow::Buttons(vec![
            CreateButton::new(OPEN_ID)
                .style(ButtonStyle::Danger)
                .label(setup_text(localizer, locale, "wizard.title")),
            CreateButton::new_link("https://vozen.org/docs/").label(setup_text(
                localizer,
                locale,
                "welcome.docs",
            )),
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
            "channel"
                | "category"
                | "create"
                | "confirm"
                | "cancel"
                | "autojoin"
                | "quiet"
                | "test"
                | "diagnose"
                | "existing"
                | "new"
                | "back"
                | "options"
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

fn is_setup_check(id: &str) -> bool {
    action_id(id).is_some_and(|(_, action)| matches!(action, "test" | "diagnose"))
}

fn setup_component_response(id: &str) -> CreateInteractionResponse {
    if is_setup_check(id) {
        // Public check buttons must never publish the actor's diagnostic result.
        CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new().ephemeral(true))
    } else {
        CreateInteractionResponse::Acknowledge
    }
}

fn completion_message(
    nonce: &str,
    session: &PendingSetup,
    channel_id: ChannelId,
    server_locale: &str,
    autojoin: bool,
    localizer: &VoiceResponseLocalizer,
) -> CreateMessage {
    let mut public_session = session.clone();
    public_session.locale = server_locale.into();
    let description = localizer
        .render_key(
            if autojoin {
                "wizard.readyDescription"
            } else {
                "wizard.readyDescriptionManualJoin"
            },
            Some(server_locale),
            None,
            &BTreeMap::from([("channel", format!("<#{}>", channel_id))]),
        )
        .expect("checked setup completion description");
    CreateMessage::new()
        .embed(
            CreateEmbed::new()
                .color(0x57F287)
                .title(public_session.text(localizer, "wizard.readyTitle"))
                .description(description)
                .footer(CreateEmbedFooter::new(
                    public_session.text(localizer, "wizard.readyFooter"),
                )),
        )
        .components(wizard_components(nonce, &public_session, localizer))
        .allowed_mentions(no_mentions())
}

fn wizard_components(
    nonce: &str,
    session: &PendingSetup,
    localizer: &VoiceResponseLocalizer,
) -> Vec<CreateActionRow> {
    let id = |action| format!("{PREFIX}{nonce}:{action}");
    if session.confirmed {
        return vec![CreateActionRow::Buttons(vec![
            CreateButton::new(id("test"))
                .style(ButtonStyle::Success)
                .label(session.text(localizer, "wizard.test")),
            CreateButton::new(id("diagnose"))
                .style(ButtonStyle::Secondary)
                .label(session.text(localizer, "wizard.diagnose")),
        ])];
    }
    if session.page == SetupPage::Start {
        return vec![CreateActionRow::Buttons(vec![
            CreateButton::new(id("existing"))
                .style(ButtonStyle::Primary)
                .label(session.text(localizer, "wizard.useExisting")),
            CreateButton::new(id("new"))
                .style(ButtonStyle::Secondary)
                .label(session.text(localizer, "wizard.create")),
            CreateButton::new(id("cancel"))
                .style(ButtonStyle::Secondary)
                .label(session.text(localizer, "premium.confirmNo")),
        ])];
    }
    if session.options_open {
        return vec![
            CreateActionRow::Buttons(vec![
                CreateButton::new(id("autojoin"))
                    .style(if session.autojoin {
                        ButtonStyle::Primary
                    } else {
                        ButtonStyle::Secondary
                    })
                    .label(session.text(
                        localizer,
                        if session.autojoin {
                            "wizard.autojoinOn"
                        } else {
                            "wizard.autojoinOff"
                        },
                    )),
                CreateButton::new(id("quiet"))
                    .style(if session.quiet {
                        ButtonStyle::Primary
                    } else {
                        ButtonStyle::Secondary
                    })
                    .label(session.text(
                        localizer,
                        if session.quiet {
                            "wizard.quietOn"
                        } else {
                            "wizard.quietOff"
                        },
                    )),
            ]),
            CreateActionRow::Buttons(vec![
                CreateButton::new(id("options"))
                    .style(ButtonStyle::Primary)
                    .label(session.text(localizer, "wizard.backToChat")),
                CreateButton::new(id("cancel"))
                    .style(ButtonStyle::Secondary)
                    .label(session.text(localizer, "premium.confirmNo")),
            ]),
        ];
    }
    let mut rows = Vec::new();
    if session.page == SetupPage::Existing {
        rows.push(CreateActionRow::SelectMenu(
            CreateSelectMenu::new(
                id("channel"),
                CreateSelectMenuKind::Channel {
                    channel_types: Some(vec![ChannelType::Text]),
                    default_channels: session.channel.map(|c| vec![c]),
                },
            )
            .placeholder(session.text(localizer, "wizard.selectChat"))
            .min_values(1)
            .max_values(1),
        ));
    } else if session.choose_category {
        rows.push(CreateActionRow::SelectMenu(
            CreateSelectMenu::new(
                id("category"),
                CreateSelectMenuKind::Channel {
                    channel_types: Some(vec![ChannelType::Category]),
                    default_channels: session.category.map(|c| vec![c]),
                },
            )
            .placeholder(session.text(localizer, "wizard.category"))
            .min_values(1)
            .max_values(1),
        ));
    }
    let primary = if session.page == SetupPage::Existing {
        CreateButton::new(id("confirm"))
            .label(session.text(localizer, "wizard.useChannel"))
            .disabled(session.channel.is_none())
    } else {
        CreateButton::new(id("create"))
            .label(session.text(localizer, "wizard.create"))
            .disabled(session.choose_category && session.category.is_none())
    };
    rows.push(CreateActionRow::Buttons(vec![
        primary.style(ButtonStyle::Success),
        CreateButton::new(id("back"))
            .style(ButtonStyle::Secondary)
            .label(session.text(localizer, "wizard.back")),
        CreateButton::new(id("options"))
            .style(ButtonStyle::Secondary)
            .label(session.text(localizer, "wizard.options")),
        CreateButton::new(id("cancel"))
            .style(ButtonStyle::Secondary)
            .label(session.text(localizer, "premium.confirmNo")),
    ]));
    rows
}

fn wizard_embed(session: &PendingSetup, localizer: &VoiceResponseLocalizer) -> CreateEmbed {
    let (title, description) = if session.options_open {
        ("wizard.options", "wizard.optionsDescription")
    } else {
        match session.page {
            SetupPage::Start => ("wizard.title", "wizard.startDescription"),
            SetupPage::Existing => ("wizard.existingTitle", "wizard.existingDescription"),
            SetupPage::Create => ("wizard.create", "wizard.createDescription"),
        }
    };
    let mut embed = CreateEmbed::new()
        .color(0x5865F2)
        .title(session.text(localizer, title))
        .description(session.text(localizer, description));
    if session.page == SetupPage::Create && !session.options_open {
        embed = embed.footer(CreateEmbedFooter::new(
            session.text(localizer, "wizard.createImmediate"),
        ));
        if !session.choose_category
            && let Some(category) = session.category
        {
            embed = embed.field(
                session.text(localizer, "wizard.categoryLabel"),
                format!("<#{}>", category),
                false,
            );
        }
    }
    embed
}

fn no_mentions() -> CreateAllowedMentions {
    CreateAllowedMentions::new()
        .all_users(false)
        .all_roles(false)
        .everyone(false)
}

pub struct SetupGatewaySink {
    localizer: VoiceResponseLocalizer,
    store: Arc<Mutex<SqliteStore>>,
    gateway: GatewayState,
    sessions: Mutex<BTreeMap<String, PendingSetup>>,
    // Serialize lookup/create/confirm so concurrent clicks cannot create duplicate channels.
    actions: tokio::sync::Mutex<()>,
    #[cfg(feature = "voice-driver")]
    voice: Option<Arc<crate::core_voice_sink::CoreVoiceGatewaySink>>,
}

impl SetupGatewaySink {
    pub fn new(
        store: Arc<Mutex<SqliteStore>>,
        gateway: GatewayState,
    ) -> Result<Self, GatewayEventDispatchError> {
        Ok(Self {
            localizer: VoiceResponseLocalizer::from_generated_contract()
                .map_err(|_| GatewayEventDispatchError)?,
            store,
            gateway,
            sessions: Mutex::new(BTreeMap::new()),
            actions: tokio::sync::Mutex::new(()),
            #[cfg(feature = "voice-driver")]
            voice: None,
        })
    }

    #[cfg(feature = "voice-driver")]
    pub fn with_voice_sink(
        mut self,
        voice: Option<Arc<crate::core_voice_sink::CoreVoiceGatewaySink>>,
    ) -> Self {
        self.voice = voice;
        self
    }

    fn text(&self, locale: &str, key: &str) -> String {
        setup_text(&self.localizer, locale, key)
    }

    fn permission_error(
        &self,
        locale: &str,
        target: &str,
        channel: Option<ChannelId>,
        actual: Permissions,
        required: Permissions,
    ) -> String {
        let mut lines = vec![
            format!(
                "{} {}",
                target,
                channel.map(|id| format!("<#{}>", id)).unwrap_or_default()
            ),
            self.text(locale, "setup.permsHeader"),
        ];
        for (flag, key) in [
            (Permissions::MANAGE_CHANNELS, "wizard.manageChannels"),
            (Permissions::VIEW_CHANNEL, "setup.permView"),
            (Permissions::SEND_MESSAGES, "setup.permSend"),
            (Permissions::CONNECT, "setup.permConnect"),
            (Permissions::SPEAK, "setup.permSpeak"),
        ] {
            if required.contains(flag) && !actual.contains(flag) {
                lines.push(
                    self.localizer
                        .render_key(
                            "setup.permMissing",
                            Some(locale),
                            None,
                            &BTreeMap::from([("label", self.text(locale, key))]),
                        )
                        .expect("checked permission label"),
                );
            }
        }
        lines.join("\n")
    }

    async fn open(
        &self,
        context: &Context,
        guild_id: GuildId,
        user_id: serenity::model::id::UserId,
        locale: &str,
    ) -> Result<(String, PendingSetup), String> {
        let text = |key| self.text(locale, key);
        let guild = guild_id
            .to_partial_guild(&context.http)
            .await
            .map_err(|_| text("error.generic"))?;
        let member = guild_id
            .member(&context.http, user_id)
            .await
            .map_err(|_| text("error.generic"))?;
        if !guild
            .member_permissions(&member)
            .contains(Permissions::MANAGE_GUILD)
        {
            return Err(text("error.needManageGuild"));
        }
        let channels = guild_id
            .channels(&context.http)
            .await
            .map_err(|_| text("error.generic"))?;
        let categories = voice_categories(&channels);
        let mut session = PendingSetup::new(guild_id.get(), user_id.get(), crate::system_now_ms());
        session.locale = locale.into();
        let config = self
            .store
            .lock()
            .map_err(|_| text("error.generic"))?
            .guild_config(&guild_id.get().to_string())
            .map_err(|_| text("error.generic"))?;
        session.autojoin = config.autojoin;
        session.channel = config
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
        let mut sessions = self.sessions.lock().map_err(|_| text("error.generic"))?;
        sessions.retain(|_, s| crate::system_now_ms() - s.issued_at < SESSION_TTL_MS);
        if sessions.len() >= SESSION_LIMIT {
            return Err(text("error.generic"));
        }
        sessions.insert(nonce.clone(), session.clone());
        Ok((nonce, session))
    }

    async fn apply_action(
        &self,
        context: &Context,
        component: &serenity::model::application::ComponentInteraction,
    ) -> Result<EditInteractionResponse, String> {
        let text = |key| self.text(&component.locale, key);
        let _guard = self.actions.lock().await;
        let (nonce, action) =
            action_id(&component.data.custom_id).ok_or_else(|| text("wizard.reopen"))?;
        if !matches!(action, "channel" | "category")
            && !matches!(component.data.kind, ComponentInteractionDataKind::Button)
        {
            return Err(text("wizard.reopen"));
        }
        let mut session = self
            .sessions
            .lock()
            .map_err(|_| text("error.generic"))?
            .get(nonce)
            .cloned()
            .ok_or_else(|| text("wizard.reopen"))?;
        let guild_id = component.guild_id.ok_or_else(|| text("wizard.reopen"))?;
        if !session.permits(
            guild_id.get(),
            component.user.id.get(),
            crate::system_now_ms(),
        ) {
            return Err(text("wizard.reopen"));
        }
        session.locale = component.locale.clone();
        if !session.allows_action(action) {
            return Err(text("wizard.reopen"));
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
                .map_err(|_| text("error.generic"))?
                .remove(nonce);
            return Ok(terminal(text("wizard.cancelled")));
        }
        // Read current Discord permissions for every action, not the stale interaction snapshot.
        let guild = guild_id
            .to_partial_guild(&context.http)
            .await
            .map_err(|_| text("error.generic"))?;
        let member = guild_id
            .member(&context.http, component.user.id)
            .await
            .map_err(|_| text("error.generic"))?;
        if !guild
            .member_permissions(&member)
            .contains(Permissions::MANAGE_GUILD)
        {
            return Err(text("error.needManageGuild"));
        }
        let bot = context
            .http
            .get_current_user()
            .await
            .map_err(|_| text("error.generic"))?;
        let bot_member = guild_id
            .member(&context.http, bot.id)
            .await
            .map_err(|_| text("error.generic"))?;
        let mut channels = guild_id
            .channels(&context.http)
            .await
            .map_err(|_| text("error.generic"))?;
        let reading_error = |c: &GuildChannel| {
            if c.kind != ChannelType::Text {
                return Some(text("wizard.channelRequired"));
            }
            let required = Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES;
            let bot_permissions = guild.user_permissions_in(c, &bot_member);
            if !bot_permissions.contains(required) {
                return Some(self.permission_error(
                    &component.locale,
                    "Vozen",
                    Some(c.id),
                    bot_permissions,
                    required,
                ));
            }
            let user_permissions = guild.user_permissions_in(c, &member);
            if !user_permissions.contains(required) {
                return Some(self.permission_error(
                    &component.locale,
                    &format!("<@{}>", component.user.id),
                    Some(c.id),
                    user_permissions,
                    required,
                ));
            }
            None
        };
        match action {
            "existing" => session.page = SetupPage::Existing,
            "new" => session.page = SetupPage::Create,
            "back" => {
                session.page = SetupPage::Start;
                session.options_open = false;
            }
            "options" => session.options_open = !session.options_open,
            "autojoin" => session.autojoin = !session.autojoin,
            "quiet" => session.quiet = !session.quiet,
            "test" | "diagnose" => {
                if session.channel != Some(component.channel_id) {
                    return Err(text("wizard.reopen"));
                }
                let voice_id = self
                    .gateway
                    .voice_channel_id(&guild_id.to_string(), &component.user.id.to_string());
                let voice_id = voice_id
                    .and_then(|id| id.parse::<u64>().ok())
                    .map(ChannelId::new);
                let voice = voice_id
                    .and_then(|id| channels.get(&id))
                    .ok_or_else(|| text("join.needVoiceChannel"))?;
                let required =
                    Permissions::VIEW_CHANNEL | Permissions::CONNECT | Permissions::SPEAK;
                let actual = guild.user_permissions_in(voice, &bot_member);
                if !actual.contains(required) {
                    return Err(self.permission_error(
                        &component.locale,
                        "Vozen",
                        Some(voice.id),
                        actual,
                        required,
                    ));
                }
                if self
                    .gateway
                    .bot_voice_channel_id(&guild_id.to_string())
                    .is_some_and(|id| id != voice.id.to_string())
                {
                    return Err(text("wizard.otherCall"));
                }
                let config = self
                    .store
                    .lock()
                    .map_err(|_| text("error.generic"))?
                    .guild_config(&guild_id.to_string())
                    .map_err(|_| text("error.generic"))?;
                if config.tts_channel_id.as_deref()
                    != Some(component.channel_id.to_string().as_str())
                {
                    return Err(text("wizard.reopen"));
                }
                let channel = config
                    .tts_channel_id
                    .as_deref()
                    .and_then(|id| id.parse::<u64>().ok())
                    .and_then(|id| channels.get(&ChannelId::new(id)))
                    .ok_or_else(|| text("wizard.channelRequired"))?;
                if let Some(error) = reading_error(channel) {
                    return Err(error);
                }
                let result = if action == "test" {
                    #[cfg(feature = "voice-driver")]
                    {
                        self.voice
                            .as_ref()
                            .ok_or_else(|| text("error.generic"))?
                            .setup_audio_test(context, component, &member, channel.id)
                            .await
                            .map_err(|_| text("error.generic"))?
                    }
                    #[cfg(not(feature = "voice-driver"))]
                    {
                        return Err(text("error.generic"));
                    }
                } else {
                    let roles = member
                        .roles
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>();
                    let bot_voice = self.gateway.bot_voice_channel_id(&guild_id.to_string());
                    let decision = vozen_discord::admit_discord_message(
                        &*self.store.lock().map_err(|_| text("error.generic"))?,
                        vozen_discord::DiscordMessageFacts {
                            guild_id: &guild_id.to_string(),
                            channel_id: &channel.id.to_string(),
                            author_id: &component.user.id.to_string(),
                            author_is_bot: false,
                            mentioned_bot: false,
                            replied_to_bot: false,
                            author_voice_channel_id: Some(&voice.id.to_string()),
                            bot_voice_channel_id: bot_voice.as_deref(),
                            member_role_ids: Some(&roles),
                            autojoined_for_author: false,
                        },
                    )
                    .map_err(|_| text("error.generic"))?;
                    use vozen_core::{
                        MessageSpeechDecision as Decision, MessageSpeechDenial as Denial,
                    };
                    text(match decision {
                        Decision::Allowed { .. } => "wizard.ready",
                        Decision::Denied {
                            reason: Denial::PassiveOptOut,
                        } => "wizard.optedOut",
                        Decision::Denied {
                            reason: Denial::RequiredRoleMissing,
                        } => "wizard.roleRequired",
                        Decision::Denied {
                            reason: Denial::NotInSameVoice,
                        } => "wizard.needJoin",
                        Decision::Denied {
                            reason: Denial::BoundVoiceMismatch,
                        } => "wizard.boundCall",
                        Decision::Denied {
                            reason: Denial::Blocked,
                        } => "tts.blocked",
                        Decision::Denied { .. } => "wizard.readingOff",
                    })
                };
                return Ok(terminal(result));
            }
            "channel" | "category" => {
                let ComponentInteractionDataKind::ChannelSelect { values } = &component.data.kind
                else {
                    return Err(text("wizard.reopen"));
                };
                if values.len() != 1 {
                    return Err(text("wizard.channelRequired"));
                }
                let channel = channels
                    .get(&values[0])
                    .ok_or_else(|| text("wizard.reopen"))?;
                if action == "channel" {
                    if let Some(error) = reading_error(channel) {
                        return Err(error);
                    }
                    session.channel = Some(channel.id);
                } else {
                    if !voice_categories(&channels).contains(&channel.id)
                        || !guild
                            .user_permissions_in(channel, &member)
                            .contains(Permissions::VIEW_CHANNEL)
                    {
                        return Err(text("wizard.category"));
                    }
                    session.category = Some(channel.id);
                }
            }
            "create" => {
                let categories = voice_categories(&channels);
                if session.category.is_some_and(|id| !categories.contains(&id)) {
                    return Err(text("wizard.reopen"));
                }
                if !categories.is_empty() && session.category.is_none() {
                    return Err(text("wizard.category"));
                }
                if let Some(category) = session.category.and_then(|id| channels.get(&id))
                    && !guild
                        .user_permissions_in(category, &member)
                        .contains(Permissions::VIEW_CHANNEL)
                {
                    return Err(text("wizard.category"));
                }
                let existing =
                    existing_chat(&channels, session.category, |c| reading_error(c).is_none());
                session.channel = match existing {
                    Some(id) => Some(id),
                    None => {
                        // Reusing a readable channel does not require permission to create one.
                        let permissions = session
                            .category
                            .and_then(|id| channels.get(&id))
                            .map(|c| guild.user_permissions_in(c, &bot_member))
                            .unwrap_or_else(|| guild.member_permissions(&bot_member));
                        let required = Permissions::MANAGE_CHANNELS
                            | Permissions::VIEW_CHANNEL
                            | Permissions::SEND_MESSAGES;
                        if !permissions.contains(required) {
                            return Err(self.permission_error(
                                &component.locale,
                                "Vozen",
                                session.category,
                                permissions,
                                required,
                            ));
                        }
                        let mut builder = CreateChannel::new("TTS Chat").kind(ChannelType::Text)
                            .topic("Write here for Vozen to read in your voice call. Use /join to connect.");
                        if let Some(parent) = session.category {
                            builder = builder.category(parent);
                        }
                        let channel = guild_id
                            .create_channel(&context.http, builder)
                            .await
                            .map_err(|_| text("error.generic"))?;
                        session.channel = Some(channel.id);
                        self.sessions
                            .lock()
                            .map_err(|_| text("error.generic"))?
                            .insert(nonce.into(), session.clone());
                        channels.insert(channel.id, channel.clone());
                        if let Some(error) = reading_error(&channel) {
                            return Err(format!("{}\n{}", text("wizard.createdUnreadable"), error));
                        }
                        Some(channel.id)
                    }
                };
                session.page = SetupPage::Existing;
            }
            "confirm" => {
                let channel_id = session
                    .channel
                    .ok_or_else(|| text("wizard.channelRequired"))?;
                let channel = channels
                    .get(&channel_id)
                    .ok_or_else(|| text("wizard.reopen"))?;
                if let Some(error) = reading_error(channel) {
                    return Err(error);
                }
                let saved = {
                    let store = self.store.lock().map_err(|_| text("error.generic"))?;
                    let saved = store
                        .update_guild_config(
                            &guild_id.get().to_string(),
                            session.config_patch(channel_id),
                        )
                        .map_err(|_| text("error.generic"))?;
                    store
                        .record_guild_setup_completed(
                            &guild_id.get().to_string(),
                            crate::system_now_ms(),
                        )
                        .map_err(|_| {
                            format!("{}\n{}", text("setup.done"), text("wizard.reopen"))
                        })?;
                    saved
                };
                session.confirmed = true;
                self.sessions
                    .lock()
                    .map_err(|_| text("error.generic"))?
                    .insert(nonce.into(), session.clone());
                let sent = channel_id
                    .send_message(
                        &context.http,
                        completion_message(
                            nonce,
                            &session,
                            channel_id,
                            &saved.locale,
                            saved.autojoin,
                            &self.localizer,
                        ),
                    )
                    .await;
                let notice = self
                    .localizer
                    .render_key(
                        if sent.is_ok() {
                            "wizard.openChannel"
                        } else {
                            "wizard.postFailed"
                        },
                        Some(&component.locale),
                        None,
                        &BTreeMap::from([("channel", format!("<#{}>", channel_id))]),
                    )
                    .expect("checked setup completion notice");
                return Ok(terminal(notice));
            }
            _ => return Err(text("wizard.reopen")),
        }
        self.sessions
            .lock()
            .map_err(|_| text("error.generic"))?
            .insert(nonce.into(), session.clone());
        Ok(EditInteractionResponse::new()
            .content("")
            .embed(wizard_embed(&session, &self.localizer))
            .components(wizard_components(nonce, &session, &self.localizer))
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
                c.create_response(&context, setup_component_response(&c.data.custom_id))
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
                            _ => self.text(&c.locale, "wizard.reopen"),
                        };
                        if is_setup_check(&c.data.custom_id) {
                            c.edit_response(
                                &context,
                                EditInteractionResponse::new()
                                    .content(text)
                                    .allowed_mentions(no_mentions()),
                            )
                            .await
                            .map_err(|_| GatewayEventDispatchError)?;
                        } else {
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
                    .embed(wizard_embed(&session, &self.localizer))
                    .components(wizard_components(&nonce, &session, &self.localizer))
                    .allowed_mentions(no_mentions()),
                other => EditInteractionResponse::new()
                    .content(match other {
                        Ok(Err(text)) => text,
                        _ => self.text(locale, "wizard.reopen"),
                    })
                    .allowed_mentions(no_mentions()),
            }
        } else {
            EditInteractionResponse::new()
                .content(self.text(locale, "wizard.reopen"))
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
    fn completion_uses_the_selected_channel_and_server_language_not_the_actor_locale() {
        let localizer = VoiceResponseLocalizer::from_generated_contract().unwrap();
        let mut session = PendingSetup::new(7, 42, 100);
        session.locale = "pt-PT".into();
        session.channel = Some(ChannelId::new(77));
        session.confirmed = true;
        let public = serde_json::to_value(completion_message(
            "0123456789abcdef0123456789abcdef",
            &session,
            session.channel.unwrap(),
            "en",
            false,
            &localizer,
        ))
        .unwrap();
        assert!(public["content"].is_null());
        assert_eq!(public["embeds"][0]["title"], "Vozen is ready");
        assert_eq!(
            public["embeds"][0]["description"],
            "Join a voice channel, use `/join`, then write in <#77>."
        );
        assert_eq!(public["embeds"][0]["color"], 0x57F287);
        assert!(
            public["embeds"][0]["footer"]["text"]
                .as_str()
                .unwrap()
                .contains("/voice")
        );
        assert_eq!(public["components"][0]["components"][0]["style"], 3);
        assert_eq!(
            public["components"][0]["components"][0]["label"],
            "Test voice"
        );
        assert_eq!(
            public["components"][0]["components"][1]["label"],
            "Check reading"
        );
        assert_eq!(public["allowed_mentions"]["parse"], serde_json::json!([]));
        assert_eq!(session.locale, "pt-PT");
        let spanish = serde_json::to_value(completion_message(
            "0123456789abcdef0123456789abcdef",
            &session,
            ChannelId::new(77),
            "es",
            true,
            &localizer,
        ))
        .unwrap();
        assert_eq!(
            spanish["components"][0]["components"][0]["label"],
            "Probar voz"
        );
        assert_eq!(spanish["embeds"][0]["title"], "Vozen está listo");
        assert_eq!(
            spanish["embeds"][0]["description"],
            "Entra en una llamada y escribe en <#77>."
        );
        for key in ["wizard.openChannel", "wizard.postFailed"] {
            let notice = localizer
                .render_key(
                    key,
                    Some(&session.locale),
                    None,
                    &BTreeMap::from([("channel", "<#77>".into())]),
                )
                .unwrap();
            assert!(notice.contains("Configuração guardada") && notice.contains("<#77>"));
        }
    }

    #[test]
    fn completion_localizes_manual_join_guidance_with_english_fallback() {
        let localizer = VoiceResponseLocalizer::from_generated_contract().unwrap();
        let mut session = PendingSetup::new(7, 42, 100);
        session.confirmed = true;
        for (locale, expected) in [
            ("pt", "Entra numa chamada, usa `/join` e escreve em <#77>."),
            (
                "es",
                "Entra en una llamada, usa `/join` y escribe en <#77>.",
            ),
            (
                "fr",
                "Join a voice channel, use `/join`, then write in <#77>.",
            ),
        ] {
            let public = serde_json::to_value(completion_message(
                "0123456789abcdef0123456789abcdef",
                &session,
                ChannelId::new(77),
                locale,
                false,
                &localizer,
            ))
            .unwrap();
            assert_eq!(public["embeds"][0]["description"], expected);
        }
    }

    #[test]
    fn public_check_buttons_acknowledge_privately_without_updating_the_public_message() {
        let nonce = "0123456789abcdef0123456789abcdef";
        for action in ["test", "diagnose"] {
            let response = serde_json::to_value(setup_component_response(&format!(
                "{PREFIX}{nonce}:{action}"
            )))
            .unwrap();
            assert_eq!(response["type"], 5);
            assert_eq!(response["data"]["flags"], 64);
        }
        for action in [
            "confirm", "cancel", "channel", "category", "quiet", "autojoin", "create",
        ] {
            let response = serde_json::to_value(setup_component_response(&format!(
                "{PREFIX}{nonce}:{action}"
            )))
            .unwrap();
            assert_eq!(response["type"], 6);
        }
    }

    #[test]
    fn setup_preserves_custom_settings_and_quiet_mode_is_explicit() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .update_guild_config(
                "7",
                GuildConfigPatch {
                    locale: Some("es".into()),
                    rate_per_min: Some(22),
                    xsaid: Some(true),
                    greet_on_join: Some(true),
                    streak_announce: Some(true),
                    vote_promos: Some(true),
                    ..Default::default()
                },
            )
            .unwrap();
        let mut session = PendingSetup::new(7, 42, 100);
        session.autojoin = true;
        store
            .update_guild_config("7", session.config_patch(ChannelId::new(77)))
            .unwrap();
        let original = store.guild_config("7").unwrap();
        assert!(
            original.xsaid
                && original.greet_on_join
                && original.streak_announce
                && original.vote_promos
        );
        session.quiet = true;
        store
            .update_guild_config("7", session.config_patch(ChannelId::new(88)))
            .unwrap();
        let quiet = store.guild_config("7").unwrap();
        assert!(
            !quiet.xsaid && !quiet.greet_on_join && !quiet.streak_announce && !quiet.vote_promos
        );
        assert_eq!(quiet.locale, "es");
        assert_eq!(quiet.rate_per_min, 22);
        assert!(quiet.autojoin && quiet.autoread);
        assert_eq!(quiet.tts_channel_id.as_deref(), Some("88"));
    }

    #[test]
    fn confirmed_setup_offers_audio_check_without_reapplying_settings() {
        let localizer = VoiceResponseLocalizer::from_generated_contract().unwrap();
        let mut session = PendingSetup::new(7, 42, 100);
        assert!(!session.autojoin && !session.quiet && !session.confirmed);
        session.confirmed = true;
        session.locale = "es-ES".into();
        let rows = serde_json::to_value(wizard_components("nonce", &session, &localizer)).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1);
        assert_eq!(rows[0]["components"][0]["label"], "Probar voz");
        assert_eq!(rows[0]["components"][1]["label"], "Comprobar lectura");
        assert!(
            rows[0]["components"]
                .as_array()
                .unwrap()
                .iter()
                .all(|b| !b["custom_id"].as_str().unwrap().ends_with(":confirm"))
        );
        assert!(!session.permits(7, 43, 101));
        assert!(!session.permits(7, 42, 100 + SESSION_TTL_MS));
    }

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
    fn existing_chat_only_shows_the_chat_picker_and_preserves_selection() {
        let localizer = VoiceResponseLocalizer::from_generated_contract().unwrap();
        let mut session = PendingSetup::new(7, 42, 100);
        session.locale = "pt-PT".into();
        session.choose_category = true;
        session.page = SetupPage::Existing;
        session.channel = Some(ChannelId::new(77));
        let rows = serde_json::to_value(wizard_components("nonce", &session, &localizer)).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 2);
        assert_eq!(
            rows[0]["components"][0]["channel_types"],
            serde_json::json!([0])
        );
        assert_eq!(rows[1]["components"][0]["disabled"], false);
        assert_eq!(rows[1]["components"][0]["label"], "Usar este chat");
        let embed = serde_json::to_value(wizard_embed(&session, &localizer)).unwrap();
        assert!(embed["fields"].is_null());
        assert_eq!(rows[0]["components"][0]["default_values"][0]["id"], 77);
    }

    #[test]
    fn guild_departure_clears_only_its_sessions() {
        let sink = SetupGatewaySink::new(
            Arc::new(Mutex::new(SqliteStore::open_in_memory().unwrap())),
            GatewayState::default(),
        )
        .unwrap();
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
        let localizer = VoiceResponseLocalizer::from_generated_contract().unwrap();
        let rows = serde_json::to_value(welcome_components(&localizer, "en")).unwrap();
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
        assert_eq!(setup["label"], "Set up Vozen");
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
    fn setup_starts_with_two_clear_choices_and_no_settings_or_pickers() {
        let localizer = VoiceResponseLocalizer::from_generated_contract().unwrap();
        let mut session = PendingSetup::new(7, 42, 100);
        let rows = serde_json::to_value(wizard_components("nonce", &session, &localizer)).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1);
        assert_eq!(rows[0]["components"].as_array().unwrap().len(), 3);
        assert_eq!(rows[0]["components"][0]["label"], "Use an existing chat");
        assert_eq!(rows[0]["components"][1]["label"], "Create TTS Chat");
        assert_eq!(rows[0]["components"][2]["label"], "Cancel");
        assert!(rows[0]["components"][0]["channel_types"].is_null());
        assert!(!session.allows_action("confirm"));
        session.page = SetupPage::Existing;
        let rows = serde_json::to_value(wizard_components("nonce", &session, &localizer)).unwrap();
        assert_eq!(
            rows[0]["components"][0]["channel_types"],
            serde_json::json!([0])
        );
        assert_eq!(rows[1]["components"][0]["disabled"], true);
    }

    #[test]
    fn creation_only_shows_voice_category_and_requires_selection_before_creation() {
        let localizer = VoiceResponseLocalizer::from_generated_contract().unwrap();
        let mut session = PendingSetup::new(7, 42, 100);
        session.page = SetupPage::Create;
        session.choose_category = true;
        let rows = serde_json::to_value(wizard_components("nonce", &session, &localizer)).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 2);
        assert_eq!(
            rows[0]["components"][0]["channel_types"],
            serde_json::json!([4])
        );
        assert_eq!(rows[1]["components"][0]["disabled"], true);
        assert!(!session.allows_action("confirm") && !session.allows_action("channel"));
        session.category = Some(ChannelId::new(55));
        let rows = serde_json::to_value(wizard_components("nonce", &session, &localizer)).unwrap();
        assert_eq!(rows[1]["components"][0]["disabled"], false);
        session.choose_category = false;
        let rows = serde_json::to_value(wizard_components("nonce", &session, &localizer)).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1);
        let embed = serde_json::to_value(wizard_embed(&session, &localizer)).unwrap();
        assert_eq!(embed["fields"][0]["value"], "<#55>");
        assert!(
            embed["footer"]["text"]
                .as_str()
                .unwrap()
                .contains("immediately")
        );
    }

    #[test]
    fn options_are_separate_and_hidden_controls_cannot_be_submitted() {
        let localizer = VoiceResponseLocalizer::from_generated_contract().unwrap();
        let mut session = PendingSetup::new(7, 42, 100);
        assert!(!session.allows_action("options") && !session.allows_action("autojoin"));
        session.page = SetupPage::Existing;
        assert!(session.allows_action("options") && !session.allows_action("quiet"));
        session.options_open = true;
        assert!(session.allows_action("quiet") && session.allows_action("autojoin"));
        assert!(!session.allows_action("confirm") && !session.allows_action("channel"));
        let rows = serde_json::to_value(wizard_components("nonce", &session, &localizer)).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 2);
        assert!(
            rows[0]["components"][0]["custom_id"]
                .as_str()
                .unwrap()
                .ends_with(":autojoin")
        );
        assert!(
            rows[0]["components"][1]["custom_id"]
                .as_str()
                .unwrap()
                .ends_with(":quiet")
        );
        assert_eq!(rows[1]["components"][0]["label"], "Back to chat");
        session.confirmed = true;
        for action in [
            "new", "existing", "back", "options", "quiet", "confirm", "create",
        ] {
            assert!(!session.allows_action(action));
        }
        assert!(session.allows_action("test") && session.allows_action("diagnose"));
    }

    #[test]
    fn permission_failure_uses_the_actor_locale_and_only_lists_missing_permissions() {
        let sink = SetupGatewaySink::new(
            Arc::new(Mutex::new(SqliteStore::open_in_memory().unwrap())),
            GatewayState::default(),
        )
        .unwrap();
        let required =
            Permissions::MANAGE_CHANNELS | Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES;
        let message = sink.permission_error(
            "pt-BR",
            "Vozen",
            Some(ChannelId::new(77)),
            Permissions::SEND_MESSAGES,
            required,
        );
        assert!(message.contains("Gerir Canais") && message.contains("<#77>"));
        assert!(message.contains("ViewChannel") && !message.contains("SendMessages"));
        assert!(!message.contains("Vozen needs") && !message.contains("Manage Channels"));
        let french = sink.permission_error(
            "fr",
            "Vozen",
            None,
            Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES,
            required,
        );
        assert!(french.contains("Gérer les salons"));
        assert!(!french.contains("ViewChannel") && !french.contains("SendMessages"));
    }

    #[test]
    fn setup_copy_covers_every_supported_locale_and_discord_region_variants() {
        let localizer = VoiceResponseLocalizer::from_generated_contract().unwrap();
        let catalog: serde_json::Value =
            serde_json::from_str(include_str!("../../../contracts/voice-response-i18n.json"))
                .unwrap();
        for locale in catalog["supported_locales"].as_array().unwrap() {
            let mut session = PendingSetup::new(7, 42, 100);
            session.locale = locale.as_str().unwrap().to_owned();
            let rows =
                serde_json::to_value(wizard_components("nonce", &session, &localizer)).unwrap();
            assert!(
                !rows[0]["components"][0]["label"]
                    .as_str()
                    .unwrap()
                    .is_empty()
            );
            assert!(!session.text(&localizer, "wizard.reopen").is_empty());
            assert!(session.text(&localizer, "wizard.reopen").contains("/setup"));
        }
        assert_eq!(
            setup_text(&localizer, "pt-PT", "wizard.confirm"),
            "Ativar leitura"
        );
        assert_eq!(
            setup_text(&localizer, "fr-CA", "wizard.confirm"),
            "Activer la lecture"
        );
        assert_eq!(
            setup_text(&localizer, "unknown", "wizard.confirm"),
            "Enable reading"
        );
    }
}
