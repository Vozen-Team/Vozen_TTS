//! Canonical PCM WAV parsing and concatenation for multi-voice requests.
//!
//! Only mono 16-bit PCM is accepted. Piper emits 22_050 Hz while Kokoro and OpenAI emit their
//! native 24_000 Hz, so concatenation keeps the first segment's sample rate and requires every
//! segment to match it. Keeping this strict prevents a malformed provider response from becoming
//! a corrupted Discord audio stream. Chunks are walked by ID rather than assuming a fixed
//! 44-byte header.

use thiserror::Error;

pub const PIPER_SAMPLE_RATE: u32 = 22_050;
pub const PIPER_CHANNELS: u16 = 1;
pub const PIPER_BITS_PER_SAMPLE: u16 = 16;
pub const DEFAULT_SEGMENT_SILENCE_MS: u32 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WavFormat {
    pub audio_format: u16,
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedWav<'a> {
    pub format: WavFormat,
    pub data: &'a [u8],
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WavError {
    #[error("WAV is too small")]
    TooSmall,
    #[error("WAV is not RIFF/WAVE")]
    InvalidContainer,
    #[error("WAV chunk exceeds the buffer")]
    TruncatedChunk,
    #[error("WAV is missing its fmt chunk")]
    MissingFormat,
    #[error("WAV is missing its data chunk")]
    MissingData,
    #[error("WAV fmt chunk is too small")]
    ShortFormat,
    #[error("WAV has unsupported PCM format")]
    UnsupportedFormat,
    #[error("combined WAV exceeds RIFF's 4 GiB limit")]
    TooLarge,
}

pub fn parse_wav(wav: &[u8]) -> Result<ParsedWav<'_>, WavError> {
    if wav.len() < 12 {
        return Err(WavError::TooSmall);
    }
    if &wav[..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return Err(WavError::InvalidContainer);
    }
    let mut format = None;
    let mut data = None;
    let mut offset = 12usize;
    while offset.saturating_add(8) <= wav.len() {
        let chunk_id = &wav[offset..offset + 4];
        let size =
            u32::from_le_bytes(wav[offset + 4..offset + 8].try_into().expect("size")) as usize;
        let body = offset + 8;
        let end = body.checked_add(size).ok_or(WavError::TruncatedChunk)?;
        if end > wav.len() {
            return Err(WavError::TruncatedChunk);
        }
        if chunk_id == b"fmt " {
            if size < 16 {
                return Err(WavError::ShortFormat);
            }
            format = Some(WavFormat {
                audio_format: u16::from_le_bytes(wav[body..body + 2].try_into().expect("format")),
                channels: u16::from_le_bytes(wav[body + 2..body + 4].try_into().expect("channels")),
                sample_rate: u32::from_le_bytes(
                    wav[body + 4..body + 8].try_into().expect("sample rate"),
                ),
                bits_per_sample: u16::from_le_bytes(
                    wav[body + 14..body + 16].try_into().expect("bits"),
                ),
            });
        } else if chunk_id == b"data" {
            data = Some(&wav[body..end]);
        }
        offset = end + (size & 1);
        if offset > wav.len() {
            return Err(WavError::TruncatedChunk);
        }
    }
    Ok(ParsedWav {
        format: format.ok_or(WavError::MissingFormat)?,
        data: data.ok_or(WavError::MissingData)?,
    })
}

