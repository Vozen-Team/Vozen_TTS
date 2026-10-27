# Plano: conformidade com os Termos/Política de Desenvolvedor do Discord

> Portal configuration examples use fictional URLs and an application-ID placeholder; replace them with your own public policy pages and app ID.

> Fontes auditadas (2026-07-11): Termos de Serviço de Desenvolvedor (efetivos 2024-07-08),
> Política de Desenvolvedor (efetiva 2024-07-08), Termos do SDK Social (2025-03-17).
> O SDK Social **não se aplica** ao Vozen (é para integrações sociais dentro de jogos);
> só se ativaria se o Vozen fosse integrado num jogo.

## Objetivo

Pôr o bot Vozen (instância pública + código open-source) em conformidade integral com os
Termos de Desenvolvedor e a Política de Desenvolvedor do Discord, e fixar essa conformidade
como regra permanente do projeto (toda a feature futura nasce conforme).

## O que JÁ está conforme (auditado, sem ação)

- **Política de privacidade** completa e fiel (PRIVACY.md + vozen.org/privacy): divulga o envio
  do texto ao Google (gTTS) e OpenAI (neural), tabelas SQLite, cache de áudio, retenção. §5(a) ✓
- **Eliminação por dado**: `/voice reset`, `/voice nickname`, `/voice opt-in`,
  `/voice abbrev remove`, `/server-pronunciation remove`, `/config reset`. §5(b) parcial ✓
- **Opt-out de leitura** (`/voice opt-out`) — respeita a escolha do utilizador. Política #3 ✓
- **Sem DMs não solicitadas** (só sends a canais de guild), **sem venda/partilha de dados**,
  **sem trackers/analytics**, **sem treino de IA** com conteúdo de mensagens. Política #5-7, #17-21 ✓
