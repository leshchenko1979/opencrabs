# operations_4 — n8n VDS

Lightweight repo for the **n8n host** (`REMOTE_HOST_IP` in `.env`).

**Stack here**: **Caddy** (TLS termination) + **n8n** (workflow automation on `127.0.0.1:5678`) + **Picoclaw** (AI agent gateway `:18790` + WebUI `:18809`).

## Quick start

1. `cp .env.example .env` — SSH host configuration.
2. `cd ../scripts && ./diagnostics-unified.sh` — run diagnostics across all boxes.
3. `cd ../scripts && ./cleanup-unified.sh` — cleanup all boxes.

## Docs

| Doc | Purpose |
|-----|---------|
| [docs/services.md](docs/services.md) | n8n, Caddy configuration |
| [docs/n8n-migration.md](docs/n8n-migration.md) | Historical: legacy n8n migration |

## Reference

- [services/n8n/docker-compose.yml](services/n8n/docker-compose.yml) — reference only (not used for production n8n). All operations use unified scripts from the repo root `scripts/` directory.
