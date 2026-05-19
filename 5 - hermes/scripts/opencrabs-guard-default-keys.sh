#!/usr/bin/env bash
# Run on hermes before restarting OpenCrabs profiles.
# Default profile must NOT use [channels.telegram] in keys.toml — OpenCrabs
# prefers that over [telegram].bot_token and config.toml, which breaks @oc_l1979_bot
# if the ops token was copied there by mistake.
set -euo pipefail

DEFAULT_KEYS="${HOME}/.opencrabs/keys.toml"
OPS_KEYS="${HOME}/.opencrabs/profiles/ops/keys.toml"
DEFAULT_CONFIG="${HOME}/.opencrabs/config.toml"

if [[ ! -f "$DEFAULT_KEYS" ]]; then
  echo "ERROR: missing $DEFAULT_KEYS" >&2
  exit 1
fi

if [[ ! -f "$OPS_KEYS" ]]; then
  echo "WARN: missing $OPS_KEYS — skipping ops token cross-check" >&2
fi

if grep -qE '^\[channels(\.|])' "$DEFAULT_KEYS"; then
  backup="${DEFAULT_KEYS}.bak.$(date +%Y%m%d%H%M%S)"
  cp "$DEFAULT_KEYS" "$backup"
  python3 - "$DEFAULT_KEYS" <<'PY'
import sys
from pathlib import Path

path = Path(sys.argv[1])
lines = path.read_text().splitlines()
out = []
skip = False
for line in lines:
    stripped = line.strip()
    if stripped.startswith("[channels"):
        skip = True
        continue
    if skip:
        if line.startswith("[") and not line.startswith("[channels"):
            skip = False
            out.append(line)
        continue
    out.append(line)
path.write_text("\n".join(out).rstrip() + "\n")
PY
  echo "Removed [channels.*] from default keys.toml (backup: $backup)"
fi

extract_token() {
  local file="$1"
  [[ -f "$file" ]] || return 0
  grep -E '^(bot_token|token)[[:space:]]*=' "$file" \
    | sed -E 's/.*=[[:space:]]*"([^"]+)".*/\1/' \
    | head -1
}

default_token="$(extract_token "$DEFAULT_KEYS")"
if [[ -z "$default_token" ]]; then
  default_token="$(extract_token "$DEFAULT_CONFIG")"
fi
ops_token="$(extract_token "$OPS_KEYS")"

if [[ -z "$default_token" ]]; then
  echo "ERROR: no Telegram token found for default profile (keys.toml or config.toml)" >&2
  exit 1
fi

if [[ -n "$ops_token" && "$default_token" == "$ops_token" ]]; then
  echo "ERROR: default and ops profiles share the same Telegram token — @oc_l1979_bot will not poll" >&2
  exit 1
fi

if [[ -n "$ops_token" ]] && grep -Fq "$ops_token" "$DEFAULT_KEYS"; then
  echo "ERROR: ops bot token still present in $DEFAULT_KEYS" >&2
  exit 1
fi

if grep -qE '^\[telegram\]' "$DEFAULT_KEYS" && grep -qE '^token[[:space:]]*=' "$DEFAULT_CONFIG" 2>/dev/null; then
  echo "NOTE: default has [telegram].bot_token in keys.toml and inline token in config.toml — keys.toml wins for channels"
fi

bot_username() {
  local token="$1"
  curl -sf "https://api.telegram.org/bot${token}/getMe" \
    | python3 -c 'import json,sys; r=json.load(sys.stdin); print(r.get("result",{}).get("username","?"))' \
    2>/dev/null || echo "?"
}

default_bot="$(bot_username "$default_token")"
echo "Default profile Telegram: @${default_bot}"
if [[ "$default_bot" == "?" ]]; then
  echo "ERROR: could not verify default bot via Telegram getMe" >&2
  exit 1
fi

if [[ -n "$ops_token" ]]; then
  ops_bot="$(bot_username "$ops_token")"
  echo "Ops profile Telegram:     @${ops_bot}"
  if [[ "$ops_bot" == "?" ]]; then
    echo "ERROR: could not verify ops bot via Telegram getMe" >&2
    exit 1
  fi
  if [[ "$default_bot" == "$ops_bot" ]]; then
    echo "ERROR: both profiles resolve to the same Telegram bot" >&2
    exit 1
  fi
fi
