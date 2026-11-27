# Plan 019: Grant 24 hours of Plus per vote and stop phantom ledger backfill

> **Executor instructions**: Read `advisor-plans/README.md` § "Bug audit 2026-11-25"
> first (branch, Docker commands, commit rules). Follow this plan step by step, run
> every verification command, and honour the STOP conditions. When done, set this
> plan's row in that README to `DONE (<commit>)`.
>
> **Drift check (run first)**: `git diff --stat c1dd4a9..HEAD -- crates/vozen-store/src/premium.rs crates/vozen-store/src/vote_reward.rs`
> If either file changed, compare the excerpts below with the live code; a mismatch is a STOP.

## Status

- **Priority**: P1 · **Effort**: S · **Risk**: LOW · **Depends on**: none · **Category**: bug
- **Planned at**: commit `c1dd4a9`, 2026-11-25

## Why this matters

The Terms (`TERMS.md:46`, `site/terms.html:78`) promise **24 hours of Vozen Plus per
eligible vote**, at most four rewards in a rolling 30 days and never more than 48 h
ahead. The access check uses a different, 48 h constant, so one vote gives between
24 h and 48 h of Plus depending on when the daily purge runs, and stacked votes can
reach 72 h. Separately, the startup ledger backfill inserts a phantom grant for every
user whose reward was stacked, which consumes their 4-per-30-days cap early. The repo
owner decided on 2026-11-25 that **24 h (the published Terms) is correct**.

## Current state

- `crates/vozen-store/src/premium.rs:8` — a second constant, used only by the access query:
  ```rust
  pub const VOTE_REWARD_MS: i64 = 48 * 60 * 60 * 1_000;
  ```
  `premium.rs:181-196` `user_premium_expiry` computes `SELECT rewarded_at + ?2 ... FROM vote_reward` with `params![user_id, VOTE_REWARD_MS]` (the 48 h one).
- `crates/vozen-store/src/vote_reward.rs:15-18` — the authoritative constants:
  ```rust
  pub const VOTE_REWARD_MS: i64 = 24 * 60 * 60 * 1_000;
  pub const VOTE_REWARD_MAX_AHEAD_MS: i64 = 48 * 60 * 60 * 1_000;
  ```
  `vote_reward.rs:194-200` stores `rewarded_at = expires_at - VOTE_REWARD_MS` (24 h);
  `vote_reward.rs:238-243` purges rows with `rewarded_at <= now - VOTE_REWARD_MS`.
  `crates/vozen-store/src/lib.rs:120` re-exports the `vote_reward` constants.
- `vote_reward.rs:46-72` `initialize_vote_redemption_ledger` (called on every start from
  `crates/vozen-runtime/src/main.rs:2589`) inserts `(user_hash, rewarded_at)` for every
  `vote_reward` row with `INSERT OR IGNORE`. Real grants insert `(user_hash, now)`
  (`vote_reward.rs:202-205`). For a stacked vote `rewarded_at != granted_at`, so the
  backfill adds an extra ledger row; the cap at `vote_reward.rs:178-184` counts those rows.

## Scope

**In scope**: `crates/vozen-store/src/premium.rs`, `crates/vozen-store/src/vote_reward.rs` (code and its `mod tests`).

**Out of scope**: `TERMS.md`, `site/`, docs (they already say 24 h); `contracts/sqlite-schema.json`
(no schema change is allowed in this plan); `crates/vozen-runtime/src/main.rs`.

## Steps

### Step 1: One vote-reward duration

In `premium.rs`, delete the local `VOTE_REWARD_MS` constant and its doc comment, and
use `crate::vote_reward::VOTE_REWARD_MS` in `user_premium_expiry`. Then
`grep -rn "premium::VOTE_REWARD_MS" crates` must return nothing (fix any import that
pointed at the removed constant to the `vote_reward` one).

**Verify**: `grep -rn "48 \* 60 \* 60" crates/vozen-store/src/premium.rs` → no output;
`bash advisor-plans/docker-check.sh cargo check -p vozen-store --all-targets --locked` → exit 0.

### Step 2: Idempotent, legacy-only backfill

Change the backfill insert so it only seeds users that have **no** ledger row at all
(true pre-ledger legacy rows), e.g.:

```sql
INSERT INTO vote_reward_ledger (user_hash, granted_at)
SELECT ?1, ?2 WHERE NOT EXISTS (SELECT 1 FROM vote_reward_ledger WHERE user_hash = ?1)
```

Keep the transaction and `assert_stable_redemption_secret` call as they are. Keep the
return value meaning ("rows inserted").

**Verify**: `bash advisor-plans/docker-check.sh cargo test -p vozen-store vote_reward --locked` → all pass.

### Step 3: Regression tests (in `vote_reward.rs` `mod tests`, model after the existing reward tests near line 340-430)

1. One vote at `NOW`: `user_premium_expiry` is `Some(NOW + 24h)`; `is_user_premium` true at `NOW + 24h - 1`, false at `NOW + 24h + 1` (put this one in `premium.rs` tests if helper access is easier there).
2. Two stacked votes (`NOW`, then `NOW + 12h`): the second grant returns `expires_at == NOW + 48h` (base `NOW + 24h` plus 24 h, below the `NOW + 12h + 48h` cap), and `user_premium_expiry` returns exactly `NOW + 48h` — not `NOW + 72h`.
3. Two stacked votes, then `initialize_vote_redemption_ledger` called **twice**: the ledger row count for that user stays 2.
4. A legacy `vote_reward` row with no ledger rows: one backfill inserts exactly one row; a second call inserts zero.

**Verify**: same command as Step 2 → all pass, including the 4 new tests.

## Done criteria

- [ ] `grep -rn "VOTE_REWARD_MS" crates/vozen-store/src/premium.rs` shows only the use site, no definition
- [ ] targeted tests pass; `bash advisor-plans/docker-check.sh gate` ends with `== GATE OK`
- [ ] `node tools/check-rust-contracts.mjs` exit 0 (no schema drift)
- [ ] `git status` shows only the two in-scope files (plus the README status row)
- [ ] one commit, e.g. `Grant 24 hours of Plus per Top.gg vote as the terms state`

## STOP conditions

- Any other code path reads the 48 h constant to show users a duration (e.g. a site API
  returning "48h") — report it instead of changing it.
- The ledger table lacks a `user_hash` column or the backfill is called from more places than `main.rs:2589`.
- A contract check reports a SQLite schema change.

## Maintenance notes

- Users currently enjoying the accidental extra day lose it on deploy; that matches the Terms.
- If the reward length ever changes, change only `vote_reward.rs` and the Terms together.
