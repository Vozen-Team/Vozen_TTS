//! Opt-in gateway adapter for aggregated `/server-stats`.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{streak_style::style_streak_message, ui::message_embed};
use serenity::{
    builder::{CreateAllowedMentions, CreateInteractionResponse, CreateInteractionResponseMessage},
    client::Context,
    model::application::Interaction,
};
use vozen_discord::{
    GatewayEventDispatchError, GatewayEventSink, VoiceResponseLocalizer, parse_server_stats_command,
};
use vozen_store::{SqliteStore, VOTE_REDEMPTION_SECRET_MIN_LENGTH, utc_day_key};

pub struct ServerStatsGatewaySink {
    store: Arc<Mutex<SqliteStore>>,
    client_id: Option<String>,
    redemption_secret: Option<String>,
    localizer: VoiceResponseLocalizer,
}

impl ServerStatsGatewaySink {
    pub fn new(
        store: Arc<Mutex<SqliteStore>>,
        client_id: Option<String>,
        redemption_secret: Option<String>,
    ) -> Result<Self, GatewayEventDispatchError> {
        Ok(Self {
            store,
            client_id,
            redemption_secret,
            localizer: VoiceResponseLocalizer::from_generated_contract()
                .map_err(|_| GatewayEventDispatchError)?,
        })
    }

    fn message(
        &self,
        key: &str,
        locale: &str,
        parameters: &BTreeMap<&str, String>,
    ) -> Result<String, GatewayEventDispatchError> {
        self.localizer
            .render_key(key, None, Some(locale), parameters)
            .ok_or(GatewayEventDispatchError)
    }

    fn response(
        &self,
        command: &serenity::model::application::CommandInteraction,
    ) -> Result<String, GatewayEventDispatchError> {
        let locale = crate::ui::guild_locale(&self.store, command.guild_id)?;
        let Some(guild_id) = command.guild_id else {
            return self.message("serverstats.empty", &locale, &BTreeMap::new());
        };
        let guild_id = guild_id.get().to_string();
        let user_id = command.user.id.get().to_string();
        let now_ms = now_ms();
        let store = self.store.lock().map_err(|_| GatewayEventDispatchError)?;
        let premium = store
            .is_guild_premium(&guild_id, now_ms)
            .map_err(|_| GatewayEventDispatchError)?
            || store
                .is_user_premium(&user_id, now_ms)
                .map_err(|_| GatewayEventDispatchError)?;
        let limit = if premium { 5 } else { 3 };
        let stats = store
            .admin_guild_stats(&guild_id, &utc_day_key(), limit)
            .map_err(|_| GatewayEventDispatchError)?;
        let games = store
            .guild_game_stats(&guild_id, if premium { 5 } else { 0 })
            .map_err(|_| GatewayEventDispatchError)?;
        if stats.messages == 0 && games.players == 0 {
            return self.message("serverstats.empty", &locale, &BTreeMap::new());
        }

        let mut lines = vec![self.message("serverstats.title", &locale, &BTreeMap::new())?];
        let mut parameters = BTreeMap::new();
        parameters.insert("total", stats.messages.to_string());
        parameters.insert("speakers", stats.speakers.to_string());
        lines.push(self.message("serverstats.messages", &locale, &parameters)?);
        if !stats.top_speakers.is_empty() {
            lines.push(self.message("serverstats.topTalkers", &locale, &BTreeMap::new())?);
            for (index, row) in stats.top_speakers.into_iter().enumerate() {
                let mut parameters = BTreeMap::new();
                parameters.insert("rank", (index + 1).to_string());
                let streak = row.streak;
                parameters.insert("user", row.user_id);
                parameters.insert("count", row.count.to_string());
                parameters.insert("streak", streak.to_string());
                let line = self.message("serverstats.talkerLine", &locale, &parameters)?;
                lines.push(style_streak_message(line, streak));
            }
        }
        if premium {
            let mut parameters = BTreeMap::new();
            parameters.insert("days", stats.streak.to_string());
            let line = self.message("serverstats.streak", &locale, &parameters)?;
            lines.push(style_streak_message(line, stats.streak));
            let mut parameters = BTreeMap::new();
            parameters.insert("points", games.points.to_string());
            parameters.insert("wins", games.wins.to_string());
            parameters.insert("players", games.players.to_string());
            lines.push(self.message("serverstats.games", &locale, &parameters)?);
            if !games.top_players.is_empty() {
                lines.push(self.message("serverstats.topPlayers", &locale, &BTreeMap::new())?);
                for (index, row) in games.top_players.into_iter().enumerate() {
                    let mut parameters = BTreeMap::new();
                    parameters.insert("rank", (index + 1).to_string());
                    parameters.insert("user", row.user_id);
                    parameters.insert("points", row.points.to_string());
                    parameters.insert("wins", row.wins.to_string());
                    lines.push(self.message("serverstats.playerLine", &locale, &parameters)?);
                }
            }
        } else {
            lines.push(self.message("serverstats.upsell", &locale, &BTreeMap::new())?);
            if let (Some(client_id), Some(secret)) = (&self.client_id, &self.redemption_secret)
                && secret.len() >= VOTE_REDEMPTION_SECRET_MIN_LENGTH
                && store
                    .vote_reward_status(&user_id, secret, now_ms)
                    .map_err(|_| GatewayEventDispatchError)?
                    .eligible
            {
                let mut parameters = BTreeMap::new();
                parameters.insert("url", format!("https://top.gg/bot/{client_id}/vote"));
                lines.push(self.message("vote.upsell", &locale, &parameters)?);
            }
        }
        Ok(lines.join("\n"))
    }
}

#[async_trait::async_trait]
impl GatewayEventSink for ServerStatsGatewaySink {
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
        if parse_server_stats_command(&command.data)
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

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            duration.as_millis().min(i64::MAX as u128) as i64
        })
}
