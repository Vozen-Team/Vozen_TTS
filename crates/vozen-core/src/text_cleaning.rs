//! Text normalisation used before any message enters TTS.
//!
//! This mirrors the pure Node cleaning contract: protected Discord content is not read aloud,
//! custom emoji names are readable, and truncation never emits half of a UTF-16 surrogate pair.

use std::sync::LazyLock;

use regex::{Captures, Regex};
use unicode_normalization::UnicodeNormalization;

static RE_CODE_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)```.*?```").expect("valid code-block regex"));
static RE_INLINE_CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"`[^`]*`").expect("valid inline-code regex"));
static RE_SPOILER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)\|\|.*?\|\|").expect("valid spoiler regex"));
static RE_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(https?://[^\s]+|www\.[^\s]+)").expect("valid URL regex"));
static RE_ROLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<@&\d+>").expect("valid role regex"));
static RE_USER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<@!?(\d+)>").expect("valid user regex"));
static RE_CHANNEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<#(\d+)>").expect("valid channel regex"));
static RE_CUSTOM_EMOJI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<a?:(\w+):\d+>").expect("valid custom emoji regex"));
static RE_EXTENDED_PICTOGRAPHIC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\p{Extended_Pictographic}").expect("valid emoji property regex"));

pub struct CleanTextOptions<'a> {
    /// Maximum UTF-16 code units, retaining the current Node API semantics.
    pub max_chars: usize,
    pub resolve_user: &'a dyn Fn(&str) -> String,
    pub resolve_channel: &'a dyn Fn(&str) -> String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Link,
    Gif,
    Spoiler,
    Code,
}

/// Removes content that must not be spoken and converts Discord syntax into readable text.
pub fn clean_text(raw: &str, options: &CleanTextOptions<'_>) -> String {
    let normalized = normalize_input(raw);
    let mut text = strip_protected(&normalized);
    text = RE_URL.replace_all(&text, " ").into_owned();
    text = RE_ROLE.replace_all(&text, " ").into_owned();
    text = RE_USER
        .replace_all(&text, |captures: &Captures<'_>| {
            (options.resolve_user)(&captures[1])
        })
        .into_owned();
    text = RE_CHANNEL
        .replace_all(&text, |captures: &Captures<'_>| {
            (options.resolve_channel)(&captures[1])
        })
        .into_owned();
    text = RE_CUSTOM_EMOJI
        .replace_all(&text, |captures: &Captures<'_>| {
            format!(" {} ", captures[1].replace('_', " "))
        })
        .into_owned();
    text = RE_EXTENDED_PICTOGRAPHIC
        .replace_all(&text, " ")
        .into_owned();
    text.retain(|character| !is_emoji_component(character));
    // Discord users often decorate words with Mathematical Alphanumeric Symbols (for example
    // `𝐌𝐈𝐂𝐎𝐍`). Those code points look like letters but many TTS engines read their compatibility
    // forms as digits or punctuation. NFKC folds them back to their ordinary letters while keeping
    // accents and normal Unicode text intact.
    text = text.nfkc().collect();
    let collapsed = collapse_repetitions(&text);
    truncate_utf16(&collapse_whitespace(&collapsed), options.max_chars)
}

/// Collects URL announcements after excluding spoilers and code, in the same order as `clean_text`.
pub fn collect_url_media(raw: &str) -> Vec<MediaKind> {
    let body = strip_protected(&normalize_input(raw));
    RE_URL
        .find_iter(&body)
        .map(|matched| {
            if is_gif_url(matched.as_str()) {
                MediaKind::Gif
            } else {
                MediaKind::Link
            }
        })
        .collect()
}

/// Collects spoiler/code announcements while ensuring nested code inside a spoiler is only counted once.
pub fn collect_markdown_media(raw: &str) -> Vec<MediaKind> {
    let normalized = normalize_input(raw);
    let (masked, code) = mask_code(&normalized);
    let spoilers = RE_SPOILER
        .find_iter(&masked)
        .map(|matched| matched.range())
        .collect::<Vec<_>>();
    let outside_spoilers = code
        .iter()
        .filter(|range| {
            !spoilers
                .iter()
                .any(|spoiler| spoiler.start <= range.start && range.end <= spoiler.end)
        })
        .count();
    let mut output = vec![MediaKind::Spoiler; spoilers.len()];
    output.extend(std::iter::repeat_n(MediaKind::Code, outside_spoilers));
    output
}

/// Drops invisible format characters and folds compatibility forms before any pattern runs, so
/// neither a full-width URL nor a zero-width space inside a blocked word slips past a filter.
fn normalize_input(raw: &str) -> String {
    raw.chars()
        .filter(|character| !is_invisible_format(*character))
        .collect::<String>()
        .nfkc()
        .collect()
}

