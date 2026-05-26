#!/usr/bin/env bash
#
# Unified diagnostics - runs deep health checks on all servers
# Auto-discovers servers from numbered directories
#

set -euo pipefail

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/ssh-vds-host.sh
source "$REPO_DIR/scripts/ssh-vds-host.sh"
SSH_USER="root"
SSH_KEY="~/.ssh/id_ed25519"
SSH_OPTS="-o StrictHostKeyChecking=no -o BatchMode=yes -o ConnectTimeout=10"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

# Thresholds
DISK_WARN=85
DISK_CRIT=90
SWAP_PRESSURE_PCT=30
SWAP_AVAIL_MB=200
ROOT_SIZE_MB=200

echo "=========================================="
echo "VDS Unified Diagnostics"
echo "=========================================="
echo ""

# Find all server dirs and collect name/ip pairs
count=0
while IFS= read -r dir; do
    [[ -z "$dir" ]] && continue
    name=$(basename "$dir")
    env_file="$dir/.env"
    [[ ! -f "$env_file" ]] && continue
    ip=$(grep '^REMOTE_HOST_IP=' "$env_file" 2>/dev/null | cut -d'=' -f2 | tr -d ' \r\n')
    [[ -z "$ip" ]] && continue
    count=$((count + 1))
    echo "=== $name | $ip ($(vds_ssh_connect_host "$ip")) ==="
done < <(find "$REPO_DIR" -maxdepth 1 -type d -name '[0-9]* - *' | sort)

if [[ $count -eq 0 ]]; then
    echo "No servers found."
    exit 1
fi

echo ""
echo "Running diagnostics on $count server(s)..."
echo ""

# Main diagnostic loop
while IFS= read -r dir; do
    [[ -z "$dir" ]] && continue
    name=$(basename "$dir")
    env_file="$dir/.env"
    [[ ! -f "$env_file" ]] && continue
    ip=$(grep '^REMOTE_HOST_IP=' "$env_file" 2>/dev/null | cut -d'=' -f2 | tr -d ' \r\n')
    [[ -z "$ip" ]] && continue

    target=$(vds_ssh_connect_host "$ip")
    echo -e "${GREEN}=== $name ($ip, ssh: $target) ===${NC}"

    ssh $SSH_OPTS -i "$SSH_KEY" "$SSH_USER@$target" 2>&1 << 'EOF'
# ── RAM + Swap ──────────────────────────────────────
echo "RAM/Swap:"
free -h | grep -E "^Mem:|Swap:"
AVAIL_MEM=$(free -m | awk '/Mem:/ {print $7}')
SWAP_TOTAL=$(free -m | awk '/Swap:/ {print $2}')
SWAP_USED=$(free -m | awk '/Swap:/ {print $3}')

# Swap pressure: swap used > threshold AND available memory low
if [[ "$SWAP_TOTAL" -gt 0 ]]; then
    SWAP_PCT=$((SWAP_USED * 100 / SWAP_TOTAL))
    if [[ "$SWAP_PCT" -gt 30 ]] && [[ "$AVAIL_MEM" -lt 300 ]]; then
        echo -e "  ${YELLOW}[SWAP PRESSURE]${NC} swap used ${SWAP_PCT}%, mem avail ${AVAIL_MEM}MB"
    fi
fi

# ── OOM kills ────────────────────────────────────────
echo "OOM kills (last boot):"
OOM_COUNT=$(journalctl -b 2>/dev/null | grep -ci "oom\|out of memory" || true)
echo "  $OOM_COUNT"
if [[ "$OOM_COUNT" -gt 0 ]]; then
    echo -e "  ${YELLOW}[WARNING]${NC} OOM kills detected — investigate memory pressure"
fi

# ── Disk ──────────────────────────────────────────────
echo "Disk:"
ROOT_PCT=$(df -h / 2>/dev/null | awk 'NR==2 {print $5}' | tr -d '%')
ROOT_AVAIL=$(df -h / 2>/dev/null | awk 'NR==2 {print $4}')
echo "  / usage: ${ROOT_PCT}% (${ROOT_AVAIL} available)"

if [[ "$ROOT_PCT" -ge 90 ]]; then
    echo -e "  ${RED}[CRITICAL]${NC} Disk >90% — clean up immediately"
elif [[ "$ROOT_PCT" -ge 85 ]]; then
    echo -e "  ${YELLOW}[WARNING]${NC} Disk >85% — plan cleanup"
fi

df -h --output=source,pcent,size,used 2>/dev/null | grep -v "tmpfs\|devtmpfs\|loop\|squashfs" | awk 'NR==1 {print "  "$0}; NR>1 {
    split($2, a, "%")
    if (a[1] >= 90) color="\033[0;31m[CRIT]\033[0m"
    else if (a[1] >= 85) color="\033[1;33m[WARN]\033[0m"
    else color=""
    print "  " $0 " " color
}'

