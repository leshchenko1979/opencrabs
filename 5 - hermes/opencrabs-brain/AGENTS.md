# AGENTS — ops runbook

Load **MEMORY.md** after pull. For repo edits, also load **CODE.md**.

## Before acting

For **`[Gatus]` alerts** and **infra chat** (fleet config, deploy, “check box N”):

1. `git -C /root/vds-servers pull --ff-only` (report failure; continue if possible).
2. `load_brain_file MEMORY.md` — use repo paths listed there, not recalled IPs or services.
3. For Gatus: also read `2 - VPN/services/gatus/config/config.yaml` after pull.

## `[Gatus]` alerts

Host alerts arrive via **A2A** (not Telegram ingress). Same triage either way.

1. **Ack first** — one-line `telegram_send` to chat `133526395`, then investigate.
2. Map endpoint → **SSH targets** (below); run `host-diag` on that box.
3. If exit ≠ 0: `uptime`, `free -h`, `df -h`, `docker ps` or `systemctl` as needed.
4. Report: metrics, likely cause, **safe** fixes only — or ask Alexey.
5. **Final** `telegram_send` when done (required for A2A; overrides OPS_SOUL “no telegram in active Telegram session” — that rule is Telegram-channel sessions only).

Safety rules: `/root/vds-servers/CLAUDE.md`.

## SSH targets (host-box2–5)

On **Hermes (box5)**. `Host hermes` → `127.0.0.1:22` in `~/.ssh/config` (not public `:18718` — no hairpin).

| Endpoint | `host-diag` |
|----------|-------------|
| `host-box2` | `ssh vpn /usr/local/bin/host-diag` |
| `host-box3` | `ssh apps /usr/local/bin/host-diag` |
| `host-box4` | `ssh n8n /usr/local/bin/host-diag` |
| `host-box5` | `/usr/local/bin/host-diag` or `ssh hermes /usr/local/bin/host-diag` |

Box5 MCP stuck: `ps aux | grep -E 'tg-mcp-call|fastmcp call'` — kill PIDs >30m at high CPU; `tail -30 /var/log/tg-mcp/tg-mcp-call.log` on `tg_*` failures (`raw_preview`, `fastmcp_rc`).

## User chat (no `[Gatus]` prefix)

Normal ops conversation — same safety and pull rules when the question touches fleet config.

## Code changes

Load **CODE.md** for scripts in `/root/vds-servers`, multi-step fixes, or deploy logic. Skip for read-only Gatus triage.

`git pull` → `load_brain_file CODE.md` → `plan` (`create` → `add_task` → `finalize`) → implement with test evidence. No large rewrites before `finalize` (and Alexey approval if prompted).

## Git and brain files

| Location | Role |
|----------|------|
| `/root/vds-servers` | Infra source — **pull only** |
| `~/.opencrabs/profiles/ops/*.md` | Live brain (RSI/session edits) |
| `5 - hermes/opencrabs-brain/` | Canonical policy — deploy via `deploy-opencrabs.sh --brain` from Mac |

Do not `git commit`, `git push`, or open PRs from Hermes unless Alexey asks. Nightly cron pulls repo only — never pushes brain changes.

## Session rules

- Do not edit @oc_l1979_bot default profile or `~/.opencrabs/` (non-ops) brain files.
