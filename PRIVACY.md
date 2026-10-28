# Privacy Policy — Vozen

> _type it, hear it._

**Last updated:** 2026-10-17

Vozen is an **open-source**, **self-hosted** Text-to-Speech (TTS) bot for Discord: each operator runs their own instance on their own computer or server (VPS). This policy describes **faithfully** what data the instance stores, where, and how to remove it.

Because Vozen is self-hosted, the **data controller is the instance operator** (the person or entity running the bot), not the project authors. The operator directly controls the database file and the cache folder on their system.

---

## 1. What data is stored

Vozen stores the data below in a local **SQLite** database (by default `tts.db`, configurable via `DB_PATH`). Records are keyed mainly by **numeric Discord IDs** (snowflakes) — `guild_id` (server) and `user_id` (user). Those identifiers and the records linked to them can be personal data even though they are not names. Vozen does not normally store your email, phone number, postal address, Discord username, or avatar. Two narrow additions are: (1) the optional **spoken nickname** (`/voice nickname`), which is free text and may contain a name; and (2) for Ko-fi purchases, a **keyed one-way hash** of the buyer's email (never the plaintext email), used solely to link purchases and renewals to the same Discord account. For instant activation, Discord supplies the verified email of the signed-in account with the user's OAuth permission; Vozen hashes it in memory and immediately discards both the email and the transient hash.

