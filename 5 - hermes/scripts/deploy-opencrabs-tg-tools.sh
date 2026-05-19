#!/usr/bin/env bash
# Install tg-mcp-call wrapper and point default tools.toml at fastmcp (not mcp proxy).
# Run from Mac. Requires fastmcp on hermes: pip3 install 'fastmcp>=3.3'
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HERMES_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPO_ROOT="$(cd "${HERMES_DIR}/.." && pwd)"
WRAPPER_SRC="${SCRIPT_DIR}/tg-mcp-call.sh"
TOOLS_TOML="/root/.opencrabs/tools.toml"
WRAPPER_DST="/usr/local/bin/tg-mcp-call"
OLD_PREFIX="/root/.local/bin/mcp tg-mcp"
NEW_PREFIX="/usr/local/bin/tg-mcp-call"

# shellcheck source=/dev/null
source "${REPO_ROOT}/scripts/hermes-ssh.sh"

hermes_ssh_init

if [[ ! -f "$WRAPPER_SRC" ]]; then
  echo "ERROR: missing $WRAPPER_SRC" >&2
  exit 1
fi

REMOTE_TMP="/tmp/tg-mcp-call.sh"
hermes_scp "$WRAPPER_SRC" "$REMOTE_TMP"
hermes_ssh "set -e
  install -m 755 ${REMOTE_TMP} ${WRAPPER_DST}
  rm -f ${REMOTE_TMP}
  if ! command -v fastmcp >/dev/null; then
    echo 'Installing fastmcp (uses fastmcp-slim deps)...'
    pip3 install --break-system-packages 'fastmcp>=3.3'
  fi
  fastmcp --version
  if [[ ! -f ${TOOLS_TOML} ]]; then
    echo 'ERROR: ${TOOLS_TOML} not found' >&2
    exit 1
  fi
  if grep -q '${OLD_PREFIX}' ${TOOLS_TOML}; then
    sed -i 's|${OLD_PREFIX}|${NEW_PREFIX}|g' ${TOOLS_TOML}
    echo 'Updated tools.toml: mcp proxy -> tg-mcp-call (fastmcp)'
  elif grep -q '${NEW_PREFIX}' ${TOOLS_TOML}; then
    echo 'tools.toml already uses tg-mcp-call'
  else
    echo 'WARN: tools.toml has unexpected command= lines — check manually' >&2
    grep 'command =' ${TOOLS_TOML} | head -3 >&2
  fi
  echo 'Smoke test (get_chat_info me)...'
  timeout 30 ${WRAPPER_DST} get_chat_info "$(printf '%s' '{"chat_id":"me","topics_limit":1}')" | head -c 200
  echo
  echo 'Optional: remove heavy mcp proxy if unused elsewhere:'
  echo '  rm -f /root/.local/bin/mcp'
"

echo "Done. Restart default opencrabs if tools were mid-session: ssh hermes 'systemctl restart opencrabs'"