pub fn concat_wavs(wavs: &[Vec<u8>], silence_ms: u32) -> Result<Vec<u8>, WavError> {
    if wavs.is_empty() {
        return Err(WavError::MissingData);
    }
    let parsed = wavs
        .iter()
        .map(|wav| parse_wav(wav))
        .collect::<Result<Vec<_>, _>>()?;
    // Providers emit different native rates (Piper 22,050 Hz, Kokoro and OpenAI 24,000 Hz), so
    // the output follows the first segment and every segment must share that exact format.
    let format = parsed[0].format;
    let align = block_align(format);
    if !is_supported_pcm(format)
        || parsed
            .iter()
            .any(|wav| wav.format != format || wav.data.len() % usize::from(align) != 0)
    {
        return Err(WavError::UnsupportedFormat);
    }
    let silence_bytes = (u64::from(silence_ms) * u64::from(format.sample_rate) / 1_000)
        .checked_mul(u64::from(align))
        .ok_or(WavError::TooLarge)?;
    let audio_bytes = parsed.iter().try_fold(0u64, |total, wav| {
        total
            .checked_add(wav.data.len() as u64)
            .ok_or(WavError::TooLarge)
    })?;
    let gaps = (parsed.len() - 1) as u64;
    let data_len = audio_bytes
        .checked_add(silence_bytes.checked_mul(gaps).ok_or(WavError::TooLarge)?)
        .ok_or(WavError::TooLarge)?;
    if data_len > u64::from(u32::MAX - 36) {
        return Err(WavError::TooLarge);
    }
    let mut data = Vec::with_capacity(data_len as usize);
    for (index, wav) in parsed.iter().enumerate() {
        if index > 0 {
            data.resize(data.len() + silence_bytes as usize, 0);
        }
        data.extend_from_slice(wav.data);
    }
    Ok(build_wav_with(format, &data))
}

/// Canonical Piper-format silence (22,050 Hz mono 16-bit).
pub fn silence_wav(milliseconds: u32) -> Vec<u8> {
    silence_wav_with(PIPER_FORMAT, milliseconds)
}

fn silence_wav_with(format: WavFormat, milliseconds: u32) -> Vec<u8> {
    let samples = u64::from(milliseconds) * u64::from(format.sample_rate) / 1_000;
    let bytes = samples
        .saturating_mul(u64::from(block_align(format)))
        .min(u64::from(u32::MAX - 36));
    build_wav_with(format, &vec![0; bytes as usize])
}

/// Prepends silence in the source WAV's own format (sample rate included).
pub fn prepend_silence_wav(wav: &[u8], milliseconds: u32) -> Result<Vec<u8>, WavError> {
    if milliseconds == 0 {
        return Ok(wav.to_vec());
    }
    let format = parse_wav(wav)?.format;
    if !is_supported_pcm(format) {
        return Err(WavError::UnsupportedFormat);
    }
    concat_wavs(&[silence_wav_with(format, milliseconds), wav.to_vec()], 0)
}

const PIPER_FORMAT: WavFormat = WavFormat {
    audio_format: 1,
    channels: PIPER_CHANNELS,
    sample_rate: PIPER_SAMPLE_RATE,
    bits_per_sample: PIPER_BITS_PER_SAMPLE,
};

/// Mono 16-bit PCM at any positive sample rate; Songbird resamples on playback.
fn is_supported_pcm(format: WavFormat) -> bool {
    format.audio_format == 1
        && format.sample_rate > 0
        && format.channels == PIPER_CHANNELS
        && format.bits_per_sample == PIPER_BITS_PER_SAMPLE
}

fn block_align(format: WavFormat) -> u16 {
    format.channels * (format.bits_per_sample / 8)
}

#[cfg(test)]
fn build_wav(data: &[u8]) -> Vec<u8> {
    build_wav_with(PIPER_FORMAT, data)
}

