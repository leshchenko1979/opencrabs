//! Tests for `agent::service::compaction_prompts`.
//!
//! Regression context (2026-05-29): a Telegram user (leshchenko1979)
//! forwarded a post-compaction one-liner — the model dropping into
//! Russian мат when expressing frustration after recovering context —
//! to friends, calling it a "Skynet" moment. Earlier we had silenced
//! all post-compaction narration via commit fb325fb5; this test
//! locks in that the fun variant is the DEFAULT (so those delight
//! moments keep happening) and that the silent variant is reachable
//! when `[agent] silent_compaction = true`.
//!
//! These tests are deliberately string-sentinel based, not full
//! equality. The exact wording will drift; what must not drift is:
//!
//! - Default (silent=false) keeps the explicit "POST-COMPACTION
//!   PROTOCOL" header and at least one invitation to acknowledge.
//! - Silent (silent=true) keeps the explicit "Silently" directive
//!   and excludes the fun acknowledgement invitation.
//! - The auto-approve tail is appended in both modes when
//!   auto_approve=false.

use crate::brain::agent::service::compaction_prompts::{
    CompactionKind, PlanRecovery, build_continuation,
};

const APPROVAL_TAIL: &str = "Tool approval is REQUIRED";
const SESSION_RECOVERY: &str = "SESSION RECOVERY";

#[test]
fn fun_regular_keeps_post_compaction_protocol_header() {
    let body = build_continuation(CompactionKind::Regular, false, true, PlanRecovery::Active);
    assert!(
        body.contains("POST-COMPACTION PROTOCOL"),
        "fun regular must include the numbered protocol header so the \
         model picks up the task; got: {body}"
    );
    assert!(
        !body.contains("Silently continue"),
        "fun variant must not carry the silent directive: {body}"
    );
}

#[test]
fn silent_regular_uses_silent_directive() {
    let body = build_continuation(CompactionKind::Regular, true, true, PlanRecovery::Active);
    assert!(
        body.contains("Silently continue"),
        "silent regular must explicitly tell the model to continue silently: {body}"
    );
    assert!(
        !body.contains("POST-COMPACTION PROTOCOL"),
        "silent variant must drop the verbose protocol header: {body}"
    );
}

#[test]
fn fun_emergency_invites_fun_cheeky_remark() {
    // The emergency path is where the most-shared one-liners came
    // from. The fun variant MUST carry an explicit invitation, not
    // just allow it implicitly — otherwise the model defaults to
    // silent recovery and the personality moment never fires.
    let body = build_continuation(CompactionKind::Emergency, false, true, PlanRecovery::Active);
    assert!(
        body.contains("fun/cheeky remark"),
        "fun emergency must explicitly invite a fun/cheeky remark: {body}"
    );
}

#[test]
fn silent_emergency_suppresses_acknowledgement() {
    let body = build_continuation(CompactionKind::Emergency, true, true, PlanRecovery::Active);
    assert!(
        body.contains("Silently resume"),
        "silent emergency must direct silent resumption: {body}"
    );
    assert!(
        !body.contains("fun/cheeky remark"),
        "silent variant must not invite a fun remark: {body}"
    );
}

#[test]
fn fun_post_tool_keeps_cursing_allowed_explicit() {
    // The post-tool prompt historically carried "cursing allowed" as
    // an explicit license. That's the line that produces the kind
    // of in-character output users have called out. If we ever
    // trim it we break the documented feature.
    let body = build_continuation(CompactionKind::PostTool, false, true, PlanRecovery::Active);
    assert!(
        body.contains("cursing allowed"),
        "fun post-tool must keep the explicit cursing-allowed license: {body}"
    );
    assert!(body.contains("IMMEDIATE TASK"));
}

#[test]
fn silent_post_tool_drops_cursing_invitation() {
    let body = build_continuation(CompactionKind::PostTool, true, true, PlanRecovery::Active);
    assert!(!body.contains("cursing allowed"));
    assert!(body.contains("Silently continue"));
}

#[test]
fn mid_loop_variants_diverge_on_silent_flag() {
    let fun = build_continuation(CompactionKind::MidLoop, false, true, PlanRecovery::Active);
    let silent = build_continuation(CompactionKind::MidLoop, true, true, PlanRecovery::Active);
    assert_ne!(fun, silent, "the two modes must produce different prompts");
    assert!(fun.contains("POST-COMPACTION PROTOCOL"));
    assert!(silent.contains("Silently continue"));
}

#[test]
fn approval_tail_appended_when_auto_approve_disabled_in_both_modes() {
    for kind in [
        CompactionKind::Regular,
        CompactionKind::MidLoop,
        CompactionKind::Emergency,
        CompactionKind::PostTool,
    ] {
        let fun = build_continuation(kind, false, false, PlanRecovery::Active);
        let silent = build_continuation(kind, true, false, PlanRecovery::Active);
        assert!(
            fun.contains(APPROVAL_TAIL),
            "fun {kind:?} must append the approval reminder when auto_approve=false: {fun}"
        );
        assert!(
            silent.contains(APPROVAL_TAIL),
            "silent {kind:?} must append the approval reminder when auto_approve=false: {silent}"
        );
    }
}

#[test]
fn approval_tail_omitted_when_auto_approve_enabled() {
    for kind in [
        CompactionKind::Regular,
        CompactionKind::MidLoop,
        CompactionKind::Emergency,
        CompactionKind::PostTool,
    ] {
        let fun = build_continuation(kind, false, true, PlanRecovery::Active);
        let silent = build_continuation(kind, true, true, PlanRecovery::Active);
        assert!(
            !fun.contains(APPROVAL_TAIL),
            "fun {kind:?} must NOT carry the approval tail when auto_approve=true: {fun}"
        );
        assert!(
            !silent.contains(APPROVAL_TAIL),
            "silent {kind:?} must NOT carry the approval tail when auto_approve=true: {silent}"
        );
    }
}

