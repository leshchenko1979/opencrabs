# VDS Operations Repository

This repository contains maintenance scripts and documentation for the VDS server running AmneziaWG.

## Server Information

- **SSH Host**: 104.128.131.166
- **SSH User**: root
- **Services**: AmneziaWG, Traefik, Gatus

## Services

### Amnezia VPN

Amnezia runs as a Docker container with the `amneziawg-go` binary managing the WireGuard interface inside the container.

**Current status** (2026-04-22): Container `amnezia-awg2` is **UP** and operational.

#### Deployment

The Amnezia installation was performed using the AmneziaVPN installation app, which sets up a Docker-based deployment:

**Container startup command** (reference):
```bash
docker run -d \
  --name amnezia-awg2 \
  --cap-add NET_ADMIN \
  --cap-add SYS_MODULE \
  --sysctl net.ipv4.conf.all.rp_filter=0 \
  -v /lib/modules:/lib/modules \
  -p 43855:43855/udp \
  amnezia-awg2:latest
```

#### Network Configuration

| Parameter | Value |
|-----------|-------|
| VPN Server IP | 104.128.131.166 |
| VPN Network | 10.8.1.0/24 |
| Listen Port | 43855/UDP |
| WireGuard Interface | `awg0` (inside container) |
| Amnezia Bridge Interface | `amn0` (host, IP: 172.29.172.1/24) |

#### Container Internals

- **Config**: `/opt/amnezia/awg/awg0.conf` (baked into image at build time)
- **Startup script**: `/opt/amnezia/start.sh` — runs on each container start:
  ```bash
  awg-quick down /opt/amnezia/awg/awg0.conf  # cleanup old instance
  awg-quick up /opt/amnezia/awg/awg0.conf  # bring up VPN
  iptables -A FORWARD -i awg0 -j ACCEPT    # allow traffic
  iptables -t nat -A POSTROUTING -s 10.8.1.0/24 -o eth0 -j MASQUERADE
  tail -f /dev/null                         # keep container running
  ```
- **Keys directory**: `/opt/amnezia/awg/` contains:
  - `awg0.conf` — Server config with AmneziaWG J confound parameters
  - `clientsTable` — Connected clients data
  - `wireguard_server_private_key.key`
  - `wireguard_server_public_key.key`
  - `wireguard_psk.key` — Pre-shared key for peer

#### Checking Status

```bash
# Container status
docker ps | grep amnezia

# WireGuard status (inside container)
docker exec amnezia-awg2 wg show

# Interface info
docker exec amnezia-awg2 ip addr show awg0

# Recent handshakes
docker exec amnezia-awg2 wg show | grep -E 'latest handshake|transfer'
```

#### Management

| Task | Command |
|------|---------|
| Restart container | `docker restart amnezia-awg2` |
| View logs | `docker logs amnezia-awg2` |
| Stop container | `docker stop amnezia-awg2` |
| View startup script | `docker exec amnezia-awg2 cat /opt/amnezia/start.sh` |
| View config | `docker exec amnezia-awg2 cat /opt/amnezia/awg/awg0.conf` |

Note: amnezia-xray was removed to free port 443 for Traefik.

### Traefik (Reverse Proxy)

Traefik v2.11 provides reverse proxy and Let's Encrypt TLS.

- **Ports**: 80 (HTTP), 443 (HTTPS), 8080 (Dashboard)
- **Dashboard**: http://104.128.131.166:8080
- **Deploy path**: `/opt/traefik/`
- **Config**: `/opt/traefik/config/config.yml`

### Gatus (Monitoring)

Gatus provides health monitoring with Telegram alerting and **OpenCrabs ops** for host failures (boxes 2–5).

- **URL**: https://gatus.l1979.ru (via Traefik)
- **Deploy path**: `/data/projects/gatus/` on box 2
- **Repo config**: `services/gatus/config/config.yaml` (secrets via `${VAR}` in `.env.gatus`, not in git)
- **Deploy Gatus**: `./scripts/deploy-gatus.sh` (from `2 - VPN/`)
- **Deploy host-diag script on all boxes**: `services/gatus/scripts/deploy-host-diag.sh`
- **OpenCrabs bridge** (host alerts only): `services/gatus/scripts/deploy-gatus-bridge.sh` → systemd `gatus-opencrabs-bridge` on `0.0.0.0:9081`, POST `/gatus/alert` → tg-mcp → @redevest_admin_tools_bot

