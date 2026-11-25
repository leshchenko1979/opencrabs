//! #1015 — skip the summariser only when tier-0 offload leaves comfortable headroom.

use crate::brain::agent::service::compaction::tier0_offload_resolves_compaction;
use crate::brain::agent::service::nudge::{PRESSURE_WARN_CEILING, PRESSURE_WARN_FLOOR};

#[test]
fn offload_to_fifty_percent_resolves_without_summariser() {
    assert!(tier0_offload_resolves_compaction(true, 50.0));
    assert!(tier0_offload_resolves_compaction(true, 49.9));
}

#[test]
fn offload_to_sixty_two_percent_still_needs_summariser() {
    assert!(!tier0_offload_resolves_compaction(true, 62.0));
    assert!(!tier0_offload_resolves_compaction(
        true,
        PRESSURE_WARN_FLOOR
    ));
    assert!(!tier0_offload_resolves_compaction(
        true,
        PRESSURE_WARN_CEILING
    ));
}

#[test]
fn still_above_sixty_five_after_offload_still_needs_summariser() {
    assert!(!tier0_offload_resolves_compaction(true, 66.0));
    assert!(!tier0_offload_resolves_compaction(true, 70.0));
}

#[test]
fn never_crossed_trigger_at_sixty_four_is_resolved() {
    assert!(tier0_offload_resolves_compaction(false, 64.0));
    assert!(tier0_offload_resolves_compaction(
        false,
        PRESSURE_WARN_FLOOR
    ));
}
