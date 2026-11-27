//! Rust-owned Discord autocomplete during the final gateway cutover.
//!
//! Autocomplete is an interaction in its own right. Leaving it in Node while Rust owns the
//! corresponding command leaves the full cutover with a silent UX regression (and can race two
//! responses in hybrid mode), so this adapter mirrors the small, synchronous Node catalogue
//! filters and only claims options whose command canary is active.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use serenity::{
    builder::{AutocompleteChoice, CreateAutocompleteResponse, CreateInteractionResponse},
    client::Context,
    model::application::{
        CommandDataOption, CommandDataOptionValue, CommandInteraction, Interaction,
    },
};
use vozen_discord::{
    GAME_CATALOG, JOKE_LANGUAGES, VoiceDisplayCatalog, VoiceResponseLocalizer,
    locale_display_options,
};
use vozen_store::{SqliteStore, UserEngine};

use crate::voice_preference_sink::{engine_token, voice_engines_for_model};

use vozen_discord::{GatewayEventDispatchError, GatewayEventSink};

const WORD_CHAIN_LANGUAGES: [(&str, &str); 4] = [
    ("pt", "Português"),
    ("en", "English"),
    ("es", "Español"),
    ("fr", "Français"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct Choice {
    name: String,
    value: String,
}

#[derive(Clone)]
pub(crate) struct AutocompleteRuntimeOptions {
    pub(crate) available_models: Vec<String>,
    pub(crate) core_voice: bool,
    pub(crate) game_play: bool,
    pub(crate) transcription_live: bool,
    pub(crate) voice_preferences: bool,
    pub(crate) config_default_voice: bool,
    pub(crate) config_language: bool,
    pub(crate) translation_preferences: bool,
    pub(crate) pronunciation: bool,
}

pub(crate) struct AutocompleteGatewaySink {
    store: Arc<Mutex<SqliteStore>>,
    options: AutocompleteRuntimeOptions,
    displays: VoiceDisplayCatalog,
    localizer: VoiceResponseLocalizer,
}

impl AutocompleteGatewaySink {
    pub(crate) fn new(
        store: Arc<Mutex<SqliteStore>>,
        options: AutocompleteRuntimeOptions,
    ) -> Result<Self, GatewayEventDispatchError> {
        Ok(Self {
            store,
            options,
            displays: VoiceDisplayCatalog::from_generated_contract()
                .map_err(|_| GatewayEventDispatchError)?,
            localizer: VoiceResponseLocalizer::from_generated_contract()
                .map_err(|_| GatewayEventDispatchError)?,
        })
    }

    fn owns(&self, command: &CommandInteraction, focused: &str) -> bool {
        let subcommand = subcommand_name(&command.data.options);
        match (command.data.name.as_str(), focused) {
            ("voice", "model") => {
                (subcommand == Some("preview") && self.options.core_voice)
                    || (matches!(subcommand, Some("set" | "favorite" | "unfavorite"))
                        && self.options.voice_preferences)
            }
            ("voice", "engine") => subcommand == Some("set") && self.options.voice_preferences,
            ("config", "model") => {
                subcommand == Some("default-voice") && self.options.config_default_voice
            }
            ("game", "game") => subcommand == Some("play") && self.options.game_play,
            ("game", "language") => subcommand == Some("play") && self.options.game_play,
            (
                "joke" | "rizz" | "8-ball" | "fortune" | "fact" | "wyr" | "randomizer",
                "language",
            ) => self.options.core_voice,
            ("transcribe", "language") => {
                subcommand == Some("start") && self.options.transcription_live
            }
            ("config", "locale") => subcommand == Some("language") && self.options.config_language,
            ("translate", "locale") => {
                subcommand == Some("speak-language") && self.options.translation_preferences
            }
            ("pronunciation" | "server-pronunciation", "term") => self.options.pronunciation,
            _ => false,
        }
    }

    fn choices(&self, command: &CommandInteraction, focused: &str, query: &str) -> Vec<Choice> {
        let locale = command.locale.as_str();
        match (command.data.name.as_str(), focused) {
            ("voice" | "config", "model") => filter_models(
                &self.options.available_models,
                query,
                locale,
                &self.displays,
            ),
            ("voice", "engine") => {
                filter_voice_engines(&self.options.available_models, &command.data.options, query)
            }
            ("game", "game") => filter_games(query, locale, &self.localizer),
            ("game", "language") => filter_word_chain_languages(query),
            (
                "joke" | "rizz" | "8-ball" | "fortune" | "fact" | "wyr" | "randomizer"
                | "transcribe",
                "language",
            ) => filter_joke_languages(query),
            ("config" | "translate", "locale") => filter_locales(query),
            ("pronunciation" | "server-pronunciation", "term") => {
                let entries = self.store.lock().ok().and_then(|store| {
                    if command.data.name == "pronunciation" {
                        store
                            .get_user_pronunciations(&command.user.id.get().to_string())
                            .ok()
                    } else {
                        command.guild_id.and_then(|guild| {
                            store
                                .get_server_pronunciations(&guild.get().to_string())
                                .ok()
                        })
                    }
                });
                entries.map_or_else(Vec::new, |entries| {
                    filter_pronunciations(
                        entries
                            .into_iter()
                            .map(|entry| (entry.term, entry.replacement))
                            .collect(),
                        query,
                    )
                })
            }
            _ => Vec::new(),
        }
    }
}

#[async_trait::async_trait]
impl GatewayEventSink for AutocompleteGatewaySink {
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
        let Interaction::Autocomplete(command) = interaction else {
            return Ok(());
        };
        let Some(focused) = command.data.autocomplete() else {
            return Ok(());
        };
        if !self.owns(&command, focused.name) {
            return Ok(());
        }
        let choices = self.choices(&command, focused.name, focused.value);
        let response = CreateAutocompleteResponse::new().set_choices(
            choices
                .into_iter()
                .map(|choice| AutocompleteChoice::new(choice.name, choice.value))
                .collect(),
        );
        command
            .create_response(&context, CreateInteractionResponse::Autocomplete(response))
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        Ok(())
    }

    async fn on_guild_delete(&self, _guild_id: &str) -> Result<(), GatewayEventDispatchError> {
        Ok(())
    }
}

fn filter_voice_engines(
    models: &[String],
    options: &[CommandDataOption],
    query: &str,
) -> Vec<Choice> {
    let Some(model) = options.iter().find_map(|option| match &option.value {
        CommandDataOptionValue::SubCommand(children) if option.name == "set" => {
            children.iter().find_map(|child| match &child.value {
                CommandDataOptionValue::String(model) if child.name == "model" => Some(model),
                _ => None,
            })
        }
        _ => None,
    }) else {
        return Vec::new();
    };
    if !models.contains(model) {
        return Vec::new();
    }
    let query = query.trim().to_lowercase();
    voice_engines_for_model(model)
        .map(|engine| Choice {
            name: match engine {
                UserEngine::Google | UserEngine::Gcloud => "Default (Google TTS)",
                UserEngine::Piper => "Piper",
                UserEngine::Kokoro => "💎 Kokoro (Premium neural)",
            }
            .to_owned(),
            value: engine_token(engine).to_owned(),
        })
        .filter(|choice| {
            choice.name.to_lowercase().contains(&query) || choice.value.contains(&query)
        })
        .collect()
}

fn filter_models(
    models: &[String],
    query: &str,
    locale: &str,
    displays: &VoiceDisplayCatalog,
) -> Vec<Choice> {
    let query = query.trim().to_lowercase();
    let mut choices = models
        .iter()
        .map(|model| Choice {
            name: displays.language_name(Some(locale), models, model),
            value: model.clone(),
        })
        .filter(|choice| {
            choice.name.to_lowercase().contains(&query)
                || choice.value.to_lowercase().contains(&query)
                || displays
                    .language_name(None, models, &choice.value)
                    .to_lowercase()
                    .contains(&query)
        })
        .collect::<Vec<_>>();
    choices.sort_by(|left, right| left.name.cmp(&right.name));
    choices.truncate(25);
    sanitize(choices)
}

fn filter_joke_languages(query: &str) -> Vec<Choice> {
    let query = query.trim().to_lowercase();
    sanitize(
        JOKE_LANGUAGES
            .iter()
            .filter(|language| language.display.to_lowercase().contains(&query))
            .take(25)
            .map(|language| Choice {
                name: language.display.to_owned(),
                value: language.key.to_owned(),
            })
            .collect(),
    )
}

fn filter_word_chain_languages(query: &str) -> Vec<Choice> {
    let query = query.trim().to_lowercase();
    sanitize(
        WORD_CHAIN_LANGUAGES
            .into_iter()
            .filter(|(value, name)| {
                query.is_empty()
                    || value.to_lowercase().contains(&query)
                    || name.to_lowercase().contains(&query)
            })
            .take(25)
            .map(|(value, name)| Choice {
                name: name.to_owned(),
                value: value.to_owned(),
            })
            .collect(),
    )
}

fn filter_locales(query: &str) -> Vec<Choice> {
    let query = query.trim().to_lowercase();
    sanitize(
        locale_display_options()
            .into_iter()
            .filter(|locale| {
                locale.id.to_lowercase().contains(&query)
                    || locale.label.to_lowercase().contains(&query)
            })
            .take(25)
            .map(|locale| Choice {
                name: locale.label,
                value: locale.id,
            })
            .collect(),
    )
}

fn filter_games(query: &str, locale: &str, localizer: &VoiceResponseLocalizer) -> Vec<Choice> {
    let query = query.trim().to_lowercase();
    sanitize(
        GAME_CATALOG
            .iter()
            .filter_map(|game| {
                let name =
                    localizer.render_key(game.name_key, Some(locale), None, &BTreeMap::new())?;
                (name.to_lowercase().contains(&query) || game.id.to_lowercase().contains(&query))
                    .then_some(Choice {
                        name,
                        value: game.id.to_owned(),
                    })
            })
            .take(25)
            .collect(),
    )
}

fn filter_pronunciations(entries: Vec<(String, String)>, query: &str) -> Vec<Choice> {
    let query = query.trim().to_lowercase();
    sanitize(
        entries
            .into_iter()
            .filter(|(term, replacement)| {
                query.is_empty()
                    || term.to_lowercase().contains(&query)
                    || replacement.to_lowercase().contains(&query)
            })
            .take(25)
            .map(|(term, replacement)| Choice {
                name: format!("{term} → {replacement}"),
                value: term,
            })
            .collect(),
    )
}

fn sanitize(mut choices: Vec<Choice>) -> Vec<Choice> {
    choices.truncate(25);
    choices
        .into_iter()
        .map(|mut choice| {
            if choice.name.trim().is_empty() {
                choice.name = "—".to_owned();
            }
            choice.name = choice.name.trim().chars().take(100).collect();
            choice.value = choice.value.chars().take(100).collect();
            choice
        })
        .collect()
}

fn subcommand_name(options: &[CommandDataOption]) -> Option<&str> {
    options.iter().find_map(|option| match &option.value {
        CommandDataOptionValue::SubCommand(_) => Some(option.name.as_str()),
        CommandDataOptionValue::SubCommandGroup(children) => subcommand_name(children),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_set_engine_suggestions_follow_the_selected_model() {
        let models = [
            "de_DE-thorsten-medium",
            "en_US-amy-medium",
            "uk_UA-google-medium",
        ]
        .map(str::to_owned);
        let suggestions = |model: Option<&str>, query: &str| {
            let mut options = vec![serde_json::json!({
                "name": "engine", "type": 3, "value": query, "focused": true
            })];
            if let Some(model) = model {
                options.push(serde_json::json!({"name": "model", "type": 3, "value": model}));
            }
            let options: Vec<CommandDataOption> = serde_json::from_value(serde_json::json!([
                {"name": "set", "type": 1, "options": options}
            ]))
            .expect("Discord autocomplete options");
            filter_voice_engines(&models, &options, query)
                .into_iter()
                .map(|choice| choice.value)
                .collect::<Vec<_>>()
        };

        assert_eq!(suggestions(Some(&models[0]), ""), ["google", "piper"]);
        assert_eq!(
            suggestions(Some(&models[1]), ""),
            ["google", "piper", "kokoro"]
        );
        assert_eq!(suggestions(Some(&models[1]), "premium"), ["kokoro"]);
        assert_eq!(suggestions(Some(&models[2]), ""), ["google"]);
        assert!(suggestions(Some(&models[0]), "kokoro").is_empty());
        assert!(suggestions(Some("de_DE"), "").is_empty());
        assert!(suggestions(None, "").is_empty());
    }

    #[test]
    fn filters_are_bounded_and_match_node_routing() {
        let displays = VoiceDisplayCatalog::from_generated_contract().expect("display contract");
        let models = vec!["en_US-amy-medium".into(), "pt_PT-tugao-medium".into()];
        assert_eq!(
            filter_models(&models, "portu", "pt-BR", &displays)[0].value,
            "pt_PT-tugao-medium"
        );
        assert_eq!(filter_joke_languages("russ")[0].value, "ru");
        assert_eq!(filter_word_chain_languages("fr")[0].value, "fr");
        assert_eq!(filter_word_chain_languages("pt")[0].value, "pt");
        assert_eq!(filter_locales("deuts")[0].value, "de");
        assert_eq!(
            filter_pronunciations(vec![("sql".into(), "sequel".into())], "sequel")[0].value,
            "sql"
        );
        assert!(
            filter_games(
                "not-a-game",
                "en",
                &VoiceResponseLocalizer::from_generated_contract().expect("i18n")
            )
            .is_empty()
        );
    }
}
