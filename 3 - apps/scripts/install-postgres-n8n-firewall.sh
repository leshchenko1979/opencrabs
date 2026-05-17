#!/bin/bash
# Copy DOCKER-USER firewall for published Postgres (n8n on operations_5) to ops3 and enable.
# To undo: ./scripts/open-postgres-public-firewall.sh
# Prereq: SSH to REMOTE_HOST_IP; docker already installed.
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

PG_DIR="${PROJECT_ROOT}/services/postgres"
scp "${PG_DIR}/iptables-docker-n8n.sh" "${PG_DIR}/docker-postgres-n8n-firewall.service" "${REMOTE_USER}@${VDS_SSH_TARGET}:/tmp/"

ssh "${REMOTE_USER}@${VDS_SSH_TARGET}" 'set -e
install -m 755 /tmp/iptables-docker-n8n.sh /usr/local/sbin/iptables-docker-n8n.sh
install -m 644 /tmp/docker-postgres-n8n-firewall.service /etc/systemd/system/docker-postgres-n8n-firewall.service
systemctl daemon-reload
systemctl enable --now docker-postgres-n8n-firewall.service
/usr/local/sbin/iptables-docker-n8n.sh
echo OK: docker-postgres-n8n-firewall.service active
systemctl is-active docker-postgres-n8n-firewall.service
'
