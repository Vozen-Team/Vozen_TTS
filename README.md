<p align="center"><img src="assets/vozen-banner.png" alt="Vozen — type it, hear it." width="640"></p>

<h1 align="center">Vozen</h1>

<p align="center"><em>type it, hear it.</em></p>

<p align="center">
  <a href="https://discord.com/oauth2/authorize?client_id=1523826014935842997&permissions=326420745232&scope=bot%20applications.commands"><img src="https://img.shields.io/badge/Add%20to-Discord-5865F2?logo=discord&logoColor=white" alt="Add to Discord"></a>
  <a href="https://vozen.org"><img src="https://img.shields.io/badge/website-vozen.org-7c5cff" alt="Website"></a>
  <a href="https://discord.gg/4kYw2WUbNN"><img src="https://img.shields.io/badge/support-server-5865F2?logo=discord&logoColor=white" alt="Support server"></a>
  <a href="https://top.gg/bot/1523826014935842997/vote"><img src="https://img.shields.io/badge/vote-top.gg-ff3366" alt="Vote on top.gg"></a>
  <a href="https://github.com/Rexy40407/vozen/actions/workflows/ci.yml"><img src="https://github.com/Rexy40407/vozen/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-AGPL--3.0-blue" alt="License"></a>
</p>

**Vozen reads your messages out loud in Discord voice channels.** Type in a text
channel and everyone in the call hears it — useful when your mic is broken, when
you can't talk, or when you just don't want to.

The voice is **neural, not robotic** — and the good voice is the default, not a
paid unlock. Vozen also **reconnects on its own** when Discord drops it, instead
of going quiet for hours.

## Add it

**[→ Add Vozen to your server](https://discord.com/oauth2/authorize?client_id=1523826014935842997&permissions=326420745232&scope=bot%20applications.commands)**

Then run **`/setup`** — it picks the channel to read and turns auto-read on. Join a
voice channel, type in the text channel, and you're done.

## What it does

- **Auto-read** — pick a channel with `/setup` and Vozen reads everything written
  there. No prefixes. It also reads mentions of and replies to the bot.
- **A voice per person** — everyone picks their own voice, speed and engine with
  `/voice config`, saves favourites/recent voices, and reads all of their messages in it.
- **35 languages, with optional automatic detection** — choose any of the 38 voices,
  or opt in to per-message detection with `/voice detection on`.
- **Says who's talking** — "Sam said hello", with nicknames you can set yourself.
- **Reads Discord, not markup** — emoji, links, code blocks, spoilers and mentions
  come out as something a human would say.
- **Stays in the call** — automatic reconnection; auto-leave when the channel empties.
- **Games in voice** — `/game play` has 16 games: Wordle, Hangman, Guess the
  Language, Dictation, Reflexes, Word Chain and more.
- **Fun commands** — `/joke`, `/laugh`, `/8-ball`, `/fortune`, `/fact`, `/wyr`,
  `/randomizer`, `/sound`, `/birthday`.
- **Moderation** — blocked words, per-user rate limit, character cap, role gating
  and per-channel control, all under `/config`.
- **Private tools anywhere** — `/tts-file`, `/translate text`, and the Translate/
  Transcribe message actions work ephemerally without joining a call; reviewed actions
  can be installed as a Discord User App.
- **Channel profiles** — admins can give each channel its own voice, engine, language,
  speed, limits, bot-reading rule and voice-channel binding from the web dashboard.
- **The interface speaks your language** — the bot's own replies are translated
  into 30+ languages and follow each person's Discord language.

## Commands worth knowing

| Command                           | What it does                                         |
| --------------------------------- | ---------------------------------------------------- |
| `/setup`                          | One-step setup for the server                        |
| `/join` `/leave`                  | Bring Vozen into your voice channel, or send it away |
| `/tts`                            | Read one message out loud                            |
| `/tts-file`                       | Create a short private audio attachment (no call)    |
| `/voice config`                   | Panel to pick your voice, speed and engine           |
| `/voice browse`                   | Search the voices currently available on this bot    |
| `/voice favorite` `/voice recent` | Save and reopen personal voice shortcuts             |
| `/translate text`                 | Translate text privately with a rolling usage limit  |
| `/skip`                           | Skip what's playing                                  |
| `/config`                         | Server settings (needs Manage Server)                |
| `/help`                           | The full command list                                |
| `/privacy erase`                  | Delete your erasable personal data from Vozen        |

## Premium

Vozen's Piper-powered core is free and always will be. **Plus** unlocks personal
Kokoro, Google HD, all voice effects and higher personal limits on every server.
**Premium** unlocks those voice perks for the server, plus higher
server limits. Live voice transcription is a server Premium feature.

**[→ Plans and prices at vozen.org](https://vozen.org/#premium)** · run `/premium info`
in Discord to see what you have.

## Support

Something broken, or an idea? **[Join the support server](https://discord.gg/4kYw2WUbNN)**
— that's the fastest way to reach us. Bugs can also go in
[GitHub issues](https://github.com/Rexy40407/vozen/issues).

## Self-hosting

Vozen is open source (AGPL-3.0), so you can run your own instance. Three paths —
there is nothing easier than inviting the hosted bot:

| Level      | Who it's for                             | How                                                                                                                                                                         |
| ---------- | ---------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Easy**   | Everyone                                 | [Invite the public bot](https://discord.com/oauth2/authorize?client_id=1523826014935842997&permissions=326420745232&scope=bot%20applications.commands) — nothing to install |
| **Normal** | You want it on your own VPS              | `docker compose -f docker-compose.rust.prod.yml` — [Rust self-hosting guide](docs/SELF-HOST.md)                                                                             |
| **Hard**   | You want to change it or tune the voices | Rust workspace + Piper/models — [development checks](docs/SELF-HOST.md#development-checks)                                                                                  |

## Privacy and terms

- [**Privacy Policy**](PRIVACY.md) — what is stored, for how long, and how to delete it.
  Message text is processed and thrown away; it is never used to train anything.
- [**Terms of Service**](TERMS.md) — acceptable use, warranties, liability.
- [**User App and public status rollout**](docs/USER-APP-STATUS-ROLLOUT.md) — reviewed
  command metadata plus the manual Developer Portal and proxy activation checklist.
- [**Live service status**](https://vozen.org/status) — coarse bot, database and provider state.

## License

Copyright (C) 2026 Rexy.

Vozen is free software: you can redistribute it and/or modify it under the terms of the **GNU Affero
General Public License, version 3** (AGPL-3.0), as published by the Free Software
Foundation. See the [`LICENSE`](LICENSE) file for the full text.

AGPL-3.0 adds one key condition to the GPL: **anyone who runs a modified version of
Vozen accessible over a network must make that version's source code available to its
users**. This keeps Vozen open even when run as a service.

The source code is provided WITH NO WARRANTY (AGPL §15). This does not affect the
mandatory consumer rights that apply to the paid Premium service — see the
[Terms of Service](TERMS.md).
