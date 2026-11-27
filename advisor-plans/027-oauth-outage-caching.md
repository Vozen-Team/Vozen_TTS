# Plan 027: Stop caching Discord outages as invalid OAuth tokens

> **Executor instructions**: Read `advisor-plans/README.md` § "Bug audit 2026-11-25"
> first. Follow the steps, run every verification, honour the STOP conditions, then set
> this plan's README row to `DONE (<commit>)`.
>
> **Drift check (run first)**: `git diff --stat c1dd4a9..HEAD -- crates/vozen-api/src/discord_oauth.rs`
> Any change → compare excerpts; mismatch = STOP.

## Status

- **Priority**: P2 · **Effort**: S · **Risk**: LOW · **Depends on**: none · **Category**: bug
- **Planned at**: commit `c1dd4a9`, 2026-11-25

## Why this matters

The claim/checkout identity resolver caches the result of every Discord lookup for 60 s, including
**temporary** failures (Discord 429/5xx or a network error). A single Discord hiccup therefore makes
checkout, the billing portal, receipt linking and the account page answer `401 invalid_token` for that
user for a full minute, which can log them out in the frontend. The dashboard resolver already does the
right thing (it drops the cache entry on `Unavailable`). This fails closed — it is an availability bug,
not a security hole.

## Current state (`crates/vozen-api/src/discord_oauth.rs`)

- Line 25: `const IDENTITY_TTL_MS: i64 = 60_000;`
- Lines 66-72: HTTP 401/403 → `DiscordHttpError::InvalidToken`; any other non-success (incl. 429, 5xx) and
  network errors → `DiscordHttpError::Unavailable`.
- Lines 117-126: `enum IdentityResolutionError { CachedRejection, OAuthInvalidToken, OAuthUnavailable,
WrongAudience, MissingIdentifyScope, UserInvalidToken, UserUnavailable, UserMismatch }`.
- Lines 177-189:
  ```rust
  if let Some(cached) = self.cache_get(cache_key, now) {
      return cached.ok_or(IdentityResolutionError::CachedRejection);
  }
  let identity = self.resolve_claim_identity_uncached(bearer).await;
  self.cache_put(cache_key, identity.clone().ok(), now);
  identity
  ```
- Reference behaviour: `crates/vozen-api/src/dashboard_oauth.rs:211-218` caches `Ok` and `InvalidToken`,
  and **removes** the entry on `Unavailable`.
- Existing tests: `discord_oauth.rs` `mod tests` from line 446 (8 tests; `resolve_claim_identity("sensitive-token")` at ~661-674).

## Scope

**In scope**: `crates/vozen-api/src/discord_oauth.rs` (code + tests).
**Out of scope**: the `resolve_identity` trait signature (`Result<_, ()>`), `premium_api.rs`, `stripe_api.rs`
response codes (returning 503 there is a follow-up that needs a trait change).

## Steps

### Step 1: Cache only definitive answers

After `resolve_claim_identity_uncached`, match the result:

- `Ok(identity)` → `cache_put(key, Some(identity), now)`
- `Err(OAuthInvalidToken | WrongAudience | MissingIdentifyScope | UserInvalidToken | UserMismatch)` → `cache_put(key, None, now)`
- `Err(OAuthUnavailable | UserUnavailable)` → remove any entry for `key` (add a `cache_remove` next to
  `cache_put` at ~line 286 if none exists); do not cache.

### Step 2: Tests

In `mod tests`, using the existing fake HTTP pattern of the tests near line 661: (a) first lookup gets a
503/429 from the fake Discord, second lookup succeeds → second call returns `Ok` (not `CachedRejection`);
(b) a 401 is still cached (second call returns `CachedRejection` without another HTTP call).

**Verify**: `bash advisor-plans/docker-check.sh cargo test -p vozen-api discord_oauth --locked` → all pass, 2 new tests.

## Done criteria

- [ ] `grep -n "identity.clone().ok()" crates/vozen-api/src/discord_oauth.rs` → no output
- [ ] targeted tests pass; `bash advisor-plans/docker-check.sh gate` → `== GATE OK`
- [ ] only `discord_oauth.rs` changed; one commit, e.g. `Do not cache temporary Discord failures as invalid sign-ins`

## STOP conditions

- The fake HTTP layer in tests cannot return a non-401 failure without changing production code.
- Another resolver in this file (e.g. `resolve_verified_email`) shares the cache and would change behaviour.

## Maintenance notes

- Follow-up (not here): let callers distinguish "unavailable" so they can answer 503 instead of 401.
