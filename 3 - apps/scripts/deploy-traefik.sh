#!/bin/bash

# Deploy Traefik to apps VDS — use `ssh apps` Host from ~/.ssh/config when IP is ops3.
# Run from repo root: ./scripts/deploy-traefik.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
DEPLOY_PATH="/data/projects/traefik"

# Load environment
if [ ! -f "${PROJECT_ROOT}/.env" ]; then
    echo "Error: .env not found at ${PROJECT_ROOT}/.env"
    exit 1
fi

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

echo "Deploying Traefik to ${REMOTE_USER}@${REMOTE_HOST_IP} (ssh: ${VDS_SSH_TARGET}) (${DEPLOY_PATH})..."

# Create traefik-public network
ssh "${REMOTE_USER}@${VDS_SSH_TARGET}" "docker network create traefik-public 2>/dev/null || true"

# Create directories
ssh "${REMOTE_USER}@${VDS_SSH_TARGET}" "mkdir -p ${DEPLOY_PATH}/{letsencrypt,config,logs} && \
    touch ${DEPLOY_PATH}/letsencrypt/acme.json && \
    chmod 600 ${DEPLOY_PATH}/letsencrypt/acme.json"

# Copy config
echo "Copying config..."
scp -r "${PROJECT_ROOT}/services/traefik/config/"* "${REMOTE_USER}@${VDS_SSH_TARGET}:${DEPLOY_PATH}/config/"
scp "${PROJECT_ROOT}/services/traefik/docker-compose.yml" "${REMOTE_USER}@${VDS_SSH_TARGET}:${DEPLOY_PATH}/"

# Deploy
echo "Pulling image and starting Traefik..."
ssh "${REMOTE_USER}@${VDS_SSH_TARGET}" "cd ${DEPLOY_PATH} && docker compose pull && docker compose down && docker compose up -d"

# Wait for healthy
echo "Waiting for Traefik to become healthy..."
MAX_WAIT=120
WAIT_TIME=0
WAIT_INCREMENT=5

while [ $WAIT_TIME -lt $MAX_WAIT ]; do
    if ssh "${REMOTE_USER}@${VDS_SSH_TARGET}" "docker ps --filter name=traefik --format '{{.Status}}' | grep -q healthy && docker ps --filter name=sablier --format '{{.Names}}' | grep -q sablier"; then
        echo "Traefik is healthy and Sablier is running!"
        break
    fi
    echo "Waiting... ($WAIT_TIME/$MAX_WAIT s)"
    sleep $WAIT_INCREMENT
    WAIT_TIME=$((WAIT_TIME + WAIT_INCREMENT))
done

if [ $WAIT_TIME -ge $MAX_WAIT ]; then
    echo "Error: Traefik failed to become healthy"
    ssh "${REMOTE_USER}@${VDS_SSH_TARGET}" "docker logs traefik 2>&1 | tail -20"
    exit 1
fi

echo "Traefik + Sablier deployed. Dashboard: http://${REMOTE_HOST_IP}:8080/dashboard/"
