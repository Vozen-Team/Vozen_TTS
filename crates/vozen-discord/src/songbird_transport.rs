//! Songbird implementation of the narrow voice-session transport.
//!
//! Kept behind `voice-driver`: control-plane tests do not require a local Opus toolchain, while
//! the production image must compile this feature before enabling Rust voice sessions.

use async_trait::async_trait;
use serenity::client::Context;
#[cfg(feature = "voice-driver")]
use serenity::{
    builder::EditVoiceState,
    http::{Http, HttpError},
    model::{
        channel::{Channel, ChannelType},
        id::{ChannelId, GuildId},
        permissions::Permissions,
    },
};
#[cfg(feature = "voice-driver")]
use std::time::Duration;

use crate::{VoiceSessionTransport, VoiceSessionTransportError};

pub struct SongbirdVoiceSessionTransport {
    #[cfg_attr(not(feature = "voice-driver"), allow(dead_code))]
    context: Context,
}

impl SongbirdVoiceSessionTransport {
    pub fn new(context: Context) -> Self {
        Self { context }
    }
}

#[cfg(feature = "voice-driver")]
#[async_trait]
impl VoiceSessionTransport for SongbirdVoiceSessionTransport {
    async fn join(
        &self,
        guild_id: &str,
        channel_id: &str,
    ) -> Result<(), VoiceSessionTransportError> {
        self.join_channel(guild_id, channel_id, None).await
    }

    async fn join_if_idle(
        &self,
        guild_id: &str,
        channel_id: &str,
        still_authorized: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), VoiceSessionTransportError> {
        self.join_channel(guild_id, channel_id, Some(still_authorized))
            .await
    }

    async fn leave(&self, guild_id: &str) -> Result<(), VoiceSessionTransportError> {
        let guild_id = parse_snowflake(guild_id)?;
        let manager = songbird::get(&self.context)
            .await
            .ok_or(VoiceSessionTransportError::Unavailable)?;
        manager
            .remove(GuildId::new(guild_id))
            .await
            .map_err(|_| VoiceSessionTransportError::Failed)
    }
}

#[cfg(feature = "voice-driver")]
impl SongbirdVoiceSessionTransport {
    async fn join_channel(
        &self,
        guild_id: &str,
        channel_id: &str,
        still_authorized: Option<&(dyn Fn() -> bool + Sync)>,
    ) -> Result<(), VoiceSessionTransportError> {
        if still_authorized.is_some_and(|authorized| !authorized()) {
            return Err(VoiceSessionTransportError::PermissionDenied);
        }
        let guild_id = GuildId::new(parse_snowflake(guild_id)?);
        let channel_id = ChannelId::new(parse_snowflake(channel_id)?);
        let manager = songbird::get(&self.context)
            .await
            .ok_or(VoiceSessionTransportError::Unavailable)?;
        // Release the call lock before waiting for Discord's handshake events.
        let joined = tokio::time::timeout(Duration::from_secs(10), async {
            let call = manager.get_or_insert(guild_id);
            let join = {
                let mut handler = call.lock().await;
                if let Some(still_authorized) = still_authorized
                    && (handler.current_channel().is_some() || !still_authorized())
                {
                    return Err(VoiceSessionTransportError::PermissionDenied);
                }
                handler
                    .join(channel_id)
                    .await
                    .map_err(|_| VoiceSessionTransportError::Failed)?
            };
            join.await.map_err(|_| VoiceSessionTransportError::Failed)
        })
        .await
        .unwrap_or(Err(VoiceSessionTransportError::Failed));
        match joined {
            Err(VoiceSessionTransportError::Failed) if still_authorized.is_none() => {
                return Err(join_failure(&self.context.http, guild_id, channel_id).await);
            }
            result => result?,
        }
        // Songbird only decodes incoming Opus when explicitly configured. Keep this opt-in so a
        // normal Rust voice canary does not pay the receive CPU cost or collect audio; the live
        // STT promotion enables it with `RUST_TRANSCRIBE_LIVE_ENABLED=true` and installs its
        // consent-gated receiver on the same call.
        if live_receive_enabled()
            && let Some(call) = manager.get(guild_id)
        {
            let mut handler = call.lock().await;
            handler.set_config(songbird::Config::default().decode_mode(
                songbird::driver::DecodeMode::Decode(songbird::driver::DecodeConfig::default()),
            ));
        }
        // Discord places bots in Stage channels as audience by default. Match Node's best-effort
        // behaviour: first self-promote, then request to speak if a moderator must approve it.
        // A Stage moderation failure never invalidates an otherwise successful voice join.
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            promote_stage_speaker(&self.context, channel_id),
        )
        .await;
        Ok(())
    }
}