#[test]
fn default_agent_config_is_fun_mode() {
    // The `silent_compaction` flag is what selects between modes;
    // verify the config default keeps fun mode active so a fresh
    // install gets the personality moments out of the box.
    let cfg = crate::config::AgentConfig::default();
    assert!(
        !cfg.silent_compaction,
        "AgentConfig::default() must keep silent_compaction=false so fun \
         post-compaction narration is the out-of-the-box behaviour"
    );
}

#[test]
fn session_recovery_hint_present_in_all_variants() {
    // After compaction the agent must check for an active plan and,
    // if coding, load CODE.md. This hint is appended to ALL variants
    // (fun + silent, all 5 kinds) so it never gets lost.
    for kind in [
        CompactionKind::Regular,
        CompactionKind::MidLoop,
        CompactionKind::Emergency,
        CompactionKind::PostTool,
        CompactionKind::Manual,
    ] {
        for silent in [false, true] {
            let body = build_continuation(kind, silent, true, PlanRecovery::Active);
            assert!(
                body.contains(SESSION_RECOVERY),
                "{kind:?} silent={silent} must include the SESSION RECOVERY hint: {body}"
            );
            assert!(
                body.contains("plan") && body.contains("start"),
                "{kind:?} silent={silent} must tell the agent to call plan with start: {body}"
            );
            assert!(
                body.contains("CODE.md"),
                "{kind:?} silent={silent} must mention CODE.md for coding sessions: {body}"
            );
        }
    }
}

#[test]
fn manual_compaction_is_brief() {
    // Manual /compact uses a short sentence, not the full POST-COMPACTION
    // PROTOCOL with numbered steps. The user explicitly triggered it.
    for silent in [false, true] {
        let body = build_continuation(CompactionKind::Manual, silent, true, PlanRecovery::Active);
        assert!(
            !body.contains("POST-COMPACTION PROTOCOL"),
            "Manual compaction must NOT use the full protocol: {body}"
        );
        assert!(
            !body.contains("session_search"),
            "Manual compaction must NOT instruct session_search (brief only): {body}"
        );
        assert!(
            body.contains("IMMEDIATE TASK"),
            "Manual compaction must still reference the IMMEDIATE TASK: {body}"
        );
    }
}

// ---------------------------------------------------------------------
// Issue #499 — a continuation document must never re-bless a COMPLETED
// obligation as the critical immediate task.
//
// Section 0 now carries an explicit status on its first line, and every
// continuation body defers to it. The population is 5 kinds x 2 modes =
// 10 bodies: the original design enumerated only four, and the missing
// kind (Emergency) carried an unconditional resume directive in its
// silent arm.
// ---------------------------------------------------------------------

const ALL_KINDS: [CompactionKind; 5] = [
    CompactionKind::Regular,
    CompactionKind::MidLoop,
    CompactionKind::Emergency,
    CompactionKind::PostTool,
    CompactionKind::Manual,
];

/// Positive arm: all 10 bodies must carry the status rule, so no body can
/// tell the model to continue an obligation unconditionally.
#[test]
fn every_body_carries_the_obligation_status_rule() {
    for kind in ALL_KINDS {
        for silent in [false, true] {
            let body = build_continuation(kind, silent, true, PlanRecovery::Active);
            assert!(
                body.contains("Obligation status"),
                "{kind:?} silent={silent} must defer to section 0's obligation \
                 status — otherwise it re-blesses a completed task: {body}"
            );
        }
    }
}

/// The rule must name all three statuses, must forbid redoing DONE work,
/// and must default an ABSENT status line to UNKNOWN — never to OPEN. An
/// absent status read as OPEN is the original defect with a new coat of
/// paint.
#[test]
fn status_rule_names_all_three_tokens_and_defaults_absent_to_unknown() {
    let body = build_continuation(CompactionKind::Regular, false, true, PlanRecovery::Active);
    for token in ["OPEN", "DONE", "UNKNOWN"] {
        assert!(
            body.contains(token),
            "the status rule must name the {token} status: {body}"
        );
    }
    assert!(
        body.contains("absent status line means UNKNOWN"),
        "an absent status line must default to UNKNOWN, never OPEN: {body}"
    );
    assert!(
        body.contains("do NOT redo"),
        "the DONE branch must forbid redoing the completed work: {body}"
    );
}

/// Negative arm, in code: no body may carry an UNGATED directive to
/// continue the obligation. Every directive must sit on a line that also
/// carries the status gate.
#[test]
fn no_body_carries_an_ungated_continue_directive() {
    const DIRECTIVES: [&str; 5] = [
        "IMMEDIATELY continue the task described",
        "Silently continue the IMMEDIATE TASK",
        "Silently resume from the IMMEDIATE TASK",
        "Silently resume the IMMEDIATE TASK",
        "Resume the IMMEDIATE TASK",
    ];
    let mut checked = 0usize;
    for kind in ALL_KINDS {
        for silent in [false, true] {
            let body = build_continuation(kind, silent, true, PlanRecovery::Active);
            for line in body.lines() {
                if DIRECTIVES.iter().any(|d| line.contains(d)) {
                    checked += 1;
                    assert!(
                        line.contains("Obligation status"),
                        "{kind:?} silent={silent} continues the obligation without \
                         an OPEN gate: {line}"
                    );
                }
            }
        }
    }
    assert!(
        checked >= 9,
        "the negative arm inspected only {checked} directive lines — the guard is \
         measuring the wrong surface"
    );
}