| Data                            | Table                                      | Columns stored                                                                                                                                                                                                                                                              | What it refers to                                                                                                                                                                                                                                                                                                       |
| ------------------------------- | ------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Per-user voice preference**   | `user_voice`                               | `guild_id`, `user_id`, `voice_model`, `speed`, `engine`                                                                                                                                                                                                                     | The voice (model), speed, and engine a user set for themselves in a server (`/voice set`).                                                                                                                                                                                                                              |
| **Voice library shortcuts**     | `user_voice_favorite`, `user_voice_recent` | `user_id`, `voice_model`, `created_at` / `used_at`                                                                                                                                                                                                                          | Up to 25 favourite and 10 recently used voice model IDs so the browser can show personal shortcuts. No message text or audio is stored. Removed by `/privacy erase`.                                                                                                                                                    |
| **Per-server configuration**    | `guild_config`                             | `guild_id`, `tts_channel_id`, `autoread`, `default_voice`, `max_chars`, `rate_per_min`, `enabled`, `tts_role_id`, `locale`, `xsaid`, `autojoin`, `read_bots`, `text_in_voice`, `greet_on_join`, `greet_locale`, `antispam`, `stay_in_call`, `streak_announce`, `soundboard` | Server settings: auto-read channel, auto-read on/off, default voice, limits, kill-switch, authorized role, interface language, and the toggles for announcing who spoke (xsaid), auto-joining the call, reading other bots, reading the text-in-voice chat, and the join greeting.                                      |
| **Optional channel profiles**   | `channel_profile`                          | `guild_id`, `channel_id`, `auto_read`, `translation_enabled`, `default_voice`, `engine`, `speed`, `max_chars`, `read_bots`, `voice_channel_id`, `locale`, `effect`                                                                                                          | Admin-managed channel-specific reading policy exposed in the server dashboard. It stores IDs and policy choices only — never message text, translated text, audio, queue entries, or member content. Deleted with the server's configuration data after removal.                                                        |
| **Spoken nickname**             | `user_nickname`                            | `guild_id`, `user_id`, `nickname`                                                                                                                                                                                                                                           | The name a user chose to be **called out loud** by xsaid (`/voice nickname`). It is free text set by the user themselves; it may or may not match their real name.                                                                                                                                                      |
| **Personal abbreviations**      | `user_abbreviation`                        | `user_id`, `term`, `replacement`                                                                                                                                                                                                                                            | Personal text substitutions (global — they follow the person across servers) set by the user themselves (`/voice abbrev`).                                                                                                                                                                                              |
| **Voice effect**                | `user_effect`                              | `guild_id`, `user_id`, `effect`                                                                                                                                                                                                                                             | The voice filter (e.g. robot, echo) the person chose for themselves (`/voice effect`).                                                                                                                                                                                                                                  |
| **Blocklist**                   | `blocklist`                                | `guild_id`, `word`                                                                                                                                                                                                                                                          | Words the server blocked from being read (`/config blockword`).                                                                                                                                                                                                                                                         |
| **Pronunciation dictionary**    | `pronunciation`                            | `guild_id`, `term`, `replacement`                                                                                                                                                                                                                                           | Pronunciation substitutions set by the server (`/config pronunciation`).                                                                                                                                                                                                                                                |
| **TTS opt-out**                 | `tts_optout`                               | `guild_id`, `user_id`                                                                                                                                                                                                                                                       | A record that a user asked **not** to be read automatically (`/voice optout`).                                                                                                                                                                                                                                          |
| **Game scores**                 | `game_score`                               | `guild_id`, `user_id`, `points`, `wins`                                                                                                                                                                                                                                     | Minigame leaderboard (`/game`): accumulated points and matches won.                                                                                                                                                                                                                                                     |
| **"Top speakers"**              | `talk_stats`                               | `guild_id`, `user_id`, `spoken_count`, `streak`, `best_streak`, `last_date`                                                                                                                                                                                                 | How many of each person's messages the bot read + daily streaks, for `/topspeakers`.                                                                                                                                                                                                                                    |
| **Aggregate TTS usage**         | `talk_usage`                               | `guild_id`, `user_id`, `language`, `engine`, `spoken_count`                                                                                                                                                                                                                 | Counts how many queued messages used each detected/selected language and resolved TTS engine. It stores counters only, never message text. Used to show dominant language and engine in the operator console. Deleted for the user by `/privacy erase` and for the server by the server-data purge.                     |
| **Birthday**                    | `user_birthday`                            | `guild_id`, `user_id`, `month`, `day`                                                                                                                                                                                                                                       | **Only month and day** (never the year), set by the user themselves, so the bot can wish them a happy birthday in the call (`/birthday`).                                                                                                                                                                               |
| **Per-server Premium**          | `premium_guild`                            | `guild_id`, `expires_at`, `source`                                                                                                                                                                                                                                          | A server's Premium status: when it expires and the source (e.g. a redeem code).                                                                                                                                                                                                                                         |
| **Per-user Premium (Plus)**     | `premium_user`                             | `user_id`, `expires_at`, `source`                                                                                                                                                                                                                                           | A person's Plus status (perks that follow them across servers).                                                                                                                                                                                                                                                         |
| **Active vote reward**          | `vote_reward`                              | `user_id`, `rewarded_at`                                                                                                                                                                                                                                                    | Activates **24h of Plus** after an eligible top.gg vote. The raw Discord ID is deleted when the entitlement expires (or by `/privacy erase`).                                                                                                                                                                           |
| **Vote reward ledger**          | `vote_reward_ledger`                       | `user_hash`, `granted_at`                                                                                                                                                                                                                                                   | A keyed one-way HMAC of the Discord ID limits valid votes to **four rewards in a rolling 30-day window** and makes webhook retries idempotent. The raw ID cannot be recovered from this marker; the pseudonymous row is automatically deleted after 30 days.                                                            |
| **Optional notice rotation**    | `vote_promo_state`                         | `guild_id`, `last_post_at`, `last_kind`                                                                                                                                                                                                                                     | Alternates the admin-controlled Top.gg reward and Vozen support cards in the server's setup channel. Notices are off by default and can be enabled with `/config vote-reminders active:true`. At most one appears in any rolling **24 hours**, and each kind at most once per **48 hours**. This contains no user data. |
| **Premium codes**               | `premium_code`                             | `code`, `plan`, `days`, `seats`, `created_by`, `created_at`, `expires_at`, `redeemed_by`, `redeemed_at`                                                                                                                                                                     | Offline-generated Premium codes. `created_by`/`redeemed_by` are Discord IDs (who created the code / who redeemed it via `/redeem`); retained as purchase/redemption history.                                                                                                                                            |
| **Ko-fi renewal link**          | `kofi_supporter`                           | `email_hash`, `discord_id`, `updated_at`                                                                                                                                                                                                                                    | Links a Ko-fi purchase to your Discord account so subscription **renewals** keep working. Vozen **never stores your email** — only a one-way keyed hash (HMAC-SHA256) of it; the plaintext email is never written to disk.                                                                                              |
| **Personal pronunciations**     | `pronunciation_user`                       | `user_id`, `term`, `replacement`                                                                                                                                                                                                                                            | Pronunciation substitutions a person set for themselves (global — they follow the person across servers), via `/pronunciation`. Removed by `/privacy erase`.                                                                                                                                                            |
| **Premium pass**                | `premium_pass`                             | `user_id`, `seats`, `expires_at`, `source`                                                                                                                                                                                                                                  | A buyer's Premium pass: how many server licences it grants and when it ends. Financial/entitlement record (retained).                                                                                                                                                                                                   |
| **Premium pass activations**    | `premium_pass_activation`                  | `user_id`, `guild_id`, `activated_at`                                                                                                                                                                                                                                       | Which servers a pass owner has spent a licence on. Financial/entitlement record (retained).                                                                                                                                                                                                                             |
| **Ko-fi pending purchase**      | `kofi_pending`                             | `transaction_id`, `email_hash`, `plan`, `days`, `seats`, …                                                                                                                                                                                                                  | A paid purchase not yet linked to a Discord account, kept so the buyer can claim it on the site. Indexed by the receipt's transaction id + a one-way hash of the email (never the plaintext email). Auto-purged after 90 days.                                                                                          |
| **Activation consent evidence** | `kofi_activation_consent`                  | `transaction_id`, `confirmation_id`, `discord_id`, `accepted_at`, `terms_version`, `method`                                                                                                                                                                                 | Minimum evidence that immediate digital delivery was requested for an activated purchase. It contains neither an email nor an email hash and is retained with the minimum payment record for accounting, disputes, fraud prevention, and legal obligations.                                                             |
| **HD-TTS usage counter**        | `gcloud_usage`                             | `scope`, `key`, `month`, `chars`                                                                                                                                                                                                                                            | Monthly character count of the paid Google HD voice, per pool. New `user`/`pass` counters use a domain-separated HMAC of the scope, UTC month and Discord ID. Current-month quota survives `/privacy erase`; cleanup deletes these HMAC rows after the month ends. Legacy raw-ID rows are not migrated at startup: a future erase converts only the current-month count to HMAC and removes that user's raw rows. `guild`/`global` pools and legacy raw-row retention remain approximately three months. No message text or audio is stored in these counters.                                                                                              |