#[cfg(feature = "voice-driver")]
async fn join_failure(
    http: &Http,
    guild_id: GuildId,
    channel_id: ChannelId,
) -> VoiceSessionTransportError {
    let denied = tokio::time::timeout(Duration::from_secs(3), async {
        let channel = match http.get_channel(channel_id).await {
            Ok(Channel::Guild(channel)) if channel.guild_id == guild_id => channel,
            Err(serenity::Error::Http(HttpError::UnsuccessfulRequest(response)))
                if response.status_code.as_u16() == 403
                    && matches!(response.error.code, 50001 | 50013) =>
            {
                return true;
            }
            _ => return false,
        };
        if !matches!(channel.kind, ChannelType::Voice | ChannelType::Stage) {
            return false;
        }
        let member = async {
            let bot = http.get_current_user().await?;
            guild_id.member(http, bot.id).await
        };
        let (guild, member) = tokio::join!(guild_id.to_partial_guild(http), member);
        match (guild, member) {
            (Ok(guild), Ok(member)) => !guild
                .user_permissions_in(&channel, &member)
                .contains(Permissions::VIEW_CHANNEL | Permissions::CONNECT),
            _ => false,
        }
    })
    .await
    .unwrap_or(false);
    if denied {
        VoiceSessionTransportError::PermissionDenied
    } else {
        VoiceSessionTransportError::Failed
    }
}

#[cfg(feature = "voice-driver")]
fn live_receive_enabled() -> bool {
    std::env::var("RUST_TRANSCRIBE_LIVE_ENABLED")
        .ok()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
}

#[cfg(feature = "voice-driver")]
async fn promote_stage_speaker(context: &Context, channel_id: ChannelId) {
    let Ok(Channel::Guild(channel)) = channel_id.to_channel(&context.http).await else {
        return;
    };
    if channel.kind != ChannelType::Stage {
        return;
    }
    if channel
        .edit_own_voice_state(&context.http, EditVoiceState::new().suppress(false))
        .await
        .is_err()
    {
        let _ = channel
            .edit_own_voice_state(&context.http, EditVoiceState::new().request_to_speak(true))
            .await;
    }
}

#[cfg(not(feature = "voice-driver"))]
#[async_trait]
impl VoiceSessionTransport for SongbirdVoiceSessionTransport {
    async fn join(
        &self,
        _guild_id: &str,
        _channel_id: &str,
    ) -> Result<(), VoiceSessionTransportError> {
        Err(VoiceSessionTransportError::Unavailable)
    }

    async fn leave(&self, _guild_id: &str) -> Result<(), VoiceSessionTransportError> {
        Err(VoiceSessionTransportError::Unavailable)
    }
}

#[cfg(any(feature = "voice-driver", test))]
fn parse_snowflake(value: &str) -> Result<u64, VoiceSessionTransportError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|id| *id != 0)
        .ok_or(VoiceSessionTransportError::Failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_discord_ids_fail_before_touching_songbird() {
        assert!(parse_snowflake("123").is_ok());
        assert!(parse_snowflake("0").is_err());
        assert!(parse_snowflake("guild").is_err());
    }

    #[cfg(feature = "voice-driver")]
    #[tokio::test]
    async fn failed_join_reports_permission_denial_only_for_discord_channel_access_errors() {
        use serenity::http::HttpBuilder;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::TcpListener;

        for (status, code, expected) in [
            (
                "403 Forbidden",
                50001,
                VoiceSessionTransportError::PermissionDenied,
            ),
            (
                "403 Forbidden",
                50013,
                VoiceSessionTransportError::PermissionDenied,
            ),
            ("403 Forbidden", 40333, VoiceSessionTransportError::Failed),
            ("404 Not Found", 10003, VoiceSessionTransportError::Failed),
            (
                "503 Service Unavailable",
                0,
                VoiceSessionTransportError::Failed,
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let http = HttpBuilder::new("synthetic-test-token")
                .proxy(format!("http://{}", listener.local_addr().unwrap()))
                .ratelimiter_disabled(true)
                .build();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                assert!(line.starts_with("GET /api/v10/channels/456 "));
                loop {
                    line.clear();
                    assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                }
                let body = format!(r#"{{"code":{code},"message":"QA fixture"}}"#);
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len(),
                );
                stream
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
            });
            let actual = join_failure(&http, GuildId::new(123), ChannelId::new(456)).await;
            tokio::time::timeout(Duration::from_secs(1), server)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(actual, expected, "{status} / {code}");
        }
    }
}
