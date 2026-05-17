# Progress log

## 2026-04-01

- **3 - apps**: `scripts/deploy-vpn-mtg.sh` — wrapper to `../2 - VPN/scripts/deploy-mtg.sh`. VS Code task **Deploy VPN mtg (2 - VPN)** in `.vscode/tasks.json` (`build`, **not** default).

- **2 - VPN `deploy-mtg.sh`**: Optional **`MTG_DEPLOY_HOST`** after `server.conf` when canonical IP has no SSH. Redeployed mtg to **104.128.131.166:8443** (91.217.76.69 still closes SSH from here).

- **2 - VPN `config/server.conf`**: `SSH_HOST` corrected to **91.217.76.69** (was wrong FirstByte IP). `docs/services.md` VPN mtg line updated to match. Re-run `2 - VPN/scripts/deploy-mtg.sh` if mtg should live on this host (previous deploy targeted the old IP).

- **mtg on VPN VDS** (`2 - VPN`): Added `services/mtg/` (compose **8443:443** — xray keeps 443), `scripts/deploy-mtg.sh` (scp uses `-o Port=`; compose or **docker run** fallback), `.env.example` + `.gitignore`. Deployed to **104.128.131.166**. README + VS Code task. `3 - apps/docs/services.md` — VPN alternate line.

- **mtg**: Pinned image to `nineseconds/mtg:2` in `services/mtg/docker-compose.yml` (was `:latest`, different digest than 2.2.6). Redeployed on VDS. `docs/services.md` — client import + mobile “Connecting…” notes.

- **Diagnostics + fixes**: Disk 86% → 75% (pruned images/build cache, ~1GB reclaimed). pdf-extract was down: found `docker stop` at 08:40 Mar 31 (restart-manager stopped, exit 2), started — now healthy. Pruned 3 ghost containers (68MB reclaimed, including redevest-crm-test which self-restarts via Sablier).
- **Deployment compliance skill**: Added Sablier container protection to SKILL.md — any prune operation must skip `label=sablier.managed=true` containers (`redevest-crm`, `redevest-crm-test`, `pdf-extract`). Added diagnostic check for Sablier-managed container health to `scripts/diagnostics.sh`.

## 2026-03-30

- **Remove business-tinder-new.l1979.ru**: Dropped DNS reference from `scripts/diagnostics.sh`, simplified `docs/services.md` Business Tinder section (single URL, removed cutover instructions), cleaned up comment in `services/traefik/config/business-tinder.yml`.

## 2026-03-28

- **Retire pdf2image (pdf-extract replaces)**: `services/traefik/config/sablier-apps.yml` — removed `pdf2image` routers, backend, `sablier-pdf2image`. `docs/services.md` — Sablier bullets + dropped PDF2Image section. `scripts/diagnostics.sh` — container grep + DNS list no longer reference pdf2image.

- **VDS cleanup (ops3)**: `docker compose down --rmi local` + removed `/data/projects/pdf2image`. `./scripts/deploy-traefik.sh` — synced Traefik/Sablier. `bash scripts/cleanup-docker.sh` — image prune + builder prune `until=168h` (~50MB reclaimed). `chmod +x scripts/cleanup-docker.sh`.

## 2026-03-27

- **README**: Removed dead `docs/operations.md` link; quick start + doc table point to `docs/services.md` / `docs/maintenance.md` only.

- **Diagnostics + docs alignment**: `scripts/diagnostics.sh` — DNS hint → `docs/services.md`; DNS adds `ai-antispam.l1979.ru`, `business-tinder.l1979.ru`, `business-tinder-new.l1979.ru`; summary warns if **sablier** or **postgres** down. `README.md` — drop broken `docs/monitoring.md` row; maintenance table blurb. `docs/maintenance.md` — Diagnostics section (script, log, `--no-log`, scope).

- **run-diagnostics command + README**: Default `./scripts/diagnostics.sh` (writes `logs/diagnostics.log`); agents analyze from that file to avoid truncated stdout. `.cursor/commands/run-diagnostics.md`, `README.md` Diagnostics section.

- **RAM-optimization.mdc**: Rewrote **Swap usage** + red flags — swap size is capacity/OOM buffer, not RAM; extra swap can postpone OOM but does not fix chronic thrashing; prefer RAM/load fixes for steady high swap.

## 2026-03-24

