# Self-hosting Vozen Rust

Vozen is now a Rust workspace. The Discord runtime, SQLite store, TTS engines,
games, Premium/OAuth API, Top.gg and Ko-fi integrations live under `crates/`.
The Node/TypeScript bot and its old compose files were removed from `main`.

## Prerequisites

- Linux VPS (x86_64 or ARM64) with Docker Engine and Compose v2;
- a Discord application token and client ID;
- Piper models and optional Kokoro/Whisper assets for the features you enable.

## Production compose

```sh
git clone https://github.com/Rexy40407/vozen.git
cd vozen
cp .env.rust.prod.example .env.rust.prod
# fill secrets and integration settings
docker compose -f docker-compose.rust.prod.yml up -d --build
docker compose -f docker-compose.rust.prod.yml logs -f vozen
```

The compose file mounts `./rust-data:/data` for SQLite and caches, `/models` for
voices and `/opt/piper` for the Piper binary. Keep `/data` persistent and back
up `rust-data/tts.db` before upgrades. The deploy script creates an online
SQLite backup, waits for health/gateway readiness and rolls back if the canary
fails.

## Configuration and integrations

Use `.env.rust.prod.example` as the variable inventory. Keep existing names for
`DISCORD_TOKEN`, `CLIENT_ID`, `TOPGG_TOKEN`, `TOPGG_WEBHOOK_SECRET`,
`VOTE_REDEMPTION_SECRET`, `KOFI_WEBHOOK_TOKEN`, `PREMIUM_API_*`,
`DISCORD_OAUTH_*`, and the TTS/voice paths. Rust validates the same command,
database, voice/i18n and game contracts before starting.

The public website remains a static Pages site (`site/`), while the dashboard/API
is served by the Rust API crates. Top.gg, Ko-fi, OAuth and Premium routes and
compatibility tests run in the Rust CI.

## Verification

```sh
docker compose -f docker-compose.rust.prod.yml ps
docker compose -f docker-compose.rust.prod.yml logs --tail 100 vozen
curl -fsS http://127.0.0.1:3001/health
```

Look for `healthy: Ready` and a successful health response. Also check
`https://api.vozen.org/health` and `https://vozen.org` on the public deployment.

## Development checks

```sh
npm ci --ignore-scripts
npm run check:site
node tools/check-rust-contracts.mjs
node tools/check-rust-canaries.mjs
cargo fmt --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

The `legacy-typescript` branch is retained as a recovery snapshot; it is not
part of the production build.


## Optional Cloudflare operator-tool configuration

`tools/cf-security-headers.mjs` and, where present, `tools/cf-web-analytics.mjs`
accept `CF_ZONE_NAME` and `CF_PUBLIC_API_ORIGIN`. Unset or blank values preserve
existing defaults; setting a zone derives `https://api.<zone>` unless the API
origin is explicit. Origins must be HTTPS without credentials, paths, queries,
fragments or wildcards. These operator variables do not change bot OAuth/CORS
policies or the active VPS environment. Review each tool before running it.

```dotenv
CF_ZONE_NAME=example.com
CF_PUBLIC_API_ORIGIN=https://api.example.com
```


## Public website build configuration

`npm run build:site` reads only `VOZEN_PUBLIC_CLIENT_ID` and
`VOZEN_PUBLIC_API_ORIGIN` for public identity/API overrides. Both are optional;
blank values preserve the previous defaults. IDs must be valid Discord
snowflakes; API URLs must be exact HTTPS origins. The build updates API endpoints,
status URLs, HTML invite fallbacks and CSP references consistently. Other
environment fields, including tokens, are not embedded. The site identity and API
must match your backend configuration; this does not relax backend OAuth/CORS
pins. Public website URLs/branding and provider identities are unchanged.

```dotenv
VOZEN_PUBLIC_CLIENT_ID=<YOUR_PUBLIC_DISCORD_APPLICATION_ID>
VOZEN_PUBLIC_API_ORIGIN=https://api.example.com
```