**Secrets (not in git):**

| File | Purpose |
|------|---------|
| `services/gatus/.env.gatus` | `TELEGRAM_BOT_TOKEN`, `TELEGRAM_CHAT_ID`, heartbeat tokens, `GATUS_BRIDGE_SECRET` |
| `/etc/gatus-bridge.env` (VPN host) | `TG_MCP_BEARER`, `GATUS_BRIDGE_SECRET` (bridge); copied into `.env.gatus` on Gatus deploy |
| `config/keys/gatus_ssh` | SSH private key for Gatus SSH checks (rendered at deploy from Mac `id_ed25519`) |

**Deploy order:** `deploy-gatus-bridge.sh` first (creates bridge secret) → `deploy-gatus.sh` (syncs secret into container env).

**Monitored endpoints** (see `config/config.yaml` for full list):
| Endpoint | Interval | Notes |
|----------|----------|-------|
| ai-antispam | 60s | HTTP; response &lt; 2s |
| ai-gateway, business-tinder, tg-mcp, pdf-extract, n8n | 60s | HTTP; response &lt; 5s |
| postgres, redis | 60s | TCP to apps host |
| redevest-crm | 3h | HTTP; Sablier wake; response &lt; 60s |
| hermes | 60s | SSH systemd check; **`enabled: false`** while service stopped manually |
| host-box2–5 | 15m | SSH `/usr/local/bin/host-diag` (load + disk + RAM) |
| sender, business-tinder-notifications | 25h | External heartbeat |

**Telegram alerting**: Configured for endpoint failures (3 consecutive failures triggers alert for most HTTP checks; host/disk use 2).

#### Host monitoring (SSH)

Source: `services/gatus/scripts/host-diag` → installed as **`/usr/local/bin/host-diag`** on each box.

| Check | Threshold (in script) |
|-------|---------------------|
| Load | fail if 1-min load ≥ 1.5 (`LOAD_MAX=150`, load×100) |
| Disk `/` | fail if use ≥ 90% |
| RAM | fail if `MemAvailable` &lt; 80 MiB |

Output: `load disk mem_mb` (e.g. `13 58 371`). Gatus uses `[STATUS] == 0` (not `[BODY]` parsing).

After editing thresholds or the script: run `deploy-host-diag.sh`, then `deploy-gatus.sh` only if `config.yaml` changed.

```yaml
- name: host-box2
  group: host
  url: "ssh://104.128.131.166:22"
  body: |
    {"command": "/usr/local/bin/host-diag"}
  interval: 15m
  conditions:
    - "[CONNECTED] == true"
    - "[STATUS] == 0"
```

Note: AmneziaDNS service is not installed on this VDS. DNS resolution is handled by the system's default DNS configuration.

## Mac Access via Reverse SSH Tunnel

### Overview

The VPN server acts as a relay for accessing your Mac (behind NAT/CGNAT) from anywhere using Termius on Android/iOS.

```
┌─────────────┐         ┌──────────────────┐         ┌─────────────┐
│   Your Mac   │ ──────► │  VPN Server      │ ◄────── │  Termius    │
│  (behind NAT)│  SSH    │  104.128.131.166 │  SSH    │  Phone/Tablet│
│             │ outbound│  (Finland)       │         │             │
└─────────────┘         └──────────────────┘         └─────────────┘
      │                         │                           │
      │  ssh -R *:4444:localhost:22 root@vps                 │
      │ ───────────────────────────────────────────────────►│
      │                         │                           │
      │                         │  ssh user@vps -p 4444     │
      │                         │ ◄─────────────────────────│
```

Your Mac initiates an outbound SSH connection to the VPN server. RKN sees outbound traffic which is normal.

### Server Setup (Already Done)

1. **User**: `root` on VPN server
2. **GatewayPorts enabled**: `GatewayPorts yes` in `/etc/ssh/sshd_config`
3. **SSH key deployed**: Your Mac's SSH key is authorized on the server

### Mac Setup

The tunnel uses a **self-healing wrapper script** managed by launchd. If the connection drops, it automatically reconnects.

1. **Enable Remote Login on your Mac** (if not already enabled):
   - System Settings → General → Sharing
   - Turn on **Remote Login**
   - Allow access for "All users" (or your user)
