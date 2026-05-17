#!/bin/bash

set -euo pipefail

echo "Enforcing root-only SSH access (key and password allowed) ..."

if [ -f /etc/security/access-ssh.conf ]; then
    echo "Removing legacy whitelist file ..."
    rm -f /etc/security/access-ssh.conf
fi

echo "Applying policy snippet ..."
cat > /etc/ssh/sshd_config.d/99-key-only.conf <<'CONFIG'
# SSH policy: allow both key and password auth from all addresses
PasswordAuthentication yes
ChallengeResponseAuthentication yes
PubkeyAuthentication yes
PermitRootLogin yes
AllowUsers root
AuthenticationMethods publickey,password
UsePAM yes
CONFIG

for setting in PermitRootLogin PasswordAuthentication ChallengeResponseAuthentication AuthenticationMethods AllowUsers; do
    if grep -q "^${setting}" /etc/ssh/sshd_config 2>/dev/null; then
        sed -i "s/^${setting}.*/# ${setting} moved to 99-key-only.conf/" /etc/ssh/sshd_config
    fi
done

echo "Restarting SSH service ..."
if systemctl restart sshd 2>/dev/null; then
    echo "SSH service restarted via sshd"
else
    systemctl restart ssh
fi

echo
echo "Validating SSH configuration ..."
if sshd -t; then
    echo "✓ SSH configuration is valid (key and password access allowed)"
else
    echo "✗ SSH configuration has errors"
    exit 1
fi
