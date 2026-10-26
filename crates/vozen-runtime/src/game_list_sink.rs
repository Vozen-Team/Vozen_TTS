//! Opt-in gateway adapter for the read-only `/game list` leaf.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use vozen_store::SqliteStore;

use crate::ui::message_embed;
use serenity::{
    builder::{CreateInteractionResponse, CreateInteractionResponseMessage},
    client::Context,
    model::application::Interaction,
};
use vozen_discord::{
    GAME_CATALOG, GatewayEventDispatchError, GatewayEventSink, VoiceResponseLocalizer,
    parse_game_list_command,
};

pub struct GameListGatewaySink {
    store: Arc<Mutex<SqliteStore>>,
    localizer: VoiceResponseLocalizer,
}

impl GameListGatewaySink {
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
        let mut lines = vec![self.render("game.list.title", command, &BTreeMap::new())?];
        for game in GAME_CATALOG {
            let mut parameters = BTreeMap::new();
            parameters.insert(
                "name",
                self.render(game.name_key, command, &BTreeMap::new())?,
            );
            parameters.insert(
                "desc",
                self.render(game.desc_key, command, &BTreeMap::new())?,
            );
            lines.push(self.render("game.list.line", command, &parameters)?);
        }
        Ok(lines.join("\n"))
    }
}

#[async_trait::async_trait]
impl GatewayEventSink for GameListGatewaySink {
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
        if parse_game_list_command(&command.data)
            .map_err(|_| GatewayEventDispatchError)?
            .is_none()
        {
            return Ok(());
        }
        command
            .create_response(
                &context,
                CreateInteractionResponse::Message(
                    CreateInteractionResponseMessage::new()
                        .embeds(vec![message_embed(self.response(&command)?)]),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn game_registry_matches_node_order_and_size() {
        assert_eq!(GAME_CATALOG.len(), 16);
        assert_eq!(
            (GAME_CATALOG[0].name_key, GAME_CATALOG[0].desc_key),
            ("game.guessLanguage.name", "game.guessLanguage.desc")
        );
        assert_eq!(GAME_CATALOG[14].desc_key, "game.wordChain.descr");
        assert_eq!(GAME_CATALOG[15].name_key, "game.headsOrTails.name");
    }
}
