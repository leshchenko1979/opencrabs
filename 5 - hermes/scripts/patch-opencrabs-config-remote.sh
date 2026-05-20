#!/usr/bin/env bash
# Patch OpenCrabs config on Hermes (run remotely via deploy-opencrabs.sh --config).
# Env: OPS_TELEGRAM_TOKEN (optional), SYNC_PROVIDER_KEYS=true (optional)
set -euo pipefail

DEFAULT_CONFIG=/root/.opencrabs/config.toml
OPS_CONFIG=/root/.opencrabs/profiles/ops/config.toml
OPS_KEYS=/root/.opencrabs/profiles/ops/keys.toml
DEFAULT_KEYS=/root/.opencrabs/keys.toml

set_memory_key() {
  local file="$1" key="$2" value="$3"
  [[ -f "$file" ]] || return 0
  grep -qE '^\[memory\]' "$file" || return 0
  sed -i "/^${key}[[:space:]]*=/d" "$file"
  sed -i "/^\[memory\]/a ${key} = ${value}" "$file"
}

strip_mcp_section() {
  python3 - "$@" <<'PY'
import re, pathlib, sys
for arg in sys.argv[1:]:
    p = pathlib.Path(arg)
    if not p.is_file():
        continue
    t = p.read_text()
    t2 = re.sub(r"(?ms)^\[mcp\].*?(?=^\[|\Z)", "", t)
    if t != t2:
        p.write_text(t2)
        print(f"stripped [mcp] from {p}")
PY
}

patch_telegram_token() {
  local file="$1" token="$2"
  [[ -f "$file" ]] || return 0
  grep -qE '^\[channels\.telegram\]' "$file" || return 0
  sed -i '/^\[channels\.telegram\]/,/^\[/{
    /^token[[:space:]]*=/d
  }' "$file"
  sed -i "/^\[channels\.telegram\]/a token = \"${token}\"" "$file"
}

sync_provider_keys() {
  [[ -f "$DEFAULT_KEYS" && -f "$OPS_KEYS" ]] || return 0
  python3 - "$DEFAULT_KEYS" "$OPS_KEYS" <<'PY'
import re, pathlib, sys
default = pathlib.Path(sys.argv[1]).read_text()
ops = pathlib.Path(sys.argv[2]).read_text()
providers = ("minimax", "openrouter", "gemini")
for prov in providers:
    m = re.search(
        rf"(?ms)^\[providers\.{re.escape(prov)}\].*?^api_key\s*=\s*\"([^\"]+)\"",
        default,
    )
    if not m:
        m = re.search(
            rf"(?ms)^\[{re.escape(prov)}\].*?^api_key\s*=\s*\"([^\"]+)\"",
            default,
        )
    if not m:
        continue
    key = m.group(1)
    if re.search(rf"(?ms)^\[providers\.{re.escape(prov)}\]", ops):
        ops = re.sub(
            rf"(?ms)(^\[providers\.{re.escape(prov)}\].*?)^api_key\s*=.*$",
            rf"\1api_key = \"{key}\"",
            ops,
            count=1,
        )
    elif re.search(rf"(?ms)^\[{re.escape(prov)}\]", ops):
        ops = re.sub(
            rf"(?ms)(^\[{re.escape(prov)}\].*?)^api_key\s*=.*$",
            rf"\1api_key = \"{key}\"",
            ops,
            count=1,
        )
pathlib.Path(sys.argv[2]).write_text(ops)
print("synced provider api_keys default -> ops keys.toml")
PY
}

strip_mcp_section "$DEFAULT_CONFIG" "$OPS_CONFIG"

set_memory_key "$DEFAULT_CONFIG" vector_enabled false
set_memory_key "$DEFAULT_CONFIG" auto_update true
set_memory_key "$OPS_CONFIG" vector_enabled false
set_memory_key "$OPS_CONFIG" auto_update true
echo "Memory: vector_enabled=false, auto_update=true (default + ops)"

if [[ -n "${OPS_TELEGRAM_TOKEN:-}" ]]; then
  patch_telegram_token "$OPS_KEYS" "$OPS_TELEGRAM_TOKEN"
  patch_telegram_token "$OPS_CONFIG" "$OPS_TELEGRAM_TOKEN"
  echo "Patched ops telegram token in keys.toml + config.toml"
fi

if [[ "${SYNC_PROVIDER_KEYS:-}" == "true" ]]; then
  sync_provider_keys
fi

echo "Config patch complete."
