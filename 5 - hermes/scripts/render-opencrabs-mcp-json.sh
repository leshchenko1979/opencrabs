#!/usr/bin/env bash
# Render /etc/tg-mcp/mcp.json from template (run from Mac).
# Usage: render-opencrabs-mcp-json.sh <output_path>
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HERMES_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPO_ROOT="$(cd "${HERMES_DIR}/.." && pwd)"
TEMPLATE="${HERMES_DIR}/config/mcp.json.template"

# shellcheck source=/dev/null
source "${REPO_ROOT}/scripts/hermes-ssh.sh"

OUTPUT="${1:-}"
if [[ -z "$OUTPUT" ]]; then
  echo "Usage: $0 <output_path>" >&2
  exit 1
fi
[[ -f "$TEMPLATE" ]] || { echo "ERROR: missing $TEMPLATE" >&2; exit 1; }

hermes_ssh_init

read_bearer_from_mcp_json() {
  hermes_ssh "python3 -c \"
import json, sys
try:
    with open(sys.argv[1]) as f:
        d = json.load(f)
    for e in (d.get('mcpServers') or {}).values():
        if isinstance(e, dict) and e.get('bearer'):
            print(e['bearer'])
            break
except FileNotFoundError:
    pass
\"" "/etc/tg-mcp/mcp.json" 2>/dev/null | head -1
}

read_bearer_from_legacy_config() {
  hermes_ssh "grep -E '^bearer[[:space:]]*=' /root/.opencrabs/config.toml \
    /root/.opencrabs/profiles/ops/config.toml 2>/dev/null | head -1 \
    | sed -E 's/^[^\"]*\"([^\"]+)\".*/\\1/'" 2>/dev/null || true
}

MCP_BEARER="${TG_MCP_BEARER:-}"
if [[ -z "$MCP_BEARER" ]]; then
  MCP_BEARER="$(read_bearer_from_mcp_json)"
fi
if [[ -z "$MCP_BEARER" ]]; then
  MCP_BEARER="$(read_bearer_from_legacy_config)"
fi
if [[ -z "$MCP_BEARER" ]]; then
  echo "Set TG_MCP_BEARER in ${HERMES_DIR}/.env or ensure /etc/tg-mcp/mcp.json has bearer" >&2
  exit 1
fi

sed "s|__TG_MCP_BEARER__|${MCP_BEARER}|g" <"$TEMPLATE" >"$OUTPUT"
echo "Rendered mcp.json to ${OUTPUT}"
