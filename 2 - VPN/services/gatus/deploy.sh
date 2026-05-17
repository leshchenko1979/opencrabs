#!/bin/bash
set -e

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
# shellcheck source=/dev/null
source "${REPO_ROOT}/scripts/ssh-vds-host.sh"

REMOTE_HOST="${REMOTE_HOST:-104.128.131.166}"
SSH_TARGET="$(vds_ssh_connect_host "$REMOTE_HOST")"
REMOTE_USER="root"
SSH_KEY="${SSH_KEY:-/Users/leshchenko/.ssh/id_ed25519}"
DEPLOY_PATH="/opt/gatus"

echo "=== Deploying Gatus ==="

ssh -i "$SSH_KEY" "$REMOTE_USER@$SSH_TARGET" "mkdir -p $DEPLOY_PATH"

cd "$(dirname "$0")"
tar -czf /tmp/gatus.tar.gz \
    --exclude='data' \
    --exclude='*.tar.gz' \
    config/ docker-compose.yml .env

scp -i "$SSH_KEY" /tmp/gatus.tar.gz "$REMOTE_USER@$SSH_TARGET:/tmp/gatus.tar.gz"

ssh -i "$SSH_KEY" "$REMOTE_USER@$SSH_TARGET" \
    "cd $DEPLOY_PATH && tar -xzf /tmp/gatus.tar.gz && rm /tmp/gatus.tar.gz && mkdir -p data"

ssh -i "$SSH_KEY" "$REMOTE_USER@$SSH_TARGET" \
    "cd $DEPLOY_PATH && docker compose up -d --force-recreate"

echo "=== Gatus deployed ==="
