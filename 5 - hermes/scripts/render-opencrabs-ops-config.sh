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

# Read provider api_keys from default profile keys.toml (canonical source).
read_default_provider_key() {
  local provider="$1"
  hermes_ssh "awk '
    /^\['\"${provider}\"'\]/ { in_b=1; in_p=0; next }
    /^\[providers\.'\"${provider}\"'\]/ { in_p=1; in_b=0; next }
    /^\[/ { in_b=0; in_p=0 }
    (in_b || in_p) && /^api_key/ {
      gsub(/.*=[[:space:]]*\"/, \"\")
      gsub(/\".*$/, \"\")
      print
      exit
    }
  ' /root/.opencrabs/keys.toml 2>/dev/null" | head -1
}

MINIMAX_KEY="$(read_default_provider_key minimax)"
OPENROUTER_KEY="$(read_default_provider_key openrouter)"
GEMINI_KEY="$(read_default_provider_key gemini)"

if [[ -z "$MINIMAX_KEY" ]]; then
  echo "ERROR: could not read MiniMax api_key from default keys.toml" >&2
  exit 1
fi
if [[ -z "$OPENROUTER_KEY" ]]; then
  echo "ERROR: could not read OpenRouter api_key from default keys.toml" >&2
  exit 1
fi
if [[ -z "$GEMINI_KEY" ]]; then
  echo "ERROR: could not read Gemini api_key from default keys.toml" >&2
  exit 1
fi

substitute_placeholders() {
  sed \
    -e "s|__OPS_TELEGRAM_TOKEN__|${OPS_TOKEN}|g" \
    -e "s|__MINIMAX_API_KEY__|${MINIMAX_KEY}|g" \
    -e "s|__OPENROUTER_API_KEY__|${OPENROUTER_KEY}|g" \
    -e "s|__GEMINI_API_KEY__|${GEMINI_KEY}|g"
}

for t in "$CONFIG_TEMPLATE" "$KEYS_TEMPLATE"; do
  [[ -f "$t" ]] || { echo "ERROR: missing template $t" >&2; exit 1; }
done

substitute_placeholders <"$CONFIG_TEMPLATE" >"${OUTPUT_DIR}/config.toml"
substitute_placeholders <"$KEYS_TEMPLATE" >"${OUTPUT_DIR}/keys.toml"
echo "Rendered ops config + keys to ${OUTPUT_DIR}/"