# ── Docker ───────────────────────────────────────────
if command -v docker &>/dev/null; then
    echo "Docker:"
    docker system df 2>/dev/null | tail -5

    # Stale images (untagged)
    STALE=$(docker images --format '{{.Repository}}:{{.Tag}}' 2>/dev/null | grep '<none>' | wc -l | tr -d ' ')
    if [[ "$STALE" -gt 0 ]]; then
        echo -e "  ${YELLOW}[WARNING]${NC} $STALE stale/dangling images — run docker image prune"
    fi

    # Image count
    IMG_COUNT=$(docker images -q 2>/dev/null | wc -l | tr -d ' ')
    echo "  Total images: $IMG_COUNT"

    # Build cache
    BUILD_CACHE=$(docker system df --format '{{.BuildCache}}' 2>/dev/null | tail -1)
    if [[ -n "$BUILD_CACHE" ]] && [[ "$BUILD_CACHE" -gt 0 ]]; then
        echo "  Build cache: $BUILD_CACHE dirs"
    fi

    # Running containers
    RUNNING=$(docker ps --format '{{.Names}}' 2>/dev/null | wc -l | tr -d ' ')
    echo "  Running containers: $RUNNING"

    # ── Key container status ──────────────────────────────
    for c in traefik sablier redis postgres; do
        if docker ps --format '{{.Names}}' 2>/dev/null | grep -q "^${c}$"; then
            echo "  $c: running"
        else
            echo -e "  ${RED}$c: NOT RUNNING${NC}"
        fi
    done
else
    echo "Docker: not installed"
    # ── Systemd services (non-Docker boxes: box 4, 5) ─────────
    echo "Systemd services:"
    for svc in n8n caddy hermes-gateway; do
        STATUS=$(systemctl is-active "$svc" 2>/dev/null || echo "unknown")
        if [[ "$STATUS" == "active" ]]; then
            echo "  $svc: active"
        else
            echo -e "  ${RED}$svc: $STATUS${NC}"
        fi
    done
    # n8n health — box 3 (apps) and box 4 (n8n) only via docker health check above
    # box 5 (hermes) uses hermes-gateway for Telegram, not n8n
    # hermes Telegram status
    if systemctl is-active hermes-gateway &>/dev/null; then
        if grep -q '"state":"connected"' /root/.hermes/gateway_state.json 2>/dev/null; then
            echo "  hermes Telegram: connected"
        else
            echo -e "  ${YELLOW}hermes Telegram: check status${NC}"
        fi
    fi
fi

# ── Journal size ──────────────────────────────────────
echo "Journal:"
JOURNAL_LINES=$(journalctl --disk-usage 2>&1)
echo "  $JOURNAL_LINES"
# Flag if journal >100MB (regardless of whether vacuum would free space)
JOURNAL_NUM=$(echo "$JOURNAL_LINES" | grep -oE '[0-9]+(\.[0-9]+)?' | head -1)
JOURNAL_UNIT=$(echo "$JOURNAL_LINES" | grep -oE 'M|B' | head -1)
if [[ -n "$JOURNAL_NUM" ]] && [[ "$JOURNAL_UNIT" == "M" ]]; then
    GT100=$(echo "$JOURNAL_NUM 100" | awk '{print ($1 > $2) ? 1 : 0}')
    if [[ "$GT100" -eq 1 ]]; then
        echo -e "  ${YELLOW}[WARNING]${NC} Journal ${JOURNAL_NUM}MB — run journalctl --vacuum-time=7d (if active, set SystemMaxUse in /etc/systemd/journald.conf)"
    fi
fi

# ── /root size (anomaly check) ────────────────────────
ROOT_SIZE=$(du -sm /root 2>/dev/null | awk '{print $1}')
if [[ -n "$ROOT_SIZE" ]] && [[ "$ROOT_SIZE" -gt 200 ]]; then
    echo -e "  ${YELLOW}[WARNING]${NC} /root is ${ROOT_SIZE}MB — anomalous, check for old backups or logs"
fi

# ── Bottom line ───────────────────────────────────────
echo ""
echo "Summary:"
docker_ps=false
if command -v docker &>/dev/null; then
    UNHEALTHY=$(docker ps --filter "health=unhealthy" --format '{{.Names}}' 2>/dev/null | wc -l | tr -d ' ')
    if [[ "$UNHEALTHY" -gt 0 ]]; then
        echo -e "  ${RED}Unhealthy containers: $UNHEALTHY${NC}"
    fi
    # Show last container restarts (possible crash loops)
    RESTARTING=$(docker ps --format '{{.Names}}' 2>/dev/null | xargs -r docker inspect --format '{{.Name}}: restarts={{.RestartCount}}' 2>/dev/null | awk -F= '$2>3{print}')
    if [[ -n "$RESTARTING" ]]; then
        echo -e "  ${YELLOW}[WARNING]${NC} Containers with >3 restarts:"
        echo "$RESTARTING" | while read line; do echo "    $line"; done
    fi
fi
EOF
    echo ""
done < <(find "$REPO_DIR" -maxdepth 1 -type d -name '[0-9]* - *' | sort)

echo "=========================================="
echo "Diagnostics complete"
echo "=========================================="