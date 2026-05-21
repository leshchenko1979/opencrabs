#!/bin/bash
# Deploy fleet SSH key + systemd tunnel: VPN docker gateway → Hermes ops A2A loopback.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATUS_DIR="$(dirname "$SCRIPT_DIR")"
VPN_ROOT="$(cd "${GATUS_DIR}/../.." && pwd)"
SERVERS_REPO="$(cd "${VPN_ROOT}/.." && pwd)"

# shellcheck source=/dev/null
source "${SERVERS_REPO}/scripts/ssh-vds-host.sh"

SSH_TARGET="$(vds_ssh_connect_host "${REMOTE_HOST:-vpn}")"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/id_ed25519}"
FLEET_SSH_KEY="${GATUS_SSH_KEY:-$SSH_KEY}"
HERMES_HOST="${HERMES_HOST:-132.243.213.9}"
HERMES_SSH_PORT="${HERMES_SSH_PORT:-18718}"
TUNNEL_BIND="${TUNNEL_BIND:-172.18.0.1}"

if [[ ! -f "$FLEET_SSH_KEY" ]]; then
  echo "ERROR: fleet SSH key not found: $FLEET_SSH_KEY" >&2
  exit 1
fi

echo "Deploying A2A SSH tunnel to ${SSH_TARGET} (${TUNNEL_BIND}:18791 → Hermes ops A2A)..."

scp -i "$SSH_KEY" -o BatchMode=yes \
  "$FLEET_SSH_KEY" \
  "${SCRIPT_DIR}/gatus-a2a-tunnel.env.example" \
  "${GATUS_DIR}/systemd/gatus-a2a-tunnel.service" \
  "root@${SSH_TARGET}:/tmp/"

ssh -i "$SSH_KEY" -o BatchMode=yes "root@${SSH_TARGET}" bash -s <<REMOTE
set -euo pipefail
install -d -m 700 /root/.ssh
install -m 600 /tmp/$(basename "$FLEET_SSH_KEY") /root/.ssh/id_ed25519
rm -f /tmp/$(basename "$FLEET_SSH_KEY")

cat > /etc/gatus-a2a-tunnel.env <<EOF
FLEET_SSH_KEY=/root/.ssh/id_ed25519
HERMES_HOST=${HERMES_HOST}
HERMES_SSH_PORT=${HERMES_SSH_PORT}
TUNNEL_BIND=${TUNNEL_BIND}
EOF
chmod 600 /etc/gatus-a2a-tunnel.env

install -m 644 /tmp/gatus-a2a-tunnel.service /etc/systemd/system/gatus-a2a-tunnel.service
rm -f /tmp/gatus-a2a-tunnel.env.example /tmp/gatus-a2a-tunnel.service

docker network create traefik-public 2>/dev/null || true

# Verify fleet key reaches Hermes before enabling tunnel
ssh -o BatchMode=yes -o ConnectTimeout=15 -p ${HERMES_SSH_PORT} -i /root/.ssh/id_ed25519 \
  root@${HERMES_HOST} 'curl -sS --max-time 5 http://127.0.0.1:18791/a2a/health'

systemctl daemon-reload
systemctl enable --now gatus-a2a-tunnel.service
sleep 1
systemctl is-enabled --quiet gatus-a2a-tunnel.service
systemctl is-active --quiet gatus-a2a-tunnel.service

curl -sS --max-time 10 "http://${TUNNEL_BIND}:18791/a2a/health"
echo ""

systemctl disable --now gatus-opencrabs-bridge.service 2>/dev/null || true
REMOTE

echo ""
echo "A2A tunnel active on ${SSH_TARGET} at http://${TUNNEL_BIND}:18791/a2a/v1"
echo "Bridge stopped. Redeploy Gatus config: cd \"2 - VPN\" && ./scripts/deploy-gatus.sh"