Stripe paid periods are stored separately in `stripe_paid_entitlement`: subscription ID,
Discord user ID, invoice ID, invoice/period timestamps, plan and seat limit. This is a
minimum financial/entitlement record, not message content or card data. It follows
the retained-payment-record rule in section 3, including operator-assisted deletion
or anonymization when no longer necessary; `/privacy erase` does not cancel paid
subscriptions or erase their accounting evidence. Plan previews are short-lived
signed confirmations and are not stored as a separate personal-data ledger.

Note on opt-out: running `/voice optout` **creates** a record with your `user_id` (so the bot remembers the preference). `/voice optin` removes that record. No other information about you is stored.

### 1.1 Website login (vozen.org)

When you log in on **vozen.org** with Discord (the account page and the server dashboard), Vozen uses Discord OAuth2 and requests only:

- **`identify`** — your Discord ID, username and avatar, to show your account and to verify that a request is really yours.
- **`email`** (account page) — the verified email on your Discord account, used only when you request instant Ko-fi activation. Vozen computes the same keyed hash used by the Ko-fi receipt in memory, then immediately discards the email and transient hash. Neither is stored, cached, or logged.
- **`guilds`** (server dashboard only) — the **list of servers you belong to**, used solely to show which servers you can configure (those where you have **Manage Server** and Vozen is present). Vozen never posts, leaves, or changes anything about those servers from this list.

