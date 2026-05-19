#!/usr/bin/env bash
# Deploy ops brain templates to hermes OpenCrabs profiles (run from Mac).
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HERMES_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
BRAIN_SRC="${HERMES_DIR}/opencrabs-brain"
REPO_ROOT="$(cd "${HERMES_DIR}/.." && pwd)"

# shellcheck source=/dev/null
source "${REPO_ROOT}/scripts/hermes-ssh.sh"

hermes_ssh_init

OPS_DIR="/root/.opencrabs/profiles/ops"
DEFAULT_DIR="/root/.opencrabs"
DEFAULT_USER_SRC="${BRAIN_SRC}/DEFAULT_USER.md"
GUARD_SCRIPT="${SCRIPT_DIR}/opencrabs-guard-default-keys.sh"
REMOTE_GUARD="/tmp/opencrabs-guard-default-keys.sh"

if [[ ! -f "$GUARD_SCRIPT" ]]; then
  echo "ERROR: Missing $GUARD_SCRIPT" >&2
  exit 1
fi

if [[ ! -f "$DEFAULT_USER_SRC" ]]; then
  echo "ERROR: Missing source file: $DEFAULT_USER_SRC" >&2
  exit 1
fi

RENDER_DIR="$(mktemp -d)"
trap 'rm -rf "$RENDER_DIR"' EXIT
"${SCRIPT_DIR}/render-opencrabs-ops-config.sh" "$RENDER_DIR"

BRAIN_DEPLOYS=(
  "${BRAIN_SRC}/OPS_SOUL.md:${OPS_DIR}/SOUL.md"
  "${BRAIN_SRC}/SOUL.md:${DEFAULT_DIR}/SOUL.md"
  "${BRAIN_SRC}/USER.md:${OPS_DIR}/USER.md"
  "${DEFAULT_USER_SRC}:${DEFAULT_DIR}/USER.md"
  "${BRAIN_SRC}/AGENTS.md:${OPS_DIR}/AGENTS.md"
  "${BRAIN_SRC}/MEMORY.md:${OPS_DIR}/MEMORY.md"
  "${BRAIN_SRC}/SYSTEM.md:${OPS_DIR}/SYSTEM.md"
  "${BRAIN_SRC}/SYSTEM.md:${DEFAULT_DIR}/SYSTEM.md"
  "${RENDER_DIR}/config.toml:${OPS_DIR}/config.toml"
  "${RENDER_DIR}/keys.toml:${OPS_DIR}/keys.toml"
)

for pair in "${BRAIN_DEPLOYS[@]}"; do
  src="${pair%%:*}"
  dst="${pair#*:}"
  hermes_scp "$src" "$dst"
done

hermes_ssh 'chmod 600 ~/.opencrabs/profiles/ops/keys.toml'
hermes_scp "$GUARD_SCRIPT" "$REMOTE_GUARD"

hermes_ssh bash -s <<REMOTE
set -euo pipefail
rm -f ~/.opencrabs/profiles/ops/IDENTITY.md ~/.opencrabs/IDENTITY.md
bash "$REMOTE_GUARD"
rm -f "$REMOTE_GUARD"
systemctl restart opencrabs-ops opencrabs
systemctl is-active --quiet opencrabs opencrabs-ops
REMOTE

echo "Brain deployed to ${OPS_DIR} + default profile. Both services restarted."
