#!/usr/bin/env bash
#
# Unified cleanup script for all VDS servers
# Auto-discovers servers from numbered directories, SSHs in, runs cleanup tasks
# Credentials: user=root, SSH_KEY=~/.ssh/id_ed25519
#

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/ssh-vds-host.sh
source "$REPO_ROOT/scripts/ssh-vds-host.sh"
SSH_USER="root"
SSH_KEY="~/.ssh/id_ed25519"
SSH_OPTS="-o StrictHostKeyChecking=no -o BatchMode=yes -o ConnectTimeout=10"

echo "=========================================="
echo "VDS Unified Cleanup"
echo "=========================================="
echo ""

# Discover servers from numbered directories
echo "=== Discovering servers ==="
declare -a SERVERS=()
while IFS= read -r dir; do
    [[ -z "$dir" ]] && continue
    dir_name=$(basename "$dir")
    env_file="$dir/.env"

    if [[ ! -f "$env_file" ]]; then
        echo "  Skipped: $dir_name (.env missing)"
        continue
    fi

    host_ip=$(grep '^REMOTE_HOST_IP=' "$env_file" 2>/dev/null | cut -d'=' -f2 | tr -d ' \r\n' || true)

    if [[ -n "$host_ip" ]]; then
        SERVERS+=("$host_ip")
        echo "  Found: $dir_name -> $host_ip (ssh: $(vds_ssh_connect_host "$host_ip"))"
    else
        echo "  Skipped: $dir_name (no REMOTE_HOST_IP)"
    fi
done < <(find "$REPO_ROOT" -maxdepth 1 -type d -name '[0-9]* - *' | sort)

