# OpenCrabs System Reference

## Architecture

OpenCrabs is a multi-profile AI agent. Each profile is fully isolated with its own brain files, memory, and database.

```
~/.opencrabs/                           # default profile root
~/.opencrabs/profiles/<name>/           # named profiles

Each profile contains:
  *.md      # brain files (SOUL, TOOLS, MEMORY, USER, AGENTS, etc.)
  *.toml    # config, keys
  opencrabs.db   # sessions, messages, tool executions
  memory/   # daily memory notes + memory.db (vector/search)
  logs/     # session logs
  locks/    # channel token locks
```

## Profiles

| Profile | Purpose | Primary Focus |
|---------|---------|---------------|
| default | RedeVest AI business assistant / coach | ~/redevest-ai/ repo |
| ops | VDS servers management and automation | ~/vds-servers/ repo |

Switch profile: opencrabs -p <profile> or OPENCRABS_PROFILE env var.

## Key Paths

| Path | Purpose |
|------|---------|
| ~/.opencrabs/ | default profile root |
| ~/.opencrabs/profiles/ops/ | ops profile |
| ~/.opencrabs/profiles.toml | profile registry |
| /usr/local/bin/opencrabs | binary |
| /tmp/opencrabs/ | source code (Rust) |
| ~/.config/opencrabs/ | runtime config |
| ~/redevest-ai/ | RedeVest AI business assistant code |
| ~/vds-servers/ | VDS servers management repo |

## Brain Files Per Profile

Each profile has its own copies — no inheritance, no shared files:

| File | Purpose |
|------|---------|
| SOUL.md | personality, behavior |
| TOOLS.md | tool usage patterns |
| MEMORY.md | persistent context, repo pointers |
| USER.md | user preferences |
| AGENTS.md | operational guidelines |
| IDENTITY.md | agent name, vibe (if present) |
| CODE.md | coding standards (if present) |
| SECURITY.md | security policies (if present) |

## Docs & Code

| Resource | Location |
|----------|----------|
| Source code | /tmp/opencrabs/src/ |
| Brain templates | /tmp/opencrabs/src/docs/reference/templates/ |
| Architecture docs | /tmp/opencrabs/src/docs/reference/ARCHITECTURE*.md |
| README | /tmp/opencrabs/README.md |
| CHANGELOG | /tmp/opencrabs/CHANGELOG.md |
| RedeVest AI | ~/redevest-ai/ |
| VDS servers | ~/vds-servers/ |

## Service Control

```bash
sudo systemctl start|stop|restart opencrabs
journalctl -u opencrabs -n 50 --no-pager
systemctl status opencrabs
```

## Database

- SQLite at: ~/.opencrabs/profiles/<profile>/opencrabs.db
- Sessions, messages, tool_executions, cron_jobs, usage_ledger tables
- memory.db in memory/ subdirectory for vector search

## Memory

Daily notes: memory/YYYY-MM-DD.md (auto-saved by agent)
Memory DB: memory/memory.db (SQLite, separate from main DB)

## RSI (Self-Improvement)

RSI proposals land in: ~/.opencrabs/profiles/<profile>/rsi/proposed_*.toml
RSI improvements log: ~/.opencrabs/rsi/improvements.md (root level)