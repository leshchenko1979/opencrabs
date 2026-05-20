# MEMORY — repo pointers (do not duplicate fleet tables here)

**Source of truth:** `/root/vds-servers` (`git@github.com:leshchenko1979/servers.git`)

Before **Gatus alerts** or **user-initiated infra chat**: `git -C /root/vds-servers pull --ff-only` (report failure; continue if possible).

## Read from repo (not from memory)

| Need | File |
|------|------|
| Coding standards, plan tool, TDD | Brain `CODE.md` (load via SOUL/AGENTS before code edits) |
| Fleet, SSH aliases, safety rules, scripts | `/root/vds-servers/CLAUDE.md` |
| Gatus, host-diag, bridge, deploy | `/root/vds-servers/2 - VPN/README.md` |
| Gatus endpoints (`host-box2`…`host-box5`) | `/root/vds-servers/2 - VPN/services/gatus/config/config.yaml` |
| `host-diag` script | `/root/vds-servers/2 - VPN/services/gatus/scripts/host-diag` |
| Hermes / dual OpenCrabs | `/root/vds-servers/5 - hermes/docs/services.md` |
| Hermes on-box SSH (box5) | `ssh hermes` → `127.0.0.1:22` via `5 - hermes/config/ssh-config` — not public `:18718` (no hairpin; see CLAUDE.md `a39070ca`) |
| Apps services | `/root/vds-servers/3 - apps/docs/services.md` |
| n8n | `/root/vds-servers/4 - n8n/docs/services.md` |
| Mac backups | `/root/vds-servers/mac-workstation-backup/README.md` |

If repo and this file disagree, **repo wins** after pull.

## This profile only

- **ops:** @redevest_admin_tools_bot — Gatus alerts + direct ops chat with Alexey
- **default** (same host): @oc_l1979_bot — do not edit `~/.opencrabs/` default brain

Brain dir: `~/.opencrabs/profiles/ops/` · `memory/` = SQLite DB only
