#!/usr/bin/env bash
# Run patch-opencrabs-config-remote.sh on Hermes (from Mac).
# Usage: patch-opencrabs-config.sh [--sync-provider-keys]
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HERMES_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPO_ROOT="$(cd "${HERMES_DIR}/.." && pwd)"
REMOTE_SCRIPT="${SCRIPT_DIR}/patch-opencrabs-config-remote.sh"

# shellcheck source=/dev/null
source "${REPO_ROOT}/scripts/hermes-ssh.sh"

SYNC_PROVIDER_KEYS=false
for arg in "$@"; do
  case "$arg" in
    --sync-provider-keys) SYNC_PROVIDER_KEYS=true ;;
  esac
done

hermes_ssh_init

hermes_scp "$REMOTE_SCRIPT" "/tmp/patch-opencrabs-config-remote.sh"
if [[ -n "${REDEVEST_ADMIN_BOT_TOKEN:-}" ]]; then
  printf '%s' "$REDEVEST_ADMIN_BOT_TOKEN" | hermes_ssh 'cat > /tmp/.ops_telegram_token && chmod 600 /tmp/.ops_telegram_token'
else
  hermes_ssh 'rm -f /tmp/.ops_telegram_token'
fi
hermes_ssh bash -s -- "$SYNC_PROVIDER_KEYS" <<'REMOTE'
set -euo pipefail
SYNC_PROVIDER_KEYS=${1:-false}
if [[ -f /tmp/.ops_telegram_token ]]; then
  export OPS_TELEGRAM_TOKEN="$(cat /tmp/.ops_telegram_token)"
  rm -f /tmp/.ops_telegram_token
fi
export SYNC_PROVIDER_KEYS
bash /tmp/patch-opencrabs-config-remote.sh
rm -f /tmp/patch-opencrabs-config-remote.sh
REMOTE
