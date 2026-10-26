//! Shared presentation helpers for Rust responses.

use serenity::builder::CreateEmbed;
use serenity::model::id::GuildId;
use std::sync::Mutex;
use vozen_discord::GatewayEventDispatchError;
use vozen_store::SqliteStore;

/// Public copy follows the saved server setting, never the command author's client language.
pub fn guild_locale(
    store: &Mutex<SqliteStore>,
    guild: Option<GuildId>,
) -> Result<String, GatewayEventDispatchError> {
    let Some(guild) = guild else {
        return Ok("en".into());
    };
    store
        .lock()
        .map_err(|_| GatewayEventDispatchError)?
        .guild_config(&guild.to_string())
        .map(|config| config.locale)
        .map_err(|_| GatewayEventDispatchError)
}

pub const BRAND_COLOR: u32 = 0x5865F2;
const SUCCESS_COLOR: u32 = 0x57F287;
const WARNING_COLOR: u32 = 0xFEE75C;
const DANGER_COLOR: u32 = 0xED4245;
const PREMIUM_COLOR: u32 = 0xF1C40F;

/// Builds the shared Vozen card used by command and gateway responses.
pub fn message_embed(content: impl Into<String>) -> CreateEmbed {
    let content = content.into();
    CreateEmbed::new()
        .color(color_for_content(&content))
        .description(content)
}

/// Explicit semantic cards for handlers whose text does not carry a status emoji.
#[allow(dead_code)]
pub fn danger_embed(content: impl Into<String>) -> CreateEmbed {
    CreateEmbed::new().color(DANGER_COLOR).description(content)
}

#[allow(dead_code)]
pub fn success_embed(content: impl Into<String>) -> CreateEmbed {
    CreateEmbed::new().color(SUCCESS_COLOR).description(content)
}

/// Mirrors the TypeScript card tone detection without depending on translated words.
pub fn color_for_content(content: &str) -> u32 {
    let start = content.trim_start();
    if start.starts_with(['✅', '☑', '🎉', '🥳', '🟢', '🟩']) {
        SUCCESS_COLOR
    } else if start.starts_with(['⚠', '⏳', '🟡', '🟨']) {
        WARNING_COLOR
    } else if start.starts_with(['❌', '🚫', '⛔', '🛑', '🔴', '🟥']) {
        DANGER_COLOR
    } else if start.starts_with(['💎', '👑']) {
        PREMIUM_COLOR
    } else {
        BRAND_COLOR
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_locale_defaults_to_english_and_is_scoped_to_the_saved_server() {
        let store = Mutex::new(SqliteStore::open_in_memory().unwrap());
        assert_eq!(guild_locale(&store, Some(GuildId::new(7))).unwrap(), "en");
        store
            .lock()
            .unwrap()
            .update_guild_config(
                "7",
                vozen_store::GuildConfigPatch {
                    locale: Some("pt".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(guild_locale(&store, Some(GuildId::new(7))).unwrap(), "pt");
        assert_eq!(guild_locale(&store, Some(GuildId::new(8))).unwrap(), "en");
        assert_eq!(guild_locale(&store, None).unwrap(), "en");
    }

    #[test]
    fn card_tone_matches_typescript_semantic_marks() {
        assert_eq!(color_for_content("✅ ready"), SUCCESS_COLOR);
        assert_eq!(color_for_content("⚠ try again"), WARNING_COLOR);
        assert_eq!(color_for_content("❌ failed"), DANGER_COLOR);
        assert_eq!(color_for_content("💎 premium"), PREMIUM_COLOR);
        assert_eq!(color_for_content("📊 stats"), BRAND_COLOR);
    }
}