/// Discord renders code literally, so `||` inside code is not a spoiler marker. Code is blanked
/// with the same number of spaces, keeping byte offsets valid for the spoiler search, and the
/// blanked ranges are returned in order.
fn mask_code(text: &str) -> (String, Vec<std::ops::Range<usize>>) {
    let mut masked = text.to_owned();
    let mut ranges = Vec::new();
    for pattern in [&*RE_CODE_BLOCK, &*RE_INLINE_CODE] {
        let found = pattern
            .find_iter(&masked)
            .map(|matched| matched.range())
            .collect::<Vec<_>>();
        for range in found {
            masked.replace_range(range.clone(), &" ".repeat(range.len()));
            ranges.push(range);
        }
    }
    ranges.sort_by_key(|range| range.start);
    (masked, ranges)
}

/// Removes code blocks, inline code and spoilers, with code taking precedence as in Discord.
fn strip_protected(text: &str) -> String {
    let (masked, _) = mask_code(text);
    RE_SPOILER.replace_all(&masked, " ").into_owned()
}

/// Zero-width and directional format characters that render as nothing. ZWNJ (U+200C) is kept
/// because it changes how Persian and Indic words are written.
fn is_invisible_format(character: char) -> bool {
    matches!(
        character,
        '\u{00ad}'
            | '\u{200b}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{feff}'
    )
}

fn is_gif_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    (lower.contains(".gif") && gif_extension_boundary(&lower))
        || lower.contains("tenor.com")
        || lower.contains("giphy.com")
}

fn gif_extension_boundary(url: &str) -> bool {
    url.match_indices(".gif").any(|(index, _)| {
        url[index + 4..]
            .chars()
            .next()
            // JavaScript's `\b` in the legacy implementation uses ASCII `\w`.
            // Keep that boundary here so URLs such as `.gifé` behave identically.
            .is_none_or(|character| !character.is_ascii_alphanumeric() && character != '_')
    })
}

fn is_emoji_component(character: char) -> bool {
    matches!(character, '\u{200d}' | '\u{fe0f}' | '\u{20e3}')
        || ('\u{1f1e6}'..='\u{1f1ff}').contains(&character)
}

fn collapse_repetitions(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut previous = None;
    let mut run = 0usize;
    for character in input.chars() {
        if previous == Some(character) {
            run += 1;
        } else {
            previous = Some(character);
            run = 1;
        }
        let cap = if character.is_lowercase() {
            3
        } else if character.is_uppercase() {
            2
        } else {
            usize::MAX
        };
        if run <= cap {
            output.push(character);
        }
    }
    output
}

fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_utf16(input: &str, max_chars: usize) -> String {
    let mut output = String::new();
    let mut used = 0usize;
    for character in input.chars() {
        let width = character.len_utf16();
        if used.saturating_add(width) > max_chars {
            break;
        }
        output.push(character);
        used += width;
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(max_chars: usize) -> CleanTextOptions<'static> {
        CleanTextOptions {
            max_chars,
            resolve_user: &|id| format!("@user{id}"),
            resolve_channel: &|id| format!("#channel{id}"),
        }
    }

    #[test]
    fn cleans_discord_syntax_without_speaking_private_or_raw_content() {
        assert_eq!(
            clean_text(
                "hello ||secret https://example.com|| <@&7> <@!8> <#9> <:party_blob:10> https://tenor.com/x",
                &options(200),
            ),
            "hello @user8 #channel9 party blob"
        );
        assert_eq!(
            clean_text("before ```<@8>``` then `npm install`", &options(200)),
            "before then"
        );
    }

    #[test]
    fn preserves_the_legacy_cleaning_order_for_urls_and_markdown() {
        assert_eq!(
            clean_text("vai a https://exemplo.com agora", &options(200)),
            "vai a agora"
        );
        assert_eq!(clean_text("ve www.exemplo.com ja", &options(200)), "ve ja");
        assert_eq!(
            clean_text("olha ||segredo grande|| aqui", &options(200)),
            "olha aqui"
        );
        assert_eq!(
            clean_text("antes ```const x = 1;``` depois", &options(200)),
            "antes depois"
        );
        assert_eq!(
            clean_text("**negrito com `code` dentro**", &options(200)),
            "**negrito com dentro**"
        );
        assert_eq!(
            clean_text("antes ```\\n<@123>\\n``` depois", &options(200)),
            "antes depois"
        );
    }

    #[test]
    fn announces_media_without_double_counting_urls_inside_protected_markdown() {
        assert_eq!(
            collect_url_media("`https://tenor.com/x` https://example.com https://giphy.com/y"),
            vec![MediaKind::Link, MediaKind::Gif]
        );
        assert_eq!(
            collect_url_media("https://example.com/gift.gif"),
            vec![MediaKind::Gif]
        );
        assert_eq!(
            collect_markdown_media("||`secret`|| then ```code``` and `inline`"),
            vec![MediaKind::Spoiler, MediaKind::Code, MediaKind::Code]
        );
        assert_eq!(
            collect_url_media("ve www.exemplo.com ja"),
            vec![MediaKind::Link]
        );
        assert_eq!(
            collect_url_media("olha https://tenor.com/view/cat-gif-12345 lol"),
            vec![MediaKind::Gif]
        );
        assert_eq!(
            collect_url_media("a https://x.com b https://tenor.com/view/y c"),
            vec![MediaKind::Link, MediaKind::Gif]
        );
        assert!(collect_url_media("||https://exemplo.com||").is_empty());
        assert!(collect_markdown_media("mensagem normal").is_empty());
    }

    #[test]
    fn code_that_shows_spoiler_syntax_does_not_unhide_a_real_spoiler() {
        let message = "use `||` to hide, like ||ending||";
        let spoken = clean_text(message, &options(200));
        assert!(
            spoken.contains("use") && spoken.contains("hide"),
            "{spoken}"
        );
        assert!(!spoken.contains("ending"), "{spoken}");
        assert_eq!(
            collect_markdown_media(message),
            vec![MediaKind::Spoiler, MediaKind::Code]
        );
        assert_eq!(
            collect_markdown_media("||secret `x`||"),
            vec![MediaKind::Spoiler]
        );
    }

    #[test]
    fn full_width_links_are_removed_like_ordinary_links() {
        let message = "see ｈｔｔｐｓ://example.com/x now";
        let spoken = clean_text(message, &options(200));
        assert!(!spoken.contains("example"), "{spoken}");
        assert_eq!(collect_url_media(message), vec![MediaKind::Link]);
    }

    #[test]
    fn invisible_characters_cannot_split_a_word_but_zwnj_survives() {
        assert_eq!(
            clean_text("bad\u{200B}word", &options(200)),
            clean_text("badword", &options(200))
        );
        assert_eq!(
            clean_text("bad\u{00AD}word", &options(200)),
            clean_text("badword", &options(200))
        );
        assert!(clean_text("می\u{200C}خواهم", &options(200)).contains('\u{200C}'));
    }

    #[test]
    fn removes_emoji_components_and_collapses_unicode_repetition() {
        assert_eq!(clean_text("❤️ 👨‍💻 1️⃣ 🇵🇹", &options(200)), "1");
        assert_eq!(clean_text("ááááá ÇÇÇÇ", &options(200)), "ááá ÇÇ");
        assert_eq!(clean_text("😀 <a:dance:1>", &options(200)), "dance");
        assert_eq!(
            clean_text("boa <:pog:789> festa", &options(200)),
            "boa pog festa"
        );
        assert_eq!(
            clean_text("aaaaaa WWWW aa BB", &options(200)),
            "aaa WW aa BB"
        );
        assert_eq!(
            clean_text("😀 https://example.com <@123>", &options(200)),
            "@user123"
        );
        assert_eq!(
            clean_text("check <@&999> <:fire:123> at https://x.com", &options(200)),
            "check fire at"
        );
    }

    #[test]
    fn folds_decorative_unicode_letters_before_tts() {
        assert_eq!(
            clean_text("𝐌𝐈𝐂𝐎𝐍 𝕋𝕋𝕊 𝙑𝙤𝙯𝙚𝙣", &options(200)),
            "MICON TTS Vozen"
        );
        assert_eq!(
            clean_text("Ｆｕｌｌｗｉｄｔｈ ① ²", &options(200)),
            "Fullwidth 1 2"
        );
    }

    #[test]
    fn truncates_at_utf16_boundary_without_a_lone_surrogate() {
        assert_eq!(clean_text("ab𠀀𠀀", &options(5)), "ab𠀀");
        assert_eq!(clean_text("hello", &options(0)), "");
        assert_eq!(clean_text(&"abcd".repeat(13), &options(10)), "abcdabcdab");
        assert_eq!(clean_text("café açaí", &options(200)), "café açaí");
        assert_eq!(clean_text("こんにちは", &options(200)), "こんにちは");
        assert_eq!(clean_text("a𠀀b", &options(200)), "a𠀀b");
        assert!(clean_text("   ", &options(200)).is_empty());
        assert!(clean_text("```apenas codigo```", &options(200)).is_empty());
    }
}
