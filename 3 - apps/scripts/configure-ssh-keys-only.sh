#!/bin/bash

# Configure VDS to allow root SSH only via public key (disables password auth for root).
# Run from repo root: ./scripts/configure-ssh-keys-only.sh
#
# PREREQUISITE: Ensure key-based login works before running! Test with:
#   ssh -i ~/.ssh/id_ed25519 root@REMOTE_HOST_IP
# If password is requested, do NOT run this script or you may lock yourself out.

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

if [ -f "${PROJECT_ROOT}/.env" ]; then
    source "${PROJECT_ROOT}/.env"
fi

SERVERS_REPO="$(cd "${PROJECT_ROOT}/.." && pwd)"
# shellcheck source=/dev/null
source "${SERVERS_REPO}/scripts/ssh-vds-host.sh"

REMOTE_HOST_IP="${REMOTE_HOST_IP:-apps}"
REMOTE_USER="${REMOTE_USER:-root}"
VDS_SSH_TARGET="$(vds_ssh_connect_host "$REMOTE_HOST_IP")"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/id_ed25519}"

SSH_OPTS="-o ConnectTimeout=10 -o StrictHostKeyChecking=no -o BatchMode=yes"
[ -f "$SSH_KEY" ] && SSH_OPTS="$SSH_OPTS -i $SSH_KEY"

run_remote() {
    ssh $SSH_OPTS "${REMOTE_USER}@${VDS_SSH_TARGET}" "$@"
}

echo "=== Configuring SSH key-only auth for root on ${REMOTE_USER}@${REMOTE_HOST_IP} (ssh: ${VDS_SSH_TARGET}) ==="

# Verify key-based auth works (BatchMode=yes would fail if password needed)
if ! run_remote "echo OK" 2>/dev/null; then
    echo "ERROR: Key-based SSH failed. Ensure your key is in ~/.ssh/authorized_keys on the server."
    echo "Do NOT proceed until key auth works, or you may lock yourself out."
    exit 1
fi

echo "Key auth OK. Applying sshd_config changes..."

run_remote 'bash -s' << 'REMOTE_SCRIPT'
set -e
CFG="/etc/ssh/sshd_config"
BACKUP="${CFG}.bak.$(date +%Y%m%d%H%M%S)"

cp -a "$CFG" "$BACKUP"
echo "Backed up to $BACKUP"

# Ensure PermitRootLogin prohibit-password (root with keys only)
if grep -qE '^PermitRootLogin\s+' "$CFG"; then
    sed -i 's/^PermitRootLogin.*/PermitRootLogin prohibit-password/' "$CFG"
else
    echo "PermitRootLogin prohibit-password" >> "$CFG"
fi

# Ensure PasswordAuthentication no (global - no password logins)
if grep -qE '^PasswordAuthentication\s+' "$CFG"; then
    sed -i 's/^PasswordAuthentication.*/PasswordAuthentication no/' "$CFG"
else
    echo "PasswordAuthentication no" >> "$CFG"
fi

# Test config before restarting
if sshd -t 2>/dev/null; then
    systemctl reload ssh 2>/dev/null || systemctl reload sshd 2>/dev/null
    echo "SSH reloaded. Root can now login only with keys."
else
    echo "ERROR: sshd_config invalid. Restoring backup."
    cp -a "$BACKUP" "$CFG"
    exit 1
fi
REMOTE_SCRIPT

echo ""
echo "Done. Password authentication for root is disabled."
