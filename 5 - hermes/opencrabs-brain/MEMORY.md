# MEMORY — l1979.ru ops (Alexey)

## Paths

- Brain: `~/.opencrabs/profiles/ops/` (markdown at profile root)
- `memory/` under the profile = SQLite session DB only (not daily markdown logs unless you create `memory/YYYY-MM-DD.md`)
- Infra repo: `/root/vds-servers` (`git@github.com:leshchenko1979/servers.git`)

## Bots (do not confuse)

| Profile | Telegram bot | Role |
|---------|--------------|------|
| **ops** (this profile) | @redevest_admin_tools_bot | Gatus host alerts, fleet SSH triage |
| **default** on same host | @oc_l1979_bot | General chat — do not edit its brain |

## Rules

- Ask before destructive ops: prune DB/Redis, `authorized_keys`, AmneziaWG restart, `docker system prune`
- No `bc` on boxes — use `awk`
- VPN `cleanup.sh`: `remove_unused_services()` stays **disabled** (wipes PG/Redis dirs)
- Sablier: do not stop/prune `redevest-crm`, `redevest-crm-test`, `pdf-extract`, `sablier`

## Fleet

| Box | IP | SSH | Services |
|-----|-----|-----|----------|
| 2 VPN | 104.128.131.166 | `ssh vpn` | AmneziaWG, Traefik, Gatus, reverse SSH :4444 |
| 3 apps | 144.31.188.163 | `ssh apps` | Traefik/Sablier, PostgreSQL, Redis, CRM, pdf-extract, antispam, tg-mcp |
| 4 n8n | 2.27.120.75 | `ssh n8n` | Caddy, n8n |
| 5 hermes | 132.243.213.9:18718 | local / `ssh hermes` | OpenCrabs (default + ops), tg-mcp |

## host-diag

Script: **`/usr/local/bin/host-diag`** on each box (source in repo: `2 - VPN/services/gatus/scripts/host-diag`).

- Output one line: `load disk mem_mb` (e.g. `13 58 371`)
- `load` = 1-min load × 100 (`LOAD_MAX=150` → fail at avg ≥ 1.5)
- Exit **1** if disk ≥ 90%, load ≥ 150, or MemAvailable &lt; 80 MiB
- **Not** the same as `scripts/diagnostics-unified.sh` (full multi-box report from Mac)

## Gatus

- Runs on VPN; UI https://gatus.l1979.ru
- Host checks `host-box2`–`host-box5` every 15m via SSH + `host-diag`
- Bridge test alert `Conditions: test / Errors: e2e` = manual E2E, ignore

## Backups

- Daily from Mac: `mac-workstation-backup/backup-all.sh` → external volume, 7-day retention
- See `mac-workstation-backup/README.md` in `/root/vds-servers`