The basic profile and server-list data are used **transiently** to answer the request and are **not stored** in the database — they may be held in memory for at most ~60 seconds (a short cache) and then discarded. The activation email path is deliberately uncached: the verified email and its transient hash exist only for that activation request and are never written to disk or logs. The access token stays in **your browser** (`sessionStorage`), is sent only to Vozen's own API over HTTPS, and is never written to disk on our side. A one-shot activation intent (containing no email) may also remain in `sessionStorage` for up to five minutes during Discord reauthorization. The dashboard only changes the same per-server settings you could change with `/config` in Discord.

For instant activation, processing the verified Discord email is necessary to perform the activation the user requests (GDPR Art. 6(1)(b), or Art. 6(1)(f) where the service is not contractual). The minimum activation-consent and transaction records are retained under applicable legal obligations and legitimate interests in accounting, fraud prevention, and resolving disputes (Art. 6(1)(c)/(f)).

### 1.2 Operator admin console

The instance **operator** (and only the operator — access is gated by the owner's
Discord identity plus a server-only signed session) has a private admin console to
run the service. It shows: active Premium/Plus entitlements and pending purchases
(to grant or revoke them); per server, the server's name, icon, member count,
**aggregate `talk_stats`** — the total number of messages the bot has read, the
number of active speakers, the top five speakers as `(Discord ID, count)`; the
dominant **language and TTS engine** for each global top talker, calculated from
aggregate `talk_usage` counters; and a
**server activity streak** (how many consecutive days at least one person spoke — a
server-level counter, not per-person); and a **global "top talkers" list** — the ten
people with the most messages read across all servers, shown with their Discord
username and avatar (fetched live from Discord's public profile for display, not
stored).

This introduces **no new data collection and no new recipient**: the figures are
**counts, never message content**, computed from `talk_stats` and `talk_usage` already
described in the table above; the server streak is a consecutive-day counter derived from the same
activity, holding no per-person data; the username and avatar in the top-talkers list
are the person's **public** Discord profile, fetched live for display and never
stored; the same top-speaker data is already public in each server via `/topspeakers`;
and the operator already controls the database directly (see §3).
The console is only an owner-facing view over data that is already disclosed,
already deletable via `/privacy erase`, and already auto-deleted 30 days after the
bot leaves a server (§3).

---

## 2. Message content

To synthesize speech, Vozen **reads the text content** of messages in the cases where it acts: the `/tts` command, auto-reading a configured channel, and mentions/replies to the bot.

**Message text is NOT stored in any database table.** It is processed **transiently** in memory: text cleaning → applying the pronunciation dictionary → checking the blocklist → speech synthesis → playback. After playback, the plain text is not persisted anywhere.

### 2.1 Audio cache

To avoid re-synthesizing repeated phrases, Vozen stores the **generated audio files** (`.wav`) in a local cache folder (`audio-cache/`, next to the database). It is important to understand exactly what this implies:

- **The production Rust cache uses a SHA-256 content identifier** derived from the cleaned text, voice model, speed and relevant synthesis options. The plain text does not appear in the file name. This hash is not encryption and must not be treated as a security boundary; the cached audio itself reveals the spoken content.
- **However, the `.wav` file itself is the spoken audio of the cleaned message.** Anyone with access to the instance's file system can play that file and hear the content. This is not a "hiding place" for the content — it is simply the generated audio, stored for reuse.
- The production Rust cache is **limited and regenerable**: it keeps at most ~500 completed files per engine and expires generated audio **seven days after its last write**. Reading a cached file does not extend that deadline. Expired hits are deleted instead of reused; a sweep runs before the Discord gateway starts and every hour thereafter, even without traffic, across all configured provider and TTS-file caches. It also removes expired synthesis temporary files and abandoned gTTS workspaces. Under normal operation, an idle file may remain until the next hourly sweep (up to seven days plus one hour). Startup fails if the initial sweep fails; later cleanup errors are logged and retried, so an operator must address storage or permission failures. Curated sound assets are not message-generated cache and are excluded. The legacy Node implementation still uses count-based eviction, not this age policy. Deleting cache loses no configuration; audio is re-synthesized when needed.