2. **Copy the wrapper script** (if not already in place):

```bash
cp ~/coding_projects/vds/servers/2\ -\ VPN/mac-access/ssh-tunnel-wrapper.sh ~/ssh-tunnel-wrapper.sh
chmod +x ~/ssh-tunnel-wrapper.sh
```

3. **Install LaunchAgent** (tunnel starts on Mac login):

```bash
ln -sf ~/Library/LaunchAgents/mac-vpn-tunnel.plist ~/Library/LaunchAgents/mac-vpn-tunnel.plist
launchctl load ~/Library/LaunchAgents/mac-vpn-tunnel.plist
```

4. **Verify tunnel is working**:

```bash
# Check LaunchAgent is running
launchctl list com.tunnel.mac-vpn

# Check port 4444 is listening on VPN server
ssh root@104.128.131.166 "ss -tlnp | grep 4444"
```

Expected output on server: `0.0.0.0:4444` (not `127.0.0.1:4444`)

### How It Works

```
Mac (launchd)          VPS (sshd)              Phone (Termius)
    │                        │                       │
    │ ssh -R *:4444:22       │                       │
    │───────────────────────►│                       │
    │                        │                       │
    │                        │  port 4444 listening  │
    │                        │◄──────────────────────│
    │                        │       SSH on 4444     │
    │◄───────────────────────────────────────────────│
```

The wrapper script (`ssh-tunnel-wrapper.sh`) runs SSH in a loop:

```bash
while true; do
    ssh -R *:4444:localhost:22 -N ...
    sleep 5  # restart after 5s if it dies
done
```

If the tunnel dies (network drop, sleep, etc.), the wrapper restarts it automatically. `KeepAlive: true` in launchd ensures the wrapper itself stays running.

### Managing the Tunnel

**Check tunnel status**:

```bash
launchctl list com.tunnel.mac-vpn
ssh root@104.128.131.166 "ss -tlnp | grep 4444"
```

**Restart tunnel manually**:

```bash
launchctl stop com.tunnel.mac-vpn && sleep 2 && launchctl start com.tunnel.mac-vpn
```

**View tunnel logs**:

```bash
cat /tmp/tunnel-mac-vpn.log
cat /tmp/tunnel-mac-vpn.err
```

**If tunnel disconnects**, launchd auto-restarts it. No manual action needed.


## Locale Configuration

The VDS server has been configured with proper locale support to prevent locale-related errors.

### Issue Fixed

The minimized Ubuntu 24.04 installation was experiencing locale errors:

```
bash: warning: setlocale: LC_ALL: cannot change locale (en_US.UTF-8)
locale: Cannot set LC_CTYPE to default locale: No such file or directory
```

### Solution Applied

Installed the required language pack:

```bash
apt update && apt install -y language-pack-en
```

This automatically generated the `en_US.UTF-8` locale and resolved all locale-related warnings.

For detailed documentation, see [docs/locale-fix-summary.md](docs/locale-fix-summary.md)

## SSH Key Policy

The VDS server enforces a root-only, key-only SSH policy with restricted firewall rules and emergency recovery tooling.

### Security Features

- **Root-only access**: `AllowUsers root` and `PermitRootLogin prohibit-password`
- **Key-only authentication**: `PasswordAuthentication no`, `AuthenticationMethods publickey`
- **Emergency access**: Dedicated emergency SSH key plus helper script
- **Firewall hardening**: UFW default deny incoming, allow SSH/HTTP/HTTPS

### Setup

To enforce the key-only policy on the server:

```bash
# Upload files to server
scp -r scripts/ root@104.128.131.166:/root/
scp -r docs/ root@104.128.131.166:/root/
scp -r config/ root@104.128.131.166:/root/

# Connect to server
ssh -i ~/.ssh/id_rsa root@104.128.131.166

# Run setup script
sudo /root/scripts/setup/setup-ssh-key-policy.sh
```

### Usage

#### Normal Connection

```bash
./scripts/connect.sh
```

#### Emergency Access

```bash
./scripts/connect.sh -e
# Or use the helper script
./scripts/utils/ssh-emergency-access.sh -s
```

#### Manage Authorized Keys

