# Plan 028: Make shut-up, skip and pause race-free and keep paired speech together

> **Executor instructions**: Read `advisor-plans/README.md` § "Bug audit 2026-11-25"
> first. Follow the steps, run every verification, honour the STOP conditions, then set
> this plan's README row to `DONE (<commit>)`.
>
> **Drift check (run first)**: `git diff --stat c1dd4a9..HEAD -- crates/vozen-discord/src crates/vozen-core/src/play_queue.rs crates/vozen-tts/src/lib.rs crates/vozen-tts/src/kokoro.rs`
> Plans 021/023 legitimately changed some of these; search by the quoted code.

## Status

- **Priority**: P3 · **Effort**: M · **Risk**: MED · **Depends on**: 021, 023 · **Category**: bug
- **Planned at**: commit `c1dd4a9`, 2026-11-25

## Why this matters

Four timing bugs in the voice queue: (1) `/shut-up` (and `/skip` while idle) only set a cancellation flag;
a message that passed its last cancellation check a moment earlier is still enqueued and plays in full after
the command reported success. (2) After `/queue pause` then `/skip`, Songbird starts the next track but the
bot still believes it is paused, so `/queue pause` answers "nothing to pause" until someone runs a pointless
`/queue resume`. (3) `/joke` and the rizz command enqueue their two parts as two separate synthesis requests,
so other chatter can land between the joke and its laugh. (4) The Piper/Kokoro cache evicts the oldest files
by mtime but a cache hit never refreshes mtime, so a frequently used file can be deleted after its path was
handed to Songbird (which opens files lazily) and before it plays.

## Current state

- `crates/vozen-discord/src/message_voice_service.rs:419-440`: last check
  `if synthesis.cancelled() || !destination.is_current() { … cancel_reservation … }`, then
  `self.playback.enqueue_reserved(guild_id, destination, Path::new(&wav), QueueEnqueueOptions { author_id, source, lane, created_at_ms }).await`.
  Same pattern in `crates/vozen-discord/src/core_voice_service.rs` around lines 1599 and 1754.
- `crates/vozen-discord/src/voice_playback.rs:505-584` `enqueue_reserved`: awaits `songbird::get`, then
  `call.lock().await`, re-checks the destination but **not** cancellation, then `enqueue(track)`.
- `crates/vozen-core/src/play_queue.rs:22-27` `pub struct QueueEnqueueOptions<'a> { author_id, source, lane, created_at_ms }` (5 constructors in the workspace).
- `core_voice_service.rs:1626-1662` `skip`/`silence`: when the playback state is `Idle` they only cancel
  synthesis (`self.synthesis.cancel_active` / `clear`) and never touch playback.
- Pause: `voice_playback.rs:253-262` `pause()`/`resume()` toggle a per-guild set; `voice_playback.rs:627-638`
  `skip` calls `queue().skip()` and leaves the flag; `voice_playback.rs:750-761` `pause_queue` returns `Ok(false)` if
  the flag is already set → shown as "NothingToPause" (`crates/vozen-discord/src/queue_control.rs:122-125`).
- Paired speech: `core_voice_service.rs:1483-1491` (joke) and `1528-1558` (laugh, `lead_silence_ms: 1_000`), and
  `1095-1127` (rizz line + sound), each via `enqueue_synth_request` (`core_voice_service.rs:1566-1578`, which takes
  the guild synthesis lease with `self.synthesis.acquire(guild_id, admitted_generation)`).
- Cache: `crates/vozen-tts/src/lib.rs:447-485` `evict_cache_files` sorts by `modified()` and deletes the oldest
  beyond the limit, skipping only `just_written`; cache hits return early at `lib.rs:300` and `lib.rs:347-349`
  without touching mtime. Kokoro has the same pattern (`kokoro.rs` ~472-500 and its `non_empty_file` hits).

## Scope

**In scope**: the file that defines the playback trait with `enqueue_reserved` (find it with
`grep -rn "fn enqueue_reserved" crates/vozen-discord/src`), `voice_playback.rs`, `message_voice_service.rs`,
`core_voice_service.rs`, `vozen-tts/src/lib.rs`, `vozen-tts/src/kokoro.rs`, and their tests. Not `play_queue.rs`.
**Out of scope**: `queue_control.rs` rendering, Songbird vendored code, the accessibility ordering helper from plan 023.

## Steps

### Step 1: Re-check cancellation under the call lock

`QueueEnqueueOptions` derives `Debug, Clone, Copy` (`play_queue.rs:21`), so do **not** add a closure field
to it. Instead add a separate parameter to the playback trait method:
`is_cancelled: &(dyn Fn() -> bool + Send + Sync)` on `enqueue_reserved` (trait definition, the Songbird
implementation in `voice_playback.rs`, and every test fake — find them with
`grep -rn "fn enqueue_reserved" crates`). In the Songbird implementation, right after taking `call.lock()`
and before `remember_track`, return `Err(CommandPlaybackError)` if `is_cancelled()` is `true`. Pass
`&|| synthesis.cancelled()` from the three call sites above, and `&|| false` where there is no lease.

### Step 2: Clear the pause flag when the head changes

In `voice_playback.rs` `skip`, after a successful `queue().skip()`, clear the guild's pause flag
(`queue_metadata…resume(guild_id)`). Do the same in the queue-removal path if it removes the current track.

### Step 3: Paired speech under one lease

Split `enqueue_synth_request` so the body after acquiring the lease is a helper that takes the lease; add
`enqueue_synth_sequence(&self, …, requests: Vec<SynthRequest>)` that acquires the lease once and enqueues the
parts back-to-back. Use it for joke+laugh and rizz line+sound. Keep `/skip` semantics unchanged (it may still
skip only the current part).

### Step 4: Refresh mtime on cache hits; protect recent files

On every cache hit in `lib.rs` and `kokoro.rs`, best-effort update the file's mtime
(`std::fs::File::options().write(true).open(&path)?.set_modified(SystemTime::now())` inside
`tokio::task::spawn_blocking`, ignoring errors). In `evict_cache_files` (and Kokoro's equivalent), never delete a
file modified within the last 120 s.

**Verify**: `bash advisor-plans/docker-check.sh cargo test -p vozen-discord --locked` and
`bash advisor-plans/docker-check.sh cargo test -p vozen-tts --locked` → pass.

## Test plan

- `core_voice_service.rs` tests (from line 1858, use its fake playback): a fake whose `enqueue_reserved` sees
  `is_cancelled` → `true` returns an error and nothing is recorded as queued.
- `voice_playback.rs`: unit-test the ledger: `pause` → `skip` path leaves the guild not paused (test the ledger
  methods directly if Songbird cannot be faked).
- `lib.rs`: eviction keeps a file whose mtime is "now" even when it is the oldest by name; a cache hit updates mtime.

## Done criteria

- [ ] targeted tests pass; `bash advisor-plans/docker-check.sh gate` → `== GATE OK`
- [ ] only in-scope files changed; one commit, e.g. `Close shut-up, pause and cache races in the voice queue`

## STOP conditions

- The new `enqueue_reserved` parameter forces edits in crates other than `vozen-discord` beyond passing `&|| false`.
- `enqueue_synth_request` holds state that cannot be shared across two requests under one lease.
- `File::set_modified` is unavailable on the toolchain (Rust 1.97 has it; if not, STOP).

## Maintenance notes

- Any new enqueue site must pass its cancellation probe, or `/shut-up` can be bypassed again.
