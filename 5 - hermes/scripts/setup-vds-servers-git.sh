#!/usr/bin/env bash
# Clone leshchenko1979/servers on hermes with deploy key (run from Mac).
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HERMES_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPO_ROOT="$(cd "${HERMES_DIR}/.." && pwd)"

# shellcheck source=/dev/null
source "${REPO_ROOT}/scripts/hermes-ssh.sh"

hermes_ssh_init

DEPLOY_KEY="${DEPLOY_KEY:-$HERMES_SSH_KEY}"
GITHUB_KEY="${GITHUB_VDS_SERVERS_KEY:-$HOME/.ssh/github_vds_servers}"
if [[ ! -f "$GITHUB_KEY" ]]; then
  echo "Using $DEPLOY_KEY as github-vds-servers key (add as deploy key on leshchenko1979/servers)"
  GITHUB_KEY="$DEPLOY_KEY"
fi

hermes_scp "$GITHUB_KEY" "/root/.ssh/github_vds_servers"
hermes_ssh 'chmod 600 ~/.ssh/github_vds_servers'

hermes_ssh bash -s <<'REMOTE'
set -euo pipefail
if [[ ! -f ~/.ssh/config ]] || ! grep -q github-vds-servers ~/.ssh/config; then
  echo "Run install-hermes-ssh-config.sh first" >&2
  exit 1
fi
if [[ -d /root/vds-servers/.git ]]; then
  git -C /root/vds-servers pull --ff-only
  echo "Updated /root/vds-servers"
else
  git clone git@github-vds-servers:leshchenko1979/servers.git /root/vds-servers
  echo "Cloned to /root/vds-servers"
fi
REMOTE

echo "Verify: ssh hermes 'git -C /root/vds-servers rev-parse --short HEAD'"