- **Minimização de PII**: e-mail do Ko-fi hashado; stats de fala divulgadas na policy. #15/#16 ✓
- **Canal de denúncia/suporte**: support server no site e no bot. Política (pós-#14) ✓
- **Segurança**: SSH só-chave, ufw deny-in, `.env` 600, APIs em loopback atrás do Caddy,
  `timingSafeEqual` no webhook, token nunca em git. §2(d)/§5(c) parcial ✓
- **Sem público <13**, conteúdo por defeito apropriado. Política #9 ✓
- **Entitlements Premium Apps já implementados** no código (`premium/entitlementSync.ts`,
  paridade preparada). Base do requisito de monetização ✓

## Scope

### In
As 6 lacunas identificadas (fases abaixo) + regra permanente no CONTRIBUTING.md.

### Out
- SDK Social (não aplicável).
- Verificação do bot / intent privilegiada de Message Content — só obrigatória a caminho dos
  ~75 servidores; fica como gate documentado, não como trabalho agora.
- Migração de gTTS para Google Cloud TTS com DPA (ver Riscos — decisão do Diogo, não bloqueia).

## Fases

### Fase 1 — Ações manuais no Discord Developer Portal (Diogo, 10 min)
Deliverable: portal em ordem; screenshot/confirmação.
- [ ] Preencher **Privacy Policy URL** = `https://bot.example.com/privacy` e **Terms of Service URL**
      = `https://bot.example.com/terms` na app `<DISCORD_APPLICATION_ID>` (General Information).
      Obrigatório por §5(a) ("links públicos e atualizados no Portal do Desenvolvedor").
- [ ] Confirmar que a **descrição da app** no portal descreve fielmente a funcionalidade (§1(b)).
- [ ] Ver o separador **Monetization**: a app é elegível para Premium Apps? (requer app
      verificada + equipa com payout). Anotar o estado — alimenta a Fase 2.
Done: os 2 URLs visíveis no portal; estado de monetização conhecido.

### Fase 2 — Requisito de monetização (Política, "Requisitos de Monetização")
Deliverable: decisão documentada + (se elegível) Premium Apps ativos com paridade de preço.
Depende de: Fase 1 (estado de elegibilidade).
- [ ] **Se a app for elegível** para Premium Apps: criar os SKUs (Plus pessoal + Premium servidor),
      ativar `entitlementSync`, e garantir preço no Discord **≤ preço no Ko-fi** (paridade).
- [ ] **Se NÃO for elegível** (app não verificada — caso provável com 5 servidores): documentar
      em `docs/COMPLIANCE-DISCORD.md` que o requisito só vincula "em regiões onde o Discord
      oferece suporte à monetização" e que a app ainda não tem acesso ao produto; recheck
      obrigatório quando a app for verificada (gate da Fase 6).
Done: ou SKUs live com paridade, ou nota de não-elegibilidade datada.

### Fase 3 — Retenção: purga de dados de guilds que removeram o bot (§5(b))
Deliverable: dados de guild apagados N dias após o bot sair (grace period para re-convite).
- [ ] TDD: teste a `purgeDepartedGuilds` — marca `left_at` em `guild_departed` no `guildDelete`;
      limpa a marca no `guildCreate` (re-convite); purga após **30 dias** todas as tabelas
      com `guild_id` (guild_config, talk_stats, game_score, blocklist, pronunciations,
      premium de servidor, etc. — enumerar via descriptor table).
- [ ] Job periódico no arranque + diário (mesmo padrão dos jobs existentes).
- [ ] Atualizar PRIVACY.md (secção retenção) com a regra dos 30 dias.
Done: teste verde a simular kick→30d→purga; PRIVACY.md atualizado.

### Fase 4 — `/privacy erase`: apagar TUDO sobre mim num comando (§5(b) "maneira acessível")
Deliverable: um comando que elimina todos os dados de um utilizador em todas as tabelas.
- [ ] TDD: `eraseUser(db, userId)` — user_voice, user_nickname, tts_optout,
      user_abbrev, talk_stats, game_score, língua/UI prefs, e o que mais tiver `user_id`
      (enumerar; premium pessoal só com confirmação extra — é um bem pago).
- [ ] Comando `/privacy erase` com confirmação (botão) + i18n nas 34 línguas.
- [ ] Atualizar a tabela do PRIVACY.md ("apagar tudo → /privacy erase").
Done: correr o comando deixa zero rows do user (verificado por teste); docs atualizados.

### Fase 5 — Encriptação em repouso + runbook de incidentes (§5(c))
Deliverable: dados inativos protegidos + processo de breach documentado.
- [ ] **Spike primeiro** (é o maior desconhecido): `better-sqlite3-multiple-ciphers` como
      substituto drop-in do better-sqlite3 (SQLCipher). Validar: migração da BD existente,
      performance, compatibilidade com o código atual. Chave via `.env` (`DB_KEY`).
- [ ] Se o spike passar: migrar a BD de produção (backup → encrypt → swap → restart).
      Se falhar/for frágil: fallback = documentar o risco residual aceite para a BD.
- [ ] `docs/INCIDENT-RESPONSE.md`: em suspeita de acesso não autorizado → conter, avaliar
      âmbito, **notificar o Discord e os utilizadores afetados imediatamente** (§5(c)),
      registar cronologia. Meia página, acionável.
Done: runbook de incidentes commitado; encriptação da BD em defer deliberado.

### Fase 6 — Regra permanente + gates futuros
Deliverable: conformidade fixada no processo do projeto.
- [ ] Secção "Conformidade Discord" no CONTRIBUTING.md do repo (regras que TODA a feature futura
      respeita — ver lista no próprio CONTRIBUTING.md). **[já feito neste planeamento]**
- [ ] `docs/COMPLIANCE-DISCORD.md`: mapa requisito→estado→evidência (a tabela desta auditoria),
      com os 2 gates futuros: (a) ~75 servidores → verificação + review da intent Message
      Content ANTES de crescer; (b) app verificada → recheck do requisito de monetização.
Done: docs commitados; qualquer agente futuro encontra as regras.

## Riscos

- **Monetização é o único risco de enforcement real hoje**: vender premium só via Ko-fi
  viola a letra da política *se* a app tiver acesso a Premium Apps e não o usar. A mitigação
  imediata é a Fase 1/2 (verificar elegibilidade e documentar). Não ignorar.
- **gTTS envia conteúdo de mensagens ao endpoint não-oficial do Google** sem contrato de
  Prestador de Serviço (§12(a) exige acordo escrito). Está divulgado na privacy policy (bom),
  mas é risco residual. Opções futuras: default Piper/Kokoro (local) ou Google Cloud TTS
  (tem DPA, mas custa). Decisão do Diogo — não bloqueia este plano.
- **Encriptação da BD em produção é o passo mais arriscado tecnicamente** (corrupção na
  migração). Por isso é spike-first com backup obrigatório e fallback definido.
- **`/privacy erase` vs premium pago**: apagar o registo premium de quem pagou é destrutivo;
  o comando deve avisar e pedir confirmação dupla nesse caso.

## MVP

Fases 1+2 (portal + monetização documentada) — resolvem o único risco de enforcement
imediato e custam ~30 min. As Fases 3–5 completam a conformidade de dados.

---

**Próxima ação concreta:** Diogo preenche os URLs de Privacy/Terms no Developer Portal e
reporta o estado do separador Monetization (Fase 1) — desbloqueia a Fase 2.
