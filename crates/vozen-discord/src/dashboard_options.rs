//! Live Discord dashboard options.
//!
//! This adapter deliberately makes a small, current Discord request only after the API's OAuth
//! authorization boundary. It fails closed when READY, the bot member, or any Discord response
//! is unavailable, and it retains no guild/member cache between browser requests.

use std::{collections::HashMap, num::NonZeroU64, time::Duration};

use serenity::model::{
    Permissions,
    channel::ChannelType,
    id::{GuildId, UserId},
};

use crate::{GatewayState, RejoinChannelState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordDashboardOption {
    pub id: String,
    pub label: String,
}

pub struct DiscordDashboardMember {
    pub name: String,
    pub avatar_url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscordDashboardOptions {
    pub channels: Vec<DiscordDashboardOption>,
    pub voice_channels: Vec<DiscordDashboardOption>,
    pub roles: Vec<DiscordDashboardOption>,
}

/// Discovers dashboard settings from current Discord state. Voice and locale catalogues live in
/// the runtime because they are deployment configuration, not Discord guild data.
#[derive(Clone)]
pub struct DiscordDashboardOptionsProvider {
    gateway_state: GatewayState,
}

impl DiscordDashboardOptionsProvider {
    pub fn new(gateway_state: GatewayState) -> Self {
        Self { gateway_state }
    }

    pub async fn guild_member_profiles(
        &self,
        guild_id: &str,
        user_ids: &[String],
    ) -> HashMap<String, DiscordDashboardMember> {
        let mut profiles = HashMap::new();
        let (Ok(guild_id), Some(http)) = (
            guild_id.parse::<NonZeroU64>(),
            self.gateway_state.discord_http(),
        ) else {
            return profiles;
        };
        let guild_id = GuildId::new(guild_id.get());
        let mut lookups = tokio::task::JoinSet::new();
        for id in user_ids {
            let Ok(user_id) = id.parse::<NonZeroU64>() else {
                continue;
            };
            let http = http.clone();
            let id = id.clone();
            lookups.spawn(async move {
                let member = guild_id
                    .member(&http, UserId::new(user_id.get()))
                    .await
                    .ok()?;
                Some((
                    id,
                    DiscordDashboardMember {
                        name: member.display_name().to_owned(),
                        avatar_url: member.face().replace("?size=1024", "?size=64"),
                    },
                ))
            });
        }
        // Keep the guild dashboard usable if Discord is slow or unavailable.
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(result) = lookups.join_next().await {
                if let Ok(Some((id, profile))) = result {
                    profiles.insert(id, profile);
                }
            }
        })
        .await;
        profiles
    }

    pub async fn options_for_guild(&self, guild_id: &str) -> Option<DiscordDashboardOptions> {
        let guild_id = GuildId::new(guild_id.parse().ok()?);
        let http = self.gateway_state.discord_http()?;
        let bot_user_id = serenity::model::id::UserId::new(
            self.gateway_state.bot_user_id()?.parse::<u64>().ok()?,
        );
        let (guild, channels, member) = tokio::join!(
            guild_id.to_partial_guild(&http),
            guild_id.channels(&http),
            guild_id.member(&http, bot_user_id),
        );
        let guild = guild.ok()?;
        let channels = channels.ok()?;
        let member = member.ok()?;
        Some(dashboard_options_from_live_guild(
            &guild, &channels, &member,
        ))
    }

    /// Resolves a persisted call only from current Discord state. A transient REST failure is
    /// deliberately indistinguishable from insufficient permission here: both retain the hint
    /// without joining, whereas a missing or non-voice channel can be forgotten safely.
    pub async fn rejoin_channel_state(
        &self,
        guild_id: &str,
        channel_id: &str,
    ) -> RejoinChannelState {
        let Ok(guild_id) = guild_id.parse::<u64>() else {
            return RejoinChannelState::Gone;
        };
        let Ok(channel_id) = channel_id.parse::<u64>() else {
            return RejoinChannelState::Gone;
        };
        let Some(http) = self.gateway_state.discord_http() else {
            return RejoinChannelState::NoPermissions;
        };
        let Some(bot_user_id) = self.gateway_state.bot_user_id() else {
            return RejoinChannelState::NoPermissions;
        };
        let Ok(bot_user_id) = bot_user_id.parse::<u64>() else {
            return RejoinChannelState::NoPermissions;
        };
        let guild_id = GuildId::new(guild_id);
        let (guild, channels, member) = tokio::join!(
            guild_id.to_partial_guild(&http),
            guild_id.channels(&http),
            guild_id.member(&http, serenity::model::id::UserId::new(bot_user_id)),
        );
        let (Ok(guild), Ok(channels), Ok(member)) = (guild, channels, member) else {
            return RejoinChannelState::NoPermissions;
        };
        let Some(channel) = channels.get(&serenity::model::id::ChannelId::new(channel_id)) else {
            return RejoinChannelState::Gone;
        };
        if !matches!(channel.kind, ChannelType::Voice | ChannelType::Stage) {
            return RejoinChannelState::Gone;
        }
        if guild
            .user_permissions_in(channel, &member)
            .contains(Permissions::VIEW_CHANNEL | Permissions::CONNECT | Permissions::SPEAK)
        {
            RejoinChannelState::Ready
        } else {
            RejoinChannelState::NoPermissions
        }
    }
}

