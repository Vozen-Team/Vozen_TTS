# 037 — Vozen Painel (no site do Vozen Helper)

> Historical plan: domains and owner identifiers below are examples, not active production settings.

> Planeado 2026-07-17, revisto após correção do Diogo: a consola NÃO vai para
> bot.example.com — vai para o site do painel do Helper. Estado: **AGUARDA APROVAÇÃO — não
> executar sem "executa".**

## Objetivo

No site do painel que já existe (`https://admin.example.com/`), criar um
**Vozen Painel** ao lado do Vozen Helper Painel — com navegação por dois botões no topo —
onde o dono (username+password + OAuth Discord restrito ao `OWNER_ID`) dá/revoga passes
Premium/Plus e vê os servidores do Vozen (nome, ícone, mensagens lidas, top speakers).
Inclui o diagnóstico do `/vozengrant` "não está on" e a verificação do funil de vendas.

**Arquitetura em uma linha:** UI no repo `vozen-helper-bot` (Pages, público) → chama a API
do bot Vozen (`api.example.com`, VPS) → que passa a aceitar o origin
`https://admin.example.com` nos endpoints `/api/admin/*`.

**Assunções explícitas:**
- O painel do Helper autentica por **OAuth Discord apenas** (confirmado: não há password no
  `Vozen-helper/src/api/server.ts`; o gate é `allowedUserId` + sessão HMAC). Para o Vozen
  Painel mantenho o que pediste na mensagem original: **username+password E OAuth** — uma
  camada a mais que o Helper. Se preferires igualar ao Helper (só OAuth), corta-se a F1
  para metade; decide quando aprovares.
- O ID autorizado (`<OWNER_DISCORD_ID>`) entra por env var no VPS (`OWNER_ID`, já existe
  no config do Vozen), nunca hardcoded nos repos públicos.
- A API do Vozen fica em `api.example.com` (URL fixo) — não precisa do mecanismo `api-url.js`
  que o site do Helper usa para o túnel.
- Aba de servidores usa **apenas dados já recolhidos** (`talk_stats` → `/topspeakers`
  público; `serverStats` agrega) + cache do discord.js (nome/ícone). Zero recolha nova →
  conforme as diretrizes do Discord, com nota no PRIVACY.

## Scope

### In
- F0: diagnóstico vendas + `/vozengrant`; escolha do wiring OAuth para a página nova.
- Repo `vozen-helper-bot`: botão "Vozen · Painel" no header do painel existente + página
  nova `site/vozen.html` (login → tabs Grants | Servidores) + botão de volta.
- Repo `Vozen-bot`: auth admin (scrypt + sessão HMAC + rate-limit), endpoints
  `/api/admin/{login,passes,grant,revoke,guilds}`, CORS para o origin do painel.
- Auditoria mínima: linha de log por mutação.
- Nota "service administration" no PRIVACY.md do Vozen.

### Out
- Ações de moderação sobre servidores (leave/ban) — aba servidores é só leitura.
- Mexer no painel do Helper além do botão de navegação.
- Multi-operador/roles; histórico gráfico de mensagens; alterações ao fluxo Ko-fi.
- Unificar as sessões dos dois painéis (cada API mantém a sua sessão).

## Fases

### F0 — Diagnóstico: vendas + `/vozengrant` + wiring OAuth (independente)
Causa provável já identificada: o comando é **`/vozengrant`** e **sem `OWNER_GUILD_ID` no
`.env` não é registado em lado nenhum** (`Vozen-bot/src/config/index.ts:115-118`).
- [ ] VPS (com o Diogo, padrão single-line + `read`): confirmar `OWNER_GUILD_ID` e `OWNER_ID`; definir se faltarem; restart; `/vozengrant` visível no servidor do dono
- [ ] Smoke do funil: webhook Ko-fi responde, claim no `/account` ok, `/vozengrant` dá e remove um passe de teste
- [ ] Decidir wiring OAuth da página nova: reutilizar o client OAuth do Helper (redirect URI novo `.../vozen.html` no Dev Portal — precisa do Diogo) OU o client do Vozen (idem); documentar a escolha no plano
- **Done:** `/vozengrant` funcional; funil verificado; redirect URI escolhido e registado.

### F1 — Núcleo de auth admin no Vozen-bot (TDD, puro) [dep: nenhuma]
- [ ] `src/premium/adminAuth.ts`: `verifyAdminPassword` (scrypt em `ADMIN_PASS_HASH`, user em `ADMIN_USER`, timingSafeEqual), `signAdminSession`/`verifyAdminSession` (HMAC-SHA256, TTL 8h, `ADMIN_SESSION_SECRET` — padrão do Helper), rate-limit de login (5/15min)
- [ ] `tools/hash-admin-pass.mjs` (gera o hash; password nunca em claro no `.env`)
- [ ] Config + `.env.example`; sem as env vars → endpoints admin inertes (404)
- [ ] Testes: password certa/errada, sessão válida/expirada/adulterada, rate-limit
- **Done:** vitest verde; sem envs, nenhum endpoint admin existe.

