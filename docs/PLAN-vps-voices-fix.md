# Plano — Repor vozes/línguas/motores/jogos no VPS

> Historical plan: the scp example uses a fictional VPS hostname; replace it with your own host.

> Contexto: o bot migrou do PC (Windows) para o VPS Hetzner (`vozen@91.98.128.192`,
> Ubuntu 24.04, repo em `/home/vozen/discord-bot-Vozen`, serviço `vozen.service`).
> A pasta `models/` é gitignored → o VPS ficou SEM os 38 `.onnx` do Piper, e o
> catálogo de línguas/vozes do bot (availableModels, src/index.ts:59) é construído
> a partir do disco. Diagnóstico completo em baixo de cada fase.
> Regras: nunca imprimir tokens/.env; comentários e commits em PT; nunca editar
> conteúdo com PowerShell Get-Content/Set-Content; git add só de caminhos explícitos.

## Objetivo / Goal
Devolver ao bot no VPS o comportamento que tinha no PC: todas as ~35 línguas no
`/voice set`/`/voice list`, motores Piper e Kokoro reais (não só Google), e `/game`
plenamente funcional — e torná-lo resiliente para nunca mais ficar sem catálogo.

## Scope
### In
- Instalar modelos Piper (.onnx + .onnx.json) e binário piper Linux no VPS.
- Sidecar Kokoro no VPS (equivalente Linux do setup-kokoro.ps1).
- Código: catálogo de línguas independente do disco (estender o mecanismo
  GTTS_ONLY_MODELS) + aviso amigável quando um motor não está disponível.