if [[ ${#SERVERS[@]} -eq 0 ]]; then
    echo "No servers found. Exiting."
    exit 0
fi

echo ""
echo "=== Running cleanup on ${#SERVERS[@]} server(s) ==="
echo ""

# Build the remote cleanup script
REMOTE_SCRIPT=$(cat << 'REMOTE_EOF'
#!/usr/bin/env bash
set -euo pipefail

echo "=== Cleanup started at $(date) on $(hostname) ==="

# 1. Logrotate setup and execution
echo "[1/8] Logrotate..."
if [[ -d /etc/logrotate.d ]] || sudo mkdir -p /etc/logrotate.d; then
    sudo tee /etc/logrotate.d/vds-servers > /dev/null << 'LOGROTATE'
/var/log/vds-servers/*.log {
    weekly
    rotate 4
    compress
    delaycompress
    missingok
    notifempty
    create 0640 root adm
}
LOGROTATE
    sudo logrotate -f /etc/logrotate.d/vds-servers 2>&1 || echo "  logrotate done"
fi

# 2. APT autoremove + clean
echo "[2/8] APT autoremove + clean..."
sudo apt-get autoremove -y 2>&1 || echo "  done"
sudo apt-get clean 2>&1 || echo "  done"

# 3. APT lists cleanup
echo "[3/8] APT lists cleanup..."
if [[ -d /var/lib/apt/lists/partial ]]; then
    sudo find /var/lib/apt/lists/partial -type f \( -name '*.stale' -o -name '*.tmp' -o -name '*.partial' -o -name '*.download' -o -name '*.lock' \) -delete 2>/dev/null || true
    echo "  done"
fi

# 4. APT autoremove + purge (old kernels)
echo "[4/8] APT autoremove --purge..."
sudo apt-get autoremove --purge -y 2>&1 || echo "  done"

# 5. systemd journal vacuum + config (caps size to prevent future bloat)
echo "[5/8] Journal vacuum (7d) + config..."
sudo journalctl --vacuum-time=7d 2>&1 || echo "  done"
if [[ ! -f /etc/systemd/journald.conf.d/vds.conf ]]; then
    sudo mkdir -p /etc/systemd/journald.conf.d
    sudo tee /etc/systemd/journald.conf.d/vds.conf > /dev/null << 'JOURNAL_CONF'
[Journal]
SystemMaxUse=100M
SystemMaxFiles=3
MaxRetentionSec=7day
JOURNAL_CONF
    echo "  Journal config deployed"
else
    echo "  Journal config already present"
fi

# 6. Snap cleanup
echo "[6/8] Snap cleanup..."
if command -v snap &>/dev/null; then
    snap list --all 2>/dev/null | awk '/disabled/{system("sudo snap remove " $1 " 2>/dev/null || true")}' || echo "  done"
else
    echo "  skipped (no snap)"
fi

# 7. /tmp/ cleanup
echo "[7/8] /tmp/ cleanup (7+ days)..."
sudo find /tmp -type f -atime +7 -delete 2>/dev/null || echo "  done"

# 8. Docker cleanup
echo "[8/8] Docker cleanup..."
if command -v docker &>/dev/null; then
    # Only remove *stopped* containers; never remove running infra (postgres, traefik, …)
    docker ps -a --filter 'status=exited' --format '{{.Names}}' 2>/dev/null | grep -v -E '^(redevest-crm|redevest-crm-test|pdf-extract|sablier)$' | xargs -r docker rm -f 2>/dev/null || true
    docker image prune -f 2>&1 || echo "  done"
    docker builder prune -f 2>&1 || echo "  done"
    echo "  done"
else
    echo "  skipped (no docker)"
fi

# 9. n8n cleanup (box 4 — non-Docker, systemd n8n)
echo "[9/9] n8n cleanup..."
if [[ -d /var/lib/n8n/.n8n ]]; then
    rm -f /var/lib/n8n/.n8n/n8nEventLog-*.log 2>/dev/null && echo "  event logs cleared" || true
    rm -rf /var/lib/n8n/.cache/* 2>/dev/null && echo "  cache cleared" || true
    if command -v sqlite3 &>/dev/null && [[ -f /var/lib/n8n/.n8n/database.sqlite ]]; then
        sqlite3 /var/lib/n8n/.n8n/database.sqlite 'PRAGMA wal_checkpoint(TRUNCATE);' 2>/dev/null && echo "  WAL checkpointed" || true
    fi
else
    echo "  skipped (n8n not installed)"
fi

echo "=== Cleanup completed at $(date) ==="
REMOTE_EOF
)

# Build cron setup script (weekly Sunday 3am)
CRON_SETUP=$(cat << 'CRON_EOF'
#!/usr/bin/env bash
set -euo pipefail

# Deploy the cleanup script
sudo tee /tmp/vds-cleanup-run.sh > /dev/null << 'SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
echo "=== Weekly cleanup at $(date) on $(hostname) ==="
sudo apt-get autoremove -y 2>&1 || true
sudo apt-get clean 2>&1 || true
sudo apt-get autoremove --purge -y 2>&1 || true
if [[ -d /var/lib/apt/lists/partial ]]; then
    sudo find /var/lib/apt/lists/partial -type f \( -name '*.stale' -o -name '*.tmp' -o -name '*.partial' -o -name '*.download' -o -name '*.lock' \) -delete 2>/dev/null || true
fi
sudo journalctl --vacuum-time=7d 2>&1 || true
if [[ ! -f /etc/systemd/journald.conf.d/vds.conf ]]; then
    sudo mkdir -p /etc/systemd/journald.conf.d
    sudo tee /etc/systemd/journald.conf.d/vds.conf > /dev/null << 'JOURNAL_CONF'
[Journal]
SystemMaxUse=100M
SystemMaxFiles=3
MaxRetentionSec=7day
JOURNAL_CONF
fi
if command -v snap &>/dev/null; then
    snap list --all 2>/dev/null | awk '/disabled/{system("sudo snap remove " $1 " 2>/dev/null || true")}' || true
fi
sudo find /tmp -type f -atime +7 -delete 2>/dev/null || true
if command -v docker &>/dev/null; then
    # Only remove *stopped* containers; never remove running infra (postgres, traefik, …)
    docker ps -a --filter 'status=exited' --format '{{.Names}}' 2>/dev/null | grep -v -E '^(redevest-crm|redevest-crm-test|pdf-extract|sablier)$' | xargs -r docker rm -f 2>/dev/null || true
    docker image prune -f 2>&1 || true
    docker builder prune -f 2>&1 || true
fi
# n8n cleanup (box 4)
if [[ -d /var/lib/n8n/.n8n ]]; then
    rm -f /var/lib/n8n/.n8n/n8nEventLog-*.log 2>/dev/null || true
    rm -rf /var/lib/n8n/.cache/* 2>/dev/null || true
    if command -v sqlite3 &>/dev/null && [[ -f /var/lib/n8n/.n8n/database.sqlite ]]; then
        sqlite3 /var/lib/n8n/.n8n/database.sqlite 'PRAGMA wal_checkpoint(TRUNCATE);' 2>/dev/null || true
    fi
fi
echo "=== Done at $(date) ==="
SCRIPT
sudo chmod +x /tmp/vds-cleanup-run.sh

# Install cron (Sunday 3am) — preserve existing entries, avoid duplicates
CRON_LINE="0 3 * * 0 /tmp/vds-cleanup-run.sh >> /var/log/vds-cleanup.log 2>&1"
EXISTING_CRONTAB=$(sudo crontab -l 2>/dev/null || true)
if echo "$EXISTING_CRONTAB" | grep -q "vds-cleanup-run"; then
    echo "  Cron cleanup already installed, skipping"
else
    echo "$EXISTING_CRONTAB" | sudo crontab - 2>/dev/null || true
    echo "$CRON_LINE" | sudo crontab -
    echo "  Cron installed: Sunday 3am"
fi
CRON_EOF
)

# Process each server
for host_ip in "${SERVERS[@]}"; do
    target=$(vds_ssh_connect_host "$host_ip")
    echo "=========================================="
    echo "Processing: $host_ip (ssh: $target)"
    echo "=========================================="

    # Test SSH
    echo "  Testing SSH..."
    if ! ssh $SSH_OPTS -i "$SSH_KEY" "$SSH_USER@$target" "echo OK" 2>&1; then
        echo "  ERROR: Cannot reach $host_ip ($target), skipping."
        echo ""
        continue
    fi
    echo "  SSH OK"

    # Upload and run cleanup
    echo "  Running cleanup..."
    echo "$REMOTE_SCRIPT" | ssh $SSH_OPTS -i "$SSH_KEY" "$SSH_USER@$target" "cat > /tmp/vds-cleanup.sh && bash /tmp/vds-cleanup.sh && rm -f /tmp/vds-cleanup.sh" 2>&1 || echo "  Cleanup done with warnings"

    # Post-cleanup disk validation
    echo "  Post-cleanup disk check..."
    DISK_PCT=$(ssh $SSH_OPTS -i "$SSH_KEY" "$SSH_USER@$target" "df / 2>/dev/null | awk 'NR==2 {print \$5}' | tr -d '%'" 2>/dev/null || echo "unknown")
    DISK_AVAIL=$(ssh $SSH_OPTS -i "$SSH_KEY" "$SSH_USER@$target" "df -h / 2>/dev/null | awk 'NR==2 {print \$4}'" 2>/dev/null || echo "unknown")
    echo "  Disk: ${DISK_PCT}% used, ${DISK_AVAIL} available"

    # Setup cron (skip if crontab not installed)
    echo "  Setting up cron..."
    HAS_CRONTAB=$(ssh $SSH_OPTS -i "$SSH_KEY" "$SSH_USER@$target" "command -v crontab" 2>&1)
    if [[ -z "$HAS_CRONTAB" ]]; then
        echo "  Skipped: crontab not installed on this server"
    else
        echo "$CRON_SETUP" | ssh $SSH_OPTS -i "$SSH_KEY" "$SSH_USER@$target" "cat > /tmp/cron-setup.sh && bash /tmp/cron-setup.sh && rm -f /tmp/cron-setup.sh" 2>&1 || echo "  Cron setup done with warnings"
    fi

    echo ""
done

echo "=========================================="
echo "All servers processed"
echo "=========================================="