In short: **we do not store the original text**, only the generated audio keyed by hash, which is limited, regenerable, and deletable.

### 2.1.1 Explicit audio-file exports

`/tts-file` is an explicit request to generate a short audio attachment. It does **not** join a
voice channel or play audio to anyone. Each new export is synthesized in its own opaque temporary
directory, separate from the persistent voice caches. Vozen reads the audio into the upload buffer
and removes the local output before attempting delivery. Partial output is also removed when
synthesis or attachment reads fail, or when a request is cancelled. Cleanup failure does not produce
a successful export response. On the production container, this temporary directory is in the
`/tmp` memory-backed filesystem; an abrupt process/container failure may interrupt cleanup, and
stopping the container clears that filesystem. Files generated by older versions may remain in
the legacy TTS-file cache until the audio-cache retention policy above removes them.
It does not create a public URL, a persistent download record, or a database row for the export.

The explicit **Transcribe voice message** action accepts only supported audio hosted on Discord's
CDN. Vozen enforces HTTPS host allow-listing, media type, byte, decoded-duration and timeout limits,
disables redirects, decodes to a short-lived local WAV, transcribes locally, and deletes the
temporary directory after the attempt. The result is returned ephemerally to the requester. This
one-shot action does not listen to a call and is separate from live `/transcribe` consent.

### 2.2 Logs

Vozen writes **operational logs** to the console/terminal and (in the production runner) to the file `logs/vozen.log` next to the executable, with size-based rotation. The logs contain operational information: level, timestamp, command names, and error messages. **The logs do NOT include the content of users' messages.** Log retention is controlled by the instance operator (the system where the bot runs).

### 2.2.1 Identity-free operational aggregates

When the optional paid Google HD engine is configured and makes a real provider request, Vozen also stores small **daily service aggregates** in its local SQLite database: UTC day, fixed metric name, provider label, and a numeric total. These cover successful and failed synthesis attempts, fallback events, provider-call latency, and provider-charged character totals. A separate provider-health row stores only the provider label, its current health state, and health/degradation timestamps.

These operational rows do **not** contain message text, generated audio, Discord user or server IDs, channel IDs, OAuth tokens, request bodies, raw provider errors, or any link to a person. They are therefore not user- or guild-scoped records and cannot be selected by `/privacy erase`; the instance operator may purge them as part of normal operational retention. No public status endpoint is enabled by this telemetry.

### 2.2.2 Optional channel translation

Automatic channel translation is **off by default**. A server administrator must explicitly configure a
source text channel, a distinct destination text channel and a target locale, then enable the
feature with `/translate`. Vozen processes only the current message in that configured source
channel. Separately, a user may explicitly run `/translate text` or the **Translate** message action
in a server, DM or User App context; only the text they selected is processed and the result is
ephemeral. No mode translates attachments, embeds, message history, voice audio or transcripts.

Before an external provider call, Discord mentions are reduced to generic display-safe labels and
Vozen sends only the current plain text plus the requested target locale. It does not send the
author, server, channel, message URL, attachments, surrounding chat, or Discord identifiers to the
provider. Source text and translated text are not stored in Vozen's SQLite database or its metrics.
Mappings, personal target-language preferences, an optional member opt-out and bounded rolling **30-day** character counters are stored
only to operate the opt-in feature; these are removed by the server reset/removal lifecycle and by
`/privacy erase` where user-scoped. Translation output is a normal Discord message in the selected
destination channel, so Discord retention controls that posted message.

An instance has no translation provider until its operator configures one. Provider configuration
is never shown to ordinary members. Translation is text-only unless a member explicitly enables
their own translate-before-speaking preference in a server; that path still obeys Vozen's separate
same-call, role, rate-limit and queue authorization rules and falls back to the original text.

### 2.3 Speech-to-text / transcription (consent-first)

