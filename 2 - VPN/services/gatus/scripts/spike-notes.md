# Gatus → OpenCrabs ops A2A

## Production

| Item | Detail |
|------|--------|
| Path | Gatus `custom` alert → `http://172.18.0.1:18791/a2a/v1` (JSON-RPC `message/send`) |
| Tunnel | VPN systemd `gatus-a2a-tunnel` — SSH `-L 172.18.0.1:18791:127.0.0.1:18791` to Hermes `:18718` |
| Fleet key | Same `id_ed25519` as all boxes → `/root/.ssh/id_ed25519` on VPN (deploy script) |
| Ops agent | OpenCrabs ops A2A `:18791` → `host-diag` + `telegram_send` → @redevest_admin_tools_bot |
| Alerts | Custom on `host-box2`–`host-box5` only (Telegram alert type remains for general chat) |
| Bridge | **Removed** — scripts in `legacy/`; production uses A2A tunnel |

**Deploy:** `./scripts/deploy-gatus.sh` (from `2 - VPN/`) — installs tunnel + renders config. Tunnel only: `services/gatus/scripts/deploy-gatus-a2a-tunnel.sh`.

**Ops brain:** `git pull /root/vds-servers` before `[Gatus]`; deploy brain from Mac via `deploy-opencrabs.sh --brain`.

## Architecture

```
Gatus (Docker, VPN) ──POST /a2a/v1──► 172.18.0.1:18791 (SSH tunnel)
                                              │
                                              ▼
                                    Hermes ops A2A 127.0.0.1:18791
                                              │
                                              ▼
                                    host-diag + telegram_send
```

## Hermes SSH loopback (fixed `a39070ca`, 2026-05-20)

On Hermes, fleet `ssh-config` sets **`Host hermes` → `127.0.0.1:22`**. Deploy: `5 - hermes/scripts/install-hermes-ssh-config.sh`.

## Spike history (2026-05-21)

**Blocker (fixed):** [opencrabs#92](https://github.com/adolfousier/opencrabs/issues/92) — tool approval on A2A in v0.3.23+.

| Hypothesis | Result |
|------------|--------|
| H1 — A2A vs Telegram sessions separate | Pass |
| H3 — thin trigger + AGENTS → tools + Telegram | Pass ×3 |
| H4 — VPN → Hermes SSH → A2A | Pass (fleet key on VPN + tunnel) |

**Harness:** `spike_a2a_telegram.py` — `health`, `a2a-trigger`, `ssh-path`

**Ops note:** if `/a2a/health` stalls, restart `opencrabs-ops` (daemon can hang; orphan PIDs from old restarts).

### Historical (2026-05-19)

Bridge era: `9081/gatus/alert` → tg-mcp. H3 failed on OpenCrabs 0.3.22 (no A2A tool approval).
