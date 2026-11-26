# Plan 021: Make Kokoro and neural multi-segment audio work and cache correctly

> **Executor instructions**: Read `advisor-plans/README.md` § "Bug audit 2026-11-25"
> first. Follow the steps, run every verification, honour the STOP conditions, then set
> this plan's README row to `DONE (<commit>)`.
>
> **Drift check (run first)**: `git diff --stat c1dd4a9..HEAD -- crates/vozen-tts/src`
> Any change → compare excerpts below; mismatch = STOP.

## Status

- **Priority**: P1 · **Effort**: M · **Risk**: MED (audio path for all providers) · **Depends on**: none · **Category**: bug
- **Planned at**: commit `c1dd4a9`, 2026-11-25

## Why this matters

The WAV helpers only accept Piper's exact format (22,050 Hz mono 16-bit). Kokoro writes
its native sample rate (24 kHz; `tools/kokoro_server.py:72-73` calls `sf.write(out, samples, sr)`
with no resampling) and OpenAI's `response_format: "wav"` is not 22,050 Hz either. So every
Kokoro message that mixes languages (multi-segment) fails after synthesising every segment,
and the router silently falls back to Google (`crates/vozen-runtime/src/engine_router.rs:96-100`).
The neural engine fails the same way for multi-segment text and for any lead silence (the
1 s pause before the laugh in `/joke`). Around that, four cache/process bugs: Kokoro's cache
key ignores segments, Kokoro drops lead silence for single requests, Piper caches a
temporary-failure fallback under the multi-voice key, Kokoro kills its warm sidecar on any
per-request error, and Piper leaks temp WAVs on errors.

## Current state

- `crates/vozen-tts/src/wav_concat.rs:9-11` — `PIPER_SAMPLE_RATE = 22_050`, `PIPER_CHANNELS = 1`,
  `PIPER_BITS_PER_SAMPLE = 16`. `wav_concat.rs:95-133` `concat_wavs` rejects any segment where
  `!is_piper_format(wav.format)` (`UnsupportedFormat`) and computes silence with
  `PIPER_SAMPLE_RATE`; `wav_concat.rs:135-149` `silence_wav` / `prepend_silence_wav` build
  silence at the Piper rate; `wav_concat.rs:151-160` `is_piper_format`; `build_wav(data)`
  (~line 161) writes a Piper-format header. `parse_wav` (line 48) returns `ParsedWav { format, data }`.
- `crates/vozen-tts/src/kokoro.rs:309-334` multi-segment path: `concat_wavs(&wavs, 20)?` then
  `prepend_silence_wav(&combined, request.lead_silence_ms)?`.
  `kokoro.rs:337-386` `synth_single` never applies `lead_silence_ms`.
  `kokoro.rs:423-432` `cache_key` hashes text, model, speed, lead silence — **not segments**.
  `kokoro.rs:262-284` `request_sidecar`: any `Err` (including a well-formed `{"ok": false}`) kills
  the sidecar (`process.kill()`), and `response.out` is never compared with `output`.
  `kokoro.rs:390-394` `struct Response { ok, ready, out: Option<String> }`.
- `crates/vozen-tts/src/neural.rs:80-89` multi-segment `concat_wavs(&wavs, 60)` +
  `prepend_silence_wav`; `neural.rs:97-118` `synth_single` applies `prepend_silence_wav`.
- `crates/vozen-tts/src/lib.rs:284-291` Piper: `match self.synth_segments(request, segments).await { Ok(path) => Ok(path), Err(_) => self.synth_single(request).await }` — `request` still carries `segments`, so `synth_single` (`lib.rs:346`, `cache_key(request)`) writes the base-voice fallback under the **multi-voice** key (`lib.rs:500-522` includes segments).
- `lib.rs:363-377` Piper temp file: `result?;` and the lead-silence `read`/`write` `?` exits return without removing `temporary` (`.{uuid}.wav`, skipped by eviction because it is a dot-file).

## Scope

**In scope**: `crates/vozen-tts/src/wav_concat.rs`, `kokoro.rs`, `neural.rs`, `lib.rs` and their tests.

**Out of scope**: `tools/kokoro_server.py` (do not resample in Python), `gtts.rs`, `gcloud.rs`
(they already produce 22,050 Hz), `crates/vozen-runtime/src/engine_router.rs`, playback code.

