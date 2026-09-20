//! The resume path feeds the ctx meter exactly like the fresh path (#450).
//!
//! A resumed turn's progress callback had no `ProgressEvent::TokenCount` arm,
//! so it fell through to the `_ => {}` catch-all. `sections.ctx` was written
//! only by the `CompactionSummary` arm, which meant a resumed turn rendered no
//! ctx segment until a compaction happened to fire.
//!
//! Both callbacks are built inline at their call sites, so there is no shared
//! helper to drive and the arm set is observable only by reading the sources.
//! These tests pin that structure directly — the same shape
//! `new_session_pane_binding_test.rs` uses for its two inline call sites.
//!
//! FALSIFYING INPUT, stated because a source-pinned test is worth only its
//! negative half: on the pre-fix tree `resume.rs` contained ZERO occurrences of
//! `ProgressEvent::TokenCount`, so `the_resume_arm_is_present`,
//! `the_two_token_count_arms_are_identical` and
//! `the_two_callbacks_handle_the_same_events` all fail there. They are not
//! vacuously green.
//!
//! The marker below is an arm HEADER (`... => {`), not a bare type name, so a
//! prose mention of `TokenCount` in a comment cannot satisfy it.

use std::collections::BTreeSet;

const RESUME_SRC: &str = include_str!("../channels/telegram/resume.rs");
const PROGRESS_SRC: &str = include_str!("../channels/telegram/progress.rs");

/// The arm header both callbacks must carry.
const MARK: &str = "ProgressEvent::TokenCount(tokens) => {";

/// Extract the brace-matched body of the arm introduced by `marker`.
///
/// Returns the text from the arm's opening `{` through its matching `}`. The
/// bodies hold no braces inside string literals, so a depth counter is
/// sufficient — a real parser would be more machinery than the assertion needs.
fn arm_body<'a>(src: &'a str, marker: &str) -> &'a str {
    let Some(at) = src.find(marker) else {
        panic!("no `{marker}` arm in the source — the arm is missing, not merely wrong");
    };
    let Some(rel) = src[at..].find('{') else {
        panic!("the `{marker}` arm opens no block");
    };
    let open = at + rel;
    let mut depth = 0usize;
    for (offset, &byte) in src.as_bytes()[open..].iter().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return &src[open..=open + offset];
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces in the `{marker}` arm");
}

/// Collapse all whitespace, so indentation alone cannot mask a real divergence
/// between the two arm bodies.
fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Every `ProgressEvent` variant a callback matches on.
///
/// A variant counts only when it is an ARM HEADER — the name followed by its
/// binding or its body (`Variant { .. }`, `Variant(x)`, or `Variant => ..`).
/// A mention in prose or a doc comment is followed by ordinary words, so the
/// shape test keeps the scan from reading comments as arms. That matters here
/// because the assertion below is a set EQUALITY: a comment present in one file
/// and not the other would otherwise read as drift.
///
/// The scan reads the whole file rather than just the callback block. Scoping
/// it would mean brace-matching from the closure binding — more machinery than
/// the assertion needs, and the shape test already excludes every non-arm.
fn arm_variants(src: &str) -> BTreeSet<String> {
    let mut variants = BTreeSet::new();
    let mut rest = src;
    while let Some(at) = rest.find("ProgressEvent::") {
        let after = &rest[at + "ProgressEvent::".len()..];
        let name: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        let tail = after[name.len()..].trim_start();
        if !name.is_empty()
            && (tail.starts_with('{') || tail.starts_with('(') || tail.starts_with("=>"))
        {
            variants.insert(name);
        }
        // Always advance past the marker, so an empty identifier cannot spin.
        rest = after;
    }
    variants
}

#[test]
fn the_resume_arm_is_present() {
    assert_eq!(
        RESUME_SRC.matches(MARK).count(),
        1,
        "the resume callback must handle TokenCount exactly once"
    );
}

#[test]
fn the_fresh_path_arm_is_still_present() {
    // The port ADDED an arm; it did not move the original. If this ever reads
    // zero, the change relocated the handler instead of duplicating it.
    assert_eq!(PROGRESS_SRC.matches(MARK).count(), 1);
}

/// The whole point of the unit: both callbacks must behave identically on
/// TokenCount. Comparing the brace-matched bodies catches a port that is subtly
/// wrong — a dropped `dirty` flag, a swapped argument, a hardcoded limit — in a
/// way a bare "the arm exists" grep cannot.
#[test]
fn the_two_token_count_arms_are_identical() {
    assert_eq!(
        squash(arm_body(RESUME_SRC, MARK)),
        squash(arm_body(PROGRESS_SRC, MARK)),
        "the resume arm drifted from the fresh-turn arm — parity is the contract"
    );
}

/// An arm placed after the catch-all would be dead code while the sources still
/// contained the marker, so presence alone does not prove the arm can run.
#[test]
fn the_resume_arm_precedes_the_catch_all() {
    let Some(arm) = RESUME_SRC.find(MARK) else {
        panic!("the resume callback carries no TokenCount arm");
    };
    let Some(catch_all) = RESUME_SRC.find("_ => {}") else {
        panic!("the progress match keeps a catch-all arm");
    };
    assert!(
        arm < catch_all,
        "the TokenCount arm sits after the catch-all, so it can never run"
    );
}

/// The arm must feed the footer from the session's own limit rather than a
/// literal: a hardcoded max would render a plausible but wrong percentage on
/// any model whose window differs.
#[test]
fn the_resume_arm_uses_the_session_ctx_limit() {
    let body = arm_body(RESUME_SRC, MARK);
    assert!(body.contains("ctx_max"));
    assert!(body.contains("crate::utils::format_ctx_footer"));
    assert!(body.contains("s.sections.ctx"));
}

/// Parity is the whole ARM SET, not just TokenCount: the next event added to
/// the fresh path and forgotten on the resume path is this same defect one
/// event later, and it would render the resume card short a segment with no
/// test failing. Set equality pins the contract rather than one instance of it.
#[test]
fn the_two_callbacks_handle_the_same_events() {
    let resume = arm_variants(RESUME_SRC);
    let fresh = arm_variants(PROGRESS_SRC);
    // An empty or near-empty scan would make the equality vacuously true, so the
    // non-vacuity floor is anchored to a fact already pinned above: the fresh
    // path handles TokenCount, so a scan that misses it is broken.
    assert!(
        fresh.contains("TokenCount"),
        "the arm scan did not even find TokenCount — the scan is broken, not the code"
    );
    assert_eq!(
        resume, fresh,
        "the resume callback's ProgressEvent arm set drifted from the fresh path's"
    );
}
