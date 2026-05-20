# Gatus → OpenCrabs bridge — notes

## Production (locked)

| Item | Detail |
|------|--------|
| Bridge | `http://172.18.0.1:9081/gatus/alert` → tg-mcp → @redevest_admin_tools_bot |
| Alerts | Custom on `host-box2`–`host-box5` only |
| Ops brain | `git pull /root/vds-servers` before `[Gatus]`; deploy brain from Mac via `deploy-opencrabs-ops-brain.sh` |

**Post-deploy fixes:** ops `keys.toml` uses `[channels.telegram]`; `opencrabs-guard-default-keys.sh` on brain deploy (2026-05-19).

## Hermes SSH loopback (fixed `a39070ca`, 2026-05-20)

On Hermes, fleet `ssh-config` sets **`Host hermes` → `127.0.0.1:22`** so `ssh hermes` works for ops agents (public `:18718` from inside the box still has no hairpin). **box5:** `ssh hermes /usr/local/bin/host-diag` or `/usr/local/bin/host-diag`. Deploy: `5 - hermes/scripts/install-hermes-ssh-config.sh`. See `AGENTS.md` **SSH targets** and `CLAUDE.md` Session Corrections.

## A2A trigger spike — ON ICE (2026-05-19)

**Tracking:** [servers#1](https://github.com/leshchenko1979/servers/issues/1) · **Blocker:** [opencrabs#92](https://github.com/adolfousier/opencrabs/issues/92)

**Decision:** Keep tg-mcp → Telegram trigger. Do not switch Gatus to A2A until #92 is fixed and `spike_a2a_telegram.py a2a-trigger` ×3 passes.

| Hypothesis | Result |
|------------|--------|
| H1 — A2A vs Telegram sessions separate | Pass |
| H3 — thin trigger + AGENTS → tools + Telegram | Fail — tools blocked (*no approval mechanism*) despite `approval_policy = auto-always` |
| H4 — VPN → Hermes SSH → A2A | Fail — pubkey not on VPN (orthogonal to #92) |

**Root cause:** OpenCrabs 0.3.22 — A2A `create_agent_service()` omits `with_auto_approve_tools`; `send.rs` has no approval callback (Telegram uses `check_approval_policy()` via callback).

**Harness (re-test later):** `spike_a2a_telegram.py` — `health`, `baseline`, `a2a-trigger`, `ssh-path`

**Revisit:** upgrade Hermes `opencrabs` after #92 release → deploy brain → `a2a-trigger` ×3 → optional VPN SSH key → only then consider bridge `A2A_MODE`.
