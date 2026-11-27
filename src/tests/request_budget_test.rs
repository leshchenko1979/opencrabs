//! Request-budget regressions for bounded context windows.
//!
//! A model's context window is a shared input + output budget. Sending the
//! global 65,536-token output cap unchanged on a 200K route leaves only ~134K
//! for input. That is exactly where the observed Astra session began compacting
//! on every tool round and occasionally lost its stream. Keep one policy here:
//! reserve no more than 20% of the active window for output, while preserving
//! the configured cap on large-window providers.

use crate::brain::agent::service::request_budget::{bounded_output_tokens, thresholds_for};

#[test]
fn two_hundred_k_window_caps_output_at_forty_k() {
    assert_eq!(bounded_output_tokens(65_536, 200_000), 40_000);
}

#[test]
fn one_million_window_keeps_configured_output_cap() {
    assert_eq!(bounded_output_tokens(65_536, 1_000_000), 65_536);
}

#[test]
fn tiny_window_still_leaves_input_headroom() {
    assert_eq!(bounded_output_tokens(65_536, 8_192), 1_638);
}

#[test]
fn zero_window_falls_back_to_configured_cap() {
    assert_eq!(bounded_output_tokens(65_536, 0), 65_536);
}

// ── #1045: the compaction thresholds have one home ──────────────────────────

/// The seam reports the percentages the gates actually use, so a call site that
/// reads it cannot drift from the gate that does not (yet).
#[test]
fn the_seam_reports_the_live_thresholds() {
    let t = thresholds_for(200_000);
    assert_eq!(t.compaction_target_pct, 65.0, "Tier-1 soft trigger");
    assert_eq!(t.backpressure_ceiling_pct, 80.0, "backpressure ceiling");
    assert_eq!(
        t.pre_truncate_pct, 85.0,
        "the historical pre-truncation target is reported, not gated on"
    );
}

/// Today every window gets the same policy. Pinned so a future window-dependent
/// change is a deliberate edit to this seam, not an accident.
#[test]
fn every_window_gets_the_same_policy_today() {
    for window in [0, 8_192, 200_000, 1_000_000] {
        assert_eq!(
            thresholds_for(window),
            thresholds_for(200_000),
            "window {window} must take the default policy"
        );
    }
}

/// The compaction gate and the warning band that sits below it read one number:
/// `nudge::PRESSURE_WARN_CEILING` aliases the target rather than restating 65.
#[test]
fn the_warning_band_ceiling_is_the_compaction_target() {
    assert_eq!(
        crate::brain::agent::service::nudge::PRESSURE_WARN_CEILING,
        thresholds_for(0).compaction_target_pct,
        "the band must end exactly where the gate fires"
    );
}
