#!/usr/bin/env bash
#
# mac-network-diag.sh — local macOS connectivity snapshot for offline AI triage.
# Run when the network misbehaves; share the report file once you are back online.
#
# Usage:
#   ./scripts/mac-network-diag.sh              # report only
#   ./scripts/mac-network-diag.sh --toggle     # report + Ethernet off/on (your usual fix)
#   ./scripts/mac-network-diag.sh --no-log     # print to terminal only
#

set -u

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LOG_DIR="${MAC_NETWORK_DIAG_DIR:-$REPO_DIR/logs/mac-network-diag}"
TOGGLE_ETHERNET=false
WRITE_LOG=true
ETHERNET_SERVICE="${MAC_ETHERNET_SERVICE:-Ethernet}"
PING_COUNT=3
PING_WAIT_MS=2000

for arg in "$@"; do
    case "$arg" in
        --toggle | --toggle-ethernet) TOGGLE_ETHERNET=true ;;
        --no-log) WRITE_LOG=false ;;
        -h | --help)
            sed -n '2,12p' "$0"
            exit 0
            ;;
        *) echo "Unknown option: $arg (try --help)" >&2; exit 2 ;;
    esac
done

ts="$(date +%Y-%m-%d_%H-%M-%S)"
report="$LOG_DIR/report_${ts}.txt"

mkdir -p "$LOG_DIR"

# --- helpers -----------------------------------------------------------------

section() {
    printf '\n========== %s ==========\n' "$1"
}

run_cmd() {
    printf '\n$ %s\n' "$*"
    # shellcheck disable=SC2090
    "$@" 2>&1 || printf '[exit %s]\n' "$?"
}

pass_fail() {
    local label="$1"
    shift
    if "$@" >/dev/null 2>&1; then
        printf '[PASS] %s\n' "$label"
    else
        printf '[FAIL] %s\n' "$label"
    fi
}

ping_host() {
    local host="$1"
    ping -c "$PING_COUNT" -W "$PING_WAIT_MS" "$host" 2>&1
}

# --- collect -----------------------------------------------------------------

