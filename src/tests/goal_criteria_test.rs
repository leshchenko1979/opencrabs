//! Tests for the three-valued goal verdict vocabulary (#299).
//!
//! The property under test throughout is **direction of failure**: every
//! malformed, partial, or ambiguous input must land on `Uncertain` or
//! `Rejected`, never on `Verified`. A goal that cannot be proven must not be
//! closed, so the tests below assert the safe direction, not just the happy
//! path.

use crate::brain::goal::types::{
    CriterionEvaluation, CriterionStatus, GoalVerdict, JudgeDecision, MAX_CONSECUTIVE_UNCERTAIN,
    aggregate_verdict,
};

/// Build a criterion evaluation with the given status.
fn eval(status: CriterionStatus) -> CriterionEvaluation {
    CriterionEvaluation {
        id: "c1".to_string(),
        criterion: "the build is green".to_string(),
        status,
        evidence: "receipt".to_string(),
    }
}

// ---------------------------------------------------------------------------
// aggregate_verdict
// ---------------------------------------------------------------------------

#[test]
fn aggregate_all_met_is_verified() {
    let criteria = vec![eval(CriterionStatus::Met), eval(CriterionStatus::Met)];
    assert_eq!(aggregate_verdict(&criteria, 2), GoalVerdict::Verified);
}

#[test]
fn aggregate_any_unmet_is_rejected() {
    let criteria = vec![eval(CriterionStatus::Met), eval(CriterionStatus::Unmet)];
    assert_eq!(aggregate_verdict(&criteria, 2), GoalVerdict::Rejected);
}

#[test]
fn aggregate_any_no_evidence_is_uncertain() {
    let criteria = vec![
        eval(CriterionStatus::Met),
        eval(CriterionStatus::NoEvidence),
    ];
    assert_eq!(aggregate_verdict(&criteria, 2), GoalVerdict::Uncertain);
}

#[test]
fn aggregate_unmet_outranks_no_evidence() {
    // A contradiction is a stronger signal than an absence of evidence: a goal
    // whose criterion is provably unmet is Rejected even if others are blank.
    let criteria = vec![
        eval(CriterionStatus::NoEvidence),
        eval(CriterionStatus::Unmet),
    ];
    assert_eq!(aggregate_verdict(&criteria, 2), GoalVerdict::Rejected);
}

#[test]
fn aggregate_empty_criteria_is_uncertain() {
    // No declared criteria is never a licence to terminate.
    assert_eq!(aggregate_verdict(&[], 0), GoalVerdict::Uncertain);
}

#[test]
fn aggregate_no_criteria_declared_ignores_evaluations() {
    // A judge that invents evaluations for a goal with no declared criteria
    // still cannot produce Verified.
    let criteria = vec![eval(CriterionStatus::Met)];
    assert_eq!(aggregate_verdict(&criteria, 0), GoalVerdict::Uncertain);
}

#[test]
fn aggregate_partial_reply_is_uncertain() {
    // Judge evaluated 2 of 3 declared criteria and both were MET. The
    // unevaluated remainder counts as missing evidence.
    let criteria = vec![eval(CriterionStatus::Met), eval(CriterionStatus::Met)];
    assert_eq!(aggregate_verdict(&criteria, 3), GoalVerdict::Uncertain);
}

#[test]
fn aggregate_empty_evaluations_with_declared_criteria_is_uncertain() {
    assert_eq!(aggregate_verdict(&[], 3), GoalVerdict::Uncertain);
}

#[test]
fn aggregate_extra_evaluations_do_not_block_verified() {
    // A judge that evaluates more entries than were declared (e.g. it split a
    // compound criterion) still verifies when every entry is MET.
    let criteria = vec![
        eval(CriterionStatus::Met),
        eval(CriterionStatus::Met),
        eval(CriterionStatus::Met),
    ];
    assert_eq!(aggregate_verdict(&criteria, 2), GoalVerdict::Verified);
}

// ---------------------------------------------------------------------------
// GoalVerdict::from_str_lossy
// ---------------------------------------------------------------------------

#[test]
fn verdict_lossy_maps_legacy_done_to_verified() {
    assert_eq!(GoalVerdict::from_str_lossy("DONE"), GoalVerdict::Verified);
}

#[test]
fn verdict_lossy_maps_garbage_to_uncertain() {
    assert_eq!(
        GoalVerdict::from_str_lossy("garbage"),
        GoalVerdict::Uncertain
    );
    assert_eq!(GoalVerdict::from_str_lossy(""), GoalVerdict::Uncertain);
}

