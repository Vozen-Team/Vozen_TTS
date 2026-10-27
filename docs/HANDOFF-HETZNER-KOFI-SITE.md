# Handoff — Hetzner, Ko-fi, site e correcoes recentes

> Historical handoff: command/configuration examples now use fictional hosts. Replace them before use; the dated observations outside examples are not current production instructions.

Data: 2026-07-09  
Projeto local: `C:\Users\diogo\Videos\second brain\bots-discord\Vozen-bot`  
Repo remoto: `https://github.com/Rexy40407/discord-bot-Vozen.git`  
VPS Hetzner: `vozen-prod`, IPv4 `91.98.128.192`, Ubuntu 24.04.4 LTS  
Utilizador de deploy no VPS: `vozen`

## Regra importante

Nao imprimir nem pedir para colar `.env`, tokens, Discord token, Ko-fi token, ou chaves privadas.

O token do Ko-fi foi colocado no `.env` do VPS durante esta sessao, mas nao deve ser escrito em docs, logs ou chat. Se houver duvida de seguranca, gerar outro token no Ko-fi e atualizar o VPS.

## Estado atual

- SSH para o VPS funciona com `vozen@91.98.128.192`.
- Repo no VPS: `/home/vozen/discord-bot-Vozen`.
- Servico systemd: `vozen.service`.
- Bot online como `Vozen#2281`.
- Ko-fi webhook ativo na porta `3001`.
- API publica responde em `https://api.vozen.org`.
- Site publico em `https://vozen.org`.
- Commit atualmente deployado no VPS depois da correcao de voz: `449cf88`.

Checks feitos:

```bash
systemctl is-active vozen.service
```

Resultado esperado:

```text
active
```

```bash
curl -i https://api.example.com/api/me/premium
```

Resultado esperado sem token Discord:

```text
HTTP/1.1 401 Unauthorized
{"error":"no_token"}
```

## O que foi configurado no Hetzner

### 1. Acesso SSH para o utilizador `vozen`

Foi adicionada a chave publica local ao servidor. O fingerprint correto da chave local e:

```text
SHA256:Mrz0q2m0Ho463SYOCly+zzyf7n7vz2CDCl5LwmOxSbo
```

No Hetzner, via consola root, o ficheiro correto e:

```bash
/home/vozen/.ssh/authorized_keys
```

Comandos usados/corrigidos:

```bash
mkdir -p /home/vozen/.ssh
printf '%s\n' '<PUBLIC_KEY_ED25519>' > /home/vozen/.ssh/authorized_keys
chown -R vozen:vozen /home/vozen
chmod 755 /home/vozen
chmod 700 /home/vozen/.ssh
chmod 600 /home/vozen/.ssh/authorized_keys
ssh-keygen -lf /home/vozen/.ssh/authorized_keys
```

Nota: durante a configuracao houve erros com `authorized-keys` vs `authorized_keys` e uma letra errada na chave (`K` vs `k`). O fingerprint acima foi usado para confirmar que ficou certo.

Teste de SSH a partir da maquina local:

```powershell
ssh -o BatchMode=yes -i $env:USERPROFILE\.ssh\id_ed25519 vozen@vps.example.com "echo VOZEN_SSH_OK && hostname && pwd"
```

Resultado esperado:

```text
VOZEN_SSH_OK
vozen-prod
/home/vozen
```

### 2. `.env` do VPS

Ficheiro:

```bash
/home/vozen/discord-bot-Vozen/.env
```

Foi confirmado sem imprimir valores:

```text
DISCORD_TOKEN=SET
CLIENT_ID=SET
KOFI_URL=SET
KOFI_WEBHOOK_TOKEN=SET
KOFI_WEBHOOK_PORT=SET
PREMIUM_API_ENABLED=SET
PREMIUM_API_ORIGIN=SET
```

Exemplos de valores nao secretos (dominio ficticio):

```env
KOFI_URL=https://ko-fi.com/YOUR_PAGE
KOFI_WEBHOOK_PORT=3001
PREMIUM_API_ENABLED=true
PREMIUM_API_ORIGIN=https://bot.example.com
```

O `KOFI_WEBHOOK_TOKEN` tambem foi colocado, mas o valor nao deve ser documentado.

Comando seguro para verificar presenca sem mostrar valores:

```bash
cd /home/vozen/discord-bot-Vozen
for k in DISCORD_TOKEN CLIENT_ID KOFI_URL KOFI_WEBHOOK_TOKEN KOFI_WEBHOOK_PORT PREMIUM_API_ENABLED PREMIUM_API_ORIGIN; do
  if grep -q "^$k=" .env; then echo "$k=SET"; else echo "$k=MISSING"; fi
done
```

### 3. Ko-fi webhook

Exemplo do campo Webhook URL no Ko-fi (substituir pelo teu dominio):

```text
https://api.example.com/
```

Fluxo esperado:

1. Comprador compra no Ko-fi.
2. Ko-fi envia webhook para a URL configurada, por exemplo `https://api.example.com/`.
3. Bot valida `KOFI_WEBHOOK_TOKEN`.
4. Bot extrai Discord User ID do campo Message.
5. Se produto contem `Premium`, cria pass de servidor com 3 licencas.
6. Se produto contem `Plus`, aplica Plus ao utilizador.
7. Para Premium de servidor, o utilizador entra no servidor e corre:

```text
/premium activate
```

Descricao recomendada para produto Ko-fi:

```text
Paste your Discord User ID in the Message field at checkout so Premium can activate automatically.

Get your Discord ID here:
https://bot.example.com/account.html

After payment:
Join the server you want to upgrade and run /premium activate.
```

## Comandos de deploy no VPS

Para atualizar o bot no VPS:

```bash
ssh vozen@vps.example.com
cd /home/vozen/discord-bot-Vozen
git pull --ff-only
npm ci
npm run build
```

Para reiniciar o servico, e necessario root/sudo. Como o utilizador `vozen` nao tem `sudo` sem password, foi usado root na consola Hetzner:

```bash
systemctl restart vozen.service
systemctl status vozen.service --no-pager -l
```

Se o servico estiver parado:

```bash
systemctl start vozen.service
```

Ver logs:

```bash
journalctl -u vozen.service --since '10 minutes ago' --no-pager
```

Ver logs recentes:

```bash
journalctl -u vozen.service --no-pager | tail -120
```

## Servico systemd

Servico:

```text
/etc/systemd/system/vozen.service
```

Conteudo observado:

```ini
[Unit]
Description=Vozen Discord TTS bot
After=network.target

[Service]
Type=simple
User=vozen
WorkingDirectory=/home/vozen/discord-bot-Vozen
ExecStart=/usr/bin/npm run start:prod
Restart=on-failure
RestartSec=5
Environment=NODE_ENV=production
Environment=PATH=/usr/bin:/usr/local/bin:/bin

[Install]
WantedBy=multi-user.target
```

Nota: se o processo filho `dist/index.js` recebe SIGTERM e fecha com codigo 0, o wrapper pode nao reiniciar. Nessa situacao, usar:

```bash
systemctl start vozen.service
```

ou:

```bash
systemctl restart vozen.service
```

## Alteracoes feitas no site

Foram feitos varios commits e pushes para atualizar o site online em GitHub Pages.

Commits principais:

```text
0f9d847 Enable site Discord premium login
2935cfa Protect premium account page behind login
5149bc9 Redesenha portal de conta premium
576c913 Ajusta foto de perfil no portal
9898d4b Corrige estado de login na homepage
a8de2f8 Evita flash do login na navbar
97d45db Adiciona copia do Discord ID
9c0de0d Reformula cabecalho da conta premium
181e6ca Preenche pagina de conta
449cf88 Corrige fallback do Piper para Google
```

Alteracoes de UX:

- Login Discord no site.
- `account.html` so fica acessivel depois de login.
- Navbar mostra avatar/nome quando o utilizador ja esta logado.
- Corrigido flash em que o botao voltava a "Log in" por meio segundo.
- Pagina de conta mostra avatar grande, nome, Discord ID copiavel, Premium e Plus.
- Discord ID fica desfocado por defeito e revela/copia ao clicar.
- Cabecalho deixou de repetir "Premium" varias vezes:

```text
Account
Your benefits
Membership status
```

- Pagina de conta foi preenchida com rail lateral:
  - `3 server licenses`
  - `Personal Plus`
  - links para planos e suporte

Cache-busting usado por ficheiro fisico:

