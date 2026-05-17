# Service configurations — 5 - Hermes

**Host**: `132.243.213.9` (SSH port 18718)

## OpenCrabs

### Default profile (`opencrabs.service`)

- **Role**: General AI agent (v0.3.19) — Telegram bot with dynamic tools via remote MCP (`tg-mcp.l1979.ru`)
- **Telegram bot**: [@oc_l1979_bot](https://t.me/oc_l1979_bot)
- **Binary**: `/usr/local/bin/opencrabs` (linux-amd64)
- **Systemd**: `opencrabs.service`
- **Data dir**: `/root/.opencrabs/` (opencrabs.db, logs/, tools.toml, keys.toml, config.toml)
- **Config**: `/root/.opencrabs/config.toml` — MiniMax provider, Telegram channel
- **8 dynamic tools**: via `fastmcp call` to `https://tg-mcp.l1979.ru/v1/mcp`
- **Known issue**: tools.toml tools work via Telegram channel but not in `run`/`agent` modes ([opencrabs#79](https://github.com/adolfousier/opencrabs/issues/79))

### Ops profile (`opencrabs-ops.service`)

- **Telegram bot**: [@redevest_admin_tools_bot](https://t.me/redevest_admin_tools_bot) — Gatus host alerts + VDS triage
- **Profile dir**: `/root/.opencrabs/profiles/ops/` (SOUL.md, AGENTS.md, keys.toml — not in git)
- **Brain templates**: `5 - hermes/opencrabs-brain/` → deploy via `scripts/deploy-opencrabs-ops-brain.sh`
- **Setup**: `scripts/deploy-opencrabs-ops-profile.sh` (profile, systemd, nightly cron)
- **Infra repo**: `git@github.com:leshchenko1979/servers.git` → `/root/vds-servers` (`scripts/setup-vds-servers-git.sh`)
- **SSH fleet**: `5 - hermes/config/ssh-config` → `scripts/install-hermes-ssh-config.sh` (shared `id_ed25519`, hosts `vpn`/`apps`/`n8n`)
- **Nightly**: `opencrabs -p ops cron` `vds-servers-nightly-pull` @ 03:00 Europe/Moscow — `git pull --ff-only` on `/root/vds-servers`

## Hermes Agent

- **Status**: Temporarily offline
- **Role**: AI agent (Nous Research Hermes v0.13.0) — Telegram-polling assistant with persistent memory (was active before OpenCrabs)
- **Runtime**: Python 3.12 + `uv` venv at `/usr/local/hermes-agent/.venv/`
- **Systemd**: `hermes-gateway.service` (stopped)
- **Data dir**: `/root/.hermes/` (state.db, sessions/, skills/, memories/)

## Caddy

`claw.l1979.ru` → proxies to port `18790` (A2A gateway). HTTP endpoints on Hermes itself.

## Box 5 specs

- 1GB RAM / 58GB disk
- Ubuntu 24.04 LTS
- 1GB swap file

## SSH

Connect: `ssh hermes` (configured in `~/.ssh/config`)