fn dashboard_options_from_live_guild(
    guild: &serenity::model::guild::PartialGuild,
    live_channels: &std::collections::HashMap<
        serenity::model::id::ChannelId,
        serenity::model::channel::GuildChannel,
    >,
    bot_member: &serenity::model::guild::Member,
) -> DiscordDashboardOptions {
    let required_text =
        Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES | Permissions::READ_MESSAGE_HISTORY;
    let mut channels = live_channels
        .values()
        .filter(|channel| matches!(channel.kind, ChannelType::Text))
        .filter(|channel| {
            guild
                .user_permissions_in(channel, bot_member)
                .contains(required_text)
        })
        .collect::<Vec<_>>();
    channels.sort_by(|left, right| {
        left.position
            .cmp(&right.position)
            .then(left.name.cmp(&right.name))
    });
    let channels = channels.into_iter().map(channel_option).collect();
    let mut voice_channels = live_channels
        .values()
        .filter(|channel| matches!(channel.kind, ChannelType::Voice | ChannelType::Stage))
        .filter(|channel| {
            guild
                .user_permissions_in(channel, bot_member)
                .contains(Permissions::VIEW_CHANNEL | Permissions::CONNECT | Permissions::SPEAK)
        })
        .collect::<Vec<_>>();
    voice_channels.sort_by(|left, right| {
        left.position
            .cmp(&right.position)
            .then(left.name.cmp(&right.name))
    });
    let voice_channels = voice_channels.into_iter().map(channel_option).collect();
    let mut roles = guild.roles.values().collect::<Vec<_>>();
    roles.retain(|role| role.id.get() != guild.id.get() && !role.managed);
    roles.sort_by(|left, right| {
        right
            .position
            .cmp(&left.position)
            .then(left.name.cmp(&right.name))
    });
    let roles = roles
        .into_iter()
        .map(|role| DiscordDashboardOption {
            id: role.id.get().to_string(),
            label: role.name.clone(),
        })
        .collect();
    DiscordDashboardOptions {
        channels,
        voice_channels,
        roles,
    }
}

fn channel_option(channel: &serenity::model::channel::GuildChannel) -> DiscordDashboardOption {
    DiscordDashboardOption {
        id: channel.id.get().to_string(),
        label: format!("#{}", channel.name),
    }
}

/// Friendly enough for the dashboard while preserving the durable model ID as the submitted
/// value. Piper model IDs are intentionally parsed conservatively: unknown formats remain
/// visible instead of being silently dropped.
pub fn voice_display_options(models: &[String]) -> Vec<DiscordDashboardOption> {
    let mut unique = models.to_vec();
    unique.sort_unstable();
    unique.dedup();
    unique
        .into_iter()
        .map(|id| DiscordDashboardOption {
            label: voice_display_name(&id),
            id,
        })
        .collect()
}

fn voice_display_name(model: &str) -> String {
    let mut segments = model.split('-');
    let locale = segments.next().unwrap_or(model).replace('_', " ");
    let name = segments.next().map(|name| name.replace('_', " "));
    match name {
        Some(name) if !name.is_empty() => format!("{locale} — {name}"),
        _ => locale,
    }
}

pub fn locale_display_options() -> Vec<DiscordDashboardOption> {
    const LOCALES: &[(&str, &str)] = &[
        ("en", "English"),
        ("pt", "Português"),
        ("es", "Español"),
        ("fr", "Français"),
        ("de", "Deutsch"),
        ("nl", "Nederlands"),
        ("pl", "Polski"),
        ("tr", "Türkçe"),
        ("cs", "Čeština"),
        ("sv", "Svenska"),
        ("fi", "Suomi"),
        ("da", "Dansk"),
        ("ro", "Română"),
        ("hu", "Magyar"),
        ("cy", "Cymraeg"),
        ("is", "Íslenska"),
        ("lb", "Lëtzebuergesch"),
        ("lv", "Latviešu"),
        ("sk", "Slovenčina"),
        ("sl", "Slovenščina"),
        ("sw", "Kiswahili"),
        ("vi", "Tiếng Việt"),
        ("ca", "Català"),
        ("it", "Italiano"),
        ("el", "Ελληνικά"),
        ("ru", "Русский"),
        ("uk", "Українська"),
        ("kk", "Қазақша"),
        ("sr", "Српски"),
        ("ar", "العربية"),
        ("fa", "فارسی"),
        ("ka", "ქართული"),
        ("ne", "नेपाली"),
        ("zh", "中文"),
        ("ja", "日本語"),
    ];
    LOCALES
        .iter()
        .map(|(id, label)| DiscordDashboardOption {
            id: (*id).to_owned(),
            label: (*label).to_owned(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_catalogue_is_deduplicated_and_keeps_unknown_models_visible() {
        assert_eq!(
            voice_display_options(&[
                "en_US-amy-medium".into(),
                "raw".into(),
                "en_US-amy-medium".into()
            ]),
            vec![
                DiscordDashboardOption {
                    id: "en_US-amy-medium".into(),
                    label: "en US — amy".into()
                },
                DiscordDashboardOption {
                    id: "raw".into(),
                    label: "raw".into()
                },
            ]
        );
    }

    #[test]
    fn locale_catalogue_matches_dashboard_validation_surface() {
        let locales = locale_display_options();
        assert_eq!(locales.len(), 35);
        assert!(
            locales
                .iter()
                .any(|locale| locale.id == "pt" && locale.label == "Português")
        );
    }

    #[tokio::test]
    async fn rejoin_validation_fails_closed_without_a_ready_discord_connection() {
        let provider = DiscordDashboardOptionsProvider::new(GatewayState::default());
        assert_eq!(
            provider
                .rejoin_channel_state("123456789012345678", "123456789012345678")
                .await,
            RejoinChannelState::NoPermissions
        );
        assert_eq!(
            provider.rejoin_channel_state("bad", "channel").await,
            RejoinChannelState::Gone
        );
    }
}
