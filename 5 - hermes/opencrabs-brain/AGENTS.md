# AGENTS — ops behavior (facts in repo)

OpenCrabs **core** prompt only includes SOUL.md + USER.md. You **must** load this file and MEMORY.md yourself:

```text
load_brain_file AGENTS.md
load_brain_file MEMORY.md
```

Do that at the **start of each session** and again when a new topic needs infra context.

## Before acting (any infra / fleet work)

Applies to **`[Gatus]` alerts** and **user-initiated chat** (questions, “check vpn”, deploy help, etc.).

1. `git -C /root/vds-servers pull --ff-only` (report failure; continue if possible).
2. `load_brain_file MEMORY.md` — then read repo paths it lists (at least `CLAUDE.md`; for Gatus also `2 - VPN/README.md` + `config/config.yaml`).
3. Do not rely on recalled IPs, services, or host-diag rules — load from repo after pull.

## User-initiated chat (no `[Gatus]` prefix)

- Treat as a normal ops conversation: answer questions, run safe diagnostics, explain findings.
- Same safety rules as Gatus (see `/root/vds-servers/CLAUDE.md`).
- Pull repo when the question touches fleet config, Gatus, deploy paths, or “what’s on box N”.

## On every `[Gatus]` message

1. Run the **Before acting** steps above.
2. Map `[ENDPOINT_NAME]` → SSH target from **current** `config/config.yaml` (host group).
3. Run `/usr/local/bin/host-diag` on that target (SSH or local for box5).
4. If exit ≠ 0: `uptime`, `free -h`, `df -h`, `docker ps` or `systemctl` as needed.
5. Reply: metrics, likely cause, **safe** fixes only — or ask Alexey.

## Respond first (Telegram)

Short acknowledgment → then `git pull` / `load_brain_file` / SSH / logs. Loading SOUL + USER is automatic; loading AGENTS + MEMORY is your job.

## Session rules

- Do not edit @oc_l1979_bot default profile or `~/.opencrabs/` (non-ops) brain files.
- Prefer `memory_search` before loading large memory when available.
