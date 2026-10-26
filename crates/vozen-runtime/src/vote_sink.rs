//! Opt-in gateway adapter for the public `/vote` command.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::ui::message_embed;
use serenity::{
    builder::{
        CreateActionRow, CreateButton, CreateInteractionResponse, CreateInteractionResponseMessage,
    },
    client::Context,
    model::application::Interaction,
};
use vozen_discord::{
    GatewayEventDispatchError, GatewayEventSink, VoiceResponseLocalizer, parse_vote_command,
};
use vozen_store::{SqliteStore, VOTE_REDEMPTION_SECRET_MIN_LENGTH};

pub struct VoteGatewaySink {
    client_id: Option<String>,
    redemption_secret: Option<String>,
    store: Arc<Mutex<SqliteStore>>,
    localizer: VoiceResponseLocalizer,
}

impl VoteGatewaySink {
    pub fn new(
        client_id: Option<String>,
        redemption_secret: Option<String>,
        store: Arc<Mutex<SqliteStore>>,
    ) -> Result<Self, GatewayEventDispatchError> {
        Ok(Self {
            client_id,
            redemption_secret,
            store,
            localizer: VoiceResponseLocalizer::from_generated_contract()
                .map_err(|_| GatewayEventDispatchError)?,
        })
    }

    fn response(
        &self,
        command: &serenity::model::application::CommandInteraction,
    ) -> Result<(String, Option<(String, String)>), GatewayEventDispatchError> {
        let locale = crate::ui::guild_locale(&self.store, command.guild_id)?;
        let Some(client_id) = self.client_id.as_deref().filter(|id| !id.trim().is_empty()) else {
            return Ok((
                self.localizer
                    .render_key("vote.noClientId", None, Some(&locale), &BTreeMap::new())
                    .ok_or(GatewayEventDispatchError)?,
                None,
            ));
        };

        let url = format!("https://top.gg/bot/{client_id}/vote");
        let mut parameters = BTreeMap::new();
        parameters.insert("url", url.clone());
        let mut content = self
            .localizer
            .render_key("vote.link", None, Some(&locale), &parameters)
            .ok_or(GatewayEventDispatchError)?;
        if let Some(secret) = self
            .redemption_secret
            .as_deref()
            .filter(|secret| secret.len() >= VOTE_REDEMPTION_SECRET_MIN_LENGTH)
        {
            let limit_reached = self
                .store
                .lock()
                .ok()
                .and_then(|store| {
                    store
                        .vote_reward_status(&command.user.id.to_string(), secret, now_ms())
                        .ok()
                })
                .is_some_and(|status| !status.eligible);
            if limit_reached {
                content.push_str("\n\n");
                content.push_str(rolling_limit_notice(&locale));
            }
        }
        let button_label = self
            .localizer
            .render_key("vote.button", None, Some(&locale), &BTreeMap::new())
            .ok_or(GatewayEventDispatchError)?;
        Ok((content, Some((url, button_label))))
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn rolling_limit_notice(locale: &str) -> &'static str {
    if locale.to_ascii_lowercase().starts_with("pt") {
        "🗳️ Esta conta atingiu o limite de 4 recompensas em 30 dias. Ainda podes votar para apoiar o Vozen."
    } else {
        "🗳️ This account has reached the limit of 4 rewards in 30 days. You can still vote to support Vozen."
    }
}

#[async_trait::async_trait]
impl GatewayEventSink for VoteGatewaySink {
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
        if parse_vote_command(&command.data)
            .map_err(|_| GatewayEventDispatchError)?
            .is_none()
        {
            return Ok(());
        }
        let (content, vote) = self.response(&command)?;
        let mut response =
            CreateInteractionResponseMessage::new().embeds(vec![message_embed(content)]);
        if let Some((url, button_label)) = vote {
            response = response.components(vec![CreateActionRow::Buttons(vec![
                CreateButton::new_link(url).label(button_label),
            ])]);
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

#[cfg(test)]
mod tests {
    #[test]
    fn vote_url_uses_the_application_id() {
        assert_eq!(
            format!("https://top.gg/bot/{}/vote", "123"),
            "https://top.gg/bot/123/vote"
        );
    }
}
