# Service configurations — 5 - Hermes

**Host**: `132.243.213.9` (SSH port 18718)

## OpenCrabs

### Deploy (from Mac)

Single entry: [`scripts/deploy-opencrabs.sh`](../scripts/deploy-opencrabs.sh)

| Flag | Use |
|------|-----|
| `--status` | Health checks only (default when no flags) |
| `--bootstrap` | First-time ops profile, seed missing config/keys, systemd, cron, tg-tools, config patch |
| `--bootstrap --no-ssh` | Bootstrap without `install-hermes-ssh-config.sh` |
| `--config` | Patch memory keys, strip `[mcp]`, optional ops token from `.env` |
| `--config --sync-provider-keys` | Also copy provider `api_key` from default → ops `keys.toml` |
| `--brain` | Deploy brain `*.md` from repo (backs up first); **does not** overwrite ops `MEMORY.md` |
| `--tg-tools` | `tg-mcp-call`, `/etc/tg-mcp/mcp.json` (bearer), `tools.toml` on default + ops |
| `--tg-tools --update-tools` | Force overwrite both `tools.toml` files |
| `--ssh` | Fleet SSH config only |

**Typical flows:**

```bash
cd "5 - hermes/scripts"
./deploy-opencrabs.sh --status
./deploy-opencrabs.sh --bootstrap          # first-time ops setup
./deploy-opencrabs.sh --tg-tools           # wrapper / mcp.json / tools
./deploy-opencrabs.sh --config             # memory + migration patches
./deploy-opencrabs.sh --brain              # repo policy docs only (rare)
```

**Migration** (existing host after redesign): `--tg-tools` then `--config`; redeploy Gatus bridge from VPN (`deploy-gatus-bridge.sh` reads bearer from `/etc/tg-mcp/mcp.json`).

**Secrets:** `5 - hermes/.env` from `.env.example` — `REDEVEST_ADMIN_BOT_TOKEN`, optional `TG_MCP_BEARER` (rendered into `/etc/tg-mcp/mcp.json`, not OpenCrabs `config.toml`).

**Polluted brain** (RSI bloat): manual on Hermes — backup then restore or edit `~/.opencrabs/profiles/ops/AGENTS.md` etc.; no automated force-reset. To push repo policy back: `./deploy-opencrabs.sh --brain` (accepts overwrite of AGENTS/SOUL; not MEMORY).

### Default profile (`opencrabs.service`)

- **Role**: General AI agent — Telegram bot with dynamic tools via remote MCP (`tg-mcp.l1979.ru`)
- **Telegram bot**: [@oc_l1979_bot](https://t.me/oc_l1979_bot)
- **Binary**: `/usr/local/bin/opencrabs` (linux-amd64)
- **Systemd**: `opencrabs.service`
- **Data dir**: `/root/.opencrabs/` (opencrabs.db, logs/, tools.toml, keys.toml, config.toml)
- **Memory**: `vector_enabled = false`, `auto_update = true` (1GB RAM). Patched via `deploy-opencrabs.sh --config`
- **8 dynamic tools**: `tools.toml` → `/usr/local/bin/tg-mcp-call` → `fastmcp call` → `https://tg-mcp.l1979.ru/v1/mcp` (auth from `/etc/tg-mcp/mcp.json`; logs `/var/log/tg-mcp/tg-mcp-call.log`)
- **Debugging tg tools**: `ssh hermes 'tail -f /var/log/tg-mcp/tg-mcp-call.log'`
- **tools.toml updates**: `./deploy-opencrabs.sh --tg-tools --update-tools`
- **Known issue**: tools.toml tools work via Telegram channel but not in `run`/`agent` modes ([opencrabs#79](https://github.com/adolfousier/opencrabs/issues/79))

### Ops profile (`opencrabs-ops.service`)

- **Telegram bot**: [@redevest_admin_tools_bot](https://t.me/redevest_admin_tools_bot) — Gatus host alerts + VDS triage
- **Profile dir**: `/root/.opencrabs/profiles/ops/` (SOUL.md, AGENTS.md, CODE.md, keys.toml, tools.toml — live, not in git)
- **Config templates**: `opencrabs-profiles/ops/*.template` — full seed only on `--bootstrap` when files missing; otherwise `--config` patches
- **Brain templates**: `opencrabs-brain/` — deploy with `--brain` only
- **Memory**: `vector_enabled = false`, `auto_update = true`; use `load_brain_file MEMORY.md`, not `memory_search`
- **Systemd**: `opencrabs-ops.service.d/memory.conf` — `MemoryMax=512M`; units from `opencrabs service install` / `opencrabs -p ops service install`
- **Token guard**: `opencrabs-guard-default-keys.sh` on `--brain` deploy
- **Infra repo**: `/root/vds-servers` — `setup-vds-servers-git.sh`; nightly `git pull` via ops cron
- **SSH fleet**: `config/ssh-config` → `install-hermes-ssh-config.sh` or `deploy-opencrabs.sh --ssh`
- **A2A (ops):** loopback `:18791` — on ice until [opencrabs#92](https://github.com/adolfousier/opencrabs/issues/92); see `2 - VPN/services/gatus/scripts/spike-notes.md`

### Brain files per profile (`--brain` deploy)

| File | default `@oc_l1979_bot` | ops `@redevest_admin_tools_bot` |
|------|------------------------|--------------------------------|
| SOUL.md | yes (`--brain`) | yes (`OPS_SOUL.md`) |
| USER.md | yes (`DEFAULT_USER.md`) | yes |
| AGENTS.md | — | yes |
| CODE.md | — | yes |
| MEMORY.md | never deployed | **excluded** from `--brain`; bootstrap seeds if missing only |
| SYSTEM.md | yes | yes |
| config.toml / keys.toml | default live only | bootstrap seed if missing; else `--config` patch |
| tools.toml | `--tg-tools` | `--tg-tools` (same template, both paths) |

## OpenCrabs Repo Clones

| Repo | OpenCrabs Profile | Purpose |
|------|-------------------|---------|
| `/root/vds-servers/` | ops | Fleet docs, Gatus, brain templates, SSH config |
| `/root/redevest-ai/` | default | RedeVest business assistant |
| `/root/tgproxy/` | — (cron) | Proxy list publisher; `mcp.json` → `/etc/tg-mcp/mcp.json` |

**`vds-servers`:** `setup-vds-servers-git.sh`; brain via `deploy-opencrabs.sh --brain`.

**`tgproxy`:** `update_proxies.py` uses `tg-mcp-call`; cron daily @ 06:00 Moscow.

## Hermes Agent

- **Status**: Temporarily offline
- **Systemd**: `hermes-gateway.service` (stopped)
- **Data dir**: `/root/.hermes/`

## Caddy

`claw.l1979.ru` → port `18790` (A2A gateway).

## Box 5 specs

- 1GB RAM / 58GB disk — Ubuntu 24.04 LTS — 1GB swap

## SSH

Mac/VPN: `ssh hermes` (`132.243.213.9:18718`). On Hermes: `ssh hermes` → `127.0.0.1:22` (`install-hermes-ssh-config.sh`).
