//! Opt-in gateway adapter for public `/invite`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use vozen_store::SqliteStore;

use crate::ui::message_embed;
use serenity::{
    builder::{
        CreateActionRow, CreateButton, CreateInteractionResponse, CreateInteractionResponseMessage,
    },
    client::Context,
    model::application::Interaction,
};
use vozen_discord::{
    GatewayEventDispatchError, GatewayEventSink, VoiceResponseLocalizer, parse_invite_command,
};

pub const INVITE_PERMISSIONS: &str = "326420745232";

pub struct InviteGatewaySink {
    store: Arc<Mutex<SqliteStore>>,
    client_id: Option<String>,
    localizer: VoiceResponseLocalizer,
}

impl InviteGatewaySink {
    pub fn new(
        client_id: Option<String>,
        store: Arc<Mutex<SqliteStore>>,
    ) -> Result<Self, GatewayEventDispatchError> {
        Ok(Self {
            store,
            client_id,
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
                    .render_key("invite.noClientId", None, Some(&locale), &BTreeMap::new())
                    .ok_or(GatewayEventDispatchError)?,
                None,
            ));
        };

        let url = format!(
            "https://discord.com/oauth2/authorize?client_id={client_id}&permissions={INVITE_PERMISSIONS}&scope=bot%20applications.commands"
        );
        let mut parameters = BTreeMap::new();
        parameters.insert("url", url.clone());
        let content = self
            .localizer
            .render_key("invite.link", None, Some(&locale), &parameters)
            .ok_or(GatewayEventDispatchError)?;
        let button_label = self
            .localizer
            .render_key("invite.button", None, Some(&locale), &BTreeMap::new())
            .ok_or(GatewayEventDispatchError)?;
        Ok((content, Some((url, button_label))))
    }
}

#[async_trait::async_trait]
impl GatewayEventSink for InviteGatewaySink {
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
        if parse_invite_command(&command.data)
            .map_err(|_| GatewayEventDispatchError)?
            .is_none()
        {
            return Ok(());
        }
        let (content, invite) = self.response(&command)?;
        let mut response =
            CreateInteractionResponseMessage::new().embeds(vec![message_embed(content)]);
        if let Some((url, button_label)) = invite {
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
    use super::*;

    #[test]
    fn canonical_url_uses_the_shared_permission_bitfield() {
        let permissions: u64 = INVITE_PERMISSIONS.parse().unwrap();
        assert_eq!(permissions, 326420745216 | 16);
        assert_eq!(permissions & 8, 0);
        let url = format!(
            "https://discord.com/oauth2/authorize?client_id=123&permissions={INVITE_PERMISSIONS}&scope=bot%20applications.commands"
        );
        assert_eq!(
            url,
            "https://discord.com/oauth2/authorize?client_id=123&permissions=326420745232&scope=bot%20applications.commands"
        );
    }
}
