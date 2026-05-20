#!/usr/bin/env bash
# Unified OpenCrabs deploy for Hermes (run from Mac).
# See 5 - hermes/docs/services.md
#
#   --bootstrap       First-time ops profile + seed config/keys if missing + systemd + tg-tools
#   --bootstrap --no-ssh   Skip install-hermes-ssh-config.sh
#   --config          Patch memory keys, strip [mcp], optional token (--sync-provider-keys)
#   --brain           Deploy brain *.md from repo (backs up first); excludes ops MEMORY.md
#   --tg-tools        Wrapper, mcp.json, tools.toml (default + ops)
#   --update-tools    With --tg-tools: force overwrite tools.toml
#   --ssh             install-hermes-ssh-config.sh only
#   --status          Remote health checks (no writes)
#   --restart         Restart opencrabs + opencrabs-ops
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HERMES_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPO_ROOT="$(cd "${HERMES_DIR}/.." && pwd)"
BRAIN_SRC="${HERMES_DIR}/opencrabs-brain"
WRAPPER_SRC="${SCRIPT_DIR}/tg-mcp-call.py"
TOOLS_TEMPLATE="${HERMES_DIR}/config/opencrabs-tools.toml.template"
GUARD_SCRIPT="${SCRIPT_DIR}/opencrabs-guard-default-keys.sh"

DO_BOOTSTRAP=false
DO_CONFIG=false
DO_BRAIN=false
DO_TG_TOOLS=false
DO_SSH=false
DO_STATUS=false
DO_RESTART=false
UPDATE_TOOLS=false
BOOTSTRAP_NO_SSH=false
SYNC_PROVIDER_KEYS=false
CHANGED=false

usage() {
  cat <<EOF
Usage: $(basename "$0") [FLAGS]

  --bootstrap [--no-ssh]   Ops profile, seed missing config/keys, systemd, tg-tools
  --config [--sync-provider-keys]   Patch configs (memory, strip [mcp], optional token)
  --brain                  Deploy brain templates (not MEMORY.md on ops)
  --tg-tools [--update-tools]   tg-mcp-call, mcp.json, tools.toml x2
  --ssh                    Fleet SSH config on Hermes
  --status                 Health checks only
  --restart                Restart both systemd units

Examples:
  ./deploy-opencrabs.sh --status
  ./deploy-opencrabs.sh --tg-tools
  ./deploy-opencrabs.sh --bootstrap
  ./deploy-opencrabs.sh --config
  ./deploy-opencrabs.sh --brain
EOF
}

for arg in "$@"; do
  case "$arg" in
    --bootstrap) DO_BOOTSTRAP=true ;;
    --no-ssh) BOOTSTRAP_NO_SSH=true ;;
    --config) DO_CONFIG=true ;;
    --sync-provider-keys) SYNC_PROVIDER_KEYS=true; DO_CONFIG=true ;;
    --brain) DO_BRAIN=true ;;
    --tg-tools) DO_TG_TOOLS=true ;;
    --update-tools) UPDATE_TOOLS=true; DO_TG_TOOLS=true ;;
    --ssh) DO_SSH=true ;;
    --status) DO_STATUS=true ;;
    --restart) DO_RESTART=true ;;
    -h|--help) usage; exit 0 ;;
    *)
      echo "Unknown flag: $arg" >&2
      usage >&2
      exit 1
      ;;
  esac
done

if ! $DO_BOOTSTRAP && ! $DO_CONFIG && ! $DO_BRAIN && ! $DO_TG_TOOLS && ! $DO_SSH && ! $DO_STATUS && ! $DO_RESTART; then
  DO_STATUS=true
  usage
  echo ""
fi

# shellcheck source=/dev/null
source "${REPO_ROOT}/scripts/hermes-ssh.sh"

need_ssh() {
  $DO_BOOTSTRAP || $DO_CONFIG || $DO_BRAIN || $DO_TG_TOOLS || $DO_STATUS || $DO_RESTART
}

if need_ssh; then
  hermes_ssh_init
fi

run_status() {
  hermes_ssh bash -s <<'REMOTE'
set -euo pipefail
ok=true
check() { echo "$1"; }
fail() { echo "FAIL: $1"; ok=false; }

check "--- OpenCrabs status ---"
for f in /root/.opencrabs/config.toml /root/.opencrabs/profiles/ops/config.toml; do
  if grep -qE '^\[mcp\]' "$f" 2>/dev/null; then
    fail "[mcp] still present in $f"
  else
    check "OK: no [mcp] in $f"
  fi
done
if [[ -f /etc/tg-mcp/mcp.json ]]; then
  if python3 -c "
import json, sys
d=json.load(open('/etc/tg-mcp/mcp.json'))
for e in (d.get('mcpServers') or {}).values():
    if isinstance(e,dict) and e.get('bearer'):
        sys.exit(0)
sys.exit(1)
" 2>/dev/null; then
    check "OK: /etc/tg-mcp/mcp.json has bearer"
  else
    fail "/etc/tg-mcp/mcp.json missing bearer"
  fi
else
  fail "/etc/tg-mcp/mcp.json missing"
fi
for t in /root/.opencrabs/tools.toml /root/.opencrabs/profiles/ops/tools.toml; do
  if [[ -f "$t" ]] && grep -q '/usr/local/bin/tg-mcp-call' "$t"; then
    check "OK: $t uses tg-mcp-call"
  else
    fail "$t missing or wrong command path"
  fi
done
for u in opencrabs opencrabs-ops; do
  if systemctl is-active --quiet "$u"; then
    check "OK: $u active"
  else
    fail "$u not active"
  fi
done
$ok || exit 1
REMOTE
}