- **Traefik deploy + verify (ops3)**: `./scripts/deploy-traefik.sh` → healthy Traefik + Sablier; file routers include `*-bots-no-wake@file`. Bot UA: `pdf2image` / `test.redevest-crm` while **exited** → **502**, stayed exited; browser UA → test CRM **200** + container **running** (~17s wake).
- **Sablier / bots**: `services/traefik/config/sablier-apps.yml` — priority-150 `*-bots-no-wake` routers (crawlers skip Sablier middleware → no scale-up of managed app containers). **DRY**: top-of-file Go-template `$crawlerUA`; `_x` anchors — merge `sablierUrl`/`sessionDuration` into all Sablier plugin blocks; bot routers share `entryPoints`/`tls`/`priority` aliases (paerser drops `_x` after YAML resolves anchors). `docs/services.md`, `.cursor/rules/sablier-container-config.mdc`.

## 2026-03-23

- **Sablier CRM verify (ops3)**: `services/traefik/config/sablier-apps.yml` matches `/data/projects/traefik/` on server. Prod/test containers `redevest-crm` / `redevest-crm-test`: `container_name` + `traefik-public`, `sablier.managed=true`; prod also `sablier.enabled=true` + `traefik.enable=false`. Live test: `docker stop` both → `curl` to `/health` via Traefik → 200 ~10s (wake). Scale-down: temp `sessionDuration: 90s` on test middlewares only → container exited ~90s idle; config restored to 30m. Optional: add `sablier.enabled=true` to test compose for parity (source not in this repo).
- **mtg memory**: `services/mtg/docker-compose.yml` — `mem_limit: 64m`, `mem_reservation: 16m`. Docs: `docs/services.md`. Redeploy: `./scripts/deploy-mtg.sh`.
- **Redis public**: `services/redis/docker-compose.yml` — `6379:6379`, `--bind 0.0.0.0`; `scripts/open-redis-public-firewall.sh` (UFW 6379 v4+v6, clear DOCKER-USER 6379 if present). Deployed + firewall on ops3. Docs: `docs/services.md`, `docs/redis-migration.md`, `scripts/deploy-redis.sh` echo.
- **Postgres firewall public**: `docker-postgres-n8n-firewall` disabled on ops3; DOCKER-USER 5432 rules cleared via `services/postgres/iptables-docker-postgres-clear.sh`; UFW `5432/tcp` ALLOW Anywhere (v4+v6). Script: `scripts/open-postgres-public-firewall.sh`. Docs: `docs/services.md`, `docs/postgres-migration.md`; `install-postgres-n8n-firewall.sh` points to open script.
- **Postgres remote listen**: `services/postgres/docker-compose.yml` — `listen_addresses=*`, `hba_file` → mounted `pg_hba.conf` (SCRAM for `0.0.0.0/0` / `::/0`, trust loopback). `scripts/deploy-postgres.sh` copies `pg_hba.conf` to `/data/projects/postgres/`. Docs: `docs/services.md`, `docs/postgres-migration.md` (any-IP vs n8n DOCKER-USER).

## 2026-03-21

- **Mac workstation backup**: `../mac-workstation-backup/` — daily script for ops3+ops4, `docs/maintenance.md` Backups section links to it.
- **Disk on ops3**: Remote cleanup — `journalctl --vacuum-size=100M`, btmp truncate, `docker builder prune -af` (~985MB), `apt-get clean`; removed unused images `mtproxy/mtproxy`, `amnezia-awg`, `alpine`, `curlimages/curl`, duplicate `business-tinder-business-tinder-notifications`. `/` went **86% → ~73%** used (~1.4G → ~2.6G avail on 9.8G vol).

- **Diagnostics UX**: `scripts/diagnostics.sh` — `--no-log`, `~` expansion for `SSH_KEY` from `.env`, SSH via array + quoted `-i` (no `ConnectTimeout` — fixes macOS OpenSSH parse error). README.md restored (overview + links). `.cursor/commands/run-diagnostics.md` uses `./scripts/diagnostics.sh --no-log`.
- **mtproxy.l1979.ru removed**: DNS record dropped (not needed). Docs: `docs/services.md` mtg — IP + SNI `github.com` only; `scripts/diagnostics.sh` DNS list no longer includes `mtproxy.l1979.ru`.
- **pdf2image TLS**: Prior LE failures hit old IP `94.250.254.232` (DNS). After A → `144.31.188.163`, `docker restart traefik` on ops3; `https://pdf2image.l1979.ru` now presents Let's Encrypt (R12), `curl` to `/health` verifies without `-k`.

