/* ═══════════════════════════════════════════════════════════
   Vozen site — main-v51.js
   ═══════════════════════════════════════════════════════════ */
(function () {
  "use strict";

  /* ── config ──────────────────────────────────────────────
     CLIENT_ID é público (está em qualquer link de convite).
     SUPPORT_URL é o convite do servidor de suporte do Vozen. */
  const CLIENT_ID = "1523826014935842997";
  const INVITE_PERMISSIONS = "326420745232"; // ManageChannels+Connect+Speak+ViewChannel+SendMessages+ReadMessageHistory+EmbedLinks + threads dos jogos (CreatePublicThreads+SendMessagesInThreads+ManageThreads)
  const SUPPORT_URL = "https://discord.gg/4kYw2WUbNN"; // servidor de suporte do Vozen
  // Painel Premium: base HTTPS da API do bot (GET /api/me/premium). VAZIO => o painel fica
  // escondido (a feature ainda não está no ar). Preenche com o teu host quando tiveres o
  // domínio/túnel + PREMIUM_API_ENABLED=true no bot. Ex.: "https://api.vozen.xyz".
  const PREMIUM_API_BASE = "https://api.vozen.org";
  const ACTIVATION_TERMS_VERSION = "2026-07-19";
  const ACTIVATION_INTENT_KEY = "vozen.activationIntent";
  const BILLING_INTENT_KEY = "vozen.billingIntent";
  const ACTIVATION_INTENT_TTL_MS = 5 * 60 * 1000;
  const INVITE_URL =
    CLIENT_ID && CLIENT_ID !== "YOUR_CLIENT_ID"
      ? `https://discord.com/oauth2/authorize?client_id=${CLIENT_ID}&permissions=${INVITE_PERMISSIONS}&scope=bot%20applications.commands`
      : "#";
  const TOPGG_URL =
    CLIENT_ID && CLIENT_ID !== "YOUR_CLIENT_ID" ? `https://top.gg/bot/${CLIENT_ID}/vote` : "#";

  // Forma do Ref de encomenda do Ko-fi, como aparece no recibo por email: `Ref: S-M1X823C9FW`.
  // Nao e um codigo que possamos aceitar — o webhook do Ko-fi nao envia este campo — mas e a unica
  // coisa com ar de codigo no email, por isso e o que a pessoa cola. Reconhece-se para a mandar
  // para a ajuda em vez de um 404 seco. Nao ha risco de apanhar um tx id verdadeiro: esses sao
  // UUIDs, e nenhum UUID comeca por "S-".
  const REF_RE = /^S-[A-Za-z0-9]{6,16}$/;

  const $ = (s, r = document) => r.querySelector(s);
  const $$ = (s, r = document) => [...r.querySelectorAll(s)];
  const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  // Página dedicada da conta (account.html). Só aí o painel Premium é o conteúdo
  // principal — e mostra o estado "em breve" enquanto o backend não está no ar.
  const IS_ACCOUNT = document.body.classList.contains("page-account");
  // Deterministic fixture for local visual QA. The hostname gate keeps this branch unreachable on
  // vozen.org; it exists so the authenticated layout can be reviewed without copying a real token
  // or Discord profile into development tools.
  const IS_ACCOUNT_PREVIEW =
    IS_ACCOUNT &&
    (location.hostname === "127.0.0.1" || location.hostname === "localhost") &&
    new URLSearchParams(location.search).get("preview") === "account";

  /* ── external links ──────────────────────────────────── */
  // target=_blank links get rel=noopener noreferrer (reverse-tabnabbing defence). The static
  // HTML links already carry it; these JS-assigned ones must set it too (SEC audit S7).
  $$(".js-invite").forEach((a) => {
    a.href = INVITE_URL;
    if (INVITE_URL !== "#") {
      a.target = "_blank";
      a.rel = "noopener noreferrer";
    }
  });
  $$(".js-support").forEach((a) => {
    a.href = SUPPORT_URL;
    a.target = "_blank";
    a.rel = "noopener noreferrer";
  });
  $$(".js-vote").forEach((a) => {
    a.href = TOPGG_URL;
    if (TOPGG_URL !== "#") {
      a.target = "_blank";
      a.rel = "noopener noreferrer";
    }
  });

  /* ── i18n ────────────────────────────────────────────── */
  const DICT = window.VOZEN_I18N;
  // Idioma por defeito: INGLÊS. NÃO fazemos sniffing do navigator (senão um browser PT
  // abria o site em português). Só respeitamos uma escolha EXPLÍCITA (guardada quando o
  // utilizador carrega no toggle EN/PT). Chave nova ("vozen.lang") para ignorar o valor que
  // a versão anterior auto-guardava a partir do navigator — assim toda a gente recomeça em EN.
  const LS_KEY = "vozen.lang";
  let lang = localStorage.getItem(LS_KEY) || "en";

  // Nome de uma língua NA LÍNGUA DO SITE (segue o botão EN/PT). Via Intl.DisplayNames
  // (dados do browser) — sem tabela à mão. PT devolve minúsculas ("inglês"), por isso
  // capitalizamos a 1.ª letra. Falha do ICU -> código em maiúsculas.
  const capFirst = (s) => (s ? s.charAt(0).toUpperCase() + s.slice(1) : s);
  function hearLangName(code, siteLang) {
    try {
      return capFirst(new Intl.DisplayNames([siteLang, "en"], { type: "language" }).of(code));
    } catch {
      return code.toUpperCase();
    }
  }
  // Reescreve as labels dos chips do "Hear it" (e o nome no cartão) na língua do site.
  function localizeHear(siteLang) {
    $$(".hear__chip").forEach((c) => {
      const nameEl = c.querySelector(".hear__chip-name");
      if (nameEl) nameEl.textContent = hearLangName(c.dataset.sample, siteLang);
    });
    const active = document.querySelector(".hear__chip.is-active");
    const langEl = document.getElementById("hearLang");
    if (active && langEl) langEl.textContent = hearLangName(active.dataset.sample, siteLang);
  }

  // Keep each translated hero phrase on the same single visual line. Some translations are
  // naturally wider than English (French is the longest current example), so a fixed font size
  // makes one phrase wrap and doubles the hero height. Measure the real rendered glyphs and only
  // reduce the display size when a phrase would overflow its column.
  function fitHeroTitle() {
    const title = document.querySelector(".hero__title");
    if (!title) return;
    title.style.removeProperty("font-size");
    const lines = [...title.querySelectorAll("span")];
    const available = title.clientWidth;
    const baseSize = Number.parseFloat(getComputedStyle(title).fontSize);
    if (!available || !baseSize || lines.length === 0) return;
    const widest = Math.max(...lines.map((line) => line.scrollWidth));
    if (widest <= available) return;
    const fitted = Math.max(34, Math.floor(baseSize * (available / widest) * 0.985));
    title.style.fontSize = `${fitted}px`;
  }

  let heroFitFrame = 0;
  function scheduleHeroTitleFit() {
    window.cancelAnimationFrame(heroFitFrame);
    heroFitFrame = window.requestAnimationFrame(fitHeroTitle);
  }

  window.addEventListener("resize", scheduleHeroTitleFit, { passive: true });
  if (document.fonts && document.fonts.ready) {
    document.fonts.ready.then(scheduleHeroTitleFit).catch(() => {});
  }

  // As 10 línguas do seletor: [código, bandeira, autónimo (nome na própria língua)].
  // O documento inteiro muda para RTL em árabe; as restantes línguas usam LTR.
  const LANG_UI = [
    ["en", "🇬🇧", "English"],
    ["pt", "🇵🇹", "Português"],
    ["fr", "🇫🇷", "Français"],
    ["es", "🇪🇸", "Español"],
    ["de", "🇩🇪", "Deutsch"],
    ["tr", "🇹🇷", "Türkçe"],
    ["ar", "🇸🇦", "العربية"],
    ["zh", "🇹🇼", "繁體中文"],
    ["ru", "🇷🇺", "Русский"],
    ["ko", "🇰🇷", "한국어"],
  ];
  const LANG_META = Object.fromEntries(LANG_UI.map(([c, flag, name]) => [c, { flag, name }]));

  // Sincroniza o dropdown custom com a língua ativa: trigger (bandeira+nome) e o item ativo.
  function syncLangMenu(code) {
    const m = LANG_META[code];
    if (!m) return;
    const bf = $("#langBtnFlag"),
      bn = $("#langBtnName");
    if (bf) bf.textContent = m.flag;
    if (bn) bn.textContent = m.name;
    $$(".lang__opt").forEach((o) => {
      const on = o.dataset.lang === code;
      o.classList.toggle("is-active", on);
      o.setAttribute("aria-selected", on ? "true" : "false");
    });
  }

  function applyLang(l) {
    lang = DICT[l] ? l : "en";
    document.documentElement.lang = lang;
    document.documentElement.dir = lang === "ar" ? "rtl" : "ltr";
    const d = DICT[lang];
    $$("[data-i18n]").forEach((el) => {
      const k = el.getAttribute("data-i18n");
      if (d[k] != null) el.textContent = d[k];
    });
    const translatedAttributes = [
      ["data-i18n-aria-label", "aria-label"],
      ["data-i18n-placeholder", "placeholder"],
      ["data-i18n-title", "title"],
    ];
    translatedAttributes.forEach(([marker, attribute]) => {
      $$(`[${marker}]`).forEach((el) => {
        const k = el.getAttribute(marker);
        if (d[k] != null) el.setAttribute(attribute, d[k]);
      });
    });
    const documentTitleKey = document.body?.getAttribute("data-i18n-document-title");
    if (documentTitleKey && d[documentTitleKey] != null) document.title = d[documentTitleKey];
    syncLangMenu(lang);
    localizeHear(lang);
    fitHeroTitle();
    scheduleHeroTitleFit();
    renderCommands();
    renderFaq();
    renderPanel(); // re-renderiza o painel Premium na língua atual (sem novo fetch)
    window.dispatchEvent(
      new CustomEvent("vozen:languagechange", { detail: { language: lang } }),
    );
  }

  // Dropdown custom de idioma (estilo MEE6): botão-trigger + painel role="listbox".
  // Nativo <select> não mostra bandeiras (o <option> é desenhado pelo SO), daí o custom.
  function buildLangMenu() {
    const menu = $("#langMenu"),
      btn = $("#langBtn"),
      panel = $("#langPanel");
    if (!menu || !btn || !panel) return;
    panel.innerHTML = LANG_UI.map(
      ([code, flag, name]) =>
        `<li class="lang__opt" role="option" data-lang="${code}" aria-selected="false" tabindex="-1">` +
        `<span class="lang__flag">${flag}</span><span class="lang__optname">${name}</span>` +
        `<svg class="lang__check" viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="3" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M20 6L9 17l-5-5"/></svg></li>`,
    ).join("");
    const opts = $$(".lang__opt", panel);
    const isOpen = () => menu.classList.contains("is-open");
    const open = () => {
      menu.classList.add("is-open");
      btn.setAttribute("aria-expanded", "true");
      const cur = panel.querySelector('[aria-selected="true"]') || opts[0];
      if (cur) cur.focus();
    };
    const close = (focusBtn) => {
      menu.classList.remove("is-open");
      btn.setAttribute("aria-expanded", "false");
      if (focusBtn) btn.focus();
    };
    const choose = (code) => {
      localStorage.setItem(LS_KEY, code); // escolha explícita persiste
      applyLang(code);
    };
    btn.addEventListener("click", () => (isOpen() ? close(false) : open()));
    btn.addEventListener("keydown", (e) => {
      if (e.key === "ArrowDown" || e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        open();
      }
    });
    opts.forEach((o, i) => {
      o.addEventListener("click", () => {
        choose(o.dataset.lang);
        close(true);
      });
      o.addEventListener("keydown", (e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          choose(o.dataset.lang);
          close(true);
        } else if (e.key === "ArrowDown") {
          e.preventDefault();
          (opts[i + 1] || opts[0]).focus();
        } else if (e.key === "ArrowUp") {
          e.preventDefault();
          (opts[i - 1] || opts[opts.length - 1]).focus();
        } else if (e.key === "Home") {
          e.preventDefault();
          opts[0].focus();
        } else if (e.key === "End") {
          e.preventDefault();
          opts[opts.length - 1].focus();
        } else if (e.key === "Escape") {
          e.preventDefault();
          close(true);
        }
      });
    });
    // clique fora fecha
    document.addEventListener("click", (e) => {
      if (isOpen() && !menu.contains(e.target)) close(false);
    });
  }
  buildLangMenu();

  /* ── toggle Mês/Ano dos preços ───────────────────────── */
  // Só troca o que se vê (classe .is-annual na grelha); os spans .amt/.per fazem o resto.
  const pricingGrid = $(".pricing");
  $$(".bill-toggle__btn").forEach((b) =>
    b.addEventListener("click", () => {
      const annual = b.dataset.bill === "year";
      if (pricingGrid) pricingGrid.classList.toggle("is-annual", annual);
      $$(".bill-toggle__btn").forEach((x) => x.classList.toggle("is-active", x === b));
    }),
  );

  /* ── toggle 2/5 servidores (só no cartão Premium) ─────── */
  // Mesma tier, muda o nº de licenças e o preço. Como o bill-toggle: só troca uma classe
  // (.is-5 no cartão); o CSS mostra o combo certo (preço/período/riscado/deal/nota).
  const proCard = $(".price-card--pro");
  $$(".seat-toggle__btn").forEach((b) =>
    b.addEventListener("click", () => {
      if (proCard) proCard.classList.toggle("is-5", b.dataset.seats === "5");
      $$(".seat-toggle__btn").forEach((x) => x.classList.toggle("is-active", x === b));
    }),
  );

  /* ── link de compra do Ko-fi por estado dos toggles ───────
     As memberships do Ko-fi só cobram ao MES, por isso os passes anuais sao produtos da
     Shop, cada um com o seu link proprio. Os toggles acima so trocam uma classe e deixam o
     CSS mostrar o preco certo — mas um href nao e algo que o CSS mude. Sem isto, quem
     escolhe "anual" vê o preço anual e recebe o link direto para o produto correspondente.
     Os codigos tem de bater certo com o KOFI_SHOP_MAP do bot: e o direct_link_code que diz
     ao webhook o que foi comprado (o Ko-fi nao envia o nome do produto). */
  /* ── Stripe checkout ───────────────────────────────────────────────── */
  function readBillingIntent() {
    try {
      const raw = sessionStorage.getItem(BILLING_INTENT_KEY);
      sessionStorage.removeItem(BILLING_INTENT_KEY);
      if (!raw) return null;
      const value = JSON.parse(raw);
      return value && ["plus", "premium"].includes(value.plan) && ["monthly", "yearly"].includes(value.interval) ? value : null;
    } catch { return null; }
  }

  let billingCheckoutOpener = null;
  let billingRequest = null;
  let billingScrollY = 0;
  let billingPendingScrollY = null;
  let billingBodyStyle = null;
  let billingStripeCheckout = null;
  let billingActions = null;
  let billingPaymentElement = null;
  let billingExpressElement = null;
  let billingSessionId = null;
  let billingConfirming = false;

  const billingAppearance = {
    theme: "night",
    variables: {
      colorPrimary: "#2ee6c8", colorBackground: "#0a0c1a", colorText: "#f6f7fe",
      colorTextSecondary: "#a3a9c4", colorDanger: "#ff6ea9", borderRadius: "12px",
      fontFamily: "Outfit, system-ui, sans-serif", spacingUnit: "4px",
    },
    rules: { ".Input": { border: "1px solid rgba(255,255,255,.13)", boxShadow: "none" } },
  };

  function lockBillingScroll() {
    if (billingBodyStyle) return;
    billingScrollY = billingPendingScrollY ?? window.scrollY;
    billingPendingScrollY = null;
    billingBodyStyle = {
      position: document.body.style.position, top: document.body.style.top,
      left: document.body.style.left, right: document.body.style.right,
      width: document.body.style.width, overflow: document.body.style.overflow,
    };
    document.body.style.position = "fixed";
    document.body.style.top = `-${billingScrollY}px`;
    document.body.style.left = "0";
    document.body.style.right = "0";
    document.body.style.width = "100%";
    document.body.style.overflow = "hidden";
  }

  function unlockBillingScroll() {
    if (!billingBodyStyle) return;
    const previous = billingBodyStyle;
    document.body.style.position = previous.position;
    document.body.style.top = previous.top;
    document.body.style.left = previous.left;
    document.body.style.right = previous.right;
    document.body.style.width = previous.width;
    document.body.style.overflow = previous.overflow;
    billingBodyStyle = null;
    billingPendingScrollY = null;
    window.scrollTo(0, billingScrollY);
  }

  function billingCheckoutModal(plan, interval) {
    const planName = plan === "plus" ? t("price.plus.name") : plan === "premium" ? t("price.pro.name") : t("price.max.name");
    const cadence = interval === "yearly" ? t("price.billYearly") : t("price.billMonthly");
    return (
      `<div class="billing-modal" id="vozenBillingModal" hidden>` +
      `<div class="billing-modal__backdrop" id="vozenBillingBackdrop"></div>` +
      `<section class="billing-modal__dialog" role="dialog" aria-modal="true" aria-labelledby="vozenBillingTitle" tabindex="-1">` +
      `<header class="billing-modal__head"><div><span class="billing-modal__eyebrow">${t("billing.eyebrow")}</span><h2 id="vozenBillingTitle">${planName}</h2><p>${cadence} · ${t("billing.secure")}</p></div>` +
      `<button class="billing-modal__close" id="vozenBillingClose" type="button" aria-label="${esc(t("claim.help.close"))}">&times;</button></header>` +
      `<div class="billing-modal__steps" aria-label="${esc(t("billing.progress"))}"><span class="is-active" id="vozenBillingStepPayment"><i>1</i><b>${t("billing.stepPayment")}</b></span><span id="vozenBillingStepDone"><i>2</i><b>${t("billing.stepDone")}</b></span></div>` +
      `<div class="billing-modal__body" id="vozenBillingBody">` +
      `<div class="billing-modal__auth" id="vozenBillingAuth" hidden><p class="billing-modal__lead">${t("panel.login")}</p><p>${t("price.idNote")}</p><button type="button" class="btn btn--primary btn--discord-cta billing-modal__login" id="vozenBillingLogin">${DISCORD_MARK}<span>${t("panel.login")}</span></button><p class="billing-modal__status" id="vozenBillingAuthStatus" role="status" aria-live="polite"></p></div>` +
      `<div class="billing-modal__payment" id="vozenBillingPayment" hidden><div class="billing-modal__checkout" id="vozenEmbeddedCheckout" aria-live="polite"><div class="billing-modal__loading"><span class="billing-modal__spinner" aria-hidden="true"></span><p>${t("billing.loading")}</p></div></div><p class="billing-modal__trust"><span aria-hidden="true">⌁</span> ${t("billing.secure")}</p></div>` +
      `<div class="billing-modal__done" id="vozenBillingDone" hidden><div class="billing-modal__done-icon" aria-hidden="true">✓</div><h3>${t("billing.doneTitle")}</h3><p id="vozenBillingDoneText" role="status" aria-live="polite">${t("billing.processing")}</p><button type="button" class="btn btn--primary" id="vozenBillingDoneClose">${t("claim.help.close")}</button></div>` +
      `</div></section></div>`
    );
  }

  function setBillingPhase(phase) {
    const modal = document.getElementById("vozenBillingModal");
    if (!modal) return;
    const auth = modal.querySelector("#vozenBillingAuth");
    const payment = modal.querySelector("#vozenBillingPayment");
    const done = modal.querySelector("#vozenBillingDone");
    const stepPayment = modal.querySelector("#vozenBillingStepPayment");
    const stepDone = modal.querySelector("#vozenBillingStepDone");
    if (auth) auth.hidden = phase !== "auth";
    if (payment) payment.hidden = phase !== "payment";
    if (done) done.hidden = phase !== "done";
    stepPayment?.classList.toggle("is-active", phase !== "done");
    stepDone?.classList.toggle("is-active", phase === "done");
    modal.dataset.phase = phase;
  }

  function showBillingError(message = t("panel.error"), allowHostedFallback = false) {
    const checkout = document.getElementById("vozenEmbeddedCheckout");
    if (!checkout) return;
    checkout.innerHTML = `<div class="billing-modal__error"><span class="billing-modal__error-icon" aria-hidden="true">!</span><strong>${esc(message)}</strong><button type="button" class="btn btn--primary" id="vozenBillingRetry">${t("panel.retry")}</button>${allowHostedFallback ? `<button type="button" class="billing-modal__hosted" id="vozenBillingHostedFallback">${t("billing.hostedFallback")}</button>` : ""}</div>`;
    document.getElementById("vozenBillingRetry")?.addEventListener("click", () => void continueBillingCheckout());
    document.getElementById("vozenBillingHostedFallback")?.addEventListener("click", () => void continueHostedBillingCheckout());
  }

  function resetBillingStripe() {
    try { billingPaymentElement?.destroy?.(); } catch {}
    try { billingExpressElement?.destroy?.(); } catch {}
    try { billingStripeCheckout?.destroy?.(); } catch {}
    billingStripeCheckout = null;
    billingActions = null;
    billingPaymentElement = null;
    billingExpressElement = null;
    billingSessionId = null;
    billingConfirming = false;
  }

  function closeBillingCheckout() {
    const modal = document.getElementById("vozenBillingModal");
    if (!modal || modal.hidden || billingConfirming) return;
    resetBillingStripe();
    modal.remove();
    document.body.classList.remove("is-modal-open");
    unlockBillingScroll();
    const trigger = billingRequest?.button;
    if (trigger instanceof HTMLButtonElement) {
      trigger.disabled = false;
      trigger.textContent = trigger.dataset.previousText || trigger.textContent;
      trigger.focus({ preventScroll: true });
    } else {
      billingCheckoutOpener?.focus?.({ preventScroll: true });
    }
    billingCheckoutOpener = null;
    billingRequest = null;
  }

  function openBillingCheckout(plan, interval, seats, button) {
    closeBillingCheckout();
    billingCheckoutOpener = document.activeElement;
    billingRequest = { plan, interval, seats, button };
    lockBillingScroll();
    document.documentElement.insertAdjacentHTML("beforeend", billingCheckoutModal(plan, interval));
    const modal = document.getElementById("vozenBillingModal");
    if (!modal) { unlockBillingScroll(); return null; }
    modal.hidden = false;
    document.body.classList.add("is-modal-open");
    document.getElementById("vozenBillingClose")?.addEventListener("click", closeBillingCheckout);
    document.getElementById("vozenBillingBackdrop")?.addEventListener("click", closeBillingCheckout);
    document.getElementById("vozenBillingDoneClose")?.addEventListener("click", closeBillingCheckout);
    document.getElementById("vozenBillingLogin")?.addEventListener("click", beginBillingAuth);
    modal.querySelector(".billing-modal__dialog")?.focus();
    setBillingPhase(storedToken() ? "payment" : "auth");
    return modal;
  }

  function beginBillingAuth() { login({ billing: true }); }

  function ensureStripeJs() {
    if (typeof window.Stripe === "function") return Promise.resolve(window.Stripe);
    return new Promise((resolve, reject) => {
      const existing = document.querySelector('script[data-vozen-stripe]');
      if (existing) {
        existing.addEventListener("load", () => typeof window.Stripe === "function" ? resolve(window.Stripe) : reject(new Error("Stripe unavailable")), { once: true });
        existing.addEventListener("error", () => reject(new Error("Stripe unavailable")), { once: true });
        window.setTimeout(() => reject(new Error("Stripe unavailable")), 7000);
        return;
      }
      const script = document.createElement("script");
      script.src = "https://js.stripe.com/dahlia/stripe.js";
      script.async = true;
      script.dataset.vozenStripe = "true";
      script.onload = () => typeof window.Stripe === "function" ? resolve(window.Stripe) : reject(new Error("Stripe unavailable"));
      script.onerror = () => reject(new Error("Stripe unavailable"));
      document.head.append(script);
    });
  }

  function checkoutRequestBody() {
    const request = billingRequest;
    return { plan: request.seats === 5 ? "max" : request.plan, interval: request.interval };
  }

  async function continueHostedBillingCheckout() {
    const token = storedToken();
    if (!token || !billingRequest) { setBillingPhase("auth"); return; }
    try {
      const res = await fetch(PREMIUM_API_BASE + "/api/billing/checkout", {
        method: "POST", headers: { "Content-Type": "application/json", Authorization: "Bearer " + token },
        body: JSON.stringify(checkoutRequestBody()),
      });
      const payload = await res.json().catch(() => ({}));
      if (res.status === 401) { try { sessionStorage.removeItem(TOK_KEY); } catch {} setBillingPhase("auth"); return; }
      if (!res.ok || !payload.url) throw new Error("hosted checkout unavailable");
      window.location.assign(payload.url);
    } catch { showBillingError(t("billing.hostedUnavailable")); }
  }

  async function mountPaymentElement(payload) {
    const checkoutRoot = document.getElementById("vozenEmbeddedCheckout");
    if (!checkoutRoot) return;
    const Stripe = await ensureStripeJs();
    const stripe = Stripe(payload.publishableKey);
    if (!stripe?.createEmbeddedCheckoutPage) throw new Error("Stripe embedded checkout unavailable");
    billingSessionId = payload.sessionId;
    billingStripeCheckout = await stripe.createEmbeddedCheckoutPage({
      clientSecret: payload.clientSecret,
      onComplete: async () => {
        setBillingPhase("done");
        const done = document.getElementById("vozenBillingDoneText");
        if (done) done.textContent = t("billing.processing");
        await refreshBillingAfterPayment(billingSessionId);
      },
    });
    if (!document.getElementById("vozenBillingModal")) {
      billingStripeCheckout.destroy?.();
      billingStripeCheckout = null;
      return;
    }
    checkoutRoot.innerHTML = "";
    billingStripeCheckout.mount("#vozenEmbeddedCheckout");
  }

  async function confirmBillingPayment(options = {}) {
    if (!billingActions || billingConfirming) return;
    billingConfirming = true;
    const button = document.getElementById("vozenBillingPay");
    if (button) { button.disabled = true; button.textContent = t("billing.processing"); }
    const status = document.getElementById("vozenPaymentStatus");
    if (status) status.textContent = "";
    try {
      const result = await billingActions.confirm({ ...options, redirect: "if_required" });
      if (result?.type === "error" || result?.error) {
        if (status) status.textContent = result.error?.message || t("billing.confirmError");
        return;
      }
      setBillingPhase("done");
      const done = document.getElementById("vozenBillingDoneText");
      if (done) done.textContent = t("billing.processing");
      await refreshBillingAfterPayment(billingSessionId);
    } catch { if (status) status.textContent = t("billing.confirmError"); }
    finally {
      billingConfirming = false;
      if (button) { button.disabled = false; button.textContent = t("billing.pay"); }
    }
  }

  async function paymentSessionStatus(sessionId) {
    const token = storedToken();
    if (!token || !sessionId) return null;
    const res = await fetch(PREMIUM_API_BASE + "/api/billing/checkout/status", {
      method: "POST", headers: { "Content-Type": "application/json", Authorization: "Bearer " + token },
      body: JSON.stringify({ session_id: sessionId }),
    });
    return res.ok ? res.json().catch(() => null) : null;
  }

  async function refreshBillingAfterPayment(sessionId) {
    for (const delay of [0, 900, 1800, 3200, 5000]) {
      if (delay) await new Promise((resolve) => window.setTimeout(resolve, delay));
      const result = await paymentSessionStatus(sessionId);
      if (result?.status === "complete" && result?.paymentStatus === "paid") {
        const done = document.getElementById("vozenBillingDoneText");
        if (done) done.textContent = t("billing.paymentReceived");
        void loadPanel();
        return;
      }
    }
    const done = document.getElementById("vozenBillingDoneText");
    if (done) done.textContent = t("billing.pending");
    void loadPanel();
  }

  async function continueBillingCheckout() {
    const token = storedToken();
    if (!billingRequest || !token) { setBillingPhase("auth"); return; }
    resetBillingStripe();
    setBillingPhase("payment");
    const checkout = document.getElementById("vozenEmbeddedCheckout");
    if (checkout) checkout.innerHTML = `<div class="billing-modal__loading"><span class="billing-modal__spinner" aria-hidden="true"></span><p>${t("billing.loading")}</p></div>`;
    try {
      const res = await fetch(PREMIUM_API_BASE + "/api/billing/checkout/elements", {
        method: "POST", headers: { "Content-Type": "application/json", Authorization: "Bearer " + token },
        body: JSON.stringify(checkoutRequestBody()),
      });
      const payload = await res.json().catch(() => ({}));
      if (res.status === 401) {
        try { sessionStorage.removeItem(TOK_KEY); } catch {}
        setBillingPhase("auth");
        const authStatus = document.getElementById("vozenBillingAuthStatus");
        if (authStatus) authStatus.textContent = t("claim.loginAgain");
        return;
      }
      if (!res.ok || !payload.clientSecret || !payload.publishableKey || !payload.sessionId) throw new Error(payload.error || "checkout unavailable");
      await mountPaymentElement(payload);
    } catch {
      showBillingError(t("billing.elementsUnavailable"), true);
    }
  }

  async function resumeBillingReturn() {
    const url = new URL(window.location.href);
    const sessionId = url.searchParams.get("session_id");
    if (url.searchParams.get("checkout") !== "return" || !sessionId) return false;
    url.searchParams.delete("checkout");
    url.searchParams.delete("session_id");
    history.replaceState(null, "", url.pathname + url.search + (url.hash || "#premium"));
    const result = await paymentSessionStatus(sessionId);
    if (!result) return false;
    openBillingCheckout("premium", "monthly", 2, null);
    setBillingPhase("done");
    const done = document.getElementById("vozenBillingDoneText");
    if (done) done.textContent = result.status === "complete" && result.paymentStatus === "paid" ? t("billing.paymentReceived") : t("billing.pending");
    void loadPanel();
    return true;
  }

  async function startCheckout(plan, interval, seats = 2) {
    const button = document.activeElement instanceof HTMLButtonElement ? document.activeElement : null;
    if (button) { button.disabled = true; button.dataset.previousText = button.textContent || ""; }
    if (!openBillingCheckout(plan, interval, seats, button)) return;
    if (storedToken()) void continueBillingCheckout();
  }

  const billingInterval = () => pricingGrid?.classList.contains("is-annual") ? "yearly" : "monthly";
  $$('[data-billing-plan]').forEach((button) => {
    const preserveScrollBeforeFocus = (event) => {
      if (event.type === "keydown" && !["Enter", " "].includes(event.key)) return;
      billingPendingScrollY = window.scrollY;
    };
    button.addEventListener("pointerdown", preserveScrollBeforeFocus, { capture: true });
    button.addEventListener("keydown", preserveScrollBeforeFocus, { capture: true });
  });
  document.querySelector("[data-billing-plan='plus']")?.addEventListener("click", () => void startCheckout("plus", billingInterval(), 1));
  document.querySelector("[data-billing-plan='premium']")?.addEventListener("click", () => void startCheckout("premium", billingInterval(), proCard?.classList.contains("is-5") ? 5 : 2));

  /* ── Painel Premium (login com Discord + estado da conta) ─────────────
     OAuth2 implicit (scopes identify + email): 100% client-side, sem segredo. O token vem no
     fragment (#access_token), guardamo-lo em sessionStorage, limpamos o fragment, e
     chamamos GET {API_BASE}/api/me/premium. A API valida o token na Discord e devolve só
     o estado DESTE utilizador. Escondido enquanto PREMIUM_API_BASE estiver vazio. */
  const TOK_KEY = "vozen.dtoken";
  const STATE_KEY = "vozen.oauthstate";
  const NAV_USER_KEY = "vozen.navuser";
  const OAUTH_REDIRECT = new URL("/account", location.href).href;
  const BILLING_OAUTH_REDIRECT = new URL("/", location.href).href;
  let panelState = { mode: "hidden" };
  let activationResume = { kind: "none" };

  // Billing ships independently from the long-lived i18n bundle. Keep a small English fallback
  // here so a browser holding the previous four-hour i18n cache never renders raw billing keys.
  const BILLING_COPY_FALLBACKS = {
    "billing.eyebrow": "Plan & payment",
    "billing.secure": "Secure payment handled by Stripe",
    "billing.progress": "Checkout progress",
    "billing.stepPayment": "Plan & payment",
    "billing.stepDone": "Done",
    "billing.loading": "Preparing secure checkout…",
    "billing.doneTitle": "Payment update",
    "billing.processing": "Confirming your payment…",
    "billing.email": "Email",
    "billing.or": "or pay with a card",
    "billing.promo": "Promotion code",
    "billing.promoPlaceholder": "Enter code",
    "billing.apply": "Apply",
    "billing.promoApplied": "Promotion code applied",
    "billing.pay": "Pay and subscribe",
    "billing.confirmError": "Your payment could not be confirmed. Check the details and try again.",
    "billing.paymentReceived": "Payment received. Your Premium access is being activated.",
    "billing.pending": "Your payment is being processed. Premium access will appear here shortly.",
    "billing.elementsUnavailable": "Secure on-site checkout is unavailable right now. You can continue on Stripe instead.",
    "billing.hostedFallback": "Open secure Stripe checkout",
    "billing.hostedUnavailable": "Stripe checkout is temporarily unavailable. Please try again shortly.",
  };
  const t = (k) =>
    (DICT[lang] && DICT[lang][k]) ||
    (DICT.en && DICT.en[k]) ||
    BILLING_COPY_FALLBACKS[k] ||
    k;
  const esc = (s) =>
    String(s).replace(
      /[&<>"']/g,
      (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c],
    );
  const fmtDate = (ts) => {
    try {
      return new Date(ts).toLocaleDateString(lang, { year: "numeric", month: "short", day: "numeric" });
    } catch {
      return new Date(ts).toISOString().slice(0, 10);
    }
  };
  const DISCORD_MARK =
    '<svg viewBox="0 0 24 18" width="20" height="15" aria-hidden="true" fill="currentColor"><path d="M20.3 1.6A19.8 19.8 0 0 0 15.4.1a14 14 0 0 0-.6 1.3 18.3 18.3 0 0 0-5.5 0A13 13 0 0 0 8.6.1 19.7 19.7 0 0 0 3.7 1.6C.6 6.3-.3 10.8.2 15.3a19.9 19.9 0 0 0 6 3 14.7 14.7 0 0 0 1.3-2.1 12.9 12.9 0 0 1-2-1c.2-.1.3-.3.5-.4a14.2 14.2 0 0 0 12 0l.5.4a12.8 12.8 0 0 1-2 1 14.5 14.5 0 0 0 1.3 2.1 19.8 19.8 0 0 0 6-3c.6-5.2-.8-9.7-3.5-13.7ZM8 12.6c-1.2 0-2.1-1.1-2.1-2.4S6.8 7.8 8 7.8s2.2 1.1 2.1 2.4c0 1.3-.9 2.4-2.1 2.4Zm8 0c-1.2 0-2.1-1.1-2.1-2.4s.9-2.4 2.1-2.4 2.2 1.1 2.1 2.4c0 1.3-.9 2.4-2.1 2.4Z"/></svg>';

  function storedToken() {
    try {
      return sessionStorage.getItem(TOK_KEY);
    } catch {
      return null;
    }
  }

  function cachedNavData() {
    if (!storedToken()) return null;
    try {
      const data = JSON.parse(sessionStorage.getItem(NAV_USER_KEY) || "null");
      return data && data.user ? data : null;
    } catch {
      return null;
    }
  }

  function cacheNavData(data) {
    try {
      if (data && data.user) {
        sessionStorage.setItem(NAV_USER_KEY, JSON.stringify({ user: data.user }));
      } else {
        sessionStorage.removeItem(NAV_USER_KEY);
      }
    } catch {}
  }

  function navDataForState(s = panelState) {
    return s.mode === "ok" ? s.data : cachedNavData();
  }

  function unlockAccountPage() {
    if (IS_ACCOUNT) document.body.classList.add("is-account-ready");
  }

  function randState() {
    const a = new Uint8Array(16);
    const c = window.crypto || window.msCrypto;
    // Fail-closed: sem CSPRNG não geramos um `state` fraco/previsível (seria um oráculo
    // de CSRF no OAuth). Abortamos o login — todos os browsers modernos têm crypto.
    if (!c || typeof c.getRandomValues !== "function") {
      throw new Error("secure-random-unavailable");
    }
    c.getRandomValues(a);
    return [...a].map((b) => b.toString(16).padStart(2, "0")).join("");
  }

  function login(options = {}) {
    let state;
    let nonce;
    try {
      state = randState();
      nonce = randState();
    } catch {
      alert("Your browser can't generate a secure login token. Please update it and try again.");
      return;
    }
    try {
      sessionStorage.setItem(STATE_KEY, state);
      if (options && options.billing === true) {
        const request = billingRequest;
        if (request && ["plus", "premium"].includes(request.plan) && ["monthly", "yearly"].includes(request.interval)) {
          sessionStorage.setItem(
            BILLING_INTENT_KEY,
            JSON.stringify({ plan: request.plan, interval: request.interval, seats: request.seats || 2 }),
          );
        } else {
          sessionStorage.removeItem(BILLING_INTENT_KEY);
        }
      } else {
        sessionStorage.removeItem(BILLING_INTENT_KEY);
      }
      if (options && options.resumeActivation === true) {
        sessionStorage.setItem(
          ACTIVATION_INTENT_KEY,
          JSON.stringify({
            action: "activate-by-email",
            termsVersion: ACTIVATION_TERMS_VERSION,
            createdAt: Date.now(),
            nonce,
            oauthState: state,
          }),
        );
      } else {
        sessionStorage.removeItem(ACTIVATION_INTENT_KEY);
      }
    } catch {}
    const u = new URL("https://discord.com/oauth2/authorize");
    u.searchParams.set("client_id", CLIENT_ID);
    u.searchParams.set("redirect_uri", options && options.billing === true ? BILLING_OAUTH_REDIRECT : OAUTH_REDIRECT);
    u.searchParams.set("response_type", "token");
    u.searchParams.set("scope", "identify email");
    u.searchParams.set("state", state);
    location.href = u.toString();
    return null;
  }

  function logout() {
    try {
      sessionStorage.removeItem(TOK_KEY);
      sessionStorage.removeItem(NAV_USER_KEY);
      sessionStorage.removeItem(ACTIVATION_INTENT_KEY);
    } catch {}
    if (IS_ACCOUNT) {
      window.location.href = "/";
      return;
    }
    setPanel({ mode: "anon" });
  }

  function logoutConfirmModal() {
    return (
      `<div class="ppmodal ppanel__logoutconfirm" id="ppLogoutConfirm" hidden>` +
      `<div class="ppmodal__backdrop" id="ppLogoutConfirmBackdrop"></div>` +
      `<div class="ppanel__logoutconfirmbox" role="dialog" aria-modal="true" aria-labelledby="ppLogoutConfirmTitle" aria-describedby="ppLogoutConfirmBody" tabindex="-1">` +
      `<span class="ppanel__logoutconfirmicon" aria-hidden="true"><svg viewBox="0 0 24 24" width="22" height="22" fill="none" stroke="currentColor" stroke-width="1.9"><path d="M10 5H6.5A2.5 2.5 0 0 0 4 7.5v9A2.5 2.5 0 0 0 6.5 19H10M14 8l4 4-4 4M8 12h10"/></svg></span>` +
      `<h2 id="ppLogoutConfirmTitle">${t("account.logoutConfirmTitle")}</h2>` +
      `<p id="ppLogoutConfirmBody">${t("account.logoutConfirmBody")}</p>` +
      `<div class="ppanel__logoutconfirmactions">` +
      `<button type="button" class="ppanel__logoutcancel" id="ppLogoutConfirmCancel">${t("account.logoutCancel")}</button>` +
      `<button type="button" class="ppanel__logoutaction" id="ppLogoutConfirmAction">${t("account.logoutConfirmAction")}</button>` +
      `</div></div></div>`
    );
  }

  // Lê o token do fragment no regresso do OAuth, valida o `state` (CSRF) e LIMPA o fragment.
  function readTokenFromHash() {
    if (!location.hash || location.hash.length < 2) return null;
    const p = new URLSearchParams(location.hash.slice(1));
    const tok = p.get("access_token");
    if (!tok) return null;
    const st = p.get("state");
    let expected = null;
    try {
      expected = sessionStorage.getItem(STATE_KEY);
      sessionStorage.removeItem(STATE_KEY);
    } catch {}
    history.replaceState(null, "", location.pathname + location.search); // fragment fora do URL
    // CSRF: exige um `state` guardado que bata certo. Sem ele (ou diferente) => descarta o
    // token — nunca aceitamos um fragment que não conseguimos verificar como nosso.
    if (!expected || st !== expected) return null;
    return { token: tok, state: st };
  }

  // The activation intent is consumed before any replay. It is tied to the OAuth state, expires
  // quickly, and cannot survive a terms-version change, which makes the replay strictly one-shot.
  function consumeActivationIntent(oauthState) {
    let raw = null;
    try {
      raw = sessionStorage.getItem(ACTIVATION_INTENT_KEY);
      sessionStorage.removeItem(ACTIVATION_INTENT_KEY);
    } catch {}
    if (!raw) return { kind: "none" };
    try {
      const intent = JSON.parse(raw);
      const age = Date.now() - Number(intent.createdAt);
      const valid =
        intent.action === "activate-by-email" &&
        intent.termsVersion === ACTIVATION_TERMS_VERSION &&
        intent.oauthState === oauthState &&
        typeof intent.nonce === "string" &&
        intent.nonce.length >= 16 &&
        Number.isFinite(age) &&
        age >= 0 &&
        age <= ACTIVATION_INTENT_TTL_MS;
      return valid ? { kind: "resume" } : { kind: "invalid" };
    } catch {
      return { kind: "invalid" };
    }
  }

  function setPanel(s) {
    if (s.mode === "ok") cacheNavData(s.data);
    if (s.mode === "anon" || (s.mode === "hidden" && !storedToken())) cacheNavData(null);
    panelState = s;
    renderNavLogin(navDataForState(s));
    renderPanel();
  }

  async function loadPanel() {
    if (IS_ACCOUNT_PREVIEW) {
      setPanel({
        mode: "ok",
        data: {
          user: { id: "100000000000000001", username: "Rexy", avatar: null },
          plus: { active: true, expiresAt: "2026-10-19T00:00:00.000Z" },
          pass: {
            active: true,
            used: 1,
            seats: 2,
            expiresAt: "2026-10-19T00:00:00.000Z",
            servers: [{ id: "123456789012345678", name: "Vozen Lab" }],
          },
        },
      });
      return;
    }
    if (!PREMIUM_API_BASE) {
      // Backend ainda não está no ar: na página da conta mostramos "em breve"
      // (nunca disparamos o OAuth, senão o redirect não registado dava erro no
      // Discord); nas outras páginas o painel fica escondido.
      setPanel(IS_ACCOUNT ? { mode: "soon" } : { mode: "hidden" });
      return;
    }
    const fromHash = readTokenFromHash();
    if (fromHash) {
      try {
        sessionStorage.setItem(TOK_KEY, fromHash.token);
      } catch {}
      let billingReturn = false;
      try { billingReturn = !!sessionStorage.getItem(BILLING_INTENT_KEY); } catch {}
      if (IS_ACCOUNT && billingReturn) {
        location.replace("/#premium");
        return;
      }
      activationResume = consumeActivationIntent(fromHash.state);
      // Bounce de regresso: o /dashboard reutiliza o redirect /account (o único registado no
      // portal) para o OAuth com scope `guilds`. O token fica no sessionStorage (mesmo
      // domínio), e saltamos de volta. Só caminhos internos (anti open-redirect).
      let rt = null;
      try {
        rt = sessionStorage.getItem("vozen.returnTo");
        sessionStorage.removeItem("vozen.returnTo");
      } catch {}
      // `^\/[^/]` (not just `^\/`): a leading "//" is protocol-relative, so "//evil"
      // would navigate OFF-SITE. Not reachable today (only our own code writes this key,
      // always "/dashboard") and the charset already rejects dots, so no real host could
      // be named — but this is the check that stops it becoming an open redirect the day
      // returnTo ever comes from somewhere less trusted.
      if (rt && /^\/[A-Za-z0-9_-][A-Za-z0-9/_-]*$/.test(rt)) {
        location.replace(rt);
        return;
      }
    }
    const tok = storedToken();
    if (!tok) {
      if (IS_ACCOUNT) {
        login();
        return;
      }
      setPanel({ mode: "hidden" });
      return;
    }
    setPanel({ mode: "loading" });
    try {
      const res = await fetch(PREMIUM_API_BASE + "/api/me/premium", {
        headers: { Authorization: "Bearer " + tok },
      });
      if (res.status === 401) {
        try {
          sessionStorage.removeItem(TOK_KEY);
          sessionStorage.removeItem(NAV_USER_KEY);
        } catch {}
        if (IS_ACCOUNT) {
          login();
          return;
        }
        setPanel({ mode: "anon" });
        return;
      }
      if (!res.ok) throw new Error("http " + res.status);
      setPanel({ mode: "ok", data: await res.json() });
      if (await resumeBillingReturn()) return;
      const purchase = readBillingIntent();
      if (purchase) {
        if (!IS_ACCOUNT) {
          history.replaceState(null, "", "#premium");
          document.getElementById("premium")?.scrollIntoView({ block: "start" });
        }
        window.setTimeout(() => void startCheckout(purchase.plan, purchase.interval, purchase.seats), 0);
        return;
      }
      const resume = activationResume;
      activationResume = { kind: "none" };
      if (resume.kind === "resume") {
        const consent = document.getElementById("ppClaimConsent");
        if (consent) consent.checked = true;
        window.setTimeout(
          () => void doInstantActivation(null, { allowRelogin: false }),
          0,
        );
      } else if (resume.kind === "invalid") {
        setClaimMessage(t("claim.resumeExpired"), "err");
      }
    } catch {
      setPanel({ mode: "error" });
    }
  }

  function discordDecorationAsset(u) {
    const asset =
      u.avatarDecorationAsset ||
      u.avatar_decoration_asset ||
      u.avatarDecorationData?.asset ||
      u.avatar_decoration_data?.asset ||
      "";
    return /^[A-Za-z0-9_]{1,128}$/.test(asset) ? asset : "";
  }

  function avatarMarkup(u, cls, size) {
    const initial = esc((u.username || "?").slice(0, 1).toUpperCase());
    let avatar;
    if (u.id && u.avatar) {
      const ext = String(u.avatar).startsWith("a_") ? "gif" : "png";
      const alt = esc(
        t("account.discordAvatar").replace("{name}", u.username || t("account.defaultUser")),
      );
      avatar = `<img class="${cls}" src="https://cdn.discordapp.com/avatars/${esc(u.id)}/${esc(u.avatar)}.${ext}?size=${size}" alt="${alt}" width="${size}" height="${size}" referrerpolicy="no-referrer">`;
    } else {
      avatar = `<span class="${cls} ${cls}--none">${initial}</span>`;
    }
    const decoration = discordDecorationAsset(u);
    if (!decoration) return avatar;
    return `<span class="discord-avatar ${cls}-wrap">${avatar}<img class="discord-avatar__decoration" src="https://cdn.discordapp.com/avatar-decoration-presets/${decoration}.png?size=256" alt="" aria-hidden="true" width="256" height="256" referrerpolicy="no-referrer"></span>`;
  }

  function statusRow(label, active, detail) {
    const state = active ? "is-on" : "is-off";
    const text = active ? t("account.active") : t("account.notActive");
    const action = active
      ? ""
      : `<a class="ppanel__get" href="/#premium">${t("nav.premium")} <span aria-hidden="true">→</span></a>`;
    return (
      `<article class="ppanel__status ${state}">` +
      `<div class="ppanel__status-top"><span class="ppanel__status-label">${esc(label)}</span>` +
      `<b class="ppanel__mark ${state}">${text}</b></div>` +
      (detail ? `<small class="ppanel__status-detail">${detail}</small>` : "") +
      action +
      `</article>`
    );
  }

  function renderNavLogin(data) {
    const btn = document.getElementById("navLogin");
    if (!btn) return;
    const u = data && data.user ? data.user : null;
    if (!u) {
      btn.classList.remove("nav__login--account");
      btn.removeAttribute("aria-label");
      btn.innerHTML = `${DISCORD_MARK}<span data-i18n="nav.login">${t("nav.login")}</span>`;
      return;
    }
    btn.classList.add("nav__login--account");
    const username = u.username || t("account.defaultUser");
    btn.setAttribute("aria-label", t("account.discordAccount").replace("{name}", username));
    btn.innerHTML = `${avatarMarkup(u, "nav__login-av", 24)}<span>${esc(username)}</span>`;
  }

  // Card "Ativar uma compra": o comprador cola o código do recibo Ko-fi e reclama a compra
  // que chegou sem Discord ID (o checkout de subscrição não tem caixa de mensagem). POST
  // autenticado a /api/link — ver doClaim. Aparece sempre que a pessoa está logada.
  function claimCard() {
    return "";
    /* return (
      `<div class="ppanel__claim ppmodal" id="activate-purchase" role="dialog" aria-modal="true" aria-labelledby="ppClaimTitle" hidden>` +
      `<div class="ppanel__claimbox" tabindex="-1">` +
      `<button type="button" class="ppanel__claimclose" id="ppClaimClose" aria-label="${esc(t("account.closeActivation"))}">&times;</button>` +
      `<div class="ppanel__claimhead"><span class="ppanel__claimeyebrow">${t("account.purchaseActivation")}</span>` +
      `<span class="ppanel__claimtitle" id="ppClaimTitle">${t("claim.title")}</span></div>` +
      // Consent is explicit in the label itself, before either delivery path. The server records a
      // stable terms version for instant activation; receipt-code activation keeps its HTTP contract.
      // {terms} no texto de claim.consent marca onde entra o link — substituido por um <a> para
      // /terms. O <a> e conteudo interativo: clicar nele abre os termos sem marcar a checkbox
      // (comportamento do <label> no HTML) — confirmado no browser.
      `<section class="ppanel__activationway ppanel__activationway--instant">` +
      `<div class="ppanel__wayhead"><span class="ppanel__waynum">01</span><span><b>${t("claim.instantBtn")}</b><small>${t("account.recommended")}</small></span></div>` +
      `<p class="ppanel__claimhint">${t("claim.instantHint")}</p>` +
      `<label class="ppanel__claimconsent"><input type="checkbox" id="ppClaimConsent"> <span>${t("claim.consent").replace("{terms}", `<a href="/terms" target="_blank" rel="noopener">${t("claim.consentTerms")}</a>`)}</span></label>` +
      `<p class="ppanel__claimmsg" id="ppInstantMsg" role="status" aria-live="polite" hidden></p>` +
      `<button type="button" class="ppanel__activatebtn" id="ppActivateBtn">${t("claim.instantBtn")}</button>` +
      `<p class="ppanel__gift">${t("claim.giftNote")}</p></section>` +
      `<details class="ppanel__receipt">` +
      `<summary><span class="ppanel__waynum">02</span><span><b>${t("claim.orReceipt")}</b><small>${t("claim.hint")}</small></span>` +
      `<svg viewBox="0 0 20 20" width="18" height="18" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true"><path d="m6 8 4 4 4-4"/></svg></summary>` +
      `<section class="ppanel__receiptbody">` +
      `<p class="ppanel__claimhint">${t("claim.hint")}</p>` +
      `<form class="ppanel__claimform" id="ppClaimForm">` +
      `<input type="text" id="ppClaimCode" class="ppanel__claiminput" placeholder="${esc(t("claim.placeholder"))}" autocomplete="off" autocapitalize="off" spellcheck="false" maxlength="120">` +
      `<button type="submit" class="ppanel__claimbtn" id="ppClaimBtn">${t("claim.btn")}</button>` +
      `</form>` +
      `<p class="ppanel__claimmsg" id="ppClaimMsg" role="status" aria-live="polite" hidden></p>` +
      // Sits AFTER the status message on purpose: "no purchase found" is the moment someone
      // realises they no longer have the receipt, and the way out should be the next thing they
      // read. Closing the receipt tab was never a dead end — Ko-fi emails every buyer a copy —
      // but the card never said so, which made it one in practice.
      `<p class="ppanel__claimlost">${t("claim.lost")} <button type="button" class="ppanel__claimlostbtn" id="ppClaimHelpOpen">${t("claim.lostHelp")}</button></p></section>` +
      `</details>` +
      `</div>` +
      `</div>`
    ); */
  }

  // Modal de ajuda (plano 036). Existe por causa de uma armadilha do recibo do Ko-fi: o email
  // mostra `Ref: S-M1X823C9FW` — a unica coisa com ar de codigo no email inteiro — e o Ref NUNCA
  // pode activar nada, porque o webhook do Ko-fi nao no-lo envia. Quem procura um codigo encontra
  // aquilo, cola na caixa de cima, e leva um 404 seco por uma compra que fez mesmo.
  //
  // Passo 1 leva ao caminho que ACTIVA (o botao do email -> endereco da pagina -> caixa de cima).
  // Passo 2 pede o EMAIL que a pessoa usou no Ko-fi — nao o Ref. Porque? A pesquisa de transacoes
  // do Ko-fi so casa por nome ou email (confirmado no painel real, 2026-07-17); o Ref nao e
  // procuravel. Um clique manda (Discord ID, email) para o dono, que cola o email na pesquisa do
  // Ko-fi, confirma a encomenda paga e faz o grant. O email e pista de procura, nao prova (plano
  // 021 continua de pe). O suporte fica no rodape, um passo mais dentro.
  //
  // O href do suporte vai inline e nao por class="js-support": essa ligacao corre UMA vez sobre o
  // documento no arranque e isto e injectado depois do OAuth — sairia sem href nenhum.
  function claimHelpModal() {
    return (
      `<div class="ppmodal" id="ppClaimHelp" hidden>` +
      `<div class="ppmodal__backdrop" id="ppClaimHelpBackdrop"></div>` +
      `<div class="ppmodal__box" role="dialog" aria-modal="true" aria-labelledby="ppClaimHelpTitle" tabindex="-1" id="ppClaimHelpBox">` +
      `<button type="button" class="ppmodal__x" id="ppClaimHelpClose" aria-label="${esc(t("claim.help.close"))}">&times;</button>` +
      `<h2 class="ppmodal__title" id="ppClaimHelpTitle">${t("claim.help.title")}</h2>` +
      `<p class="ppmodal__step"><span class="ppmodal__num">1</span> ${t("claim.help.step1")}</p>` +
      `<p class="ppmodal__step"><span class="ppmodal__num">2</span> ${t("claim.help.step2")}</p>` +
      `<form class="ppanel__claimform" id="ppClaimHelpForm">` +
      `<input type="email" id="ppClaimHelpEmail" class="ppanel__claiminput" placeholder="${esc(t("claim.help.emailPlaceholder"))}" autocomplete="email" autocapitalize="off" inputmode="email" spellcheck="false" maxlength="254">` +
      `<button type="submit" class="ppanel__claimbtn" id="ppClaimHelpSend">${t("claim.help.send")}</button>` +
      `</form>` +
      `<p class="ppanel__claimmsg" id="ppClaimHelpMsg" role="status" aria-live="polite" hidden></p>` +
      `<p class="ppmodal__foot"><a href="${SUPPORT_URL}" target="_blank" rel="noopener">${t("claim.help.stillStuck")}</a></p>` +
      `</div>` +
      `</div>`
    );
  }

  function renderOk(d) {
    const u = d.user || {};
    const av = avatarMarkup(u, "ppanel__av", 128);
    const plus = d.plus || {};
    const pass = d.pass;
    const plusActive = !!plus.active;
    const premiumActive = !!(pass && pass.active);
    const plusDetail = plus.expiresAt
      ? `${plusActive ? t("panel.activeUntil") : t("panel.expiredOn")} ${esc(fmtDate(plus.expiresAt))}`
      : "";
    const premiumParts = [];
    if (pass) {
      premiumParts.push(`${esc(pass.used ?? 0)} / ${esc(pass.seats ?? 0)} ${t("panel.seatsUsed")}`);
      if (pass.expiresAt) {
        premiumParts.push(`${premiumActive ? t("panel.activeUntil") : t("panel.expiredOn")} ${esc(fmtDate(pass.expiresAt))}`);
      }
    }
    let servers = "";
    if (premiumActive) {
      if (pass.servers && pass.servers.length) {
        const items = pass.servers
          .map((s) => `<li>${s.name ? esc(s.name) : "<code>" + esc(s.id) + "</code>"}</li>`)
          .join("");
        servers = `<div class="ppanel__servers"><span class="ppanel__meta">${t("panel.servers")}</span><ul>${items}</ul></div>`;
      } else {
        servers = `<p class="ppanel__meta">${t("panel.noServers")}</p>`;
      }
    }
    if (pass && pass.inactiveServers && pass.inactiveServers.length) {
      const label = lang === "pt" ? "Ligações guardadas (sem licença ativa)" : "Saved links (no active license)";
      const items = pass.inactiveServers
        .map((s) => `<li>${s.name ? esc(s.name) : "<code>" + esc(s.id) + "</code>"}</li>`)
        .join("");
      servers += `<div class="ppanel__servers"><span class="ppanel__meta">${label}</span><ul>${items}</ul></div>`;
    }
    return (
      `<div class="ppanel__account">` +
      `<div class="ppanel__user">${av}</div>` +
      `<div class="ppanel__identity"><span class="ppanel__name">${esc(u.username || t("account.defaultUser"))}</span>` +
      `<button type="button" class="ppanel__id" data-id="${esc(u.id || "")}" aria-label="${esc(t("account.copyDiscordId"))}">${t("account.discordId")}: <span class="ppanel__idnum">${esc(u.id || "-")}</span>` +
      `<svg class="ppanel__copyic" viewBox="0 0 24 24" width="14" height="14" fill="currentColor" aria-hidden="true"><path d="M8 7a3 3 0 0 1 3-3h7a3 3 0 0 1 3 3v7a3 3 0 0 1-3 3h-1v-2h1a1 1 0 0 0 1-1V7a1 1 0 0 0-1-1h-7a1 1 0 0 0-1 1v1H8V7Z"/><path d="M3 10a3 3 0 0 1 3-3h7a3 3 0 0 1 3 3v7a3 3 0 0 1-3 3H6a3 3 0 0 1-3-3v-7Zm3-1a1 1 0 0 0-1 1v7a1 1 0 0 0 1 1h7a1 1 0 0 0 1-1v-7a1 1 0 0 0-1-1H6Z"/></svg>` +
      `<svg class="ppanel__okic" viewBox="0 0 24 24" width="14" height="14" fill="currentColor" aria-hidden="true"><path d="M9.2 16.6 4.9 12.3l1.4-1.4 2.9 2.9 8.5-8.5 1.4 1.4-9.9 9.9Z"/></svg>` +
      `<span class="ppanel__tip" aria-hidden="true">${t("account.copyHint")}</span></button>` +
      `<button type="button" class="ppanel__logout" id="ppLogout"><svg class="ppanel__logout-icon" viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.9" aria-hidden="true"><path d="M10 5H6.5A2.5 2.5 0 0 0 4 7.5v9A2.5 2.5 0 0 0 6.5 19H10M14 8l4 4-4 4M8 12h10"/></svg><span>${t("panel.logout")}</span></button></div></div>` +
      `<div class="ppanel__benefitshead"><div><span class="account-kicker">${t("account.benefits")}</span><h3>${t("account.activeTitle")}</h3></div>` +
      `<p>${t("account.benefitsDescription")}</p></div>` +
      `<div class="ppanel__statusgrid">` +
      statusRow("Vozen Premium", premiumActive, premiumParts.join(" &middot; ")) +
      statusRow("Vozen Plus", plusActive, plusDetail) +
      `</div>${servers}${claimCard()}`
    );
  }

  function setClaimMessage(text, kind) {
    const msg = document.getElementById("ppInstantMsg");
    if (!msg) return null;
    msg.hidden = false;
    msg.textContent = text;
    msg.className = "ppanel__claimmsg" + (kind ? " is-" + kind : "");
    return msg;
  }

  function setClaimAction(text, label, action) {
    const msg = setClaimMessage(text, "err");
    if (!msg) return;
    msg.appendChild(document.createTextNode(" "));
    const button = document.createElement("button");
    button.type = "button";
    button.className = "ppanel__msglink";
    button.textContent = label;
    button.addEventListener("click", action, { once: true });
    msg.appendChild(button);
  }

  function downloadActivationConfirmation(confirmation, items) {
    const acceptedAt = Number(confirmation && confirmation.acceptedAt);
    const safeItems = Array.isArray(items)
      ? items.map((item) => ({
          plan: item && item.plan,
          days: item && item.days,
          seats: item && item.seats,
          expiresAt: item && item.expiresAt,
        }))
      : [];
    const payload = {
      service: "Vozen",
      confirmation: {
        id: confirmation && confirmation.id,
        acceptedAt: Number.isFinite(acceptedAt) ? acceptedAt : null,
        acceptedAtIso: Number.isFinite(acceptedAt) ? new Date(acceptedAt).toISOString() : null,
        termsVersion: confirmation && confirmation.termsVersion,
        method: confirmation && confirmation.method,
      },
      items: safeItems,
    };
    const blob = new Blob([JSON.stringify(payload, null, 2) + "\n"], {
      type: "application/json;charset=utf-8",
    });
    const href = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = href;
    link.download = `vozen-activation-${payload.confirmation.id || "confirmation"}.json`;
    document.body.appendChild(link);
    link.click();
    link.remove();
    window.setTimeout(() => URL.revokeObjectURL(href), 0);
  }

  function renderActivationSuccess(body) {
    const msg = setClaimMessage(t("claim.activationOk"), "ok");
    if (!msg) return;
    const items = Array.isArray(body.items) ? body.items : [];
    const list = document.createElement("ul");
    list.className = "ppanel__activationitems";
    for (const item of items) {
      const row = document.createElement("li");
      const plan = item && item.plan === "premium" ? "Vozen Premium" : "Vozen Plus";
      const parts = [plan, `${Number(item && item.days) || 0} ${t("claim.days")}`];
      if (item && item.plan === "premium") {
        parts.push(`${Number(item.seats) || 0} ${t("claim.seats")}`);
      }
      row.textContent = parts.join(" · ");
      list.appendChild(row);
    }
    msg.appendChild(list);
    const download = document.createElement("button");
    download.type = "button";
    download.className = "ppanel__msglink";
    download.textContent = t("claim.downloadConfirmation");
    download.addEventListener("click", () =>
      downloadActivationConfirmation(body.confirmation, items),
    );
    msg.appendChild(download);
  }

  async function doInstantActivation(ev, options = {}) {
    if (ev && typeof ev.preventDefault === "function") ev.preventDefault();
    const button = document.getElementById("ppActivateBtn");
    const consent = document.getElementById("ppClaimConsent");
    const receiptButton = document.getElementById("ppClaimBtn");
    const receiptInput = document.getElementById("ppClaimCode");
    const receiptMessage = document.getElementById("ppClaimMsg");
    if (!button) return;
    if (receiptMessage) receiptMessage.hidden = true;
    if (!consent || !consent.checked) {
      setClaimMessage(t("claim.consentRequired"), "err");
      consent?.focus();
      return;
    }
    const allowRelogin = options.allowRelogin !== false;
    const token = storedToken();
    if (!token) {
      if (allowRelogin) login({ resumeActivation: true });
      else
        setClaimAction(t("claim.loginAgain"), t("panel.login"), () =>
          login({ resumeActivation: true }),
        );
      return;
    }

    const previous = button.textContent;
    button.disabled = true;
    if (receiptButton) receiptButton.disabled = true;
    if (receiptInput) receiptInput.disabled = true;
    button.textContent = t("claim.instantWorking");
    try {
      const res = await fetch(PREMIUM_API_BASE + "/api/activate", {
        method: "POST",
        headers: { "Content-Type": "application/json", Authorization: "Bearer " + token },
        body: JSON.stringify({
          termsAccepted: true,
          termsVersion: ACTIVATION_TERMS_VERSION,
        }),
      });
      let body = {};
      try {
        body = await res.json();
      } catch {}
      if (res.status === 200 && body.ok === true) {
        renderActivationSuccess(body);
        return;
      }
      const error = body && typeof body.error === "string" ? body.error : "";
      if (res.status === 403 && error === "no_email_scope") {
        if (allowRelogin) login({ resumeActivation: true });
        else
          setClaimAction(t("claim.loginAgain"), t("panel.login"), () =>
            login({ resumeActivation: true }),
          );
        return;
      }
      if (res.status === 401) {
        try {
          sessionStorage.removeItem(TOK_KEY);
          sessionStorage.removeItem(NAV_USER_KEY);
        } catch {}
        setClaimAction(t("claim.loginAgain"), t("panel.login"), () =>
          login({ resumeActivation: true }),
        );
        return;
      }
      if (res.status === 404) {
        setClaimMessage(t("claim.activationNotFound"), "err");
        receiptInput?.focus();
        return;
      }
      if (res.status === 429) {
        setClaimMessage(t("claim.ratelimited"), "err");
        return;
      }
      if (res.status === 422 && error === "email_missing") {
        setClaimMessage(t("claim.emailMissing"), "err");
        return;
      }
      if (res.status === 422 && error === "email_unverified") {
        setClaimMessage(t("claim.emailUnverified"), "err");
        return;
      }
      if (res.status === 503) {
        setClaimMessage(t("claim.serviceUnavailable"), "err");
        return;
      }
      if (error === "bad_terms_version") {
        setClaimMessage(t("claim.resumeExpired"), "err");
        return;
      }
      setClaimMessage(t("claim.error"), "err");
    } catch {
      setClaimMessage(t("claim.serviceUnavailable"), "err");
    } finally {
      button.disabled = false;
      if (receiptButton) receiptButton.disabled = false;
      if (receiptInput) receiptInput.disabled = false;
      button.textContent = previous;
    }
  }

  // Submete o código da transação a POST {API_BASE}/api/link (autenticado com o token do OAuth).
  // 200 => ativado (recarrega o painel); 400 "use_receipt_code" => colaram um email em vez do
  // código (plano 021: o email já não é aceite como prova de posse); 404 => código não
  // encontrado; 429 => rate-limit; 401 => token expirou (re-login). Nunca expõe qual código é
  // válido (404 genérico do backend).
  async function doClaim(ev) {
    ev.preventDefault();
    const input = document.getElementById("ppClaimCode");
    const btn = document.getElementById("ppClaimBtn");
    const msg = document.getElementById("ppClaimMsg");
    const instantMessage = document.getElementById("ppInstantMsg");
    const consent = document.getElementById("ppClaimConsent");
    if (!input || !btn) return;
    if (instantMessage) instantMessage.hidden = true;
    const code = (input.value || "").trim();
    const setMsg = (text, kind) => {
      if (!msg) return;
      msg.hidden = false;
      msg.textContent = text;
      msg.className = "ppanel__claimmsg" + (kind ? " is-" + kind : "");
    };
    if (!code) {
      setMsg(t("claim.receiptRequired"), "err");
      input.focus();
      return;
    }
    // O Ref do recibo (`Ref: S-M1X823C9FW`) e a unica coisa com ar de codigo no email do Ko-fi, e
    // nunca casa com nada: o webhook do Ko-fi nao envia esse campo. Deixa-lo ir ao servidor da um
    // 404 que a pessoa nao consegue accionar — e ela pagou mesmo. Apanha-se aqui, ANTES do fetch,
    // e abre-se a ajuda — que agora pede o EMAIL (o Ref nao serve para ninguem procurar).
    if (REF_RE.test(code)) {
      setMsg(t("claim.help.refPasted"), "err");
      openClaimHelp();
      return;
    }
    // Sem consentimento nao se entrega. Se falhassemos ABERTO aqui, activavamos o passe sem a
    // pessoa ter reconhecido nada — e o direito de retratacao ficava de pe, que e exactamente o
    // que isto existe para evitar.
    if (!consent || !consent.checked) {
      setMsg(t("claim.consentRequired"), "err");
      return;
    }
    const tok = storedToken();
    if (!tok) {
      login();
      return;
    }
    const prev = btn.textContent;
    btn.disabled = true;
    input.disabled = true;
    btn.textContent = t("claim.working");
    try {
      const res = await fetch(PREMIUM_API_BASE + "/api/link", {
        method: "POST",
        headers: { "Content-Type": "application/json", Authorization: "Bearer " + tok },
        body: JSON.stringify({ code }),
      });
      if (res.ok) {
        setMsg(t("claim.ok"), "ok");
        input.value = "";
        window.setTimeout(loadPanel, 1200); // reflete o novo estado no painel
        return;
      }
      if (res.status === 401) {
        login();
        return;
      }
      if (res.status === 429) {
        setMsg(t("claim.ratelimited"), "err");
        return;
      }
      if (res.status === 404) {
        setMsg(t("claim.notfound"), "err");
        return;
      }
      let errCode = "";
      try {
        const errBody = await res.json();
        if (errBody && typeof errBody.error === "string") errCode = errBody.error;
      } catch {
        /* corpo sem JSON válido — cai no erro genérico abaixo */
      }
      setMsg(errCode === "use_receipt_code" ? t("claim.useReceiptCode") : t("claim.error"), "err");
    } catch {
      setMsg(t("claim.error"), "err");
    } finally {
      btn.disabled = false;
      input.disabled = false;
      btn.textContent = prev;
    }
  }

  let purchaseActivationOpener = null;

  // Move the purchase flow to <body>. The account card uses backdrop-filter, which would trap a
  // fixed descendant inside the card instead of letting it cover the viewport.
  function mountPurchaseActivation(el) {
    const modal = el.querySelector("#activate-purchase");
    if (!modal) return;
    document.body.appendChild(modal);
  }

  function openPurchaseActivation() {
    const modal = document.getElementById("activate-purchase");
    if (!modal) return;
    purchaseActivationOpener = document.activeElement;
    modal.hidden = false;
    document.body.classList.add("is-claim-open");
    document.getElementById("ppClaimClose")?.focus();
  }

  function closePurchaseActivation() {
    const modal = document.getElementById("activate-purchase");
    if (!modal || modal.hidden) return;
    modal.hidden = true;
    document.body.classList.remove("is-claim-open");
    if (
      purchaseActivationOpener &&
      typeof purchaseActivationOpener.focus === "function"
    ) {
      purchaseActivationOpener.focus();
    }
    purchaseActivationOpener = null;
  }

  let logoutConfirmOpener = null;

  function mountLogoutConfirm(el) {
    document.getElementById("ppLogoutConfirm")?.remove();
    if (!el.querySelector("#ppLogout")) return;
    document.body.insertAdjacentHTML("beforeend", logoutConfirmModal());
    document
      .getElementById("ppLogoutConfirmCancel")
      ?.addEventListener("click", closeLogoutConfirm);
    document
      .getElementById("ppLogoutConfirmBackdrop")
      ?.addEventListener("click", closeLogoutConfirm);
    document.getElementById("ppLogoutConfirmAction")?.addEventListener("click", logout);
  }

  function openLogoutConfirm() {
    const modal = document.getElementById("ppLogoutConfirm");
    if (!modal) return;
    logoutConfirmOpener = document.activeElement;
    modal.hidden = false;
    document.body.classList.add("is-modal-open");
    document.getElementById("ppLogoutConfirmCancel")?.focus();
  }

  function closeLogoutConfirm() {
    const modal = document.getElementById("ppLogoutConfirm");
    if (!modal || modal.hidden) return;
    modal.hidden = true;
    document.body.classList.remove("is-modal-open");
    if (logoutConfirmOpener && typeof logoutConfirmOpener.focus === "function") {
      logoutConfirmOpener.focus();
    }
    logoutConfirmOpener = null;
  }

  // Quem abriu o modal, para lhe devolver o foco ao fechar. Sem isto, quem navega por teclado
  // volta ao topo do documento e tem de refazer o caminho todo ate ao cartao.
  let claimHelpOpener = null;

  // O modal vive no <body>, NAO dentro do #premiumPanel que o desenha. Nao e preferencia: o
  // .premium-panel tem `transform` e `backdrop-filter`, e qualquer um deles faz do painel o
  // containing block dos descendentes `position: fixed`. La dentro, o modal ficava preso ao
  // painel — media 836px de largura no meio da pagina em vez de cobrir o ecra, e o blur so tapava
  // o proprio painel. Verificado no browser; nenhum teste de string apanha isto.
  //
  // renderPanel reescreve o innerHTML do painel a cada loadPanel, por isso o modal e re-montado
  // aqui a seguir: substitui-se o anterior para nao acumular copias no body, e os handlers
  // ligam-se por document.getElementById (o modal ja nao esta dentro de `el`).
  function mountClaimHelp(el) {
    document.getElementById("ppClaimHelp")?.remove();
    if (!el.querySelector("#ppClaimHelpOpen")) return; // sem cartao de activacao, sem modal
    document.body.insertAdjacentHTML("beforeend", claimHelpModal());
    document.getElementById("ppClaimHelpClose")?.addEventListener("click", closeClaimHelp);
    document.getElementById("ppClaimHelpBackdrop")?.addEventListener("click", closeClaimHelp);
    document.getElementById("ppClaimHelpForm")?.addEventListener("submit", sendClaimHelp);
  }

  function openClaimHelp() {
    const modal = document.getElementById("ppClaimHelp");
    if (!modal) return;
    claimHelpOpener = document.activeElement;
    modal.hidden = false;
    document.body.classList.add("is-modal-open");
    // O foco entra na caixa (nao no input): quem usa leitor de ecra ouve o titulo e os dois
    // passos antes do campo, que e a ordem que explica o campo. Nao se pre-preenche nada — o campo
    // e o EMAIL do Ko-fi, e o que a pessoa possa ter colado antes (um Ref) nao e um email.
    document.getElementById("ppClaimHelpBox")?.focus();
  }

  function closeClaimHelp() {
    const modal = document.getElementById("ppClaimHelp");
    if (!modal || modal.hidden) return;
    modal.hidden = true;
    document.body.classList.remove("is-modal-open");
    if (claimHelpOpener && typeof claimHelpOpener.focus === "function") claimHelpOpener.focus();
    claimHelpOpener = null;
  }

  // Esc fecha. Registado UMA vez no documento (nao por render) — o painel volta a desenhar-se a
  // cada loadPanel e um listener por render acumulava-se em silencio.
  document.addEventListener("keydown", (ev) => {
    const billingModal = document.getElementById("vozenBillingModal");
    if (billingModal && !billingModal.hidden && ev.key === "Tab") {
      const focusable = [...billingModal.querySelectorAll("button, a[href], input, select, textarea, [tabindex]:not([tabindex='-1'])")]
        .filter((element) => !element.disabled && element.getClientRects().length > 0);
      if (focusable.length) {
        const first = focusable[0];
        const last = focusable[focusable.length - 1];
        if (ev.shiftKey && document.activeElement === first) {
          ev.preventDefault();
          last.focus();
        } else if (!ev.shiftKey && document.activeElement === last) {
          ev.preventDefault();
          first.focus();
        }
      }
      return;
    }
    if (ev.key !== "Escape") return;
    const logoutModal = document.getElementById("ppLogoutConfirm");
    if (logoutModal && !logoutModal.hidden) {
      closeLogoutConfirm();
      return;
    }
    const help = document.getElementById("ppClaimHelp");
    if (help && !help.hidden) {
      closeClaimHelp();
      return;
    }
    if (document.getElementById("vozenBillingModal")) {
      closeBillingCheckout();
      return;
    }
    closePurchaseActivation();
  });

  // Um UUID em qualquer forma (codigo solto, ou dentro de um link de recibo). Se aparecer no campo
  // de email, a pessoa colou o codigo/link no sitio errado — e ativa-se na mesma pela caixa
  // principal, sem ela ter de perceber a diferenca.
  const UUID_RE = /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i;

  // Quando a notificacao nao sai (API em baixo, webhook por configurar), a pessoa nao pode ficar
  // sem saida: mostra-se a mensagem ja pronta para ela colar no suporte, com o email la dentro. E
  // o mesmo trabalho, feito a mao — mas ela sai daqui com alguma coisa.
  function showClaimHelpFallback(setMsg, email) {
    setMsg(t("claim.help.sendError"), "err");
    const box = document.getElementById("ppClaimHelpMsg");
    if (!box) return;
    const pre = document.createElement("pre");
    pre.className = "ppmodal__copy";
    pre.textContent = `Vozen: I can't activate my purchase. Ko-fi email: ${email}`;
    box.appendChild(pre);
  }

  // Envia (Discord ID via token, email do Ko-fi) para o dono activar a mao. O email e pista de
  // procura, NAO prova (ver claimHelpModal). Se o endpoint falhar, mostra uma mensagem pronta a
  // copiar: a pessoa nunca fica sem saida por a nossa API estar em baixo.
  async function sendClaimHelp(ev) {
    ev.preventDefault();
    const input = document.getElementById("ppClaimHelpEmail");
    const btn = document.getElementById("ppClaimHelpSend");
    const msg = document.getElementById("ppClaimHelpMsg");
    if (!input || !btn) return;
    const value = (input.value || "").trim();
    const setMsg = (text, kind) => {
      if (!msg) return;
      msg.hidden = false;
      msg.textContent = text;
      msg.className = "ppanel__claimmsg" + (kind ? " is-" + kind : "");
    };
    if (!value) return;
    // Colou aqui o codigo verdadeiro (ou o link do recibo)? Manda-se de volta para a caixa
    // principal e ativa-se — a pessoa nao tem de saber a diferenca.
    if (UUID_RE.test(value)) {
      const main = document.getElementById("ppClaimCode");
      if (main) {
        main.value = value;
        closeClaimHelp();
        main.focus();
        return;
      }
    }
    // Nao e um email? Diz-lho antes de enviar — o dono nao pode ficar com um ping inutil.
    if (!/^[^@\s]+@[^@\s]+$/.test(value)) {
      setMsg(t("claim.help.notEmail"), "err");
      return;
    }
    const tok = storedToken();
    if (!tok) {
      login();
      return;
    }
    const prev = btn.textContent;
    btn.disabled = true;
    input.disabled = true;
    btn.textContent = t("claim.working");
    try {
      const res = await fetch(PREMIUM_API_BASE + "/api/claim-help", {
        method: "POST",
        headers: { "Content-Type": "application/json", Authorization: "Bearer " + tok },
        body: JSON.stringify({ email: value }),
      });
      if (res.ok) {
        setMsg(t("claim.help.sent"), "ok");
        return;
      }
      if (res.status === 401) {
        login();
        return;
      }
      if (res.status === 429) {
        setMsg(t("claim.ratelimited"), "err");
        return;
      }
      showClaimHelpFallback(setMsg, value);
    } catch {
      showClaimHelpFallback(setMsg, value);
    } finally {
      btn.disabled = false;
      input.disabled = false;
      btn.textContent = prev;
    }
  }

  function renderPanel() {
    const el = document.getElementById("premiumPanel");
    if (!el) return;
    const previousPurchaseModal = document.getElementById("activate-purchase");
    if (previousPurchaseModal && !el.contains(previousPurchaseModal)) {
      previousPurchaseModal.remove();
      document.body.classList.remove("is-claim-open");
      purchaseActivationOpener = null;
    }
    if (panelState.mode === "hidden") {
      el.hidden = true;
      el.innerHTML = "";
      renderNavLogin(navDataForState());
      return;
    }
    el.hidden = false;
    if (IS_ACCOUNT && panelState.mode !== "anon") unlockAccountPage();
    const head = `<div class="ppanel__head"><span class="ppanel__title">💎 ${t("account.membershipStatus")}</span></div>`;
    let body = "";
    if (panelState.mode === "soon") {
      // Estado dormente da página da conta: backend ainda não configurado.
      body = `<p class="ppanel__meta">${t("panel.soon")}</p>`;
    } else if (panelState.mode === "anon") {
      body = `<div class="ppanel__anon"><button type="button" class="btn--discord" id="ppLogin">${DISCORD_MARK}<span>${t("panel.login")}</span></button><p class="ppanel__meta">${t("panel.noneSub")}</p></div>`;
    } else if (panelState.mode === "loading") {
      body = `<div class="ppanel__loading" aria-label="${esc(t("panel.loading"))}"><span class="ppanel__skel ppanel__skel--lg"></span><span class="ppanel__skel"></span><span class="ppanel__skel ppanel__skel--sm"></span></div>`;
    } else if (panelState.mode === "error") {
      body = `<div class="ppanel__error"><span class="ppanel__error-ic" aria-hidden="true">!</span><p class="ppanel__meta">${t("panel.error")}</p><button type="button" class="btn--ghost" id="ppRetry">${t("panel.retry")}</button></div>`;
    } else if (panelState.mode === "ok") {
      body = renderOk(panelState.data || {});
    }
    el.innerHTML = head + body;
    renderNavLogin(navDataForState());
    const byId = (id) => el.querySelector("#" + id);
    byId("ppLogin")?.addEventListener("click", login);
    byId("ppLogout")?.addEventListener("click", openLogoutConfirm);
    byId("ppRetry")?.addEventListener("click", loadPanel);
    byId("ppActivateBtn")?.addEventListener("click", doInstantActivation);
    byId("ppClaimForm")?.addEventListener("submit", doClaim);
    byId("ppClaimHelpOpen")?.addEventListener("click", () => openClaimHelp(""));
    byId("ppClaimClose")?.addEventListener("click", closePurchaseActivation);
    byId("activate-purchase")?.addEventListener("click", (ev) => {
      if (ev.target === ev.currentTarget) closePurchaseActivation();
    });
    mountClaimHelp(el);
    mountPurchaseActivation(el);
    mountLogoutConfirm(el);
    el.querySelector(".ppanel__id")?.addEventListener("click", async (ev) => {
      const btn = ev.currentTarget;
      const id = btn.dataset.id || "";
      if (!id || !navigator.clipboard) return;
      const tip = btn.querySelector(".ppanel__tip");
      try {
        await navigator.clipboard.writeText(id);
        btn.classList.add("is-copied");
        if (tip) tip.textContent = t("account.copied");
        window.setTimeout(() => {
          btn.classList.remove("is-copied");
          if (tip) tip.textContent = t("account.copyHint");
        }, 1500);
      } catch {}
    });
  }

  document
    .getElementById("accountActivateOpen")
    ?.addEventListener("click", openPurchaseActivation);

  const planDialog = document.getElementById("planChangeDialog");
  if (planDialog) {
    const node = (id) => document.getElementById("planChange" + id);
    const copy = (key) => {
      const en = {
        title: "Change plan", policy: "Upgrades require payment. Downgrades and yearly-to-monthly changes start when your current paid period ends.",
        subtitle: "Preview an upgrade or schedule a downgrade",
        subscription: "Subscription", plan: "New plan", interval: "Billing interval", monthly: "Monthly", yearly: "Yearly",
        preview: "Preview change", close: "Close", loading: "Loading your subscriptions…",
        empty: "No eligible active Stripe subscription. You can still manage billing through the Stripe portal.",
        error: "The change could not be confirmed. Open billing management and check for a pending payment before trying again.",
        choose: "Choose a different plan or billing interval.", pay: "Confirm and pay", schedule: "Schedule change",
        next: "New recurring price", due: "Due now", effective: "Starts on", pending: "Stripe is confirming your payment. Access updates after payment succeeds.",
        scheduled: "Change scheduled. Your current paid access remains until", seats: "On a smaller server plan, the oldest linked servers keep access up to the new limit. Other server links stay saved.",
        personal: "Plus covers your account, not server licenses. Stripe server access ends on that date; independent benefits remain.",
        pendingChange: "A change is already scheduled. Remove it to choose another plan or cancel renewal through billing management. Your current subscription stays active.",
        remove: "Remove scheduled change", removed: "Scheduled change removed. Your current subscription is unchanged. Open billing management if you want to cancel renewal.",
      };
      const pt = {
        title: "Trocar de plano", policy: "Os upgrades exigem pagamento. Os downgrades e as trocas de anual para mensal começam no fim do período já pago.",
        subtitle: "Pré-visualiza um upgrade ou agenda um downgrade",
        subscription: "Subscrição", plan: "Novo plano", interval: "Periodicidade", monthly: "Mensal", yearly: "Anual",
        preview: "Pré-visualizar troca", close: "Fechar", loading: "A carregar as tuas subscrições…",
        empty: "Não há uma subscrição Stripe ativa elegível. Podes gerir a faturação no portal Stripe.",
        error: "Não foi possível confirmar a troca. Abre a gestão de faturação e verifica se existe um pagamento pendente antes de tentar novamente.",
        choose: "Escolhe outro plano ou outra periodicidade.", pay: "Confirmar e pagar", schedule: "Agendar troca",
        next: "Novo preço recorrente", due: "A pagar agora", effective: "Começa em", pending: "A Stripe está a confirmar o pagamento. O acesso muda após o pagamento ter sucesso.",
        scheduled: "Troca agendada. Manténs o acesso já pago até", seats: "Num plano com menos servidores, os servidores associados há mais tempo mantêm o acesso até ao novo limite. As restantes associações ficam guardadas.",
        personal: "O Plus cobre a tua conta, não licenças de servidor. O acesso Stripe dos servidores termina nessa data; os benefícios independentes mantêm-se.",
        pendingChange: "Já existe uma troca agendada. Remove-a para escolher outro plano ou cancelar a renovação na gestão de faturação. A tua subscrição atual mantém-se ativa.",
        remove: "Remover troca agendada", removed: "Troca agendada removida. A tua subscrição atual não mudou. Abre a gestão de faturação se quiseres cancelar a renovação.",
      };
      return (lang.startsWith("pt") ? pt : en)[key];
    };
    let quote = null;
    let busy = false;
    let subscriptions = [];
    let pendingToken = null;
    const status = (message) => { node("Status").textContent = message; };
    const resetQuote = () => {
      quote = null;
      node("Preview").hidden = false;
      node("Confirm").hidden = true;
      node("Confirm").disabled = true;
      node("CancelScheduled").hidden = !pendingToken;
    };
    const setBusy = (value) => {
      busy = value;
      ["Subscription", "Plan", "Interval", "Preview", "Close"].forEach((id) => {
        node(id).disabled = value || (["Subscription", "Plan", "Interval"].includes(id) && !subscriptions.length);
      });
      node("Preview").disabled = value || !subscriptions.length;
      ["Plan", "Interval", "Preview"].forEach((id) => { node(id).disabled ||= !!pendingToken; });
      node("CancelScheduled").disabled = value || !pendingToken;
      node("Confirm").disabled = value || !quote;
    };
    async function planApi(route, body = {}) {
      const token = storedToken();
      if (!token) throw new Error("login required");
      const response = await fetch(PREMIUM_API_BASE + "/api/billing/" + route, {
        method: "POST", headers: { Authorization: "Bearer " + token, "Content-Type": "application/json" },
        body: JSON.stringify(body), signal: AbortSignal.timeout(45000),
      });
      const payload = await response.json().catch(() => ({}));
      if (!response.ok) throw new Error("plan change unavailable");
      return payload;
    }
    function selectCurrentSubscription() {
      pendingToken = null;
      resetQuote();
      const current = subscriptions.find((item) => item.id === node("Subscription").value);
      if (current) { node("Plan").value = current.plan; node("Interval").value = current.interval; }
      pendingToken = current?.cancelToken || null;
      node("CancelScheduled").hidden = !pendingToken;
      node("Preview").hidden = !!pendingToken;
      status(copy(pendingToken ? "pendingChange" : "choose"));
      setBusy(false);
    }
    const trigger = document.getElementById("accountPlanChange");
    trigger.querySelector("b").textContent = copy("title");
    trigger.querySelector("small").textContent = copy("subtitle");
    window.addEventListener("vozen:languagechange", () => {
      trigger.querySelector("b").textContent = copy("title");
      trigger.querySelector("small").textContent = copy("subtitle");
    });
    trigger.addEventListener("click", async () => {
      if (!storedToken()) return login();
      if (planDialog.open || busy) return;
      node("Title").textContent = copy("title");
      node("Policy").textContent = copy("policy");
      node("Close").textContent = copy("close");
      node("CancelScheduled").textContent = copy("remove");
      node("Preview").textContent = copy("preview");
      ["Subscription", "Plan", "Interval"].forEach((id) => {
        document.querySelector(`label[for="planChange${id}"]`).textContent = copy(id.toLowerCase());
      });
      [...node("Interval").options].forEach((option) => { option.textContent = copy(option.value); });
      subscriptions = [];
      pendingToken = null;
      node("Subscription").replaceChildren();
      resetQuote(); setBusy(true); status(copy("loading")); planDialog.showModal();
      try {
        const payload = await planApi("plans");
        subscriptions = (payload.subscriptions || []).filter((item) => item.changeable || item.cancelToken);
        subscriptions.forEach((item, index) => {
          const option = document.createElement("option");
          option.value = item.id;
          option.textContent = `${item.plan.toUpperCase()} / ${copy(item.interval)}${subscriptions.length > 1 ? ` (${index + 1})` : ""}`;
          node("Subscription").append(option);
        });
        if (subscriptions.length) selectCurrentSubscription(); else status(copy("empty"));
      } catch { status(copy("error")); }
      finally { setBusy(false); }
    });
    node("Close").addEventListener("click", () => { if (!busy) planDialog.close(); });
    planDialog.addEventListener("cancel", (event) => { if (busy) event.preventDefault(); });
    planDialog.addEventListener("close", resetQuote);
    node("Subscription").addEventListener("change", selectCurrentSubscription);
    ["Plan", "Interval"].forEach((id) => node(id).addEventListener("change", () => { resetQuote(); status(copy("choose")); }));
    node("Form").addEventListener("submit", async (event) => {
      event.preventDefault();
      if (busy || !subscriptions.length || pendingToken) return;
      resetQuote(); setBusy(true);
      try {
        quote = await planApi("change/preview", { subscription_id: node("Subscription").value, plan: node("Plan").value, interval: node("Interval").value });
        const money = (cents) => new Intl.NumberFormat(lang, { style: "currency", currency: quote.currency }).format(cents / 100);
        status(`${quote.plan.toUpperCase()}: ${copy("next")} ${money(quote.recurringAmount)} / ${copy(quote.interval)}.\n${quote.deferred ? `${copy("effective")} ${fmtDate(quote.effectiveAt * 1000)}.\n${copy(quote.plan === "plus" ? "personal" : "seats")}` : `${copy("due")} ${money(quote.amountDue)}.`}`);
        node("Confirm").textContent = quote.deferred ? copy("schedule") : `${copy("pay")} ${money(quote.amountDue)}`;
        node("Confirm").hidden = false;
        node("Preview").hidden = true;
      } catch { resetQuote(); status(copy("error")); }
      finally { setBusy(false); }
    });
    node("Confirm").addEventListener("click", async () => {
      if (busy || !quote) return;
      const token = quote.token;
      resetQuote(); setBusy(true);
      try {
        const result = await planApi("change/confirm", { token });
        if (result.paymentUrl) {
          const destination = new URL(result.paymentUrl);
          if (destination.protocol !== "https:" || destination.hostname !== "invoice.stripe.com") throw new Error("unexpected payment URL");
          window.location.assign(destination.href);
        }
        status(result.status === "scheduled" ? `${copy("scheduled")} ${fmtDate(result.effectiveAt * 1000)}.` : copy("pending"));
        subscriptions = []; // Require a fresh read before another change.
        void loadPanel();
      } catch { status(copy("error")); subscriptions = []; }
      finally { setBusy(false); }
    });
    node("CancelScheduled").addEventListener("click", async () => {
      if (busy || !pendingToken) return;
      const token = pendingToken;
      setBusy(true);
      try {
        await planApi("change/cancel", { token });
        status(copy("removed"));
      } catch { status(copy("error")); }
      finally {
        pendingToken = null;
        subscriptions = [];
        resetQuote();
        setBusy(false);
      }
    });
  }

  document.getElementById("accountBilling")?.addEventListener("click", async (event) => {
    const button = event.currentTarget;
    const token = storedToken();
    if (!token) return login();
    button.disabled = true;
    try {
      const response = await fetch(PREMIUM_API_BASE + "/api/billing/portal", {
        method: "POST", headers: { Authorization: "Bearer " + token },
      });
      const payload = await response.json().catch(() => ({}));
      if (!response.ok || !payload.url) throw new Error("portal unavailable");
      window.location.href = payload.url;
    } catch {
      button.disabled = false;
      alert("Billing management is temporarily unavailable. Please try again shortly.");
    }
  });

  // Botão de login na navbar: leva à página dedicada da conta (account.html). Já na
  // página da conta, faz login OAuth quando o backend está no ar; senão faz scroll ao
  // painel (que mostra "em breve") — nunca dispara um redirect que ainda não existe.
  function scrollPremiumPanel() {
    document
      .getElementById("premiumPanel")
      ?.scrollIntoView({ behavior: reduce ? "auto" : "smooth" });
  }

  // Keep the URL clean of the #fragment that in-page navigation leaves behind.
  // A native <a href="#features"> click scrolls correctly through the browser
  // (respecting the page's scroll container and scroll-behavior) but leaves
  // "/#features" in the address bar; once the browser has applied the fragment
  // (hashchange) we strip it with replaceState, keeping the scroll position.
  // The OAuth return fragment (#access_token=…) is left for readTokenFromHash.
  window.addEventListener("hashchange", () => {
    const id = location.hash.slice(1);
    if (!id || id.indexOf("=") !== -1) return;
    if (document.getElementById(id)) {
      history.replaceState(null, "", location.pathname + location.search);
    }
  });

  const navLoginBtn = document.getElementById("navLogin");
  if (navLoginBtn) {
    navLoginBtn.addEventListener("click", () => {
      if (!IS_ACCOUNT) {
        if (storedToken() || !PREMIUM_API_BASE) window.location.href = "/account";
        else login();
        return;
      }
      if (panelState.mode === "ok") {
        scrollPremiumPanel();
        return;
      }
      if (PREMIUM_API_BASE) login();
      else scrollPremiumPanel();
    });
  }

  /* ── navbar ──────────────────────────────────────────── */
  const nav = $("#nav");
  const onScroll = () => nav.classList.toggle("is-stuck", window.scrollY > 12);
  onScroll();
  window.addEventListener("scroll", onScroll, { passive: true });

  const burger = $("#burger");
  const links = $(".nav__links");
  function closeMobileNav(restoreFocus) {
    links.classList.remove("is-open");
    burger.setAttribute("aria-expanded", "false");
    if (restoreFocus) burger.focus();
  }
  burger.addEventListener("click", () => {
    const open = links.classList.toggle("is-open");
    burger.setAttribute("aria-expanded", String(open));
  });
  $$(".nav__links a").forEach((a) =>
    a.addEventListener("click", () => closeMobileNav(false)),
  );
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && links.classList.contains("is-open")) closeMobileNav(true);
  });

  /* ── reveal on scroll ────────────────────────────────── */
  const io = new IntersectionObserver(
    (entries) => {
      entries.forEach((e) => {
        if (e.isIntersecting) {
          e.target.classList.add("in-view");
          io.unobserve(e.target);
        }
      });
    },
    { threshold: 0.15 },
  );
  $$(".reveal:not([data-r])").forEach((el) => io.observe(el));

  /* ── animated counters ───────────────────────────────── */
  const counters = $$(".count");
  const cio = new IntersectionObserver(
    (entries) => {
      entries.forEach((e) => {
        if (!e.isIntersecting) return;
        cio.unobserve(e.target);
        const el = e.target;
        const to = +el.dataset.to;
        if (reduce) {
          el.textContent = to;
          return;
        }
        const dur = 1100;
        const start = performance.now();
        const tick = (now) => {
          const p = Math.min(1, (now - start) / dur);
          el.textContent = Math.round(to * (1 - Math.pow(1 - p, 3)));
          if (p < 1) requestAnimationFrame(tick);
        };
        requestAnimationFrame(tick);
      });
    },
    { threshold: 0.5 },
  );
  counters.forEach((c) => cio.observe(c));

  /* ── language marquee ────────────────────────────────── */
  // Vitrina de línguas (marquee): cada uma NA SUA PRÓPRIA LÍNGUA (autónimo). ~35, a par
  // do "35 languages" anunciado no site. É só showcase — não é a lista funcional do bot.
  const LANGS = [
    ["🇬🇧", "English"], ["🇵🇹", "Português"], ["🇪🇸", "Español"], ["🇫🇷", "Français"],
    ["🇩🇪", "Deutsch"], ["🇮🇹", "Italiano"], ["🇳🇱", "Nederlands"], ["🇵🇱", "Polski"],
    ["🇷🇺", "Русский"], ["🇺🇦", "Українська"], ["🇹🇷", "Türkçe"], ["🇸🇪", "Svenska"],
    ["🇯🇵", "日本語"], ["🇰🇷", "한국어"], ["🇨🇳", "中文"], ["🇸🇦", "العربية"],
    ["🇬🇷", "Ελληνικά"], ["🇨🇿", "Čeština"], ["🇩🇰", "Dansk"], ["🇫🇮", "Suomi"],
    ["🇷🇴", "Română"], ["🇭🇺", "Magyar"], ["🇻🇳", "Tiếng Việt"], ["🇮🇸", "Íslenska"],
    ["🇬🇪", "ქართული"], ["🇮🇷", "فارسی"], ["🇮🇳", "हिन्दी"], ["🇹🇭", "ไทย"],
    ["🇮🇩", "Bahasa Indonesia"], ["🇷🇸", "Српски"], ["🇸🇰", "Slovenčina"], ["🇸🇮", "Slovenščina"],
    ["🇱🇻", "Latviešu"], ["🇰🇿", "Қазақ тілі"], ["🇧🇬", "Български"],
  ];
  const track = $("#marqueeTrack");
  if (track) {
    const chip = ([f, n]) => `<span class="chip">${f} ${n}</span>`;
    // duplicate the set so the -50% loop is seamless
    track.innerHTML = LANGS.map(chip).join("") + LANGS.map(chip).join("");
  }

  /* ── wordle mock ─────────────────────────────────────── */
  // Solve to VOZEN: VOICE (V,O green) → TOKEN (O,E,N green) → VOZEN (all green).
  const WORD = [
    [["V", "g"], ["O", "g"], ["I", "x"], ["C", "x"], ["E", "y"]],
    [["T", "x"], ["O", "g"], ["K", "x"], ["E", "g"], ["N", "g"]],
    [["V", "g"], ["O", "g"], ["Z", "g"], ["E", "g"], ["N", "g"]],
  ];
  const wordle = $("#wordle");
  if (wordle) {
    wordle.innerHTML = WORD.map(
      (row) =>
        `<div class="wrow">${row
          .map(([ch, s]) => `<span class="wtile ${s}">${ch}</span>`)
          .join("")}</div>`,
    ).join("");
  }

  /* ── commands ────────────────────────────────────────── */
  const cmdList = $("#cmdList");
  let activeTab = "general";
  function renderCommands() {
    if (!cmdList) return;
    const rows = window.VOZEN_COMMANDS[activeTab] || [];
    cmdList.innerHTML = rows
      .map(([cmd, desc]) => `<div class="cmd"><code>${cmd}</code><span>${desc[lang] || desc.en}</span></div>`)
      .join("");
  }
  $$("#cmdTabs button").forEach((b) =>
    b.addEventListener("click", () => {
      activeTab = b.dataset.tab;
      $$("#cmdTabs button").forEach((x) => x.classList.toggle("is-active", x === b));
      renderCommands();
    }),
  );

  /* ── faq ─────────────────────────────────────────────── */
  const faqList = $("#faq-list");
  function renderFaq() {
    if (!faqList) return;
    faqList.innerHTML = window.VOZEN_FAQ.map(
      ([q, a], i) => `
      <div class="qa">
        <button class="qa__q" aria-expanded="false" aria-controls="qa-${i}">
          <span>${q[lang] || q.en}</span>
        </button>
        <div class="qa__a" id="qa-${i}" role="region"><p>${a[lang] || a.en}</p></div>
      </div>`,
    ).join("");
    $$(".qa__q", faqList).forEach((btn) =>
      btn.addEventListener("click", () => {
        const qa = btn.parentElement;
        const panel = qa.querySelector(".qa__a");
        const open = qa.classList.toggle("is-open");
        btn.setAttribute("aria-expanded", String(open));
        panel.style.maxHeight = open ? panel.scrollHeight + "px" : "0";
      }),
    );
  }

  /* ── animated Discord chat mock ──────────────────────── */
  const chat = $("#chat");
  const SCRIPT = [
    { name: "Kai", av: "K", cls: "u", text: "what's the plan tonight?", say: "EN" },
    { name: "Ana", av: "A", cls: "u", text: "boa noite pessoal 🌙", say: "PT" },
    { name: "Léa", av: "L", cls: "u", text: "on lance une partie ? 🎮", say: "FR" },
  ];

  function bubble(m, textHtml, speaking) {
    const el = document.createElement("div");
    el.className = "msg";
    el.innerHTML = `
      <div class="msg__av msg__av--${m.cls}">${m.av}</div>
      <div class="msg__b">
        <div class="msg__name">${m.name}</div>
        <div class="msg__text">${textHtml}</div>
        ${speaking ? `<div class="speaking"><span class="eq"><i></i><i></i><i></i><i></i><i></i></span> Vozen is speaking…</div>` : ""}
      </div>`;
    return el;
  }

  function vozenBubble() {
    const el = document.createElement("div");
    el.className = "msg";
    el.innerHTML = `
      <div class="msg__av msg__av--v"><span class="eq" style="height:16px"><i></i><i></i><i></i><i></i><i></i></span></div>
      <div class="msg__b">
        <div class="msg__name"><b>Vozen</b><span class="tag">APP</span></div>
        <div class="speaking"><span class="eq"><i></i><i></i><i></i><i></i><i></i></span> reading it out loud…</div>
      </div>`;
    return el;
  }

  async function typeInto(node, text) {
    const t = node.querySelector(".msg__text");
    for (let i = 0; i <= text.length; i++) {
      t.innerHTML = text.slice(0, i) + '<span class="cursor"></span>';
      await sleep(38 + Math.random() * 40);
    }
    t.textContent = text;
  }

  async function runChat() {
    if (!chat) return;
    if (reduce) {
      // static: show one exchange, no loop
      chat.appendChild(bubble(SCRIPT[0], SCRIPT[0].text, false));
      chat.appendChild(vozenBubble());
      return;
    }
    let i = 0;
    for (;;) {
      const m = SCRIPT[i % SCRIPT.length];
      chat.innerHTML = "";
      const b = bubble(m, '<span class="cursor"></span>', false);
      chat.appendChild(b);
      await sleep(300);
      await typeInto(b, m.text);
      await sleep(450);
      chat.appendChild(vozenBubble());
      await sleep(2600);
      i++;
    }
  }

  /* ── hero waveform (signature: sound made visible) ───── */
  const wave = $("#wave");
  if (wave && !reduce) {
    const ctx = wave.getContext("2d");
    let W = 0, H = 0;
    const resize = () => {
      const dpr = Math.min(window.devicePixelRatio || 1, 2);
      W = wave.clientWidth;
      H = wave.clientHeight;
      wave.width = Math.round(W * dpr);
      wave.height = Math.round(H * dpr);
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    };
    resize();
    window.addEventListener("resize", resize, { passive: true });

    // three neon layers, each a speech-like burst tapered at the edges
    const layers = [
      { amp: 0.52, freq: 1.5, speed: 0.6, col: "rgba(102,114,255,0.85)", lw: 2.6 },
      { amp: 0.36, freq: 2.4, speed: -0.95, col: "rgba(46,230,200,0.8)", lw: 2.1 },
      { amp: 0.22, freq: 3.7, speed: 1.35, col: "rgba(154,164,255,0.55)", lw: 1.5 },
    ];
    let t = 0;
    const draw = () => {
      ctx.clearRect(0, 0, W, H);
      const mid = H / 2;
      for (const L of layers) {
        ctx.beginPath();
        ctx.lineWidth = L.lw;
        ctx.strokeStyle = L.col;
        ctx.shadowBlur = 10;
        ctx.shadowColor = L.col;
        for (let x = 0; x <= W; x += 5) {
          const p = x / W;
          const env = Math.sin(p * Math.PI); // taper to 0 at both ends
          const y =
            mid +
            (Math.sin(p * Math.PI * 2 * L.freq + t * L.speed) * (H * L.amp * 0.5) +
              Math.sin(p * Math.PI * L.freq + t * L.speed * 1.7) * (H * L.amp * 0.22)) *
              env;
          x === 0 ? ctx.moveTo(x, y) : ctx.lineTo(x, y);
        }
        ctx.stroke();
      }
      ctx.shadowBlur = 0;
      t += 0.028;
      requestAnimationFrame(draw);
    };
    draw();
  }

  /* ── hear (audio demo) ─────────────────────────────────── */
  // Cada amostra: bandeira, nome da lingua e a frase EXATA que foi sintetizada
  // (tem de bater certo com o audio) + o mp3 gerado por tools/gen-samples.mjs.
  const SAMPLES = {
    // SÓ línguas audíveis nos 3 motores (Google + Piper + Kokoro) — cada uma tem clipe
    // em todos, por isso NENHUM chip fica desativado. de saiu (sem voz Kokoro) e ja saiu
    // (Kokoro partido + sem Piper). Ver plano 018. Ficheiros: <lang>.mp3 (Google),
    // <lang>-piper.mp3 (Piper), <lang>-kokoro.mp3 (Kokoro).
    en: { flag: "🇬🇧", lang: "English", phrase: "Hey! Welcome to the server. Type anything and I'll read it out loud.", engines: ["google", "piper", "kokoro"] },
    pt: { flag: "🇵🇹", lang: "Português", phrase: "Olá! Escreva qualquer coisa e eu leio em voz alta.", engines: ["google", "piper", "kokoro"] },
    es: { flag: "🇪🇸", lang: "Español", phrase: "¡Hola! Escribe lo que quieras y lo leeré en voz alta.", engines: ["google", "piper", "kokoro"] },
    fr: { flag: "🇫🇷", lang: "Français", phrase: "Salut ! Écris ce que tu veux, je le lis à voix haute.", engines: ["google", "piper", "kokoro"] },
    it: { flag: "🇮🇹", lang: "Italiano", phrase: "Ciao! Scrivi quello che vuoi e lo leggerò ad alta voce.", engines: ["google", "piper", "kokoro"] },
  };

  function initHear() {
    const stage = $("#hearStage");
    const audio = $("#hearAudio");
    const btn = $("#hearBtn");
    if (!stage || !audio || !btn) return;
    const flagEl = $("#hearFlag"),
      langEl = $("#hearLang"),
      phraseEl = $("#hearPhrase"),
      engTag = $("#hearEngTag");
    const ENGINE_NAME = { google: "Google", piper: "Piper", kokoro: "Kokoro" };
    let current = "en";
    let engine = "google";

    const ENGINE_SUFFIX = { google: "", piper: "-piper", kokoro: "-kokoro" };
    const srcFor = (code, eng) => "assets/samples/" + code + (ENGINE_SUFFIX[eng] || "") + ".mp3";

    const select = (code, autoplay) => {
      const s = SAMPLES[code];
      if (!s) return;
      // Se a lingua nao tem o motor escolhido (ex.: japones sem Piper), toca no Google.
      const eng = s.engines.includes(engine) ? engine : "google";
      current = code;
      flagEl.textContent = s.flag;
      langEl.textContent = hearLangName(code, lang); // nome na língua do site (segue EN/PT)
      phraseEl.textContent = s.phrase;
      engTag.textContent = ENGINE_NAME[eng];
      $$(".hear__chip").forEach((c) => c.classList.toggle("is-active", c.dataset.sample === code));
      audio.src = srcFor(code, eng); // preload="none" => so descarrega ao dar play
      if (autoplay) audio.play().catch(() => {});
    };

    // Marca como indisponiveis os chips das linguas sem o motor escolhido.
    const refreshChips = () =>
      $$(".hear__chip").forEach((c) => {
        const s = SAMPLES[c.dataset.sample];
        c.classList.toggle("is-disabled", !s || !s.engines.includes(engine));
      });

    // Troca de motor: reavalia os chips e recarrega a lingua atual nesse motor (ou cai
    // numa que o tenha — ex.: japones + Piper -> ingles). Mantem o estado tocar/pausa.
    $$(".hear__eng").forEach((b) =>
      b.addEventListener("click", () => {
        engine = b.dataset.engine;
        $$(".hear__eng").forEach((x) => x.classList.toggle("is-active", x === b));
        refreshChips();
        const playing = !audio.paused;
        select(SAMPLES[current].engines.includes(engine) ? current : "en", playing);
      }),
    );

    // Toca/pausa. is-playing (icone + equalizador) segue os eventos do <audio>, por
    // isso trocar de lingua/motor a meio nunca deixa dois a tocar.
    btn.addEventListener("click", () => (audio.paused ? audio.play().catch(() => {}) : audio.pause()));
    $$(".hear__chip").forEach((chip) =>
      chip.addEventListener("click", () => select(chip.dataset.sample, true)),
    );
    audio.addEventListener("play", () => {
      stage.classList.add("is-playing");
      btn.setAttribute("aria-pressed", "true");
    });
    const off = () => {
      stage.classList.remove("is-playing");
      btn.setAttribute("aria-pressed", "false");
    };
    audio.addEventListener("pause", off);
    audio.addEventListener("ended", off);

    refreshChips();
    select("en", false); // estado inicial: EN + Google, sem tocar
  }

  /* ── boot ────────────────────────────────────────────── */
  applyLang(lang);
  runChat();
  initHear();
  loadPanel();
})();