{
    section "Meta"
    echo "hostname=$(hostname)"
    echo "user=$(whoami)"
    echo "date=$(date -Iseconds 2>/dev/null || date)"
    echo "sw_vers=$(sw_vers 2>/dev/null | tr '\n' ' ')"
    echo "uptime=$(uptime)"

    section "Summary checks"
    gw=""
    gw=$(route -n get default 2>/dev/null | awk '/gateway:/{print $2; exit}')
    echo "default_gateway=${gw:-<none>}"
    pass_fail "ping gateway" ping_host "${gw:-127.0.0.1}"
    pass_fail "ping 1.1.1.1 (IP)" ping_host 1.1.1.1
    pass_fail "ping google.com (DNS+IP)" ping_host google.com
    if command -v curl >/dev/null 2>&1; then
        pass_fail "HTTPS google.com" curl -fsS --connect-timeout 5 -m 10 -o /dev/null https://www.google.com
    fi
    if command -v dig >/dev/null 2>&1; then
        pass_fail "DNS dig google.com" dig +time=3 +tries=1 +short google.com
    fi

    section "Hardware ports"
    run_cmd networksetup -listallhardwareports

    section "Network services"
    run_cmd networksetup -listallnetworkservices
    run_cmd networksetup -listnetworkserviceorder

    section "Ethernet service ($ETHERNET_SERVICE)"
    run_cmd networksetup -getinfo "$ETHERNET_SERVICE"
    run_cmd networksetup -getdnsservers "$ETHERNET_SERVICE"
    run_cmd networksetup -getsearchdomains "$ETHERNET_SERVICE"
    run_cmd networksetup -getdhcpinfo "$ETHERNET_SERVICE" 2>/dev/null || true

    section "Interface details (en0 + active)"
    run_cmd ifconfig en0
    for iface in en0 en1 bridge0 utun0 utun1 utun2 utun3; do
        if ifconfig "$iface" >/dev/null 2>&1; then
            run_cmd ifconfig "$iface"
        fi
    done

    section "Routing"
    run_cmd route -n get default
    run_cmd netstat -rn

    section "DNS / proxy / reachability"
    run_cmd scutil --dns
    run_cmd scutil --proxy
    run_cmd scutil --reachability

    section "ARP (gateway + neighbors)"
    if [[ -n "$gw" ]]; then
        run_cmd arp -n "$gw"
    fi
    run_cmd arp -a

    section "DNS resolution tests"
    if command -v dig >/dev/null 2>&1; then
        for server in "$gw" 1.1.1.1 8.8.8.8 111.88.96.50; do
            [[ -z "$server" ]] && continue
            printf '\n--- dig google.com @%s ---\n' "$server"
            dig +time=3 +tries=1 +short google.com "@$server" 2>&1 || true
        done
    fi

    section "HTTP timing"
    if command -v curl >/dev/null 2>&1; then
        for url in https://www.google.com https://1.1.1.1; do
            printf '\n--- curl %s ---\n' "$url"
            curl -sS -o /dev/null -w 'http_code=%{http_code} time_total=%{time_total}s remote_ip=%{remote_ip}\n' \
                --connect-timeout 5 -m 12 "$url" 2>&1 || true
        done
    fi

    section "Network quality (may take ~30s)"
    if command -v networkQuality >/dev/null 2>&1; then
        run_cmd networkQuality -s
    else
        echo "networkQuality not available"
    fi

    section "SSH targets (VDS — may fail when local net is broken)"
    for host in vpn apps n8n hermes; do
        printf '\n--- ssh -G %s (config) ---\n' "$host"
        ssh -G "$host" 2>/dev/null | awk '/^(hostname|port|user|identityfile) /' || echo "no ssh config for $host"
        printf '--- ssh connect test %s ---\n' "$host"
        ssh -o BatchMode=yes -o ConnectTimeout=8 -o StrictHostKeyChecking=no "$host" 'echo ok' 2>&1 || true
    done

    section "Reverse tunnel / VPN-related processes"
    ps aux 2>/dev/null | grep -E '[s]sh.*(-R|-L)|[a]mnezia|[w]ireguard|[o]penvpn' || echo "(none matched)"

    section "Listening / established (network)"
    run_cmd netstat -an -p tcp 2>/dev/null | head -80 || netstat -an | head -80

    section "Recent logs (network, last 90m)"
    if command -v log >/dev/null 2>&1; then
        log show --style syslog --last 90m \
            --predicate 'subsystem CONTAINS "network" OR subsystem CONTAINS "WiFi" OR eventMessage CONTAINS[c] "en0"' \
            2>/dev/null | tail -120 || log show --last 30m 2>/dev/null | grep -iE 'network|en0|ethernet|dhcp|dns' | tail -80 || echo "(log show unavailable or empty)"
    fi

    section "Automated warnings"
    if networksetup -listallnetworkservices 2>/dev/null | grep -q '^\*Ethernet'; then
        echo "WARN: Ethernet network service is DISABLED (Wi-Fi or other path may be active)."
    fi
    if ifconfig en0 2>/dev/null | grep -q 'status: inactive'; then
        echo "WARN: en0 link inactive — toggling Ethernet service often restores this."
    fi
    if networksetup -getsearchdomains "$ETHERNET_SERVICE" 2>/dev/null | grep -qE '^https?://'; then
        echo "WARN: Search domain looks like a DoH URL (should not be a search domain):"
        networksetup -getsearchdomains "$ETHERNET_SERVICE" 2>/dev/null | sed 's/^/  /'
        echo "  Fix: networksetup -setsearchdomains \"$ETHERNET_SERVICE\" \"\""
    fi
    dns_line="$(networksetup -getdnsservers "$ETHERNET_SERVICE" 2>/dev/null | head -1 || true)"
    if [[ -n "$dns_line" && "$dns_line" != *"aren't any"* && "$dns_line" != "192.168.31.1" ]]; then
        echo "WARN: Non-router DNS on $ETHERNET_SERVICE: $(networksetup -getdnsservers "$ETHERNET_SERVICE" 2>/dev/null | tr '\n' ' ')"
        echo "  If DNS fails but ping 1.1.1.1 works, try: networksetup -setdnsservers \"$ETHERNET_SERVICE\" 192.168.31.1"
    fi
    if ifconfig en0 2>/dev/null | grep -q '100baseTX'; then
        echo "WARN: en0 negotiated 100 Mbps (not 1 Gbps) — check cable, switch port, and dock."
    fi

    section "Notes for AI triage"
    cat <<'NOTES'
Check in this order:
1. Summary [PASS]/[FAIL] — L2 (gateway) vs L3 (1.1.1.1) vs DNS (google.com) vs HTTPS.
2. en0 media line — 100baseTX vs 1000baseT; flapping link often fixed by toggling Ethernet.
3. Search domains — a URL like https://.../dns-query as search domain is invalid and breaks resolution.
4. Manual DNS (111.88.96.x etc.) vs router DHCP DNS — try router DNS if custom resolvers fail.
5. utun* + SSH tunnels — VPN/tunnel can steal routes or stall when parent link glitches.
6. networkQuality "Low" responsiveness — bufferbloat or congested path, not always total outage.
NOTES

} | {
    if [[ "$WRITE_LOG" == true ]]; then
        tee "$report"
    else
        cat
    fi
}

if $WRITE_LOG; then
    echo ""
    echo "Report saved: $report"
    echo "Share this file with an AI agent when back online."
fi

if $TOGGLE_ETHERNET; then
    section "Toggle Ethernet"
    echo "Disabling $ETHERNET_SERVICE ..."
    networksetup -setnetworkserviceenabled "$ETHERNET_SERVICE" off
    sleep 3
    echo "Enabling $ETHERNET_SERVICE ..."
    networksetup -setnetworkserviceenabled "$ETHERNET_SERVICE" on
    sleep 5
    echo "Post-toggle summary:"
    gw=$(route -n get default 2>/dev/null | awk '/gateway:/{print $2; exit}')
    ping_host "${gw:-127.0.0.1}" || true
    ping_host 1.1.1.1 || true
fi
