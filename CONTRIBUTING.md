# Contributing to Vozen

Vozen's production runtime is the Rust workspace under `crates/`. The static
website is kept in `site/` and uses the small Node toolchain in `tools/`.

## Commands

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

`npm run check` runs the complete local gate. Production uses
`docker compose -f docker-compose.rust.prod.yml up -d --build`; do not run a
second bot process beside that container.

## Hard rules

- Preserve the public Discord command, response, SQLite, voice/i18n and game
  contracts. Do not create a parallel Node implementation.
- Keep Top.gg, Stripe, Premium/OAuth and dashboard API routes fail-closed and
  covered by Rust tests.
- Any stored user data needs a documented deletion path in `PRIVACY.md`.
- Never commit real `.env` files or secrets.
- Do not use destructive database commands during development or deployment.
- User-facing locale catalogs remain multilingual; source and comments are in
  English.
- Website changes must preserve its no-tracking/CSP/privacy guarantees.

## Architecture

- `crates/vozen-runtime`: process, gateway, command promotion and integrations.
- `crates/vozen-discord`: Discord command parsing, responses, voice and games.
- `crates/vozen-core`: pure policies, games, text and translation logic.
- `crates/vozen-store`: SQLite schema, migrations, privacy and entitlements.
- `crates/vozen-tts`: Piper, Google, Kokoro and neural provider adapters.
- `crates/vozen-api`: dashboard, OAuth, Premium/Stripe, admin and Top.gg HTTP routes.
- `contracts/`: committed compatibility contracts checked before build/deploy.
- `site-tests/`: static-site acceptance tests.

The `legacy-typescript` branch is a recovery snapshot only. It is not built,
tested or deployed by `main`.
