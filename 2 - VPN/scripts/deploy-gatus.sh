#!/bin/bash
# Deploy Gatus to VPN box — run from "2 - VPN": ./scripts/deploy-gatus.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
GATUS_DIR="${PROJECT_ROOT}/services/gatus"
DEPLOY_PATH="/data/projects/gatus"
RENDER_SCRIPT="${GATUS_DIR}/scripts/render-gatus-config.py"
CONFIG_SRC="${GATUS_DIR}/config/config.yaml"
GATUS_ENV="${GATUS_DIR}/.env.gatus"
GATUS_SSH_KEY="${GATUS_SSH_KEY:-$HOME/.ssh/id_ed25519}"

# shellcheck source=/dev/null
source "$(cd "${PROJECT_ROOT}/.." && pwd)/scripts/ssh-vds-host.sh"
SSH_ALIAS="$(vds_ssh_connect_host "${SSH_ALIAS:-vpn}")"
SSH_OPTS=(-o BatchMode=yes)

if [[ ! -f "$CONFIG_SRC" ]]; then
  echo "Error: missing $CONFIG_SRC"
  exit 1
fi

if [[ ! -f "$GATUS_ENV" ]]; then
  echo "Error: missing $GATUS_ENV — copy from .env.gatus.example and set GATUS_EXTERNAL_TOKEN"
  exit 1
fi

if ! grep -qE '^GATUS_EXTERNAL_TOKEN=.+' "$GATUS_ENV"; then
  echo "Error: GATUS_EXTERNAL_TOKEN is empty or missing in $GATUS_ENV"
  exit 1
fi

echo "Deploying Gatus to ${SSH_ALIAS} (${DEPLOY_PATH})..."

TUNNEL_DEPLOY="${GATUS_DIR}/scripts/deploy-gatus-a2a-tunnel.sh"
if [[ -x "$TUNNEL_DEPLOY" ]]; then
  SSH_ALIAS="$SSH_ALIAS" GATUS_SSH_KEY="$GATUS_SSH_KEY" bash "$TUNNEL_DEPLOY"
fi

TMP_CONFIG="$(mktemp)"
trap 'rm -f "$TMP_CONFIG"' EXIT

export GATUS_SSH_KEY
python3 "$RENDER_SCRIPT" "$CONFIG_SRC" >"$TMP_CONFIG"

ssh "${SSH_OPTS[@]}" "$SSH_ALIAS" \
  "docker network create traefik-public 2>/dev/null || true; \
   mkdir -p ${DEPLOY_PATH}/config/keys ${DEPLOY_PATH}/data; \
   chmod 777 ${DEPLOY_PATH}/data 2>/dev/null || true"

scp "${SSH_OPTS[@]}" \
  "${GATUS_DIR}/docker-compose.yml" \
  "${GATUS_DIR}/.env.gatus.example" \
  "${SSH_ALIAS}:${DEPLOY_PATH}/"

scp "${SSH_OPTS[@]}" \
  "$GATUS_ENV" \
  "${SSH_ALIAS}:${DEPLOY_PATH}/.env.gatus.local"

scp "${SSH_OPTS[@]}" "$TMP_CONFIG" \
  "${SSH_ALIAS}:${DEPLOY_PATH}/config/config.yaml"

ssh "${SSH_OPTS[@]}" "$SSH_ALIAS" \
  "DEPLOY_PATH='${DEPLOY_PATH}' bash -s" <<'REMOTE'
set -euo pipefail
cd "${DEPLOY_PATH}"
install -m 600 .env.gatus.local .env.gatus
rm -f .env.gatus.local
grep -qE '^GATUS_EXTERNAL_TOKEN=.+' .env.gatus \
  || { echo "ERROR: GATUS_EXTERNAL_TOKEN missing in .env.gatus"; exit 1; }
REMOTE

ssh "${SSH_OPTS[@]}" "$SSH_ALIAS" \
  "cd ${DEPLOY_PATH} && docker compose pull && docker compose up -d"

echo ""
echo "Gatus deployed at ${DEPLOY_PATH}"
echo "UI: https://gatus.l1979.ru"
