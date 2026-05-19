# AGENTS — ops runbook

You are reading **AGENTS.md** (loaded via SOUL bootstrap). Ensure **MEMORY.md** is loaded. For code work, also load **CODE.md**.

## Before acting (any infra / fleet work)

Applies to **`[Gatus]` alerts** and **user-initiated chat** (questions, “check vpn”, deploy help, etc.).

1. `git -C /root/vds-servers pull --ff-only` (report failure; continue if possible).
2. `load_brain_file MEMORY.md` — then read repo paths it lists (at least `CLAUDE.md`; for Gatus also `2 - VPN/README.md` + `config/config.yaml`).
3. Do not rely on recalled IPs, services, or host-diag rules — load from repo after pull.

## Code changes

**Load CODE.md** when: editing scripts in `/root/vds-servers`, multi-step fixes, new Python/shell, deploy logic, or Alexey asks about coding style. **Skip** for pure Gatus triage with no repo edits.

**Workflow:** `git pull` → `load_brain_file CODE.md` → `plan` tool (`create` → `add_task` → `finalize`, unless exempt) → `start_task` / implement / `complete_task` with test evidence. No `edit_file` or large `bash` rewrites until `finalize` (and Alexey approved if prompted).

## User-initiated chat (no `[Gatus]` prefix)

- Treat as a normal ops conversation: answer questions, run safe diagnostics, explain findings.
- Same safety rules as Gatus (see `/root/vds-servers/CLAUDE.md`).
- Pull repo when the question touches fleet config, Gatus, deploy paths, or “what’s on box N”.

## SSH targets (host-box2–5)

You run on **Hermes (box5)**. Map endpoint → how to run `host-diag`:

| Endpoint | Run diagnostics |
|----------|-----------------|
| `host-box2` | `ssh vpn /usr/local/bin/host-diag` |
| `host-box3` | `ssh apps /usr/local/bin/host-diag` |
| `host-box4` | `ssh n8n /usr/local/bin/host-diag` |
| `host-box5` | `ssh hermes-local /usr/local/bin/host-diag` **or** `/usr/local/bin/host-diag` |

**Never `ssh hermes` from Hermes** — `Host hermes` uses the public IP (`132.243.213.9:18718`); loopback via that address **hangs/times out** (no hairpin). **Use `hermes-local`** when SSH is needed for box5 (same pattern as `ssh vpn` / `ssh apps`).

If `host-diag` exits 1 on box5: check load (`uptime`), then stuck MCP — `ps aux | grep -E 'tg-mcp-call|fastmcp call|mcp tg-mcp'`; kill PIDs running >30m at high CPU, then re-run diag.

## On every `[Gatus]` message

1. Run the **Before acting** steps above.
2. Map `[ENDPOINT_NAME]` using the **SSH targets** table above (not raw IPs from memory).
3. Run `/usr/local/bin/host-diag` per that row (local on box5; `ssh vpn` / `ssh apps` / `ssh n8n` elsewhere).
4. If exit ≠ 0: `uptime`, `free -h`, `df -h`, `docker ps` or `systemctl` as needed.
5. Reply: metrics, likely cause, **safe** fixes only — or ask Alexey.

## Respond first (Telegram)

Short acknowledgment → then `git pull` / `load_brain_file` / SSH / logs. SOUL + USER are automatic; AGENTS, MEMORY, and CODE (when coding) are your job.

## Git and brain files (important)

| Location | Role |
|----------|------|
| `/root/vds-servers` | Infra source of truth — **pull only** (`git pull --ff-only`) |
| `~/.opencrabs/profiles/ops/*.md` | Live OpenCrabs brain — may be edited by you, RSI, or RSI template sync |
| `5 - hermes/opencrabs-brain/` in repo | Canonical policy files — updated on Mac, deployed via `deploy-opencrabs-ops-brain.sh` |

**Do not** `git commit`, `git push`, or open PRs from hermes unless Alexey explicitly asks in chat.

RSI and session edits on the profile brain are **not** auto-synced to GitHub. Pushing them would often re-introduce upstream template bloat or wrong facts. Nightly cron (`vds-servers-nightly-pull`) only pulls the repo — it does not push brain changes.

If Alexey wants something preserved in git, say what to copy and wait for instruction (or suggest he run deploy from Mac after editing repo).

## A2A ingress

**On ice** until [opencrabs#92](https://github.com/adolfousier/opencrabs/issues/92) — production Gatus alerts use Telegram only ([servers#1](https://github.com/leshchenko1979/servers/issues/1)).

When the task is **not** in an active Telegram channel session but the message contains `[Gatus]` (A2A / programmatic trigger):

1. `load_brain_file AGENTS.md` and `MEMORY.md` if not already loaded.
2. Run the same steps as **On every `[Gatus]` message** (pull, map endpoint via **SSH targets**, `host-diag`, investigate).
3. **MUST** `telegram_send` to chat `133526395`: one-line ack immediately; final report when done.
4. If no safe fix: send investigation results and concrete proposals via `telegram_send`.
5. Overrides OPS_SOUL “do not telegram_send in active Telegram session” — that rule applies only to Telegram-channel sessions.

## Session rules

- Do not edit @oc_l1979_bot default profile or `~/.opencrabs/` (non-ops) brain files.
- Prefer `memory_search` before loading large memory when available.
