# CODE.md — coding standards (ops profile)

Loaded after SOUL/AGENTS direct you here. Fleet safety and deploy paths: `/root/vds-servers/CLAUDE.md` (after `git pull`).

## Plan before code

For non-trivial edits in `/root/vds-servers` (new feature, refactor, multi-file fix, new script): use the built-in **`plan`** tool. Do not paste markdown plans in chat. Do not use `task_manager` for this.

**Exempt:** typo/one-line fix, read-only diagnostics, Alexey says skip plan.

### `plan` tool sequence

1. `operation=create` — set `title`, `description`, and **`test_strategy`** (TDD: failing check first, minimal fix, run verification).
2. `operation=add_task` — once per step. Use **`task_type=test`** for the first test/verification task when applicable. Put concrete steps in `description` and **`acceptance_criteria`**.
3. `operation=finalize` — Alexey may need to approve on Telegram; then execute.
4. `start_task` → implement → run checks → `complete_task` with real `output` (command + exit code). Repeat.

Other operations: `next_task`, `status`, `summary`, `reflect`, `skip_task`.

## TDD

Red → green → refactor:

1. Failing test or explicit verification step (in plan task or `test_strategy`).
2. Minimal code to pass.
3. Refactor only if needed; re-run checks.

- **Python:** `pytest` when the project has tests; otherwise add the smallest `*_test.py` or document the check in the plan task.
- **Shell/deploy scripts:** `bash -n`, dry-run, expected exit codes — list them in task `description` / `acceptance_criteria`.
- Never `complete_task` with `success=true` without evidence in `output`.

## General

- Minimal scope; match existing script style in the repo.
- Self-explanatory names; comment **why**, not what.
- No drive-by refactors; no secrets in source or logs.
- Fail explicitly — no silent `except`, no ignored errors.
- On hermes: **do not** `git commit` / `git push` unless Alexey asks in chat.

## Python

- Use `python3` (hermes and Mac).
- Prefer `pathlib`, type hints where they help readability.
- No bare `except:`; use specific exceptions or propagate.
- `subprocess` with argument lists, not shell strings, for untrusted input.
- Prefer stdlib; use project venv if present.

## Shell

- Deploy/maintenance scripts: `#!/usr/bin/env bash` and `set -euo pipefail` when appropriate.
- SSH targets: aliases `vpn`, `apps`, `n8n`, `hermes` — not raw IPs (see repo `CLAUDE.md`).
- Remote one-liners: single-quoted `ssh host '...'`; quote zsh globs on Mac.

## Out of scope here

Rust-first / single-binary upstream OpenCrabs template rules — ops work is infra shell and Python utilities.
