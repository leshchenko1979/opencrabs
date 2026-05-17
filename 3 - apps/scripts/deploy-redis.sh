#!/bin/bash

# Deploy Redis to ops3 VDS (Docker, traefik-public; optional host 6379 — open-redis-public-firewall.sh)
# Run from operations_3: ./scripts/deploy-redis.sh
#
# Prerequisites: REDIS_PASSWORD in .env (match migrated RDB source password during cutover)

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
DEPLOY_PATH="/data/projects/redis"

if [ ! -f "${PROJECT_ROOT}/.env" ]; then
    echo "Error: .env not found at ${PROJECT_ROOT}/.env"
    exit 1
fi

# shellcheck source=/dev/null
source "${PROJECT_ROOT}/.env"
SERVERS_REPO="$(cd "${PROJECT_ROOT}/.." && pwd)"
# shellcheck source=/dev/null
source "${SERVERS_REPO}/scripts/ssh-vds-host.sh"
REMOTE_HOST_IP="${REMOTE_HOST_IP:-apps}"
REMOTE_USER="${REMOTE_USER:-root}"
VDS_SSH_TARGET="$(vds_ssh_connect_host "$REMOTE_HOST_IP")"

if [ -z "${REMOTE_HOST_IP}" ] || [ -z "${REMOTE_USER}" ]; then
    echo "Error: REMOTE_HOST_IP or REMOTE_USER not set"
    exit 1
fi

if [ -z "${REDIS_PASSWORD}" ]; then
    echo "Error: REDIS_PASSWORD not set in .env"
    exit 1
fi

echo "Deploying Redis to ${REMOTE_USER}@${REMOTE_HOST_IP} (ssh: ${VDS_SSH_TARGET})..."

ssh "${REMOTE_USER}@${VDS_SSH_TARGET}" "docker network create traefik-public 2>/dev/null || true"

ssh "${REMOTE_USER}@${VDS_SSH_TARGET}" "mkdir -p ${DEPLOY_PATH}/data"
# Official redis:alpine runs as user redis (UID 999) for /data writes
ssh "${REMOTE_USER}@${VDS_SSH_TARGET}" "chown 999:999 ${DEPLOY_PATH}/data 2>/dev/null || true"

scp "${PROJECT_ROOT}/services/redis/docker-compose.yml" "${REMOTE_USER}@${VDS_SSH_TARGET}:${DEPLOY_PATH}/"

ENV_TMP=$(mktemp)
chmod 600 "${ENV_TMP}"
printf 'REDIS_PASSWORD=%s\n' "${REDIS_PASSWORD}" >"${ENV_TMP}"
scp "${ENV_TMP}" "${REMOTE_USER}@${VDS_SSH_TARGET}:${DEPLOY_PATH}/.env"
rm -f "${ENV_TMP}"

ssh "${REMOTE_USER}@${VDS_SSH_TARGET}" "cd ${DEPLOY_PATH} && docker compose pull && docker compose up -d"

echo ""
echo "Redis deployed at ${DEPLOY_PATH}. Apps on traefik-public: REDIS_HOST=redis, REDIS_PASSWORD=<same as .env>"
echo "Host 6379: ./scripts/open-redis-public-firewall.sh — docs/redis-migration.md"
echo "Migration from operations VDS: see docs/redis-migration.md"