## Steps

### Step 1: Format-agnostic concat and silence

- `concat_wavs`: parse all inputs; require `audio_format == 1`, `bits_per_sample == 16`, and that
  **every segment has the same `sample_rate` and `channels` as the first** (else `UnsupportedFormat`).
  Compute silence bytes from the first segment's rate and block align. Build the output header
  with that format (give `build_wav` a `WavFormat` parameter; keep a Piper-format default for `silence_wav`).
- `prepend_silence_wav(wav, ms)`: parse `wav`, generate silence in **its** format, then concat.
- Keep `PIPER_*` constants (other modules use them); keep `TooLarge` overflow checks.

**Verify**: `bash advisor-plans/docker-check.sh cargo test -p vozen-tts wav --locked` → pass, existing tests unchanged.

### Step 2: Kokoro cache key and lead silence

- Extend `kokoro::cache_key` with the same segments discriminator as `lib.rs:507-519`
  (`[1]` + count + each segment's text/model, or `[0]` when `None`). Prefer extracting a
  `pub(crate) fn hash_segments(digest: &mut Sha256, segments: &Option<Vec<SpeechSegment>>)` in
  `lib.rs` and calling it from both.
- In `kokoro::synth_single`, after `parse_wav(&bytes)?`, apply
  `prepend_silence_wav(&bytes, request.lead_silence_ms)` when `> 0` before `write_cached`.

### Step 3: Piper fallback under its own key

In `lib.rs:288-291`, build `let mut base = request.clone(); base.segments = None;` and call
`self.synth_single(&base)` in the `Err` arm.

### Step 4: Keep the Kokoro sidecar alive on protocol-valid failures

In `request_sidecar`, return a distinct error (or flag) for a parsed response with `ok != true`,
and only `kill()` the process for I/O, timeout or unparsable-JSON errors. Also treat
`response.out.as_deref() != Some(output.to_string_lossy().as_ref())` as a desync: kill and error.

### Step 5: No leaked Piper temp files

In `lib.rs` single-voice synthesis, remove `temporary` on every error path after it is
created (runner error, lead-silence read/write error), mirroring `kokoro.rs:372-374`.

**Verify (after Steps 2–5)**: `bash advisor-plans/docker-check.sh cargo test -p vozen-tts --locked` → all pass.

## Test plan

- `wav_concat.rs` tests: build 24,000 Hz mono 16-bit fixtures (write a small helper that emits a
  header with a given rate); concat of two 24 kHz WAVs succeeds and keeps 24,000 Hz; 22,050 + 24,000
  → `UnsupportedFormat`; `prepend_silence_wav(24k, 1000)` adds exactly 24,000 samples (48,000 bytes).
- `kokoro.rs` tests: `cache_key` differs between a request with `segments: None` and the same text
  with two segments.
- `lib.rs` tests (use the existing `FakeRunner` pattern at the top of `mod tests`): a runner that fails
  for one segment model → result path equals the `segments: None` key, not the multi-segment key; a
  failing runner leaves no `.*.wav` file in the cache dir.

## Done criteria

- [ ] `grep -n "is_piper_format(wav.format)" crates/vozen-tts/src/wav_concat.rs` → no output
- [ ] `bash advisor-plans/docker-check.sh cargo test -p vozen-tts --locked` → 0 failed, new tests present
- [ ] `bash advisor-plans/docker-check.sh gate` → `== GATE OK`
- [ ] only `crates/vozen-tts/src/*` changed; one commit, e.g. `Support native sample rates when joining Kokoro and neural speech`

## STOP conditions

- Another crate depends on `concat_wavs` rejecting non-Piper formats (search `concat_wavs` and `UnsupportedFormat` outside `vozen-tts`).
- OpenAI WAV headers fail `parse_wav` for a reason other than sample rate (e.g. a streaming size of `0xFFFFFFFF`) — report; do not add a lenient parser in this plan.
- The Kokoro `Response` protocol carries fields this plan doesn't list.

## Maintenance notes

- Existing Kokoro cache entries become misses once (new key) — expected.
- If a provider ever returns stereo or 24-bit audio, concat will reject it; add conversion then, not here.
