//! Deterministic last-mile preparation of cleaned Discord text for a TTS provider.
//!
//! This module deliberately has no Discord, database, provider or language-model dependency.
//! The adapter supplies the already-detected ISO language only when a user opted into automatic
//! detection; the default remains a fixed, explicitly selected voice.

use crate::speech_safety::apply_pronunciation_checked;
use crate::{
    PronunciationEntry, SpeechSegment, SynthRequest, SynthesisEngine, accent_language_of_model,
    cap_synth_request, expand_abbreviations, pick_voice_for_language, restore_accents,
    split_english_slang,
};

#[derive(Debug, Clone, PartialEq)]
pub struct VoicePreference {
    pub model: String,
    pub speed: f64,
    pub engine: SynthesisEngine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaAnnouncementKind {
    Link,
    Gif,
    Image,
    Video,
    Audio,
    File,
    Archive,
    Multiple,
    Sticker,
    Spoiler,
    Code,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaAnnouncement {
    pub kind: MediaAnnouncementKind,
    /// Used only for stickers. An empty label falls back to the localised noun.
    pub text: Option<String>,
}

pub struct SpeechPreparationInput<'a> {
    /// Text after user-specific substitutions from the caller.
    pub personal: &'a str,
    pub pronunciations: &'a [PronunciationEntry],
    pub user_voice: Option<&'a VoicePreference>,
    pub available_models: &'a [String],
    pub guild_default_voice: Option<&'a str>,
    pub channel_engine: Option<SynthesisEngine>,
    pub default_voice: &'a str,
    pub default_speed: f64,
    pub default_engine: SynthesisEngine,
    /// This is false by default. The Discord adapter must never infer consent from a message.
    pub auto_detect: bool,
    /// ISO 639-3 result produced by the opted-in detector. `None` fails safely to the preferred
    /// voice without guessing the language.
    pub detected_language: Option<&'a str>,
    pub announce_speaker: Option<&'a str>,
    pub media: &'a [MediaAnnouncement],
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedSpeech {
    /// Uncapped text, kept for blocklist evaluation before synthesis.
    pub spoken: String,
    pub request: SynthRequest,
}

/// Prepares one request preserving the legacy precedence:
/// personal substitutions (upstream) > stored pronunciations > built-in English slang.
pub fn prepare_speech(input: SpeechPreparationInput<'_>) -> PreparedSpeech {
    let speed = input
        .user_voice
        .map_or(input.default_speed, |voice| voice.speed);
    let preferred = preferred_model(&input);
    let engine = preferred_engine(&input);
    let pronounced = apply_pronunciation_checked(input.personal, input.pronunciations);
    let rejected_expansion = pronounced.is_none();
    let pronounced = pronounced.unwrap_or_default();

    let mut prepared = if !input.auto_detect {
        let spoken = restore_accents(
            &expand_abbreviations(&pronounced),
            accent_language_of_model(&preferred),
        );
        PreparedSpeech {
            request: SynthRequest {
                text: spoken.clone(),
                model: preferred,
                asset_path: None,
                speed,
                engine,
                gcloud_budget: None,
                segments: None,
                single_voice: Some(true),
                emphasis_source: Some(spoken.clone()),
                lead_silence_ms: 0,
            },
            spoken,
        }
    } else {
        prepare_detected(
            &pronounced,
            input.detected_language,
            input.available_models,
            &preferred,
            speed,
            engine,
        )
    };

    if !rejected_expansion {
        decorate_announcements(&mut prepared, input.announce_speaker, input.media);
    }
    prepared.request = cap_synth_request(&prepared.request);
    prepared
}

fn preferred_model(input: &SpeechPreparationInput<'_>) -> String {
    let configured = [
        input.user_voice.map(|voice| voice.model.as_str()),
        input.guild_default_voice.filter(|model| !model.is_empty()),
        (!input.default_voice.is_empty()).then_some(input.default_voice),
    ];
    configured
        .into_iter()
        .flatten()
        .find(|model| {
            input
                .available_models
                .iter()
                .any(|available| available == *model)
        })
        .map(str::to_owned)
        .or_else(|| input.available_models.first().cloned())
        .or_else(|| configured.into_iter().flatten().next().map(str::to_owned))
        .unwrap_or_else(|| "en_US-amy-medium".to_owned())
}

fn preferred_engine(input: &SpeechPreparationInput<'_>) -> SynthesisEngine {
    input
        .user_voice
        .map(|voice| voice.engine)
        .or(input.channel_engine)
        .unwrap_or(input.default_engine)
}

fn prepare_detected(
    pronounced: &str,
    detected_language: Option<&str>,
    available_models: &[String],
    preferred: &str,
    speed: f64,
    engine: SynthesisEngine,
) -> PreparedSpeech {
    let raw_segments = split_english_slang(pronounced);
    let base_language = detected_language.unwrap_or("");
    let segments: Vec<_> = raw_segments
        .iter()
        .map(|segment| {
            let language = if segment.is_english {
                "eng"
            } else {
                base_language
            };
            let text = restore_accents(&expand_abbreviations(&segment.text), language);
            SpeechSegment {
                text,
                model: pick_voice_for_language(language, available_models, preferred),
            }
        })
        .collect();
    let spoken = segments
        .iter()
        .map(|segment| segment.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let has_english = raw_segments.iter().any(|segment| segment.is_english);
    let has_other = raw_segments.iter().any(|segment| !segment.is_english);
    let base_model = pick_voice_for_language(base_language, available_models, preferred);
    let model = if has_english && !has_other {
        pick_voice_for_language("eng", available_models, preferred)
    } else {
        base_model
    };
    PreparedSpeech {
        request: SynthRequest {
            text: spoken.clone(),
            model,
            asset_path: None,
            speed,
            engine,
            gcloud_budget: None,
            segments: (has_english && has_other).then_some(segments),
            single_voice: None,
            emphasis_source: Some(spoken.clone()),
            lead_silence_ms: 0,
        },
        spoken,
    }
}

fn decorate_announcements(
    prepared: &mut PreparedSpeech,
    speaker: Option<&str>,
    media: &[MediaAnnouncement],
) {
    let phrases = phrases_for_model(&prepared.request.model);
    let prefix = speaker
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| format!("{name} {}", phrases.said));
    let suffix = media
        .iter()
        .map(|item| media_phrase(item, phrases))
        .filter(|item| !item.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if prefix.is_none() && suffix.is_empty() {
        return;
    }
    prepared.spoken = [
        prefix.as_deref(),
        Some(prepared.spoken.as_str()),
        (!suffix.is_empty()).then_some(suffix.as_str()),
    ]
    .into_iter()
    .flatten()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(" ");
    prepared.request.text = prepared.spoken.clone();
    if let Some(segments) = prepared.request.segments.as_mut() {
        let model = prepared.request.model.clone();
        let mut decorated = Vec::new();
        if let Some(prefix) = prefix {
            decorated.push(SpeechSegment {
                text: prefix,
                model: model.clone(),
            });
        }
        decorated.append(segments);
        if !suffix.is_empty() {
            decorated.push(SpeechSegment {
                text: suffix,
                model,
            });
        }
        *segments = decorated;
    }
}

struct Phrases {
    said: &'static str,
    link: &'static str,
    gif: &'static str,
    image: &'static str,
    video: &'static str,
    audio: &'static str,
    file: &'static str,
    archive: &'static str,
    multiple: &'static str,
    sticker: &'static str,
    spoiler: &'static str,
    code: &'static str,
}

const EN: Phrases = Phrases {
    said: "said",
    link: "a link",
    gif: "a gif",
    image: "an image",
    video: "a video",
    audio: "an audio",
    file: "a file",
    archive: "a compressed file",
    multiple: "multiple files",
    sticker: "a sticker",
    spoiler: "spoiler",
    code: "code",
};
const PT: Phrases = Phrases {
    said: "disse",
    link: "um link",
    gif: "um gif",
    image: "uma imagem",
    video: "um vídeo",
    audio: "um áudio",
    file: "um arquivo",
    archive: "um arquivo compactado",
    multiple: "vários arquivos",
    sticker: "uma figurinha",
    spoiler: "spoiler",
    code: "código",
};
const ES: Phrases = Phrases {
    said: "dijo",
    link: "un enlace",
    gif: "un gif",
    image: "una imagen",
    video: "un vídeo",
    audio: "un audio",
    file: "un archivo",
    archive: "un archivo comprimido",
    multiple: "varios archivos",
    sticker: "un sticker",
    spoiler: "spoiler",
    code: "código",
};
const FR: Phrases = Phrases {
    said: "a dit",
    link: "un lien",
    gif: "un gif",
    image: "une image",
    video: "une vidéo",
    audio: "un audio",
    file: "un fichier",
    archive: "un fichier compressé",
    multiple: "plusieurs fichiers",
    sticker: "un sticker",
    spoiler: "spoiler",
    code: "du code",
};
const DE: Phrases = Phrases {
    said: "sagt",
    link: "ein Link",
    gif: "ein GIF",
    image: "ein Bild",
    video: "ein Video",
    audio: "eine Audiodatei",
    file: "eine Datei",
    archive: "eine komprimierte Datei",
    multiple: "mehrere Dateien",
    sticker: "ein Sticker",
    spoiler: "Spoiler",
    code: "Code",
};
const IT: Phrases = Phrases {
    said: "ha detto",
    link: "un link",
    gif: "una gif",
    image: "un'immagine",
    video: "un video",
    audio: "un audio",
    file: "un file",
    archive: "un file compresso",
    multiple: "più file",
    sticker: "uno sticker",
    spoiler: "spoiler",
    code: "codice",
};
const NL: Phrases = Phrases {
    said: "zei",
    link: "een link",
    gif: "een gif",
    image: "een afbeelding",
    video: "een video",
    audio: "een audio",
    file: "een bestand",
    archive: "een gecomprimeerd bestand",
    multiple: "meerdere bestanden",
    sticker: "een sticker",
    spoiler: "spoiler",
    code: "code",
};
const PL: Phrases = Phrases {
    said: "powiedział",
    link: "link",
    gif: "gif",
    image: "obraz",
    video: "wideo",
    audio: "audio",
    file: "plik",
    archive: "skompresowany plik",
    multiple: "wiele plików",
    sticker: "naklejka",
    spoiler: "spoiler",
    code: "kod",
};
const RU: Phrases = Phrases {
    said: "сказал",
    link: "ссылка",
    gif: "гиф",
    image: "изображение",
    video: "видео",
    audio: "аудио",
    file: "файл",
    archive: "сжатый файл",
    multiple: "несколько файлов",
    sticker: "стикер",
    spoiler: "спойлер",
    code: "код",
};

fn phrases_for_model(model: &str) -> &'static Phrases {
    match model.split_once('_').map(|(language, _)| language) {
        Some("pt") => &PT,
        Some("es") => &ES,
        Some("fr") => &FR,
        Some("de") => &DE,
        Some("it") => &IT,
        Some("nl") => &NL,
        Some("pl") => &PL,
        Some("ru") => &RU,
        _ => &EN,
    }
}

fn media_phrase(item: &MediaAnnouncement, phrases: &Phrases) -> String {
    match item.kind {
        MediaAnnouncementKind::Link => phrases.link,
        MediaAnnouncementKind::Gif => phrases.gif,
        MediaAnnouncementKind::Image => phrases.image,
        MediaAnnouncementKind::Video => phrases.video,
        MediaAnnouncementKind::Audio => phrases.audio,
        MediaAnnouncementKind::File => phrases.file,
        MediaAnnouncementKind::Archive => phrases.archive,
        MediaAnnouncementKind::Multiple => phrases.multiple,
        MediaAnnouncementKind::Sticker => item
            .text
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or(phrases.sticker),
        MediaAnnouncementKind::Spoiler => phrases.spoiler,
        MediaAnnouncementKind::Code => phrases.code,
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn models() -> Vec<String> {
        [
            "en_US-amy-medium",
            "pt_PT-google-medium",
            "es_ES-davefx-medium",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    fn input<'a>(personal: &'a str, available_models: &'a [String]) -> SpeechPreparationInput<'a> {
        SpeechPreparationInput {
            personal,
            pronunciations: &[],
            user_voice: None,
            available_models,
            guild_default_voice: None,
            channel_engine: None,
            default_voice: "en_US-amy-medium",
            default_speed: 1.0,
            default_engine: SynthesisEngine::Piper,
            auto_detect: false,
            detected_language: None,
            announce_speaker: None,
            media: &[],
        }
    }

    #[test]
    fn fixed_voice_has_priority_and_preserves_the_anti_detection_default() {
        let available = models();
        let prepared = prepare_speech(input("nao brb", &available));
        assert_eq!(prepared.request.model, "en_US-amy-medium");
        assert_eq!(prepared.request.single_voice, Some(true));
        assert_eq!(prepared.spoken, "nao be right back");

        let voice = VoicePreference {
            model: "pt_PT-google-medium".into(),
            speed: 1.25,
            engine: SynthesisEngine::Kokoro,
        };
        let mut preferred = input("nao voce", &available);
        preferred.user_voice = Some(&voice);
        let prepared = prepare_speech(preferred);
        assert_eq!(prepared.request.model, voice.model);
        assert_eq!(prepared.request.speed, 1.25);
        assert_eq!(prepared.request.engine, SynthesisEngine::Kokoro);
        assert_eq!(prepared.spoken, "não você");
    }

    #[test]
    fn auto_detection_is_explicit_and_keeps_english_slang_in_its_own_voice() {
        let available = models();
        let mut detected = input("nao btw", &available);
        detected.auto_detect = true;
        detected.detected_language = Some("por");
        let prepared = prepare_speech(detected);
        assert_eq!(prepared.spoken, "não by the way");
        assert_eq!(prepared.request.model, "pt_PT-google-medium");
        assert_eq!(prepared.request.single_voice, None);
        assert_eq!(
            prepared.request.segments.as_ref().expect("mixed")[1].model,
            "en_US-amy-medium"
        );
    }

    #[test]
    fn rejected_pronunciation_is_not_revived_by_speaker_or_media_announcements() {
        let available = models();
        let dictionary = [PronunciationEntry {
            term: "a".into(),
            replacement: "b".repeat(64 * 1024 + 1),
        }];
        let media = [MediaAnnouncement {
            kind: MediaAnnouncementKind::Gif,
            text: None,
        }];
        for auto_detect in [false, true] {
            let mut excessive = input("a", &available);
            excessive.pronunciations = &dictionary;
            excessive.announce_speaker = Some("Alice");
            excessive.media = &media;
            excessive.auto_detect = auto_detect;
            let prepared = prepare_speech(excessive);
            assert!(prepared.spoken.is_empty());
            assert!(prepared.request.text.is_empty());
            assert!(
                prepared
                    .request
                    .emphasis_source
                    .as_deref()
                    .is_none_or(str::is_empty)
            );
        }
        let mut media_only = input("", &available);
        media_only.media = &media;
        assert!(!prepare_speech(media_only).spoken.is_empty());
    }

    #[test]
    fn announcements_are_localised_without_affecting_emphasis_source_or_request_budget() {
        let available = models();
        let media = [MediaAnnouncement {
            kind: MediaAnnouncementKind::Gif,
            text: None,
        }];
        let voice = VoicePreference {
            model: "pt_PT-google-medium".into(),
            speed: 1.0,
            engine: SynthesisEngine::Piper,
        };
        let mut with_announcements = input("ola", &available);
        with_announcements.user_voice = Some(&voice);
        with_announcements.announce_speaker = Some("DIOGO");
        with_announcements.media = &media;
        let prepared = prepare_speech(with_announcements);
        assert_eq!(prepared.spoken, "DIOGO disse ola um gif");
        assert_eq!(prepared.request.emphasis_source.as_deref(), Some("ola"));
    }

    #[test]
    fn xsaid_text_is_identical_for_google_piper_and_kokoro_requests() {
        let available = models();
        for engine in [
            SynthesisEngine::Default,
            SynthesisEngine::Piper,
            SynthesisEngine::Kokoro,
        ] {
            let voice = VoicePreference {
                model: "en_US-amy-medium".into(),
                speed: 1.0,
                engine,
            };
            let mut input = input("hello", &available);
            input.user_voice = Some(&voice);
            input.announce_speaker = Some("Alex");
            let prepared = prepare_speech(input);
            assert_eq!(prepared.request.text, "Alex said hello");
        }
    }

    #[test]
    fn markdown_announcements_are_spoken_without_exposing_the_protected_body() {
        let mut available = models();
        available.push("fr_FR-siwis-medium".into());
        let media = [
            MediaAnnouncement {
                kind: MediaAnnouncementKind::Spoiler,
                text: None,
            },
            MediaAnnouncement {
                kind: MediaAnnouncementKind::Code,
                text: None,
            },
        ];
        let voice = VoicePreference {
            model: "fr_FR-siwis-medium".into(),
            speed: 1.0,
            engine: SynthesisEngine::Gcloud,
        };
        let mut with_markdown = input("bonjour", &available);
        with_markdown.user_voice = Some(&voice);
        with_markdown.media = &media;
        let prepared = prepare_speech(with_markdown);
        assert_eq!(prepared.spoken, "bonjour spoiler du code");
        assert_eq!(prepared.request.emphasis_source.as_deref(), Some("bonjour"));
    }
}
