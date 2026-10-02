# Governor policy matrix — the frozen contract

**Issue:** [#635](https://github.com/leshchenko1979/opencrabs/issues/635) — consolidate onto one admission policy.
**Base:** fork `main` @ `23d1cee15b824b7c0f3bd199b991df3b13212334` (worktree `refactor/635-one-pace-engine`, `/root/opencrabs-wt/635-pace-engine`).
**Purpose:** every later step of the refactor is diffed against this table. A cell that changes is a
behavioural change and must be named as one. A cell that is missing is a bug in this document.
**Home:** this file is the durable copy (the state dir survives plan archival). The same table is
pasted into the session plan .md per step 1 of the approved design.

File under review: `governor.rs` = `src/channels/telegram/governor.rs` (2008 lines) unless a cell says otherwise.

---

## 1. The four gates — one row each

| # | Gate (fn) | Surface | Buckets | Floor policy | On-dry policy | Max hold | Fail-open | Return type | Counters + ring push |
|---|---|---|---|---|---|---|---|---|---|
| G1 | `admit_chat_action` @ governor.rs:887 | `sendChatAction` (typing) | `peer.typing`, burst `typing_burst`, refill `1/typing_interval` — governor.rs:924-928 | **EXEMPT** — no `spacing_wait` call at all (#676) — governor.rs:917-923 | drop when `waited + wait > typing_max_hold` — governor.rs:935-940 | `typing_max_hold` — governor.rs:932 | n/a — the drop IS the terminal outcome — governor.rs:938 | `bool` — governor.rs:887 | `admitted_typing` governor.rs:932; `dropped_typing` governor.rs:937; `throttled_typing_ms` governor.rs:976; push `SURFACE_TYPING` governor.rs:933 |
| G2 | `edit_admission` @ governor.rs:1131 | `editMessageText` / `editRichMessage` | `peer.edits`, burst `edit_burst`, refill `edit_rate_per_sec`, reserve `INTERACTIVE_RESERVE = 2.0` — governor.rs:1175-1176 | **droppable only** — `class.is_droppable() && !spacing_ok` → false (#676) — governor.rs:1169-1171 | `Final` → queue latest-wins governor.rs:1203-1218; chrome → `note_drop` governor.rs:1220-1223; `Interactive` floor-dry → pass through governor.rs:1195-1199 | none — no wait loop; G2 returns immediately | n/a — `Final` queues, chrome drops | `bool` — governor.rs:1131 | `admitted_edits` governor.rs:1187 / `admitted_interactive` governor.rs:1185 / `interactive_overflow` governor.rs:1197 / `queued_finals`+`superseded_finals` governor.rs:1212-1216 / `dropped_*` governor.rs:1221 / `dropped_spacing` governor.rs:1170; push `SURFACE_EDITS` **only if `!payload.is_rich()`** — governor.rs:1191-1193 |
| G3 | `pace_send` @ governor.rs:1501 | `sendMessage` | **two AND-ed**: `peer.sends_sec` (burst `send_burst`, refill `1/send_interval`) + `peer.sends_min` (capacity `send_minute_ceiling`) — governor.rs:1548-1556 | **WAITED** — `spacing_hold = spacing_wait(..)`, max'd into `need` — governor.rs:1543, 1558-1562 | `need == 0` → take both, admit governor.rs:1563-1569; `waited + need > SEND_MAX_HOLD` → **admit anyway** governor.rs:1570-1574; else wait | `SEND_MAX_HOLD = 30s` — governor.rs:89 | **YES**, past 30 s — admit + `FailOpen(need)` — governor.rs:1570-1574 | `()` — governor.rs:1501 | `admitted_sends` governor.rs:1566; `throttled_send_ms` governor.rs:1808-1822; push `SURFACE_SENDS` on Go governor.rs:1567 **and** on FailOpen governor.rs:1572 |
| G4 | `pace_rich` @ governor.rs:1632 | `sendRichMessage` + rich edits | `peer.rich`, burst `rich_burst`, refill `rich_rate_per_sec` — governor.rs:1712 | **droppable drop / Final waits** — droppable+hold → `Dropped` governor.rs:1696-1700; `floor_hold = spacing_hold` iff `class == Final` (#757) — governor.rs:1707-1711 | `need == 0` → `bucket.take`: `Ok` admit governor.rs:1714-1721; `Err` + droppable → `Dropped` governor.rs:1722-1725; `Err` + content → wait governor.rs:1726 | none — waits until a token refills | **NEVER** — governor.rs:1616-1619 | `RichAdmission` = `Now` / `Dropped(class)` / `Deferred` — governor.rs:1627-1632 | `admitted_rich` governor.rs:1718; `dropped_rich`+`dropped_spacing` governor.rs:1698-1699; `deferred_rich` governor.rs:1783; push `SURFACE_RICH` governor.rs:1719 |

### Shared pre-gate stages (the duplicated skeleton)

All four gates run the same stages in the same order. The only per-gate differences are the
`forum_seen` set-condition and whether the global permit is consulted:

| Stage | G1 (governor.rs:887) | G2 (governor.rs:1131) | G3 (governor.rs:1501) | G4 (governor.rs:1632) |
|---|---|---|---|---|
| global-cooldown fast path | `is_global_cooldown_active()` → false — governor.rs:889-891 | active && class ∉ {Final, Interactive} → false — governor.rs:1145-1150 | — (not present) | — (not present) |
| `acquire_global_permit()` | **absent** | **absent** | `!may_proceed()` → debug + **proceed** — governor.rs:1523-1532 | `!may_proceed()` → `refuse_rich` — governor.rs:1656-1658 |
| DM guard `chat_id >= 0` | → true governor.rs:896-898 | → true governor.rs:1153-1155 | → return governor.rs:1513-1515 | → `Now` governor.rs:1638-1640 |
| `Limits::from_config()` + `!enabled` | → true governor.rs:900-902 | → true governor.rs:1156-1158 | → return governor.rs:1534-1536 | → `Now` governor.rs:1659-1661 |
| `ensure_summary_task()` | governor.rs:903 | governor.rs:1159 | **absent** | governor.rs:1663 |
| `peers().lock()` + `entry(chat).or_default()` | governor.rs:907 | governor.rs:1157 | governor.rs:1534 | governor.rs:1681 |
| `forum_seen` set / bail | set iff `thread_id.is_some()` governor.rs:905-907; bail → true governor.rs:909-911 | **no set**; bail → true governor.rs:1162-1164 | **no set**; bail → return governor.rs:1537-1539 | set iff `thread_id.is_some()` governor.rs:1683-1685; bail → `Now` governor.rs:1686-1688 |
| in-loop cooldown re-check | — | — | — | `wait_global_cooldown(MAX_INLINE_RATE_LIMIT_WAIT)` → false → `refuse_rich` — governor.rs:1672-1679 |
| `gate_now()` | governor.rs:912 | governor.rs:1153 | governor.rs:1541 | governor.rs:1689 |
| sleep + test-advance + fold | governor.rs:960-971, `fold_throttle_ms` governor.rs:975 | — (no loop) | governor.rs:1586-1598, `fold_send_ms` governor.rs:1807 | governor.rs:1756-1768, `fold_rich_ms` governor.rs:1788 |

---

## 2. The three pause concepts (step 5's target)

| # | Concept | Where declared | Where enforced | Scope |
|---|---|---|---|---|
| P1 | global 429 deadline | `rate_limit.rs` — `MAX_INLINE_RATE_LIMIT_WAIT = 60s` — rate_limit.rs:107 | `is_global_cooldown_active` / `wait_global_cooldown`, read at governor.rs:889, governor.rs:1145, governor.rs:1672 | process-wide |
| P2 | per-chat 429 pause | armed in `note_429_pause` governor.rs:1074: `let pause = wait.min(MAX_429_PAUSE)` governor.rs:1085 | `pause_until = Some(until)` written on `peer.rich` **and** `peer.edits` — governor.rs:1114-1115; `pause_armed_429 += 1` governor.rs:1116 | per-chat, per-arm |
| P3 | bucket pause field | `Bucket.pause_until: Option<Instant>` — governor.rs:301 | `Bucket::refill` freezes governor.rs:353; `Bucket::take` blocks bulk governor.rs:366 | per-bucket |

`MAX_429_PAUSE = 45s` — governor.rs:1005. Interactive `take_any` bypasses P3 by construction — governor.rs:377-385.

---

## 3. Live defect found while tabulating (feeds step 6)

`Counters` declares **22** fields (governor.rs:609-655). `format_summary` emits all 22 (governor.rs:795-833).
`all_zero()` (governor.rs:698-719) tests only **20** — it omits:

- `admitted_rich`
- `throttled_rich_ms`

Consequence: a peer whose only activity is rich traffic is judged "all zero" and its periodic
summary line is **suppressed** — governor.rs:795-797. Measured this turn by structural parse, not by eye.
This is exactly the failure mode step 6 exists to make impossible.

## 4. The config mirror (step 7's target)

`RateLimiterConfig` declares **13** knobs — config/types.rs:738-802. `Limits` declares **13** fields
— governor.rs:230-249 — and `Limits::from_config` (governor.rs:251-273) reads exactly those 13 `rl.*` names —
a hand-maintained 1:1 mirror with no compiler link.

## 5. Duplication census (the baseline this refactor must reduce)

| Measure | Baseline (this turn, worktree @ 23d1cee15) |
|---|---|
| `peers()` occurrences in governor.rs | 22 (1 def @ governor.rs:748, 15 production lock sites, 6 in `#[cfg(test)]`) |
| production lock sites | 15 — gates governor.rs:907/1157/1534/1681; folds governor.rs:983/1796/1815; consumers governor.rs:766/840/1087/1268/1296/1314/1362/1776 |
| `or_default()` | 11 |
| `fold_*` helpers | 3 (`fold_throttle_ms` governor.rs:975, `fold_rich_ms` governor.rs:1788, `fold_send_ms` governor.rs:1807) |
| gate sleep+advance pairs | 3 (governor.rs:960, governor.rs:1586, governor.rs:1756) + `acquire_global_permit` governor.rs:218 + drainer governor.rs:1291 |
| governor.rs / rich/api.rs / rate_limit.rs | 2008 / 1092 / 308 lines |

**Note on the step-3 acceptance criterion:** `grep -c 'peers().lock()'` counts the literal on one
line and therefore includes the 6 `#[cfg(test)]` sites, so it cannot reach ≤3. The comparable
production-only measure is the "production lock sites" row above (15 → the engine's own).
