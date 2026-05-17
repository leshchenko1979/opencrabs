---
name: diagnosing-servers
description: Use when asked to run diagnostics, investigate server problems, analyze health metrics across multiple VDS boxes, or improve monitoring/diagnostic scripts.
---

# Diagnosing Servers

## Overview
Systematic multi-phase server diagnosis: run diagnostic scripts, identify root causes of problem areas, apply fixes, and iteratively improve the diagnostic tooling itself.

## When to Use

- User says "run diagnostics", "check server health", "analyze disk/RAM/swap issues"
- Diagnostic output shows WARNING or CRITICAL flags
- Server metrics (disk >85%, swap pressure, OOM kills) are outside normal ranges
- User asks to "improve the diagnostic script" or "analyze the script itself"
- Cross-server inconsistency or anomalous values detected

## Diagnostic Workflow

### Phase 1: Run Diagnostics
```bash
# Run unified diagnostics across all servers
bash scripts/diagnostics-unified.sh

# For deep per-server analysis (when issues found)
ssh root@<IP> "df -h; free -h; docker system df; journalctl --disk-usage"
```

### Phase 2: Identify Problem Areas
For each server, evaluate in priority order:

1. **Disk** — Check `df -h /` first. >85% = warning, >90% = critical
2. **Swap pressure** — When swap used >30% AND free RAM <300MB
3. **OOM kills** — `journalctl -b | grep -ci "oom"` > 0 = investigate
4. **Docker bloat** — `docker system df` shows reclaimable images + old build cache
5. **Journal size** — `journalctl --disk-usage` > 100MB
6. **Anomalous `/root`** — `du -sh /root` > 200MB on minimal boxes

### Phase 3: Fix Problems (Priority Order)

When multiple boxes need fixes, **parallelize** — SSH to each box simultaneously.

**Disk cleanup (box has Docker):**
```bash
ssh root@<IP> "docker system prune -a -f && docker builder prune -f"
```

**Disk cleanup (no Docker):**
```bash
ssh root@<IP> "apt-get autoremove --purge -y && apt-get clean && journalctl --vacuum-time=7d"
```

**Swap pressure:**
```bash
# Check what's consuming memory
ssh root@<IP> "ps aux --sort=-%mem | head -10"
# Identify Docker containers to restart or services to tune
```

**OOM investigation:**
```bash
ssh root@<IP> "journalctl -b | grep -i 'oom\|out of memory' | tail -20"
```

**Anomalous `/root`:**
```bash
ssh root@<IP> "du -hsm /root/* 2>/dev/null | sort -h | tail -10"
# Clean old logs, cursor-server data, npm cache, cargo cache
```

### Phase 4: Verify Fixes

After applying fixes, **re-run diagnostics** to confirm:
```bash
bash scripts/diagnostics-unified.sh
# Check disk % dropped, swap pressure resolved, warnings cleared
```

### Phase 5: Improve Diagnostic Script

**After every diagnostic session**, improve the script if:
- New warning thresholds needed (e.g., disk 85%→90% wasn't catching early enough)
- Missing metrics discovered (e.g., build cache, stale images, `/root` size)
- Output was hard to parse (color codes, inline `[WARN]` tags needed)
- Commands produced truncated or messy output

**Keep parsing simple** — `journalctl --disk-usage` output format varies; avoid complex awk.宁可输出原始命令结果让agent自己判断。

**Note:** `bc` is not installed on boxes — use `awk` for numeric comparisons (e.g., `echo "$NUM 100" | awk '{print ($1 > $2) ? 1 : 0}'`).

**Log improvements and update CLAUDE.md** after script changes.

## Quick Reference

| Symptom | Command | Fix |
|---------|---------|-----|
| Disk >90% | `df -h /` | `docker system prune -a -f` or `apt-get autoremove --purge -y` |
| Swap pressure | `free -h` + `swapon --show` | Identify memory hogs, restart containers |
| OOM kills | `journalctl -b \| grep -ci oom` | Increase RAM or reduce container count |
| Stale Docker images | `docker images \| grep <none>` | `docker image prune -a -f` |
| Large journal | `journalctl --disk-usage` | `journalctl --vacuum-time=7d` |
| Anomalous /root | `du -sh /root` | `rm -rf /root/.cache /root/.cursor-server /root/.npm` |
| Build cache | `docker system df` (Build Cache row) | `docker builder prune -f` |

## Common Mistakes

- **Fix symptoms not root cause**: High disk might be old images (fix images) not data
- **Skip non-Docker boxes**: Box 4 has no Docker — needs apt autoremove + journal vacuum, not docker prune
- **Ignore swap pressure**: Swap used 50%+ with low RAM is a warning even if not OOM yet
- **Don't verify after fix**: Always re-run diagnostics to confirm issue resolved
- **Don't update the diagnostic script**: Finding a problem without improving the script means next diagnosis won't catch it
- **Forget to update CLAUDE.md and memory**: Changes to server state or tooling need to be persisted

## Red Flags — STOP

- Output shows disk >90% and you don't immediately clean Docker/images
- Swap pressure detected but no investigation started
- Script improvements identified but not applied
- Server state changed but CLAUDE.md not updated
- Fix applied but not verified with re-run of diagnostics