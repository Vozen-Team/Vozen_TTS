//! Opt-in ephemeral adapter for the textual preference leaves of `/voice`.
//!
//! The mixed `/voice` surface also contains the interactive configuration panel. Preview playback
//! is owned by the core voice sink so it shares the live Songbird queue; this sink owns the
//! browser, configuration panel and durable preference leaves.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serenity::{
    builder::{
        CreateActionRow, CreateAllowedMentions, CreateButton, CreateEmbed,
        CreateInteractionResponse, CreateInteractionResponseMessage, CreateSelectMenu,
        CreateSelectMenuKind, CreateSelectMenuOption, EditInteractionResponse,
    },
    client::Context,
    model::application::{
        ButtonStyle, ComponentInteraction, ComponentInteractionDataKind, Interaction,
    },
};
use vozen_discord::{
    GatewayEventDispatchError, GatewayEventSink, VoiceDisplayCatalog, VoicePreferenceCommand,
    VoicePreferenceInvocation, VoicePreferenceOutcome, VoicePreferenceService,
    VoicePreferenceSettings, VoiceResponseLocalizer, parse_voice_preference_command,
    voice_engine_supports_model,
};
use vozen_store::{SqliteStore, UserEngine, VoiceEffect};

use crate::system_now_ms;
use crate::ui::message_embed;

const BROWSE_PAGE_SIZE: usize = 8;
const BROWSE_TTL_SECONDS: i64 = 120;
const MAX_BROWSE_SESSIONS: usize = 512;
const CONFIG_TTL_MS: i64 = 120_000;
const MAX_CONFIG_SESSIONS: usize = 256;
const CONFIG_LANG_PAGE_SIZE: usize = 24;
const CONFIG_SPEEDS: [f64; 6] = [0.5, 0.75, 1.0, 1.25, 1.5, 2.0];

#[derive(Clone, Debug, PartialEq, Eq)]
struct BrowseVoice {
    id: String,
    label: String,
    engine: String,
}

#[derive(Clone, Debug)]
struct BrowseSession {
    owner_user_id: String,
    locale: String,
    voices: Vec<BrowseVoice>,
    favourites: BTreeSet<String>,
    recent: BTreeSet<String>,
    page: usize,
    expires_at: i64,
}

#[derive(Clone, Debug)]
struct VoiceConfigSession {
    owner_user_id: String,
    guild_id: String,
    channel_id: String,
    locale: String,
    guild_locale: Option<String>,
    models: Vec<String>,
    locales: Vec<String>,
    model: String,
    engine: String,
    speed: f64,
    language_page: usize,
    issued_at_ms: i64,
}

impl VoiceConfigSession {
    fn select_compatible_engine(&mut self) {
        if !voice_engines_for_model(&self.model).any(|engine| engine_token(engine) == self.engine) {
            self.engine = engine_token(UserEngine::Google).to_owned();
        }
    }
}

pub struct VoicePreferenceGatewaySink {
    store: Arc<Mutex<SqliteStore>>,
    service: VoicePreferenceService,
    localizer: VoiceResponseLocalizer,
    displays: VoiceDisplayCatalog,
    available_models: Vec<String>,
    default_speed: f64,
    browse_sessions: Arc<Mutex<BTreeMap<String, BrowseSession>>>,
    config_sessions: Arc<Mutex<BTreeMap<String, VoiceConfigSession>>>,
}