### F2 — Endpoints admin no router do Vozen-bot (TDD) [dep: F1]
- [ ] CORS: adicionar `https://admin.example.com` aos origins aceites **só nas rotas `/api/admin/*`** (o resto mantém bot.example.com)
- [ ] `POST /api/admin/login` — `{user, pass}` + `Authorization: Bearer <token OAuth>`; password ∧ identidade OAuth ∧ `id === config.ownerId` → sessão HMAC; senão 403 indistinto
- [ ] `GET /api/admin/passes` — ativos + pendentes Ko-fi por reclamar
- [ ] `POST /api/admin/grant` — `{id, kind: plus|premium, days, seats?}` → `grantUserPremium`/`grantGuildPass` com `source='manual'` (o MESMO caminho do `/vozengrant`)
- [ ] `POST /api/admin/revoke` — desativa um passe
- [ ] `GET /api/admin/guilds` — cache do bot (id, nome, ícone, memberCount) + `buildServerStats` (totalMessages, activeSpeakers, top speakers)
- [ ] Linha de auditoria no log por mutação; testes 403 (sem sessão/forjada/ID errado) + happy paths
- **Done:** vitest verde; nenhum dado sai sem sessão válida.

### F3 — UI no repo vozen-helper-bot: navegação + Vozen Painel (grants) [dep: F2]
- [ ] Header do painel existente (`site/index.html`): botão "Vozen · Painel" → `vozen.html` (como no desenho); em `vozen.html`, botão "Helper · Painel" de volta
- [ ] `site/vozen.html` — self-contained como as páginas do site do Helper, mesma linguagem visual (dark navy, labels mono, badges verde/vermelho)
- [ ] Login: user+pass + OAuth Discord (fluxo do wiring da F0) → `POST /api/admin/login` → sessão em memória
- [ ] Tab Grants: form (Discord ID, Plus/Premium, dias, seats) + tabela de passes com revogar + pendentes por reclamar; estados: login errado, sessão expirada→login, grant ok/erro, tabela vazia
- [ ] Verificação no browser contra a API real
- **Done:** dou um Plus de teste pelo browser; outra conta Discord → 403; deploy Pages do repo helper verde.

### F4 — Tab Servidores + transparência [dep: F3]
- [ ] Tab na `vozen.html`: tabela ordenável (ícone, nome, membros, msgs lidas) + linha expansível com top speakers (IDs + contagens)
- [ ] Ícones: `img-src` para `cdn.discordapp.com` — verificar CSP/headers do site do helper (Pages)
- [ ] PRIVACY.md do Vozen + site privacy: nota — o operador vê agregados de uso por servidor para administração do serviço (dados já recolhidos/divulgados; apagamento inalterado)
- **Done:** tab mostra servidores reais; PRIVACY atualizado; nada de novo escrito na BD.

### F5 — Fecho [dep: F0–F4]
- [ ] `npm run check` verde no Vozen-bot; deploy bot (deploy-bot.yml) + Pages do vozen-helper-bot
- [ ] Envs no VPS com o Diogo: `ADMIN_USER`, `ADMIN_PASS_HASH`, `ADMIN_SESSION_SECRET` (+ F0 se faltarem) → restart
- [ ] Verificação live: login com a tua conta ok; segunda conta → 403; sem password → 403
- [ ] Doc-sync: ARCHITECTURE.md (API admin + origin extra) + este plano marcado
- **Done:** grant real feito pelo painel em produção, visível no `/account` do alvo.

## UI Blueprint (acessório — confinado à F3/F4)

- **Direção:** a linguagem do painel do Helper (dark navy, mono, badges) — o Vozen Painel
  deve parecer irmão do que já existe nesse site, não um enxerto do bot.example.com.
- **Tokens:** extrair do `index.html` do site do helper na F3 (cores/fontes/espaçamentos
  inline); novos: apenas badge de tier (Plus/Premium) e estados de tabela.
- **Componentes:** SwitcherButtons (header, 2 painéis) → LoginCard (default/erro/loading) →
  TabBar → GrantForm (default/loading/ok/erro) · PassesTable (vazia/carregada/erro) ·
  GuildsTable (vazia/carregada; expansível).
- **Layout:** desktop-first (operador no PC); utilizável a 375px (tabelas com scroll próprio).
- **Fluxos:** (1) helper→botão→vozen→login→grant ok; (2) vozen→servers→expandir; (3) volta ao helper.
- **Acessibilidade:** focus visível, AA, alvos ≥44px.
- **Ordem:** tokens (extração) → primitivos → tabelas/forms → página.

## Riscos

- **Dois repos públicos** — nem a página nem o código são segredo; a segurança vive toda na
  API do Vozen (password ∧ OAuth ∧ ID via env). Nenhum segredo em nenhum repo.
- **F0 pode resolver o problema original sozinha** (env vars em falta no VPS). Diagnóstico
  primeiro, de propósito — o painel passa a conveniência, não fix.
- **OAuth redirect URI** para a página nova exige o Dev Portal (Diogo) — decidido na F0
  para não bloquear a F3 a meio.
- **Grants = dinheiro** — mitigado por reutilizar o caminho testado do `/vozengrant` +
  auditoria por mutação.
- **CORS:** `https://admin.example.com` is a fictional private-panel origin. Configure
  the exact origin you control; CORS does not replace authentication on `/api/admin/*`.
- **Consistência de auth:** o Helper é só-OAuth; o Vozen Painel terá user+pass extra (como
  pediste). Se preferires igualar, diz na aprovação — corta metade da F1.
- **Compliance Discord:** ok porque não há recolha nova (`talk_stats` já existe, já público
  via `/topspeakers`, já apagável). Histórico novo seria outro plano.
- **Envs no VPS precisam do Diogo** (padrão single-line do plano 036).

## MVP

**F0–F3**: funil verificado + `/vozengrant` operacional + dou/revogo passes pelo browser a
partir do site do Helper. A tab Servidores (F4) é a segunda entrega.

---

Próxima ação concreta: F0 — correr no VPS o check de `OWNER_GUILD_ID`/`OWNER_ID` no `.env` (comando single-line contigo) e confirmar o `/vozengrant` no servidor do dono.
