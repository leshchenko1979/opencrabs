# Service configurations — operations_4 (n8n VDS)

**Host**: `2.27.120.75` (see `.env` `REMOTE_HOST_IP`)

## Disk Management

**Disk is tight on this box** — 9.8G total, ~76% used as of 2026-05-04. n8n's `node_modules` alone occupies ~2.2G.

### Root cause of 2026-04-14 outage
Disk filled to 100% → `SQLITE_IOERR` on every n8n start → crash loop. The main DB (`/var/lib/n8n/.n8n/database.sqlite`, 35MB) survived intact; the 0-byte `/var/lib/n8n/database.sqlite` is a decoy path, not used.

### WAL file danger
If n8n crashes or is killed while writing, the WAL (`database.sqlite-wal`) can grow to hundreds of MB and cause `SQLITE_FULL` even with free disk space. Recovery procedure:
```bash
systemctl stop n8n
sqlite3 /var/lib/n8n/.n8n/database.sqlite "PRAGMA wal_checkpoint(TRUNCATE);"
systemctl start n8n
```

### @opentelemetry is not removable
`@sentry/node` in n8n auto-initializes OpenTelemetry instrumentation. Removing `@opentelemetry` packages causes `Cannot find module '@opentelemetry/api'` / `@opentelemetry/instrumentation-http' errors on next start. The full 326MB `@opentelemetry` stack must remain.

### Cleanup done (100% → 76%)
- Removed old kernel: `linux-image/modules/headers-6.8.0-107-generic`
- Removed `snapd` (unused on this box)
- Cleared `/var/log/btmp*` (SSH brute-force logs, >80MB)
- Truncated active `/var/log/syslog` (101MB)
- Deleted old n8n event logs: `n8nEventLog-{1,2,3}.log`
- Set journal cap: `/etc/systemd/journald.conf` → `SystemMaxUse=100M`
- Cleared npm cache (`/root/.npm`) and system cache (`/root/.cache`) — freed ~1.2G
- Removed n8n corrupted install and reinstalled cleanly

### Future disk pressure targets
If disk fills again, in order of ease and safety:
1. `/var/log/btmp*` — grows fast from SSH brute-force bots; safe to truncate anytime
2. `/var/lib/n8n/.n8n/n8nEventLog-*.log` — rotated event logs; safe to delete old ones
3. `/var/log/syslog` — rotate with `logrotate -f /etc/logrotate.d/rsyslog`, then truncate `.1`
4. Journal: already capped at 100M; can vacuum with `journalctl --vacuum-time=7d` if archived journals accumulate

### Monitoring
Gatus (box 2) checks disk via SSH every 5 minutes. Alerts to Telegram when ≥ 90%.

## n8n

- **Role**: Workflow automation (migrated from legacy operations Docker deployment).
- **Runtime**: Node.js LTS + `n8n` (npm global), **systemd** unit `n8n`.
- **Current state** (2026-05-04): Healthy, 200 OK at `https://n8n.l1979.ru/healthz/readiness`. Disk 76% (2.4GB free).
- **Data directory**: `/var/lib/n8n` (`N8N_USER_FOLDER`) — populate via [n8n-migration.md](n8n-migration.md) from Docker volume `n8n_data` on the legacy host (not from `/root/services/n8n/data`; that path in old docs was inaccurate).
- **Secrets / env**: `/etc/n8n.env` (root-only). Mirror variables from [legacy compose](../services/n8n/docker-compose.yml); recover missing secrets with `docker inspect n8n` on the legacy server.
- **Code node / `require()`**: set `NODE_FUNCTION_ALLOW_BUILTIN=*` and `NODE_FUNCTION_ALLOW_EXTERNAL=js-yaml` (and restart `n8n`) so workflows can `require('js-yaml')`; the package ships with the global `n8n` install under its `node_modules`.
- **URL**: `https://n8n.l1979.ru` — DNS A → this VDS; `N8N_HOST` / `WEBHOOK_URL` / `N8N_EDITOR_BASE_URL` in `/etc/n8n.env` must match.
- **Reverse proxy**: **Caddy** terminates TLS and reverse-proxies to `127.0.0.1:5678` with WebSocket support. Set `N8N_PROTOCOL=https`, `N8N_PROXY_HOPS=1`, and public URLs in `/etc/n8n.env` to match the vhost. If you remove old names (e.g. staging) from DNS, delete those site blocks from `/etc/caddy/Caddyfile` and `systemctl reload caddy`.

## PostgreSQL (operations_3)

- **Host for n8n credentials**: `144.31.188.163`, port `5432` — do **not** use the hostname `postgres` (that only works inside Docker on ops3).
- **Firewall / deploy**: [operations_3 postgres-migration.md](../../3%20-%20apps/docs/postgres-migration.md) (n8n subsection).

## Caddy

- **Role**: TLS termination and reverse proxy for `n8n.l1979.ru` → `127.0.0.1:5678`
- **Config**: `/etc/caddy/Caddyfile`
- **Reload**: `systemctl reload caddy`
- **No other vhosts** — Hermes and OpenCrabs moved to box 5.

## Reference only

- [services/n8n/docker-compose.yml](../services/n8n/docker-compose.yml) — copied from `operations`; documents Traefik/Docker env parity. This VDS does **not** run that compose by default.
