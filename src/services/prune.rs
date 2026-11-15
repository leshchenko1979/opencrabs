//! Shared day-window cutoffs for the retention sweeps (#841).
//!
//! Three prunes implemented the same "delete rows older than N days" operation,
//! each with its own copy of the cutoff prologue — and two of them used a
//! different representation of the same predicate. The `86_400` literal was
//! duplicated verbatim, and nothing in the code said which representation
//! belonged to which column, so the next prune added picked one idiom by coin
//! flip.
//!
//! Both helpers here are that statement, in one place:
//!
//! | Helper | Column type | Serves |
//! |---|---|---|
//! | [`day_cutoff`] | `INTEGER`, unix seconds | `messages.created_at`, `sessions.created_at` |
//! | [`iso_day_cutoff`] | `TEXT`, ISO8601 `%Y-%m-%dT%H:%M:%SZ` | `feedback_ledger.created_at` |
//!
//! The split is a property of the column, never of the caller: a prune's
//! representation follows the column it compares against, because a lexicographic
//! comparison on ISO8601 text is chronological while the same comparison on a
//! mixed column type would silently match nothing.
//!
//! ## `0` disables every sweep
//!
//! Each caller guards `days == 0` and returns `Ok(0)` before asking for a
//! cutoff, so an upgrade never deletes history by itself. That contract is
//! stated here because it is shared by all of them; the guard itself stays at
//! the call site, where "this sweep is disabled" is the function's own return.

use chrono::Utc;

/// Seconds in one day. The single copy: every day-window cutoff is built from
/// this, so the two sweeps cannot drift apart by a factor of 86 400.
pub(crate) const SECS_PER_DAY: i64 = 86_400;

/// Cutoff for a column that stores **unix seconds** as `INTEGER`.
///
/// Returns the instant `days` days before now, as an epoch second — compare it
/// directly against `created_at < ?1`.
pub(crate) fn day_cutoff(days: u32) -> i64 {
    Utc::now().timestamp() - i64::from(days) * SECS_PER_DAY
}

/// Cutoff for a column that stores **ISO8601 text** (`%Y-%m-%dT%H:%M:%SZ`).
///
/// Formatted exactly like the rows it is compared against, so the `DELETE`'s
/// range walk stays lexicographic and can use the column's index instead of
/// scanning the table.
pub(crate) fn iso_day_cutoff(days: u32) -> String {
    (Utc::now() - chrono::Duration::days(i64::from(days)))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}
