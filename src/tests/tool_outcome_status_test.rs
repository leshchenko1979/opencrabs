//! The tool-level outcome predicate, #763.
//!
//! `tool_outcome_status` is the single home for the status a tool execution
//! record is written with. It used to be restated at five call sites (four in
//! `tool_loop.rs`, one in `parallel_tools.rs`), each writing the same three-way
//! `if` — the shape a rule drifts in.
//!
//! The split it encodes: a tool that RAN ITS PROCESS TO COMPLETION is a tool
//! that ran, whatever code came back, so a carried `exit_code` pins the status
//! to `success`. `error` is reserved for the tool itself failing. A tool that
//! spawns no process carries no code and keeps the pre-#763 mapping of its own
//! `success` flag, so non-bash behaviour is untouched.
//!
//! Before #763 the process code was folded into `status`, so an intentional
//! `rc != 0` (a `grep` with no match, a probe's deliberate `exit 1`,
//! `test 1 -eq 2`) was written as `status = 'error'` — the same value a genuine
//! tool failure gets, and the failure-rate predicate could not tell them apart.

use crate::db::repository::tool_outcome_status;

/// The full matrix. Each row is a case the pre-#763 record could not separate
/// from at least one of its neighbours.
#[test]
fn the_predicate_separates_process_outcome_from_tool_failure() {
    // An intentional `rc != 0`: a process ran and completed. Pre-#763 this was
    // `status = 'error'` — the defect #763 exists to remove.
    assert_eq!(tool_outcome_status(false, Some(1)), "success");
    // A process ran and reported success, e.g. `grep` with a match.
    assert_eq!(tool_outcome_status(true, Some(0)), "success");
    // A process ran and failed — the code is carried on the row, but the tool
    // still did its job, so the status is the tool's own.
    assert_eq!(tool_outcome_status(false, Some(127)), "success");
    // No process ran and the tool reports failure: unchanged, and the only
    // shape that reads `error`.
    assert_eq!(tool_outcome_status(false, None), "error");
    // No process ran and the tool reports success: unchanged.
    assert_eq!(tool_outcome_status(true, None), "success");
}

/// The code's VALUE must never reach the predicate — a non-zero code is not a
/// failure. This pins that, so a later "improvement" that starts treating
/// `Some(0)` as the only success has to break a test rather than a series.
#[test]
fn a_non_zero_code_is_not_a_failure() {
    for code in [1, 2, 127, 130, 143] {
        assert_eq!(
            tool_outcome_status(false, Some(code)),
            "success",
            "rc={code} is a completed run, not a tool failure"
        );
    }
}
