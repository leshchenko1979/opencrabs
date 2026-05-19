#!/usr/bin/env bash
# Create OpenCrabs ops profile, keys, systemd, nightly cron (run from Mac).
# Requires: REDEVEST_ADMIN_BOT_TOKEN in env or in 5 - hermes/.env
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HERMES_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPO_ROOT="$(cd "${HERMES_DIR}/.." && pwd)"

# shellcheck source=/dev/null
source "${REPO_ROOT}/scripts/hermes-ssh.sh"

hermes_ssh_init

RENDER_DIR="$(mktemp -d)"
trap 'rm -rf "$RENDER_DIR"' EXIT
"${SCRIPT_DIR}/render-opencrabs-ops-config.sh" "$RENDER_DIR"

hermes_ssh bash -s <<'REMOTE'
set -euo pipefail
OPS=~/.opencrabs/profiles/ops
mkdir -p "$OPS"

if [[ ! -d "$OPS" ]] || [[ -z "$(ls -A "$OPS" 2>/dev/null)" ]]; then
  if [[ -d "$OPS" ]]; then
    mv "$OPS" "${OPS}.bak.$(date +%s)"
    mkdir -p "$OPS"
  fi
  if ! opencrabs profile list 2>/dev/null | grep -qE '(^|[[:space:]])ops($|[[:space:]])'; then
    opencrabs profile create ops
  else
    mkdir -p "$OPS"
  fi
fi
REMOTE

hermes_scp "${RENDER_DIR}/config.toml" "/root/.opencrabs/profiles/ops/config.toml"
hermes_scp "${RENDER_DIR}/keys.toml" "/root/.opencrabs/profiles/ops/keys.toml"
hermes_ssh 'chmod 600 ~/.opencrabs/profiles/ops/keys.toml'

hermes_ssh bash -s <<'REMOTE'
set -euo pipefail
opencrabs service install 2>/dev/null || true
opencrabs -p ops service install 2>/dev/null || true

opencrabs -p ops cron remove vds-servers-nightly-pull 2>/dev/null || true
opencrabs -p ops cron add --name vds-servers-nightly-pull \
  --cron "0 3 * * *" --tz Europe/Moscow \
  --prompt "Run only: git -C /root/vds-servers pull --ff-only. One-line reply."

# Disable ChromaDB vector memory on default profile (1GB Hermes — not in git template)
if grep -qE '^vector_enabled[[:space:]]*=[[:space:]]*true' /root/.opencrabs/config.toml 2>/dev/null; then
  sed -i '/^vector_enabled[[:space:]]*=[[:space:]]*true/d' /root/.opencrabs/config.toml
  echo "Removed vector_enabled from default config.toml"
fi

systemctl restart opencrabs opencrabs-ops
echo "ops profile ready (opencrabs restarted)"
REMOTE

"${SCRIPT_DIR}/deploy-opencrabs-ops-brain.sh"
echo "Deploy complete. DM @redevest_admin_tools_bot to test."