#[test]
fn verdict_lossy_accepts_new_tokens() {
    assert_eq!(
        GoalVerdict::from_str_lossy("VERIFIED"),
        GoalVerdict::Verified
    );
    assert_eq!(
        GoalVerdict::from_str_lossy("UNCERTAIN"),
        GoalVerdict::Uncertain
    );
    assert_eq!(
        GoalVerdict::from_str_lossy("REJECTED"),
        GoalVerdict::Rejected
    );
}

#[test]
fn verdict_lossy_is_case_and_whitespace_tolerant() {
    assert_eq!(
        GoalVerdict::from_str_lossy("  done  "),
        GoalVerdict::Verified
    );
    assert_eq!(
        GoalVerdict::from_str_lossy("verified\n"),
        GoalVerdict::Verified
    );
}

#[test]
fn verdict_wire_tokens_round_trip() {
    for verdict in [
        GoalVerdict::Verified,
        GoalVerdict::Uncertain,
        GoalVerdict::Rejected,
    ] {
        assert_eq!(GoalVerdict::from_str_lossy(verdict.as_str()), verdict);
    }
}

#[test]
fn verdict_never_upgrades_unknown_to_verified() {
    // Exhaustive-ish sweep: nothing that is not recognisably a success token
    // may produce Verified, and CONTINUE (the retired loop token) must not.
    for token in ["CONTINUE", "IN_PROGRESS", "in_progress", "TRUE", "1", "yes"] {
        assert_ne!(
            GoalVerdict::from_str_lossy(token),
            GoalVerdict::Verified,
            "token {:?} must not verify a goal",
            token
        );
    }
}

// ---------------------------------------------------------------------------
// CriterionStatus::from_str_lossy
// ---------------------------------------------------------------------------

#[test]
fn status_lossy_accepts_casing_and_separator_drift() {
    assert_eq!(CriterionStatus::from_str_lossy("MET"), CriterionStatus::Met);
    assert_eq!(CriterionStatus::from_str_lossy("met"), CriterionStatus::Met);
    assert_eq!(
        CriterionStatus::from_str_lossy("no_evidence"),
        CriterionStatus::NoEvidence
    );
    assert_eq!(
        CriterionStatus::from_str_lossy("NO EVIDENCE"),
        CriterionStatus::NoEvidence
    );
}

#[test]
fn status_lossy_never_upgrades_unknown_to_met() {
    for token in ["", "maybe", "partial", "unknown", "TBD"] {
        assert_eq!(
            CriterionStatus::from_str_lossy(token),
            CriterionStatus::NoEvidence,
            "token {:?} must not be read as MET",
            token
        );
    }
}

#[test]
fn status_wire_tokens_round_trip() {
    for status in [
        CriterionStatus::Met,
        CriterionStatus::Unmet,
        CriterionStatus::NoEvidence,
    ] {
        assert_eq!(CriterionStatus::from_str_lossy(status.as_str()), status);
    }
}

#[test]
fn status_default_is_no_evidence() {
    assert_eq!(CriterionStatus::default(), CriterionStatus::NoEvidence);
}

// ---------------------------------------------------------------------------
// JudgeDecision::parse_or_continue
// ---------------------------------------------------------------------------

#[test]
fn parse_or_continue_fails_open_on_garbage() {
    let decision = JudgeDecision::parse_or_continue("not json at all");
    assert_eq!(decision.verdict, GoalVerdict::Uncertain);
    assert!(decision.criteria.is_empty());
    assert!(decision.reason.contains("parse error"));
}

#[test]
fn parse_or_continue_fails_open_on_truncated_multibyte() {
    // A cut landing inside a multi-byte codepoint must not panic while the
    // warning line is being formatted.
    let raw = format!(
        "{{\"verdict\": \"VERIFIED\", \"reason\": \"{}",
        "日".repeat(500)
    );
    let decision = JudgeDecision::parse_or_continue(&raw);
    assert_eq!(decision.verdict, GoalVerdict::Uncertain);
}

#[test]
fn parse_accepts_full_well_formed_reply() {
    let raw = r#"{
        "verdict": "VERIFIED",
        "reason": "all criteria met",
        "criteria": [
            {"id": "c1", "criterion": "build is green", "status": "MET", "evidence": "run 123"},
            {"id": "c2", "criterion": "tests pass", "status": "MET", "evidence": "run 123"}
        ]
    }"#;
    let decision = JudgeDecision::parse_or_continue(raw);
    assert_eq!(decision.verdict, GoalVerdict::Verified);
    assert_eq!(decision.criteria.len(), 2);
    assert_eq!(
        aggregate_verdict(&decision.criteria, 2),
        GoalVerdict::Verified
    );
}

