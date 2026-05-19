#!/usr/bin/env bash
# Render ops config.toml and keys.toml from templates (run from Mac).
# Usage: render-opencrabs-ops-config.sh <output_dir>
#   Writes config.toml and keys.toml into output_dir (must exist).
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
  OPS_TOKEN="$(hermes_ssh "grep -E '^token[[:space:]]*=' /root/.opencrabs/profiles/ops/keys.toml /root/.opencrabs/profiles/ops/config.toml 2>/dev/null | head -1 | sed -E 's/.*=[[:space:]]*\"([^\"]+)\".*/\\1/'" || true)"
fi
if [[ -z "$OPS_TOKEN" ]]; then
  echo "Set REDEVEST_ADMIN_BOT_TOKEN in env or ${HERMES_DIR}/.env" >&2
  exit 1
fi

# MiniMax from default profile on hermes: [minimax] or [providers.minimax]
MINIMAX_KEY="$(hermes_ssh "awk '
  /^\[minimax\]/ { in_minimax=1; next }
  /^\[/ { in_minimax=0 }
  in_minimax && /^api_key/ {
    gsub(/.*=[[:space:]]*\"/, \"\")
    gsub(/\".*$/, \"\")
    print
    exit
  }
' /root/.opencrabs/keys.toml" 2>/dev/null || true)"

if [[ -z "$MINIMAX_KEY" ]]; then
  MINIMAX_KEY="$(hermes_ssh "awk '
    /^\[providers\.minimax\]/ { in_p=1; next }
    /^\[/ { in_p=0 }
    in_p && /^api_key/ {
      gsub(/.*=[[:space:]]*\"/, \"\")
      gsub(/\".*$/, \"\")
      print
      exit
    }
  ' /root/.opencrabs/keys.toml /root/.opencrabs/config.toml 2>/dev/null" | head -1)"
fi

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

render_file() {
  local template="$1"
  local out="$2"
  sed \
    -e "s|__OPS_TELEGRAM_TOKEN__|${OPS_TOKEN}|g" \
    -e "s|__MINIMAX_API_KEY__|${MINIMAX_KEY}|g" \
    -e "s|__TG_MCP_BEARER__|${MCP_BEARER}|g" \
    "$template" >"$out"
}

for t in "$CONFIG_TEMPLATE" "$KEYS_TEMPLATE"; do
  if [[ ! -f "$t" ]]; then
    echo "ERROR: missing template $t" >&2
    exit 1
  fi
done

render_file "$CONFIG_TEMPLATE" "${OUTPUT_DIR}/config.toml"
render_file "$KEYS_TEMPLATE" "${OUTPUT_DIR}/keys.toml"
echo "Rendered ops config + keys to ${OUTPUT_DIR}/"
