# Deploy do Vozen Rust

O runtime oficial é a imagem `Dockerfile.rust` e o compose de produção
`docker-compose.rust.prod.yml`. O antigo runtime Node/TypeScript não faz parte da
imagem nem do processo de deploy.

## VPS

```sh
git clone --branch migration/vozen-rust --single-branch https://github.com/Rexy40407/vozen.git
cd vozen
cp .env.rust.prod.example .env.rust.prod
# preencher DISCORD_TOKEN, CLIENT_ID e integrações Top.gg/Ko-fi/API necessárias
docker compose -f docker-compose.rust.prod.yml up -d --build
docker compose -f docker-compose.rust.prod.yml logs -f vozen
```

O volume `./rust-data:/data` contém a base SQLite e as caches. Faça backup de
`rust-data/tts.db` antes de cada atualização; o deploy oficial também cria um
backup automático antes de recriar o contentor. `restart: unless-stopped` mantém
o bot ativo depois de crashes e reboots, desde que o serviço Docker arranque no
boot.

## Verificação

```sh
docker compose -f docker-compose.rust.prod.yml ps
docker compose -f docker-compose.rust.prod.yml logs --tail 100 vozen
curl -fsS http://127.0.0.1:3001/health
```

Confirme nos logs `gateway ... Ready` e uma resposta HTTP 200. O site público e
a API (`https://vozen.org` e `https://api.vozen.org/health`) são serviços
separados, mas continuam a usar os contratos Premium/OAuth, Top.gg e Ko-fi
mantidos nos crates Rust.

## Atualizações e rollback

The production compose file enables `PUBLIC_STATUS_ENABLED=true` automatically.
Once Caddy points `api.vozen.org` at port 3001, `https://vozen.org/status`
reads the live bot, database, and voice-provider state. The public endpoint only
exposes coarse aggregate states; it never exposes tokens, messages, or internals.

1. Confirme a revisão aprovada pela CI e que o checkout está limpo antes de
   atualizar os ficheiros; preserve configurações privadas e dados.
2. Na instalação encriptada, desbloqueie o volume e execute o verificador do host.
3. Execute `VOZEN_COMPOSE_PROJECT=vozen-rust-prod bash scripts/deploy-rust-vps.sh`.
   O script constrói com o bot online, faz backup SQLite consistente e só depois
   para o supervisor systemd para recriar o contentor. Reinicia a supervisão
   antes de verificar saúde e mantém a imagem anterior para rollback.
4. Em falha, o script tenta restaurar a imagem anterior e a supervisão. Não
   restaura uma base antiga sobre dados mais recentes. Recuperação de dados
   exige uma decisão separada do operador.

Antes de publicar, a CI executa os contratos JSON, canários, testes/clippy Rust,
os testes do site e a construção da imagem. A branch `legacy-typescript` mantém
o snapshot de recuperação do runtime antigo.

## Encrypted production storage with manual unlock

The installed host procedures in `deploy/encryption/` operate on the existing
LUKS device `/dev/mapper/vozen-data` mounted at `/srv/vozen-secure`. They do not
format or migrate disks. Do not install them blindly on other hosts.

For this installation, Docker uses `restart=no` and the encrypted runtime
systemd service supervises the container after operator unlock. After reboot,
the bot remains stopped until its key is supplied through stdin. Keep keys
outside the VPS and Git, with an independently tested recovery copy. Two DPAPI
files tied to the same Windows profile are not an independent recovery backup.

Install the mount unit with its escaped name `srv-vozen\x2dsecure.mount`.
The runtime orders itself after the mount but does not require it at boot,
avoiding a dependency wait for an absent mapper. The host guard checks the
mount, mapper, data marker, database and expected data-directory symlink.

Unlock and run `/usr/local/sbin/vozen-data-guard` before publishing. Use the
actual project `VOZEN_COMPOSE_PROJECT=vozen-rust-prod`, also the script default,
not the historical `vozen-prod`. The deployment script exports the disabled Docker restart
policy when `/etc/vozen/encryption-enabled` exists. For manual Compose commands,
pass `--env-file .env.rust.prod`: a service's `env_file` does not provide Compose
interpolation variables. Preserve `VOZEN_REQUIRE_ENCRYPTED_DATA=1` in private
configuration. Never start a second production gateway.

The GitHub deployment workflow checks the encryption guard before changing
configuration or the checkout. It uses `vozen-rust-prod`; stale CI events only
inspect the running container and health. The script requires passwordless
operator access to the installed guard and runtime systemd unit on this host.
Build or backup failures leave the supervisor running. Replacement and rollback
restore supervision; inability to restore it remains a deployment failure.

Preserve ACLs and xattrs when copying data (`rsync -aAX`) and test access with
the actual container UID before cutover. Use the SQLite online backup script
instead of copying only an active WAL database's main file. Verify integrity,
foreign keys, HTTP health, gateway readiness and the encrypted backup target.
Never restore an old database snapshot over newer writes without an explicit
recovery decision; image rollback and database restore are separate operations.

Encryption of active data does not encrypt historical plaintext copies,
provider backups or old disk blocks from a copy-based migration. Retire exact
historical copies only after independent recovery verification and explicit
owner authorization. Do not claim complete coverage while those copies remain.

## Giveaway votes for the three Vozen bots

The giveaway card links to TTS, Helper and Music on top.gg. Each project contributes
at most one entry per person per giveaway. Further votes for that same project do
not add tickets. Existing historical tickets are retained during the additive
`giveaway_vote.bot_id` upgrade and default to the preceding TTS source.

The existing TTS webhook remains `/webhook/topgg` with `TOPGG_WEBHOOK_SECRET`.
Configure the additional provider-generated secrets only when their listings are
available and the corresponding URLs are saved on top.gg:

- Helper: `https://api.vozen.org/webhook/topgg/helper`, `TOPGG_HELPER_WEBHOOK_SECRET`.
- Music: `https://api.vozen.org/webhook/topgg/music`, `TOPGG_MUSIC_WEBHOOK_SECRET`.

Each route verifies its own signature and exact Discord application ID. Missing
secrets leave those routes absent. Helper and Music votes only enter giveaways;
they do not grant the separate TTS Plus vote promotion. The additional buttons stay hidden by default. Set
`VOZEN_GIVEAWAY_THREE_BOTS_ENABLED=true` only after both listings are approved
and both signed webhooks pass a provider test. Both secrets must be non-empty
before the extra buttons can appear. Restart through the encrypted runtime
service; existing open announcements are refreshed automatically. Publish the
reviewed three-button private-panel HTML at the same time. Endpoint preparation
and deployment are safe while this flag remains false.
