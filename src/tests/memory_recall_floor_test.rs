//! #912 — the recall floor is configuration, not a compiled constant.
//!
//! `RECALL_MIN_SCORE` was calibrated against one corpus pair, so it encodes a
//! measurement rather than a law: an install with a different corpus needs a
//! different value. It moves to `[memory] recall_min_score`, and this battery
//! pins the three things that make moving it safe —
//!
//! 1. the resolver is the ONLY reader (both the per-turn path and `eval` call
//!    it, so a swept curve can never describe a configuration nobody runs),
//! 2. an unset or nonsensical value falls back to the compiled default rather
//!    than to zero,
//! 3. the clamp band stops a typo from either disabling the filter or emptying
//!    every ranking silently.

use crate::brain::memory_recall::{
    RECALL_MIN_SCORE, RECALL_MIN_SCORE_MAX, RECALL_MIN_SCORE_MIN, clamp_recall_min_score,
};
use crate::config::MemoryConfig;

/// Unset means "whatever this build was calibrated to", never 0.0 — a floor of
/// zero would put an unfiltered shortlist on every turn.
#[test]
fn an_unset_floor_uses_the_compiled_default() {
    assert_eq!(clamp_recall_min_score(None), RECALL_MIN_SCORE);
}

/// A NaN cannot arise from TOML, but it can from arithmetic upstream, and it
/// must not propagate: every comparison against NaN is false, so an unclamped
/// NaN would disable the floor by accident.
#[test]
fn a_nan_floor_uses_the_compiled_default() {
    assert_eq!(clamp_recall_min_score(Some(f64::NAN)), RECALL_MIN_SCORE);
}

/// Too low is clamped up: below the band the shortlist stops filtering, and a
/// `0.0` typo must not be able to reproduce that.
#[test]
fn a_floor_below_the_band_clamps_up() {
    assert_eq!(clamp_recall_min_score(Some(0.0)), RECALL_MIN_SCORE_MIN);
    assert_eq!(clamp_recall_min_score(Some(-1.0)), RECALL_MIN_SCORE_MIN);
}

/// Too high is clamped down: above the band buys no precision, only silence.
#[test]
fn a_floor_above_the_band_clamps_down() {
    assert_eq!(clamp_recall_min_score(Some(9.0)), RECALL_MIN_SCORE_MAX);
}

/// The band's own endpoints are inside it — clamping is inclusive, so a value
/// set exactly to a boundary is honoured rather than nudged.
#[test]
fn the_band_endpoints_are_honoured() {
    assert_eq!(
        clamp_recall_min_score(Some(RECALL_MIN_SCORE_MIN)),
        RECALL_MIN_SCORE_MIN
    );
    assert_eq!(
        clamp_recall_min_score(Some(RECALL_MIN_SCORE_MAX)),
        RECALL_MIN_SCORE_MAX
    );
}

/// A value inside the band passes through untouched: this is the whole point
/// of the knob, and a resolver that "helpfully" snapped to the default would
/// make the sweep meaningless.
#[test]
fn a_floor_inside_the_band_passes_through() {
    assert_eq!(clamp_recall_min_score(Some(0.2)), 0.2);
    assert_eq!(clamp_recall_min_score(Some(0.45)), 0.45);
}

/// `[memory] recall_min_score = 0.2` reaches `MemoryConfig`, and the resolver
/// turns it into exactly the floor the recall path uses.
#[test]
fn the_config_key_reaches_the_resolver() {
    let parsed: MemoryConfig =
        toml::from_str("recall_min_score = 0.2").expect("[memory] table parses");
    assert_eq!(parsed.recall_min_score, Some(0.2));
    assert_eq!(clamp_recall_min_score(parsed.recall_min_score), 0.2);
}

/// An install that never sets the key keeps the compiled default — the same
/// agreement `Default::default()` and serde's empty-table parse must keep.
#[test]
fn an_absent_key_leaves_the_floor_unset() {
    let parsed: MemoryConfig = toml::from_str("").expect("an empty [memory] table must parse");
    assert_eq!(parsed.recall_min_score, None);
    assert_eq!(
        clamp_recall_min_score(parsed.recall_min_score),
        clamp_recall_min_score(MemoryConfig::default().recall_min_score),
    );
    assert_eq!(clamp_recall_min_score(None), RECALL_MIN_SCORE);
}
