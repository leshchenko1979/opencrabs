#!/bin/bash
# Deploy Gatus → OpenCrabs bridge on VPN box
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATUS_DIR="$(dirname "$SCRIPT_DIR")"
VPN_ROOT="$(cd "${GATUS_DIR}/../.." && pwd)"
SERVERS_REPO="$(cd "${VPN_ROOT}/.." && pwd)"

# shellcheck source=/dev/null
source "${SERVERS_REPO}/scripts/ssh-vds-host.sh"

SSH_TARGET="$(vds_ssh_connect_host "${REMOTE_HOST:-vpn}")"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/id_ed25519}"
HERMES_ENV="${SERVERS_REPO}/5 - hermes/.env"

install -d "${TMPDIR:-/tmp}" 2>/dev/null || true

if [[ -f "$HERMES_ENV" ]]; then
  # shellcheck source=/dev/null
  source "$HERMES_ENV"
fi

MCP_BEARER=$(ssh -i "$SSH_KEY" -o BatchMode=yes root@hermes \
  "grep -E '^bearer\\s*=' /root/.opencrabs/config.toml 2>/dev/null | head -1 | sed -E 's/^[^\"]*\"([^\"]+)\".*/\\1/'" || true)

if [[ -z "$MCP_BEARER" ]]; then
  echo "ERROR: could not read tg-mcp bearer from hermes /root/.opencrabs/config.toml"
  exit 1
fi

BRIDGE_SECRET="${GATUS_BRIDGE_SECRET:-$(openssl rand -hex 32)}"

echo "Deploying bridge to ${SSH_TARGET}..."

scp -i "$SSH_KEY" -o BatchMode=yes \
  "${SCRIPT_DIR}/gatus-opencrabs-bridge.sh" \
  "${SCRIPT_DIR}/gatus_opencrabs_bridge.py" \
  "${GATUS_DIR}/systemd/gatus-opencrabs-bridge.service" \
  "root@${SSH_TARGET}:/tmp/"

ssh -i "$SSH_KEY" -o BatchMode=yes "root@${SSH_TARGET}" bash -s <<REMOTE
set -euo pipefail
install -m 755 /tmp/gatus-opencrabs-bridge.sh /usr/local/bin/gatus-opencrabs-bridge.sh
install -m 644 /tmp/gatus_opencrabs_bridge.py /usr/local/bin/gatus_opencrabs_bridge.py
install -m 644 /tmp/gatus-opencrabs-bridge.service /etc/systemd/system/gatus-opencrabs-bridge.service
cat > /etc/gatus-bridge.env <<EOF
TG_MCP_BEARER=${MCP_BEARER}
GATUS_BRIDGE_SECRET=${BRIDGE_SECRET}
EOF
chmod 600 /etc/gatus-bridge.env
systemctl daemon-reload
systemctl enable gatus-opencrabs-bridge.service
systemctl restart gatus-opencrabs-bridge.service
rm -f /tmp/gatus-opencrabs-bridge.sh /tmp/gatus_opencrabs_bridge.py /tmp/gatus-opencrabs-bridge.service
REMOTE

echo ""
echo "Bridge deployed. GATUS_BRIDGE_SECRET (add to Gatus custom header at deploy):"
echo "$BRIDGE_SECRET"
echo ""
echo "Test: ssh ${SSH_TARGET} '/usr/local/bin/gatus-opencrabs-bridge.sh notify-test'"
