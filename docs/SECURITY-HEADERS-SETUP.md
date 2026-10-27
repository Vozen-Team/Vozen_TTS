# Cabeçalhos de segurança do example.com (securityheaders.com F → A/A+)

> Historical guide: configuration examples use fictional domains and a documentation IP. Replace them before use; the dated audit below remains a historical record.

**Estado:** F em `securityheaders.com` porque o site é servido pelo **GitHub Pages**, que
**não permite cabeçalhos HTTP personalizados** (e ignora ficheiros `_headers`). A meta-tag
CSP no HTML **não conta** para a nota (o scanner só lê cabeçalhos de resposta HTTP).

**Solução (Rota A, recomendada):** pôr a **Cloudflare** à frente do GitHub Pages e injetar
os cabeçalhos com uma **Transform Rule**. ~30 min, grátis, mantém o hosting no GitHub Pages.
Único passo que exige o dono do domínio: **mudar os nameservers no registrar**.

The configuration values below are examples. Replace the fictional domains before use;
the dated audit at the end records historical resources, not the current deployment.

---

## Os 6 cabecalhos (exemplos; substituir os dominios)

| Cabeçalho | Valor |
| --- | --- |
| `Strict-Transport-Security` | `max-age=31536000; includeSubDomains; preload` |
| `Content-Security-Policy` | *(ver bloco abaixo — é uma linha só)* |
| `X-Frame-Options` | `DENY` |
| `X-Content-Type-Options` | `nosniff` |
| `Referrer-Policy` | `strict-origin-when-cross-origin` |
| `Permissions-Policy` | `accelerometer=(), camera=(), geolocation=(), gyroscope=(), magnetometer=(), microphone=(), payment=(), usb=()` |

**CSP (uma linha):**

```
default-src 'self'; base-uri 'self'; object-src 'none'; script-src 'self' https://js.stripe.com; style-src 'self' 'unsafe-inline'; font-src 'self'; img-src 'self' data: https://cdn.discordapp.com https://*.stripe.com; connect-src 'self' https://api.example.com https://api.stripe.com https://checkout.stripe.com https://r.stripe.com; frame-src https://checkout.stripe.com; media-src 'self'; form-action 'self'; frame-ancestors 'none'
```

> É o CSP que já está na meta-tag **+ `frame-ancestors 'none'`** (a meta-tag não consegue
> definir `frame-ancestors`; o cabeçalho consegue, e é o que dá o ponto do X-Frame no A+).

---

## Rota A — Cloudflare em frente do GitHub Pages

### Passo 1 — Adicionar o domínio à Cloudflare
1. Criar conta grátis em cloudflare.com → **Add a site** → `example.com` → plano **Free**.
2. A Cloudflare faz scan do DNS atual. Confirmar que ficam os registos do GitHub Pages,
   **todos com o proxy LIGADO (nuvem laranja)**:
   - `A  @  185.199.108.153`
   - `A  @  185.199.109.153`
   - `A  @  185.199.110.153`
   - `A  @  185.199.111.153`
   - `CNAME  www  <user>.github.io` (o `<user>` é o dono do repo `discord-bot-Vozen`)
3. A Cloudflare mostra **2 nameservers** (ex.: `xxx.ns.cloudflare.com`). **← ação do dono:**
   ir ao **registrar** do example.com e substituir os nameservers pelos da Cloudflare.
4. Esperar a propagação (a Cloudflare avisa por email; costuma ser minutos a poucas horas).

### Passo 2 — HTTPS
- Cloudflare → **SSL/TLS → Overview → modo `Full`** (não "Flexible").
- **SSL/TLS → Edge Certificates → `Always Use HTTPS` = ON**.
- (Deixar o HSTS do painel Cloudflare **desligado** — vamos servir o HSTS pela Transform
  Rule, para o ter no repo/runbook e não em dois sítios.)

### Passo 3 — Injetar os cabeçalhos (Transform Rule)
Cloudflare → **Rules → Transform Rules → Modify Response Header → Create rule**:
- **Nome:** `Vozen security headers`
- **If:** `Hostname` `equals` `example.com` (ou "All incoming requests")
- **Then → Set static** (um por cabeçalho, botão *+ Set new header* para cada):
  os 6 da tabela acima (nome + valor).
- **Deploy.**

> Alternativa equivalente: um **Cloudflare Worker** que faz `fetch` ao origin e acrescenta
> os 6 cabeçalhos à resposta. A Transform Rule é mais simples e chega — usar o Worker só se
> for preciso lógica condicional.

