//! #844: the retention switches live under `[agent.retention]`, and the
//! maintenance timings carry an ordering invariant.
//!
//! Two things have to hold for the move to be safe:
//!
//! 1. **Nothing changes for an existing config.** The four flat keys are still
//!    read under their original names, and an untouched config resolves to the
//!    historical defaults (subagent 7, messages 0, ledger 0, files 7).
//! 2. **The fallback is a fallback.** `default_days` fills in only for row
//!    prunes that name no key of their own — it must never override an explicit
//!    key, and it must never reach the file purge, which runs at startup and
//!    deletes files rather than rows.
//!
//! The timing half pins the ordering that #838 and #840 were each a violation
//! of: a retry at the tick is phase-locked, and a zero warmup offset puts both
//! loops on the shared permit in the same instant.

use crate::config::AgentConfig;
use crate::services::maintenance::{
    MAINTENANCE_CONTENDED_RETRY, MEMORY_RECLAIM_TICK, MaintenanceTiming, RECLAIM_WARMUP_OFFSET,
    SWEEP_INTERVAL, SWEEP_WARMUP,
};

fn parse(raw: &str) -> AgentConfig {
    toml::from_str(raw).expect("AgentConfig must parse")
}

/// An untouched config keeps every historical default, so upgrading changes
/// nothing and never starts deleting data silently.
#[test]
fn untouched_config_keeps_the_historical_defaults() {
    let cfg = AgentConfig::default();
    assert_eq!(cfg.subagent_session_ttl_days(), 7);
    assert_eq!(cfg.message_retention_days(), 0);
    assert_eq!(cfg.feedback_ledger_retention_days(), 0);
    assert_eq!(cfg.usage_ledger_retention_days(), 0);
    assert_eq!(cfg.tool_output_retention_days(), 7);
    assert_eq!(cfg.command_run_retention_days(), 2);

    // Same through the parser: an absent `[agent.retention]` is not an error
    // and not a behaviour change.
    let parsed = parse("");
    assert_eq!(parsed.subagent_session_ttl_days(), 7);
    assert_eq!(parsed.message_retention_days(), 0);
    assert_eq!(parsed.feedback_ledger_retention_days(), 0);
    assert_eq!(parsed.usage_ledger_retention_days(), 0);
    assert_eq!(parsed.tool_output_retention_days(), 7);
    assert_eq!(parsed.command_run_retention_days(), 2);
}

/// The deprecated flat keys are still read under their original TOML names —
/// this is the whole back-compat guarantee, and it is a serde `rename`, so a
/// typo here silently reverts every existing operator to defaults.
#[test]
fn legacy_flat_keys_still_resolve() {
    let cfg = parse(
        r#"
subagent_session_ttl_days = 30
message_retention_days = 90
feedback_ledger_retention_days = 45
tool_output_retention_days = 3
"#,
    );
    assert_eq!(cfg.subagent_session_ttl_days(), 30);
    assert_eq!(cfg.message_retention_days(), 90);
    assert_eq!(cfg.feedback_ledger_retention_days(), 45);
    assert_eq!(cfg.tool_output_retention_days(), 3);
}

/// The new section resolves, including a literal `0` — which must stay a real
/// value meaning "disabled", not be mistaken for "unset".
#[test]
fn retention_section_keys_resolve() {
    let cfg = parse(
        r#"
[retention]
subagent_sessions = 14
messages = 60
feedback_ledger = 30
tool_output_files = 2
"#,
    );
    assert_eq!(cfg.subagent_session_ttl_days(), 14);
    assert_eq!(cfg.message_retention_days(), 60);
    assert_eq!(cfg.feedback_ledger_retention_days(), 30);
    assert_eq!(cfg.tool_output_retention_days(), 2);

    let zeroed = parse(
        r#"
[retention]
subagent_sessions = 0
"#,
    );
    assert_eq!(
        zeroed.subagent_session_ttl_days(),
        0,
        "an explicit 0 disables the prune; it must not fall through to the default"
    );
}