impl VoicePreferenceGatewaySink {
    pub fn new(
        store: Arc<Mutex<SqliteStore>>,
        settings: VoicePreferenceSettings,
    ) -> Result<Self, GatewayEventDispatchError> {
        let available_models = settings.available_models.clone();
        let default_speed = settings.default_speed;
        Ok(Self {
            store: store.clone(),
            service: VoicePreferenceService::new(store, settings),
            localizer: VoiceResponseLocalizer::from_generated_contract()
                .map_err(|_| GatewayEventDispatchError)?,
            displays: VoiceDisplayCatalog::from_generated_contract()
                .map_err(|_| GatewayEventDispatchError)?,
            available_models,
            default_speed,
            browse_sessions: Arc::new(Mutex::new(BTreeMap::new())),
            config_sessions: Arc::new(Mutex::new(BTreeMap::new())),
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

    fn prune_config_sessions(&self, now_ms: i64) {
        if let Ok(mut sessions) = self.config_sessions.lock() {
            sessions
                .retain(|_, session| now_ms.saturating_sub(session.issued_at_ms) <= CONFIG_TTL_MS);
            if sessions.len() > MAX_CONFIG_SESSIONS {
                let mut oldest = sessions
                    .iter()
                    .map(|(id, session)| (id.clone(), session.issued_at_ms))
                    .collect::<Vec<_>>();
                oldest.sort_by_key(|(_, issued_at)| *issued_at);
                for (id, _) in oldest
                    .into_iter()
                    .take(sessions.len() - MAX_CONFIG_SESSIONS)
                {
                    sessions.remove(&id);
                }
            }
        }
    }

    fn config_locales(&self) -> Vec<String> {
        let mut locales = self
            .available_models
            .iter()
            .map(|model| model.split('-').next().unwrap_or(model).to_owned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        locales.sort_by(|left, right| {
            let left_model = self
                .available_models
                .iter()
                .find(|model| model.starts_with(&format!("{left}-")))
                .map(String::as_str)
                .unwrap_or(left);
            let right_model = self
                .available_models
                .iter()
                .find(|model| model.starts_with(&format!("{right}-")))
                .map(String::as_str)
                .unwrap_or(right);
            self.displays
                .language_name(Some("en"), &self.available_models, left_model)
                .cmp(
                    &self
                        .displays
                        .language_name(Some("en"), &self.available_models, right_model),
                )
        });
        locales
    }

    fn models_for_locale<'a>(&'a self, models: &'a [String], locale: &str) -> Vec<&'a String> {
        models
            .iter()
            .filter(|model| model.split('-').next().unwrap_or(model) == locale)
            .collect()
    }

    fn config_summary(
        &self,
        session: &VoiceConfigSession,
    ) -> Result<String, GatewayEventDispatchError> {
        let mut parameters = BTreeMap::new();
        parameters.insert(
            "voice",
            self.displays
                .voice_name(Some(&session.locale), &session.models, &session.model),
        );
        parameters.insert("engine", config_engine_label(&session.engine).to_owned());
        parameters.insert("speed", session.speed.to_string());
        let title = self.message(
            "voice.config.title",
            &session.locale,
            session.guild_locale.as_deref(),
            &BTreeMap::new(),
        )?;
        let summary = self.message(
            "voice.config.summary",
            &session.locale,
            session.guild_locale.as_deref(),
            &parameters,
        )?;
        Ok(format!("{title}\n{summary}"))
    }

    fn config_components(
        &self,
        session_id: &str,
        session: &VoiceConfigSession,
    ) -> Result<Vec<CreateActionRow>, GatewayEventDispatchError> {
        let page_count = session.locales.len().div_ceil(CONFIG_LANG_PAGE_SIZE).max(1);
        let page = session.language_page % page_count;
        let start = page * CONFIG_LANG_PAGE_SIZE;
        let mut language_options = session.locales
            [start..(start + CONFIG_LANG_PAGE_SIZE).min(session.locales.len())]
            .iter()
            .map(|locale| {
                let model = self
                    .models_for_locale(&session.models, locale)
                    .first()
                    .map(|model| model.as_str())
                    .unwrap_or(locale);
                CreateSelectMenuOption::new(
                    self.displays
                        .language_name(Some(&session.locale), &session.models, model),
                    locale,
                )
                .default_selection(
                    session.model.split('-').next().unwrap_or(&session.model) == locale,
                )
            })
            .collect::<Vec<_>>();
        if page_count > 1 {
            language_options.push(CreateSelectMenuOption::new(
                self.message(
                    "voice.config.more",
                    &session.locale,
                    session.guild_locale.as_deref(),
                    &BTreeMap::new(),
                )?,
                "__more__",
            ));
        }
        let language = CreateSelectMenu::new(
            format!("vcfg:lang:{session_id}"),
            CreateSelectMenuKind::String {
                options: language_options,
            },
        )
        .placeholder(self.message(
            "voice.config.pickLanguage",
            &session.locale,
            session.guild_locale.as_deref(),
            &BTreeMap::new(),
        )?)
        .min_values(1)
        .max_values(1);
        let mut rows = vec![CreateActionRow::SelectMenu(language)];

        let current_locale = session.model.split('-').next().unwrap_or(&session.model);
        let voices = self.models_for_locale(&session.models, current_locale);
        if voices.len() > 1 {
            let voice_options = voices
                .into_iter()
                .map(|model| {
                    CreateSelectMenuOption::new(
                        self.displays
                            .voice_name(Some(&session.locale), &session.models, model),
                        model,
                    )
                    .default_selection(model == &session.model)
                })
                .collect();
            rows.push(CreateActionRow::SelectMenu(
                CreateSelectMenu::new(
                    format!("vcfg:voice:{session_id}"),
                    CreateSelectMenuKind::String {
                        options: voice_options,
                    },
                )
                .placeholder(self.message(
                    "voice.config.pickVoice",
                    &session.locale,
                    session.guild_locale.as_deref(),
                    &BTreeMap::new(),
                )?)
                .min_values(1)
                .max_values(1),
            ));
        }

        let engines = voice_engines_for_model(&session.model)
            .map(|engine| {
                let label = if engine == UserEngine::Kokoro {
                    format!("💎 {}", engine_label(engine))
                } else {
                    engine_label(engine).to_owned()
                };
                let token = engine_token(engine);
                CreateSelectMenuOption::new(label, token).default_selection(token == session.engine)
            })
            .collect();
        rows.push(CreateActionRow::SelectMenu(
            CreateSelectMenu::new(
                format!("vcfg:engine:{session_id}"),
                CreateSelectMenuKind::String { options: engines },
            )
            .placeholder(self.message(
                "voice.config.pickEngine",
                &session.locale,
                session.guild_locale.as_deref(),
                &BTreeMap::new(),
            )?)
            .min_values(1)
            .max_values(1),
        ));

        let speeds = CONFIG_SPEEDS
            .into_iter()
            .map(|speed| {
                CreateSelectMenuOption::new(format!("{speed}×"), speed.to_string())
                    .default_selection((speed - session.speed).abs() < f64::EPSILON)
            })
            .collect();
        rows.push(CreateActionRow::SelectMenu(
            CreateSelectMenu::new(
                format!("vcfg:speed:{session_id}"),
                CreateSelectMenuKind::String { options: speeds },
            )
            .placeholder(self.message(
                "voice.config.pickSpeed",
                &session.locale,
                session.guild_locale.as_deref(),
                &BTreeMap::new(),
            )?)
            .min_values(1)
            .max_values(1),
        ));
        let save = self.message(
            "voice.config.save",
            &session.locale,
            session.guild_locale.as_deref(),
            &BTreeMap::new(),
        )?;
        let cancel = self.message(
            "voice.config.cancel",
            &session.locale,
            session.guild_locale.as_deref(),
            &BTreeMap::new(),
        )?;
        rows.push(CreateActionRow::Buttons(vec![
            CreateButton::new(format!("vcfg:save:{session_id}"))
                .label(save)
                .style(ButtonStyle::Success)
                .emoji('💾'),
            CreateButton::new(format!("vcfg:cancel:{session_id}"))
                .label(cancel)
                .style(ButtonStyle::Secondary)
                .emoji('✖'),
        ]));
        Ok(rows)
    }

    async fn config_command(
        &self,
        context: &Context,
        command: serenity::model::application::CommandInteraction,
    ) -> Result<(), GatewayEventDispatchError> {
        let Some(guild_id) = command.guild_id else {
            return Ok(());
        };
        if self.available_models.is_empty() {
            let content = self.message(
                "voice.listEmpty",
                &command.locale,
                command.guild_locale.as_deref(),
                &BTreeMap::new(),
            )?;
            command
                .create_response(
                    context,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content(content)
                            .ephemeral(true),
                    ),
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            return Ok(());
        }
        let guild_id = guild_id.get().to_string();
        let user_id = command.user.id.get().to_string();
        let (guild_config, current_voice) = {
            let store = self.store.lock().map_err(|_| GatewayEventDispatchError)?;
            (
                store
                    .guild_config(&guild_id)
                    .map_err(|_| GatewayEventDispatchError)?,
                store
                    .get_user_voice(&guild_id, &user_id)
                    .map_err(|_| GatewayEventDispatchError)?,
            )
        };
        let default_model = if self.available_models.contains(&guild_config.default_voice) {
            guild_config.default_voice
        } else {
            self.available_models[0].clone()
        };
        let model = current_voice
            .as_ref()
            .map(|voice| voice.model.clone())
            .filter(|model| self.available_models.contains(model))
            .unwrap_or(default_model);
        let speed = current_voice
            .as_ref()
            .map(|voice| voice.speed)
            .filter(|speed| speed.is_finite() && CONFIG_SPEEDS.contains(speed))
            .unwrap_or(self.default_speed);
        let engine = current_voice
            .as_ref()
            // Google HD is no longer offered. Treat profiles created before this
            // removal as the free Google gTTS choice instead of rendering a dead option.
            .map(|voice| match voice.engine {
                UserEngine::Gcloud => "google",
                engine => engine_token(engine),
            })
            .unwrap_or("google")
            .to_owned();
        let session_id = command.id.get().to_string();
        let mut session = VoiceConfigSession {
            owner_user_id: user_id,
            guild_id,
            channel_id: command.channel_id.get().to_string(),
            locale: command.locale.clone(),
            guild_locale: command.guild_locale.clone(),
            models: self.available_models.clone(),
            locales: self.config_locales(),
            model,
            engine,
            speed,
            language_page: 0,
            issued_at_ms: system_now_ms(),
        };
        session.select_compatible_engine();
        let content = self.config_summary(&session)?;
        let rows = self.config_components(&session_id, &session)?;
        self.prune_config_sessions(session.issued_at_ms);
        self.config_sessions
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .insert(session_id, session);
        command
            .create_response(
                context,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .content(content)
                        .ephemeral(true)
                        .components(rows),
                ),
            )
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        Ok(())
    }

    async fn respond_config_expired(
        &self,
        context: &Context,
        component: &ComponentInteraction,
        locale: &str,
        guild_locale: Option<&str>,
    ) -> Result<bool, GatewayEventDispatchError> {
        let content = self.message(
            "voice.config.expired",
            locale,
            guild_locale,
            &BTreeMap::new(),
        )?;
        component
            .create_response(
                context,
                CreateInteractionResponse::UpdateMessage(
                    CreateInteractionResponseMessage::new()
                        .content(content)
                        .components(Vec::new()),
                ),
            )
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        Ok(true)
    }

    async fn config_component(
        &self,
        context: &Context,
        component: ComponentInteraction,
    ) -> Result<bool, GatewayEventDispatchError> {
        let Some((action, session_id)) = parse_config_component(&component.data.custom_id) else {
            return Ok(false);
        };
        let now_ms = system_now_ms();
        self.prune_config_sessions(now_ms);
        let session = self
            .config_sessions
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .get(session_id)
            .cloned();
        let Some(mut session) = session else {
            // The panel outlived its session (TTL prune, eviction or restart). Say so instead of
            // letting Discord report a failed interaction.
            return self
                .respond_config_expired(
                    context,
                    &component,
                    &component.locale,
                    component.guild_locale.as_deref(),
                )
                .await;
        };
        let guild_id = component.guild_id.map(|id| id.get().to_string());
        if session.owner_user_id != component.user.id.get().to_string()
            || session.guild_id != guild_id.unwrap_or_default()
            || session.channel_id != component.channel_id.get().to_string()
        {
            return Ok(true);
        }

        if now_ms.saturating_sub(session.issued_at_ms) > CONFIG_TTL_MS {
            self.config_sessions
                .lock()
                .map_err(|_| GatewayEventDispatchError)?
                .remove(session_id);
            return self
                .respond_config_expired(
                    context,
                    &component,
                    &session.locale,
                    session.guild_locale.as_deref(),
                )
                .await;
        }

        self.config_sessions
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .remove(session_id);

        if action == "cancel" {
            let content = self.message(
                "voice.config.cancelled",
                &session.locale,
                session.guild_locale.as_deref(),
                &BTreeMap::new(),
            )?;
            component
                .create_response(
                    context,
                    CreateInteractionResponse::UpdateMessage(
                        CreateInteractionResponseMessage::new()
                            .content(content)
                            .components(Vec::new()),
                    ),
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            return Ok(true);
        }

        if action == "save" {
            let outcome = self.service.execute(
                VoicePreferenceInvocation {
                    guild_id: Some(&session.guild_id),
                    user_id: &session.owner_user_id,
                    now_ms,
                },
                VoicePreferenceCommand::Set {
                    model: session.model.clone(),
                    speed: Some(session.speed),
                    engine: Some(session.engine.clone()),
                },
            );
            if matches!(outcome, VoicePreferenceOutcome::SavedVoice { .. }) {
                let content = self.localized_outcome(
                    outcome,
                    &session.locale,
                    session.guild_locale.as_deref(),
                )?;
                component
                    .create_response(
                        context,
                        CreateInteractionResponse::UpdateMessage(
                            CreateInteractionResponseMessage::new()
                                .content(content)
                                .components(Vec::new()),
                        ),
                    )
                    .await
                    .map_err(|_| GatewayEventDispatchError)?;
                return Ok(true);
            }
            let content =
                self.localized_outcome(outcome, &session.locale, session.guild_locale.as_deref())?;
            self.config_sessions
                .lock()
                .map_err(|_| GatewayEventDispatchError)?
                .insert(session_id.to_owned(), session);
            component
                .create_response(
                    context,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content(content)
                            .ephemeral(true),
                    ),
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            return Ok(true);
        }

        let selected = match &component.data.kind {
            ComponentInteractionDataKind::StringSelect { values } if values.len() == 1 => {
                values.first().cloned()
            }
            _ => None,
        };
        let Some(selected) = selected else {
            self.config_sessions
                .lock()
                .map_err(|_| GatewayEventDispatchError)?
                .insert(session_id.to_owned(), session);
            return Ok(true);
        };
        let valid = match action {
            "lang" if selected == "__more__" => {
                let pages = session.locales.len().div_ceil(CONFIG_LANG_PAGE_SIZE).max(1);
                session.language_page = (session.language_page + 1) % pages;
                true
            }
            "lang" if session.locales.iter().any(|locale| locale == &selected) => self
                .models_for_locale(&session.models, &selected)
                .first()
                .map(|model| session.model = (*model).clone())
                .is_some(),
            "voice" if session.models.iter().any(|model| model == &selected) => {
                let selected_locale = selected.split('-').next().unwrap_or(&selected);
                selected_locale == session.model.split('-').next().unwrap_or(&session.model) && {
                    session.model = selected;
                    true
                }
            }
            "engine"
                if voice_engines_for_model(&session.model)
                    .any(|engine| engine_token(engine) == selected) =>
            {
                session.engine = selected;
                true
            }
            "speed" => selected
                .parse::<f64>()
                .ok()
                .filter(|speed| CONFIG_SPEEDS.contains(speed))
                .map(|speed| session.speed = speed)
                .is_some(),
            _ => false,
        };
        if !valid {
            self.config_sessions
                .lock()
                .map_err(|_| GatewayEventDispatchError)?
                .insert(session_id.to_owned(), session);
            return Ok(true);
        }
        session.select_compatible_engine();
        let content = self.config_summary(&session)?;
        let rows = self.config_components(session_id, &session)?;
        self.config_sessions
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .insert(session_id.to_owned(), session);
        component
            .create_response(
                context,
                CreateInteractionResponse::UpdateMessage(
                    CreateInteractionResponseMessage::new()
                        .content(content)
                        .components(rows),
                ),
            )
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        Ok(true)
    }

    async fn browse_command(
        &self,
        context: &Context,
        command: serenity::model::application::CommandInteraction,
        query: Option<String>,
        locale: Option<String>,
        engine: String,
    ) -> Result<(), GatewayEventDispatchError> {
        let requested_locale = locale.as_deref().unwrap_or_default();
        if !requested_locale.is_empty()
            && (requested_locale.len() != 2
                || !requested_locale
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase()))
        {
            let content = self.message(
                "voice.browse.invalidLocale",
                &command.locale,
                command.guild_locale.as_deref(),
                &BTreeMap::new(),
            )?;
            command
                .create_response(
                    context,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content(content)
                            .ephemeral(true),
                    ),
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            return Ok(());
        }
        let voices = browse_voices(
            &self.displays,
            &self.available_models,
            &command.locale,
            query.as_deref(),
            locale.as_deref(),
            &engine,
        );
        if voices.is_empty() {
            let content = self.message(
                "voice.browse.empty",
                &command.locale,
                command.guild_locale.as_deref(),
                &BTreeMap::new(),
            )?;
            command
                .create_response(
                    context,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content(content)
                            .ephemeral(true),
                    ),
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            return Ok(());
        }
        let user_id = command.user.id.get().to_string();
        let (favourites, recent) = {
            let store = self.store.lock().map_err(|_| GatewayEventDispatchError)?;
            let favourites = store
                .list_voice_favorites(&user_id)
                .map_err(|_| GatewayEventDispatchError)?
                .into_iter()
                .collect::<BTreeSet<_>>();
            let recent = store
                .list_recent_voices(&user_id)
                .map_err(|_| GatewayEventDispatchError)?
                .into_iter()
                .collect::<BTreeSet<_>>();
            (favourites, recent)
        };
        let page_count = browse_page_count(&voices);
        let mut parameters = BTreeMap::new();
        parameters.insert("page", "1".to_owned());
        parameters.insert("pages", page_count.to_string());
        let title = self.message(
            "voice.browse.title",
            &command.locale,
            command.guild_locale.as_deref(),
            &parameters,
        )?;
        let session_id = command.id.get().to_string();
        let session = BrowseSession {
            owner_user_id: user_id,
            locale: command.locale.clone(),
            voices,
            favourites,
            recent,
            page: 0,
            expires_at: now_seconds() + BROWSE_TTL_SECONDS,
        };
        let content = browse_content(&session, &title);
        let previous = self.message(
            "voice.browse.previous",
            &command.locale,
            command.guild_locale.as_deref(),
            &BTreeMap::new(),
        )?;
        let next = self.message(
            "voice.browse.next",
            &command.locale,
            command.guild_locale.as_deref(),
            &BTreeMap::new(),
        )?;
        let expired_content = self.message(
            "voice.browse.expired",
            &command.locale,
            command.guild_locale.as_deref(),
            &BTreeMap::new(),
        )?;
        let expiration_context = context.clone();
        let expiration_command = command.clone();
        let expiration_sessions = self.browse_sessions.clone();
        let response = CreateInteractionResponseMessage::new()
            .content(content)
            .components(vec![browse_buttons(
                &session_id,
                session.page,
                page_count,
                &previous,
                &next,
            )])
            .ephemeral(true);
        command
            .create_response(context, CreateInteractionResponse::Message(response))
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        let mut sessions = self
            .browse_sessions
            .lock()
            .map_err(|_| GatewayEventDispatchError)?;
        sessions.retain(|_, existing| existing.expires_at > now_seconds());
        while sessions.len() >= MAX_BROWSE_SESSIONS {
            let Some(oldest) = sessions.keys().next().cloned() else {
                break;
            };
            sessions.remove(&oldest);
        }
        sessions.insert(session_id, session);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(BROWSE_TTL_SECONDS as u64)).await;
            let should_expire = expiration_sessions
                .lock()
                .ok()
                .and_then(|mut sessions| sessions.remove(&expiration_command.id.get().to_string()))
                .is_some();
            if should_expire {
                let _ = expiration_command
                    .edit_response(
                        &expiration_context,
                        EditInteractionResponse::new()
                            .content(expired_content)
                            .components(Vec::new()),
                    )
                    .await;
            }
        });
        Ok(())
    }

