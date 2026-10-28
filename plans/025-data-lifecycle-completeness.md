# Plan 025: Data-lifecycle completeness — broaden the rot-guard + erase/disclose `kofi_supporter` & `gcloud_usage`

> **Executor instructions**: TDD mandatory (this is compliance-critical). Write the
> broadened rot-guard test FIRST and watch it flag the two tables, THEN fix.
> Update this plan's row in `plans/README.md` when done.
>
> **Drift check (run first)**: `git diff --stat f1a1ac1..HEAD -- src/store/dataLifecycle.ts src/store/db.ts src/store/premium.ts src/store/gcloudUsage.ts tests/dataLifecycle.test.ts`

## Status

- **Priority**: P1 (Discord Developer Policy + GDPR — `/privacy erase` must be complete)
- **Effort**: S–M
- **Risk**: LOW
- **Depends on**: none
- **Category**: security / privacy / compliance
- **Planned at**: commit `f1a1ac1`, audit findings DATA-01, DATA-02, DATA-03

## Why this matters

`src/store/dataLifecycle.ts` is the single source of truth for GDPR/Policy data
deletion (`purgeGuild` on guild-leave, `eraseUser` for `/privacy erase`). Its
safety net — the rot-guard test — has a **systematic blind spot**: it only
requires categorization for columns literally named `user_id`/`guild_id`. Discord
user IDs stored under **other** column names escape both the guard AND the
deletion lists, so `/privacy erase` silently leaves them behind while the test
stays green.

Two such tables exist today, both undeleted by `/privacy erase`:
- **`kofi_supporter`** — links a user's `discord_id` to the HMAC of their email
  (PII-derived). Never deleted anywhere; absent from both erase and retained lists.
  Contradicts `PRIVACY.md:114` ("removes all data tied to your user ID").
- **`gcloud_usage`** — `key` holds a user Discord ID when `scope` is `'user'`/`'pass'`.
  No DELETE exists at all; not purged on guild-leave, not erased, not disclosed.

## Current state (exact locations)

- **Rot-guard**: `tests/dataLifecycle.test.ts:~171,177` — asserts categorization
  only `if (cols.includes('guild_id'))` / `if (cols.includes('user_id'))`.
- **`kofi_supporter`** (`src/store/db.ts:~233`): columns `email_hash, discord_id, updated_at`.
  Writers/readers only: `src/store/premium.ts:296-316` (`rememberKofiSupporter`,
  `lookupKofiSupporter`) — no delete. Absent from all 4 lists in `dataLifecycle.ts:17-65`.
- **`gcloud_usage`** (`src/store/db.ts:~219-225`): columns `scope, key, month, chars`.
  Store `src/store/gcloudUsage.ts` has only `get`/`add` (UPSERT) — no delete.
  Absent from all 4 lists. Scope semantics documented at `gcloudUsage.ts:8-12`.
- **`eraseUser`**: `src/store/dataLifecycle.ts:92-112` iterates `USER_ERASE_TABLES`
  deleting `WHERE user_id = ?`; note `kofi_supporter` is keyed by **`discord_id`**,
  not `user_id`, so it needs a bespoke delete, not a list entry.

## Scope

