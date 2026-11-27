import { existsSync, readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, expect, it } from 'vitest';
const LANGS = ['en', 'pt', 'fr', 'es', 'de', 'tr', 'ar', 'zh', 'ru', 'ko'];
const source = (path) => readFileSync(resolve(process.cwd(), path), { encoding: 'utf8' });
const locale = (lang) => JSON.parse(source(`tools/i18n-src/${lang}.json`));
describe('site localization contract', () => {
  it('keeps the ten advertised languages in exact key parity', () => {
    const canonical = Object.keys(locale('en').ui).sort();
    for (const lang of LANGS) {
      expect(Object.keys(locale(lang).ui).sort(), lang).toEqual(canonical);
    }
  });
  it('has complete account and dashboard namespaces in every language', () => {
    const required = [
      'account.documentTitle',
      'account.back',
      'account.pageTitle',
      'account.pageSubtitle',
      'account.signedIn',
      'account.yourAccount',
      'account.synced',
      'account.startHere',
      'account.nextTitle',
      'account.nextSubtitle',
      'account.manageServer',
      'account.manageServerDescription',
      'account.activatePurchase',
      'account.activatePurchaseDescription',
      'account.comparePlans',
      'account.signInTrustLead',
      'account.signInTrustBody',
      'account.discordId',
      'account.copyDiscordId',
      'account.copyHint',
      'account.copied',
      'account.benefits',
      'account.activeTitle',
      'account.benefitsDescription',
      'account.active',
      'account.notActive',
      'account.membershipStatus',
      'account.logoutConfirmTitle',
      'account.logoutConfirmBody',
      'account.logoutCancel',
      'account.logoutConfirmAction',
      'dashboard.documentTitle',
      'dashboard.loginTitle',
      'dashboard.loading',
      'dashboard.pick',
      'dashboard.none',
      'dashboard.error',
      'dashboard.save',
      'dashboard.saved',
      'dashboard.sec_reading',
      'dashboard.f_autoread',
      'dashboard.d_autoread',
      'dashboard.f_locale',
      'dashboard.d_locale',
      'dashboard.f_ttsChannelId',
      'dashboard.d_ttsChannelId',
      'dashboard.f_defaultVoice',
      'dashboard.d_defaultVoice',
      'dashboard.channelNone',
      'dashboard.voiceGlobal',
      'dashboard.unavailableChannel',
      'dashboard.unavailableVoice',
    ];
    for (const lang of LANGS) {
      const ui = locale(lang).ui;
      for (const key of required) expect(ui[key], `${lang}: ${key}`).toBeTruthy();
    }
  });
  it('ships one generated dictionary and the immediately cache-busted runtimes', () => {
    const pages = ['site/index.html', 'site/account.html', 'site/dashboard.html'];
    for (const pagePath of pages) {
      const page = source(pagePath);
      expect(page, pagePath).toContain('js/i18n-v41.js');
      expect(page, pagePath).toContain('js/main-v51.js');
      expect(page, pagePath).not.toContain('js/i18n-v39.js');
      expect(page, pagePath).not.toContain('js/i18n-v38.js');
      expect(page, pagePath).not.toContain('js/main-v42.js');
    }
    expect(source('site/dashboard.html')).toContain('js/dashboard-v8.js');
    expect(source('site/dashboard.html')).not.toContain('js/dashboard-v6.js');
    expect(source('site/dashboard.html')).not.toContain('js/dashboard-v5.js');
    expect(existsSync(resolve(process.cwd(), 'site/js/i18n-v39.js'))).toBe(false);
    expect(existsSync(resolve(process.cwd(), 'site/js/i18n-v38.js'))).toBe(false);
    expect(existsSync(resolve(process.cwd(), 'site/js/main-v42.js'))).toBe(false);
    expect(existsSync(resolve(process.cwd(), 'site/js/dashboard-v5.js'))).toBe(false);
  });
  it('translates text, accessible attributes and the document title before announcing changes', () => {
    const script = source('site/js/main-v51.js');
    expect(script).toContain('[data-i18n]');
    expect(script).toContain('"data-i18n-aria-label"');
    expect(script).toContain('"data-i18n-placeholder"');
    expect(script).toContain('"data-i18n-title"');
    expect(script).toContain('document.title =');
    expect(script).toContain('new CustomEvent("vozen:languagechange"');
    expect(script.indexOf('document.title =')).toBeLessThan(
      script.indexOf('new CustomEvent("vozen:languagechange"'),
    );
  });
  it('uses semantic keys for static account and dashboard content', () => {
    const account = source('site/account.html');
    const dashboard = source('site/dashboard.html');
    expect(account).toContain('data-i18n="account.pageTitle"');
    expect(account).toContain('data-i18n="account.manageServer"');
    expect(account).toContain('data-i18n-aria-label="account.tasksLabel"');
    expect(account).toContain('data-i18n="account.synced"');
    expect(account).toContain('data-i18n="account.signInTrustLead"');
    expect(account).toContain('data-i18n="account.signInTrustBody"');
    expect(dashboard).toContain('data-i18n="dashboard.eyebrow"');
    expect(dashboard).toContain('data-i18n="dashboard.title"');
    expect(dashboard).toContain('data-i18n="dashboard.subtitle"');
    expect(dashboard).toContain('data-i18n-aria-label="common.skipToContent"');
  });
  it('removes the dashboard dictionary duplicate and preserves dirty fields on language change', () => {
    const script = source('site/js/dashboard-v8.js');
    expect(script).not.toContain('var STR =');
    expect(script).not.toContain('new MutationObserver');
    expect(script).toContain('window.VOZEN_I18N');
    expect(script).toContain('window.addEventListener("vozen:languagechange"');
    const start = script.indexOf('onLang = function relocalizeForm()');
    const end = script.indexOf('\n    };', start);
    const relocalizer = script.slice(start, end);
    expect(start).toBeGreaterThan(-1);
    expect(relocalizer).not.toContain('renderForm(');
    expect(relocalizer).not.toContain('loadForm(');
    expect(relocalizer).not.toContain('view(');
    expect(relocalizer).toContain('refresh()');
  });
  it('supports a non-writing generated-bundle check in the site gate', () => {
    const generator = source('tools/build-i18n.mjs');
    const pkg = JSON.parse(source('package.json'));
    expect(generator).toContain("process.argv.includes('--check')");
    expect(generator).toContain("value.replace(/\\r\\n?/g, '\\n')");
    expect(generator).toContain('site/js/i18n-v41.js is out of date');
    expect(pkg.scripts['build:i18n']).toBe('node tools/build-i18n.mjs');
    expect(pkg.scripts['check:i18n']).toBe('node tools/build-i18n.mjs --check');
    expect(pkg.scripts['check:site']).toContain('site-tests/siteI18n.test.mjs');
    expect(pkg.scripts['check:site']).toContain('npm run check:i18n');
  });
});