- Upgrade Node 20 → 22 LTS no VPS.
- Verificação ao vivo de /voice, /game, /joke.
### Out
- Motor neural OpenAI (TTS_ENGINE=neural) — não usado.
- Voice-clone sidecar (Chatterbox) no VPS — fica para depois.
- Auto-deploy GitHub Actions (plano próprio, passo 12 do DEPLOY-VPS.md).
- As alterações não commitadas em src/premium/* (ver Fase 0 — só resolver, não expandir).

## Fases / Phases

### 0. Higiene do working tree (PC) — ANTES de qualquer deploy
O PC tem alterações NÃO commitadas (de outro agente): `src/premium/kofiWebhook.ts`,
`src/premium/statusApi.ts`, `tests/kofi.test.ts`, `tests/statusApi.test.ts`.
- [ ] `git diff` a esses 4 ficheiros; correr `npx vitest run tests/kofi.test.ts tests/statusApi.test.ts`.
- [ ] Se coerentes e verdes → commit próprio ("premium: <o que for>"). Se duvidosos → mostrar ao Diogo antes.
- [ ] **Done:** `git status` limpo (exceto docs/ untracked de outras sessões, que ficam).

### 1. Modelos Piper no VPS (dados — devolve as línguas TODAS)
- [ ] No PC, gerar a lista exata: `ls models/*.onnx` (38 nomes, ex. `es_ES-davefx-medium`).
- [ ] No VPS, baixar cada modelo do HuggingFace `rhasspy/piper-voices` (download direto
      no datacenter é rápido; ~2–3 GB total, disco tem 33 GB livres). Padrão do URL:
      `https://huggingface.co/rhasspy/piper-voices/resolve/v1.0.0/{lang}/{locale}/{voz}/{qualidade}/{modelo}.onnx`
      e o mesmo com `.onnx.json` (o Piper PRECISA do .json ao lado). Escrever um script
      `tools/fetch-piper-models.sh` que recebe a lista e faz wget com retry.
      ⚠️ `pt_PT-tugao-medium` pode não existir no HF (voz custom?) — se falhar o download,
      copiar do PC via scp: `scp models/pt_PT-tugao-medium.onnx* vozen@vps.example.com:~/discord-bot-Vozen/models/`.
- [ ] Instalar o binário piper Linux: release `piper_linux_x86_64.tar.gz` de
      github.com/rhasspy/piper (2023.11.14-2), extrair para `~/piper/`, e no `.env` do
      VPS acrescentar `PIPER_PATH=/home/vozen/piper/piper` (sem imprimir o resto do .env).
- [ ] `systemctl restart vozen.service` (root via consola Hetzner ou sudo).
- [ ] **Done:** log de boot SEM "nenhum modelo .onnx"; `/voice list` mostra ~35 línguas;
      `/voice set` autocomplete mostra as línguas todas; warnings es_ES desaparecem.

### 2. Kokoro no VPS (motor neural leve)
- [ ] Criar `tools/setup-kokoro.sh` (porta Linux do setup-kokoro.ps1): `python3 -m venv
      tools/kokoro-venv`, `pip install -r tools/requirements-kokoro.txt`, baixar
      `kokoro-v1.0.onnx` + `voices-v1.0.bin` (URLs do repo thewh1teagle/kokoro-onnx —
      ver os que o setup-kokoro.ps1 usa) para `tools/`.
- [ ] Correr no VPS; instalar `python3-venv`/`python3-pip` via apt se faltar.
- [ ] Restart + **Done:** log do factory diz `kokoro (sidecar ...)` em vez de
      `(sem sidecar -> = gTTS)`; `/voice set engine:Kokoro` + mensagem soa diferente do Google.

### 3. Código — catálogo resiliente (nunca mais "só Japonês")
- [ ] Em `src/index.ts`, generalizar `GTTS_ONLY_MODELS`: para CADA língua gTTS suportada
      (usar a tabela de línguas existente em voiceMap/LANG_TO_PREFIX) sem NENHUM modelo
      Piper no disco, injetar uma voz sintética `xx_XX-google-medium` no catálogo — o
      mesmo que já se faz para ja_JP. Assim, num servidor sem .onnx, as línguas aparecem
      todas na mesma e falam via Google.
- [ ] No handler do `/voice set` com `engine=piper`/`kokoro` indisponível no processo
      (sem binário/modelos/sidecar), responder com nota amigável tipo "esse motor não está
      instalado neste servidor — vou usar Google" (o fallback runtime já existe; isto é UX).
- [ ] Testes: unit para a expansão do catálogo (com models=[] → todas as línguas gTTS);
      atualizar testes existentes que assumam o catálogo antigo.
- [ ] `npm run build && npm run typecheck && npx vitest run` verdes; commit + push;
      deploy no VPS (git pull --ff-only, npm ci, npm run build, restart).
- [ ] **Done:** apagar temporariamente um .onnx num ambiente de teste NÃO remove a língua
      das opções (fala via Google).

### 4. Node 22 LTS + verificação final ao vivo
- [ ] No VPS: instalar Node 22 (nodesource setup_22.x), `npm ci && npm run build`, restart.
      Confirmar que o aviso do `@discordjs/voice` (>=22.12) desaparece e o bot arranca.
- [ ] Testes ao vivo no Discord: `/voice list` (línguas todas), `/voice set` com cada motor,
      mensagem lida no auto-read, `/joke language:Português`, `/game play` (1 jogo de voz,
      ex. Ditado, do início ao fim) e `/game play` Wordle (texto).
- [ ] `journalctl -u vozen.service --since '10 minutes ago'` sem ERROR.
- [ ] **Done:** checklist acima toda verde; atualizar docs/DEPLOY-VPS.md com a secção
      "modelos Piper + Kokoro no VPS" para instalações futuras.

## Riscos / Risks
- **pt_PT-tugao-medium** (voz calibrada do Diogo) pode não existir no HuggingFace → scp do PC (lento mas one-off). O mesmo para qualquer modelo custom.
- **Espaço/rede**: 2–3 GB de modelos + ~300 MB Kokoro — ok (33 GB livres), mas correr downloads com retry e verificar tamanhos no fim (`ls -la models/ | wc -l` = 38×2 ficheiros).
- **Node 22**: rebuild dos módulos nativos (better-sqlite3, @discordjs/opus) — se falhar compilação, ficar em Node 20 (funciona, só avisa) e tratar à parte.
- **Fase 3 mexe no arranque do catálogo** — é a única fase de código com risco real; testes primeiro, deploy só com suite verde. Fases 1–2 são só dados/infra e reversíveis.
- **Restart precisa de root**: o user `vozen` não tem sudo sem password — usar consola Hetzner para o `systemctl restart`, ou configurar antes o sudoers NOPASSWD (linha única para `systemctl restart vozen.service`, como no DEPLOY-VPS.md passo 12).

## MVP
Fim da **Fase 1**: línguas todas de volta no `/voice set`/`/voice list`, Piper real a
funcionar, warnings es_ES extintos, e o `/game` re-testado (provavelmente já ok com o
fallback + modelos). As Fases 2–4 são qualidade (Kokoro, resiliência, Node).

**Próxima ação concreta:** correr `ls models/*.onnx` no PC e gerar a lista dos 38 modelos para o script de download da Fase 1.
