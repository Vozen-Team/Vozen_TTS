# Plan 023: Honour the accessibility priority lane in Songbird playback

> **Executor instructions**: Read `advisor-plans/README.md` § "Bug audit 2026-11-25"
> first. Follow the steps, run every verification, honour the STOP conditions, then set
> this plan's README row to `DONE (<commit>)`.
>
> **Drift check (run first)**: `git diff --stat c1dd4a9..HEAD -- crates/vozen-discord/src/voice_playback.rs crates/vozen-core/src/play_queue.rs`
> Any change → compare excerpts; mismatch = STOP.

## Status

- **Priority**: P2 · **Effort**: M · **Risk**: MED (live queue reordering) · **Depends on**: none · **Category**: bug
- **Planned at**: commit `c1dd4a9`, 2026-11-25

## Why this matters

`/config queue-role priority` lets a server give members (often people who rely on TTS
for accessibility) a priority lane. Messages from those members are tagged
`QueueLane::Accessibility` (`crates/vozen-core/src/lib.rs:171-176`), and `/queue show` labels them
"accessibility", but the Songbird adapter ignores the lane and appends every track to one FIFO.
The feature is advertised and does nothing. The lane scheduler with its fairness rule
(at most 3 accessibility items before a waiting standard one) exists in
`crates/vozen-core/src/play_queue.rs` but nothing in production uses it.

## Current state

- `crates/vozen-core/src/play_queue.rs:9-10` — `pub const MAX_ACCESSIBILITY_BURST: u8 = 3;`;
  `play_queue.rs:105-120` `PlayQueue::dequeue` implements the rule (accessibility first while the burst
  is below 3 or the standard lane is empty; a standard item resets the burst). `PlayQueue::new` has no
  production caller.
- `crates/vozen-discord/src/voice_playback.rs:485-503` `reserve(&self, guild_id, _lane)` ignores the lane.
- `voice_playback.rs:505-584` `enqueue_reserved`: takes `call.lock()`, validates destination, stores
  `QueueTrackMetadata { author_id, source, lane, created_at_ms }` via `remember_track` (lane at line 545),
  builds the Songbird `Track` and ends with
  `let handle = songbird_driver_mut(&mut handler).enqueue(track).await;` (line 584) — always at the back.
- Songbird (vendored, `vendor/songbird/src/tracks/queue.rs`) offers `TrackQueue::modify_queue(|vq| ...)`
  over the `VecDeque` of queued tracks; index 0 is the currently playing track.
- Test that misses this: `crates/vozen-discord/src/core_voice_service.rs:2402-2433` only checks the lane is
  passed to a fake playback. Adapter tests live in `voice_playback.rs` `mod tests` (line 929+, metadata
  tests use `QueueLane::Accessibility` near line 1010).

## Scope

**In scope**: `crates/vozen-discord/src/voice_playback.rs` (code + tests). Optionally import
`MAX_ACCESSIBILITY_BURST` from `vozen_core`.

**Out of scope**: `play_queue.rs` (do not wire the unused `PlayQueue` in), `queue_control.rs`, the
lane assignment in `vozen-core/src/lib.rs`, any i18n string.

## Steps

### Step 1: A pure insertion rule

Add `fn accessibility_insert_index(pending: &[QueueLane]) -> usize` where `pending` are the lanes of
the tracks **after** the current one (queue index 1..). Rule:

1. `p` = index after the last `Accessibility` in `pending` (FIFO among accessibility), or `0` if none.
2. While the run of consecutive `Accessibility` items ending right before `p` is `>= MAX_ACCESSIBILITY_BURST`
   and `p < pending.len()`, move `p` to just after the next `Standard` item.

Required results (unit tests): `[]→0`, `[S,S]→0`, `[A,S]→1`, `[A,A,S,S]→2`, `[A,A,A,S]→4`, `[A,A,A]→3`.

**Verify**: `bash advisor-plans/docker-check.sh cargo test -p vozen-discord accessibility_insert --locked` → pass.

### Step 2: Reorder on enqueue, under the same call lock

In `enqueue_reserved`, after `enqueue(track)` and before `drop(handler)`: if `options.lane ==
QueueLane::Accessibility` and the queue length is `> 2`, read the lanes of queue items `1..len-1` from
the track metadata ledger (by track UUID), compute the index, and `modify_queue` to move the last entry
to `1 + index`. Never move anything into index 0. If any metadata is missing, leave the order unchanged.

### Step 3: Keep the ledger and `/queue` consistent

Confirm `/queue show` and `/queue remove <n>` read order from the Songbird queue (not from insertion
order in the ledger). If they read the ledger order, update that function in `voice_playback.rs` to
follow the Songbird order.

**Verify**: `bash advisor-plans/docker-check.sh cargo test -p vozen-discord voice_playback --locked` → pass.

## Test plan

- Step 1 table as unit tests.
- If the module has a test seam for queue ordering, add one: standard, standard, then accessibility enqueued →
  order is [current, accessibility, standard]. If no seam exists without a real Songbird driver, keep to the
  pure-function tests and say so in the commit body.

## Done criteria

- [ ] `grep -n "_lane: vozen_core::QueueLane" crates/vozen-discord/src/voice_playback.rs` still allowed for `reserve`, but `enqueue_reserved` uses `options.lane` for ordering
- [ ] targeted tests pass; `bash advisor-plans/docker-check.sh gate` → `== GATE OK`
- [ ] only `voice_playback.rs` changed; one commit, e.g. `Play priority-role speech ahead of the standard queue`

## STOP conditions

- `modify_queue` is not available on the vendored Songbird `TrackQueue`, or moving a `Queued` entry requires
  touching `vendor/`.
- `/queue remove` indexes are derived from something that would break when the order changes and the fix
  needs `queue_control.rs`.

## Maintenance notes

- The current track is never interrupted; priority only affects what plays next.
- Plan 028 later edits `skip`/pause in the same file; keep this logic in its own helper.
