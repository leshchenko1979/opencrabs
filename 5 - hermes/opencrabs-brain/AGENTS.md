# AGENTS — Gatus host workflow

## Fleet map

| Gatus endpoint | SSH target | Notes |
|----------------|------------|--------|
| host-box2 | `ssh vpn` | VPN box 104.128.131.166 |
| host-box3 | `ssh apps` | Apps 144.31.188.163 |
| host-box4 | `ssh n8n` | n8n 2.27.120.75 |
| host-box5 | local | `host-diag` on hermes (132.243.213.9:18718) |

SSH config: `~/.ssh/config` hosts `vpn`, `apps`, `n8n`, `hermes`.

## On every `[Gatus]` message

0. `git -C /root/vds-servers pull --ff-only` — report failure; still try SSH if possible.
1. Parse endpoint name from the alert (e.g. `host-box3`).
2. Run `/usr/local/bin/host-diag` on the target (via SSH or locally for box5).
3. If unhealthy: gather `uptime`, `free -h`, `df -h`, `docker ps` or relevant `systemctl`.
4. Reply with load/disk/memory, likely cause, and **safe** fixes only.

## Never without explicit user approval

- Prune or wipe PostgreSQL/Redis data
- Edit `authorized_keys`
- Restart AmneziaWG
- `docker system prune` on production boxes
- Stop/prune Sablier-managed: `redevest-crm`, `redevest-crm-test`, `pdf-extract`, `sablier` image/container

## VPN cleanup

`remove_unused_services()` in VPN cleanup scripts must stay **disabled** (destructive to DB dirs).

## host-diag

One-line: `load disk mem_mb`. Exit 1 if load≥1.5, disk≥90%, or MemAvailable&lt;80 MiB.
