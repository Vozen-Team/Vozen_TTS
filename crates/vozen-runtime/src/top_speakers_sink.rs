//! Opt-in gateway adapter for public `/top-speakers`.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use crate::{streak_style::style_streak_message, ui::message_embed};
use serenity::{
    builder::{CreateAllowedMentions, CreateInteractionResponse, CreateInteractionResponseMessage},
    client::Context,
    model::application::Interaction,
};
use vozen_discord::{
    GatewayEventDispatchError, GatewayEventSink, VoiceResponseLocalizer, parse_top_speakers_command,
};
use vozen_store::{SqliteStore, utc_day_key};

pub struct TopSpeakersGatewaySink {
    store: Arc<Mutex<SqliteStore>>,
    localizer: VoiceResponseLocalizer,
}

impl TopSpeakersGatewaySink {
    pub fn new(store: Arc<Mutex<SqliteStore>>) -> Result<Self, GatewayEventDispatchError> {
        Ok(Self {
            store,
            localizer: VoiceResponseLocalizer::from_generated_contract()
                .map_err(|_| GatewayEventDispatchError)?,
        })
    }

    fn message(
        &self,
        key: &str,
        command: &serenity::model::application::CommandInteraction,
        parameters: &BTreeMap<&str, String>,
    ) -> Result<String, GatewayEventDispatchError> {
        self.localizer
            .render_key(
                key,
                None,
                Some(&crate::ui::guild_locale(&self.store, command.guild_id)?),
                parameters,
            )
            .ok_or(GatewayEventDispatchError)
    }

    fn response(
        &self,
        command: &serenity::model::application::CommandInteraction,
    ) -> Result<String, GatewayEventDispatchError> {
        let Some(guild_id) = command.guild_id else {
            return self.message("topspeakers.empty", command, &BTreeMap::new());
        };
        let rows = self
            .store
            .lock()
            .map_err(|_| GatewayEventDispatchError)?
            .top_speakers(&guild_id.get().to_string(), &utc_day_key(), 10)
            .map_err(|_| GatewayEventDispatchError)?;
        if rows.is_empty() {
            return self.message("topspeakers.empty", command, &BTreeMap::new());
        }
        let mut lines = Vec::with_capacity(rows.len() + 1);
        lines.push(self.message("topspeakers.title", command, &BTreeMap::new())?);
        for (index, row) in rows.into_iter().enumerate() {
            let mut parameters = BTreeMap::new();
            parameters.insert("rank", (index + 1).to_string());
            let streak = row.streak;
            parameters.insert("user", row.user_id);
            parameters.insert("count", row.count.to_string());
            parameters.insert("streak", streak.to_string());
            let line = self.message("topspeakers.line", command, &parameters)?;
            lines.push(style_streak_message(line, streak));
        }
        Ok(lines.join("\n"))
    }
}

#[async_trait::async_trait]
impl GatewayEventSink for TopSpeakersGatewaySink {
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
        let Interaction::Command(command) = interaction else {
            return Ok(());
        };
        if parse_top_speakers_command(&command.data)
            .map_err(|_| GatewayEventDispatchError)?
            .is_none()
        {
            return Ok(());
        }
        let response = CreateInteractionResponseMessage::new()
            .embeds(vec![message_embed(self.response(&command)?)])
            .allowed_mentions(
                CreateAllowedMentions::new()
                    .all_users(false)
                    .all_roles(false)
                    .everyone(false),
            );
        command
            .create_response(&context, CreateInteractionResponse::Message(response))
            .await
            .map_err(|_| GatewayEventDispatchError)?;
        Ok(())
    }

    async fn on_guild_delete(&self, _guild_id: &str) -> Result<(), GatewayEventDispatchError> {
        Ok(())
    }
}