#[test]
fn parse_missing_verdict_defaults_to_uncertain() {
    let raw = r#"{"reason": "still working"}"#;
    let decision = JudgeDecision::parse_or_continue(raw);
    assert_eq!(decision.verdict, GoalVerdict::Uncertain);
    assert!(decision.criteria.is_empty());
}

#[test]
fn parse_legacy_done_token_is_advisory_verified() {
    let raw = r#"{"verdict": "DONE", "reason": "legacy reply"}"#;
    let decision = JudgeDecision::parse_or_continue(raw);
    assert_eq!(decision.verdict, GoalVerdict::Verified);
    // ...but with no criteria evaluated the aggregate cannot verify.
    assert_eq!(
        aggregate_verdict(&decision.criteria, 0),
        GoalVerdict::Uncertain
    );
}

#[test]
fn parse_null_verdict_keeps_the_criteria() {
    // The regression this guards: a single bad `verdict` token used to fail the
    // whole reply, discarding the only evidence the judge produced.
    let raw = r#"{
        "verdict": null,
        "reason": "hedged",
        "criteria": [{"id": "c1", "criterion": "x", "status": "MET", "evidence": "y"}]
    }"#;
    let decision = JudgeDecision::parse_or_continue(raw);
    assert_eq!(decision.verdict, GoalVerdict::Uncertain);
    assert_eq!(decision.criteria.len(), 1);
    assert_eq!(decision.criteria[0].status, CriterionStatus::Met);
}

#[test]
fn parse_numeric_verdict_keeps_the_criteria() {
    let raw = r#"{
        "verdict": 1,
        "reason": "hedged",
        "criteria": [{"id": "c1", "criterion": "x", "status": "MET", "evidence": "y"}]
    }"#;
    let decision = JudgeDecision::parse_or_continue(raw);
    assert_eq!(decision.verdict, GoalVerdict::Uncertain);
    assert_eq!(decision.criteria.len(), 1);
}

#[test]
fn parse_malformed_criterion_keeps_sibling_count() {
    // A malformed entry must not be DROPPED: dropping it would shrink the
    // count below `declared` and make a partial reply look complete.
    let raw = r#"{
        "verdict": "VERIFIED",
        "reason": "looks done",
        "criteria": [
            {"id": "c1", "criterion": "a", "status": "MET", "evidence": "e1"},
            {"id": "c2", "criterion": "b", "status": {"nested": "junk"}},
            {"id": "c3", "criterion": "c", "status": "MET", "evidence": "e3"}
        ]
    }"#;
    let decision = JudgeDecision::parse_or_continue(raw);
    assert_eq!(decision.criteria.len(), 3);
    assert_eq!(decision.criteria[1].status, CriterionStatus::NoEvidence);
    assert_eq!(
        aggregate_verdict(&decision.criteria, 3),
        GoalVerdict::Uncertain
    );
}

#[test]
fn parse_missing_status_is_no_evidence() {
    let raw = r#"{
        "verdict": "VERIFIED",
        "reason": "looks done",
        "criteria": [{"id": "c1", "criterion": "a"}]
    }"#;
    let decision = JudgeDecision::parse_or_continue(raw);
    assert_eq!(decision.criteria[0].status, CriterionStatus::NoEvidence);
    assert_eq!(
        aggregate_verdict(&decision.criteria, 1),
        GoalVerdict::Uncertain
    );
}

#[test]
fn parse_accepts_uncertain_and_rejected_verdicts() {
    let uncertain =
        JudgeDecision::parse_or_continue(r#"{"verdict": "UNCERTAIN", "reason": "no receipts"}"#);
    assert_eq!(uncertain.verdict, GoalVerdict::Uncertain);

    let rejected = JudgeDecision::parse_or_continue(
        r#"{"verdict": "REJECTED", "reason": "criterion c1 failed"}"#,
    );
    assert_eq!(rejected.verdict, GoalVerdict::Rejected);
}

#[test]
fn parse_corrections_round_trip() {
    let raw = r#"{
        "verdict": "UNCERTAIN",
        "reason": "missing evidence",
        "corrections": "paste the CI run id",
        "criteria": []
    }"#;
    let decision = JudgeDecision::parse_or_continue(raw);
    assert_eq!(decision.corrections.as_deref(), Some("paste the CI run id"));
}

#[test]
fn uncertain_cap_is_three() {
    // The constant the manager parks on (#299). Pinned here because a silent
    // change to it changes how long a non-converging loop spins.
    assert_eq!(MAX_CONSECUTIVE_UNCERTAIN, 3);
}
