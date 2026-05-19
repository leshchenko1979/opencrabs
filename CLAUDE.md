# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

Multi-server VDS (Virtual Dedicated Server) management repo for **l1979.ru** ecosystem. Each numbered directory is an independent operations repository for a separate VDS.


| Box            | IP              | Services                                                                                             |
| -------------- | --------------- | ---------------------------------------------------------------------------------------------------- |
| 2 - VPN        | 104.128.131.166 | AmneziaWG VPN (awg0), Traefik, Gatus, reverse SSH tunnel (port 4444), mac-access tunnel               |
| 3 - apps       | 144.31.188.163  | Traefik + Sablier, PostgreSQL, Redis, Redevest CRM, pdf-extract, AI Antispam, Business Tinder, TG MCP |
| 4 - n8n        | 2.27.120.75     | Caddy, n8n                                                                                          |
| 5 - hermes     | 132.243.213.9:18718 | Hermes Agent (Telegram AI assistant, Nous Research v0.13)                                        |


## Architecture

### Common Pattern Across All Servers

Each server directory follows the same structure:

- `**.env` / `.env.example`** — SSH credentials (`REMOTE_HOST_IP`, `REMOTE_USER`, `SSH_KEY`), service secrets
- `**scripts/`** — Deployment and maintenance scripts (deploy-<service>.sh per service)
- `**mac-access/`** — macOS SSH tunnel setup (launchd plist + watchdog scripts)
- `**docs/**` — Server-specific documentation
- `**services/**` — Docker Compose configurations for deployed services

### SSH Access Pattern

All servers are accessed via SSH with key-based authentication. Each server's `.env` contains:

- `REMOTE_HOST_IP` — Server IP
- `REMOTE_USER` — SSH user (typically `root`)
- `SSH_KEY` — Path to SSH private key (`~/.ssh/id_ed25519`)

On your **macOS workstation**, add matching `Host` blocks in `~/.ssh/config` so you can connect without typing IPs (same pattern as `Host claw`):

| `ssh …` | Box | `HostName` |
| ------- | --- | ---------- |
| `ssh vpn` | 2 – VPN | `104.128.131.166` |
| `ssh apps` | 3 – apps | `144.31.188.163` |
| `ssh n8n` | 4 – n8n | `2.27.120.75` |
| `ssh hermes` | 5 – hermes | `132.243.213.9` (port 18718) |

Each block should use `User root`, `IdentityFile ~/.ssh/id_ed25519`, and `AddKeysToAgent yes` (or the key you use in per-server `.env`).

Repo scripts that SSH from your Mac source `scripts/ssh-vds-host.sh` and call `vds_ssh_connect_host` so connections use `vpn` / `apps` / `n8n` / `hermes` when `REMOTE_HOST_IP` matches a canonical box IP (URLs and `.env` stay on real IPs).

### macOS SSH Tunnel (Box 2)

Box 2 runs a reverse SSH tunnel via `mac-access/`:
- `com.leshchenko.tmux-tunnel.plist` — launchd agent, starts tunnel on boot
- `ssh-tunnel-watchdog.sh` — monitors and restarts tunnel if port 4444 drops
- `ssh-tunnel-wrapper.sh` — self-healing wrapper (auto-restarts on disconnect)

### Unified Scripts (Repo Root)

```bash
./scripts/diagnostics-unified.sh  # RAM/OOM/swap/disk across all servers
./scripts/cleanup-unified.sh      # APT/docker/journal cleanup + weekly cron
```

### Service Deployment

Services are deployed via Docker with **Traefik** as the reverse proxy (except on box 4). SSL/TLS uses Let's Encrypt via Traefik's ACME resolver (`certResolver: le`). **Sablier** provides scale-to-zero for idle containers.

### Monitoring

