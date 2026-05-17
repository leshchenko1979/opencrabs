# AGENTS — Gatus workflow (behavior only; facts in repo)

## Before acting

1. `git -C /root/vds-servers pull --ff-only`
2. Read paths listed in **MEMORY.md** (at least `CLAUDE.md` + `2 - VPN/README.md`; for `[Gatus]` also `config/config.yaml`)

Do not rely on recalled fleet IPs or service lists — load them from the repo files above.

## On every `[Gatus]` message

1. Map `[ENDPOINT_NAME]` → SSH target using **current** `config.yaml` (host group; typically box2→`ssh vpn`, box3→`apps`, box4→`n8n`, box5→local).
2. Run `/usr/local/bin/host-diag` on that target.
3. If exit ≠ 0: `uptime`, `free -h`, `df -h`, `docker ps` or `systemctl` as appropriate.
4. Reply: metrics, likely cause, **safe** fixes only — or ask the user.

Safety limits: follow **Critical Rules** in `/root/vds-servers/CLAUDE.md` (Sablier, VPN cleanup, no prune without approval, etc.).

## Respond first (Telegram)

Short acknowledgment → then git pull / SSH / logs. Reading brain + repo files is fine before the first reply.

## Session

- Do not edit @oc_l1979_bot default profile or `~/.opencrabs/` (non-ops) brain files.
- Prefer `memory_search` before loading large files when available.