**In scope**: `tests/dataLifecycle.test.ts` (broaden guard), `src/store/dataLifecycle.ts`
(add the two deletions / categorizations), possibly `src/store/gcloudUsage.ts` (a
`deleteUserGcloudUsage` helper) and a retention purge, `PRIVACY.md` disclosure of
`gcloud_usage` (or fold into plan 026's PRIVACY reconcile — coordinate).
**Out of scope**: `premium_code.created_by/redeemed_by` (financial, intentionally
retained — but the broadened guard must ALLOW-LIST them, not delete them); the
financial tables in `USER_RETAINED_TABLES`; encryption.

## Approach

**Decisions to bake in (state them in code comments):**
- `kofi_supporter`: **erase** it on `/privacy erase` (keyed by `discord_id`). It is
  self-healing — the next Ko-fi renewal re-learns the mapping (per its own comment
  at `db.ts:227-235`). Do NOT put it in retained.
- `gcloud_usage`: for `scope IN ('user','pass')` rows, **erase** on `/privacy erase`
  keyed by `key = ?`. Separately add a monthly-rollover purge (drop `month < cutoff`,
  e.g. keep 3 months) so the table doesn't grow forever; guild-scope rows
  (`scope='guild'`) can be purged on guild-leave OR covered by the rollover purge.
- Broaden the rot-guard so a future identifier column forces a conscious decision.

## Steps

### Step 1 (RED): broaden the rot-guard test

In `tests/dataLifecycle.test.ts`, extend the schema scan so that, in addition to
`user_id`/`guild_id`, any column whose name matches an identifier heuristic —
`/_(id|by)$/` OR name `=== 'key'` OR `=== 'discord_id'` — must be EITHER in a
lifecycle list OR in an explicit `REVIEWED_EXEMPT` set (a new `as const` in the
test naming columns intentionally not lifecycle-scoped, e.g.
`premium_code.created_by`, `premium_code.redeemed_by`, and the financial-retained
ones). Run it → it should now **fail**, listing `kofi_supporter.discord_id` and
`gcloud_usage.key` as uncategorized. That failure is the proof the guard works.

(Implementation note: the guard reads `PRAGMA table_info(<table>)` per table, as
it already does for user_id/guild_id — reuse that enumeration.)

### Step 2 (GREEN): erase `kofi_supporter` on user erase

Add a delete of `kofi_supporter WHERE discord_id = ?` inside the `eraseUser`
transaction in `src/store/dataLifecycle.ts:92-112` (it can't go in
`USER_ERASE_TABLES` because that loop keys on `user_id`; add it like the
`user_clone` special-case already there). Add `kofi_supporter` to the test's
allow-list resolution by virtue of it now being deleted (the guard should accept
"deleted in eraseUser" — make the guard's satisfaction check cover the bespoke
deletes too, or add `kofi_supporter` to a small `ERASED_BESPOKE` set the guard
treats as covered).

### Step 3 (GREEN): erase + purge `gcloud_usage`

- Add `deleteUserGcloudUsage(db, discordId)` in `src/store/gcloudUsage.ts` deleting
  `WHERE key = ? AND scope IN ('user','pass')`. Call it from `eraseUser`.
- Add a retention purge `purgeOldGcloudUsage(db, cutoffMonth)` (`DELETE WHERE month < ?`)
  and wire it into the existing daily maintenance tick (find where
  `kofiPending` purge / any daily job runs — grep `startPendingPurgeJob` — and
  mirror it). Categorize `gcloud_usage` in the guard as covered.

### Step 4: disclosure

Add `gcloud_usage` (and confirm `kofi_supporter`) to `PRIVACY.md §1` — OR, if
executing plan 026 in the same batch, hand this to 026 and just note it here.
Keep it factual: Discord ID + monthly HD-TTS char count, erasable via `/privacy erase`.

## Test plan

- Broadened rot-guard (Step 1) — the RED→GREEN driver.
- New tests: `eraseUser` removes a seeded `kofi_supporter` row (by discord_id) and
  a seeded `gcloud_usage` `scope='user'` row; `purgeOldGcloudUsage` drops old
  months but keeps the current one. Follow `tests/dataLifecycle.test.ts` patterns.
- `npm run check` green.

## Done criteria

- [ ] Rot-guard flags identifier-shaped columns (not just `user_id`/`guild_id`) and passes only with every one categorized or explicitly exempt
- [ ] `eraseUser` deletes `kofi_supporter` (by `discord_id`) and `gcloud_usage` user/pass rows — proven by tests
- [ ] `gcloud_usage` has a retention purge wired to the daily job, with a test
- [ ] `gcloud_usage` disclosed in `PRIVACY.md` (here or via plan 026)
- [ ] `npm run check` exits 0

## STOP conditions

- If broadening the guard surfaces MORE uncategorized columns than the two named
  here, STOP and list them — each needs a conscious erase-vs-retain decision, not
  a blanket delete (some may be financial-retained like `premium_code`).
- If `gcloud_usage` guild-scope rows have a retention rationale you can't
  determine, default to the monthly-rollover purge (safe) and note the open
  question rather than guessing a guild-leave delete.

## Maintenance notes

- The broadened guard is now the gate: any new table with an identifier column
  fails CI until categorized or explicitly exempt — this is the intended friction.
- `kofi_supporter` erase is keyed by `discord_id`; if the schema ever renames that
  column, update the bespoke delete.


## Current Rust Google HD quota retention (2026-10-28)

The approved Rust implementation removes raw user/pass identifiers on an actual future
privacy erase while retaining the current UTC month's character count as a domain-separated
HMAC of scope, month and user ID. New personal/pass counter writes use that HMAC; reads
and reservations include any existing raw row, without a startup data migration. The
existing stable VOTE_REDEMPTION_SECRET and pinned fingerprint are reused, with no new
schema, dependency or configuration variable. Missing or mismatched keys fail closed.
The daily cleanup removes HMAC counters after month end; existing legacy raw-row, guild
and global retention remains unchanged. Counter rows contain no text or audio. Pending
user-data invalidation, paid grants and the global daily budget remain intact. Mirror
tombstones still delete raw user/pass rows; no existing database is rewritten by deployment.