1. Log into the server with emergency or regular access.
2. Append your new key to `/root/.ssh/authorized_keys`:
  ```bash
   cat /path/to/new.pub >> /root/.ssh/authorized_keys
  ```
3. Ensure permissions remain strict:
  ```bash
   chmod 600 /root/.ssh/authorized_keys
   chmod 700 /root/.ssh
  ```
4. If key changes require it, restart SSH: `systemctl restart sshd`

#### Validate Policy

```bash
./scripts/utils/ssh-emergency-access.sh -s
ufw status verbose
cat /etc/ssh/sshd_config.d/99-key-only.conf
```

For detailed documentation, see:

- [docs/ssh-key-deployment.md](docs/ssh-key-deployment.md) - Deployment guide
- [docs/ssh-key-setup.md](docs/ssh-key-setup.md) - Setup instructions
- [docs/ssh-key-recovery.md](docs/ssh-key-recovery.md) - Recovery procedures
- [docs/ssh-key-practices.md](docs/ssh-key-practices.md) - Key policy best practices

## SCP Transfer Performance Fix

### Issue Identified

When transferring files to the VDS server using SCP, transfers would stall at approximately 250KB and then resume after about a minute. This was caused by SSH's default Quality of Service (QoS) settings marking packets with low priority, resulting in network congestion and buffering issues.

### Solution Applied

A permanent fix has been implemented by creating `/etc/ssh/sshd_config.d/99-scp-fix.conf` with the following configuration:

```
IPQoS throughput
```

This configures SSH to mark packets with higher priority for throughput, preventing the buffering issues that were causing stalls.

### Performance Results

After applying the fix:

- **Transfer Speed**: ~8.5 MB/s (consistent for both SCP and SFTP)
- **Behavior**: No more stalling at 250KB
- **Reliability**: Transfers complete smoothly without interruption

### Alternative Workarounds

If you encounter similar issues on other systems, you can use:

```bash
# Use SCP with QoS override
scp -o "IPQoS=throughput" file.txt user@host:/path/

# Use SFTP instead (generally more resilient)
sftp user@host
# Then use: put file.txt /path/

# Use rsync over SSH (if available)
rsync -avz --progress file.txt user@host:/path/
```

## Log Rotation Setup

The VDS server is configured with automated log rotation using logrotate and cron jobs. This ensures logs are properly managed and don't consume excessive disk space.

### Features

- **Automated Rotation**: Daily and weekly log rotation based on log type
- **Compression**: Old logs are compressed to save space
- **Retention Policies**: Different retention periods for different log types
- **Monitoring**: Built-in status checking and alerting
- **Integration**: Works with existing maintenance scripts

### Log Retention Policies

- **Nginx logs**: Daily rotation, 7 days retention
- **Docker logs**: Weekly rotation, 4 weeks retention
- **System logs**: Daily rotation, 7 days retention
- **Application logs**: Daily rotation, 14 days retention

### Setup

To set up log rotation on the server:

```bash
./scripts/setup/setup-logrotate.sh
```

### Monitoring

Check log rotation status:

```bash
/usr/local/sbin/check-logrotate.sh
```

For detailed documentation, see [docs/log-rotation-setup.md](docs/log-rotation-setup.md)

## Repository Structure