**Gatus** (on VPN server) monitors endpoints across all servers with Telegram alerting. **Host failures** (`host-box2`–`host-box5`) also POST to `gatus-opencrabs-bridge` (`:9081` on VPN host) → tg-mcp → **@redevest_admin_tools_bot** → OpenCrabs **ops** profile on hermes (`git pull /root/vds-servers`, SSH `host-diag`). General chat stays on **@oc_l1979_bot** (default profile). Deploy: `2 - VPN/services/gatus/scripts/deploy-gatus-bridge.sh` (set `GATUS_BRIDGE_SECRET` in `.env.gatus` on first run) then `deploy-gatus.sh`; hermes: `5 - hermes/scripts/deploy-opencrabs-ops-profile.sh` (needs `5 - hermes/.env` from `.env.example`). Ops brain **`CODE.md`** (`5 - hermes/opencrabs-brain/CODE.md`): plan tool + TDD — deploy via `deploy-opencrabs-ops-brain.sh`.

## Common Commands

### Diagnostics (all servers)

```bash
# Unified (all boxes at once — auto-discovers servers from numbered dirs)
cd /path/to/servers
./scripts/diagnostics-unified.sh           # All servers, logs to terminal + logs/diagnostics.log
./scripts/diagnostics-unified.sh --no-log  # Console only

# Direct SSH (single box) — or: ssh vpn | ssh apps | ssh n8n | ssh hermes
ssh root@<IP> "df -h && free -h && docker ps 2>/dev/null || systemctl status n8n caddy"
```

### Service Deployment

```bash
cd "N - server-name"
./scripts/deploy-<service>.sh         # Deploy specific service (box 3 only)
```

## Key Documentation


| Doc                                    | Server | Contents                                                                |
| -------------------------------------- | ------ | ----------------------------------------------------------------------- |
| `3 - apps/docs/services.md`            | apps   | Full service catalog (configs, deploy commands, networks, memory notes) |
| `2 - VPN/README.md`                    | VPN    | AmneziaWG VPN, reverse SSH tunnel, Gatus, mac-access tunnel             |
| `2 - VPN/mac-access/`                  | VPN    | launchd plist, watchdog and wrapper scripts for reverse SSH tunnel       |
| `4 - n8n/docs/services.md`      | n8n   | Caddy, n8n                                                                                         |
| `4 - n8n/docs/n8n-migration.md` | n8n   | Legacy n8n migration history                                                                      |
| `5 - hermes/docs/services.md`   | hermes | Hermes Agent (Telegram AI, Nous Research v0.13)                                                   |
| `scripts/cleanup-unified.sh`    | all    | APT/docker/journal cleanup — weekly cron job                                                      |


### Backup (mac-workstation-backup/)

Daily automated backup from apps + n8n + hermes to external volume. Uses SSH config host aliases.

```bash
# Manual run
cd mac-workstation-backup
BACKUP_ROOT=/Volumes/leshchenko/vds-backups \
  POSTGRES_PASSWORD=... REDIS_PASSWORD=... \
  ./backup-all.sh

# Cron: 03:00 daily (see crontab.example)
```

Backs up: apps (PostgreSQL, Redis, /data/projects configs), n8n (/var/lib/n8n, /etc/n8n.env), hermes (/root/.hermes/). Retention: 7 days.

See `mac-workstation-backup/README.md` for full recovery procedures.

## Skills

Project-level skill: `.claude/skills/diagnosing-servers/SKILL.md`

**Important note:** `bc` is not installed on boxes — use `awk` for numeric comparisons.

**Script development lessons learned:**
- `journalctl --disk-usage` outputs `170.4M` (not `MB`) — `grep -oE 'M'` works, `grep -oE '[MG]B'` does not
- Always check if required tools (`bc`, `jq`, etc.) exist on remote boxes before using in scripts
- Journal vacuum can free 0B even when journal is large — active journals can't be vacuumed, need `SystemMaxUse` config

## Critical Rules