fn build_wav_with(format: WavFormat, data: &[u8]) -> Vec<u8> {
    let align = block_align(format);
    let mut out = Vec::with_capacity(44 + data.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&format.channels.to_le_bytes());
    out.extend_from_slice(&format.sample_rate.to_le_bytes());
    out.extend_from_slice(&(format.sample_rate * u32::from(align)).to_le_bytes());
    out.extend_from_slice(&align.to_le_bytes());
    out.extend_from_slice(&format.bits_per_sample.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concatenates_canonical_wavs_with_the_expected_silence() {
        let first = build_wav(&[1, 2, 3, 4]);
        let second = build_wav(&[5, 6]);
        let combined = concat_wavs(&[first, second], 1).expect("combined");
        let parsed = parse_wav(&combined).expect("parsed");
        assert_eq!(parsed.format.sample_rate, PIPER_SAMPLE_RATE);
        assert_eq!(parsed.data.len(), 4 + 44 + 2);
        assert_eq!(&parsed.data[..4], [1, 2, 3, 4]);
        assert_eq!(&parsed.data[48..], [5, 6]);
    }

    #[test]
    fn walks_extra_chunks_and_rejects_non_piper_audio() {
        let mut wav = build_wav(&[1, 2]);
        wav.splice(36..36, b"LIST\x01\x00\x00\x00x\x00".iter().copied());
        assert_eq!(parse_wav(&wav).expect("extra chunk").data, &[1, 2]);
        let mut stereo = build_wav(&[1, 2]);
        stereo[22..24].copy_from_slice(&2u16.to_le_bytes());
        assert_eq!(concat_wavs(&[stereo], 0), Err(WavError::UnsupportedFormat));
    }

    #[test]
    fn rejects_incomplete_pcm_samples_before_concatenation() {
        let mut malformed = build_wav(&[1, 2, 3]);
        malformed.push(0); // RIFF padding does not form part of the PCM sample data.
        let riff_size = (malformed.len() - 8) as u32;
        malformed[4..8].copy_from_slice(&riff_size.to_le_bytes());
        assert_eq!(parse_wav(&malformed).expect("container").data.len(), 3);
        assert_eq!(
            concat_wavs(&[malformed, build_wav(&[4, 5])], 0),
            Err(WavError::UnsupportedFormat)
        );
    }

    #[test]
    fn prepends_requested_silence_without_mutating_the_original() {
        let source = build_wav(&[1, 2, 3, 4]);
        let prefixed = prepend_silence_wav(&source, 1).expect("prefixed");
        let parsed = parse_wav(&prefixed).expect("parsed");
        assert_eq!(parsed.data.len(), 44 + 4);
        assert_eq!(&parsed.data[..44], vec![0; 44].as_slice());
        assert_eq!(&parsed.data[44..], &[1, 2, 3, 4]);
        assert_eq!(parse_wav(&source).expect("source").data, &[1, 2, 3, 4]);
    }

    const KOKORO_FORMAT: WavFormat = WavFormat {
        audio_format: 1,
        channels: 1,
        sample_rate: 24_000,
        bits_per_sample: 16,
    };

    #[test]
    fn concatenates_native_24_khz_segments_without_resampling() {
        let first = build_wav_with(KOKORO_FORMAT, &[1, 2]);
        let second = build_wav_with(KOKORO_FORMAT, &[3, 4]);
        let combined = concat_wavs(&[first, second], 1).expect("combined");
        let parsed = parse_wav(&combined).expect("parsed");
        assert_eq!(parsed.format, KOKORO_FORMAT);
        // 1 ms at 24 kHz is 24 samples, 48 bytes of silence between the segments.
        assert_eq!(parsed.data.len(), 2 + 48 + 2);
        assert_eq!(&parsed.data[50..], [3, 4]);
    }

    #[test]
    fn rejects_segments_with_different_sample_rates() {
        let piper = build_wav(&[1, 2]);
        let kokoro = build_wav_with(KOKORO_FORMAT, &[3, 4]);
        assert_eq!(
            concat_wavs(&[piper, kokoro], 0),
            Err(WavError::UnsupportedFormat)
        );
    }

    #[test]
    fn lead_silence_follows_the_source_sample_rate() {
        let source = build_wav_with(KOKORO_FORMAT, &[1, 2]);
        let prefixed = prepend_silence_wav(&source, 1_000).expect("prefixed");
        let parsed = parse_wav(&prefixed).expect("parsed");
        assert_eq!(parsed.format.sample_rate, 24_000);
        assert_eq!(parsed.data.len(), 48_000 + 2);
        assert_eq!(&parsed.data[48_000..], [1, 2]);
    }
}