```
.
├── README.md                 # This file
├── .vscode/
│   └── tasks.json          # VS Code tasks configuration
├── config/
│   └── server.conf          # Server configuration
├── scripts/
│   ├── cleanup.sh           # Disk cleanup script (integrated with logrotate)
│   ├── connect.sh           # SSH connection utility
│   ├── diagnostics.sh       # System diagnostics script (includes logrotate checks)
│   ├── disk-usage.sh       # Disk usage analysis script
│   ├── setup-logrotate.sh   # Setup automated log rotation
│   ├── setup-ssh-key-policy.sh    # Setup SSH key-only hardening
│   ├── system-update.sh     # System update script (verifies logrotate)
│   ├── common/
│   │   └── remote-exec.sh   # Common remote execution functions
│   ├── cron/
│   │   ├── logrotate-daily.sh    # Daily log rotation script
│   │   └── logrotate-weekly.sh   # Weekly log cleanup script
│   ├── logrotate/
│   │   ├── docker-custom.conf     # Docker logrotate configuration
│   │   ├── system-custom.conf     # System logrotate configuration
│   │   └── application-custom.conf # Application logrotate configuration
│   ├── monitoring/
│   │   └── check-logrotate.sh     # Log rotation status checker
│   ├── test/
│   │   └── test-logrotate.sh      # Test script for log rotation
│   └── utils/
│       ├── force-kill.sh          # Utility script
│       └── ssh-emergency-access.sh # SSH emergency access utility
├── services/
│   ├── traefik/            # Traefik reverse proxy
│   │   ├── config/
│   │   │   └── config.yml
│   │   ├── docker-compose.yml
│   │   └── deploy.sh
│   ├── gatus/              # Health monitoring
│   │   ├── config/
│   │   │   └── config.yaml
│   │   ├── docker-compose.yml
│   │   ├── .env
│   │   └── deploy.sh
│   └── traefik/             # Reverse proxy
│   ├── locale-fix-summary.md     # Locale configuration fix documentation
│   └── log-rotation-setup.md    # Log rotation documentation
├── plans/
│   ├── log-rotation-architecture.md  # Log rotation architecture plan
│   ├── log-rotation-workflow.md      # Log rotation workflow diagrams
│   ├── log-rotation-implementation.md # Detailed implementation guide
│   └── log-rotation-summary.md       # Summary of log rotation setup
└── logs/                    # Directory for operation logs
```

## Quick Start

### Prerequisites

1. SSH access to the server with appropriate keys
2. Bash shell environment

### Deploying Services

**Traefik**:
```bash
cd services/traefik && bash deploy.sh
```

**Gatus**:
```bash
cd services/gatus && bash deploy.sh
```

### Running Diagnostics

To run the diagnostics script:

```bash
./scripts/diagnostics.sh
```

This will:

- Check system resources (CPU, memory, disk)
- Verify service status
- Test network connectivity
- Review security settings
- Generate a summary report

Results are logged to `logs/diagnostics.log`

### Running Cleanup

To run the disk cleanup script:

```bash
./scripts/cleanup.sh
```

This will:

- Clean package manager cache (APT/YUM/DNF)
- Rotate and clean old log files
- Remove temporary files
- Clean Docker resources (if installed)
- Remove old backups
- Clear system cache

Results are logged to `logs/cleanup-YYYYMMDD-HHMMSS.log`

**Note**: This script must be run as root.

## Configuration

Server settings can be modified in `config/server.conf`:

- SSH connection details
- Services to monitor
- Alert thresholds
- Log paths

## Maintenance Scripts

### Diagnostics (`scripts/diagnostics.sh`)

Comprehensive system health check including:

- System resource utilization
- Service status monitoring (Traefik, Gatus, Amnezia containers)
- Network connectivity tests
- Security configuration review
- Amnezia container checks (AWG, AWG2)

### Cleanup (`scripts/cleanup.sh`)

Disk space cleanup script that performs:

- Package cache cleanup (APT/YUM/DNF)
- Log rotation and cleanup
- Temporary file removal
- Docker resource cleanup (without stopping containers)
- Old backup removal
- System cache clearing

### Connect (`scripts/connect.sh`)

SSH connection utility that provides:

- Interactive SSH sessions
- Remote command execution
- SSH tunnel creation

### Disk Usage (`scripts/disk-usage.sh`)

Disk usage analysis script that:

- Shows overall disk usage
- Analyzes usage by directory
- Identifies large files
- Checks Docker space usage

### System Update (`scripts/system-update.sh`)

System maintenance script that:

- Updates system packages
- Creates backups before updates
- Verifies service status after updates
- Checks if reboot is required

## VS Code Integration

The repository includes VS Code tasks configuration in `.vscode/tasks.json`. Each script can be run as a build task:

- Run Cleanup Script
- Run Connect Script
- Run Diagnostics Script
- Run Disk Usage Script
- Run System Update Script
- Setup Autostart Script
- Setup Nginx SSL Script

Access these tasks through Command Palette (Ctrl+Shift+P) → "Tasks: Run Task"

## Logs

All operations are logged to the `logs/` directory:

- `diagnostics.log` - Output from diagnostic runs
- `cleanup-YYYYMMDD-HHMMSS.log` - Output from cleanup runs

## Security Notes

- This repository contains sensitive server information
- Ensure proper access controls are in place
- SSH keys should be properly secured
- Review logs regularly for unusual activity
