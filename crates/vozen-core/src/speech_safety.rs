//! Pure speech preparation safeguards shared by message and command paths.
//!
//! This ports the legacy whole-word pronunciation and moderation behaviour without
//! retaining message content. Adapters decide where a request is eventually synthesized.

use regex::{Regex, RegexBuilder};

pub const MAX_SYNTH_CHARS: usize = 2_400;
// Bound every intermediate pronunciation result without truncating moderation input.
const MAX_PRONUNCIATION_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PronunciationEntry {
    pub term: String,
    pub replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeechSegment {
    pub text: String,
    pub model: String,
}

/// The requested synthesis route after all persistent preference precedence has resolved.
/// `Default` retains the legacy SQLite meaning of `google`: use the operator-configured
/// default provider, rather than assuming a particular external provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SynthesisEngine {
    Default,
    Piper,
    Kokoro,
    Gcloud,
    /// OpenAI's hosted `tts-1` provider selected by the operator's legacy
    /// `TTS_ENGINE=neural` setting. It is a default-provider route rather than
    /// a user-persisted engine choice.
    Neural,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcloudBudgetScope {
    User,
    Pass,
    Guild,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcloudBudget {
    pub scope: GcloudBudgetScope,
    pub key: String,
    pub seats: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SynthRequest {
    pub text: String,
    pub model: String,
    /// Trusted, repository-curated WAV to enqueue directly instead of invoking TTS.
    pub asset_path: Option<std::path::PathBuf>,
    pub speed: f64,
    pub engine: SynthesisEngine,
    /// Paid Google Cloud requests must carry a server-resolved budget descriptor. The provider
    /// rejects the request before network I/O when this is absent.
    pub gcloud_budget: Option<GcloudBudget>,
    pub segments: Option<Vec<SpeechSegment>>,
    pub single_voice: Option<bool>,
    pub emphasis_source: Option<String>,
    /// Optional silence inserted before this utterance. Zero keeps legacy output unchanged.
    pub lead_silence_ms: u32,
}

/// A message is speakable only when it retains a Unicode letter or number.
pub fn has_readable_text(text: &str) -> bool {
    text.chars().any(char::is_alphanumeric)
}

/// Applies server/user pronunciation entries in their declared order.
///
/// Each term is case-insensitive and must be delimited by non-letter/non-number
/// characters, matching the legacy JavaScript lookaround rule. Replacements are always
/// literal: administrator-controlled `$1` is spoken as `$1`, never interpreted as regex
/// syntax. Excessive intermediate expansion is rejected as empty text.
pub fn apply_pronunciation(text: &str, dictionary: &[PronunciationEntry]) -> String {
    apply_pronunciation_checked(text, dictionary).unwrap_or_default()
}

pub(crate) fn apply_pronunciation_checked(
    text: &str,
    dictionary: &[PronunciationEntry],
) -> Option<String> {
    if text.len() > MAX_PRONUNCIATION_BYTES {
        return None;
    }
    dictionary
        .iter()
        .try_fold(text.to_owned(), |current, entry| {
            let term = entry.term.trim();
            if term.is_empty() {
                return Some(current);
            }
            replace_whole_word(&current, term, &entry.replacement)
        })
}

/// True when at least one blocked term appears as a complete Unicode word.
pub fn is_blocked(text: &str, blocklist: &[String]) -> bool {
    blocklist
        .iter()
        .map(|word| word.trim())
        .filter(|word| !word.is_empty())
        .any(|word| contains_whole_word(text, word))
}

/// Removes blocked whole words while retaining the remaining readable content.
///
/// Whitespace is normalized only when a removal happened, matching the old request path.
pub fn redact_blocked(text: &str, blocklist: &[String]) -> String {
    let mut output = text.to_owned();
    let mut changed = false;
    for word in blocklist
        .iter()
        .map(|word| word.trim())
        .filter(|word| !word.is_empty())
    {
        let (next, did_replace) = replace_whole_word_with(&output, word, |_| " ".to_owned());
        output = next;
        changed |= did_replace;
    }
    if changed {
        output.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        output
    }
}

/// Redacts the text and every multi-voice segment before synthesis. Empty segments are removed.
pub fn redact_request(request: &SynthRequest, blocklist: &[String]) -> SynthRequest {
    if blocklist.is_empty() {
        return request.clone();
    }

    let segments = request.segments.as_ref().and_then(|segments| {
        let retained: Vec<_> = segments
            .iter()
            .map(|segment| SpeechSegment {
                text: redact_blocked(&segment.text, blocklist),
                model: segment.model.clone(),
            })
            .filter(|segment| has_readable_text(&segment.text))
            .collect();
        (!retained.is_empty()).then_some(retained)
    });

    SynthRequest {
        text: redact_blocked(&request.text, blocklist),
        model: request.model.clone(),
        asset_path: request.asset_path.clone(),
        speed: request.speed,
        engine: request.engine,
        gcloud_budget: request.gcloud_budget.clone(),
        segments,
        single_voice: request.single_voice,
        emphasis_source: request.emphasis_source.clone(),
        lead_silence_ms: request.lead_silence_ms,
    }
}

/// Caps the material sent to a provider after expansions and announcements.
///
/// The historical implementation first compares UTF-16 length, then truncates by code point to
/// avoid a lone surrogate. Rust strings are scalar-value safe, so the result cannot contain a
/// broken surrogate pair while preserving the same effective request budget.
pub fn cap_synth_request(request: &SynthRequest) -> SynthRequest {
    if request.text.encode_utf16().count() <= MAX_SYNTH_CHARS {
        return request.clone();
    }

    let mut budget = MAX_SYNTH_CHARS;
    let segments = request.segments.as_ref().map(|segments| {
        let mut kept = Vec::new();
        for segment in segments {
            if budget == 0 {
                break;
            }
            let text = take_code_points(&segment.text, budget);
            budget = budget.saturating_sub(text.chars().count());
            kept.push(SpeechSegment {
                text,
                model: segment.model.clone(),
            });
        }
        kept
    });

    SynthRequest {
        text: take_code_points(&request.text, MAX_SYNTH_CHARS),
        model: request.model.clone(),
        asset_path: request.asset_path.clone(),
        speed: request.speed,
        engine: request.engine,
        gcloud_budget: request.gcloud_budget.clone(),
        segments,
        single_voice: request.single_voice,
        emphasis_source: request.emphasis_source.clone(),
        lead_silence_ms: request.lead_silence_ms,
    }
}

fn take_code_points(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn contains_whole_word(text: &str, term: &str) -> bool {
    let regex = literal_case_insensitive_regex(term);
    regex
        .find_iter(text)
        .any(|matched| has_word_boundaries(text, matched.start(), matched.end()))
}

fn replace_whole_word(text: &str, term: &str, replacement: &str) -> Option<String> {
    let regex = literal_case_insensitive_regex(term);
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    for matched in regex.find_iter(text) {
        if !has_word_boundaries(text, matched.start(), matched.end()) {
            continue;
        }
        for piece in [&text[cursor..matched.start()], replacement] {
            if piece.len() > MAX_PRONUNCIATION_BYTES - output.len() {
                return None;
            }
            output.push_str(piece);
        }
        cursor = matched.end();
    }
    let remainder = &text[cursor..];
    if remainder.len() > MAX_PRONUNCIATION_BYTES - output.len() {
        return None;
    }
    output.push_str(remainder);
    Some(output)
}

pub(crate) fn replace_whole_word_with<F>(text: &str, term: &str, replacement: F) -> (String, bool)
where
    F: Fn(&str) -> String,
{
    let regex = literal_case_insensitive_regex(term);
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut changed = false;

    for matched in regex.find_iter(text) {
        if !has_word_boundaries(text, matched.start(), matched.end()) {
            continue;
        }
        output.push_str(&text[cursor..matched.start()]);
        output.push_str(&replacement(matched.as_str()));
        cursor = matched.end();
        changed = true;
    }

    if !changed {
        return (text.to_owned(), false);
    }
    output.push_str(&text[cursor..]);
    (output, true)
}

fn literal_case_insensitive_regex(term: &str) -> Regex {
    RegexBuilder::new(&regex::escape(term))
        .case_insensitive(true)
        .unicode(true)
        .build()
        .expect("escaped pronunciation or blocklist term is a valid regex")
}

fn has_word_boundaries(text: &str, start: usize, end: usize) -> bool {
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();
    before.is_none_or(|character| !character.is_alphanumeric())
        && after.is_none_or(|character| !character.is_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(text: &str) -> SynthRequest {
        SynthRequest {
            text: text.to_owned(),
            model: "en_US-amy-medium".to_owned(),
            asset_path: None,
            speed: 1.0,
            engine: SynthesisEngine::Default,
            gcloud_budget: None,
            segments: None,
            single_voice: Some(true),
            emphasis_source: Some(text.to_owned()),
            lead_silence_ms: 0,
        }
    }

    #[test]
    fn pronunciation_is_literal_case_insensitive_and_whole_word_only() {
        let dictionary = vec![
            PronunciationEntry {
                term: "btw".to_owned(),
                replacement: "R$1".to_owned(),
            },
            PronunciationEntry {
                term: "ola".to_owned(),
                replacement: "olá".to_owned(),
            },
        ];
        assert_eq!(
            apply_pronunciation("btw, BTW; btwx ola OlA", &dictionary),
            "R$1, R$1; btwx olá olá"
        );
    }

    #[test]
    fn chained_personal_expansions_are_rejected_before_large_intermediate_allocations() {
        let dictionary: Vec<_> = [("a", "b"), ("b", "c"), ("c", "d")]
            .into_iter()
            .map(|(term, word)| PronunciationEntry {
                term: term.into(),
                replacement: std::iter::repeat_n(word, 100).collect::<Vec<_>>().join(" "),
            })
            .collect();
        let input = std::iter::repeat_n("a", 249).collect::<Vec<_>>().join(" ");
        assert!(apply_pronunciation(&input, &dictionary).is_empty());
        let literal = "é".repeat(MAX_PRONUNCIATION_BYTES / 2);
        assert_eq!(
            apply_pronunciation(
                "a",
                &[PronunciationEntry {
                    term: "a".into(),
                    replacement: literal.clone()
                }]
            ),
            literal
        );
        assert!(
            apply_pronunciation(
                "a",
                &[PronunciationEntry {
                    term: "a".into(),
                    replacement: "é".repeat(MAX_PRONUNCIATION_BYTES / 2 + 1)
                }]
            )
            .is_empty()
        );
        let moderated = apply_pronunciation(
            "a",
            &[PronunciationEntry {
                term: "a".into(),
                replacement: format!("{} blocked", "safe ".repeat(1_000)),
            }],
        );
        assert!(is_blocked(&moderated, &["blocked".into()]));
    }

    #[test]
    fn blocklist_fails_closed_on_unicode_words_without_overmatching() {
        let list = vec!["palavrão".to_owned(), "ßeta".to_owned()];
        assert!(is_blocked("Olá PALAVRÃO, aqui", &list));
        assert!(!is_blocked("palavrãox", &list));
        assert_eq!(
            redact_blocked("Olá palavrão palavrãox e ßETA!", &list),
            "Olá palavrãox e !"
        );
    }

    #[test]
    fn request_redaction_retains_only_speakable_segments() {
        let mut input = request("ola palavrao hi");
        input.segments = Some(vec![
            SpeechSegment {
                text: "ola palavrao".to_owned(),
                model: "pt_PT-google-medium".to_owned(),
            },
            SpeechSegment {
                text: "palavrao".to_owned(),
                model: "en_US-amy-medium".to_owned(),
            },
            SpeechSegment {
                text: "hi".to_owned(),
                model: "en_US-amy-medium".to_owned(),
            },
        ]);
        let output = redact_request(&input, &["palavrao".to_owned()]);
        assert_eq!(output.text, "ola hi");
        assert_eq!(
            output.segments,
            Some(vec![
                SpeechSegment {
                    text: "ola".to_owned(),
                    model: "pt_PT-google-medium".to_owned(),
                },
                SpeechSegment {
                    text: "hi".to_owned(),
                    model: "en_US-amy-medium".to_owned(),
                },
            ])
        );
        assert!(!has_readable_text("!!! ,. "));
        assert!(has_readable_text("こんにちは 1"));
    }

    #[test]
    fn output_cap_preserves_safe_code_points_and_segment_order() {
        let mut input = request(&"𝕏".repeat(MAX_SYNTH_CHARS + 1));
        input.segments = Some(vec![
            SpeechSegment {
                text: "a".repeat(MAX_SYNTH_CHARS),
                model: "pt_PT-google-medium".to_owned(),
            },
            SpeechSegment {
                text: "b".to_owned(),
                model: "en_US-amy-medium".to_owned(),
            },
        ]);
        let output = cap_synth_request(&input);
        assert_eq!(output.text.chars().count(), MAX_SYNTH_CHARS);
        assert_eq!(output.segments.unwrap().len(), 1);
    }
}
