#!/bin/bash
set -e

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
# shellcheck source=/dev/null
source "${REPO_ROOT}/scripts/ssh-vds-host.sh"

REMOTE_HOST="${REMOTE_HOST:-104.128.131.166}"
SSH_TARGET="$(vds_ssh_connect_host "$REMOTE_HOST")"
REMOTE_USER="root"
SSH_KEY="${SSH_KEY:-/Users/leshchenko/.ssh/id_ed25519}"

echo "=== Deploying Traefik to VPN ==="

# Create directory
ssh -i "$SSH_KEY" "$REMOTE_USER@$SSH_TARGET" "mkdir -p /opt/traefik/config"

# Create empty acme.json
ssh -i "$SSH_KEY" "$REMOTE_USER@$SSH_TARGET" "touch /opt/traefik/config/acme.json && chmod 600 /opt/traefik/config/acme.json"

# Create tarball
cd "$(dirname "$0")"
tar -czf /tmp/traefik.tar.gz config/ docker-compose.yml

# Transfer and extract
scp -i "$SSH_KEY" /tmp/traefik.tar.gz "$REMOTE_USER@$SSH_TARGET:/tmp/traefik.tar.gz"
ssh -i "$SSH_KEY" "$REMOTE_USER@$SSH_TARGET" \
    "cd /opt/traefik && tar -xzf /tmp/traefik.tar.gz && rm /tmp/traefik.tar.gz"

# Start
ssh -i "$SSH_KEY" "$REMOTE_USER@$SSH_TARGET" \
    "cd /opt/traefik && docker compose up -d"

# Verify
sleep 5
ssh -i "$SSH_KEY" "$REMOTE_USER@$SSH_TARGET" \
    "curl -sf http://localhost:8080/api/version"

echo "=== Traefik deployed ==="
