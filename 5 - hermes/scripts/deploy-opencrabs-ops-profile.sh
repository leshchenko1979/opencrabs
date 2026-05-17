#!/usr/bin/env bash
# Create OpenCrabs ops profile, keys, systemd, nightly cron (run from Mac).
# Requires: REDEVEST_ADMIN_BOT_TOKEN in env or in 5 - hermes/.env
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HERMES_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPO_ROOT="$(cd "${HERMES_DIR}/.." && pwd)"
# shellcheck source=/dev/null
source "${REPO_ROOT}/scripts/ssh-vds-host.sh"

if [[ -f "${HERMES_DIR}/.env" ]]; then
  set -a
  # shellcheck source=/dev/null
  source "${HERMES_DIR}/.env"
  set +a
fi

TOKEN="${REDEVEST_ADMIN_BOT_TOKEN:-}"
if [[ -z "$TOKEN" ]]; then
  echo "Set REDEVEST_ADMIN_BOT_TOKEN (Gatus/admin bot) in env or 5 - hermes/.env" >&2
  exit 1
fi

HERMES_IP="${REMOTE_HOST_IP:-132.243.213.9}"
HERMES_USER="${REMOTE_USER:-root}"
HERMES_PORT="${REMOTE_SSH_PORT:-18718}"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/id_ed25519}"

ssh -p "$HERMES_PORT" -i "$SSH_KEY" "${HERMES_USER}@${HERMES_IP}" "bash -s" <<REMOTE
set -euo pipefail
export REDEVEST_ADMIN_BOT_TOKEN='${TOKEN}'
OPS=~/.opencrabs/profiles/ops
mkdir -p "\$OPS"

if ! opencrabs profile list 2>/dev/null | grep -qE '^ops\$'; then
  opencrabs profile create ops || true
fi

# Fresh config for admin bot — do not migrate from default
cat > "\$OPS/config.toml" <<'CFG'
auto_update = false
max_concurrent = 2

[telegram]
enabled = true

[agent]
model = "anthropic/claude-sonnet-4-20250514"
CFG

mkdir -p "\$OPS"
cat > "\$OPS/keys.toml" <<KEYS
[telegram]
bot_token = "\${REDEVEST_ADMIN_BOT_TOKEN}"
KEYS
chmod 600 "\$OPS/keys.toml"

opencrabs -p ops service install 2>/dev/null || true
opencrabs -p ops service start 2>/dev/null || systemctl restart opencrabs-ops || true

# Nightly git pull
opencrabs -p ops cron remove vds-servers-nightly-pull 2>/dev/null || true
opencrabs -p ops cron add --name vds-servers-nightly-pull \\
  --cron "0 3 * * *" --tz Europe/Moscow \\
  --prompt "Run only: git -C /root/vds-servers pull --ff-only. One-line reply."

echo "ops profile ready"
REMOTE

"${SCRIPT_DIR}/deploy-opencrabs-ops-brain.sh"
echo "Deploy complete. DM @redevest_admin_tools_bot to test."