1. **Always use SSH aliases** — `ssh vpn`, `ssh apps`, `ssh hermes`, `ssh n8n` instead of raw IPs. The short aliases are configured in `~/.ssh/config`.
2. **Per-server context**: Read the server-specific README before working in any numbered directory
2. **`mac-access/` is server-specific**: Only box 2 has this directory — do not assume other boxes have SSH tunnel configs
3. **Check local folders on every question**: Read relevant files (configs, scripts, docs) to load context before answering
4. **Log and update docs after config changes**: After every config deployment or change, update CLAUDE.md and server docs
5. **`*.env` is sensitive**: Never commit `.env` files (they're in `.gitignore`)
6. **Sablier stack**: Never prune `redevest-crm`, `redevest-crm-test`, `pdf-extract` — scale-to-zero managed by Sablier. **Do not prune** the **`sablierapp/sablier`** image (the running `sablier` container / Traefik plugin stack depends on it)
7. **First character workaround**: In shell commands, sometimes the first character disappears — insert a space before commands
8. **Log mistakes and solutions in CLAUDE.md**: When a script fails, a command doesn't work on a box, or a tool is missing, document the problem and solution in CLAUDE.md under a dedicated section. Future runs must not repeat the same mistake.

### Critical Safety Notes

- **`2 - VPN/scripts/cleanup.sh` `remove_unused_services()` is DISABLED**: That function unconditionally deletes PostgreSQL and Redis data directories if they appear "unused" (no active connections). This would destroy production databases. The function was gutted on 2026-05-04 to just log a warning. Never re-enable the body without extensive review.

## Session Corrections

### 2026-05-19
- **Issue**: Hermes `tools.toml` used `/root/.local/bin/mcp tg-mcp` (14MB proxy); hung on 1GB RAM. Removing the proxy broke `tgproxy` cron before `update_proxies.py` was migrated.
  - **Correct approach**: Use `fastmcp-slim[client]` + `/usr/local/bin/tg-mcp-call` (wraps `fastmcp call` with bearer from `config.toml`). Deploy via `5 - hermes/scripts/deploy-opencrabs-tg-tools.sh`. Migrate tgproxy to `tg-mcp-call` before deleting the old `mcp` binary. `fastmcp call` with `--json` duplicates data for agents — wrapper uses default CLI output (lean JSON only). Wrapper logs to `/var/log/tg-mcp/tg-mcp-call.log` (no bearer; args truncated).
- **Issue**: OpenCrabs ops `vector_enabled = true` (ChromaDB) → ~800MB RSS, OOM loop, SSH banner timeouts on 1GB Hermes.
  - **Correct approach**: Set `vector_enabled = false` in ops template and default `config.toml` (omitting the key may still load ChromaDB); redeploy via `deploy-opencrabs-ops-profile.sh` (restarts both daemons). Keep `MemoryMax=512M` drop-in on `opencrabs-ops`. Ops brain: `load_brain_file MEMORY.md`, not `memory_search`.

### 2026-05-04
- **Issue**: Failed to find gatus redeployment script — searched for "gatus" in yaml files and globbed for docker-compose, found the location but missed the deploy scripts
  - **Correct approach**: When looking for a service's deployment mechanism, use `ls` on the service directory to find all files including `deploy.sh` scripts. Don't just grep for config files — list the directory contents first.

### 2026-04-23
- **Issue**: Deployed macOS `ai-gateway` binary to Linux box 4 via rsync
  - **Correct approach**: Server binaries are Linux x86_64. Always verify architecture with `ssh root@<box> uname -m` before rsyncing, or download the correct platform binary from GitHub releases (`picoclaw_Linux_x86_64.tar.gz`).
- **Issue**: Confused `deployed_projects/ai-gateway/` with picoclaw — they are unrelated projects
  - **Correct approach**: picoclaw on box 4 is `sipeed/picoclaw` from GitHub releases at `/usr/bin/picoclaw`. The `ai-gateway` project is a separate Go service for a different purpose.