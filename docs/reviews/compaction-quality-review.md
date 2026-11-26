# Compaction subsystem quality review

Branch reviewed: `cursor/issue-1023-compaction-notices`  
Review/fix branch: `cursor/compaction-quality-review`  
Scope: compaction implementation, continuation prompts, Telegram progress/resume paths, related tests and `prompts/compaction*.toml`.

Severity: **Critical** · **High** · **Medium** · **Low** · **Info**

---

## Duplication

### CQ-001 — Monolithic `compaction.rs`

| Field | Value |
|-------|-------|
| Location | `src/brain/agent/service/compaction.rs:1` (~2,920 lines pre-fix) |
| Severity | Medium |
| **VALIDATION** | Line count; module holds budget enforcement, idle lane, spawn, guards, and unit tests. |
| Status | **Deferred** — splitting without behavior change needs a dedicated refactor PR. |

### CQ-002 — `enforce_context_budget` size

| Field | Value |
|-------|-------|
| Location | `compaction.rs` ~680–1175 |
| Severity | Medium |
| **VALIDATION** | Read span; single method owns tier-0, guards, sync head-cut, safety net, emits. |
| Status | **Deferred** (tied to CQ-001). |

### CQ-003 — Repeated `usage_pct` recompute blocks

| Field | Value |
|-------|-------|
| Location | `compaction.rs` (multiple visits within `enforce_context_budget`) |
| Severity | Low |
| **VALIDATION** | `rg 'usage_pct = if effective_max'` — five recomputes after mutations; intentional drift measurement. |
| Status | **Rejected** — recomputation is load-bearing, not copy-paste slop. |

### CQ-004 — Duplicated `static_overhead` bootstrap

| Field | Value |
|-------|-------|
| Location | `compaction.rs:721`, `1187`, `2048`, `2131` |
| Severity | Low |
| **VALIDATION** | Grep `static_overhead = if context.provider_anchor` |
| Status | **Deferred** — small helper; low risk/reward in this pass. |

### CQ-005 — Loop-guard vs non-reducing-brake suppress blocks

| Field | Value |
|-------|-------|
| Location | `compaction.rs` ~831–916 |
| Severity | Medium |
| **VALIDATION** | Side-by-side diff: same warn → `safety_truncate_to_80` → `emit_compaction_event` skeleton. |
| Status | **Deferred** — shared helper would need distinct marker text and trigger fields. |

### CQ-006 — Head-cut loops (auto vs emergency)

| Field | Value |
|-------|-------|
| Location | `compaction.rs` ~1024–1261 (pre-fix) |
| Severity | Medium |
| **VALIDATION** | Compared both `MAX_HEAD_CUTS` / `HEAD_CUT_ATTEMPTS` loops; emergency omitted the “still over target” warn only. |
| Status | **Fixed** — `run_head_cut_passes` extracted. |

### CQ-007 — Background document apply duplicated

| Field | Value |
|-------|-------|
| Location | `resolve_pending_compaction` vs `apply_background_compaction_document` |
| Severity | Medium |
| **VALIDATION** | Matched marker/summary/`note_compaction_success` arms; refactor routes resolve through shared helper. |
| Status | **Fixed** (resolve `Ok` path). |

### CQ-008 — Summariser inventory stamp prep triplicated

| Field | Value |
|-------|-------|
| Location | `compaction.rs` spawn, `context.rs` `compact_context` / `compact_head_chunk` |
| Severity | Medium |
| **VALIDATION** | Grep `format_context_inventory` — three call sites. |
| Status | **Deferred** — cross-module extraction; behavior-sensitive. |

### CQ-009 — Telegram `ProgressEvent` match duplicated (progress vs resume)

| Field | Value |
|-------|-------|
| Location | `progress.rs:21–385` vs `resume.rs:776–1112` |
| Severity | High |
| **VALIDATION** | Diff; only `handler.rs` uses `build_progress_cb`; resume inlines ~330 lines. |
| Status | **Deferred** — large seam move; high regression risk for #135 / #1086 parity. |

### CQ-010 — `CompactionSummary` arm parity (resume vs live)

| Field | Value |
|-------|-------|
| Location | `progress.rs:366–379` vs `resume.rs:1089–1107` |
| Severity | Medium |
| **VALIDATION** | Resume updates `sections.ctx` + dirty; live progress only enqueues notice. |
| Status | **Deferred** — may be intentional resume-only ctx refresh; needs product sign-off. |

