#!/usr/bin/env bash
# Install tg-mcp-call, mcp.json, fastmcp-slim[client]; patch tools.toml if needed.
# Run from Mac. See 5 - hermes/docs/services.md
#   --update-tools   overwrite /root/.opencrabs/tools.toml from template (JSON-safe param defaults)
set -euo pipefail

UPDATE_TOOLS=false
for arg in "$@"; do
  case "$arg" in
    --update-tools) UPDATE_TOOLS=true ;;
  esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HERMES_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPO_ROOT="$(cd "${HERMES_DIR}/.." && pwd)"
WRAPPER_SRC="${SCRIPT_DIR}/tg-mcp-call.py"
MCP_JSON_SRC="${HERMES_DIR}/config/mcp.json"
TOOLS_TEMPLATE="${HERMES_DIR}/config/opencrabs-tools.toml.template"
MCP_JSON_DST="/etc/tg-mcp/mcp.json"
WRAPPER_DST="/usr/local/bin/tg-mcp-call"
TOOLS_TOML="/root/.opencrabs/tools.toml"

# shellcheck source=/dev/null
source "${REPO_ROOT}/scripts/hermes-ssh.sh"

hermes_ssh_init

for f in "$WRAPPER_SRC" "$MCP_JSON_SRC"; do
  if [[ ! -f "$f" ]]; then
    echo "ERROR: missing $f" >&2
    exit 1
  fi
done

hermes_scp "$WRAPPER_SRC" "/tmp/tg-mcp-call.py"
hermes_scp "$MCP_JSON_SRC" "/tmp/tg-mcp-mcp.json"
if [[ -f "$TOOLS_TEMPLATE" ]]; then
  hermes_scp "$TOOLS_TEMPLATE" "/tmp/opencrabs-tools.toml.template"
fi

hermes_ssh bash -s -- "$UPDATE_TOOLS" <<'REMOTE'
set -euo pipefail
UPDATE_TOOLS=${1:-false}

pip3 uninstall -y fastmcp --break-system-packages 2>/dev/null || true
pip3 install --break-system-packages --upgrade 'fastmcp-slim[client]==3.3.0'
command -v fastmcp
fastmcp --version

mkdir -p /etc/tg-mcp /var/log/tg-mcp
chmod 755 /var/log/tg-mcp
install -m 644 /tmp/tg-mcp-mcp.json /etc/tg-mcp/mcp.json
rm -f /tmp/tg-mcp-mcp.json

install -m 755 /tmp/tg-mcp-call.py /usr/local/bin/tg-mcp-call
rm -f /tmp/tg-mcp-call.py

if [[ -f /tmp/opencrabs-tools.toml.template ]]; then
  install -m 644 /tmp/opencrabs-tools.toml.template /root/.opencrabs/tools.toml.new
  if [[ "$UPDATE_TOOLS" == "true" ]]; then
    mv /root/.opencrabs/tools.toml.new /root/.opencrabs/tools.toml
    echo "Installed tools.toml from template (--update-tools)"
  elif [[ ! -f /root/.opencrabs/tools.toml ]] || ! grep -q '/usr/local/bin/tg-mcp-call' /root/.opencrabs/tools.toml; then
    mv /root/.opencrabs/tools.toml.new /root/.opencrabs/tools.toml
    echo "Installed tools.toml from template"
  else
    rm -f /root/.opencrabs/tools.toml.new
    echo "tools.toml already uses tg-mcp-call — template not overwritten (use --update-tools)"
  fi
  rm -f /tmp/opencrabs-tools.toml.template
elif [[ -f /root/.opencrabs/tools.toml ]]; then
  if grep -q '/root/.local/bin/mcp tg-mcp' /root/.opencrabs/tools.toml; then
    sed -i 's|/root/.local/bin/mcp tg-mcp|/usr/local/bin/tg-mcp-call|g' /root/.opencrabs/tools.toml
    echo "Updated tools.toml: mcp proxy -> tg-mcp-call"
  elif grep -q '/usr/local/bin/tg-mcp-call' /root/.opencrabs/tools.toml; then
    echo "tools.toml already uses tg-mcp-call"
  else
    echo "WARN: unexpected tools.toml command= lines" >&2
    grep -E '^command[[:space:]]*=' /root/.opencrabs/tools.toml | head -3 >&2 || true
  fi
else
  echo "ERROR: /root/.opencrabs/tools.toml not found and no template" >&2
  exit 1
fi

mkdir -p /root/tgproxy
ln -sf /etc/tg-mcp/mcp.json /root/tgproxy/mcp.json

rm -f /root/.local/bin/mcp

echo "Smoke test get_chat_info..."
out=$(/usr/local/bin/tg-mcp-call get_chat_info '{"chat_id":"me","topics_limit":1}')
echo "$out" | head -c 200
echo
if echo "$out" | python3 -c "import sys,json; d=json.load(sys.stdin); sys.exit(0 if 'content' not in d and 'structured_content' not in d else 1)"; then
  echo "OK: lean stdout (no MCP envelope keys)"
else
  echo "WARN: output may include MCP envelope keys" >&2
fi
REMOTE

echo "Done. Restart if needed: ssh hermes 'systemctl restart opencrabs'"
