//! The tool-level outcome predicate, #763 and #938.
//!
//! `tool_outcome_status` is the single home for the status a tool execution
//! record is written with. It used to be restated at five call sites (four in
//! `tool_loop.rs`, one in `parallel_tools.rs`), each writing the same three-way
//! `if` — the shape a rule drifts in.
//!
//! Two rules meet here, and they pull in opposite directions:
//!
//! * #763 — a run that deliberately returned non-zero is NOT a tool failure.
//!   Before #763 the process code was folded into `status`, so an intentional
//!   `rc != 0` (a `grep` with no match, a probe's deliberate `exit 1`,
//!   `test 1 -eq 2`) was written as `status = 'error'`, the same value a
//!   genuine tool failure gets, and the failure-rate predicate could not tell
//!   them apart.
//! * #938 — a run that returned non-zero is NOT a success either. After #763
//!   the status was pinned to `success` whenever an `exit_code` was carried,
//!   without ever consulting the code, so a killed or failed process read
//!   `status='success'` beside a non-zero `exit_code`: measured 926 such rows
//!   in a 24 h window, 7.45 % of all success rows, on a binary rebuilt the day
//!   it was filed. Every aggregate that bins by `status` counted them as
//!   successes.
//!
//! The resolution is a third value, `nonzero`: the process ran (so it is not
//! an `error`), and it did not report clean (so it is not a `success`). It is
//! a new value, not a relabel of `error`, so `SUM(status = 'error')` keeps
//! meaning "the tool failed" — which is exactly what #763 bought.

use crate::db::repository::tool_outcome_status;

/// The full matrix, one row per shape a record can arrive in.
///
/// Kills the mutant that pins the status to `success` whenever a code is
/// carried (#938's defect), and the opposite mutant that reads any non-zero
/// code as `error` (which would re-open #763).
#[test]
fn the_predicate_separates_success_from_nonzero_from_error() {
    // (tool's own `success` flag, process exit code, expected status)
    let matrix: &[(bool, Option<i32>, &str)] = &[
        // A process ran and reported clean. The code is the more specific
        // evidence, so it wins over a disagreeing tool flag.
        (true, Some(0), "success"),
        (false, Some(0), "success"),
        // A process ran and returned non-zero: `nonzero`. Not `success` (the
        // defect #938 removes) and not `error` (the defect #763 removed).
        (false, Some(1), "nonzero"),
        (true, Some(1), "nonzero"),
        (false, Some(127), "nonzero"),
        (false, Some(130), "nonzero"),
        (false, Some(143), "nonzero"),
        // No process outcome: the tool's own flag decides, unchanged since
        // before #763, so non-bash behaviour is untouched.
        (false, None, "error"),
        (true, None, "success"),
    ];
    for &(success, exit_code, expected) in matrix {
        assert_eq!(
            tool_outcome_status(success, exit_code),
            expected,
            "success={success} exit_code={exit_code:?} must read {expected:?}"
        );
    }
}

/// `nonzero` is its own value — never folded into `success`, never into
/// `error`.
///
/// Kills the mutant that relabels `nonzero` as `error` (every deliberate
/// `grep`-with-no-match becomes a tool failure again, and the failure-rate
/// predicate counts it) and the mutant that relabels it as `success` (the
/// defect being fixed).
#[test]
fn nonzero_is_distinct_from_success_and_error() {
    for code in [1, 2, 127, 130, 143] {
        let status = tool_outcome_status(false, Some(code));
        assert_ne!(status, "success", "rc={code} is not a success (#938)");
        assert_ne!(status, "error", "rc={code} is not a tool failure (#763)");
        assert_eq!(status, "nonzero", "rc={code} has its own value");
    }
}

/// A missing code keeps the pre-#763 mapping: the tool's own flag decides.
///
/// Kills the mutant that routes "no process ran" onto `nonzero`, which would
/// relabel every non-shell tool failure as a warning and leave `error`
/// unreachable.
#[test]
fn a_missing_code_keeps_the_tool_flag_mapping() {
    assert_eq!(tool_outcome_status(false, None), "error");
    assert_eq!(tool_outcome_status(true, None), "success");
}

/// The vocabulary is closed: `success`, `nonzero`, `error`. Nothing else.
///
/// Kills the mutant that invents a fourth value (e.g. `blocked`, or a
/// per-code string such as `exit_1`), which every consumer that branches on
/// the string would silently mis-bin.
#[test]
fn the_status_vocabulary_is_closed() {
    for success in [false, true] {
        for code in [None, Some(0), Some(1), Some(127), Some(-1)] {
            let status = tool_outcome_status(success, code);
            assert!(
                matches!(status, "success" | "nonzero" | "error"),
                "success={success} exit_code={code:?} produced {status:?}, which is not in the \
                 closed vocabulary"
            );
        }
    }
}
