# Plan 020: Make giveaway entries match the published rules and keep draws alive

> **Executor instructions**: Read `advisor-plans/README.md` § "Bug audit 2026-11-25"
> first. Follow the steps, run every verification, honour the STOP conditions, then set
> this plan's README row to `DONE (<commit>)`.
>
> **Drift check (run first)**: `git diff --stat c1dd4a9..HEAD -- crates/vozen-runtime/src/giveaway_bot.rs crates/vozen-store/src/giveaway.rs crates/vozen-store/src/vote_reward.rs`
> Plan 019 legitimately changed `vote_reward.rs`; any other change → compare excerpts, mismatch = STOP.

## Status

- **Priority**: P1 · **Effort**: M · **Risk**: LOW · **Depends on**: 019 (same file) · **Category**: bug
- **Planned at**: commit `c1dd4a9`, 2026-11-25

## Why this matters

Giveaways award paid plans. Four bugs make the draw disagree with what the announcement
card promises or stop it altogether: (1) Helper/Music votes become extra tickets even when
the card says "one entry per person"; (2) a vote whose Top.gg timestamp is a few seconds
ahead of our clock silently loses its entry forever; (3) a stored user ID of `0` panics
the giveaway task (`serenity::UserId::new(0)` panics) on every draw attempt; (4) when the
announcement message is deleted or unreachable, the bot retries the edit every 5 s forever,
and a gateway stop or a startup DB error silently stops all draws until restart.

## Current state

- **Three-bot mode** is decided only on the card side —
  `crates/vozen-runtime/src/giveaway_bot.rs:295-300`:
  ```rust
  fn three_bots_enabled(flag: Option<&str>, secrets: [Option<&str>; 2]) -> bool {
      flag == Some("true") && secrets.iter().all(|s| s.is_some_and(|v| !v.trim().is_empty()))
  }
  ```
  called at `giveaway_bot.rs:46-54` with `VOZEN_GIVEAWAY_THREE_BOTS_ENABLED`,
  `TOPGG_HELPER_WEBHOOK_SECRET`, `TOPGG_MUSIC_WEBHOOK_SECRET`. But the webhook routes are
  built from the secrets alone — `crates/vozen-runtime/src/main.rs:2133-2137`
  (`giveaway_webhook_secrets: [nonempty_env("TOPGG_HELPER_WEBHOOK_SECRET"), nonempty_env("TOPGG_MUSIC_WEBHOOK_SECRET")]`)
  → `crates/vozen-api/src/topgg_webhook.rs:96-115` adds `/webhook/topgg/helper` and `/music`
  whenever a secret is `Some`. Tickets are drawn from both tables regardless —
  `crates/vozen-store/src/giveaway.rs:344-355` (`giveaway_candidates` unions
  `giveaway_entry` and `giveaway_vote`).
- **Future `voted_at`** — `crates/vozen-store/src/vote_reward.rs:166`
  (`voted_at.filter(|time| *time <= now && *time >= 0)`) and `vote_reward.rs:228`
  (`|| voted_at > now` → `Ok(0)`). The event id is already marked processed at
  `vote_reward.rs:156-164`, so a Top.gg retry is a `DuplicateEvent`. Signature tolerance
  is `vozen_core::TOPGG_SIGNATURE_TOLERANCE_MS` (re-exported in `crates/vozen-core/src/lib.rs:97`).
- **User IDs** — `record_giveaway_vote` (`vote_reward.rs:213-233`) accepts any 1–25 digit
  string (including `"0"`); `insert_giveaway_vote` is at `vote_reward.rs:258-274`.
  `giveaway_bot.rs:265` does `UserId::new(candidate.parse()?)` and `giveaway_bot.rs:229`
  does `UserId::new(award.user_id.parse()?)`; both panic on `0`.
- **Retry loop** — `giveaway_bot.rs:203-218` edits the announcement with `.await?`
  and only calls `giveaway_sync` on success; `giveaway_bot.rs:282-285`
  `discord_permission_error` recognises HTTP 403/404. Startup: `giveaway_bot.rs:60-66`
  returns (disables giveaways for the process) if `giveaway_refresh_open` fails once;
  `giveaway_bot.rs:75-78` `tokio::select!` stops the job loop when the gateway stops.

## Scope

**In scope**: `crates/vozen-runtime/src/giveaway_bot.rs`, `crates/vozen-store/src/giveaway.rs`,
`crates/vozen-store/src/vote_reward.rs`, and their tests.