## 2026-03-20

- **Cursor command**: `.cursor/commands/run-diagnostics.md` — point to `./scripts/diagnostics.sh` and `.env` SSH vars; document `logs/diagnostics.log` vs one-shot `ssh` when avoiding log file; README fallback to `docs/services.md`.
- **Postgres closed on host**: Removed `5432:5432` from `services/postgres/docker-compose.yml`; `./scripts/deploy-postgres.sh` applied on ops3. UFW rule 5432 from legacy IP removed; stripped `operations_3 postgres` blocks from `/etc/ufw/after.rules` and `after6.rules` (backups `.bak.<ts>` on VDS). Docs: `docs/services.md`, `docs/postgres-migration.md` (close-public section + cutover).
- **Disk + swap**: Ran cleanup-logs.sh (~200MB), cleanup-docker.sh, docker builder prune -af (~1.1GB), docker volume prune. Swap increased 512MB→1GB via new /swapfile2; fstab updated. docs/maintenance.md swap section.
- **Sablier + CRM + pdf2image on ops3**: Traefik `docker-compose.yml` — Sablier plugin + `sablier` container; `config/sablier-apps.yml` routes for `redevest-crm.ru`, `test.redevest-crm.ru`, `pdf2image.l1979.ru`. Repo: `deployed_projects/redevest-crm` `.env.prod` / `.env.test` → `VDS_HOST=144.31.188.163`, `DATABASE_URL` `@postgres:5432` (test DB `redevest_crm_test`). `pdf2image` compose: file-only routing (`traefik.enable=false`); Dockerfile `UV_COMPILE_BYTECODE=0` for slow VPS builds. `cleanup-docker.sh`: drop `image prune -af`; added `scripts/docker-cleanup-safe.sh` (copy of operations); `docs/maintenance.md`. `diagnostics.sh`: `check_sablier_stack`, DNS checks for CRM + pdf2image. **Sender**: `.env` `REMOTE_HOST=144.31.188.163`; `deploy.sh` uses `SSH_OPTS=-o ControlMaster=no` — run deploy when SSH works; install cron on ops3.
- **DNS (manual)**: A records `redevest-crm.ru`, `test.redevest-crm.ru`, `pdf2image.l1979.ru` → `144.31.188.163`. **2026-03-20 resume**: Legacy Traefik no longer serves Sablier routes; CRM/pdf2image containers removed on legacy — until reg.ru A records point to ops3, those hostnames will not reach the live stacks.
- **DNS cutover (user)**: reg.ru A records updated to ops3. Verified from tooling: `curl -k -H 'Host: redevest-crm.ru' https://144.31.188.163/health` → 200; same for `pdf2image.l1979.ru`. Public resolver view can lag; use `dig +short … @8.8.8.8` to confirm propagation. First real clients to ops3 trigger LE if certs not yet issued for new routing.
- **Diagnostics redis fix**: Summary used `docker inspect redis -q` which fails on older Docker (unknown shorthand `-q`). Switched to `docker ps --filter name=redis -q` to match traefik/mtg checks. `scripts/diagnostics.sh`.
- **PostgreSQL migration**: `pg_dumpall` from `94.250.254.232` to `/data/projects/postgres/pg_global.sql` on ops3, `services/postgres/docker-compose.yml` (`postgres:16-alpine`, `traefik-public`, `/data/projects/postgres/data`), `scripts/deploy-postgres.sh`, `.env` / `.env.example` `POSTGRES_*`, restore via `psql` (no `ON_ERROR_STOP`; skip `DROP` errors on `postgres` role). UFW allow 5432 from legacy + DOCKER-USER allowlist + IPv6 drop on 5432 (`/etc/ufw/after.rules`, `after6.rules`). ai-antispam `PG_HOST=postgres`. Repo `redevest-crm/.env.prod` → `144.31.188.163`; **apply same `DATABASE_URL` on legacy server** when SSH works and restart CRM. Docs: `docs/postgres-migration.md`, `docs/services.md`; operations `docs/services.md`; ai-antispam `memory-bank/techContext.md`. `scripts/diagnostics.sh` `check_postgres`.
- **Redis cutover**: Replicated from `94.250.254.232` into `/data/projects/redis/data/dump.rdb`, `./scripts/deploy-redis.sh`, verified DB1; Business Tinder `/data/projects/business-tinder/.env` `REDIS_HOST=redis`, bot healthy. Old host: `SHUTDOWN SAVE` via redis-cli but **systemd restarted** `redis-server` — disable with `systemctl disable --now redis-server` when SSH to operations VDS works.
- **Redis on ops3**: `services/redis/docker-compose.yml` (`redis:alpine`, `traefik-public`, `/data/projects/redis/data`), `scripts/deploy-redis.sh`, `.env.example` `REDIS_PASSWORD`, `docs/services.md`, `docs/redis-migration.md` (RDB migration + Business Tinder `REDIS_HOST=redis`), `scripts/diagnostics.sh` `check_redis`.

