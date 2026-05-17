#!/usr/bin/env bash
# Deploy ops brain templates to hermes OpenCrabs profile (run from Mac).
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HERMES_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
BRAIN_SRC="${HERMES_DIR}/opencrabs-brain"
REPO_ROOT="$(cd "${HERMES_DIR}/.." && pwd)"
# shellcheck source=/dev/null
source "${REPO_ROOT}/scripts/ssh-vds-host.sh"

HERMES_IP="${REMOTE_HOST_IP:-132.243.213.9}"
HERMES_USER="${REMOTE_USER:-root}"
HERMES_PORT="${REMOTE_SSH_PORT:-18718}"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/id_ed25519}"
OPS_DIR="/root/.opencrabs/profiles/ops"

for f in SOUL.md IDENTITY.md USER.md AGENTS.md MEMORY.md; do
  scp -P "$HERMES_PORT" -i "$SSH_KEY" "${BRAIN_SRC}/${f}" \
    "${HERMES_USER}@${HERMES_IP}:${OPS_DIR}/${f}"
done

echo "Brain deployed to ${OPS_DIR}. Restart: ssh hermes 'systemctl restart opencrabs-ops'"