---

## Modularity and size

Same as CQ-001/CQ-002. Additional leaky state:

### CQ-014 — `spawn_turn_generation` stored but never read for invalidation

| Field | Value |
|-------|-------|
| Location | `PendingCompaction` `compaction.rs:276–278`; set at `compaction.rs:2272` |
| Severity | Medium |
| **VALIDATION** | `rg spawn_turn_generation` — only written + test fixture; prefix hash used instead for discard. |
| Status | **Deferred** — F12 generation guard unfinished; wire-up is a feature, not hygiene-only. |

---

## Error handling and logging consistency

### CQ-012 — Duplicate WARN on gate fire

| Field | Value |
|-------|-------|
| Location | `compaction.rs` ~919–952 |
| Severity | Low |
| **VALIDATION** | Read log sequence: integer `>65%` then `gate_warn_message` with one decimal (#1733). |
| Status | **Deferred** — logging-only; no behavioral bug. |

### CQ-019 — Idle eager path swallows summariser `Err`

| Field | Value |
|-------|-------|
| Location | `on_idle_compaction_ready` `compaction.rs:1899–1900` |
| Severity | Medium |
| **VALIDATION** | Compare to `resolve_pending` `Failed` arm with `SelfHealingAlert`. |
| Status | **Deferred** — needs channel receipt policy for between-turn failures. |

### CQ-022 — Eager persist failure: error log only

| Field | Value |
|-------|-------|
| Location | `compaction.rs:1966` |
| Severity | Low |
| **VALIDATION** | Unlike background `Failed` path, no `ProgressEvent::SelfHealingAlert`. |
| Status | **Deferred**. |

---

## Naming (`shed` cleanup)

### CQ-023 — Doc “shed” in compaction trend comment

| Field | Value |
|-------|-------|
| Location | `compaction.rs:146` |
| Severity | Low |
| **VALIDATION** | Grep — updated to “degradation path”. |
| Status | **Fixed**. |

### CQ-024 — Log “compaction shed”

| Field | Value |
|-------|-------|
| Location | `builder.rs:2381` |
| Severity | Low |
| **VALIDATION** | Grep log string — renamed to “compaction degradation”. |
| Status | **Fixed**. |

### CQ-025 — Legacy `"[Tool output shed:"` stub detector

| Field | Value |
|-------|-------|
| Location | `compaction_offload.rs:54` |
| Severity | Low |
| **VALIDATION** | `compaction_tool_offload_test.rs` documents on-disk legacy marker. |
| Status | **Rejected** — required for migrated sessions (#1009 compat). |

### CQ-026 — Prompt prose “shed” / “Shedding”

| Field | Value |
|-------|-------|
| Location | `prompts/compaction.default.toml` |
| Severity | Info |
| **VALIDATION** | Read toml — model-facing vocabulary, not harness code. |
| Status | **Rejected** (by scope rule). |

### CQ-027 — Stale “five compaction sites” comment

| Field | Value |
|-------|-------|
| Location | `tool_loop.rs:652` vs `apply_compaction_continuation` “six sites” |
| Severity | Low |
| **VALIDATION** | `compaction_persist_continuation_test.rs` expects 11 wired call sites. |
| Status | **Fixed** (continuation builder comment). |

---

## Concurrency and race risks

### CQ-016 — Prefix discard without `abort()` (**bug**)

| Field | Value |
|-------|-------|
| Location | `resolve_pending_compaction` `compaction.rs:1643–1652` (pre-fix) |
| Severity | **High** |
| **VALIDATION** | Read path: `take_pending` then prefix mismatch returned `Empty` without `PendingCompaction::abort()`. Added failing test `idle_prefix_mismatch_aborts_in_flight_cancel_token`. |
| Status | **Fixed** — `log_and_abort_stale_prefix_pending`. |

### CQ-017 — Misleading discard log (“Idle post-turn” for all triggers)

| Field | Value |
|-------|-------|
| Location | Same block |
| Severity | Low |
| **VALIDATION** | Log text applied to any pending compaction type. |
| Status | **Fixed** — neutral “Background compaction discarded”. |

### CQ-018 — Eager idle apply reinserted stale pending on prefix mismatch

| Field | Value |
|-------|-------|
| Location | `on_idle_compaction_ready` `compaction.rs:1940–1945` (pre-fix) |
| Severity | Medium |
| **VALIDATION** | Code reinserted pending after DB reload mismatch; could re-attempt stale apply. |
| Status | **Fixed** — abort + drop instead of reinsert (aligns with settle path). |

### CQ-020 — Idle thin-doc retry logged as `background_auto()` (**bug**)

| Field | Value |
|-------|-------|
| Location | `compaction.rs:2345–2364` (pre-fix) |
| Severity | Low (telemetry) |
| **VALIDATION** | Read spawn: `run_ctx` passed to attempt 1; retry hardcoded `CompactionRunContext::background_auto()`. |
| Status | **Fixed** — retry uses `run_ctx`. |

### CQ-021 — `lock().ok()` on `pending_compactions`

| Field | Value |
|-------|-------|
| Location | Multiple in `compaction.rs` |
| Severity | Medium |
| **VALIDATION** | Project-wide pattern; poisoned mutex → silent no-op. |
| Status | **Deferred** — repo-wide convention, not compaction-only. |

### CQ-033 — `DefaultHasher` prefix fingerprint

| Field | Value |
|-------|-------|
| Location | `compaction_prefix_fingerprint` |
| Severity | Low |
| **VALIDATION** | Documented process-local hash; not stable across Rust versions. |
| Status | **Deferred** — changing hash algo would alter discard behavior (on-disk policy). |

---

## Dead code

### CQ-015 — `CompactionEventStage::TruncateOnly`

| Field | Value |
|-------|-------|
| Location | `compaction_event.rs:19` |
| Severity | Low |
| **VALIDATION** | `dead_code` allow; schema retention after #996. |
| Status | **Rejected** — intentional schema retention. |

---

## Magic numbers

### CQ-011 — Literal `65.0` / `90.0` vs `PRESSURE_WARN_CEILING`

| Field | Value |
|-------|-------|
| Location | `compaction.rs` gate, idle spawn, offload threshold |
| Severity | Medium |
| **VALIDATION** | Compared to `nudge::PRESSURE_WARN_CEILING` and tier-2 90% comments in-module. |
| Status | **Fixed** — `COMPACTION_HARD_TRUNCATE_CEILING_PCT`, `PRESSURE_WARN_CEILING`, `compaction_soft_gate_token_target`. |

### CQ-013 — `* 65 / 100` target tokens

| Field | Value |
|-------|-------|
| Location | Head-cut loops (pre-fix) |
| Severity | Low |
| **VALIDATION** | Grep `* 65 / 100`. |
| Status | **Fixed** — uses `compaction_soft_gate_token_target`. |

---

## Test gaps

### CQ-028 — `on_idle_compaction_ready` eager DB path

| Field | Value |
|-------|-------|
| Location | — (missing) |
| Severity | High |
| **VALIDATION** | `rg on_idle` in tests — no dedicated integration test. |
| Status | **Deferred** — heavy DB/fixture harness; partial coverage via #1023 idle tests. |

### CQ-029 — Prefix discard without abort assertion

| Field | Value |
|-------|-------|
| Location | `compaction_issue_1023_idle_test.rs` |
| Severity | Medium |
| **VALIDATION** | Extended with cancel-token test (CQ-016). |
| Status | **Fixed**. |

### CQ-030 — `maybe_spawn_idle_consolidation` integration

| Field | Value |
|-------|-------|
| Location | — |
| Severity | Medium |
| **VALIDATION** | Lane tests cover scope math only, not “slot occupied” / thresholds end-to-end. |
| Status | **Deferred**. |

### CQ-031 — `PendingState::Failed` → blocking sync follow-up

| Field | Value |
|-------|-------|
| Location | — |
| Severity | Low |
| **VALIDATION** | No test mocks failed background join then sync gate. |
| Status | **Deferred**. |

### CQ-032 — Resume vs live progress golden parity

| Field | Value |
|-------|-------|
| Location | — |
| Severity | Medium |
| **VALIDATION** | Tied to CQ-009. |
| Status | **Deferred**. |

---

## Findings summary table

| ID | Severity | Validated | Disposition |
|----|----------|-----------|-------------|
| CQ-001 | Medium | Yes | Deferred |
| CQ-002 | Medium | Yes | Deferred |
| CQ-003 | Low | Yes | Rejected |
| CQ-004 | Low | Yes | Deferred |
| CQ-005 | Medium | Yes | Deferred |
| CQ-006 | Medium | Yes | **Fixed** |
| CQ-007 | Medium | Yes | **Fixed** |
| CQ-008 | Medium | Yes | Deferred |
| CQ-009 | High | Yes | Deferred |
| CQ-010 | Medium | Yes | Deferred |
| CQ-011 | Medium | Yes | **Fixed** |
| CQ-012 | Low | Yes | Deferred |
| CQ-013 | Low | Yes | **Fixed** |
| CQ-014 | Medium | Yes | Deferred |
| CQ-015 | Low | Yes | Rejected |
| CQ-016 | High | Yes (test) | **Fixed** |
| CQ-017 | Low | Yes | **Fixed** |
| CQ-018 | Medium | Yes | **Fixed** |
| CQ-019 | Medium | Yes | Deferred |
| CQ-020 | Low | Yes | **Fixed** |
| CQ-021 | Medium | Yes | Deferred |
| CQ-022 | Low | Yes | Deferred |
| CQ-023 | Low | Yes | **Fixed** |
| CQ-024 | Low | Yes | **Fixed** |
| CQ-025 | Low | Yes | Rejected |
| CQ-026 | Info | Yes | Rejected |
| CQ-027 | Low | Yes | **Fixed** |
| CQ-028 | High | Yes | Deferred |
| CQ-029 | Medium | Yes (test) | **Fixed** |
| CQ-030 | Medium | Yes | Deferred |
| CQ-031 | Low | Yes | Deferred |
| CQ-032 | Medium | Yes | Deferred |
| CQ-033 | Low | Yes | Deferred |

### CQ-034 — Tier-0 offload gating (owner decision)

| Field | Value |
|-------|-------|
| Location | `compaction.rs` idle spawn, `spawn_background_compaction`, `apply_background_compaction_document`, `tool_loop.rs` reload |
| Severity | High |
| **VALIDATION** | `compaction_consolidation_offload_test.rs`, `compaction_tool_offload_notice_test.rs` (>65% mid-loop), restart/folded tests |
| Status | **Fixed** |

Tier-0 tool-output offload may run only when the prompt cache breaks anyway:

1. Folded into summariser / delta / idle-merge / consolidation **apply** (same prefix rewrite; `tokens_freed` / `blocks_offloaded` on the Apply `compaction_event`, not a standalone `ToolOutputOffloaded` card).
2. Session **restart/reload** when the compaction window is anchored on a `MarkerKind::Restart` row (`CompactionTrigger::RestartReload` structured log only).
3. Mid-loop **>65%** gated pass in `enforce_context_budget` (unchanged standalone notice path).

Removed: `BudgetPhase::PostTurn` + `CompactionPath::Background` offload in `maybe_spawn_idle_consolidation`, and `offload_before_consolidation_summariser` before background spawn.

**#1025:** Apply-stage cache metrics now include tier-0 `tokens_freed` when offload is folded into the apply rewrite (standalone mid-loop offload events remain on `CompactionEventStage::ToolOffload`).

---

## Real bugs addressed

1. **CQ-016** — Stale background/idle compaction discarded on prefix mismatch without cancelling the in-flight summariser (wasted provider work; empty slot could accept overlapping spawns).
2. **CQ-018** — Eager idle apply reinserted pending after prefix mismatch instead of dropping cancelled work.
3. **CQ-020** — Thin-document retry mis-tagged idle spawns in structured logs.

Each of CQ-016/CQ-018 includes or extends regression coverage in `compaction_issue_1023_idle_test.rs`.

---

## Deferred (would need broader refactors or policy)

- Split `compaction.rs` / shrink `enforce_context_budget` (CQ-001/002).
- Telegram progress/resume DRY and parity tests (CQ-009/010/032).
- Shared inventory stamp helper across spawn paths (CQ-008).
- `spawn_turn_generation` invalidation (CQ-014).
- Idle eager-path integration test and `SelfHealingAlert` parity (CQ-028/019/022).
- Stable cross-version prefix fingerprint (CQ-033).

---

## Coverage note (pre-existing strength)

Loop guard, non-reducing brake, scope escalation, shed order/degradation, background gap/tail preservation, manifest/slug/breakdown cards, persist-all-sites guard, tier-0 offload, and #1023 hold/apply timing remain well covered by the `compaction_*` test battery.