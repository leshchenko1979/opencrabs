#!/bin/bash
# Deploy Gatus to VPN box — run from "2 - VPN": ./scripts/deploy-gatus.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
GATUS_DIR="${PROJECT_ROOT}/services/gatus"
DEPLOY_PATH="/data/projects/gatus"
RENDER_SCRIPT="${GATUS_DIR}/scripts/render-gatus-config.py"
CONFIG_SRC="${GATUS_DIR}/config/config.yaml"

if [[ ! -f "${PROJECT_ROOT}/.env" ]]; then
  echo "Error: .env not found at ${PROJECT_ROOT}/.env"
  exit 1
fi

# shellcheck source=/dev/null
source "${PROJECT_ROOT}/.env"
SERVERS_REPO="$(cd "${PROJECT_ROOT}/.." && pwd)"
# shellcheck source=/dev/null
source "${SERVERS_REPO}/scripts/ssh-vds-host.sh"

REMOTE_HOST_IP="${REMOTE_HOST_IP:-vpn}"
REMOTE_USER="${REMOTE_USER:-root}"
VDS_SSH_TARGET="$(vds_ssh_connect_host "$REMOTE_HOST_IP")"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/id_ed25519}"
GATUS_SSH_KEY="${GATUS_SSH_KEY:-$SSH_KEY}"

if [[ -z "${REMOTE_HOST_IP}" ]] || [[ -z "${REMOTE_USER}" ]]; then
  echo "Error: REMOTE_HOST_IP or REMOTE_USER not set"
  exit 1
fi

if [[ ! -f "$CONFIG_SRC" ]]; then
  echo "Error: missing $CONFIG_SRC"
  exit 1
fi

echo "Deploying Gatus to ${REMOTE_USER}@${VDS_SSH_TARGET} (${DEPLOY_PATH})..."

TMP_CONFIG="$(mktemp)"
trap 'rm -f "$TMP_CONFIG"' EXIT

export GATUS_SSH_KEY
python3 "$RENDER_SCRIPT" "$CONFIG_SRC" >"$TMP_CONFIG"

ssh -i "$SSH_KEY" -o BatchMode=yes "${REMOTE_USER}@${VDS_SSH_TARGET}" \
  "docker network create traefik-public 2>/dev/null || true"
ssh -i "$SSH_KEY" -o BatchMode=yes "${REMOTE_USER}@${VDS_SSH_TARGET}" \
  "mkdir -p ${DEPLOY_PATH}/config/keys ${DEPLOY_PATH}/data"
ssh -i "$SSH_KEY" -o BatchMode=yes "${REMOTE_USER}@${VDS_SSH_TARGET}" \
  "chmod 777 ${DEPLOY_PATH}/data 2>/dev/null || true"

scp -i "$SSH_KEY" -o BatchMode=yes \
  "${GATUS_DIR}/docker-compose.yml" \
  "${GATUS_DIR}/.env.gatus.example" \
  "${REMOTE_USER}@${VDS_SSH_TARGET}:${DEPLOY_PATH}/"

scp -i "$SSH_KEY" -o BatchMode=yes "$TMP_CONFIG" \
  "${REMOTE_USER}@${VDS_SSH_TARGET}:${DEPLOY_PATH}/config/config.yaml"

# Preserve remote .env.gatus if present; seed from example only when missing
ssh -i "$SSH_KEY" -o BatchMode=yes "${REMOTE_USER}@${VDS_SSH_TARGET}" bash -s <<REMOTE
set -euo pipefail
cd ${DEPLOY_PATH}
if [[ ! -f .env.gatus ]]; then
  if [[ -f .env ]]; then
    cp .env .env.gatus
  else
    cp .env.gatus.example .env.gatus
    echo "WARNING: created .env.gatus from example — set tokens on server"
  fi
  chmod 600 .env.gatus
fi
if [[ -f /etc/gatus-bridge.env ]] && ! grep -q '^GATUS_BRIDGE_SECRET=' .env.gatus 2>/dev/null; then
  secret=$(grep '^GATUS_BRIDGE_SECRET=' /etc/gatus-bridge.env | cut -d= -f2-)
  echo "GATUS_BRIDGE_SECRET=${secret}" >> .env.gatus
fi
REMOTE

ssh -i "$SSH_KEY" -o BatchMode=yes "${REMOTE_USER}@${VDS_SSH_TARGET}" \
  "cd ${DEPLOY_PATH} && docker compose pull && docker compose up -d"

echo ""
echo "Gatus deployed at ${DEPLOY_PATH}"
echo "UI: https://gatus.l1979.ru"
