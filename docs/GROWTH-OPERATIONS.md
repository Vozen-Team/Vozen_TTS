# Growth operations runbook

> Installation endpoint examples use api.example.com. Replace it with your configured API host; examples do not change runtime defaults or production configuration.

This runbook turns on the growth features only after each external dependency is
ready. Do not copy any secret into the public site, the static panel or a Git
repository.

## 1. Top.gg server-count publishing

1. Create or rotate the **v1 API token** in the Vozen TTS Top.gg project.
2. Put it in the untracked production environment as `TOPGG_TOKEN`; keep
   `CLIENT_ID` set to the TTS application ID.
3. Restart the Rust runtime and open the private panel's TTS Growth card.
4. Confirm an HTTP 2xx result and a fresh `last success` time. The runtime
   publishes after the gateway is ready, on every server join/leave and every
   30 minutes.
5. Treat an alert after 90 minutes without success, or a drift above 5% since
   the last sent count, as an operational incident. Check the v1 token/project
   first; never fall back to a legacy endpoint or token.

The panel records only sent count, HTTP status, failure count and sanitised
error category. It does not expose the token, response body or a server ID.

## 2. Vote reward migration

Set `TOPGG_WEBHOOK_SECRET` and a stable `VOTE_REDEMPTION_SECRET` (at least 32
characters) in the runtime environment. A valid non-duplicate vote now grants
24 hours of Plus, with four grants in any rolling 30 days and no more than 48
hours accumulated ahead. The raw entitlement ID is removed when it expires;
the keyed ledger and provider replay ID are removed after 30 days.

Before announcing the change, send one verified test vote and confirm that a
replayed delivery does not change the entitlement or vote count.

The private growth endpoint returns `votes` for the requested period and a
`votes` counter on each daily row. These are long-lived aggregate counters only:
they contain no Discord user ID, user hash, server ID or provider event ID. On
the first upgraded startup, retained provider events are backfilled into the
daily series idempotently so the visible 30-day history does not restart at zero.

## 3. TTS server-side installation

In the Discord Developer Portal, register your configured callback URL, for example:

```
https://api.example.com/api/install/tts/callback
```

Then provision only on the server:

```
RUST_TTS_INSTALL_OAUTH_ENABLED=true
TTS_INSTALL_OAUTH_REDIRECT_URI=https://api.example.com/api/install/tts/callback
TTS_INSTALL_OAUTH_CLIENT_SECRET=<Discord OAuth client secret>
TTS_INSTALL_OAUTH_STATE_SECRET=<random value of at least 32 characters>
```

The public success URL defaults to `https://vozen.org/dashboard/`; it is pinned
by the runtime. First test start, cancel, permission denial and successful
installation on a disposable guild. Confirm the browser reaches the dashboard
with `installed=1`, the state cannot be replayed, and the panel records the
chosen allowed source.

Only after that can the website deploy set
`window.VOZEN_INSTALL.ttsStartEndpoint` to:

```
https://api.example.com/api/install/tts/start
```

Leaving it empty is intentional: all existing CTAs then keep using the
compatible dashboard install route instead of a partly configured OAuth flow.

## 4. Cloudflare Web Analytics

Cloudflare's public Web Analytics beacon token/site tag is not a secret, but
the GraphQL API token is. Set the beacon value in the website's
`analytics-config.js` only after creating the Web Analytics site. Keep the
following in the runtime environment only:

```
CLOUDFLARE_WEB_ANALYTICS_ENABLED=true
CLOUDFLARE_ACCOUNT_ID=<account id>
CLOUDFLARE_ZONE_ID=<zone id>
CLOUDFLARE_WEB_ANALYTICS_SITE_TAG=<Web Analytics property ID from /web-analytics/edit/...>
CLOUDFLARE_WEB_ANALYTICS_TOKEN=<read-only GraphQL token>
```

The private endpoint has a five-minute cache and accepts at most 90 days. Test
it from the private TTS panel: no token or identifier may appear in browser
responses. The beacon is deliberately absent from account, dashboard, callback
and private-panel pages.

## 5. Growth measurement coverage

`currentGuilds` is the current inventory, independent of the selected date
range. `joins`, `leaves` and `net` describe observed changes inside that range.
They must not be presented as the bot's lifetime installation count.

`baselineGuilds` preserves the initial inventory imported when telemetry began.
Those rows are not new acquisition events and have no trustworthy historical
installation date. `measurementStartedOn` is the first telemetry day, not the
bot's launch date. A 90-day filter cannot reconstruct events that predate this
coverage. The private panel must show that coverage alongside measured growth.

## 6. Launch checks

1. Deploy the backend canary first, then the static site canary, then the
   private panel.
2. Verify `/robots.txt`, `/sitemap.xml`, `/llms.txt`, canonical pages and the
   Helper public page. Keep private routes `noindex`.
3. In Google Search Console, verify `vozen.org` and submit the generated
   sitemap after the Helper release.
4. After one hour, compare Top.gg's visible count with the private gateway
   count; investigate a difference greater than 5%.
5. Record the live p75 LCP, INP and CLS in the private panel before claiming
   the performance targets are met.
