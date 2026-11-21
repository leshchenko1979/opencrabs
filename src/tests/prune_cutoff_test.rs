//! Retention cutoffs (#841).
//!
//! Three prunes ask one of two helpers for their cutoff, and WHICH helper is
//! the column's choice, not the caller's: unix seconds for `INTEGER` columns
//! (`messages.created_at`, `sessions.created_at`), ISO8601 text for
//! `feedback_ledger.created_at`.
//!
//! These tests pin both representations. A cutoff that stops matching its
//! column's format does not fail — it compares against nothing and the prune
//! silently deletes zero rows forever, which is the one failure mode a
//! retention sweep cannot report about itself.

use crate::services::prune::{SECS_PER_DAY, day_cutoff, iso_day_cutoff};
use chrono::{NaiveDateTime, Utc};

/// Unix-seconds cutoff: `days` days behind now, to the second.
#[test]
fn day_cutoff_is_days_behind_now() {
    let now = Utc::now().timestamp();
    let cutoff = day_cutoff(90);
    let expected = now - 90 * SECS_PER_DAY;
    assert!(
        (cutoff - expected).abs() <= 5,
        "day_cutoff(90) = {cutoff}, expected ~{expected}"
    );
}

/// `0` days is now, not the epoch: the helper never subtracts a window the
/// caller did not ask for. (`0` meaning "disabled" is the CALLER's guard, which
/// returns `Ok(0)` before asking for a cutoff at all.)
#[test]
fn day_cutoff_of_zero_days_is_now() {
    let now = Utc::now().timestamp();
    assert!((day_cutoff(0) - now).abs() <= 5);
}

/// ISO cutoff: parses back with the exact format the column stores, and is the
/// same instant as the unix helper for the same window.
#[test]
fn iso_day_cutoff_matches_the_ledger_column_format() {
    let cutoff = iso_day_cutoff(90);
    let parsed = NaiveDateTime::parse_from_str(&cutoff, "%Y-%m-%dT%H:%M:%SZ").unwrap_or_else(|e| {
        panic!("iso_day_cutoff produced {cutoff:?}, not the column format: {e}")
    });
    let age = Utc::now().naive_utc() - parsed;
    assert!(
        (age.num_seconds() - 90 * SECS_PER_DAY).abs() <= 5,
        "iso_day_cutoff(90) = {cutoff} is {}s old, expected ~{}s",
        age.num_seconds(),
        90 * SECS_PER_DAY
    );
}

/// The ISO representation exists so a lexicographic comparison is chronological.
/// If the format ever gained a fractional second or a numeric offset, this
/// ordering would break while the parse above still passed.
#[test]
fn iso_day_cutoffs_order_lexicographically_by_age() {
    let older = iso_day_cutoff(90);
    let newer = iso_day_cutoff(1);
    assert!(older < newer, "expected {older} < {newer}");
}
