# Hospedar o Vozen

O caminho suportado é Docker no VPS com o runtime Rust:

```sh
git clone https://github.com/Rexy40407/vozen.git
cd vozen
cp .env.rust.prod.example .env.rust.prod
docker compose -f docker-compose.rust.prod.yml up -d --build
docker compose -f docker-compose.rust.prod.yml logs -f vozen
```

Preenche `DISCORD_TOKEN`, `CLIENT_ID`, os segredos Top.gg/Stripe/OAuth/Premium e
os caminhos de voz em `.env.rust.prod`. Mantém `rust-data` num disco persistente;
é onde vivem SQLite e caches. Consulta [DEPLOY.md](../DEPLOY.md) para backup,
healthcheck e rollback.

Não uses `npm start`, `npm run register`, `Dockerfile` ou os compose antigos: o
runtime Node/TypeScript foi removido da `main`. Para desenvolver, usa os gates
Rust e site descritos em [SELF-HOST.md](SELF-HOST.md).