run_ssh() {
  "${SCRIPT_DIR}/install-hermes-ssh-config.sh"
  CHANGED=true
}

run_bootstrap() {
  RENDER_DIR="$(mktemp -d)"
  trap 'rm -rf "$RENDER_DIR"' RETURN
  "${SCRIPT_DIR}/render-opencrabs-ops-config.sh" "$RENDER_DIR"

  hermes_ssh bash -s <<'REMOTE'
set -euo pipefail
OPS=~/.opencrabs/profiles/ops
mkdir -p "$OPS"
if [[ ! -d "$OPS" ]] || [[ -z "$(ls -A "$OPS" 2>/dev/null)" ]]; then
  if [[ -d "$OPS" ]]; then
    mv "$OPS" "${OPS}.bak.$(date +%s)"
    mkdir -p "$OPS"
  fi
  if ! opencrabs profile list 2>/dev/null | grep -qE '(^|[[:space:]])ops($|[[:space:]])'; then
    opencrabs profile create ops
  else
    mkdir -p "$OPS"
  fi
fi
REMOTE

  if ! hermes_ssh 'test -f /root/.opencrabs/profiles/ops/config.toml'; then
    hermes_scp "${RENDER_DIR}/config.toml" "/root/.opencrabs/profiles/ops/config.toml"
    CHANGED=true
    echo "Seeded ops config.toml"
  fi
  if ! hermes_ssh 'test -f /root/.opencrabs/profiles/ops/keys.toml'; then
    hermes_scp "${RENDER_DIR}/keys.toml" "/root/.opencrabs/profiles/ops/keys.toml"
    hermes_ssh 'chmod 600 ~/.opencrabs/profiles/ops/keys.toml'
    CHANGED=true
    echo "Seeded ops keys.toml"
  fi

  if [[ -f "${BRAIN_SRC}/MEMORY.md" ]] && ! hermes_ssh 'test -f /root/.opencrabs/profiles/ops/MEMORY.md'; then
      hermes_scp "${BRAIN_SRC}/MEMORY.md" "/root/.opencrabs/profiles/ops/MEMORY.md"
    echo "Seeded ops MEMORY.md (bootstrap only)"
    CHANGED=true
  fi

  hermes_ssh bash -s <<'REMOTE'
set -euo pipefail
opencrabs service install 2>/dev/null || true
opencrabs -p ops service install 2>/dev/null || true
opencrabs -p ops cron remove vds-servers-nightly-pull 2>/dev/null || true
opencrabs -p ops cron add --name vds-servers-nightly-pull \
  --cron "0 3 * * *" --tz Europe/Moscow \
  --prompt "Run only: git -C /root/vds-servers pull --ff-only. One-line reply."
echo "systemd + nightly cron configured"
REMOTE
  CHANGED=true

  if ! $BOOTSTRAP_NO_SSH; then
    run_ssh
  fi

  run_tg_tools
  SYNC_PROVIDER_KEYS=false
  run_config
}

run_config() {
  local extra=()
  $SYNC_PROVIDER_KEYS && extra+=(--sync-provider-keys)
  "${SCRIPT_DIR}/patch-opencrabs-config.sh" "${extra[@]}"
  CHANGED=true
}

