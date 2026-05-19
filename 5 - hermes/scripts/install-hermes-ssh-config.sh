#!/usr/bin/env bash
# Install shared SSH key, config, and known_hosts on hermes (run from Mac).
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HERMES_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPO_ROOT="$(cd "${HERMES_DIR}/.." && pwd)"

# shellcheck source=/dev/null
source "${REPO_ROOT}/scripts/hermes-ssh.sh"

hermes_ssh_init

hermes_ssh 'mkdir -p ~/.ssh && chmod 700 ~/.ssh'
hermes_scp "$HERMES_SSH_KEY" "/root/.ssh/id_ed25519"
hermes_scp "${HERMES_SSH_KEY}.pub" "/root/.ssh/id_ed25519.pub"
hermes_ssh 'chmod 600 ~/.ssh/id_ed25519 && chmod 644 ~/.ssh/id_ed25519.pub'
hermes_scp "${HERMES_DIR}/config/ssh-config" "/root/.ssh/config"
hermes_ssh 'chmod 600 ~/.ssh/config'

hermes_ssh bash -s <<'REMOTE'
set -euo pipefail
touch ~/.ssh/known_hosts
for h in vpn apps n8n; do
  host=$(ssh -G "$h" 2>/dev/null | awk '/^hostname /{print $2}')
  port=$(ssh -G "$h" 2>/dev/null | awk '/^port /{print $2}')
  port=${port:-22}
  ssh-keyscan -p "$port" -H "$host" 2>/dev/null >> ~/.ssh/known_hosts || true
done
ssh-keyscan -p 22 -H 127.0.0.1 2>/dev/null >> ~/.ssh/known_hosts || true
chmod 600 ~/.ssh/known_hosts
echo "SSH config installed; known_hosts updated"
REMOTE

echo "=== Verify fleet SSH from Hermes ==="
hermes_ssh 'for h in vpn apps n8n; do ssh -o BatchMode=yes -o ConnectTimeout=10 "$h" /usr/local/bin/host-diag; echo "$h exit:$?"; done'
hermes_ssh 'ssh -o BatchMode=yes -o ConnectTimeout=5 hermes-local /usr/local/bin/host-diag; echo "hermes-local exit:$?"'
echo "Done."