Transcription (`/transcribe`, a Premium feature) turns the speech in a voice channel into text messages. It is strictly **consent-first** and processes audio **locally** (a Whisper sidecar on the instance's machine — nothing is sent to an external service, nothing is used to train AI). How it works:

- **Consent first, per speaker.** Only people who have **explicitly consented** in that server are transcribed — an on-screen Allow button that **only that person** can press. Consent is asked **once per server** and remembered (`stt_consent` stores your user ID, the server ID and `consent_at`); people who never consent are simply **not transcribed** while the rest of the call continues. Revoking consent stops future transcription of you.
- **Audio is never persisted.** For each utterance the bot writes a **short-lived temporary file** (in the OS temp directory) that the local Whisper sidecar reads to produce text, then **deletes it immediately** afterwards. The audio is never kept, never added to any database table, and never sent to an external service.
- **Daily usage is capped per person.** Vozen Plus allows 60 minutes of effectively-transcribed audio per server UTC day; a Premium server allows 30 minutes per person. The counter is keyed by the server's UTC date and user ID, and is calculated by the bot/server clock — changing a member's PC or phone clock cannot reset it. The counter is not tied to a guild and is removed by `/privacy erase`.
- **The bot announces transcription** in the channel when it **starts and stops**, and **auto-stops** when nobody consented remains in the call.
- **Transcriptions are normal Discord messages — and therefore permanent.** The text the bot posts lives in the channel like any other message. This means it is **outside `/privacy erase`** (which only removes data from the bot's own database, not messages already posted to Discord). Revoking consent stops new transcriptions but does **not** retroactively delete messages already posted; to remove those, delete the messages in Discord (or ask a server moderator to). Only the **consent record** (`stt_consent`) is bot-side data, and that is removed by `/privacy erase`.

> Compliance note: transcribing a person's speech acts on that person, so it requires their explicit prior consent (handled above). The instance operator is responsible for honoring consent and removal requests.

---

## 3. Data retention and deletion

Because Vozen is self-hosted, **the instance operator directly controls** the SQLite file (`DB_PATH`) and the cache folder (`audio-cache/`) — both are local files that can be deleted at any time. On a Docker deployment, deleting everything is `docker compose down -v` (removes the volume with the database and the cache).

For users and server administrators, the bot's commands allow data to be removed:

| To remove...                    | Use...                           | Notes                                                                                                                                                                                                                                                                                  |
| ------------------------------- | -------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Your voice preference           | `/voice reset`                   | Deletes your record in `user_voice`.                                                                                                                                                                                                                                                   |
| Your spoken nickname            | `/voice nickname` (with no name) | Deletes your record in `user_nickname`.                                                                                                                                                                                                                                                |
| Your opt-out (be read again)    | `/voice optin`                   | Deletes your record in `tts_optout`.                                                                                                                                                                                                                                                   |
| Your transcription consent      | `/transcribe revoke`             | Deletes your row in `stt_consent`; you stop being transcribed. Messages already posted are normal Discord messages (delete in Discord).                                                                                                                                                |
| Server configuration            | `/config reset`                  | Restores `guild_config` to its default values. **Note:** the reset does **NOT** clear the blocklist or the pronunciation dictionary.                                                                                                                                                   |
| A word from the blocklist       | `/config blockword remove`       | Removed individually (not by `/config reset`).                                                                                                                                                                                                                                         |
| A pronunciation term            | `/config pronunciation remove`   | Removed individually (not by `/config reset`).                                                                                                                                                                                                                                         |
| **Your erasable personal data** | `/privacy erase`                 | One command (asks you to confirm). Deletes personal settings, preferences, consent, daily transcription usage, scores, `talk_stats`, and `talk_usage` tied to your user ID across **all** servers. Paid rights and minimum financial records follow the separate retention rule below. |

> **To delete your erasable personal data, use `/privacy erase`** — it asks you to confirm, then removes personal settings, preferences, consent, daily transcription usage, scores, `talk_stats`, `talk_usage`, and any active free vote entitlement tied to your user ID across every server (`user_voice`, `user_nickname`, `user_abbreviation`, `pronunciation_user`, `user_effect`, `tts_optout`, `stt_consent`, `stt_daily_usage`, `game_score`, `talk_stats`, `talk_usage`, `user_birthday`, `vote_reward`, and the other user-scoped operational records listed above). Transcription text already posted to a channel is a normal Discord message, not bot-side data (see section 2.3). Erasure also retains the current UTC month's pseudonymous Google HD quota count solely for cost control; cleanup removes that HMAC row after month end. Erasure does **not** reset the rolling vote-reward limit: the keyed pseudonymous HMAC ledger is retained for no more than 30 days solely for rate limiting and abuse prevention. Active paid entitlements and the minimum purchase and activation-consent records needed for accounting, fraud prevention, disputes, and legal obligations may also be retained. Retained records are restricted to those purposes and deleted or anonymized when no longer necessary; contact the operator to exercise a legal right.

Older installations can retain a `user_persona` table from a previous runtime. The Rust runtime does not create new persona records, but user erasure and departed-server cleanup also remove matching records from that table when it exists.

Erasing the live database does not rewrite earlier backup snapshots. Operators must control access and retention for those copies separately, including external copies, and account for erasure requests before returning a restored snapshot to service. The production instance's managed daily SQLite backups retain the current Lisbon calendar date and the preceding 29 dates; historical backups and manually exported copies are outside that automated retention mechanism.

The audio cache is regenerable and self-limiting (see section 2.1); deleting it loses no configuration.

**Automatic retention limit (servers).** When Vozen is removed from a server, that server's data (configuration, blocklist, pronunciations, stats, scores, per-user voice settings for that server, etc.) is **automatically deleted 30 days later** if the bot is not re-invited. The 30-day grace period exists so an accidental kick or a short outage does not wipe everything immediately; re-inviting the bot within that window cancels the deletion. An active paid entitlement and the minimum payment ledger required for accounting, fraud prevention, disputes, or law may be retained for those purposes only and removed or anonymized when the applicable period ends.

---

## 4. Third parties

### 4.1 Speech synthesis engine (where the text goes)

To generate the audio, the **cleaned text to be synthesized** is handed to a TTS engine. Which one depends on the instance's `TTS_ENGINE` configuration:

| `TTS_ENGINE` | Where the text goes                                                                                                                        | Note                                                                  |
| ------------ | ------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------- |
| `piper`      | **Stays local** — runs on the instance's machine, **sends nothing external**.                                                              | Code default.                                                         |
| `gtts`       | **Sent to Google** (`translate.google.com`) to generate the audio. [Google's Privacy Policy](https://policies.google.com/privacy) applies. | Legacy, explicit operator opt-in; this is not an official Google API. |
| `router`     | By default uses **gTTS (Google)** and falls back to **Piper (local)** if Google fails — that is, the text **may** be sent to Google.       | —                                                                     |
| `neural`     | **Sent to OpenAI** (`api.openai.com`). Requires the operator's API key.                                                                    | Opt-in.                                                               |

> In every case, **only the text to be read** (already cleaned) is sent, not user IDs or history. The external engine returns audio; Vozen does not store the text↔user association.

### 4.2 Other services

- **Discord.** Vozen connects to Discord's API/gateway to work (receiving messages, joining voice channels, responding). Use of Discord is governed by [Discord's Privacy Policy](https://discord.com/privacy).
- **Bot lists (opt-in, e.g. top.gg).** If `TOPGG_TOKEN` is set, Vozen publishes only its **server count**. When someone votes, top.gg sends Vozen that voter's Discord user ID in a signed webhook. Each eligible valid vote grants **24h of Plus**, up to **four rewards in a rolling 30-day window**; the entitlement cannot extend beyond 48 hours ahead. The raw ID is kept only for the active entitlement; a keyed one-way HMAC ledger and provider event ID are retained for 30 days for rate limiting and replay protection. No message content is shared.
- **Error webhook (opt-in).** If `ERROR_WEBHOOK_URL` is set, technical error reports (stack traces) are sent to a private **operator** channel for monitoring. They are not designed to include message content.
- **Activation-help webhook (opt-in).** If you use the "get help" option on the account page (when you cannot activate a purchase yourself), the **email address you enter** and your Discord ID are sent to a private **operator** channel so the operator can find your Ko-fi order and activate your pass by hand. This is the one case where an email you type is transmitted (not stored in the database); it is used only to locate your order for that support request. Requires `CLAIM_HELP_WEBHOOK_URL` (or `ERROR_WEBHOOK_URL`) to be set — otherwise nothing is sent.
- **Operator admin console (opt-in).** The operator may enable a private admin console (the `/api/admin/*` routes, gated by the operator's own Discord login — only the owner's Discord identity may enter) to grant/revoke Premium and to view **aggregate usage per server**: the server's name and icon (from Discord), how many messages Vozen has read there, top talkers, and each global top talker's dominant language and TTS engine from the `talk_usage` counters. These are counts, never message content. User-scoped `talk_stats` and `talk_usage` are removed by `/privacy erase`; server-scoped rows are removed by the automatic purge after the bot leaves a server. Inert unless the `ADMIN_*` env vars are configured.
- **Optional translation provider.** If an operator explicitly configures Azure-compatible text translation, the current plain text from an administrator-configured source channel and its target locale are sent to that provider solely to produce the translated text. It remains disabled without complete operator configuration; no message history, IDs, attachments, embeds or voice data are sent.

**No data sale. No third-party analytics. No trackers.** Vozen does not sell, rent, or share data with third parties for marketing purposes, and does not integrate external analytics services.

### 4.3 Children and US privacy requests

Vozen is not directed to children under 13 (or a higher local minimum age). If an operator learns that an underage child supplied personal data, the operator must stop the relevant processing and delete it unless retention is legally required. A parent or guardian may contact the operator using section 5.

Where US state privacy law applies, residents may request access, correction, deletion, or a portable copy and may appeal a refusal by contacting the operator. Vozen does not sell personal data, share it for cross-context behavioural advertising, run targeted advertising, or discriminate against a person for exercising a privacy right. An authorised agent may submit a request where local law permits, subject to identity and authority verification.

---

## 5. Contact / Responsible party

The **responsible party** for a Vozen instance is the **operator** who runs it. For questions about your data on a specific server, to report a problem/abuse, or to request the removal of your data, contact that server's administrator or the bot's operator.

> **Official public Vozen instance:**
>
> - Support / reports / data requests: **[support server](https://discord.gg/4kYw2WUbNN)** (`discord.gg/4kYw2WUbNN`)
> - Source code: <https://github.com/Rexy40407/vozen>
>
> **Running your own instance?** Replace the contact above with your own before making the bot public.

The Vozen project authors provide only the software (open-source); each operator of a third-party instance is responsible for the data that instance processes.

---

## Nota / Note (EN)

_Vozen is a self-hosted, open-source Discord TTS bot (AGPL-3.0). The instance operator is the data controller. The bot stores Discord numeric IDs plus the preferences and service records described above. Message text is processed transiently and is not stored in a database table; generated audio is kept only in a bounded, regenerable cache. Where text goes depends on `TTS_ENGINE`: Piper is local; legacy gTTS/router modes may send text to Google; neural sends text to OpenAI; Premium Google HD uses the official Google Cloud TTS API. Transcription is consent-first and local on the configured instance. Vozen does not sell data, run third-party advertising analytics, or use message content for model training. Use `/privacy erase` to delete your data, or contact the [support server](https://discord.gg/4kYw2WUbNN) to request access to a copy of it or exercise any other data right._

# Supabase mirror erasure

When the optional PostgreSQL mirror is enabled, `/privacy erase` and guild purge write a durable
privacy tombstone in the same SQLite transaction as the local deletion. The mirror applies that
tombstone to user/guild aggregates and deletes transient raw outbox payloads. Financial evidence,
idempotency markers and the documented anti-abuse HMAC remain retained where required by the policy.
