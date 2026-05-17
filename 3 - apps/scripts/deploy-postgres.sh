#!/bin/bash

# Deploy PostgreSQL to ops3 VDS (Docker, traefik-public; host 5432 published — firewall: open-postgres-public-firewall.sh vs install-postgres-n8n-firewall.sh)
# Run from operations_3: ./scripts/deploy-postgres.sh
#
# Prerequisites: POSTGRES_USER, POSTGRES_PASSWORD in .env (match legacy during migration).
# Optional: POSTGRES_MAJOR (default 16) — align with source server major version.

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
DEPLOY_PATH="/data/projects/postgres"

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

if [ -z "${POSTGRES_USER}" ] || [ -z "${POSTGRES_PASSWORD}" ]; then
    echo "Error: POSTGRES_USER and POSTGRES_PASSWORD must be set in .env"
    exit 1
fi

POSTGRES_MAJOR="${POSTGRES_MAJOR:-16}"

echo "Deploying PostgreSQL (${POSTGRES_MAJOR}-alpine) to ${REMOTE_USER}@${REMOTE_HOST_IP} (ssh: ${VDS_SSH_TARGET})..."

ssh "${REMOTE_USER}@${VDS_SSH_TARGET}" "docker network create traefik-public 2>/dev/null || true"

ssh "${REMOTE_USER}@${VDS_SSH_TARGET}" "mkdir -p ${DEPLOY_PATH}/data"

scp "${PROJECT_ROOT}/services/postgres/docker-compose.yml" \
    "${PROJECT_ROOT}/services/postgres/pg_hba.conf" \
    "${REMOTE_USER}@${VDS_SSH_TARGET}:${DEPLOY_PATH}/"

ENV_TMP=$(mktemp)
chmod 600 "${ENV_TMP}"
{
    printf 'POSTGRES_USER=%s\n' "${POSTGRES_USER}"
    printf 'POSTGRES_PASSWORD=%s\n' "${POSTGRES_PASSWORD}"
    printf 'POSTGRES_MAJOR=%s\n' "${POSTGRES_MAJOR}"
} >"${ENV_TMP}"
scp "${ENV_TMP}" "${REMOTE_USER}@${VDS_SSH_TARGET}:${DEPLOY_PATH}/.env"
rm -f "${ENV_TMP}"

ssh "${REMOTE_USER}@${VDS_SSH_TARGET}" "cd ${DEPLOY_PATH} && docker compose pull && docker compose up -d"

echo ""
echo "PostgreSQL deployed at ${DEPLOY_PATH}. Containers on traefik-public: host postgres, port 5432."
echo "Host port 5432 is published. Admin: docker exec -it postgres psql -h 127.0.0.1 -U ${POSTGRES_USER}"
echo "Public 5432: ./scripts/open-postgres-public-firewall.sh — docs/postgres-migration.md"