/// #849: a finished command's files get their own window, deliberately SHORTER
/// than the file window it was split from.
///
/// It never inherits `default_days`: that fallback governs the ROW prunes, and
/// a value an operator set for rows must not silently shorten how long a
/// command's captured output survives.
#[test]
fn command_runs_resolves_on_its_own_key_and_never_inherits_default_days() {
    assert_eq!(AgentConfig::default().command_run_retention_days(), 2);

    let explicit = parse("[retention]\ncommand_runs = 5\n");
    assert_eq!(explicit.command_run_retention_days(), 5);

    let with_default = parse("[retention]\ndefault_days = 30\n");
    assert_eq!(
        with_default.command_run_retention_days(),
        2,
        "default_days governs the row prunes, not the command file window"
    );

    // A literal 0 is a real value here too: keep finished commands forever.
    let zeroed = parse("[retention]\ncommand_runs = 0\n");
    assert_eq!(zeroed.command_run_retention_days(), 0);
}

/// A specific key beats a generic fallback on BOTH sides of the split, so a
/// legacy key already on disk is never silently overridden by `default_days`.
#[test]
fn specific_keys_beat_default_days_and_legacy_beats_it_too() {
    let cfg = parse(
        r#"
subagent_session_ttl_days = 30

[retention]
default_days = 90
messages = 7
"#,
    );
    assert_eq!(
        cfg.subagent_session_ttl_days(),
        30,
        "the legacy flat key must beat default_days"
    );
    assert_eq!(
        cfg.message_retention_days(),
        7,
        "an explicit section key must beat default_days"
    );
    assert_eq!(
        cfg.feedback_ledger_retention_days(),
        90,
        "an unnamed row prune inherits default_days"
    );
}

/// `default_days` fills only the row prunes that name nothing — and never the
/// file purge.
#[test]
fn default_days_fills_unnamed_row_prunes_only() {
    let cfg = parse(
        r#"
[retention]
default_days = 30
"#,
    );
    assert_eq!(cfg.subagent_session_ttl_days(), 30);
    assert_eq!(cfg.message_retention_days(), 30);
    assert_eq!(cfg.feedback_ledger_retention_days(), 30);
    assert_eq!(
        cfg.tool_output_retention_days(),
        7,
        "the startup FILE purge must not inherit a row-prune fallback"
    );
}

/// A zero `default_days` means "no fallback", not "prune everything" — the
/// difference between an upgrade that is a no-op and one that deletes a
/// profile's entire message history.
#[test]
fn zero_default_days_is_no_fallback_not_prune_everything() {
    let cfg = parse(
        r#"
[retention]
default_days = 0
"#,
    );
    assert_eq!(cfg.subagent_session_ttl_days(), 7);
    assert_eq!(cfg.message_retention_days(), 0);
    assert_eq!(cfg.feedback_ledger_retention_days(), 0);
    assert_eq!(cfg.tool_output_retention_days(), 7);
}

/// The shipped timing set satisfies its own ordering — the invariant is only
/// worth having if the values the daemon actually runs with pass it.
#[test]
fn default_maintenance_timing_satisfies_its_ordering() {
    let timing = MaintenanceTiming::default();
    assert_eq!(timing.contended_retry(), MAINTENANCE_CONTENDED_RETRY);
    assert_eq!(timing.reclaim_tick(), MEMORY_RECLAIM_TICK);
    assert_eq!(timing.sweep_interval(), SWEEP_INTERVAL);
    assert_eq!(timing.sweep_warmup(), SWEEP_WARMUP);

    // #838: a retry at or above the tick is phase-locked to it.
    assert!(timing.contended_retry() < timing.reclaim_tick());
    // The ticker must tick inside a sweep period.
    assert!(timing.reclaim_tick() < timing.sweep_interval());
    // #840: the sweep reaches the permit first, by construction.
    assert!(timing.sweep_warmup() < timing.reclaim_warmup());
    assert_eq!(
        timing.reclaim_warmup(),
        SWEEP_WARMUP + RECLAIM_WARMUP_OFFSET,
        "the reclaim warmup is the sweep's plus the offset, never a second constant"
    );
}

