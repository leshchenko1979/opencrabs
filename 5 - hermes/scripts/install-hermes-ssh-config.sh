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
chmod 600 ~/.ssh/known_hosts
echo "SSH config installed; known_hosts updated"
REMOTE

echo "Done. Verify: ssh hermes 'for h in vpn apps n8n; do ssh -o BatchMode=yes \"\$h\" /usr/local/bin/host-diag; done'"
