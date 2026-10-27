# Deploy do bot num VPS 24/7 + API do Premium em HTTPS

> Historical guide: configuration examples use fictional domains and IDs. Replace them with your own values; this is not the current production configuration.

Guia passo-a-passo para pôr o Vozen a correr sempre ligado num VPS Linux, e
expor a API do painel Premium (`GET /api/me/premium`) em HTTPS — a peça que
falta para o login com Discord no site (`account.html`) funcionar a sério.

Sem isto o site fica no estado "em breve" (painel dormente). Depois deste
guia: `PREMIUM_API_ENABLED=true` + domínio `api.example.com` + login OAuth
real.

## 0. Escolha do VPS

**Recomendado:** [Hetzner Cloud](https://www.hetzner.com/cloud) — plano
**CX22** (2 vCPU, 4 GB RAM, 40 GB disco), ~€4,5/mês. Região `Falkenstein`
ou `Nuremberg` (Alemanha) — mais perto de Portugal, boa latência.

Ao criar o servidor:
- **Imagem:** Ubuntu 24.04 LTS
- **SSH key:** adiciona a tua chave pública (`ssh-keygen` se ainda não
  tiveres uma) — mais seguro que password. Se preferires password, o
  Hetzner manda-a por email.
- Nome sugerido: `vozen-prod`

No fim, o Hetzner dá-te um **IP público** — precisas dele nos passos
seguintes.

## 1. Apontar o subdomínio da API

Na Namecheap (Advanced DNS do `bot.example.com`, o mesmo sítio dos outros
registos), adiciona:

| Type | Host | Value | TTL |
|---|---|---|---|
| A Record | `api` | `<IP do VPS>` | Automatic |

Isto faz `api.example.com` apontar para o VPS. Confirma com
`nslookup api.example.com` ao fim de uns minutos.

## 2. Primeiro acesso e atualização do sistema

```bash
ssh root@<IP do VPS>

apt update && apt upgrade -y
apt install -y curl git ffmpeg build-essential ufw
```

`ffmpeg` é obrigatório (o player de voz depende dele).
`build-essential` é preciso para compilar módulos nativos do npm
(`better-sqlite3`, `@discordjs/opus`).

## 3. Node.js 22 LTS

O `@discordjs/voice` exige Node >= 22.12 (o `engines` do package.json também).

```bash
curl -fsSL https://deb.nodesource.com/setup_22.x | bash -
apt install -y nodejs
node -v   # confirma v22.x
```

## 4. Utilizador não-root (boa prática)

```bash
adduser vozen
usermod -aG sudo vozen
su - vozen
```

Todos os passos seguintes correm como `vozen`, não como `root`.

## 5. Clonar e preparar o bot

```bash
git clone https://github.com/Rexy40407/discord-bot-Vozen.git
cd discord-bot-Vozen
npm ci
npm run build
```

## 6. `.env` de produção

```bash
cp .env.example .env
nano .env
```

Preenche (**nunca commitar este ficheiro**):

```dotenv
DISCORD_TOKEN=<o token do bot>
CLIENT_ID=<DISCORD_APPLICATION_ID>

# Motor TTS — piper é grátis/self-hosted, é o default se deixares em branco
TTS_ENGINE=piper

# Ko-fi (se já tiveres o produto configurado)
KOFI_WEBHOOK_TOKEN=<token da Ko-fi, em Settings -> API>
KOFI_WEBHOOK_PORT=3001
# Rust runtime: set HEALTH_PORT=3001 instead when the Rust process owns the HTTP listener.
# KOFI_WEBHOOK_PORT is the legacy Node-only listener setting.

# Painel Premium — ATIVA aqui
PREMIUM_API_ENABLED=true
PREMIUM_API_ORIGIN=https://bot.example.com

# Recompensa top.gg (o segredo whs_ vem do dashboard da top.gg)
TOPGG_WEBHOOK_SECRET=<whs_...>
# Gera UMA VEZ com: openssl rand -hex 32
# Nunca troques esta chave sem migrar o ledger de resgates.
VOTE_REDEMPTION_SECRET=<64 caracteres hex>

# Cópias SQLite locais fora do checkout Git
DB_BACKUP_DIR=../vozen-backups
DB_BACKUP_RETENTION_DAYS=30
```

`PREMIUM_API_ORIGIN` é a única origem que o CORS deixa chamar a API — tem
de ser exatamente `https://bot.example.com` (sem barra final).

## 7. Serviço systemd (mantém o bot ligado 24/7)

```bash
sudo nano /etc/systemd/system/vozen.service
```

Conteúdo:

```ini
[Unit]
Description=Vozen Discord TTS bot
After=network.target
StartLimitIntervalSec=300
StartLimitBurst=10

[Service]
Type=simple
User=vozen
WorkingDirectory=/home/vozen/discord-bot-Vozen
ExecStart=/usr/bin/npm run start:prod
Restart=always
RestartSec=5
Environment=NODE_ENV=production

[Install]
WantedBy=multi-user.target
```

> **Porquê `Restart=always` (e não `on-failure`)**: se alguém/algo matar diretamente o
> processo-folha `dist/index.js` (ex.: `kill <pid>`), o handler de shutdown do bot sai
> com código **0**; o supervisor vê "saída limpa" e não reinicia, e `on-failure` ignora
> saídas limpas — o bot 24/7 ficava DOWN até um `systemctl start` manual. `always`
> recupera esse caso e continua a respeitar o `systemctl stop` do operador (o systemd
> suprime o restart em stops intencionais). O `StartLimitIntervalSec/Burst` trava
> crash-loops (máx. 10 restarts em 5 min). Verificado ao vivo em 2026-07-09: kill ao
> leaf → "Scheduled restart job" → bot online outra vez em ~25s.

Ativa:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now vozen.service
sudo systemctl status vozen.service   # confirma "active (running)"
sudo journalctl -u vozen.service -f   # logs em direto (Ctrl+C para sair)
```

O `start:prod` usa o supervisor `scripts/start-prod.mjs` (lock, restart
automático). O systemd é a segunda camada — reinicia mesmo que o processo
Node morra por completo ou a máquina reinicie.

## 8. Caddy — HTTPS automático e grátis para `api.example.com`

Caddy trata do certificado Let's Encrypt sozinho, sem qualquer configuração
manual de SSL.

```bash
sudo apt install -y debian-keyring debian-archive-keyring apt-transport-https
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' | sudo tee /etc/apt/sources.list.d/caddy-stable.list
sudo apt update
sudo apt install -y caddy
```

Edita o Caddyfile:

```bash
sudo nano /etc/caddy/Caddyfile
```

Substitui tudo por:

```
api.example.com {
    reverse_proxy localhost:3001
}
```

Aplica:

```bash
sudo systemctl reload caddy
```

Caddy vai buscar o certificado automaticamente na primeira ligação
(precisa do DNS do passo 1 já propagado). Testa:

```bash
curl -I https://api.example.com/api/me/premium
# esperado: 401 Unauthorized (sem token) — confirma que o HTTPS + proxy funcionam
```

## 9. Firewall

Só deixa passar SSH, HTTP e HTTPS. A porta 3001 fica **só** acessível
localmente (o Caddy é que fala com ela via `localhost`).

```bash
sudo ufw allow OpenSSH
sudo ufw allow 80/tcp
sudo ufw allow 443/tcp
sudo ufw enable
sudo ufw status
```

## 10. Ligar o site ao backend (eu faço isto)

Três alterações no código, depois de confirmares que `api.example.com`
responde em HTTPS:

1. `site/js/main-v14.js`: `PREMIUM_API_BASE = "https://api.example.com"`
2. Build + commit + push do site (dispara o deploy do GitHub Pages).
3. **Discord Developer Portal** → a tua app → **OAuth2 → Redirects** →
   adicionar `https://bot.example.com/account.html`.

Depois disto, `https://bot.example.com/account.html` deixa de mostrar "em
breve" e passa a ter o botão real de login com Discord.

## 11. Deploys seguintes (manual)

O runtime de produção é Rust desde 2026-07-26. Para um deploy manual:

```bash
cd ~/vozen-rust-prod
git fetch origin
git reset --hard origin/main
bash scripts/deploy-rust-vps.sh
```

O script compila a imagem enquanto o container atual continua online, cria e
verifica um backup SQLite consistente, recria o container, espera por health +
gateway Ready e volta à imagem anterior se o arranque falhar.

## 12. Deploy automático (GitHub Actions → SSH ao VPS)

> **ATUALIZADO PARA RUST (2026-07-26).** O workflow
> `.github/workflows/deploy-bot.yml` só avança depois de a CI completa passar na
> `main`. Usa os secrets `VPS_HOST`, `VPS_USER` e `VPS_SSH_KEY`.

Depois da CI verde, o GitHub faz SSH ao VPS, alinha
`~/vozen-rust-prod` com `origin/main` e executa
`scripts/deploy-rust-vps.sh`. Os segredos funcionais continuam apenas em
`.env.rust.prod`; não atravessam os logs nem são enviados a cada deploy.

**No VPS**, gera um par de chaves SSH dedicado ao deploy (não uses a tua
chave pessoal):

```bash
ssh-keygen -t ed25519 -f ~/.ssh/deploy_key -N ""
cat ~/.ssh/deploy_key.pub >> ~/.ssh/authorized_keys
cat ~/.ssh/deploy_key        # copia esta CHAVE PRIVADA para o passo seguinte
```

**No GitHub** → repositório → **Settings → Secrets and variables → Actions
→ New repository secret**, cria estes segredos:

| Nome | Valor |
|---|---|
| `VPS_HOST` | o IP do VPS |
| `VPS_USER` | `vozen` |
| `VPS_SSH_KEY` | o conteúdo da chave **privada** (`~/.ssh/deploy_key`, tudo incluindo `-----BEGIN...-----`) |

A cópia online inclui alterações committed ainda presentes no WAL e fica em
`~/vozen-backups`, fora do checkout. Isto protege reinícios e deploys, mas
**não** a perda total do disco/VPS: copia regularmente `rust-data/tts.db`,
`~/vozen-backups/` e `.env.rust.prod` para armazenamento externo cifrado.

## Checklist rápida

- [ ] VPS criado (Ubuntu 24.04), IP anotado
- [ ] DNS `api.example.com` → IP do VPS
- [ ] Docker Engine + Compose instalados
- [ ] Repo em `~/vozen-rust-prod` alinhado com `origin/main`
- [ ] `.env.rust.prod` preenchido e protegido (`chmod 600`)
- [ ] `vozen.service` Node parado/desativado
- [ ] `vozen-prod-vozen-1` a correr e healthy
- [ ] Caddy a servir `https://api.example.com` com certificado válido
- [ ] `ufw` ativo, só 22/80/443 abertas
- [ ] `PREMIUM_API_BASE` atualizado no site + push
- [ ] Redirect URI adicionado no Discord Developer Portal
- [ ] Deploy automático (passo 12): três secrets SSH no GitHub + workflow Rust
- [ ] `VOTE_REDEMPTION_SECRET` gerado uma vez e guardado com a cópia cifrada do `.env`
- [ ] `scripts/backup-rust-db.py` cria uma cópia restaurável fora do checkout Git
- [ ] Backup externo cifrado de `rust-data/tts.db`, `vozen-backups/` e `.env.rust.prod`

## Estado real do VPS (feito em 2026-07-09)

Setup já aplicado no `vozen-prod`. Se algum dia re-clonares o repo do zero, repõe:

### Modelos Piper (línguas + motor Piper)
`./models` é **gitignored** — um clone fresco fica sem os 38 `.onnx`, e o catálogo
de línguas colapsa (só sobram as vozes gTTS sintéticas). Repõe copiando do PC:
```bash
# do PC (tem-nos em C:\Users\diogo\piper_pkg\piper\models — 38 .onnx + 38 .onnx.json, 2.3 GB):
scp C:/Users/diogo/piper_pkg/piper/models/*.onnx* vozen@vps.example.com:~/discord-bot-Vozen/models/
```
Binário piper **Linux** (o `.exe` do PC não serve) + `PIPER_PATH` no `.env`:
```bash
cd ~ && wget -q https://github.com/rhasspy/piper/releases/download/2023.11.14-2/piper_linux_x86_64.tar.gz && tar xzf piper_linux_x86_64.tar.gz
# no .env do bot (append, não imprime segredos):
grep -q '^PIPER_PATH=' ~/discord-bot-Vozen/.env || echo 'PIPER_PATH=/home/vozen/piper/piper' >> ~/discord-bot-Vozen/.env
```
Nota: desde `210f171`, `syntheticGttsModels()` faz o catálogo **independente do disco**
— mesmo sem `.onnx` as ~40 línguas aparecem (via Google). Os modelos só acrescentam
as vozes Piper reais.

### Kokoro (motor neural) — porta Linux do setup-kokoro.ps1
`kokoro-venv/`, `kokoro-v1.0.onnx`, `voices-v1.0.bin` são gitignored. Repõe:
```bash
sudo apt-get install -y python3-venv python3-pip python3-dev libsndfile1
cd ~/discord-bot-Vozen/tools && python3 -m venv kokoro-venv
kokoro-venv/bin/python -m pip install -U pip && kokoro-venv/bin/python -m pip install -r requirements-kokoro.txt
REL=https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0
wget -q $REL/kokoro-v1.0.onnx -O kokoro-v1.0.onnx && wget -q $REL/voices-v1.0.bin -O voices-v1.0.bin
```
Desde `210f171`, `resolveKokoroCmd()` auto-deteta `bin/python` (Linux) — sem `KOKORO_CMD`.

### Node 22 LTS
```bash
curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash - && sudo apt-get install -y nodejs
cd ~/discord-bot-Vozen && npm rebuild   # recompila os módulos nativos para o ABI do Node 22
```

### Restart (sudoers já configurado para o vozen)
```bash
sudo -n systemctl restart vozen.service
```
