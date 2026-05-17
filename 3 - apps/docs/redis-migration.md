# Redis on ops3 — deploy and migration

## Deploy (empty or after you placed `dump.rdb`)

- Set `REDIS_PASSWORD` in [operations_3/.env](../.env) (same as source Redis when migrating).
- From repo: `cd operations_3 && ./scripts/deploy-redis.sh`

**Data directory (host)**: `/data/projects/redis/data` → mounted as `/data` in the container. Ownership: UID/GID **999** (`redis` in `redis:alpine`).

## Public host port (optional)

After `./scripts/deploy-redis.sh` with compose that publishes **`6379:6379`**, run **`./scripts/open-redis-public-firewall.sh`** on your Mac (uses `.env` SSH vars) so UFW allows **6379/tcp** from anywhere. Same caveat as Postgres: strong **`REDIS_PASSWORD`**; Redis has no TLS in this stack.

## Consumers

- **Business Tinder** (only deployed project using Redis): in deploy `.env` on the VDS use `REDIS_HOST=redis`, `REDIS_PASSWORD=<same as redis stack>`, `REDIS_DB=1`, then redeploy with `deployed_projects/business-tinder/deploy-bot.sh`.
- **ai-antispam** does not use Redis; ignore stale `REDIS_*` in its `.env`.

## Migrate `dump.rdb` from operations VDS (94.250.254.232)

A single `dump.rdb` is a **full-instance** snapshot (logical DBs 0–15). Restoring it copies every DB that existed on the old server.

1. **Stop writers** — Stop `business-tinder-bot` on ops3 (and anything else using the old Redis) so FSM keys are not written during the copy.
2. **Snapshot on source** — On the operations host:
   `redis-cli -a '<password>' SAVE`
   or `BGSAVE` and wait until the save completes (`INFO persistence`).
   Default file is usually `/var/lib/redis/dump.rdb`.
3. **Copy to ops3** — e.g. `scp root@94.250.254.232:/var/lib/redis/dump.rdb /tmp/dump.rdb` on ops3 (adjust user/host).
4. **Install the file** — If Redis **never** ran: put `dump.rdb` in `/data/projects/redis/data/` and `chown 999:999 /data/projects/redis/data/dump.rdb`, then run `./scripts/deploy-redis.sh`.
   If Redis **already** ran: `cd /data/projects/redis && docker compose down`, replace `/data/projects/redis/data/dump.rdb`, fix ownership `chown 999:999 ...`, then `docker compose up -d`.
5. **Verify** — `docker exec redis redis-cli --no-auth-warning -a '<password>' -n 1 DBSIZE` and `INFO keyspace`.
6. **Point the bot** — Business Tinder deploy `.env`: `REDIS_HOST=redis`, matching `REDIS_PASSWORD`, `REDIS_DB=1`; run `./deploy-bot.sh`.
7. **Decommission old Redis** — After verification, stop/disable `redis-server` on the old VDS if nothing else uses it.

### Automated cleanup (legacy operations VDS)

When SSH to `94.250.254.232` works from your machine:

```bash
cd operations
CONFIRM_REMOVE_REDIS=yes ./scripts/remove-redis-on-legacy-vds.sh
```

This stops and disables `redis-server`, purges Debian packages (`redis-server`, `redis-tools`), and removes `/var/lib/redis`, `/etc/redis`, `/var/log/redis`. Uses `REMOTE_HOST_IP` / `REMOTE_USER` from [operations/.env](../../operations/.env) unless you set `REDIS_LEGACY_HOST` / `REDIS_LEGACY_USER`.

`SHUTDOWN` via `redis-cli` alone is not enough if **systemd** restarts the service — you still need `systemctl disable --now` or the script above.

Plan a short maintenance window for steps 1–6.

**Optional lower downtime**: temporary `REPLICAOF` from the new instance to the old one, then `REPLICAOF NO ONE`, only if ops3 can reach the old Redis port and you accept the extra networking setup.

## Image

- `redis:alpine` — smallest official variant; tracks current stable Redis on `docker compose pull`.
