# Service Configurations

Services deployed on VDS (144.31.188.163).

## Infrastructure

### Redis
- **Purpose**: In-memory cache / FSM storage (Business Tinder)
- **Image**: `redis:alpine`
- **Location**: `/data/projects/redis/` (compose + `.env`; data bind mount `/data/projects/redis/data` → `/data`)
- **Network**: `traefik-public` (DNS **`redis`**, port **6379**); host **`6379:6379`** published. **Firewall**: `./scripts/open-redis-public-firewall.sh` — UFW **6379/tcp** Anywhere (v4+v6). Remote clients: host **144.31.188.163**, **AUTH** with `REDIS_PASSWORD` (no TLS on plain Redis — use VPN/SSH tunnel if you need encryption).
- **Auth**: `requirepass` from `REDIS_PASSWORD` in deploy `.env`; **`--bind 0.0.0.0`** so the published port accepts non-loopback connections
- **Memory**: 128MB `maxmemory`, `allkeys-lru`; container limit 160MB / reservation 64MB, 0.25 CPU
- **Deploy**: `./scripts/deploy-redis.sh` from `operations_3` (requires `REDIS_PASSWORD` in local `.env`)
- **Migration / cutover**: [redis-migration.md](redis-migration.md)

### PostgreSQL
- **Purpose**: Primary Postgres for ai-antispam and **Redevest CRM** on this VDS (`DATABASE_URL` host **`postgres`** on `traefik-public`)
- **Image**: `postgres:16-alpine` (major via `POSTGRES_MAJOR` in deploy `.env`)
- **Location**: `/data/projects/postgres/` (compose + `.env`; data bind mount `/data/projects/postgres/data` → `/var/lib/postgresql/data`)
- **Network**: `traefik-public` (DNS name **`postgres`**, port **5432**); host **`5432:5432`** published. **Firewall**: `./scripts/open-postgres-public-firewall.sh` — UFW `5432/tcp` from anywhere + no DOCKER-USER drop on 5432. To restrict to **n8n** only again (operations_5 IP): [postgres-migration.md](postgres-migration.md#n8n-on-operations_4--allow-postgres-only-from-that-host) and `./scripts/install-postgres-n8n-firewall.sh`.
- **Listen / remote TCP**: `listen_addresses=*`; `pg_hba.conf` (`services/postgres/pg_hba.conf`) — SCRAM for remote (`0.0.0.0/0`, `::/0`), trust loopback.
- **Auth**: `POSTGRES_USER` / `POSTGRES_PASSWORD` in deploy `.env` (must match restored cluster during migration)
- **Memory / CPU**: 384MB limit, 128MB reservation, 0.5 CPU; `shared_buffers=64MB`, `max_connections=100`
- **Deploy**: `./scripts/deploy-postgres.sh` from `operations_3`
- **Firewall**: After migration, remove legacy **5432** UFW rule and `operations_3 postgres` blocks in `/etc/ufw/after.rules` / `after6.rules` if present — see [postgres-migration.md](postgres-migration.md#close-public-port-5432-post-migration)
- **Migration / cutover**: [postgres-migration.md](postgres-migration.md)

### Traefik
- **Purpose**: Reverse proxy, SSL termination, automatic Let's Encrypt
- **URL**: Dashboard at `http://144.31.188.163:8080/dashboard/`
- **Location**: `/data/projects/traefik/`
- **Ports**: 80, 443, 8080
- **Memory**: 192MB limit, 96MB reservation
- **SSL**: HTTP→HTTPS redirect, ACME resolver `le`
- **Dynamic files**: `middlewares.yml`, `sablier-apps.yml`, `business-tinder.yml`, `ai-antispam.yml`, `fast-mcp-telegram.yml`, `tls.yml`
- **Sablier**: `docker-compose.yml` includes `experimental.plugins.sablier` and **`sablier`** service (`sablierapp/sablier`); scale-to-zero for CRM via `sablier-apps.yml`

### Sablier (scale-to-zero)
- **Container**: `sablier` on `traefik-public`, Docker provider, talks to Traefik plugin on `http://sablier:10000`
- **Managed containers**: `redevest-crm`, `redevest-crm-test` (labels `sablier.managed=true`)
- **Routes**: `services/traefik/config/sablier-apps.yml` — `redevest-crm.ru`, `test.redevest-crm.ru`
- **Crawler / bot traffic (no scale-up of managed apps)**: Routers `*-bots-no-wake` at priority **150** — same paths as Sablier routes but `HeaderRegexp` on `User-Agent` for known crawlers; **no** Sablier **middleware**, so idle **Sablier-managed containers** (CRM, …) are not started by those requests (502 if backend down). The Sablier **service** itself keeps running. Crawler regexp is defined once at the top of `sablier-apps.yml` (Traefik Go template). Shared Sablier plugin fields use YAML merge from `_x` (unknown root key stripped by Traefik after anchor resolution).
- **Deploy Traefik stack**: `./scripts/deploy-traefik.sh` from `operations_3`

### Redevest CRM
- **URLs**: `https://redevest-crm.ru`, `https://test.redevest-crm.ru`
- **Location**: `/data/projects/redevest-crm` (prod), `/data/projects/redevest-crm/test` (staging compose)
- **DB**: `postgres:5432` — prod DB `redevest_crm`, test DB `redevest_crm_test` (create if missing: `CREATE DATABASE redevest_crm_test;`)
- **Deploy**: `deployed_projects/redevest-crm/scripts/deploy-crm.sh`, `scripts/deploy-test.sh`

### pdf-extract API
- **URL**: `https://pdf-extract.l1979.ru`
- **Location**: `/data/projects/pdf-extract/`
- **Routing**: File provider in `sablier-apps.yml` (`/health`, `/v1/*`); middleware `pdf-extract-buffering@file` (32 MiB body); container `traefik.enable=false`
- **DNS**: A/AAAA `pdf-extract.l1979.ru` → same host as Traefik (e.g. `144.31.188.163`)
- **Deploy**: `deployed_projects/pdf-extract/deploy.sh` (GitHub Actions CI/CD)
- **Env on container**: `PUBLIC_BASE_URL=https://pdf-extract.l1979.ru`
- **Always-on**: Not managed by Sablier scale-to-zero

### Sender (cron)
- **Location**: `/data/projects/sender/`
- **Schedule**: `CRON_TZ=Europe/Moscow`, hourly `9–21` → `./run.sh` (Docker image `sender`)
- **Deploy**: `deployed_projects/sender/deploy.sh` (`REMOTE_HOST=144.31.188.163`)

### AI Antispam
- **URL**: `https://ai-antispam.l1979.ru`
- **Purpose**: Anti-spam Telegram bot webhook (AI moderation)
- **Location**: `/data/projects/ai-antispam/`
- **Traefik**: `config/ai-antispam.yml` → `ai-antispam:8080` on `traefik-public` (file-only routing)
- **Endpoints**: `POST /process-tg-updates`, `GET /health`
- **Telegram webhook**: `setWebhook` on startup from env `TELEGRAM_WEBHOOK_URL` (default `https://ai-antispam.l1979.ru/process-tg-updates`)
- **Database**: PostgreSQL on this VDS — `PG_HOST=postgres` on `traefik-public` (deploy `.env` on `/data/projects/ai-antispam/`)

### Business Tinder
- **URL**: `https://business-tinder.l1979.ru`
- **Traefik**: `config/business-tinder.yml` → `business-tinder-bot:8080` on `traefik-public`
- **Telegram webhook**: bot calls `setWebhook` on startup from env `TELEGRAM_WEBHOOK_URL`. Docker labels removed — routing is file-only.
- **Redis**: use `REDIS_HOST=redis` on this VDS (see **Redis** above); `REDIS_DB=1` for FSM

### ai-gateway
- **URL**: `https://ai-gateway.l1979.ru`
- **Image**: ai-gateway-ai-gateway
- **Port**: 8080 (internal)
- **Memory**: 64MB limit, 32MB reservation
- **SSL**: Let's Encrypt via Traefik (certResolver: le)


### TG MCP Server
- **URL**: `https://tg-mcp.l1979.ru`
- **Purpose**: MCP server for Telegram API integration
- **Location**: `/data/projects/fast-mcp-telegram/`
- **Memory**: 256MB limit, 64MB reservation
- **Deploy**: From fast-mcp-telegram project: `./scripts/deploy-mcp.sh` (set VDS_HOST, VDS_PROJECT_PATH, DOMAIN in .env)
- **Features**: HTTP transport, MTProto API over HTTP, auth via Bearer token
