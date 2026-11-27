# Vozen — Monetization Plan

> Status: **implemented pricing and entitlement model**. Piper and the essential TTS
> workflow remain free; Kokoro/Google HD, higher limits and extras require personal Plus
> or server Premium. Free is marketing; Premium funds the servers.

## Pricing (decided)

| Tier | Price | Scope | Notes |
|------|-------|-------|-------|
| **Vozen Premium** | **€6.99 / month** or **€66.99 / year** | up to **2 servers** | Full Premium benefits for everyone in the activated servers |
| **Vozen Premium Max** | **€14.99 / month** or **€133.99 / year** | up to **5 servers** | The same benefits with five server licences |
| **Vozen Plus** | **€3.99 / month** or **€37.99 / year** | per **user** | Personal perks that follow the user across servers |

Payment: the hosted plans currently use the site's **Stripe Checkout** flow (the six
monthly/yearly price IDs are injected by the production deploy workflow). Discord **Premium
Apps** (native, in-app) remains the intended second channel once the app is eligible; the
legacy **`/redeem` codes** path is still supported for manual grants. Ko-fi is no longer
used: its webhook and claim routes were removed, and passes it granted run until they expire.

---

## 🆓 FREE — the essential Piper-powered core stays free

- **Core TTS:** auto-read channel, `/tts`, "Speak" context menu, 35 languages, automatic
  language detection, mixed-language synthesis in one sentence.
- **Voices:** all 38 current Piper voices, speed control, `/voice preview`.
- **Personalization:** spoken nickname, the 5 personas (pirate, uwu, Yoda, cowboy,
  medieval), 10 personal abbreviations.
- **Fun:** `/joke`, `/laugh`, `/8-ball`, `/fortune`, `/fact`, `/wyr`, birthday shout-outs,
  `/top-speakers`.
- **Games:** **13 of the 16 minigames + leaderboard — 100% free, always.** These drive
  invites and engagement; paywalling them would kill growth. Word Chain, Wordle and Chess
  are the three Premium games.
  - **Leaderboard: weekly reset is FREE too** (decided). Premium only adds *automatic
    roles for the weekly top players*.
- **Admin:** blocklist, pronunciation dictionary, rate-limit, role gating, kill-switch,
  join greetings, xsaid — all of it.

---

## 💎 PREMIUM — new, exclusive value

| Feature | Why it's Premium |
|---------|------------------|
| **Premium voices (Kokoro / top neural)** | ⭐ The anchor. Already on the roadmap ("hybrid engine phase 2: Kokoro"). Visibly better voices are what TTS competitors charge for, and they cost real CPU/infra. |
| **24/7 in the call** | Free bot leaves after 5 min alone; Premium stays. Classic voice-bot perk, cheap to build. |
| **Custom join greeting** | Server writes its own line instead of the fixed "Hello {name}". |
| **Multiple read channels** | Free: 1 tts-channel; Premium: several. |
| **Priority synthesis queue** | Under load, Premium servers synthesize first. |
| **Full soundboard + uploads** | Server's own custom sounds (⚠️ uploads = moderation risk, decide later). |
| **Custom game-winner announcement** | Cosmetic: Vozen announces the winner with the server's own line. |
| **Automatic roles for weekly top** | The Premium slice of the (otherwise free) leaderboard. |

---

## ⚖️ BOTH tiers, Premium gets an upgrade

| Feature | Free | Premium |
|---------|------|---------|
| Personal abbreviations | 10 | 50 |
| Pronunciation dictionary | 25 entries | 250 |
| Max characters / message | 2000 | 4000 |
| Personas | 5 current | +5 new (robot, baby, villain, DJ, grandpa) |
| **Voice effects** *(new)* | 2 samples (robot, echo) | all 8 (deep, chipmunk, radio, phone, underwater, demon…) |
| **Soundboard `/sfx`** *(new)* | 5 sounds | 25+ and uploads |
| **Keyword sound triggers** *(new)* | 2 triggers | 20 |
| **`/translate` + speak** *(new)* | 10 / day | unlimited |

---

## 🆕 New features to build (and where they land)

- **For Free (growth):** lite voice effects, mini soundboard, limited triggers,
  `/translate` with a daily quota — each gives the taste that converts.
