#!/bin/bash
# Open published Postgres 5432 to any source: disable n8n-only DOCKER-USER rules, clear iptables, widen UFW.
# Run from repo root: ./scripts/open-postgres-public-firewall.sh
# Optional: N8N_IP=1.2.3.4 to match a non-default allowlist from install-postgres-n8n-firewall.sh

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
N8N_IP="${N8N_IP:-144.31.98.200}"

PG_DIR="${PROJECT_ROOT}/services/postgres"
scp "${PG_DIR}/iptables-docker-postgres-clear.sh" "${REMOTE_USER}@${VDS_SSH_TARGET}:/tmp/"
ssh "${REMOTE_USER}@${VDS_SSH_TARGET}" "N8N_IP='${N8N_IP}' bash -s" <<'REMOTE'
set -euo pipefail
install -m 755 /tmp/iptables-docker-postgres-clear.sh /usr/local/sbin/iptables-docker-postgres-clear.sh
systemctl disable --now docker-postgres-n8n-firewall.service 2>/dev/null || true
N8N_IP="${N8N_IP:-144.31.98.200}" /usr/local/sbin/iptables-docker-postgres-clear.sh
# Drop n8n-only UFW rule if it exists (ignore failure)
ufw delete allow from "${N8N_IP}" to any port 5432 proto tcp 2>/dev/null || true
ufw allow 5432/tcp comment 'postgres-public' 2>/dev/null || ufw allow 5432/tcp
ufw reload
echo "--- DOCKER-USER (5432) ---"
iptables -S DOCKER-USER | grep 5432 || echo '(none)'
ip6tables -S DOCKER-USER | grep 5432 || echo '(none ipv6)'
echo "--- UFW 5432 ---"
ufw status | grep 5432 || true
REMOTE

echo "OK: Postgres 5432 open at firewall layer (DOCKER-USER cleared, UFW allows 5432/tcp)."
