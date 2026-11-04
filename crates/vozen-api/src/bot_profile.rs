use std::{sync::Arc, time::Duration};

use axum::http::{HeaderValue, StatusCode};
use serde::Deserialize;
use serde_json::{Map, Value, json};

const DISCORD_API: &str = "https://discord.com/api/v10";
const PROFILE_POLICY: &str = include_str!("bot-profile-policy.json");
pub(super) const MAX_PROFILE_BODY: usize = 5_600_256;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProfileEdit {
    nick: Option<String>,
    avatar: Option<String>,
    banner: Option<String>,
    #[serde(default)]
    reset: bool,
}

#[derive(Clone)]
pub struct DiscordBotProfile {
    client: reqwest::Client,
    auth: HeaderValue,
    bot_id: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    pub(super) uploads: Arc<tokio::sync::Semaphore>,
}

impl DiscordBotProfile {
    pub fn new(
        token: &str,
        bot_id: impl Fn() -> Option<String> + Send + Sync + 'static,
    ) -> Option<Self> {
        if token.is_empty() {
            return None;
        }
        let mut auth = HeaderValue::from_str(&format!("Bot {token}")).ok()?;
        auth.set_sensitive(true);
        Some(Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .build()
                .ok()?,
            auth,
            bot_id: Arc::new(bot_id),
            uploads: Arc::new(tokio::sync::Semaphore::new(2)),
        })
    }

    pub(super) async fn read(&self, guild: &str) -> Result<Value, ProfileError> {
        let bot = (self.bot_id)().ok_or(ProfileError::Unavailable)?;
        let member = self
            .client
            .get(format!("{DISCORD_API}/guilds/{guild}/members/{bot}"))
            .header("Authorization", self.auth.clone())
            .send()
            .await
            .map_err(|_| ProfileError::Unavailable)?;
        let member = read_member(member).await?;
        let user = &member["user"];
        let mut profile = profile_fields(&member);
        profile["botId"] = json!(bot);
        profile["defaultName"] = json!(user["username"].as_str().unwrap_or("Vozen TTS"));
        profile["defaultAvatar"] = user["avatar"].clone();
        Ok(profile)
    }

    pub(super) async fn update(
        &self,
        guild: &str,
        edit: ProfileEdit,
    ) -> Result<Value, ProfileError> {
        let patch = tokio::task::spawn_blocking(move || edit.into_patch())
            .await
            .map_err(|_| ProfileError::Unavailable)??;
        let member = self
            .client
            .patch(format!("{DISCORD_API}/guilds/{guild}/members/@me"))
            .header("Authorization", self.auth.clone())
            .json(&patch)
            .send()
            .await
            .map_err(|_| ProfileError::Unavailable)?;
        Ok(profile_fields(&read_member(member).await?))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ProfileError {
    InvalidName,
    InvalidImage,
    InvalidEdit,
    MissingPermission,
    Busy,
    Unavailable,
}

impl ProfileError {
    pub(super) fn response(self) -> (StatusCode, &'static str) {
        match self {
            Self::InvalidName => (StatusCode::BAD_REQUEST, "invalid_name"),
            Self::InvalidImage => (StatusCode::BAD_REQUEST, "invalid_image"),
            Self::InvalidEdit => (StatusCode::BAD_REQUEST, "invalid_profile"),
            Self::MissingPermission => (StatusCode::FORBIDDEN, "discord_permission"),
            Self::Busy => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            Self::Unavailable => (StatusCode::BAD_GATEWAY, "discord_unavailable"),
        }
    }
}

async fn read_member(response: reqwest::Response) -> Result<Value, ProfileError> {
    match response.status() {
        StatusCode::OK => response.json().await.map_err(|_| ProfileError::Unavailable),
        StatusCode::FORBIDDEN => Err(ProfileError::MissingPermission),
        StatusCode::TOO_MANY_REQUESTS => Err(ProfileError::Busy),
        _ => Err(ProfileError::Unavailable),
    }
}

fn profile_fields(member: &Value) -> Value {
    json!({
        "nick": member["nick"],
        "bio": member["bio"],
        "avatar": member["avatar"],
        "banner": member["banner"],
    })
}

impl ProfileEdit {
    pub(super) fn changes_images(&self) -> bool {
        self.reset || self.avatar.is_some() || self.banner.is_some()
    }

    fn into_patch(self) -> Result<Value, ProfileError> {
        let mut patch = Map::new();
        if self.reset {
            if self.nick.is_some() || self.avatar.is_some() || self.banner.is_some() {
                return Err(ProfileError::InvalidEdit);
            }
            return Ok(json!({"nick": null, "avatar": null, "banner": null}));
        }
        if let Some(nick) = self.nick {
            validate_name(&nick)?;
            patch.insert(
                "nick".into(),
                if nick.is_empty() {
                    Value::Null
                } else {
                    json!(nick)
                },
            );
        }
        for (key, image) in [("avatar", self.avatar), ("banner", self.banner)] {
            if let Some(image) = image {
                patch.insert(
                    key.into(),
                    json!(profile_image(&image).map_err(|_| ProfileError::InvalidImage)?),
                );
            }
        }
        if patch.is_empty() {
            return Err(ProfileError::InvalidEdit);
        }
        Ok(Value::Object(patch))
    }
}

fn validate_name(name: &str) -> Result<(), ProfileError> {
    if name.chars().count() > 32
        || (!name.is_empty() && name.trim().is_empty())
        || name.chars().any(char::is_control)
    {
        return Err(ProfileError::InvalidName);
    }
    let policy: Value = serde_json::from_str(PROFILE_POLICY).expect("bundled profile policy");
    let compact = normalized_profile_text(name);
    let blocked = policy["blocked_terms"]
        .as_array()
        .unwrap()
        .iter()
        .any(|term| compact.contains(term.as_str().unwrap()))
        || policy["blocked_words"]
            .as_array()
            .unwrap()
            .iter()
            .any(|term| {
                let term = term.as_str().unwrap();
                compact == term
                    || name
                        .split(|c: char| !c.is_alphanumeric())
                        .any(|word| normalized_profile_text(word) == term)
            });
    if blocked {
        Err(ProfileError::InvalidName)
    } else {
        Ok(())
    }
}

fn normalized_profile_text(value: &str) -> String {
    value
        .chars()
        .flat_map(char::to_lowercase)
        .filter_map(|c| {
            let c = match c {
                '\u{ff01}'..='\u{ff5e}' => char::from_u32(c as u32 - 0xfee0).unwrap(),
                other => other,
            };
            Some(match c {
                '0' | 'ο' | 'о' | 'ò' | 'ó' | 'ô' | 'õ' | 'ö' => 'o',
                '1' | '!' | 'і' | 'í' | 'ì' | 'î' | 'ï' => 'i',
                '3' | 'е' | 'é' | 'è' | 'ê' | 'ë' => 'e',
                '4' | '@' | 'а' | 'à' | 'á' | 'â' | 'ã' | 'ä' => 'a',
                '5' | '$' => 's',
                '7' | 'τ' | 'т' => 't',
                'ç' | 'с' => 'c',
                'ù' | 'ú' | 'û' | 'ü' => 'u',
                'ñ' => 'n',
                'ɡ' => 'g',
                c if c.is_ascii_alphanumeric() => c,
                _ => return None,
            })
        })
        .collect()
}

fn animated_image(bytes: &[u8]) -> bool {
    let png = bytes.starts_with(b"\x89PNG\r\n\x1a\n");
    let webp = bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP";
    if !png && !webp {
        return false;
    }
    let mut offset = if png { 8 } else { 12 };
    while offset + 12 <= bytes.len() {
        let (kind, length) = if png {
            (
                &bytes[offset + 4..offset + 8],
                u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize,
            )
        } else {
            (
                &bytes[offset..offset + 4],
                u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize,
            )
        };
        if png && kind == b"acTL" || webp && (kind == b"ANIM" || kind == b"ANMF") {
            return true;
        }
        let overhead = if png { 12 } else { 8 + (length % 2) };
        let Some(next) = offset
            .checked_add(length)
            .and_then(|n| n.checked_add(overhead))
        else {
            return false;
        };
        offset = next;
    }
    false
}

fn profile_image(data: &str) -> Result<String, Box<dyn std::error::Error>> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    if data.len() > 2_800_000 {
        return Err("Image exceeds 2 MB.".into());
    }
    let (prefix, encoded) = data
        .split_once(",")
        .filter(|(p, _)| {
            matches!(
                *p,
                "data:image/png;base64" | "data:image/jpeg;base64" | "data:image/webp;base64"
            )
        })
        .ok_or("Upload a PNG, JPEG or WebP image.")?;
    let bytes = STANDARD.decode(encoded)?;
    if bytes.len() > 2 * 1024 * 1024 || animated_image(&bytes) {
        return Err("Choose a static image up to 2 MB.".into());
    }
    let format = match prefix {
        "data:image/png;base64" => image::ImageFormat::Png,
        "data:image/jpeg;base64" => image::ImageFormat::Jpeg,
        _ => image::ImageFormat::WebP,
    };
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader.decode()?;
    let mut png = std::io::Cursor::new(Vec::new());
    decoded.write_to(&mut png, image::ImageFormat::Png)?;
    if png.get_ref().len() > 2 * 1024 * 1024 {
        return Err("The processed image exceeds 2 MB. Use a smaller image.".into());
    }
    Ok(format!(
        "data:image/png;base64,{}",
        STANDARD.encode(png.into_inner())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::STANDARD};

    fn patch(json: Value) -> Result<Value, ProfileError> {
        serde_json::from_value::<ProfileEdit>(json)
            .unwrap()
            .into_patch()
    }

    #[test]
    fn profile_edits_touch_only_requested_fields_and_preserve_bio() {
        assert_eq!(
            patch(json!({"nick":"Vozen QA"})),
            Ok(json!({"nick":"Vozen QA"}))
        );
        assert_eq!(patch(json!({"nick":""})), Ok(json!({"nick":null})));
        assert_eq!(
            patch(json!({"reset":true})),
            Ok(json!({"nick":null,"avatar":null,"banner":null}))
        );
        assert_eq!(
            patch(json!({"reset":true,"nick":"Mixed"})),
            Err(ProfileError::InvalidEdit)
        );
        assert!(serde_json::from_value::<ProfileEdit>(json!({"bio":"edited"})).is_err());
        assert_eq!(
            patch(json!({"nick":"a\nb"})),
            Err(ProfileError::InvalidName)
        );
        assert_eq!(
            patch(json!({"nick":"a".repeat(33)})),
            Err(ProfileError::InvalidName)
        );
    }

    #[test]
    fn profile_images_are_decoded_and_reencoded_under_limits() {
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(2, 2)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let encoded = STANDARD.encode(png.into_inner());
        let image = format!("data:image/png;base64,{encoded}");
        let valid = patch(json!({"avatar":image})).unwrap();
        assert!(
            valid["avatar"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert_eq!(
            patch(json!({"avatar":format!("data:image/jpeg;base64,{encoded}")})),
            Err(ProfileError::InvalidImage)
        );
        assert_eq!(
            patch(json!({"banner":"data:image/png;base64,broken"})),
            Err(ProfileError::InvalidImage)
        );
        assert_eq!(
            patch(json!({"banner":"a".repeat(2_800_001)})),
            Err(ProfileError::InvalidImage)
        );
    }
}