    async fn browse_component(
        &self,
        context: &Context,
        component: ComponentInteraction,
    ) -> Result<(), GatewayEventDispatchError> {
        let Some((action, session_id)) = parse_browse_component(&component.data.custom_id) else {
            return Ok(());
        };
        let user_id = component.user.id.get().to_string();
        let mut expired = false;
        let session = {
            let mut sessions = self
                .browse_sessions
                .lock()
                .map_err(|_| GatewayEventDispatchError)?;
            match sessions.get_mut(session_id) {
                None => {
                    expired = true;
                    None
                }
                Some(session) => {
                    if session.owner_user_id != user_id {
                        return Ok(());
                    }
                    if session.expires_at <= now_seconds() {
                        sessions.remove(session_id);
                        expired = true;
                        None
                    } else {
                        let page_count = browse_page_count(&session.voices);
                        match action {
                            "prev" => session.page = session.page.saturating_sub(1),
                            "next" => {
                                session.page = (session.page + 1).min(page_count.saturating_sub(1));
                            }
                            _ => return Ok(()),
                        }
                        Some(session.clone())
                    }
                }
            }
        };
        if expired || session.is_none() {
            let content = self.message(
                "voice.browse.expired",
                &component.locale,
                component.guild_locale.as_deref(),
                &BTreeMap::new(),
            )?;
            component
                .create_response(
                    context,
                    CreateInteractionResponse::UpdateMessage(
                        CreateInteractionResponseMessage::new()
                            .content(content)
                            .components(Vec::new()),
                    ),
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            return Ok(());
        }
        let session = session.expect("checked above");
        let page_count = browse_page_count(&session.voices);
        let mut parameters = BTreeMap::new();
        parameters.insert("page", (session.page + 1).to_string());
        parameters.insert("pages", page_count.to_string());
        let title = self.message(
            "voice.browse.title",
            &session.locale,
            component.guild_locale.as_deref(),
            &parameters,
        )?;
        let content = browse_content(&session, &title);
        let previous = self.message(
            "voice.browse.previous",
            &session.locale,
            component.guild_locale.as_deref(),
            &BTreeMap::new(),
        )?;
        let next = self.message(
            "voice.browse.next",
            &session.locale,
            component.guild_locale.as_deref(),
            &BTreeMap::new(),
        )?;
        component
            .create_response(
                context,
                CreateInteractionResponse::UpdateMessage(
                    CreateInteractionResponseMessage::new()
                        .content(content)
                        .components(vec![browse_buttons(
                            session_id,
                            session.page,
                            page_count,
                            &previous,
                            &next,
                        )]),
                ),
            )
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        Ok(())
    }
}

fn is_promoted(command: &VoicePreferenceCommand) -> bool {
    matches!(
        command,
        VoicePreferenceCommand::Config
            | VoicePreferenceCommand::List
            | VoicePreferenceCommand::Browse { .. }
            | VoicePreferenceCommand::Reset
            | VoicePreferenceCommand::Set { .. }
            | VoicePreferenceCommand::Favorite { .. }
            | VoicePreferenceCommand::Unfavorite { .. }
            | VoicePreferenceCommand::Favorites
            | VoicePreferenceCommand::Recent
            | VoicePreferenceCommand::Detection { .. }
            | VoicePreferenceCommand::OptOut
            | VoicePreferenceCommand::OptIn
            | VoicePreferenceCommand::Nickname { .. }
            | VoicePreferenceCommand::Effect { .. }
    )
}

fn browse_voices(
    displays: &VoiceDisplayCatalog,
    available_models: &[String],
    interaction_locale: &str,
    query: Option<&str>,
    requested_locale: Option<&str>,
    requested_engine: &str,
) -> Vec<BrowseVoice> {
    let query = query.unwrap_or_default().trim().to_ascii_lowercase();
    let requested_locale = requested_locale
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let mut seen = BTreeSet::new();
    let mut voices = available_models
        .iter()
        .filter(|model| seen.insert((*model).clone()))
        .map(|model| {
            let label = displays.voice_name(Some(interaction_locale), available_models, model);
            let engine = if model.contains("-google-") {
                "google"
            } else {
                "local"
            };
            BrowseVoice {
                id: model.clone(),
                label,
                engine: engine.to_owned(),
            }
        })
        .filter(|voice| {
            (requested_engine == "all" || requested_engine == voice.engine)
                && (requested_locale.is_empty()
                    || voice
                        .id
                        .to_ascii_lowercase()
                        .starts_with(&format!("{requested_locale}_")))
                && (query.is_empty()
                    || voice.label.to_ascii_lowercase().contains(&query)
                    || voice.id.to_ascii_lowercase().contains(&query))
        })
        .collect::<Vec<_>>();
    voices.sort_by(|left, right| left.label.cmp(&right.label));
    voices
}

fn browse_page_count(voices: &[BrowseVoice]) -> usize {
    voices.len().div_ceil(BROWSE_PAGE_SIZE).max(1)
}

fn browse_content(session: &BrowseSession, title: &str) -> String {
    let page_count = browse_page_count(&session.voices);
    let page = session.page.min(page_count.saturating_sub(1));
    let start = page * BROWSE_PAGE_SIZE;
    let lines = session.voices[start..(start + BROWSE_PAGE_SIZE).min(session.voices.len())]
        .iter()
        .map(|voice| {
            let mut badges = String::new();
            if session.favourites.contains(&voice.id) {
                badges.push_str(" ⭐");
            }
            if session.recent.contains(&voice.id) {
                badges.push_str(" 🕘");
            }
            format!("• **{}** — {}{}", voice.label, voice.engine, badges)
        })
        .collect::<Vec<_>>();
    format!("{title}\n{}", lines.join("\n"))
}

fn browse_buttons(
    session_id: &str,
    page: usize,
    page_count: usize,
    previous: &str,
    next: &str,
) -> CreateActionRow {
    CreateActionRow::Buttons(vec![
        CreateButton::new(format!("vbr:prev:{session_id}"))
            .label(previous)
            .style(ButtonStyle::Secondary)
            .disabled(page == 0),
        CreateButton::new(format!("vbr:next:{session_id}"))
            .label(next)
            .style(ButtonStyle::Secondary)
            .disabled(page >= page_count.saturating_sub(1)),
    ])
}

fn parse_browse_component(custom_id: &str) -> Option<(&str, &str)> {
    let remainder = custom_id.strip_prefix("vbr:")?;
    remainder.split_once(':')
}

fn parse_config_component(custom_id: &str) -> Option<(&str, &str)> {
    let remainder = custom_id.strip_prefix("vcfg:")?;
    let (action, session_id) = remainder.split_once(':')?;
    if session_id.is_empty() || !session_id.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    matches!(
        action,
        "lang" | "voice" | "engine" | "speed" | "save" | "cancel"
    )
    .then_some((action, session_id))
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

fn format_voice_list(
    displays: &VoiceDisplayCatalog,
    available_models: &[String],
    interaction_locale: &str,
) -> String {
    let mut groups = BTreeMap::<String, Vec<String>>::new();
    for model in available_models {
        let locale = model.split('-').next().unwrap_or(model).to_owned();
        groups.entry(locale).or_default().push(model.clone());
    }

    let mut rendered_groups = groups
        .into_iter()
        .map(|(locale, models)| {
            let header = displays.language_name(
                Some(interaction_locale),
                available_models,
                models.first().map(String::as_str).unwrap_or(&locale),
            );
            (header, models)
        })
        .collect::<Vec<_>>();
    rendered_groups.sort_by(|left, right| left.0.cmp(&right.0));

    rendered_groups
        .into_iter()
        .flat_map(|(header, mut models)| {
            models.sort_by(|left, right| {
                VoiceDisplayCatalog::voice_label(left).cmp(&VoiceDisplayCatalog::voice_label(right))
            });
            std::iter::once(header).chain(
                models.into_iter().map(|model| {
                    format!("• {} ({model})", VoiceDisplayCatalog::voice_label(&model))
                }),
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn engine_label(engine: UserEngine) -> &'static str {
    match engine {
        // Keep the confirmation text user-facing. The stored enum value is an
        // implementation token, not a label Discord users should see.
        UserEngine::Google => "Google (gTTS)",
        UserEngine::Piper => "Piper",
        UserEngine::Kokoro => "Kokoro",
        UserEngine::Gcloud => "Google HD",
    }
}

pub(crate) fn voice_engines_for_model(model: &str) -> impl Iterator<Item = UserEngine> {
    [UserEngine::Google, UserEngine::Piper, UserEngine::Kokoro]
        .into_iter()
        .filter(move |&engine| voice_engine_supports_model(engine, model))
}

pub(crate) fn engine_token(engine: UserEngine) -> &'static str {
    match engine {
        UserEngine::Google => "google",
        UserEngine::Piper => "piper",
        UserEngine::Kokoro => "kokoro",
        UserEngine::Gcloud => "google",
    }
}

fn config_engine_label(engine: &str) -> &'static str {
    match engine {
        "piper" => "Piper",
        "kokoro" => "Kokoro",
        "gcloud" => "Google HD",
        _ => "Google (gTTS)",
    }
}

fn effect_label(effect: VoiceEffect) -> &'static str {
    match effect {
        VoiceEffect::None => "None (normal)",
        VoiceEffect::Robot => "🤖 Robot",
        VoiceEffect::Echo => "🔊 Echo",
        VoiceEffect::Deep => "🕳️ Deep",
        VoiceEffect::Chipmunk => "🐿️ Chipmunk",
        VoiceEffect::Radio => "📻 Radio",
        VoiceEffect::Phone => "📞 Phone",
        VoiceEffect::Underwater => "🌊 Underwater",
        VoiceEffect::Demon => "😈 Demon",
    }
}

#[async_trait::async_trait]
impl GatewayEventSink for VoicePreferenceGatewaySink {
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
        if let Interaction::Component(component) = interaction {
            if self.config_component(&context, component.clone()).await? {
                return Ok(());
            }
            return self.browse_component(&context, component).await;
        }
        let Interaction::Command(command) = interaction else {
            return Ok(());
        };
        let Some(parsed) =
            parse_voice_preference_command(&command.data).map_err(|_| GatewayEventDispatchError)?
        else {
            return Ok(());
        };
        if !is_promoted(&parsed) {
            return Ok(());
        }
        if matches!(parsed, VoicePreferenceCommand::Config) {
            return self.config_command(&context, command).await;
        }
        if let VoicePreferenceCommand::Browse {
            query,
            locale,
            engine,
        } = &parsed
        {
            return self
                .browse_command(
                    &context,
                    command,
                    query.clone(),
                    locale.clone(),
                    engine.clone(),
                )
                .await;
        }
        command
            .defer_ephemeral(&context)
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        if matches!(parsed, VoicePreferenceCommand::List) {
            let header = self.message(
                "voice.listHeader",
                &command.locale,
                command.guild_locale.as_deref(),
                &BTreeMap::new(),
            )?;
            let body = if self.available_models.is_empty() {
                self.message(
                    "voice.listEmpty",
                    &command.locale,
                    command.guild_locale.as_deref(),
                    &BTreeMap::new(),
                )?
            } else {
                format_voice_list(&self.displays, &self.available_models, &command.locale)
            };
            command
                .edit_response(
                    &context,
                    EditInteractionResponse::new()
                        .embeds(vec![
                            CreateEmbed::new().description(format!("{header}\n{body}")),
                        ])
                        .allowed_mentions(
                            CreateAllowedMentions::new()
                                .all_users(false)
                                .all_roles(false)
                                .everyone(false),
                        ),
                )
                .await
                .map_err(|_| GatewayEventDispatchError)?;
            return Ok(());
        }
        let guild_id = command.guild_id.map(|id| id.get().to_string());
        let user_id = command.user.id.get().to_string();
        let guild_locale = command.guild_locale.as_deref();
        let outcome = self.service.execute(
            VoicePreferenceInvocation {
                guild_id: guild_id.as_deref(),
                user_id: &user_id,
                now_ms: system_now_ms(),
            },
            parsed,
        );
        let content = match outcome {
            VoicePreferenceOutcome::FavoriteSaved { model } => format!(
                "Added **{}** to your favourites.",
                self.displays
                    .voice_name(Some(&command.locale), &self.available_models, &model)
            ),
            VoicePreferenceOutcome::FavoriteLimit => {
                "Your favourites are full. Remove one before adding another.".to_owned()
            }
            VoicePreferenceOutcome::FavoriteRemoved { .. } => {
                "Voice removed from your favourites.".to_owned()
            }
            VoicePreferenceOutcome::FavoriteNotSaved { .. } => {
                "That voice was not saved.".to_owned()
            }
            VoicePreferenceOutcome::VoiceLibrary { favorites, models } => {
                let title = if favorites {
                    "Favourite voices"
                } else {
                    "Recent voices"
                };
                if models.is_empty() {
                    format!("**{title}**\nNo available voices yet.")
                } else {
                    let lines = models
                        .iter()
                        .map(|model| {
                            format!(
                                "• {} — `{model}`",
                                self.displays.voice_name(
                                    Some(&command.locale),
                                    &self.available_models,
                                    model,
                                )
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    format!("**{title}**\n{lines}")
                }
            }
            outcome => self.localized_outcome(outcome, &command.locale, guild_locale)?,
        };
        command
            .edit_response(
                &context,
                EditInteractionResponse::new()
                    .embeds(vec![message_embed(content)])
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

impl VoicePreferenceGatewaySink {
    fn localized_outcome(
        &self,
        outcome: VoicePreferenceOutcome,
        interaction_locale: &str,
        guild_locale: Option<&str>,
    ) -> Result<String, GatewayEventDispatchError> {
        let mut parameters = BTreeMap::new();
        let key = match outcome {
            VoicePreferenceOutcome::SavedVoice {
                model,
                speed,
                engine,
            } => {
                parameters.insert(
                    "name",
                    self.displays.voice_name(
                        Some(interaction_locale),
                        &self.available_models,
                        &model,
                    ),
                );
                parameters.insert("model", model);
                parameters.insert("speed", speed.to_string());
                parameters.insert("engine", engine_label(engine).to_owned());
                "voice.set"
            }
            VoicePreferenceOutcome::Reset => "voice.reset",
            VoicePreferenceOutcome::Detection { enabled: true } => "voice.detection.on",
            VoicePreferenceOutcome::Detection { enabled: false } => "voice.detection.off",
            VoicePreferenceOutcome::OptedOut => "voice.optout",
            VoicePreferenceOutcome::OptedIn => "voice.optin",
            VoicePreferenceOutcome::NicknameSet { nickname } => {
                parameters.insert("name", nickname);
                "voice.nickname.set"
            }
            VoicePreferenceOutcome::NicknameCleared => "voice.nickname.cleared",
            VoicePreferenceOutcome::InvalidNickname => "voice.nickname.invalid",
            VoicePreferenceOutcome::EffectSet { effect } => {
                parameters.insert("effect", effect_label(effect).to_owned());
                "voice.effect.set"
            }
            VoicePreferenceOutcome::EffectCleared => "voice.effect.cleared",
            VoicePreferenceOutcome::PremiumEffectLocked { effect } => {
                parameters.insert("effect", effect_label(effect).to_owned());
                "voice.effect.locked"
            }
            VoicePreferenceOutcome::UnknownModel => "voice.unknownModel",
            VoicePreferenceOutcome::InvalidSpeed => "voice.badSpeed",
            VoicePreferenceOutcome::PremiumEngineLocked {
                engine: UserEngine::Kokoro,
            } => "voice.engine.kokoroLocked",
            VoicePreferenceOutcome::PremiumEngineLocked {
                engine: UserEngine::Gcloud,
            } => "voice.engine.gcloudLocked",
            // Any other condition is a malformed command or an unavailable dependency. Keep
            // the response generic rather than leaking storage/provider detail.
            VoicePreferenceOutcome::InvalidEngine
            | VoicePreferenceOutcome::InvalidEffect
            | VoicePreferenceOutcome::PremiumEngineLocked { .. }
            | VoicePreferenceOutcome::GuildRequired
            | VoicePreferenceOutcome::StoreUnavailable => "error.generic",
            VoicePreferenceOutcome::FavoriteSaved { .. }
            | VoicePreferenceOutcome::FavoriteLimit
            | VoicePreferenceOutcome::FavoriteRemoved { .. }
            | VoicePreferenceOutcome::FavoriteNotSaved { .. }
            | VoicePreferenceOutcome::VoiceLibrary { .. } => {
                unreachable!("voice-library outcomes render before localization")
            }
        };
        self.message(key, interaction_locale, guild_locale, &parameters)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_textual_preference_leaves_can_be_claimed() {
        assert!(is_promoted(&VoicePreferenceCommand::Config));
        assert!(is_promoted(&VoicePreferenceCommand::List));
        assert!(is_promoted(&VoicePreferenceCommand::Browse {
            query: None,
            locale: None,
            engine: "all".into(),
        }));
        assert!(is_promoted(&VoicePreferenceCommand::Reset));
        assert!(is_promoted(&VoicePreferenceCommand::Set {
            model: "en_US-amy-medium".into(),
            speed: None,
            engine: None,
        }));
        assert!(is_promoted(&VoicePreferenceCommand::Favorite {
            model: "en_US-amy-medium".into(),
        }));
        assert!(is_promoted(&VoicePreferenceCommand::Recent));
        assert!(is_promoted(&VoicePreferenceCommand::Effect {
            effect: "robot".into()
        }));
    }

    #[test]
    fn formats_voice_list_by_localized_language_and_voice_name() {
        let displays = VoiceDisplayCatalog::from_generated_contract().expect("catalog");
        let models = vec![
            "en_US-amy-medium".to_owned(),
            "pt_PT-tugao-medium".to_owned(),
            "en_US-lessac-medium".to_owned(),
        ];
        let rendered = format_voice_list(&displays, &models, "en");
        assert!(rendered.starts_with("English"));
        assert!(rendered.contains("• Amy (en_US-amy-medium)"));
        assert!(rendered.contains("• Lessac (en_US-lessac-medium)"));
        assert!(rendered.contains("Portuguese"));
    }

    #[test]
    fn browse_filters_by_query_locale_engine_and_deduplicates_models() {
        let displays = VoiceDisplayCatalog::from_generated_contract().expect("catalog");
        let models = vec![
            "en_US-amy-medium".to_owned(),
            "en_US-amy-medium".to_owned(),
            "pt_PT-google-medium".to_owned(),
        ];
        let local = browse_voices(&displays, &models, "en", Some("amy"), Some("en"), "local");
        assert_eq!(
            local,
            vec![BrowseVoice {
                id: "en_US-amy-medium".into(),
                label: "English — Amy".into(),
                engine: "local".into(),
            }]
        );
        assert!(
            browse_voices(&displays, &models, "en", None, None, "google")
                .iter()
                .all(|voice| voice.engine == "google")
        );
    }

    #[test]
    fn browse_session_controls_are_bound_to_the_expected_id() {
        assert_eq!(
            parse_browse_component("vbr:next:123"),
            Some(("next", "123"))
        );
        assert_eq!(
            parse_browse_component("vbr:delete:123"),
            Some(("delete", "123"))
        );
        assert_eq!(parse_browse_component("vbr:next"), None);
        assert_eq!(parse_browse_component("other:next:123"), None);
    }

    #[test]
    fn config_session_controls_are_strict_and_action_bound() {
        assert_eq!(
            parse_config_component("vcfg:lang:123"),
            Some(("lang", "123"))
        );
        assert_eq!(
            parse_config_component("vcfg:save:987654"),
            Some(("save", "987654"))
        );
        assert_eq!(parse_config_component("vcfg:delete:123"), None);
        assert_eq!(parse_config_component("vcfg:lang:123:extra"), None);
        assert_eq!(parse_config_component("vcfg:lang:abc"), None);
        assert_eq!(parse_config_component("other:lang:123"), None);
    }

    #[test]
    fn voice_config_updates_engine_choices_when_the_language_changes() {
        let english = "en_US-amy-medium";
        let german = "de_DE-thorsten-medium";
        let models = vec![english.to_owned(), german.to_owned()];
        let store = Arc::new(Mutex::new(SqliteStore::open_in_memory().expect("store")));
        let sink = VoicePreferenceGatewaySink::new(
            store,
            VoicePreferenceSettings {
                available_models: models.clone(),
                default_speed: 1.0,
            },
        )
        .expect("voice configuration sink");
        let mut session = VoiceConfigSession {
            owner_user_id: "user".into(),
            guild_id: "guild".into(),
            channel_id: "channel".into(),
            locale: "en".into(),
            guild_locale: None,
            models,
            locales: sink.config_locales(),
            model: english.into(),
            engine: "kokoro".into(),
            speed: 1.0,
            language_page: 0,
            issued_at_ms: 0,
        };
        let engine_options = |session: &VoiceConfigSession| {
            let rows = serde_json::to_value(sink.config_components("123", session).expect("rows"))
                .expect("serialized rows");
            rows.as_array()
                .expect("action rows")
                .iter()
                .map(|row| &row["components"][0])
                .find(|component| component["custom_id"] == "vcfg:engine:123")
                .expect("engine menu")["options"]
                .as_array()
                .expect("engine options")
                .clone()
        };

        assert!(
            engine_options(&session)
                .iter()
                .any(|option| { option["value"] == "kokoro" && option["default"] == true })
        );
        session.model = german.into();
        session.select_compatible_engine();
        let options = engine_options(&session);
        assert_eq!(options.len(), 2);
        assert_eq!(options[0]["value"], "google");
        assert_eq!(options[0]["default"], true);
        assert_eq!(options[1]["value"], "piper");
    }

    #[test]
    fn preserves_the_node_engine_tokens_in_a_voice_set_response() {
        assert_eq!(engine_label(UserEngine::Google), "Google (gTTS)");
        assert_eq!(engine_label(UserEngine::Piper), "Piper");
        assert_eq!(engine_label(UserEngine::Kokoro), "Kokoro");
        assert_eq!(engine_label(UserEngine::Gcloud), "Google HD");
    }
}