run_brain() {
  [[ -f "$GUARD_SCRIPT" ]] || { echo "ERROR: missing $GUARD_SCRIPT" >&2; exit 1; }
  [[ -f "${BRAIN_SRC}/DEFAULT_USER.md" ]] || { echo "ERROR: missing DEFAULT_USER.md" >&2; exit 1; }

  OPS_DIR="/root/.opencrabs/profiles/ops"
  DEFAULT_DIR="/root/.opencrabs"
  TS="$(date +%Y%m%dT%H%M%S)"

  BRAIN_DEPLOYS=(
    "${BRAIN_SRC}/OPS_SOUL.md:${OPS_DIR}/SOUL.md"
    "${BRAIN_SRC}/SOUL.md:${DEFAULT_DIR}/SOUL.md"
    "${BRAIN_SRC}/USER.md:${OPS_DIR}/USER.md"
    "${BRAIN_SRC}/DEFAULT_USER.md:${DEFAULT_DIR}/USER.md"
    "${BRAIN_SRC}/AGENTS.md:${OPS_DIR}/AGENTS.md"
    "${BRAIN_SRC}/CODE.md:${OPS_DIR}/CODE.md"
    "${BRAIN_SRC}/SYSTEM.md:${OPS_DIR}/SYSTEM.md"
    "${BRAIN_SRC}/SYSTEM.md:${DEFAULT_DIR}/SYSTEM.md"
  )

  for pair in "${BRAIN_DEPLOYS[@]}"; do
    src="${pair%%:*}"
    dst="${pair#*:}"
    hermes_ssh "[[ -f '$dst' ]] && cp -a '$dst' '${dst}.bak.deploy.${TS}' || true"
    hermes_scp "$src" "$dst"
    echo "Deployed $src -> $dst"
  done

  hermes_scp "$GUARD_SCRIPT" "/tmp/opencrabs-guard-default-keys.sh"
  hermes_ssh bash -s <<REMOTE
set -euo pipefail
rm -f ~/.opencrabs/profiles/ops/IDENTITY.md ~/.opencrabs/IDENTITY.md
bash /tmp/opencrabs-guard-default-keys.sh
rm -f /tmp/opencrabs-guard-default-keys.sh
REMOTE
  CHANGED=true
  echo "Brain deployed (ops MEMORY.md not touched)"
}

run_tg_tools() {
  for f in "$WRAPPER_SRC" "${HERMES_DIR}/config/mcp.json.template"; do
    [[ -f "$f" ]] || { echo "ERROR: missing $f" >&2; exit 1; }
  done

  RENDER_MCP="$(mktemp)"
  trap 'rm -f "$RENDER_MCP"' RETURN
  "${SCRIPT_DIR}/render-opencrabs-mcp-json.sh" "$RENDER_MCP"

  hermes_scp "$WRAPPER_SRC" "/tmp/tg-mcp-call.py"
  hermes_scp "$RENDER_MCP" "/tmp/tg-mcp-mcp.json"
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
install -m 600 /tmp/tg-mcp-mcp.json /etc/tg-mcp/mcp.json
rm -f /tmp/tg-mcp-mcp.json

install -m 755 /tmp/tg-mcp-call.py /usr/local/bin/tg-mcp-call
rm -f /tmp/tg-mcp-call.py

install_tools() {
  local dest="$1"
  if [[ ! -f /tmp/opencrabs-tools.toml.template ]]; then
    return 0
  fi
  install -m 644 /tmp/opencrabs-tools.toml.template "${dest}.new"
  if [[ "$UPDATE_TOOLS" == "true" ]]; then
    mv "${dest}.new" "$dest"
    echo "Installed $dest from template (--update-tools)"
  elif [[ ! -f "$dest" ]] || ! grep -q '/usr/local/bin/tg-mcp-call' "$dest"; then
    mv "${dest}.new" "$dest"
    echo "Installed $dest from template"
  else
    rm -f "${dest}.new"
    echo "$dest already uses tg-mcp-call (use --update-tools to overwrite)"
  fi
}

install_tools /root/.opencrabs/tools.toml
mkdir -p /root/.opencrabs/profiles/ops
install_tools /root/.opencrabs/profiles/ops/tools.toml
rm -f /tmp/opencrabs-tools.toml.template

mkdir -p /root/tgproxy
ln -sf /etc/tg-mcp/mcp.json /root/tgproxy/mcp.json
rm -f /root/.local/bin/mcp

python3 -c "
import re, pathlib
for p in map(pathlib.Path, [
    '/root/.opencrabs/config.toml',
    '/root/.opencrabs/profiles/ops/config.toml',
]):
    if not p.is_file():
        continue
    t = p.read_text()
    t2 = re.sub(r'(?ms)^\[mcp\].*?(?=^\[|\Z)', '', t)
    if t != t2:
        p.write_text(t2)
        print(f'stripped [mcp] from {p}')
"

echo "Smoke test get_chat_info..."
out=$(/usr/local/bin/tg-mcp-call get_chat_info '{"chat_id":"me","topics_limit":1}')
echo "$out" | head -c 200
echo
REMOTE
  CHANGED=true
  echo "tg-tools deploy complete"
}

run_restart() {
  hermes_ssh 'systemctl restart opencrabs opencrabs-ops && systemctl is-active --quiet opencrabs opencrabs-ops'
  echo "Restarted opencrabs + opencrabs-ops"
}

$DO_STATUS && run_status
$DO_SSH && run_ssh
$DO_BOOTSTRAP && run_bootstrap
$DO_TG_TOOLS && run_tg_tools
$DO_CONFIG && run_config
$DO_BRAIN && run_brain

if $CHANGED && ! $DO_RESTART; then
  DO_RESTART=true
fi
$DO_RESTART && $CHANGED && run_restart

if $DO_BOOTSTRAP; then
  echo "Bootstrap complete. DM @redevest_admin_tools_bot to test."
fi