/// #838: a retry equal to the tick is rejected at construction, because that
/// exact arrangement cost a whole incarnation of housekeeping.
#[test]
#[should_panic(expected = "phase-locked")]
fn timing_rejects_a_retry_that_matches_the_tick() {
    MaintenanceTiming::new(
        MEMORY_RECLAIM_TICK,
        MEMORY_RECLAIM_TICK,
        SWEEP_INTERVAL,
        SWEEP_WARMUP,
        RECLAIM_WARMUP_OFFSET,
    );
}

/// #840: a zero offset restores the simultaneous first wake that let the
/// ticker take the shared permit every boot.
#[test]
#[should_panic(expected = "zero warmup offset")]
fn timing_rejects_a_zero_warmup_offset() {
    MaintenanceTiming::new(
        MAINTENANCE_CONTENDED_RETRY,
        MEMORY_RECLAIM_TICK,
        SWEEP_INTERVAL,
        SWEEP_WARMUP,
        std::time::Duration::ZERO,
    );
}

/// The ticker must tick several times per sweep period, or "retry sooner" has
/// no free window to land in.
#[test]
#[should_panic(expected = "shorter than the sweep interval")]
fn timing_rejects_a_tick_longer_than_the_sweep_interval() {
    MaintenanceTiming::new(
        MAINTENANCE_CONTENDED_RETRY,
        SWEEP_INTERVAL,
        SWEEP_INTERVAL,
        SWEEP_WARMUP,
        RECLAIM_WARMUP_OFFSET,
    );
}

/// #1076: `usage_ledger` retention is NEW, so its historical default is `0`
/// (retain forever) — an upgrade must not start deleting the cost ledger. It
/// resolves on its own key and MAY inherit `default_days`, like the other row
/// prunes; there is no legacy flat key to shadow it.
#[test]
fn usage_ledger_retention_defaults_off_and_honours_its_key() {
    assert_eq!(
        AgentConfig::default().usage_ledger_retention_days(),
        0,
        "an untouched config must keep cost history forever"
    );
    assert_eq!(parse("").usage_ledger_retention_days(), 0);

    let explicit = parse("[retention]\nusage_ledger = 180\n");
    assert_eq!(explicit.usage_ledger_retention_days(), 180);

    let zeroed = parse("[retention]\nusage_ledger = 0\n");
    assert_eq!(
        zeroed.usage_ledger_retention_days(),
        0,
        "an explicit 0 means retain forever, not 'fall through to a default'"
    );

    // It is a ROW prune, so the generic fallback applies when the key is unset.
    let with_default = parse("[retention]\ndefault_days = 30\n");
    assert_eq!(with_default.usage_ledger_retention_days(), 30);

    // ...but an explicit key still beats it.
    let both = parse("[retention]\ndefault_days = 30\nusage_ledger = 365\n");
    assert_eq!(both.usage_ledger_retention_days(), 365);
}

/// #1077: the `tool_success` window is a NARROWER refinement of a window that
/// already has its own fallback, so it resolves to `0` (disabled) when unset
/// and NEVER inherits `default_days` — cascading it would surprise an operator
/// who set only the generic key, silently shortening success history.
#[test]
fn feedback_ledger_success_defaults_off_and_never_inherits_default_days() {
    assert_eq!(
        AgentConfig::default().feedback_ledger_success_retention_days(),
        0,
        "unset must mean disabled, so an upgrade changes no retention"
    );
    assert_eq!(parse("").feedback_ledger_success_retention_days(), 0);

    let explicit = parse("[retention]\nfeedback_ledger_success = 7\n");
    assert_eq!(explicit.feedback_ledger_success_retention_days(), 7);

    let zeroed = parse("[retention]\nfeedback_ledger_success = 0\n");
    assert_eq!(zeroed.feedback_ledger_success_retention_days(), 0);

    let with_default = parse("[retention]\ndefault_days = 90\n");
    assert_eq!(
        with_default.feedback_ledger_success_retention_days(),
        0,
        "default_days governs the row prunes, not this narrower refinement"
    );

    // The uniform window is a SEPARATE key and is unaffected by this one.
    let separate = parse("[retention]\nfeedback_ledger = 90\nfeedback_ledger_success = 7\n");
    assert_eq!(separate.feedback_ledger_retention_days(), 90);
    assert_eq!(separate.feedback_ledger_success_retention_days(), 7);
}
