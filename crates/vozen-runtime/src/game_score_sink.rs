//! Opt-in gateway adapter for read-only game score views.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use crate::ui::message_embed;
use serenity::{
    builder::{CreateAllowedMentions, CreateInteractionResponse, CreateInteractionResponseMessage},
    client::Context,
    model::application::Interaction,
};
use vozen_discord::{
    GameScoreCommand, GatewayEventDispatchError, GatewayEventSink, VoiceResponseLocalizer,
    parse_game_score_command,
};
use vozen_store::SqliteStore;

pub struct GameScoreGatewaySink {
    store: Arc<Mutex<SqliteStore>>,
    localizer: VoiceResponseLocalizer,
}

impl GameScoreGatewaySink {
    pub fn new(store: Arc<Mutex<SqliteStore>>) -> Result<Self, GatewayEventDispatchError> {
        Ok(Self {
            store,
            localizer: VoiceResponseLocalizer::from_generated_contract()
                .map_err(|_| GatewayEventDispatchError)?,
        })
    }

    fn render(
        &self,
        key: &str,
        locale: &str,
        parameters: &BTreeMap<&str, String>,
    ) -> Result<String, GatewayEventDispatchError> {
        self.localizer
            .render_key(key, Some(locale), None, parameters)
            .ok_or(GatewayEventDispatchError)
    }

    fn response(
        &self,
        command: &serenity::model::application::CommandInteraction,
        action: GameScoreCommand,
    ) -> Result<(String, bool), GatewayEventDispatchError> {
        let locale = if matches!(action, GameScoreCommand::Stats) {
            command.locale.clone()
        } else {
            crate::ui::guild_locale(&self.store, command.guild_id)?
        };
        let Some(guild_id) = command.guild_id else {
            return Ok((
                self.render("game.leaderboard.empty", &locale, &BTreeMap::new())?,
                false,
            ));
        };
        let guild_id = guild_id.get().to_string();
        let store = self.store.lock().map_err(|_| GatewayEventDispatchError)?;
        match action {
            GameScoreCommand::Leaderboard => {
                let rows = store
                    .game_leaderboard(&guild_id, 10)
                    .map_err(|_| GatewayEventDispatchError)?;
                if rows.is_empty() {
                    return Ok((
                        self.render("game.leaderboard.empty", &locale, &BTreeMap::new())?,
                        false,
                    ));
                }
                let mut lines =
                    vec![self.render("game.leaderboard.title", &locale, &BTreeMap::new())?];
                for (index, row) in rows.into_iter().enumerate() {
                    let mut parameters = BTreeMap::new();
                    parameters.insert("rank", rank_medal(index + 1));
                    parameters.insert("user", row.user_id);
                    parameters.insert("points", row.points.to_string());
                    parameters.insert("wins", row.wins.to_string());
                    lines.push(self.render("game.leaderboard.line", &locale, &parameters)?);
                }
                Ok((lines.join("\n"), false))
            }
            GameScoreCommand::Stats => {
                let stats = store
                    .game_user_stats(&guild_id, &command.user.id.get().to_string())
                    .map_err(|_| GatewayEventDispatchError)?;
                if stats.points == 0 && stats.wins == 0 {
                    return Ok((
                        self.render("game.stats.none", &locale, &BTreeMap::new())?,
                        true,
                    ));
                }
                let rank = match stats.rank {
                    Some(rank) => {
                        let mut parameters = BTreeMap::new();
                        parameters.insert("rank", rank.to_string());
                        parameters.insert("total", stats.total.to_string());
                        self.render("game.stats.rank", &locale, &parameters)?
                    }
                    None => self.render("game.stats.unranked", &locale, &BTreeMap::new())?,
                };
                let mut parameters = BTreeMap::new();
                parameters.insert("points", stats.points.to_string());
                parameters.insert("wins", stats.wins.to_string());
                parameters.insert("rank", rank);
                Ok((self.render("game.stats.body", &locale, &parameters)?, true))
            }
        }
    }
}

#[async_trait::async_trait]
impl GatewayEventSink for GameScoreGatewaySink {
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
        let Some(action) =
            parse_game_score_command(&command.data).map_err(|_| GatewayEventDispatchError)?
        else {
            return Ok(());
        };
        let (content, ephemeral) = self.response(&command, action)?;
        let mut response = CreateInteractionResponseMessage::new()
            .embeds(vec![message_embed(content)])
            .allowed_mentions(
                CreateAllowedMentions::new()
                    .all_users(false)
                    .all_roles(false)
                    .everyone(false),
            );
        if ephemeral {
            response = response.ephemeral(true);
        }
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

fn rank_medal(rank: usize) -> String {
    match rank {
        1 => "🥇".to_owned(),
        2 => "🥈".to_owned(),
        3 => "🥉".to_owned(),
        _ => format!("#{rank}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rank_labels_match_node() {
        assert_eq!(rank_medal(1), "🥇");
        assert_eq!(rank_medal(3), "🥉");
        assert_eq!(rank_medal(4), "#4");
    }
}