**Out of scope**: `crates/vozen-runtime/src/main.rs` and `crates/vozen-api/src/topgg_webhook.rs`
(the routes must stay live for the Top.gg provider test, see Step 1); the card texts
(`VOTE_RULE` etc.); SQLite schema; `DEPLOY.md`.

## Steps

### Step 1: Keep the webhook routes as they are (no code change)

The Helper/Music routes MUST stay reachable while the flag is off: `DEPLOY.md` (commit `cc7089d`)
requires both signed webhooks to "pass a provider test" before `VOZEN_GIVEAWAY_THREE_BOTS_ENABLED`
is set to `true`, and says "Endpoint preparation and deployment are safe while this flag remains
false". Do **not** gate the routes in `main.rs`. The fix is at draw time (Step 2).

### Step 2: Draw only the tickets the card promises

Add a `include_partner_votes: bool` parameter to `giveaway_candidates`; when false, the
`giveaway_vote` arm only includes `bot_id = vozen_core::GIVEAWAY_VOTE_BOTS[0].1` (the TTS bot).
`giveaway_bot.rs` passes `self.three_bots`. Update all callers (`grep -rn giveaway_candidates crates`).

**Verify**: `bash advisor-plans/docker-check.sh cargo test -p vozen-store giveaway --locked` → pass.

### Step 3: Tolerate small clock skew on `voted_at`

In both places in `vote_reward.rs`, treat `voted_at` in `(now, now + TOPGG_SIGNATURE_TOLERANCE_MS]`
as `now`; keep rejecting anything further ahead and negatives.

### Step 4: Valid snowflakes only, and never panic in the draw

- In `record_giveaway_vote` and before `insert_giveaway_vote` in the TTS path, require that
  `user_id.parse::<u64>()` succeeds and is non-zero (return `Ok(0)` / skip the entry otherwise).
- In `giveaway_bot.rs` `eligible()` and `notify()`, parse with `str::parse::<std::num::NonZeroU64>()`
  and build `UserId::from(non_zero)`; in `eligible()` skip unparsable candidates (log once)
  instead of returning `Err`. Keep returning `Err` for transient HTTP errors (the next 5 s tick retries).

### Step 5: Stop the endless edit retry and decouple the job loop

- In the announcement edit (`giveaway_bot.rs:203-218`): if the error matches
  `discord_permission_error`, log it and call `giveaway_sync(g.id, g.revision)` so the job
  stops retrying; the giveaway itself stays open and still draws.
- In `spawn`: on `giveaway_refresh_open` failure, retry every 30 s (bounded log noise) instead
  of returning. Run `bot.run(&http)` in its own `tokio::spawn` so it keeps going if
  `client.start()` returns; log when the gateway stops.

**Verify**: `bash advisor-plans/docker-check.sh cargo test -p vozen-runtime giveaway --locked` → pass.

## Test plan

Add to `vote_reward.rs` tests: skewed `voted_at = now + 3_000` records an entry; `now + 10 min`
does not; user id `"0"` and a 25-digit id are rejected by `record_giveaway_vote`.
Add to `giveaway.rs` tests (model after the existing draw tests near line 574+): with
`include_partner_votes = false`, a Helper vote row does not appear in candidates; with `true` it does.
Add to `giveaway_bot.rs` tests: `three_bots_enabled(Some("false"), [Some("a"), Some("b")])` is false
(documents the predicate the draw now depends on).

## Done criteria

- [ ] targeted tests above pass; `bash advisor-plans/docker-check.sh gate` → `== GATE OK`
- [ ] `grep -n "UserId::new(.*parse()?)" crates/vozen-runtime/src/giveaway_bot.rs` → no output
- [ ] `node tools/check-rust-contracts.mjs` exit 0
- [ ] only in-scope files changed; one commit, e.g. `Keep giveaway tickets and draws consistent with the announced rules`

## STOP conditions

- `giveaway_candidates` has callers outside `giveaway_bot.rs` that rely on partner votes.
- Any step seems to require disabling or removing the Helper/Music webhook routes.
- Any test fixture uses a user id that is not a non-zero u64 and changing it would alter a contract file.

## Maintenance notes

- Partner votes are still recorded while the flag is off (routes stay live for the provider test),
  but a draw that runs with the flag off ignores them. If the flag is switched on before a giveaway
  ends, its partner votes count — they are real votes cast under the three-bot card.
- Reviewers: check that no draw path can still reach `UserId::new` with unvalidated input.
