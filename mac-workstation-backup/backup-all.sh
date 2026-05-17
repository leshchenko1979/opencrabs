#!/usr/bin/env bash
# Daily backup from apps + n8n + hermes to BACKUP_ROOT (Mac + external volume).
# Usage: from this directory:
#   ./backup-all.sh
# Requires: bash, ssh, tar, gzip; Full Disk Access for cron if BACKUP_ROOT is on a volume.
# SSH uses ~/.ssh/config host aliases: apps, n8n, hermes

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

: "${BACKUP_ROOT:?Set BACKUP_ROOT env var}"
: "${POSTGRES_PASSWORD:?Set POSTGRES_PASSWORD env var}"
: "${REDIS_PASSWORD:?Set REDIS_PASSWORD env var}"

SSH_OPTS=( -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=20 )

log() {
  echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*"
}

DAY="$(date '+%Y-%m-%d')"
DEST="${BACKUP_ROOT}/full-${DAY}"
LOG_FILE="${BACKUP_ROOT}/backup-${DAY}_$(date '+%H%M%S').log"

mkdir -p "${DEST}/apps" "${DEST}/n8n" "${DEST}/hermes"
exec > >(tee -a "$LOG_FILE") 2>&1

log "=== VDS backup started (apps + n8n + hermes) ==="
log "Destination: ${DEST}"

log "1. apps: PostgreSQL pg_dumpall..."
ssh "${SSH_OPTS[@]}" apps "docker exec -e PGPASSWORD='${POSTGRES_PASSWORD}' postgres pg_dumpall -U postgres" 2>/dev/null \
  > "${DEST}/apps/postgres-backup.sql"

log "2. apps: Redis RDB snapshot..."
ssh "${SSH_OPTS[@]}" apps "docker exec redis redis-cli -a '${REDIS_PASSWORD}' SAVE" >/dev/null 2>&1
ssh "${SSH_OPTS[@]}" apps "docker exec redis cat /data/dump.rdb" > "${DEST}/apps/redis-backup.rdb"

log "3. apps: /data/projects (excluding postgres/redis data dirs)..."
ssh "${SSH_OPTS[@]}" apps 'tar czf - --exclude=projects/postgres/data --exclude=projects/redis/data -C /data projects' 2>/dev/null \
  > "${DEST}/apps/projects-configs.tar.gz"

log "4. n8n: /var/lib/n8n..."
ssh "${SSH_OPTS[@]}" n8n 'tar czf - --exclude=.cache --exclude=.n8n/cache --exclude="*.log" -C /var/lib n8n' \
  > "${DEST}/n8n/n8n-data.tar.gz"

log "5. n8n: /etc/n8n.env..."
ssh "${SSH_OPTS[@]}" n8n 'cat /etc/n8n.env' > "${DEST}/n8n/n8n.env"

log "6. hermes: /root/.hermes (excluding venv, cache, logs)..."
# tar warns "file changed as we read it" (active DB writes) — non-fatal, archive is valid
# || true ensures non-zero tar exit (from file-change warnings) doesn't abort with set -e
ssh "${SSH_OPTS[@]}" hermes 'tar czf - \
  --exclude=.cache \
  --exclude="*.log" \
  --exclude=state.db-shm \
  --exclude=state.db-wal \
  --exclude=.venv/ \
  --exclude=bin/ \
  -C /root .hermes' 2>/dev/null > "${DEST}/hermes/hermes-data.tar.gz" || true

{
  echo "backup_date=${DAY}"
  echo "generated=$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  echo ""
  echo "files:"
  find "${DEST}" -type f -exec ls -lh {} \;
} > "${DEST}/manifest.txt"

log "7. Sizes:"
du -sh "${DEST}/apps" "${DEST}/n8n" "${DEST}/hermes" "${DEST}" || true

log "8. Retention..."
find "${BACKUP_ROOT}" -maxdepth 1 -type d -name 'full-*' -mtime "+${RETENTION_DAYS:-7}" -print -exec rm -rf {} \; 2>/dev/null || true
find "${BACKUP_ROOT}" -maxdepth 1 -type f -name 'backup-*.log' -mtime "+${RETENTION_DAYS:-7}" -delete 2>/dev/null || true

log "=== Backup completed successfully ==="