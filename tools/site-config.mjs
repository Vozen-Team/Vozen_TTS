import { isIP } from 'node:net';

const hostnamePattern =
  /^(?:[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.)+[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/;
const isHostname = (value) => value.length <= 253 && hostnamePattern.test(value) && !isIP(value);

export function loadCloudflareSiteConfig(env) {
  const zoneName = (env.CF_ZONE_NAME?.trim() || '').toLowerCase();
  if (!isHostname(zoneName)) {
    throw new Error('CF_ZONE_NAME must be a DNS hostname without scheme, path or port.');
  }
  return {
    zoneName,
    publicApiOrigin: exactHttpsOrigin(
      env.CF_PUBLIC_API_ORIGIN?.trim() || '',
      'CF_PUBLIC_API_ORIGIN',
    ),
  };
}

function exactHttpsOrigin(value, name) {
  let api;
  try {
    api = new URL(value);
  } catch {
    throw new Error(`${name} must be an exact HTTPS origin.`);
  }
  if (
    api.protocol !== 'https:' ||
    api.username ||
    api.password ||
    api.href !== `${api.origin}/` ||
    !isHostname(api.hostname)
  ) {
    throw new Error(`${name} must be an exact HTTPS origin.`);
  }
  return api.origin;
}

export function loadPublicSiteConfig(env) {
  const clientId = env.VOZEN_PUBLIC_CLIENT_ID?.trim() || '';
  if (!/^[1-9]\d{16,19}$/.test(clientId) || BigInt(clientId) > 18446744073709551615n) {
    throw new Error('VOZEN_PUBLIC_CLIENT_ID must be a valid Discord snowflake.');
  }
  return {
    clientId,
    apiOrigin: exactHttpsOrigin(
      env.VOZEN_PUBLIC_API_ORIGIN?.trim() || '',
      'VOZEN_PUBLIC_API_ORIGIN',
    ),
  };
}

export function configurePublicSiteSource(source, config) {
  return source
    .replaceAll('__VOZEN_PUBLIC_API_ORIGIN__', config.apiOrigin)
    .replaceAll('__VOZEN_PUBLIC_CLIENT_ID__', config.clientId);
}
