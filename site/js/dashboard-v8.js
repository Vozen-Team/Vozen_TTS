/* Vozen — dashboard web de configuração da guild.
   OAuth: reutiliza o redirect /account (o único registado no portal) com scope
   `identify guilds`; o main.js guarda o token no sessionStorage e salta de volta a
   /dashboard (via `vozen.returnTo`). Aqui lemos o token e falamos com a API do bot
   (/api/dashboard/*). A autorização real (MANAGE_GUILD + bot presente) é no servidor.
   HUD v5: formulário agrupado por secções (Reading/Voice/Community/Limits), toggle
   switches em vez de checkboxes nativas, campo de língua (locale — a API já o aceita),
   e save com estado (só ativo quando há alterações). CSP: zero handlers inline, tudo
   por addEventListener; CSS injetado num <style> (style-src tem 'unsafe-inline'). */
(function () {
  "use strict";
  var CLIENT_ID = "__VOZEN_PUBLIC_CLIENT_ID__";
  var API = "__VOZEN_PUBLIC_API_ORIGIN__";
  var REDIRECT = new URL("/account", location.href).href;
  var TOK_KEY = "vozen.dtoken";
  // The account flow requests `identify email`; the dashboard flow requests `identify guilds`.
  // Keep an ownership marker so an account-only token triggers dashboard consent instead of
  // being sent to /api/dashboard and misleadingly displayed as an expired login.
  var DASHBOARD_AUTH_KEY = "vozen.dashboardAuth";
  var STATE_KEY = "vozen.oauthstate";
  var RETURN_KEY = "vozen.returnTo";
  var LS_LANG = "vozen.lang";

  var root = document.getElementById("dashRoot");
  if (!root) return;

  /* Re-localização ao vivo: main-v39 anuncia `vozen:languagechange` depois de atualizar
     texto, atributos e título. Cada vista regista aqui o seu relocalizador; no formulário
     ele atua in-place para não tocar nos inputs nem no estado por guardar. */
  var onLang = null;

  /* O bundle versionado é a única fonte de strings da homepage, conta e painel. */
  var DICT = window.VOZEN_I18N || {};
  function lang() {
    try {
      var l = localStorage.getItem(LS_LANG) || "en";
      return DICT[l] ? l : "en";
    } catch (e) {
      return "en";
    }
  }
  function t(k) {
    var l = lang();
    return (DICT[l] && DICT[l][k]) || (DICT.en && DICT.en[k]) || k;
  }

  /* Estrutura do formulário: campos agrupados por tema. A whitelist de escrita é no
     backend (DASHBOARD_FIELDS em src/premium/dashboardApi.ts) — isto é só a vista. */
  var SECTIONS = [
    { id: "reading", fields: ["autoread", "readBots", "textInVoice", "antispam", "translationEnabled"] },
    { id: "voice", fields: ["xsaid", "autojoin", "greetOnJoin", "stayInCall"] },
    { id: "community", fields: ["streakAnnounce", "soundboard", "votePromos"] },
    { id: "limits", fields: ["maxChars", "ratePerMin", "locale", "priorityRoleId", "blockedRoleId"] },
  ];
  var FIELD = {
    autoread: { type: "toggle" },
    readBots: { type: "toggle" },
    textInVoice: { type: "toggle" },
    antispam: { type: "toggle" },
    xsaid: { type: "toggle" },
    autojoin: { type: "toggle" },
    greetOnJoin: { type: "toggle" },
    streakAnnounce: { type: "toggle" },
    soundboard: { type: "toggle" },
    maxChars: { type: "num", min: 1, max: 2000 },
    ratePerMin: { type: "num", min: 1, max: 120 },
    locale: { type: "select" },
    ttsChannelId: { type: "channel" },
    defaultVoice: { type: "voice" },
    priorityRoleId: { type: "role" },
    blockedRoleId: { type: "role" },
    translationEnabled: { type: "toggle" },
    votePromos: { type: "toggle" },
    stayInCall: { type: "toggle" },
  };
  var NEW_FIELD_COPY = {
    priorityRoleId: ["Priority queue role", "Members with this role use the accessibility queue lane."],
    blockedRoleId: ["Blocked TTS role", "This restriction always wins over priority and subscriptions."],
    translationEnabled: ["Automatic translation", "Allow only the channel mappings explicitly configured for this server."],
    votePromos: ["Community reminders", "Show the opt-in Top.gg and support rotation."],
    stayInCall: ["Stay in call", "Keep the bot connected when an active Premium server is empty."],
  };
  function fieldCopy(key, description) {
    var copy = NEW_FIELD_COPY[key];
    return copy ? copy[description ? 1 : 0] : t("dashboard." + (description ? "d_" : "f_") + key);
  }

  function sectionsFor(meta) {
    var sections = SECTIONS.map(function (section) {
      return { id: section.id, fields: section.fields.slice() };
    });
    if (
      meta &&
      meta.capabilities &&
      meta.options &&
      meta.capabilities.ttsChannelId &&
      meta.capabilities.defaultVoice &&
      Array.isArray(meta.options.channels) &&
      Array.isArray(meta.options.voices)
    ) {
      var voice = sections.filter(function (section) {
        return section.id === "voice";
      })[0];
      var fields = voice.fields;
      fields.unshift("ttsChannelId", "defaultVoice");
    }
    return sections;
  }

  function eachField(sections, fn) {
    for (var s = 0; s < sections.length; s++) {
      var f = sections[s].fields;
      for (var i = 0; i < f.length; i++) fn(f[i], FIELD[f[i]]);
    }
  }

  function esc(s) {
    return String(s).replace(/[&<>"']/g, function (c) {
      return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c];
    });
  }
  function token() {
    try {
      return sessionStorage.getItem(TOK_KEY);
    } catch (e) {
      return null;
    }
  }
  function clearToken() {
    try {
      sessionStorage.removeItem(TOK_KEY);
      sessionStorage.removeItem(DASHBOARD_AUTH_KEY);
    } catch (e) {}
  }
  function hasDashboardAuth() {
    try {
      return sessionStorage.getItem(DASHBOARD_AUTH_KEY) === "1";
    } catch (e) {
      return false;
    }
  }
  function authHeaders() {
    return { Authorization: "Bearer " + token() };
  }

  /* ── OAuth: pede identify+guilds via o redirect /account; volta a /dashboard ── */
  function randState() {
    var a = new Uint8Array(16);
    var c = window.crypto || window.msCrypto;
    if (!c || typeof c.getRandomValues !== "function") throw new Error("no-csprng");
    c.getRandomValues(a);
    return [].map
      .call(a, function (b) {
        return b.toString(16).padStart(2, "0");
      })
      .join("");
  }
  function login() {
    var state;
    try {
      state = randState();
    } catch (e) {
      alert(t("dashboard.secureTokenError"));
      return;
    }
    try {
      sessionStorage.setItem(STATE_KEY, state);
      sessionStorage.setItem(RETURN_KEY, "/dashboard");
      sessionStorage.setItem(DASHBOARD_AUTH_KEY, "1");
    } catch (e) {}
    var u = new URL("https://discord.com/oauth2/authorize");
    u.searchParams.set("client_id", CLIENT_ID);
    u.searchParams.set("redirect_uri", REDIRECT);
    u.searchParams.set("response_type", "token");
    u.searchParams.set("scope", "identify guilds");
    u.searchParams.set("state", state);
    location.href = u.toString();
  }

  /* ── estilos inline (CSP permite; usam as vars do tema do site) ── */
  var CARD =
    "background:var(--panel-2,#12121c);border:1px solid var(--line-2,#23233a);border-radius:16px;padding:22px;margin-top:18px";
  var BTN = "btn btn--primary";
  var MUTED = "color:var(--text-2,#9a9ab0)";

  /* Seletor cai um SVG chevron via data: (img-src permite data:). %23 = # (cor). */
  var SEL_ARROW =
    "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='12' height='12' viewBox='0 0 24 24' fill='none' stroke='%239a9ab0' stroke-width='2.5' stroke-linecap='round' stroke-linejoin='round'%3E%3Cpath d='M6 9l6 6 6-6'/%3E%3C/svg%3E";

  /* Classes precisam de :hover/:focus/::after — impossível em style="" inline;
     injetamos um <style> uma vez. */
  var CSS = [
    /* picker de servidores: lista horizontal, com a densidade do dashboard de billing */
    ".dash-picker{margin-top:18px;padding:22px;background:rgba(23,22,19,.74);border:1px solid rgba(225,205,157,.13);border-radius:16px}",
    ".dash-picker__list{display:grid;gap:10px;margin-top:28px}",
    ".dash-server{display:grid;grid-template-columns:40px minmax(0,1fr) auto;align-items:center;gap:14px;width:100%;min-height:66px;padding:10px 14px;background:#171613;border:1px solid rgba(225,205,157,.13);border-radius:14px;cursor:pointer;font:inherit;color:var(--text-1,#e9e9f2);text-align:left;transition:border-color .16s ease,background .16s ease,transform .16s ease,box-shadow .16s ease}",
    ".dash-server:hover,.dash-server:focus-visible{border-color:rgba(225,205,157,.42);background:#1c1a17;box-shadow:0 10px 24px rgba(0,0,0,.16);transform:translateY(-1px)}",
    ".dash-server:active{transform:translateY(0)}",
    ".dash-server__img,.dash-server__ph{width:40px;height:40px;border-radius:12px;flex:none}",
    ".dash-server__img{object-fit:cover;background:#292721}",
    ".dash-server__ph{display:flex;align-items:center;justify-content:center;background:#292721;border:1px solid rgba(225,205,157,.16);font-weight:800;font-size:.92rem;color:#e8d39a}",
    ".dash-server__name{min-width:0;font-size:.94rem;font-weight:700;line-height:1.3;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}",
    ".dash-server__arrow{display:grid;width:34px;height:34px;place-items:center;border:1px solid rgba(255,255,255,.1);border-radius:10px;color:#d8c99f;font-size:1.2rem;line-height:1}",
    /* formulário */
    ".dash-form{background:var(--panel-2,#12121c);border:1px solid var(--line-2,#23233a);border-radius:16px;padding:22px;margin-top:18px}",
    ".dash-head{display:flex;align-items:center;gap:14px;flex-wrap:wrap;margin-bottom:4px}",
    ".dash-head__ic,.dash-head__ph{width:44px;height:44px;border-radius:50%;flex:none}",
    ".dash-head__ic{object-fit:cover;background:var(--bg-0,#0a0a12)}",
    ".dash-head__ph{display:flex;align-items:center;justify-content:center;background:var(--bg-0,#0a0a12);border:1px solid var(--line-2,#23233a);font-weight:700;color:var(--aqua,#38e0c8)}",
    ".dash-head__name{margin:0;font-size:1.2rem;font-family:var(--f-display,inherit);flex:1;min-width:120px;overflow-wrap:anywhere}",
    ".dash-back{margin-left:auto}",
    ".dash-sec{margin-top:22px}",
    ".dash-sec__t{font-family:var(--f-mono,inherit);font-size:.72rem;letter-spacing:.14em;text-transform:uppercase;color:var(--aqua,#38e0c8);margin:0 0 2px}",
    ".dash-row{display:flex;align-items:center;gap:16px;padding:13px 10px;margin:0 -10px;border-radius:10px;border-bottom:1px solid var(--line-2,#23233a);cursor:pointer}",
    ".dash-sec .dash-row:last-child{border-bottom:0}",
    ".dash-row:hover{background:var(--glass,rgba(255,255,255,.03))}",
    ".dash-row__txt{flex:1;min-width:0}",
    ".dash-row__l{display:block;font-size:.98rem;color:var(--text-1,#e9e9f2)}",
    ".dash-row__d{display:block;font-size:.82rem;color:var(--text-2,#9a9ab0);margin-top:2px}",
    /* toggle switch */
    ".dash-sw{position:relative;flex:none;width:44px;height:24px}",
    ".dash-sw input{position:absolute;inset:0;width:100%;height:100%;margin:0;opacity:0;cursor:pointer}",
    ".dash-sw__tr{position:absolute;inset:0;border-radius:999px;background:var(--line-2,#23233a);transition:background .15s ease;pointer-events:none}",
    ".dash-sw__tr::after{content:'';position:absolute;top:3px;left:3px;width:18px;height:18px;border-radius:50%;background:#fff;transition:transform .15s ease}",
    ".dash-sw input:checked+.dash-sw__tr{background:var(--aqua,#38e0c8)}",
    ".dash-sw input:checked+.dash-sw__tr::after{transform:translateX(20px)}",
    ".dash-sw input:focus-visible+.dash-sw__tr{outline:2px solid var(--aqua,#38e0c8);outline-offset:2px}",
    /* número + select */
    ".dash-num,.dash-sel{background:var(--bg-0,#0a0a12);color:var(--text-1,#e9e9f2);border:1px solid var(--line-2,#23233a);border-radius:10px;font:inherit;transition:border-color .15s ease}",
    ".dash-num{width:92px;padding:9px 10px;font-family:var(--f-mono,inherit);text-align:right}",
    ".dash-sel{max-width:180px;padding:9px 30px 9px 11px;cursor:pointer;-webkit-appearance:none;-moz-appearance:none;appearance:none;background-image:url(\"" + SEL_ARROW + "\");background-repeat:no-repeat;background-position:right 10px center}",
    ".dash-num:focus,.dash-sel:focus{outline:none;border-color:var(--aqua,#38e0c8)}",
    /* barra de guardar */
    ".dash-savebar{display:flex;align-items:center;gap:14px;margin-top:22px;padding-top:18px;border-top:1px solid var(--line-2,#23233a)}",
    ".dash-save[disabled]{opacity:.45;cursor:not-allowed}",
    ".dash-status{font-size:.9rem;color:var(--text-2,#9a9ab0)}",
    ".dash-status--ok{color:var(--aqua,#38e0c8)}",
    ".dash-status--err{color:var(--amber,#e6b34d)}",
    ".dash-profiles{margin-top:22px;padding-top:20px;border-top:1px solid var(--line-2,#23233a)}",
    ".dash-profiles__grid{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:12px;margin-top:12px}",
    ".dash-profile-field{display:flex;flex-direction:column;gap:6px;color:var(--text-2,#9a9ab0);font-size:.78rem}",
    ".dash-profile-field .dash-sel,.dash-profile-field .dash-num{width:100%;max-width:none;text-align:left;box-sizing:border-box}",
    ".dash-profile-actions{display:flex;gap:10px;align-items:center;margin-top:14px;flex-wrap:wrap}",
    /* mobile: barra de guardar colada ao fundo (forms longos) */
    "@media(max-width:720px){.dash-savebar{position:sticky;bottom:0;margin:22px -22px -22px;padding:14px 22px;background:var(--panel-2,#171613);border-top:1px solid var(--line-2,#23233a)}.dash-sel{max-width:150px}.dash-profiles__grid{grid-template-columns:1fr}.dash-server{grid-template-columns:38px minmax(0,1fr) auto;gap:11px;padding-left:11px;padding-right:11px}.dash-server__img,.dash-server__ph{width:38px;height:38px}}",
    "@media(prefers-reduced-motion:reduce){.dash-server,.dash-sw__tr,.dash-sw__tr::after,.dash-num,.dash-sel{transition:none}}",
  ].join("\n");
  var styleEl = document.createElement("style");
  styleEl.textContent = CSS;
  document.head.appendChild(styleEl);

  function view(html) {
    root.innerHTML = html;
  }

  function renderLogin(msgKey) {
    var msg = msgKey ? t(msgKey) : "";
    view(
      '<div style="' +
        CARD +
        '">' +
        (msg ? '<p style="color:var(--amber,#e6b34d);margin:0 0 12px">' + esc(msg) + "</p>" : "") +
        '<h2 style="margin:0 0 6px;font-size:1.25rem">' +
        esc(t("dashboard.loginTitle")) +
        '</h2><p style="' +
        MUTED +
        ';margin:0 0 18px">' +
        esc(t("dashboard.loginSub")) +
        '</p><button type="button" class="' +
        BTN +
        '" id="dashLogin">' +
        esc(t("dashboard.loginBtn")) +
        "</button></div>",
    );
    var b = document.getElementById("dashLogin");
    if (b) b.addEventListener("click", login);
    onLang = function () {
      renderLogin(msgKey);
    };
  }

  function renderMessage(titleKey, hintKey, opts) {
    opts = opts || {};
    var title = t(titleKey);
    var hint = hintKey ? t(hintKey) : "";
    view(
      '<div style="' +
        CARD +
        '"><h2 style="margin:0 0 6px;font-size:1.25rem">' +
        esc(title) +
        '</h2><p style="' +
        MUTED +
        ';margin:0">' +
        esc(hint) +
        "</p>" +
        (opts.retry
          ? '<button type="button" class="' +
            BTN +
            '" id="dashRetry" style="margin-top:16px">' +
            esc(t("dashboard.retry")) +
            "</button>"
          : "") +
        "</div>",
    );
    if (opts.retry) {
      var r = document.getElementById("dashRetry");
      if (r) r.addEventListener("click", login);
    }
    // Keep semantic keys so a language change can also translate loading/error/empty states.
    onLang = function () {
      renderMessage(titleKey, hintKey, opts);
    };
  }

  /* CDN de ícones da Discord (img-src já permite cdn.discordapp.com no CSP).
     Ícones animados têm hash "a_..." e servem-se como .gif. */
  function guildIconUrl(g) {
    if (!g.icon) return null;
    var ext = String(g.icon).indexOf("a_") === 0 ? "gif" : "png";
    return "https://cdn.discordapp.com/icons/" + g.id + "/" + g.icon + "." + ext + "?size=128";
  }
  function guildInitials(name) {
    var parts = String(name).trim().split(/\s+/).slice(0, 2);
    var out = "";
    for (var i = 0; i < parts.length; i++) out += parts[i].charAt(0);
    return out.toUpperCase() || "?";
  }
  // Liga o fallback de um <img> de ícone: se falhar, troca por placeholder de iniciais.
  function wireIconFallback(img, name, phClass) {
    if (!img) return;
    img.addEventListener("error", function () {
      var ph = document.createElement("span");
      ph.className = phClass;
      ph.setAttribute("aria-hidden", "true");
      ph.textContent = guildInitials(name || "?");
      if (img.parentNode) img.parentNode.replaceChild(ph, img);
    });
  }

  function renderPicker(guilds) {
    var cards = guilds
      .map(function (g, i) {
        var url = guildIconUrl(g);
        var art = url
          ? '<img class="dash-server__img" src="' + esc(url) + '" alt="">'
          : '<span class="dash-server__ph" aria-hidden="true">' +
            esc(guildInitials(g.name)) +
            "</span>";
        return (
          '<button type="button" class="dash-server" data-i="' +
          i +
          '">' +
          art +
          '<span class="dash-server__name">' +
          esc(g.name) +
          '</span><span class="dash-server__arrow" aria-hidden="true">›</span></button>'
        );
      })
      .join("");
    view(
      '<div class="dash-picker"><h2 style="margin:0 0 6px;font-size:1.25rem">' +
        esc(t("dashboard.pick")) +
        '</h2><p style="' +
        MUTED +
        ';margin:0">' +
        esc(t("dashboard.pickHint")) +
        '</p><div class="dash-picker__list">' +
        cards +
        "</div></div>",
    );
    var btns = root.querySelectorAll(".dash-server");
    function onPick(ev) {
      var g = guilds[Number(ev.currentTarget.getAttribute("data-i"))];
      if (g) loadForm(g, guilds);
    }
    for (var i = 0; i < btns.length; i++) {
      btns[i].addEventListener("click", onPick);
      var g = guilds[i];
      wireIconFallback(btns[i].querySelector(".dash-server__img"), g && g.name, "dash-server__ph");
    }
    onLang = function () {
      renderPicker(guilds);
    };
  }

  /* ── construção das linhas do formulário ── */
  function swHtml(key, on) {
    return (
      '<span class="dash-sw"><input type="checkbox" data-k="' +
      key +
      '"' +
      (on ? " checked" : "") +
      '><span class="dash-sw__tr"></span></span>'
    );
  }
  function numHtml(key, f, val) {
    return (
      '<input class="dash-num" type="number" data-k="' +
      key +
      '" min="' +
      f.min +
      '" max="' +
      f.max +
      '" value="' +
      esc(val) +
      '">'
    );
  }
  function optionsFor(key, val, meta) {
    if (meta && meta.options) {
      if (key === "ttsChannelId" && Array.isArray(meta.options.channels)) {
        return meta.options.channels;
      }
      if (key === "defaultVoice" && Array.isArray(meta.options.voices)) {
        return meta.options.voices;
      }
      if (key === "locale" && Array.isArray(meta.options.locales)) {
        return meta.options.locales;
      }
      if ((key === "priorityRoleId" || key === "blockedRoleId") && Array.isArray(meta.options.roles)) {
        return meta.options.roles;
      }
    }
    // Compatibility with the old API: keep the current locale visible without inventing a
    // client-side catalogue. Channel and voice remain hidden by sectionsFor().
    return key === "locale" ? [{ id: String(val || "en"), label: String(val || "en") }] : [];
  }
  function selectOptionsHtml(key, val, meta) {
    var opts = "";
    if (key === "ttsChannelId") {
      opts += '<option value=""' + (val === null ? " selected" : "") + ">" + esc(t("dashboard.channelNone")) + "</option>";
    } else if (key === "defaultVoice") {
      opts += '<option value=""' + (val === "" ? " selected" : "") + ">" + esc(t("dashboard.voiceGlobal")) + "</option>";
    } else if (key === "priorityRoleId" || key === "blockedRoleId") {
      opts += '<option value=""' + (val === null ? " selected" : "") + ">No role</option>";
    }
    var options = optionsFor(key, val, meta);
    for (var i = 0; i < options.length; i++) {
      var option = options[i];
      var label = option.label;
      if (option.unavailable) {
        var unavailableKey =
          key === "ttsChannelId" ? "dashboard.unavailableChannel" : "dashboard.unavailableVoice";
        label = t(unavailableKey).replace("{name}", option.label);
      }
      opts +=
        '<option value="' +
        esc(option.id) +
        '"' +
        (option.id === val ? " selected" : "") +
        (option.unavailable ? " disabled" : "") +
        ">" +
        esc(label) +
        "</option>";
    }
    return opts;
  }
  function selHtml(key, val, meta) {
    return (
      '<select class="dash-sel" data-k="' +
      key +
      '">' +
      selectOptionsHtml(key, val, meta) +
      "</select>"
    );
  }
  function rowHtml(key, cfg, meta) {
    var f = FIELD[key];
    var control;
    var desc = fieldCopy(key, true);
    if (f.type === "toggle") control = swHtml(key, !!cfg[key]);
    else if (f.type === "num") {
      control = numHtml(key, f, cfg[key]);
      desc += " (" + f.min + "–" + f.max + ")";
    } else control = selHtml(key, fieldValue(key, cfg), meta);
    return (
      '<label class="dash-row"><span class="dash-row__txt"><span class="dash-row__l">' +
      esc(fieldCopy(key, false)) +
      '</span><span class="dash-row__d">' +
      esc(desc) +
      "</span></span>" +
      control +
      "</label>"
    );
  }
  function headHtml(guild) {
    var url = guildIconUrl(guild);
    var art = url
      ? '<img class="dash-head__ic" src="' + esc(url) + '" alt="">'
      : '<span class="dash-head__ph" aria-hidden="true">' + esc(guildInitials(guild.name)) + "</span>";
    return (
      '<div class="dash-head">' +
      art +
      '<h2 class="dash-head__name">' +
      esc(guild.name) +
      '</h2><button type="button" id="dashBack" class="btn btn--ghost btn--sm dash-back">' +
      esc(t("dashboard.back")) +
      "</button></div>"
    );
  }

  function loadForm(guild, guilds) {
    renderMessage("dashboard.loading", "");
    fetch(API + "/api/dashboard/guild/" + guild.id, { headers: authHeaders() })
      .then(function (res) {
        if (res.status === 401) {
          clearToken();
          renderLogin("dashboard.expired");
          return null;
        }
        if (res.status === 403) {
          renderMessage("dashboard.forbidden", "dashboard.noneHint");
          return null;
        }
        if (!res.ok) {
          renderMessage("dashboard.error", "");
          return null;
        }
        return res.json();
      })
      .then(function (data) {
        if (data && data.config) renderForm(guild, data.config, guilds, data, false);
      })
      .catch(function () {
        renderMessage("dashboard.error", "");
      });
  }

  function profileOptionHtml(options, value, emptyLabel) {
    var out = '<option value="">' + esc(emptyLabel) + "</option>";
    for (var i = 0; i < options.length; i++) {
      out +=
        '<option value="' +
        esc(options[i].id) +
        '"' +
        (options[i].id === value ? " selected" : "") +
        ">" +
        esc(options[i].label) +
        "</option>";
    }
    return out;
  }

  function triStateHtml(key) {
    return (
      '<select class="dash-sel" data-p="' +
      key +
      '"><option value="">Inherit server setting</option><option value="true">On</option><option value="false">Off</option></select>'
    );
  }

  function profileEditorHtml(meta) {
    if (!meta.capabilities.channelProfiles) return "";
    var channels = meta.options.channels || [];
    var voices = meta.options.voices || [];
    var voiceChannels = meta.options.voiceChannels || [];
    var locales = meta.options.locales || [];
    return (
      '<section class="dash-profiles" aria-labelledby="dashProfilesTitle">' +
      '<p class="dash-sec__t" id="dashProfilesTitle">' + esc(t("dashboard.sec_voice")) + "</p>" +
      '<p class="dash-row__d">' + esc(t("dashboard.d_ttsChannelId")) + "</p>" +
      '<div class="dash-profiles__grid">' +
      '<label class="dash-profile-field">Text channel<select class="dash-sel" id="dashProfileChannel">' +
      profileOptionHtml(channels, "", "Choose a channel") +
      "</select></label>" +
      '<label class="dash-profile-field">' + esc(t("dashboard.f_autoread")) + triStateHtml("autoRead") + "</label>" +
      '<label class="dash-profile-field">' +
      triStateHtml("translationEnabled") +
      "</label>" +
      '<label class="dash-profile-field">' + esc(t("dashboard.f_readBots")) + triStateHtml("readBots") + "</label>" +
      '<label class="dash-profile-field">' + esc(t("dashboard.f_defaultVoice")) + '<select class="dash-sel" data-p="defaultVoice">' +
      profileOptionHtml(voices, "", "Inherit server voice") +
      "</select></label>" +
      '<label class="dash-profile-field">Engine<select class="dash-sel" data-p="engine"><option value="">Inherit</option><option value="google">Default</option><option value="piper">Piper</option><option value="kokoro">Kokoro (paid)</option><option value="gcloud">Google HD (paid)</option></select></label>' +
      '<label class="dash-profile-field">' + esc(t("dashboard.f_locale")) + '<select class="dash-sel" data-p="locale">' +
      profileOptionHtml(locales, "", "Infer from the voice") +
      "</select></label>" +
      '<label class="dash-profile-field">Voice effect<select class="dash-sel" data-p="effect"><option value="">None / personal choice</option><option value="robot">Robot</option><option value="echo">Echo</option><option value="deep">Deep (paid)</option><option value="chipmunk">Chipmunk (paid)</option><option value="radio">Radio (paid)</option><option value="phone">Phone (paid)</option><option value="underwater">Underwater (paid)</option><option value="demon">Demon (paid)</option></select></label>' +
      '<label class="dash-profile-field">Voice-channel binding<select class="dash-sel" data-p="voiceChannelId">' +
      profileOptionHtml(voiceChannels, "", "Any active call") +
      "</select></label>" +
      '<label class="dash-profile-field">Speed (0.5–2.0)<input class="dash-num" data-p="speed" type="number" min="0.5" max="2" step="0.05" placeholder="Inherit"></label>' +
      '<label class="dash-profile-field">' + esc(t("dashboard.f_maxChars")) + '<input class="dash-num" data-p="maxChars" type="number" min="1" max="2000" placeholder="Inherit"></label>' +
      "</div>" +
      '<div class="dash-profile-actions"><button type="button" class="' +
      BTN +
      '" id="dashProfileSave">Save profile</button><button type="button" class="btn btn--ghost" id="dashProfileDelete">Delete profile</button><span class="dash-status" id="dashProfileStatus" aria-live="polite"></span></div></section>'
    );
  }

  function wireProfileEditor(guild, guilds, meta) {
    if (!meta.capabilities.channelProfiles) return;
    var channel = document.getElementById("dashProfileChannel");
    var save = document.getElementById("dashProfileSave");
    var remove = document.getElementById("dashProfileDelete");
    var status = document.getElementById("dashProfileStatus");
    if (!channel || !save || !remove || !status) return;

    function selectedProfile() {
      var profiles = Array.isArray(meta.channelProfiles) ? meta.channelProfiles : [];
      for (var i = 0; i < profiles.length; i++) {
        if (profiles[i].channelId === channel.value) return profiles[i];
      }
      return null;
    }
    function setProfileValue(key, value) {
      var input = root.querySelector('[data-p="' + key + '"]');
      if (!input) return;
      input.value = value === null || value === undefined ? "" : String(value);
    }
    function loadSelected() {
      var profile = selectedProfile();
      ["autoRead", "translationEnabled", "readBots", "defaultVoice", "engine", "locale", "effect", "voiceChannelId", "speed", "maxChars"].forEach(function (key) {
        setProfileValue(key, profile ? profile[key] : null);
      });
      remove.disabled = !profile;
      status.textContent = "";
    }
    function profileBody() {
      var out = {};
      ["autoRead", "translationEnabled", "readBots"].forEach(function (key) {
        var value = root.querySelector('[data-p="' + key + '"]').value;
        out[key] = value === "" ? null : value === "true";
      });
      ["defaultVoice", "engine", "locale", "effect", "voiceChannelId"].forEach(function (key) {
        var value = root.querySelector('[data-p="' + key + '"]').value;
        out[key] = value === "" ? null : value;
      });
      ["speed", "maxChars"].forEach(function (key) {
        var value = root.querySelector('[data-p="' + key + '"]').value;
        out[key] = value === "" ? null : Number(value);
      });
      return out;
    }
    channel.addEventListener("change", loadSelected);
    save.addEventListener("click", function () {
      if (!channel.value) {
        status.textContent = "Choose a text channel first.";
        return;
      }
      status.textContent = "Saving…";
      fetch(API + "/api/dashboard/guild/" + guild.id + "/profile/" + channel.value, {
        method: "POST",
        headers: Object.assign({ "Content-Type": "application/json" }, authHeaders()),
        body: JSON.stringify(profileBody()),
      })
        .then(function (res) {
          if (!res.ok) throw new Error("profile-save");
          return res.json();
        })
        .then(function (data) {
          renderForm(guild, data.config, guilds, data, true);
        })
        .catch(function () {
          status.textContent = "Could not save this profile.";
        });
    });
    remove.addEventListener("click", function () {
      if (!channel.value || !selectedProfile()) return;
      status.textContent = "Deleting…";
      fetch(API + "/api/dashboard/guild/" + guild.id + "/profile/" + channel.value, {
        method: "DELETE",
        headers: authHeaders(),
      })
        .then(function (res) {
          if (res.status !== 204) throw new Error("profile-delete");
          return loadForm(guild, guilds);
        })
        .catch(function () {
          status.textContent = "Could not delete this profile.";
        });
    });
    loadSelected();
  }

  // Valor normalizado de um campo (bool/num/string) a partir de um objeto de config.
  function fieldValue(key, src) {
    var f = FIELD[key];
    if (f.type === "toggle") return !!src[key];
    if (f.type === "num") return Number(src[key]);
    if (f.type === "channel") return src[key] === null ? null : String(src[key]);
    if (f.type === "role") return src[key] === null ? null : String(src[key]);
    if (f.type === "voice") return String(src[key] || "");
    return String(src[key] || "en");
  }
  // Lê o valor atual do controlo no DOM.
  function domValue(key) {
    var el = root.querySelector('[data-k="' + key + '"]');
    if (!el) return undefined;
    var f = FIELD[key];
    if (f.type === "toggle") return el.checked;
    if (f.type === "num") return el.value.trim() === "" ? undefined : Number(el.value);
    if (f.type === "channel") return el.value === "" ? null : el.value;
    if (f.type === "role") return el.value === "" ? null : el.value;
    return el.value;
  }

  function renderForm(guild, cfg, guilds, meta, saved) {
    var sections = sectionsFor(meta);
    // Baseline para dirty-tracking: o botão só fica ativo quando algo muda.
    var baseline = {};
    eachField(sections, function (key) {
      baseline[key] = fieldValue(key, cfg);
    });

    var sectionsHtml = sections.map(function (sec) {
      var rows = sec.fields
        .map(function (k) {
          return rowHtml(k, cfg, meta);
        })
        .join("");
      return (
        '<div class="dash-sec"><p class="dash-sec__t">' +
        esc(t("dashboard.sec_" + sec.id)) +
        "</p>" +
        rows +
        "</div>"
      );
    }).join("");

    var savebar =
      '<div class="dash-savebar"><button type="button" class="' +
      BTN +
      ' dash-save" id="dashSave" disabled>' +
      esc(t("dashboard.save")) +
      '</button><span class="dash-status" id="dashStatus" aria-live="polite"></span></div>';

    view(
      '<div class="dash-form">' +
        headHtml(guild) +
        sectionsHtml +
        profileEditorHtml(meta) +
        savebar +
        "</div>",
    );
    wireIconFallback(root.querySelector(".dash-head__ic"), guild.name, "dash-head__ph");

    var formEl = root.querySelector(".dash-form");
    var saveBtn = document.getElementById("dashSave");
    var statusEl = document.getElementById("dashStatus");
    wireProfileEditor(guild, guilds, meta);

    function countChanges() {
      var n = 0;
      eachField(sections, function (key) {
        var value = domValue(key);
        if (FIELD[key].type === "num" && value === undefined) return;
        if (value !== baseline[key]) n++;
      });
      return n;
    }
    function setStatus(msg, cls) {
      statusEl.textContent = msg || "";
      statusEl.className = "dash-status" + (cls ? " dash-status--" + cls : "");
    }
    function refresh() {
      var n = countChanges();
      saveBtn.disabled = n === 0;
      saveBtn.textContent =
        n === 0
          ? t("dashboard.save")
          : n === 1
            ? t("dashboard.save1")
            : t("dashboard.saveN").replace("{n}", n);
      if (n > 0) setStatus(""); // limpa "Guardado ✓" assim que se volta a mexer
    }

    document.getElementById("dashBack").addEventListener("click", function () {
      renderPicker(guilds);
    });
    // Listeners no próprio form (substituído a cada render -> morrem com ele; sem leaks).
    formEl.addEventListener("input", refresh);
    function syncChannelAutoread(event) {
      var target = event.target;
      if (!target || target.getAttribute("data-k") !== "ttsChannelId") return;
      var autoread = root.querySelector('[data-k="autoread"]');
      if (autoread) autoread.checked = target.value !== "";
    }
    formEl.addEventListener("change", function (event) {
      syncChannelAutoread(event);
      refresh();
    });

    saveBtn.addEventListener("click", function () {
      if (saveBtn.disabled) return;
      var patch = {};
      eachField(sections, function (key) {
        var v = domValue(key);
        if (v === baseline[key]) return;
        if (FIELD[key].type === "num" && !isFinite(v)) return; // campo vazio -> não envia
        patch[key] = v;
      });
      saveBtn.disabled = true;
      saveBtn.textContent = t("dashboard.saving");
      setStatus("");
      fetch(API + "/api/dashboard/guild/" + guild.id, {
        method: "POST",
        headers: Object.assign({ "Content-Type": "application/json" }, authHeaders()),
        body: JSON.stringify(patch),
      })
        .then(function (res) {
          if (res.status === 401) {
            clearToken();
            renderLogin("dashboard.expired");
            return;
          }
          if (res.status === 403) {
            renderMessage("dashboard.forbidden", "dashboard.noneHint");
            return;
          }
          if (!res.ok) {
            setStatus(t("dashboard.saveFail"), "err");
            refresh();
            return;
          }
          return res.json();
        })
        .then(function (data) {
          // Rebuild the controls and dirty baseline from the authoritative server response.
          if (data && data.config) renderForm(guild, data.config, guilds, data, true);
        })
        .catch(function () {
          setStatus(t("dashboard.saveFail"), "err");
          refresh();
        });
    });

    refresh(); // estado inicial: sem alterações -> desativado
    if (saved) setStatus(t("dashboard.saved"), "ok");

    // Re-localizador in-place: reescreve só os text-nodes traduzíveis (títulos de secção,
    // nomes/descrições dos campos, botão voltar) e deixa o refresh() recalcular o rótulo do
    // Guardar com a contagem de alterações atual. Não toca nos inputs -> preserva valores e
    // o estado "por guardar". Registado como re-localizador enquanto o form está visível.
    onLang = function relocalizeForm() {
      var secEls = root.querySelectorAll(".dash-sec");
      sections.forEach(function (sec, i) {
        var tEl = secEls[i] && secEls[i].querySelector(".dash-sec__t");
        if (tEl) tEl.textContent = t("dashboard.sec_" + sec.id);
      });
      eachField(sections, function (key) {
        var ctrl = root.querySelector('[data-k="' + key + '"]');
        var row = ctrl && ctrl.closest ? ctrl.closest(".dash-row") : null;
        if (!row) return;
        var lEl = row.querySelector(".dash-row__l");
        var dEl = row.querySelector(".dash-row__d");
        if (lEl) lEl.textContent = fieldCopy(key, false);
        if (dEl) {
          var desc = fieldCopy(key, true);
          if (FIELD[key].type === "num") desc += " (" + FIELD[key].min + "–" + FIELD[key].max + ")";
          dEl.textContent = desc;
        }
        if (
          FIELD[key].type === "select" ||
          FIELD[key].type === "channel" ||
          FIELD[key].type === "voice"
          || FIELD[key].type === "role"
        ) {
          var currentValue = domValue(key);
          ctrl.innerHTML = selectOptionsHtml(key, currentValue, meta);
          ctrl.value = currentValue === null ? "" : String(currentValue);
        }
      });
      var back = document.getElementById("dashBack");
      if (back) back.textContent = t("dashboard.back");
      refresh(); // recomputa o rótulo do Guardar (usa a baseline/contagem do closure)
    };
  }

  function boot() {
    var tok = token();
    if (!tok) {
      renderLogin("");
      return;
    }
    // A token created by /account has `identify email`, not `guilds`. Re-authorize once for the
    // panel instead of sending it to the dashboard API and misleadingly displaying "expired".
    if (!hasDashboardAuth()) {
      login();
      return;
    }
    renderMessage("dashboard.loading", "");
    fetch(API + "/api/dashboard/guilds", { headers: authHeaders() })
      .then(function (res) {
        if (res.status === 401) {
          clearToken();
          renderLogin("dashboard.expired");
          return null;
        }
        if (!res.ok) {
          renderMessage("dashboard.error", "", { retry: true });
          return null;
        }
        return res.json();
      })
      .then(function (data) {
        if (!data) return;
        var guilds = data.guilds || [];
        if (!guilds.length) {
          renderMessage("dashboard.none", "dashboard.noneHint");
          return;
        }
        renderPicker(guilds);
      })
      .catch(function () {
        renderMessage("dashboard.error", "", { retry: true });
      });
  }

  window.addEventListener("vozen:languagechange", function () {
    if (typeof onLang === "function") onLang();
  });

  boot();
})();