- **AI Antispam**: Moved from operations VDS to operations_3. Traefik `config/ai-antispam.yml` → `ai-antispam.l1979.ru` → `ai-antispam:8080`. App: `TELEGRAM_WEBHOOK_URL`, file-only routing (labels removed). Postgres on this host (`PG_HOST=postgres`). Repo: `deployed_projects/ai-antispam`, `operations` Traefik/docs cleanup.
- **Business Tinder**: bot + Traefik (`-new` SAN) on 144.31.188.163; webhook env; operations VDS stack removed. `docs/services.md`.
- **fast-mcp-telegram**: leftover project dir + image removed from operations VDS (94.250.254.232).

## 2026-03-17

- **mtg migration**: Replaced mtproxy with mtg (9seconds/mtg). services/mtg/, scripts/deploy-mtg.sh, scripts/mtg-connected.sh. Requires MTG_SECRET in .env.

## 2026-03-16

- **fast-mcp-telegram migration**: Moved from operations VDS (94.250.254.232) to operations_3 (144.31.188.163). Added services/traefik/config/fast-mcp-telegram.yml, docs/services.md TG MCP Server entry. Sessions migrated via rsync. Deploy: Traefik config, then fast-mcp-telegram deploy-mcp.sh from project with updated .env (VDS_HOST, VDS_PROJECT_PATH, DOMAIN). DNS cutover: tg-mcp.l1979.ru A record → 144.31.188.163.

## 2026-03-15

- **Log cleanup**: scripts/cleanup-logs.sh, docs/maintenance.md; cron (Sun 03:30) for journald + btmp; ran once — ~1.1GB reclaimed
- **Telegram proxy SNI**: Changed HostSNI and -D from 1c.ru to github.com
- **Diagnostics**: Removed n8n from DNS check (n8n runs on another server); docs/maintenance.md
- **Cleanup**: Added scripts/cleanup-docker.sh, docs/maintenance.md (Docker prune, cron schedule)
- **SSH hardening**: Added scripts/configure-ssh-keys-only.sh — PermitRootLogin prohibit-password, PasswordAuthentication no

## 2026-03-05

- **Telegram proxy**: Traefik TCP router (HostSNI passthrough), deploy script

## 2026-03-03

- **ai-gateway**: Moved to l1979.ru; diagnostics now verifies n8n + ai-gateway DNS
- **ai-gateway cert**: Updated DOMAIN in /root/services/ai-gateway/.env (redevest.ru → l1979.ru), recreated container → LE cert obtained

## 2026-02-16

- **Documentation rules**: Copied from operations repo → `.cursor/rules/memory-bank.mdc`, `documentation-structure.mdc`, `docs/`
- **SSH + tasks**: SSH key auth for 144.31.188.163; `.vscode/tasks.json` with SSH connect and Run Diagnostics tasks
- **Docker**: Docker 29.2.1 and Docker Compose 5.0.2 on VDS
- **Traefik**: Installed at `/data/projects/traefik/`, HTTP→HTTPS redirect, Let's Encrypt ACME
- **Diagnostics**: `scripts/diagnostics.sh` — system, Docker, Traefik, DNS check, OOM, logs, network
- **Fixes**: `scripts/fix-dns.sh`, `.env`, DNS check in diagnostics, `docs/dns-setup.md`; UFW enabled (22,80,443,8080)
- **ai-gateway LE cert**: acme.json reset + Traefik restart → Let's Encrypt cert obtained
