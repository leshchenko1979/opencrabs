#!/bin/bash
# Install /usr/local/bin/host-diag on all VDS boxes.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../../.." && pwd)"
# shellcheck source=/dev/null
source "${REPO_ROOT}/scripts/ssh-vds-host.sh"

REMOTE_USER="${REMOTE_USER:-root}"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/id_ed25519}"
SSH_OPTS=(-i "$SSH_KEY" -o BatchMode=yes -o ConnectTimeout=15)

HOST_DIAG="${SCRIPT_DIR}/host-diag"

deploy_to() {
  local name="$1"
  local target="$2"
  echo "=== ${name} (${target}) ==="
  scp "${SSH_OPTS[@]}" "$HOST_DIAG" "${REMOTE_USER}@${target}:/usr/local/bin/host-diag"
  ssh "${SSH_OPTS[@]}" "${REMOTE_USER}@${target}" 'chmod +x /usr/local/bin/host-diag'
  ssh "${SSH_OPTS[@]}" "${REMOTE_USER}@${target}" '/usr/local/bin/host-diag; echo exit:$?'
}

deploy_to "box2-vpn" "$(vds_ssh_connect_host "${BOX2_IP:-104.128.131.166}")"
deploy_to "box3-apps" "$(vds_ssh_connect_host "${BOX3_IP:-144.31.188.163}")"
deploy_to "box4-n8n" "$(vds_ssh_connect_host "${BOX4_IP:-2.27.120.75}")"
deploy_to "box5-hermes" "$(vds_ssh_connect_host "${BOX5_IP:-132.243.213.9}")"

echo "=== host-diag deployed on all boxes ==="
