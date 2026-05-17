#!/bin/bash
# Allow inbound Redis 6379 (UFW v4+v6). Compose must publish 6379:6379 — see services/redis/docker-compose.yml.
# Run from repo root: ./scripts/open-redis-public-firewall.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

if [ ! -f "${PROJECT_ROOT}/.env" ]; then
	echo "Error: .env not found at ${PROJECT_ROOT}/.env"
	exit 1
fi
# shellcheck source=/dev/null
source "${PROJECT_ROOT}/.env"

SERVERS_REPO="$(cd "${PROJECT_ROOT}/.." && pwd)"
# shellcheck source=/dev/null
source "${SERVERS_REPO}/scripts/ssh-vds-host.sh"
REMOTE_HOST_IP="${REMOTE_HOST_IP:-apps}"
REMOTE_USER="${REMOTE_USER:-root}"
VDS_SSH_TARGET="$(vds_ssh_connect_host "$REMOTE_HOST_IP")"

ssh "${REMOTE_USER}@${VDS_SSH_TARGET}" 'set -euo pipefail
ufw allow 6379/tcp comment redis-public 2>/dev/null || ufw allow 6379/tcp
ufw reload
echo "--- UFW 6379 ---"
ufw status | grep 6379 || true
# Clear DOCKER-USER drops on 6379 if a future restrict script added them (idempotent)
while iptables -D DOCKER-USER -p tcp -m tcp --dport 6379 -j DROP 2>/dev/null; do :; done
while ip6tables -D DOCKER-USER -p tcp -m tcp --dport 6379 -j DROP 2>/dev/null; do :; done
echo "--- DOCKER-USER 6379 ---"
iptables -S DOCKER-USER | grep 6379 || echo "(none)"
'

echo "OK: UFW allows 6379/tcp. Redeploy Redis if compose was updated: ./scripts/deploy-redis.sh"
