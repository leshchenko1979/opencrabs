#!/usr/bin/env bash
# Daily backup from apps + n8n + hermes to BACKUP_ROOT (Mac + external volume).
# Usage: from this directory:
#   ./backup-all.sh
# Requires: bash, ssh, tar, gzip; Full Disk Access for cron if BACKUP_ROOT is on a volume.
# SSH uses ~/.ssh/config host aliases: apps, n8n, hermes

set -euo pipefail

ENV_FILE="${HOME}/.env/vds-backup.env"

# Source env file if vars are missing (allows cron without inline env vars)
if [[ -z "${BACKUP_ROOT:-}" ]] || [[ -z "${POSTGRES_PASSWORD:-}" ]]; then
  if [[ -f "${ENV_FILE}" ]]; then
    set -a; source "${ENV_FILE}"; set +a
  fi
fi

: "${BACKUP_ROOT:?Set BACKUP_ROOT env var}"
: "${POSTGRES_PASSWORD:?Set POSTGRES_PASSWORD env var}"
: "${REDIS_PASSWORD:?Set REDIS_PASSWORD env var}"
GATUS_EXTERNAL_TOKEN="${GATUS_EXTERNAL_TOKEN:-}"

START_EPOCH="$(date +%s)"

SSH_OPTS=( -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=20 )
MIN_SIZE=100  # minimum expected bytes for a non-empty backup file

log() {
  echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*"
}

backup_failed() {
  log "ERROR: $*"
  FAILED=1
}

DAY="$(date '+%Y-%m-%d')"
DEST="${BACKUP_ROOT}/full-${DAY}"
LOG_FILE="${BACKUP_ROOT}/backup-${DAY}_$(date '+%H%M%S').log"

mkdir -p "${DEST}/apps" "${DEST}/n8n" "${DEST}/hermes"

FAILED=0

# Tee both stdout and stderr to log file, then wait for tee to finish
exec > >(tee -a "$LOG_FILE") 2>&1
PID=$!

log "=== VDS backup started (apps + n8n + hermes) ==="
log "Destination: ${DEST}"

log "1. apps: PostgreSQL pg_dumpall..."
if ssh "${SSH_OPTS[@]}" apps "docker exec -e PGPASSWORD='${POSTGRES_PASSWORD}' postgres pg_dumpall -U postgres" 2>/dev/null \
  > "${DEST}/apps/postgres-backup.sql"; then
  actual_size=$(wc -c < "${DEST}/apps/postgres-backup.sql")
  if [[ "${actual_size}" -lt "${MIN_SIZE}" ]]; then
    backup_failed "postgres-backup.sql is only ${actual_size} bytes — likely empty"
  fi
else
  backup_failed "pg_dumpall failed on apps"
fi

log "2. apps: Redis RDB snapshot..."
if ! ssh "${SSH_OPTS[@]}" apps "docker exec redis redis-cli -a '${REDIS_PASSWORD}' SAVE" >/dev/null 2>&1; then
  backup_failed "Redis SAVE failed on apps"
fi

if ssh "${SSH_OPTS[@]}" apps "docker exec redis cat /data/dump.rdb" > "${DEST}/apps/redis-backup.rdb" 2>/dev/null; then
  actual_size=$(wc -c < "${DEST}/apps/redis-backup.rdb")
  if [[ "${actual_size}" -lt "${MIN_SIZE}" ]]; then
    backup_failed "redis-backup.rdb is only ${actual_size} bytes — likely empty"
  fi
else
  backup_failed "Redis dump.rdb fetch failed"
fi

log "3. apps: /data/projects (excluding postgres/redis data dirs)..."
if ! ssh "${SSH_OPTS[@]}" apps 'tar czf - --exclude=projects/postgres/data --exclude=projects/redis/data -C /data projects' 2>/dev/null \
  > "${DEST}/apps/projects-configs.tar.gz"; then
  backup_failed "apps projects tar failed"
fi

log "4. n8n: /var/lib/n8n..."
if ! ssh "${SSH_OPTS[@]}" n8n 'tar czf - --exclude=.cache --exclude=.n8n/cache --exclude="*.log" -C /var/lib n8n' \
  > "${DEST}/n8n/n8n-data.tar.gz" 2>/dev/null; then
  backup_failed "n8n tar failed"
fi

log "5. n8n: /etc/n8n.env..."
if ! ssh "${SSH_OPTS[@]}" n8n 'cat /etc/n8n.env' > "${DEST}/n8n/n8n.env" 2>/dev/null; then
  backup_failed "n8n.env fetch failed"
fi

log "6. hermes: /root/.hermes + /root/.opencrabs (excluding sessions, snapshots, bin, logs, *.bak)..."
# sessions/ = ephemeral per-conversation; state-snapshots/ = temp; bin/ = redeployed from install
# *.log, opencrabs logs = noise; *.bak, ops.bak.* = auto-generated edit backups
if ! ssh "${SSH_OPTS[@]}" hermes 'tar czf - \
  --exclude=sessions/ \
  --exclude=state-snapshots/ \
  --exclude=bin/ \
  --exclude="*.log" \
  --exclude=state.db-shm \
  --exclude=state.db-wal \
  --exclude=.venv/ \
  --exclude=.cache \
  -C /root .hermes' 2>/dev/null > "${DEST}/hermes/hermes-data.tar.gz"; then
  backup_failed "hermes .hermes tar failed"
fi

if ! ssh "${SSH_OPTS[@]}" hermes 'tar czf - \
  --exclude="*.bak" \
  --exclude="profiles/ops.bak.*" \
  --exclude="logs/" \
  --exclude=opencrabs.db-wal \
  -C /root .opencrabs' 2>/dev/null > "${DEST}/hermes/opencrabs-data.tar.gz"; then
  backup_failed "hermes .opencrabs tar failed"
fi

{
  echo "backup_date=${DAY}"
  echo "generated=$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
  echo "status=${FAILED:-0}"
  echo ""
  echo "files:"
  find "${DEST}" -type f -exec ls -lh {} \;
} > "${DEST}/manifest.txt"

log "7. Sizes:"
du -sh "${DEST}/apps" "${DEST}/n8n" "${DEST}/hermes" "${DEST}" || true

if [[ "${FAILED}" -eq 0 ]]; then
  log "8. Retention..."
  find "${BACKUP_ROOT}" -maxdepth 1 -type d -name 'full-*' -mtime "+${RETENTION_DAYS:-7}" -print -exec rm -rf {} \; 2>/dev/null || true
  find "${BACKUP_ROOT}" -maxdepth 1 -type f -name 'backup-*.log' -mtime "+${RETENTION_DAYS:-7}" -delete 2>/dev/null || true

  log "9. Gatus heartbeat..."
  if [[ -n "${GATUS_EXTERNAL_TOKEN}" ]]; then
    duration=$(( $(date +%s) - START_EPOCH ))
    curl -sf -X POST \
      "https://gatus.l1979.ru/api/v1/endpoints/infra_mac-backup/external?success=true&duration=${duration}ms" \
      -H "Authorization: Bearer ${GATUS_EXTERNAL_TOKEN}" \
      -m 10 || log "Gatus heartbeat failed (non-fatal)"
  fi

  log "=== Backup completed successfully ==="
else
  log "=== Backup FAILED — retention skipped, leaving ${DEST} for inspection ==="
fi

exit ${FAILED}