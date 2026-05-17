# AGENTS — Gatus host workflow

## Fleet map

| Gatus endpoint | SSH target | Notes |
|----------------|------------|--------|
| host-box2 | `ssh vpn` | VPN box 104.128.131.166 |
| host-box3 | `ssh apps` | Apps 144.31.188.163 |
| host-box4 | `ssh n8n` | n8n 2.27.120.75 |
| host-box5 | local | `/usr/local/bin/host-diag` on hermes |

SSH config: `~/.ssh/config` — `vpn`, `apps`, `n8n`, `hermes`.

## On every `[Gatus]` message

0. `git -C /root/vds-servers pull --ff-only` — report failure; still try SSH if possible.
1. Parse endpoint name (e.g. `host-box3`).
2. Run `/usr/local/bin/host-diag` on the target (SSH or local for box5).
3. If exit ≠ 0: `uptime`, `free -h`, `df -h`, `docker ps` or `systemctl` as needed.
4. Reply with metrics, likely cause, **safe** fixes only.

## Never without explicit user approval

- Prune or wipe PostgreSQL/Redis data
- Edit `authorized_keys`
- Restart AmneziaWG
- `docker system prune` on production boxes
- Stop/prune Sablier-managed: `redevest-crm`, `redevest-crm-test`, `pdf-extract`, `sablier`

## VPN cleanup

`remove_unused_services()` in VPN cleanup scripts must stay **disabled**.

## host-diag

One line: `load disk mem_mb`. Exit 1 if load≥1.5 (×100 ≥ 150), disk≥90%, or MemAvailable&lt;80 MiB.

## Respond first (all Telegram messages)

1. **Send a short reply first** — acknowledge, say what you will check.
2. **Then** run SSH, git pull, logs, etc.

Do not run long tools before the user sees a reply (reading SOUL/MEMORY/AGENTS is fine).

If stuck in a tool loop: stop, summarize findings, ask or propose next step.

## Session

- Read `SOUL.md`, `USER.md`, and `MEMORY.md` at session start.
- Use `memory_search` before loading large memory files when available.
- Do not edit @oc_l1979_bot default profile files.
