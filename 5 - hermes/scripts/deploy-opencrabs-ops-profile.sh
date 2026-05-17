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
  if [[ -d "\$OPS" ]]; then
    mv "\$OPS" "\${OPS}.bak.\$(date +%s)"
  fi
  opencrabs profile create ops
fi

# MiniMax from default profile; admin Telegram token only in ops keys
TEMPLATE="\${OPS}/config.toml"
if [[ -f /root/vds-servers/5\\ -\\ hermes/opencrabs-profiles/ops/config.toml.template ]]; then
  cp "/root/vds-servers/5 - hermes/opencrabs-profiles/ops/config.toml.template" "\$OPS/config.toml"
else
  cat > "\$OPS/config.toml" <<'CFG'
auto_update = false
max_concurrent = 2

[providers.minimax]
enabled = true
default_model = "MiniMax-M2.7"

[channels.telegram]
enabled = true

[agent]
default_model = "MiniMax-M2.7"

[mcp]
enabled = false

[memory]
vector_enabled = false
CFG
fi

minimax_key=\$(grep -A1 '^\[providers.minimax\]' ~/.opencrabs/keys.toml | grep api_key | sed -E 's/.*"([^"]+)".*/\\1/')
cat > "\$OPS/keys.toml" <<KEYS
[channels.telegram]
token = "\${REDEVEST_ADMIN_BOT_TOKEN}"

[providers.minimax]
api_key = "\${minimax_key}"
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
