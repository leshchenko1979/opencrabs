#!/usr/bin/env bash
# Render ops config.toml and keys.toml from templates (run from Mac).
# Usage: render-opencrabs-ops-config.sh <output_dir>
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HERMES_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPO_ROOT="$(cd "${HERMES_DIR}/.." && pwd)"
PROFILES_DIR="${HERMES_DIR}/opencrabs-profiles/ops"
CONFIG_TEMPLATE="${PROFILES_DIR}/config.toml.template"
KEYS_TEMPLATE="${PROFILES_DIR}/keys.toml.template"

# shellcheck source=/dev/null
source "${REPO_ROOT}/scripts/hermes-ssh.sh"

OUTPUT_DIR="${1:-}"
if [[ -z "$OUTPUT_DIR" || ! -d "$OUTPUT_DIR" ]]; then
  echo "Usage: $0 <output_dir>" >&2
  exit 1
fi

hermes_ssh_init

OPS_TOKEN="${REDEVEST_ADMIN_BOT_TOKEN:-}"
if [[ -z "$OPS_TOKEN" ]]; then
  OPS_TOKEN="$(hermes_ssh "grep -E '^token[[:space:]]*=' \
    /root/.opencrabs/profiles/ops/keys.toml \
    /root/.opencrabs/profiles/ops/config.toml 2>/dev/null | head -1 \
    | sed -E 's/.*=[[:space:]]*\"([^\"]+)\".*/\\1/'" || true)"
fi
if [[ -z "$OPS_TOKEN" ]]; then
  echo "Set REDEVEST_ADMIN_BOT_TOKEN in env or ${HERMES_DIR}/.env" >&2
  exit 1
fi

MINIMAX_KEY="$(hermes_ssh "awk '
  /^\[minimax\]/ { in_m=1; in_p=0; next }
  /^\[providers\.minimax\]/ { in_p=1; in_m=0; next }
  /^\[/ { in_m=0; in_p=0 }
  (in_m || in_p) && /^api_key/ {
    gsub(/.*=[[:space:]]*\"/, \"\")
    gsub(/\".*$/, \"\")
    print
    exit
  }
' /root/.opencrabs/keys.toml /root/.opencrabs/config.toml 2>/dev/null" | head -1)"

if [[ -z "$MINIMAX_KEY" ]]; then
  echo "ERROR: could not read MiniMax api_key from hermes default profile" >&2
  exit 1
fi

MCP_BEARER="${TG_MCP_BEARER:-}"
if [[ -z "$MCP_BEARER" ]]; then
  MCP_BEARER="$(hermes_ssh "grep -E '^bearer[[:space:]]*=' /root/.opencrabs/config.toml 2>/dev/null | head -1 | sed -E 's/^[^\"]*\"([^\"]+)\".*/\\1/'" || true)"
fi
if [[ -z "$MCP_BEARER" ]]; then
  echo "ERROR: set TG_MCP_BEARER in .env or ensure default /root/.opencrabs/config.toml has [mcp] bearer" >&2
  exit 1
fi

substitute_placeholders() {
  sed \
    -e "s|__OPS_TELEGRAM_TOKEN__|${OPS_TOKEN}|g" \
    -e "s|__MINIMAX_API_KEY__|${MINIMAX_KEY}|g" \
    -e "s|__TG_MCP_BEARER__|${MCP_BEARER}|g"
}

for t in "$CONFIG_TEMPLATE" "$KEYS_TEMPLATE"; do
  [[ -f "$t" ]] || { echo "ERROR: missing template $t" >&2; exit 1; }
done

substitute_placeholders <"$CONFIG_TEMPLATE" >"${OUTPUT_DIR}/config.toml"
substitute_placeholders <"$KEYS_TEMPLATE" >"${OUTPUT_DIR}/keys.toml"
echo "Rendered ops config + keys to ${OUTPUT_DIR}/"
