---
name: gatus-ssh-discovery
description: Configure or debug Gatus SSH health checks (host-diag, load/disk/RAM). Use when Gatus SSH endpoints fail, escaping breaks inline shell, or adding host monitoring on VDS boxes.
---

# Gatus SSH monitoring

## When to use

- Adding or changing **host-box2–host-box5** checks
- Gatus SSH shows `success=false` but the same command works over manual `ssh`
- Choosing between inline `body` commands vs a remote script

## Production setup (use this)

| Item | Value |
|------|--------|
| Script (repo) | `2 - VPN/services/gatus/scripts/host-diag` |
| On each box | `/usr/local/bin/host-diag` |
| Deploy script | `2 - VPN/services/gatus/scripts/deploy-host-diag.sh` |
| Gatus config | `2 - VPN/services/gatus/config/config.yaml` → `host-box2` … `host-box5` |
| Interval | **15m** |
| Conditions | `[CONNECTED] == true`, `[STATUS] == 0` |
| Alerts | `failure-threshold: 2` |
| Ops docs | `2 - VPN/README.md` → Gatus section |

**Thresholds** live in `host-diag` (not Gatus YAML):

| Constant | Fail when |
|----------|-----------|
| `LOAD_MAX=150` | 1-min load ≥ 1.5 (scaled: load×100) |
| `DISK_MAX=90` | `/` use ≥ 90% |
| `MEM_MIN_MB=80` | `MemAvailable` &lt; 80 MiB |

Script prints one line: `load disk mem_mb` (e.g. `13 58 371`). Gatus does **not** parse `[BODY]` for these endpoints—only exit code.

**Workflow after edits:**

1. Change `host-diag` in repo → run `deploy-host-diag.sh` (all boxes).
2. Change `config.yaml` → run `2 - VPN/scripts/deploy-gatus.sh`.

**box5:** SSH URL must include port `ssh://132.243.213.9:18718` (use `hermes` / `vds_ssh_connect_host` in deploy scripts).

---

## Gatus SSH command path

Commands flow: YAML `body` → JSON → remote shell. Backslashes and `$` in inline commands are fragile.

### Do

| Pattern | Example condition |
|---------|-------------------|
| Remote script, exit code | `{"command": "/usr/local/bin/host-diag"}` + `[STATUS] == 0` |
| Remote script, single number in BODY | echo disk % only + `[BODY] < 90` (legacy disk-check pattern) |
| Trivial inline | `{"command": "exit 0"}` + `[STATUS] == 0` |
| Literal BODY match | `[BODY] == hello` — **no quotes** around `hello` |

### Do not

| Pattern | Why |
|---------|-----|
| Inline `awk` with `$5`, pipes, nested quotes | Escaping breaks → `success=false` over SSH |
| `[BODY] < 90` on **multi-field** output (`13 58 371`) | BODY is one string; comparison is wrong |
| `[BODY] == "hello"` | Quoted literal in condition fails |

---

## Legacy: disk-only checks

Superseded by `host-diag`. Previously `/usr/local/bin/disk-check` echoed one integer and Gatus used `[BODY] < 90`. Still valid for a **single metric**; do not use for load+RAM together—use exit codes in one script instead.

`test-disk-threshold.sh` exercises the old disk-check flow only; **deprecated**.

---

## Historical: inline awk failures (2026-04-22)

Manual SSH succeeded; Gatus SSH returned `success=false` for all of:

- `df / | awk 'NR==2{print int($5)}'` with `[STATUS] == 0` or `[BODY] < 90`
- `awk '{exit ($5 >= 90 ? 1 : 0)}'` and escaped `\$5` variants

**Lesson:** put `awk`/`df` logic in a remote script file; keep Gatus `body` to a single path with no shell metacharacters.

Wrapper echoing one number + `[BODY] < 90` **does** work when the command string is only `/usr/local/bin/disk-check` (no inline awk).

---

## Verify

```bash
# On a box
/usr/local/bin/host-diag; echo exit:$?

# After deploy-gatus.sh
ssh vpn 'docker logs gatus --since 20m 2>&1 | grep host-box2 | tail -3'
```

Or `2 - VPN/services/gatus/deploy-report.sh` (expects `host-box2` in logs).
