#!/usr/bin/env node
// tools/cf-security-headers.mjs
//
// Configures the SECURITY HEADERS for vozen.org on Cloudflare via API (Route A of
// docs/SECURITY-HEADERS-SETUP.md). Idempotent: you can run it as many times as you want.
//
// PREREQUISITES (done by the domain owner, once):
//   1. Add vozen.org to Cloudflare + change the nameservers at the registrar (zone 'active').
//   2. Create an API token SCOPED to the vozen.org zone with the permissions:
//        Zone > DNS > Edit · Zone > Zone Settings > Edit · Zone > Transform Rules > Edit
//
// USAGE:
//   CF_API_TOKEN=xxxxx node tools/cf-security-headers.mjs             # configures + verifies
//   CF_API_TOKEN=xxxxx node tools/cf-security-headers.mjs --verify-only  # verifies only
//
// The token is NEVER printed, logged, or saved. Pass it only via the environment.

import { loadCloudflareSiteConfig } from './site-config.mjs';

const { zoneName: ZONE_NAME, publicApiOrigin: PUBLIC_API_ORIGIN } = loadCloudflareSiteConfig(process.env);
const TOKEN = process.env.CF_API_TOKEN;
if (!TOKEN) {
  console.error('ERROR: CF_API_TOKEN is missing. See this file header.');
  process.exit(1);
}

const API = 'https://api.cloudflare.com/client/v4';
const VERIFY_ONLY = process.argv.includes('--verify-only');

// Values VERIFIED against the site's real resources (see SECURITY-HEADERS-SETUP.md).
const SECURITY_HEADERS = {
  'Strict-Transport-Security': 'max-age=31536000; includeSubDomains; preload',
  'Content-Security-Policy':
    "default-src 'self'; base-uri 'self'; object-src 'none'; script-src 'self' https://static.cloudflareinsights.com https://js.stripe.com https://*.js.stripe.com; " +
    "style-src 'self' 'unsafe-inline'; " +
    "font-src 'self'; " +
    "img-src 'self' data: https://cdn.discordapp.com https://*.stripe.com https://*.link.com; " +
    `connect-src 'self' https://cloudflareinsights.com ${PUBLIC_API_ORIGIN} https://api.stripe.com https://checkout.stripe.com https://r.stripe.com https://*.stripe.com https://link.com https://*.link.com; ` +
    "frame-src https://checkout.stripe.com https://js.stripe.com https://*.js.stripe.com https://hooks.stripe.com https://link.com https://*.link.com; media-src 'self'; " +
    "form-action 'self'; frame-ancestors 'none'",
  'X-Frame-Options': 'DENY',
  'X-Content-Type-Options': 'nosniff',
  'Referrer-Policy': 'strict-origin-when-cross-origin',
  'Permissions-Policy':
    'accelerometer=(), camera=(), geolocation=(), gyroscope=(), magnetometer=(), ' +
    'microphone=(), payment=(self "https://js.stripe.com" "https://*.js.stripe.com" "https://checkout.stripe.com"), usb=()',
};

function safePath(path) {
  return path
    .replace(/\/zones\/[^/]+/g, '/zones/{zone_id}')
    .replace(/\/accounts\/[^/]+/g, '/accounts/{account_id}');
}

function failureSummary(json) {
  const errors = Array.isArray(json?.errors) ? json.errors : [];
  const messages = errors
    .map((error) => String(error?.message || error?.code || '').trim())
    .filter(Boolean)
    .slice(0, 3);
  return messages.join('; ') || 'Cloudflare rejected the request';
}

async function cf(path, opts = {}) {
  const res = await fetch(API + path, {
    ...opts,
    headers: {
      Authorization: `Bearer ${TOKEN}`,
      'Content-Type': 'application/json',
      ...(opts.headers || {}),
    },
  });
  const json = await res.json().catch(() => ({}));
  if (!json.success) {
    throw new Error(`${opts.method || 'GET'} ${safePath(path)} -> ${failureSummary(json)}`);
  }
  return json.result;
}

async function main() {
  // 1. Resolve the zone.
  const zones = await cf(`/zones?name=${ZONE_NAME}`);
  if (!zones.length) {
    throw new Error(
      `Zona ${ZONE_NAME} não existe na Cloudflare. Adiciona o domínio e muda os nameservers primeiro.`,
    );
  }
  const zoneId = zones[0].id;
  console.log(`zona: ${ZONE_NAME} status=${zones[0].status}`);
  if (zones[0].status !== 'active') {
    console.warn(
      "WARNING: the zone is not 'active' yet (nameservers are still propagating). The rule is created, " +
        'but headers are only served after it becomes active and DNS is proxied.',
    );
  }

  if (!VERIFY_ONLY) {
    // 2. Ensure the apex/www records are PROXIED (otherwise traffic does not pass through CF).
    const recs = await cf(`/zones/${zoneId}/dns_records?per_page=100`);
    for (const r of recs) {
      const proxiable = r.type === 'A' || r.type === 'AAAA' || r.type === 'CNAME';
      const target = r.name === ZONE_NAME || r.name === `www.${ZONE_NAME}`;
      if (proxiable && target && !r.proxied) {
        await cf(`/zones/${zoneId}/dns_records/${r.id}`, {
          method: 'PATCH',
          body: JSON.stringify({ proxied: true }),
        });
        console.log(`DNS proxied ON: ${r.type} ${r.name}`);
      }
    }

    // 3. HTTPS: Full mode + Always Use HTTPS.
    await cf(`/zones/${zoneId}/settings/ssl`, {
      method: 'PATCH',
      body: JSON.stringify({ value: 'full' }),
    });
    await cf(`/zones/${zoneId}/settings/always_use_https`, {
      method: 'PATCH',
      body: JSON.stringify({ value: 'on' }),
    });
    console.log('SSL=full · always_use_https=on');

    // 4. Transform Rule: inject the 6 headers into the response (phase http_response_headers_transform).
    const headers = {};
    for (const [name, value] of Object.entries(SECURITY_HEADERS)) {
      headers[name] = { operation: 'set', value };
    }
    await cf(`/zones/${zoneId}/rulesets/phases/http_response_headers_transform/entrypoint`, {
      method: 'PUT',
      body: JSON.stringify({
        rules: [
          {
            action: 'rewrite',
            action_parameters: { headers },
            expression: 'true',
            description: 'Vozen security headers',
            enabled: true,
          },
        ],
      }),
    });
    console.log(`Transform Rule aplicada (${Object.keys(SECURITY_HEADERS).length} cabeçalhos).`);
  }

  // 5. Verify what is actually served at https://vozen.org/.
  try {
    const res = await fetch(`https://${ZONE_NAME}/`, { method: 'HEAD', redirect: 'manual' });
    console.log(`\nVerificação de https://${ZONE_NAME}/ (HTTP ${res.status}):`);
    const names = Object.keys(SECURITY_HEADERS);
    const missing = names.filter((h) => !res.headers.get(h));
    for (const h of names) console.log(`  ${res.headers.get(h) ? 'OK   ' : 'MISSING'} ${h}`);
    console.log(
      missing.length
        ? `\n${missing.length} missing; DNS may still be propagating, the record may not be proxied, or the rule may not be active.`
        : '\nAll six headers are present ✅. Confirm the A/A+ grade at securityheaders.com.',
    );
  } catch (e) {
    console.log(
      `\n(verificação HTTP falhou: ${e.message} — provavelmente o DNS ainda a propagar.)`,
    );
  }
}

main().catch((e) => {
  console.error(`FAILED: ${e.message}`);
  process.exit(1);
});
