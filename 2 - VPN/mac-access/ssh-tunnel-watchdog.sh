#!/bin/bash
# SSH Tunnel Watchdog - restarts tunnel if port 4444 isn't listening
# Run via launchd or cron

HOST="vpn"
PORT=4444
KEEPALIVE=60

log() {
    echo "$(date '+%Y-%m-%d %H:%M:%S') - $1"
}

# Check if tunnel is actually working (port is listening on server)
check_tunnel() {
    ssh -o ConnectTimeout=5 -o StrictHostKeyChecking=no -p 22 -i ~/.ssh/id_ed25519 root@${HOST} "ss -tlnp | grep -q ':${PORT}'" 2>/dev/null
}

# Check if local SSH process is running
check_process() {
    pgrep -f "ssh.*-R.*:${PORT}:localhost:22" > /dev/null
}

restart_tunnel() {
    log "Tunnel not responding on port ${PORT}, restarting..."
    launchctl kickstart -k gui/501/com.tunnel.mac-vpn
    sleep 3
}

# Main loop
if check_process; then
    if ! check_tunnel; then
        restart_tunnel
    fi
else
    log "Tunnel process not running, starting via launchd..."
    launchctl start com.tunnel.mac-vpn
fi