```text
site/css/main-v15.css -> site/css/main-v16.css
site/js/main-v21.js -> site/js/main-v22.js
```

GitHub Pages publica `site-dist/`, gerado por:

```bash
npm run build:site
```

Ver deploy:

```bash
gh run list --workflow pages.yml --limit 3
```

## Correcao do bot nao falar

Sintoma:

```text
o bot nao esta a falar
```

Logs no VPS mostraram:

```text
[ERROR] [player] erro na sintese, item saltado: Error: Modelo Piper nao encontrado: models/es_ES-davefx-medium.onnx
```

Causa:

- O utilizador/mensagem estava a usar `engine=piper`.
- O VPS nao tem modelos `.onnx` em `./models`.
- O `PerUserEngineRouter` enviava Piper direto e a falha fazia o player saltar a fala.

Correcao implementada:

Ficheiro:

```text
src/tts/perUserRouter.ts
```

Agora:

- `engine=kokoro` continua a ir para Kokoro.
- `engine=google` ou ausente vai para Google/gTTS.
- `engine=piper` tenta Piper.
- Se Piper falhar, faz fallback para Google/gTTS e regista warning.

Log esperado apos correcao:

```text
[WARN] [tts] Piper falhou para es_ES-davefx-medium; fallback para Google: Modelo Piper nao encontrado: models/es_ES-davefx-medium.onnx
```

Teste adicionado:

```text
tests/perUserRouter.test.ts
```

Comando executado:

```bash
npm test -- tests/perUserRouter.test.ts
npm run build
```

Resultado:

```text
8 tests passed
tsc OK
```

Commit:

```text
449cf88 Corrige fallback do Piper para Google
```

Deploy no VPS:

```bash
cd /home/vozen/discord-bot-Vozen
git pull --ff-only
npm ci
npm run build
systemctl start vozen.service
```

Estado confirmado:

```text
vozen.service active/running
online como Vozen#2281
commit 449cf88
```

## Avisos e proximos passos

### Node version

No VPS:

```text
node v20.20.2
npm 10.8.2
```

Durante `npm ci`, aparece aviso:

```text
@discordjs/voice@0.19.2 required node >=22.12.0
current node v20.20.2
```

O bot arrancou mesmo assim, mas convem planear upgrade para Node 22 LTS no VPS.

### Piper models

O VPS nao tem `.onnx` em `./models`, por isso aparecem avisos:

```text
[index] nenhum modelo .onnx em ./models — so vozes gTTS ficarao disponiveis.
```

Com a correcao, isto ja nao deve deixar o bot mudo, mas users que escolheram Piper vao cair para Google.

Opcoes futuras:

1. Instalar modelos Piper no VPS.
2. Remover/ocultar opcao Piper quando nao houver modelos.
3. Manter fallback atual e considerar isto aceitavel.

### Ko-fi token

O token foi partilhado no chat durante a configuracao. Por higiene de seguranca, o ideal seria:

1. Gerar novo token no Ko-fi.
2. Atualizar `KOFI_WEBHOOK_TOKEN` no VPS.
3. Reiniciar `vozen.service`.

Nao e obrigatorio se o dono decidiu deixar assim, mas e recomendado.

### Owner grant

Logs atuais:

```text
[owner] OWNER_GUILD_ID não definido — /vozen-grant não é registado.
```

Se for necessario usar `/vozen-grant`, configurar no `.env`:

```env
OWNER_GUILD_ID=<guild id>
OWNER_ID=<discord user id>
```

Depois:

```bash
systemctl restart vozen.service
```

## Comandos uteis para outro agente

Entrar no VPS:

```bash
ssh -i ~/.ssh/id_ed25519 vozen@vps.example.com
```

Ver estado:

```bash
systemctl status vozen.service --no-pager -l
```

Ver logs:

```bash
journalctl -u vozen.service --since '10 minutes ago' --no-pager
```

Atualizar codigo:

```bash
cd /home/vozen/discord-bot-Vozen
git pull --ff-only
npm ci
npm run build
```

Confirmar API:

```bash
curl -i https://api.example.com/api/me/premium
```

Confirmar site:

```bash
curl -L -H 'Cache-Control: no-cache' 'https://bot.example.com/account.html?deploy=check'
```

Ver Pages:

```bash
gh run list --workflow pages.yml --limit 3
```
