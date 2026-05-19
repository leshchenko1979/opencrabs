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
- **Memory**: `vector_enabled = false`, `auto_update = true` (1GB RAM — no ChromaDB; FTS + daily notes re-indexed). Set on deploy via `deploy-opencrabs-ops-profile.sh`
- **8 dynamic tools**: `tools.toml` → `/usr/local/bin/tg-mcp-call` (source `scripts/tg-mcp-call.py`; strips JSON `null` keys before MCP — tg-mcp rejects null optional ints) → `fastmcp call` → `https://tg-mcp.l1979.ru/v1/mcp` (lean tool JSON on stdout; config `/etc/tg-mcp/mcp.json`; logs `/var/log/tg-mcp/tg-mcp-call.log`). Deploy: `scripts/deploy-opencrabs-tg-tools.sh` (`fastmcp-slim[client]`)
- **Debugging tg tools**: `ssh hermes 'tail -f /var/log/tg-mcp/tg-mcp-call.log'`. Lines: `call start` / `call ok` / `call failed` / `exit_code=N`. Parse errors include `raw_preview` (malformed JSON from `tools.toml` shell expansion). MCP errors: `fastmcp_rc`, `stderr_preview`. One-off verbose args: `TG_MCP_LOG_LEVEL=DEBUG` (up to 2KB preview). No bearer in logs.
- **tools.toml updates**: `opencrabs-tools.toml.template` uses JSON-safe `default` on optional params (`null`, `[]`, `""`). Deploy: `./scripts/deploy-opencrabs-tg-tools.sh --update-tools` then restart `opencrabs` / `opencrabs-ops`.
- **Known issue**: tools.toml tools work via Telegram channel but not in `run`/`agent` modes ([opencrabs#79](https://github.com/adolfousier/opencrabs/issues/79))

### Ops profile (`opencrabs-ops.service`)

- **Telegram bot**: [@redevest_admin_tools_bot](https://t.me/redevest_admin_tools_bot) — Gatus host alerts + VDS triage
- **Profile dir**: `/root/.opencrabs/profiles/ops/` (SOUL.md, AGENTS.md, CODE.md, keys.toml — not in git)
- **Config templates**: `5 - hermes/opencrabs-profiles/ops/config.toml.template` + `keys.toml.template` (placeholders only; rendered on Mac)
- **Brain templates**: `5 - hermes/opencrabs-brain/` — thin pointers to `/root/vds-servers`. Deploy: `scripts/deploy-opencrabs-ops-brain.sh`. Re-deploy after OpenCrabs RSI template sync if files balloon.
- **Setup**: `scripts/deploy-opencrabs-ops-profile.sh` (profile, systemd, nightly cron) — calls brain deploy at end
- **Memory**: `vector_enabled = false`, `auto_update = true` in `opencrabs-profiles/ops/config.toml.template` (same 1GB constraint; use `load_brain_file MEMORY.md`, not `memory_search`)
- **Systemd**: `opencrabs-ops.service.d/memory.conf` — `MemoryMax=512M` safety cap; units from `opencrabs service install` / `opencrabs -p ops service install` (do not hand-edit unit files)
- **Token guard**: `scripts/opencrabs-guard-default-keys.sh` runs on brain deploy (strips `[channels.*]` from default `keys.toml`, verifies distinct bots via Telegram `getMe`)
- **Deploy secrets**: copy `5 - hermes/.env.example` → `.env` with `REDEVEST_ADMIN_BOT_TOKEN` (optional `TG_MCP_BEARER`; MiniMax/MCP otherwise read from default profile on hermes)
- **Infra repo**: `git@github.com:leshchenko1979/servers.git` → `/root/vds-servers` (`scripts/setup-vds-servers-git.sh`)
- **SSH fleet**: `5 - hermes/config/ssh-config` → `scripts/install-hermes-ssh-config.sh` (shared `id_ed25519`, hosts `vpn`/`apps`/`n8n`)
- **Nightly**: `opencrabs -p ops cron` `vds-servers-nightly-pull` @ 03:00 Europe/Moscow — `git pull --ff-only` on `/root/vds-servers` only (no push; brain policy files are deployed from Mac via `deploy-opencrabs-ops-brain.sh`)
- **A2A (ops):** loopback `:18791` — spike to replace Gatus tg-mcp trigger **on ice** until [opencrabs#92](https://github.com/adolfousier/opencrabs/issues/92); see `2 - VPN/services/gatus/scripts/spike-notes.md`

### Brain files per profile (deployed from repo)

| File | default `@oc_l1979_bot` | ops `@redevest_admin_tools_bot` |
|------|------------------------|--------------------------------|
| SOUL.md | `opencrabs-brain/SOUL.md` | `opencrabs-brain/OPS_SOUL.md` |
| USER.md | `opencrabs-brain/DEFAULT_USER.md` | `opencrabs-brain/USER.md` |
| AGENTS.md | — | yes |
| CODE.md | — | yes (plan + TDD; load via SOUL/AGENTS) |
| MEMORY.md | not overwritten (live RedeVest context) | yes |
| SYSTEM.md | yes | yes |
| config.toml / keys.toml | default profile only | rendered from `opencrabs-profiles/ops/*.template` |

## OpenCrabs Repo Clones

OpenCrabs operates from repo clones in `/root/` — not a single git monorepo. Each clone serves a specific profile:

| Repo | Size | OpenCrabs Profile | Purpose |
|------|------|-------------------|---------|
| `/root/vds-servers/` | 1.5M | ops (`@redevest_admin_tools_bot`) | Fleet docs, Gatus config, brain templates, SSH fleet config |
| `/root/redevest-ai/` | 75M | default (`@oc_l1979_bot`) | RedeVest business assistant — website, memory-bank, channel posts, scripts |
| `/root/tgproxy/` | 1.2M | — (cron-driven) | Telegram proxy list publisher at tgproxy.l1979.ru (A2A channel [@telemtrs](https://t.me/telemtrs)) |

**`vds-servers` clone details:**
- Source: `git@github.com:leshchenko1979/servers.git`
- Setup: `5 - hermes/scripts/setup-vds-servers-git.sh`
- Nightly pull: `opencrabs -p ops cron` `vds-servers-nightly-pull` @ 03:00 Europe/Moscow (`git pull --ff-only` only, no push)
- Brain templates live at `5 - hermes/opencrabs-brain/` and are deployed via `scripts/deploy-opencrabs-ops-brain.sh`

**`redevest-ai` clone details:**
- Source: `git@github.com:leshchenko1979/redevest-ai.git`
- Contains: website at rede-vest.ru, `memory-bank/Редевест/`, `channel-posts/`, `scripts/`, `docs/`
- OpenCrabs default profile uses it as working context

**`tgproxy` details:**
- Script: `update_proxies.py` — calls `/usr/local/bin/tg-mcp-call` → fastmcp → tg-mcp; fetches [@telemtrs](https://t.me/telemtrs) topic "Free proxy" (topic_id: 16160)
- Repo: `git@github.com:leshchenko1979/tgproxy.git` → `/root/tgproxy` (`mcp.json` → `/etc/tg-mcp/mcp.json`)
- Published to: GitHub Pages at tgproxy.l1979.ru
- Cron: daily @ 06:00 Moscow time (OpenCrabs job ID: `6da1f200`)

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

Connect from Mac/VPN: `ssh hermes` (public `132.243.213.9:18718`, in `~/.ssh/config`).

On Hermes itself: `ssh hermes-local` → `127.0.0.1:22` for box5 diagnostics (never `ssh hermes` from Hermes — no NAT hairpin). Fleet config: `5 - hermes/config/ssh-config`; install: `scripts/install-hermes-ssh-config.sh`.