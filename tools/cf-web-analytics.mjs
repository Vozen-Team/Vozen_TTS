#!/usr/bin/env node
// tools/cf-web-analytics.mjs
//
// Verifies and, only with --apply, provisions the manual Cloudflare Web
// Analytics property for vozen.org. It deliberately never enables automatic
// injection: the static site loads the public beacon only on marked public
// marketing, documentation and legal pages.
//
// CF_API_TOKEN must have:
//   - Zone > Zone > Read (to resolve vozen.org)
//   - Account > Account Settings > Read (to inspect Web Analytics sites)
//   - Account > Account Settings > Edit (only for --apply when a site is absent)
//   - Account > Account Analytics > Read (to prove the private RUM proxy works)
//
// The public beacon token is domain-bound and intentionally emitted as a
// GitHub Actions notice after it is found or created. It is not an API secret.

import { loadCloudflareSiteConfig } from './site-config.mjs';

const TOKEN = process.env.CF_API_TOKEN;
const API = 'https://api.cloudflare.com/client/v4';
const GRAPHQL_API = `${API}/graphql`;
const { zoneName: HOST } = loadCloudflareSiteConfig(process.env);
const APPLY = process.argv.includes('--apply');

if (!TOKEN) {
  console.error('ERROR: CF_API_TOKEN is missing.');
  process.exit(1);
}

function failureSummary(json) {
  const errors = Array.isArray(json?.errors) ? json.errors : [];
  const messages = errors
    .map((error) => String(error?.message || error?.code || '').trim())
    .filter(Boolean)
    .slice(0, 3);
  return messages.join('; ') || 'Cloudflare rejected the request';
}

function safePath(path) {
  return path
    .replace(/\/accounts\/[^/]+/g, '/accounts/{account_id}')
    .replace(/\/zones\/[^/]+/g, '/zones/{zone_id}');
}

async function cf(path, options = {}) {
  const response = await fetch(`${API}${path}`, {
    ...options,
    headers: {
      Authorization: `Bearer ${TOKEN}`,
      'Content-Type': 'application/json',
      ...(options.headers || {}),
    },
  });
  const body = await response.json().catch(() => ({}));
  if (!response.ok || body.success !== true) {
    throw new Error(`${options.method || 'GET'} ${safePath(path)} failed: ${failureSummary(body)}`);
  }
  return body.result;
}

async function graphql(query, variables) {
  const response = await fetch(GRAPHQL_API, {
    method: 'POST',
    headers: {
      Authorization: `Bearer ${TOKEN}`,
      'Content-Type': 'application/json',
    },
    body: JSON.stringify({ query, variables }),
  });
  const body = await response.json().catch(() => ({}));
  const errors = Array.isArray(body.errors) ? body.errors : [];
  if (!response.ok || errors.length) {
    throw new Error(`GraphQL RUM check failed: ${failureSummary({ errors })}`);
  }
  return body.data;
}

function isVozenSite(site) {
  if (String(site?.host || '').trim().toLowerCase() === HOST) return true;
  return Array.isArray(site?.rules) && site.rules.some(
    (rule) => String(rule?.host || '').trim().toLowerCase() === HOST,
  );
}

function publicBeaconNotice(site) {
  const token = String(site?.site_token || '').trim();
  if (!token) {
    throw new Error('Cloudflare did not return a public beacon token for the Web Analytics site.');
  }
  console.log('Web Analytics site: ready for manual public-page injection.');
  console.log(`::notice title=Cloudflare public beacon token::${token}`);
}

async function main() {
  await cf('/user/tokens/verify');
  console.log('Cloudflare API token: valid.');

  const zones = await cf(`/zones?name=${HOST}`);
  if (!Array.isArray(zones) || zones.length !== 1) {
    throw new Error(`Expected exactly one Cloudflare zone for ${HOST}.`);
  }
  const zone = zones[0];
  const zoneId = String(zone?.id || '').trim();
  const accountId = String(zone?.account?.id || '').trim();
  if (!zoneId || !accountId) {
    throw new Error(`Cloudflare did not return a usable zone/account association for ${HOST}.`);
  }
  console.log(`Cloudflare zone: resolved (${zone.status || 'unknown'}).`);
  console.log('Cloudflare account: resolved.');

  const sites = await cf(`/accounts/${accountId}/rum/site_info/list`);
  if (!Array.isArray(sites)) {
    throw new Error('Cloudflare returned an invalid Web Analytics site list.');
  }
  let site = sites.find(isVozenSite);
  if (!site) {
    if (!APPLY) {
      console.log('Web Analytics site: absent (verification mode made no changes).');
      console.log(`Rerun this workflow with apply=true to create the manual ${HOST} property.`);
      return;
    }
    site = await cf(`/accounts/${accountId}/rum/site_info`, {
      method: 'POST',
      body: JSON.stringify({
        // Manual loading is essential: dashboard, account and callback pages
        // are excluded at the static-site level.
        auto_install: false,
        host: HOST,
        zone_tag: zoneId,
      }),
    });
    console.log('Web Analytics site: created with automatic injection disabled.');
  } else {
    console.log('Web Analytics site: existing property found.');
  }

  publicBeaconNotice(site);

  const now = new Date();
  const since = new Date(now.getTime() - 60 * 60 * 1000).toISOString();
  const until = now.toISOString();
  await graphql(
    `query VozenRumPermissionCheck($accountId: String!, $siteTag: String!, $since: String!, $until: String!) {
      viewer {
        accounts(filter: {accountTag: $accountId}) {
          rumPageloadEventsAdaptiveGroups(
            filter: {datetime_gt: $since, datetime_leq: $until, siteTag: $siteTag}
            limit: 1
          ) { count }
        }
      }
    }`,
    { accountId, siteTag: String(site.site_tag || ''), since, until },
  );
  console.log('Cloudflare GraphQL RUM access: verified.');
  console.log('Next: store a dedicated Account Analytics:Read token and the resolved IDs only on the VPS.');
}

main().catch((error) => {
  // The token is never interpolated into requests or error output.
  console.error(`FAILED: ${error instanceof Error ? error.message : 'unknown error'}`);
  process.exit(1);
});