### Passo 4 — Verificar (loop até A)
```bash
# 1) Os 6 cabeçalhos saem mesmo?
curl -sI https://example.com | grep -iE "strict-transport|content-security|x-frame|x-content|referrer|permissions"

# 2) E na página do painel?
curl -sI https://example.com/account.html | grep -iE "content-security|x-frame"
```
Depois: `securityheaders.com` → scan a `example.com`. Se **< A**, ler que cabeçalho falha/está
mal formado, corrigir a Transform Rule, re-scan. Repetir até **A/A+**.

**Teste funcional do CSP (importante):** abrir `https://example.com/account.html` no browser,
consola aberta. O CSP real é mais estrito que a meta — validar que **não há violações de CSP**
e que o login Discord (redirect para `discord.com`) e o `GET api.example.com/api/me/premium`
funcionam. Se aparecer violação, ajustar `connect-src`/`img-src`/etc. na regra.

### Passo 5 — Limpeza no repo (SÓ depois do header estar vivo e validado)
Enquanto a meta-tag CSP **e** o cabeçalho CSP coexistem, o browser aplica **os dois** (ganha o
mais restritivo) — funciona, mas é confuso. Depois de confirmar A/A+ **e** o `account.html` OK:
- Remover das **4** páginas (`site/index.html`, `account.html`, `privacy.html`, `terms.html`),
  linhas ~6–8, o bloco:
  ```html
  <meta http-equiv="Content-Security-Policy" content="…" />
  ```
- A `<meta name="referrer" …>` (linha ~9) pode ficar ou sair (o cabeçalho `Referrer-Policy`
  passa a mandar; é redundante mas inofensiva).
- Rebuild do site + cache-bust dos HTML tocados; commit; deixar o `pages.yml` publicar.
- Re-scan final: deve continuar **A/A+** já sem a meta CSP.

---

## Gotchas / rollback
- **HSTS `preload`:** com `preload`, um erro de HTTPS fica em cache nos browsers e é difícil de
  reverter. Só manter `preload` depois de confirmar HTTPS 100% OK. Em dúvida, começar sem
  `preload` (`max-age=31536000; includeSubDomains`) e acrescentar depois.
- **Propagação DNS:** o loop do Passo 4 tem de aguardar a propagação antes de concluir "falhou".
- **CORS lax** (`access-control-allow-origin: *`, default do GitHub Pages): aparece no scanner
  como aviso em "Additional Information", **não** baixa a nota A. Opcional endurecer via outra
  Transform Rule.
- **Não remover a meta CSP antes** de o cabeçalho estar vivo, senão o site fica sem CSP nenhum
  durante a janela de setup.

---

## Rotas alternativas (se NÃO quiseres Cloudflare-proxy)
- **B — VPS Hetzner + Caddy:** servir o site do VPS (onde já corre o bot) com um bloco `header`
  no Caddyfile + `file_server`, `example.com { ... }`, auto-HTTPS. Mudar o `pages.yml` para
  fazer rsync do `site-dist/` para o VPS e apontar o DNS `example.com → 203.0.113.10`. Controlo
  total, sem 3.º, mas o site sai do GitHub Pages e o VPS passa a ser SPOF do site.
- **C — Cloudflare Pages:** migrar o hosting do GitHub Pages para Cloudflare Pages (liga ao repo,
  faz build) — aí os cabeçalhos vivem num ficheiro `_headers` **versionado no repo**. Deploy
  continua Git-based. Também precisa dos nameservers na Cloudflare. Elegante, mas troca de
  provedor de hosting.

---

## Porque estes valores (auditoria feita em 2026-07-10)
Origens externas que o site **realmente carrega** (grep a `site/`):
- **Fontes:** `fonts.googleapis.com` (CSS) + `fonts.gstatic.com` (ficheiros) → `style-src`/`font-src`.
- **Imagens:** `cdn.discordapp.com` (avatares) + `data:` (inline) → `img-src`.
- **Fetch (XHR):** **só** `https://api.vozen.org` (`GET /api/me/premium`) → `connect-src`.
  (`api.vozen.xyz` aparece só num **comentário** de exemplo em `main-v24.js`, não é chamado.)
- **Scripts:** só locais (`js/i18n-v15.js`, `js/main-v24.js`) → `script-src 'self'`
  (0 `<script>` inline / 0 `onclick=` no HTML — confirmado).
- **Login Discord:** é uma **navegação** (`location.href = discord.com/oauth2/...`), **não** um
  form → `form-action 'self'` não o bloqueia.
- `github.com`, `ko-fi.com`, `top.gg`, `discord.gg`, `policies.google.com` são **links** de
  navegação (não recursos carregados) → não precisam de entrada no CSP.
- `w3.org`/`schema.org` são **namespaces** (SVG xmlns / JSON-LD), não recursos → ignorados.
