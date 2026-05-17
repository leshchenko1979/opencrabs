# Mac workstation backup — apps + n8n + hermes

Daily backup to external volume (same pattern as former legacy `vds-daily-backup.sh`).

## Layout

- **This directory**: `mac-workstation-backup/` next to the server trees
- **Scripts**:
  - `backup-all.sh` — daily full backup (apps + n8n + hermes)

## Prerequisites

1. **SSH**: Must have working `apps`, `n8n`, `hermes` entries in `~/.ssh/config`
   - `apps` → 144.31.188.163
   - `n8n` → 2.27.120.75
   - `hermes` → 132.243.213.9:18718

2. **PostgreSQL / Redis passwords** — set as env vars at runtime:
   ```bash
   POSTGRES_PASSWORD=... REDIS_PASSWORD=... ./backup-all.sh
   ```

3. **External volume** mounted before cron runs; `BACKUP_ROOT` must exist and be writable (e.g. `/Volumes/leshchenko/vds-backups`)

4. **macOS Full Disk Access** for `/usr/sbin/cron` if backups go to a removable volume (System Settings → Privacy & Security → Full Disk Access)

5. **Optional wake**: `sudo pmset repeat wake MTWRFSU 02:55:00` so the Mac is awake before the backup window

## Cron

```cron
0 3 * * * caffeinate -i /path/to/servers/mac-workstation-backup/backup-all.sh
```

## Output structure

```
${BACKUP_ROOT}/
├── full-YYYY-MM-DD/
│   ├── manifest.txt
│   ├── apps/
│   │   ├── postgres-backup.sql
│   │   ├── redis-backup.rdb
│   │   └── projects-configs.tar.gz   # /data/projects minus postgres/redis data dirs
│   ├── n8n/
│   │   ├── n8n-data.tar.gz
│   │   └── n8n.env
│   └── hermes/
│       └── hermes-data.tar.gz        # /root/.hermes (excludes venv, bin, cache, logs)
└── backup-YYYY-MM-DD_HHMMSS.log
```

Retention: `full-*` and old `backup-*.log` files older than 7 days are removed on each successful run.

## Manual run

```bash
cd mac-workstation-backup
BACKUP_ROOT=/Volumes/leshchenko/vds-backups \
  POSTGRES_PASSWORD=... REDIS_PASSWORD=... \
  ./backup-all.sh
```

## Recovery procedures

### Before you restore

- Stop traffic to the affected host (DNS pause or maintenance page) if needed
- Restore to a **test** path or VM first when possible

### apps — PostgreSQL

1. Copy `apps/postgres-backup.sql` to the server (e.g. `/tmp/postgres-backup.sql`)
2. Ensure the `postgres` container is running and empty or disposable target DBs
3. Restore:

   ```bash
   docker exec -i -e PGPASSWORD="$POSTGRES_PASSWORD" postgres psql -U postgres < /tmp/postgres-backup.sql
   ```

4. Verify: `docker exec postgres psql -U postgres -c '\l'`

### apps — Redis

1. Stop consumers using Redis if needed
2. Stop the Redis container, replace data file:

   ```bash
   docker stop redis
   cp /tmp/redis-backup.rdb /data/projects/redis/data/dump.rdb
   chown 999:999 /data/projects/redis/data/dump.rdb
   docker start redis
   ```

3. Verify: `docker exec redis redis-cli -a "$REDIS_PASSWORD" PING`

### apps — Projects / Traefik configs

1. Extract on the server:

   ```bash
   mkdir -p /data
   tar xzf projects-configs.tar.gz -C /data
   ```

2. Reconcile with your repo: redeploy Traefik or apps from the server directories if compose files changed after the backup

3. Restart affected stacks: `docker compose up -d` in `/data/projects/traefik`, etc.

### n8n

1. Stop n8n: `systemctl stop n8n`
2. Restore data:

   ```bash
   rm -rf /var/lib/n8n/*
   tar xzf n8n-data.tar.gz -C /var/lib
   ```

3. Restore env: `cp n8n.env /etc/n8n.env && chmod 600 /etc/n8n.env`
4. Start: `systemctl start n8n` — **`N8N_ENCRYPTION_KEY` must match** the previous install or credentials store is unreadable

### hermes

1. Stop hermes: `systemctl stop hermes-gateway`
2. Restore data:

   ```bash
   rm -rf /root/.hermes/*
   tar xzf hermes-data.tar.gz -C /root
   ```

3. Restart: `systemctl start hermes-gateway`
4. **Important**: The venv at `/usr/local/hermes-agent/.venv/` is NOT backed up. If you need a full restore, re-create the venv:
   ```bash
   cd /usr/local/hermes-agent
   uv venv .venv
   uv pip install -e .
   ```

### Verification checklist

- apps: `curl` to Traefik routes or app `/health` URLs; Postgres clients connect; Redis `PING`
- n8n: n8n UI loads; run a trivial workflow; check webhooks
- hermes: `systemctl status hermes-gateway` is active; Telegram bot responds