- **For Premium (revenue):** the full version of each + Kokoro + 24/7 + custom greetings.
- **Growth loop:** each eligible `/vote` on top.gg unlocks **24h of Vozen Plus**; cap it at four rewards in a rolling 30-day window and no more than 48 hours ahead.
  Free for us, addictive for them, boosts our ranking.

---

## ⚠️ Honest notes (read before flipping the switch)

1. **Site copy:** say exactly which engine belongs to each tier: Piper is free;
   Kokoro and Google HD require Plus or server Premium. Avoid blanket “no paywall” claims.
2. **gTTS is an unofficial endpoint** — can't sell reliability on top of it. Premium must
   run on Piper/Kokoro (local, controlled). Revenue pays for serious infra.
3. **The bot runs on a home PC.** Before charging, it needs a VPS (~€5–10/mo). First goal
   of revenue is to cover that.
4. **Timing:** grow first (site, top.gg, votes), monetize at ~50–100 active servers.
   The current paid bundles cover 2 or 5 servers.

---

## Technical rollout (small, additive — nobody loses anything)

1. `premium_guild` / `premium_user` tables + `redeem_code` table (SQLite).
2. `isGuildPremium()` / `isUserPremium()` helpers (expiry-based).
3. `/redeem <code>` command + `/premium` status; offline code-gen script for Ko-fi/Patreon.
4. Gate only the **new** features (voice effects, soundboard, translate…) — existing free
   features are never gated.
5. **Discord Premium Apps (entitlements) — BUILT, inert until onboarded.** `src/premium/
   entitlements.ts` + `syncDiscordEntitlements()` map active Discord subscriptions to the
   same `premium_*` tables (`source='discord'`), reconciling on ClientReady + on every
   `Entitlement*` gateway event. Activates only when `PREMIUM_GUILD_SKU_ID` /
   `PREMIUM_USER_SKU_ID` are set — until then it's a no-op and the bot runs on `/redeem`.

---

## ✅ Discord Monetization Policy — compliance (required once verifiable)

Discord's Monetization Requirements (in force since 7 Oct 2024) say: if you sell any paid
capability, you must **also** sell it through Discord Premium Apps, at a **price no higher**
than any other channel (price parity). It applies **only to the extent Premium Apps supports
your locale and offering type**, and Premium Apps requires a **Verified**, **Team-owned**
app. Vozen is neither yet (verification needs ~75 servers), so the rule does not yet bind us
— but Portugal is a supported locale, so it will the moment we can onboard.

### Price parity map (SKU prices set in USD, auto-localized by Discord)

| Offering | Ko-fi / external | Discord SKU (USD) | Discord type | Notes |
|---|---|---|---|---|
| **Vozen Premium** (2 servers) | €6.99 / mo | Configure at or below the external EUR-equivalent | subscription | Preserve price parity during Discord onboarding. |
| **Vozen Premium Max** (5 servers) | €14.99 / mo | Configure at or below the external EUR-equivalent | subscription | Preserve price parity during Discord onboarding. |
| **Vozen Plus** (per user) | €3.99 / mo | Configure at or below the external EUR-equivalent | user subscription | Personal entitlement. |
| **Annual plans** | €37.99 / €66.99 / €133.99 | — | annual sub ⏳ not yet supported | Offer via Discord once annual subs ship; grace period applies. |

> Rule of thumb: the **final** Discord price (after any discount) must be **≤** the price on
> any other channel. Keep the USD SKU at or below the EUR-equivalent so parity always holds.

### Manual steps (portal-only — cannot be done in code)

1. Create a **Team** in the Developer Portal and transfer the app to it.
2. Apply for **app verification** (opens at ~75 servers).
3. Complete **Monetization onboarding** (payment info + agree to Monetization Terms — Team
   Owner only).
4. Create the **SKUs** (guild sub, user sub) with the USD prices above.
5. Set `PREMIUM_GUILD_SKU_ID` / `PREMIUM_USER_SKU_ID` in the bot's env → the entitlement
   sync (already built & tested) activates automatically.
6. Put the Privacy Policy + Terms URLs and the support server on the app's profile.

Fee note: Discord takes a Platform Fee of **15%** (Growth Tier, first $1M) + payment
processing; factor into margins.

> Known limitation (rare edge): if a guild has **both** a `/redeem` code and a Discord
> subscription, the `premium_*` row keeps whichever expiry is further out; the sync never
> shortens an existing premium, and only reconciles/revokes rows whose `source='discord'`.
