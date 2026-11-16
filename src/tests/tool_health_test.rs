//! #1706 regression tests: tool health annotations built from feedback
//! ledger stats, and the ledger round-trip that feeds them. The pure
//! builder (`build_tool_health_annotations`) is the contract the schema
//! path depends on: flag a tool only when it has at least
//! `TOOL_HEALTH_MIN_SAMPLES` attempts in the window AND a success rate
//! below `TOOL_HEALTH_MAX_RATE`; never flag sentinel dimensions.

use crate::brain::agent::service::feedback::{
    TOOL_HEALTH_MAX_RATE, TOOL_HEALTH_MIN_SAMPLES, build_tool_health_annotations,
    tool_health_enabled,
};
use crate::config::Config;
use crate::db::database::Database;
use crate::db::repository::FeedbackLedgerRepository;
use crate::db::repository::feedback_ledger::DimensionStats;

fn stat(dimension: &str, total: i64, successes: i64) -> DimensionStats {
    DimensionStats {
        dimension: dimension.to_string(),
        total_events: total,
        successes,
        failures: total - successes,
        success_rate: if total > 0 {
            successes as f64 / total as f64
        } else {
            0.0
        },
        avg_value: 0.0,
    }
}

#[test]
fn zero_success_tool_gets_broken_warning() {
    let stats = vec![stat("codegraph_explore", 6, 0)];
    let map = build_tool_health_annotations(&stats);
    let warning = map.get("codegraph_explore").expect("dead tool flagged");
    assert!(
        warning.contains("0/6"),
        "warning states the numbers: {warning}"
    );
    assert!(
        warning.contains("appears broken"),
        "zero successes say broken: {warning}"
    );
    assert!(
        warning.contains("prefer an alternative"),
        "actionable: {warning}"
    );
}

#[test]
fn partial_success_tool_gets_verify_warning() {
    let stats = vec![stat("flaky_tool", 9, 2)];
    let map = build_tool_health_annotations(&stats);
    let warning = map.get("flaky_tool").expect("under-half tool flagged");
    assert!(
        warning.contains("only 2/9"),
        "warning states the numbers: {warning}"
    );
    assert!(
        warning.contains("verify"),
        "partial success says verify, not broken: {warning}"
    );
}

#[test]
fn healthy_tool_not_flagged() {
    let stats = vec![stat("bash", 20, 18)];
    let map = build_tool_health_annotations(&stats);
    assert!(map.is_empty(), "healthy tool must not be flagged: {map:?}");
}

#[test]
fn below_min_samples_not_flagged() {
    // 0/3 is a 0% rate but 3 samples is noise, not evidence. Isolates the
    // floor: the rate condition alone would let this through to flagging.
    let stats = vec![stat("once_used_tool", 3, 0)];
    let map = build_tool_health_annotations(&stats);
    assert!(
        map.is_empty(),
        "below {TOOL_HEALTH_MIN_SAMPLES} attempts must not flag: {map:?}"
    );
}

#[test]
fn exactly_at_max_rate_not_flagged() {
    // rate == TOOL_HEALTH_MAX_RATE is the boundary: only strictly-below flags.
    let stats = vec![stat("boundary_tool", 10, 5)];
    assert_eq!(stats[0].success_rate, TOOL_HEALTH_MAX_RATE);
    let map = build_tool_health_annotations(&stats);
    assert!(map.is_empty(), "boundary rate must not flag: {map:?}");
}

#[test]
fn sentinel_dimensions_never_flagged() {
    let stats = vec![stat("phantom_tool_call", 20, 0), stat("", 12, 0)];
    let map = build_tool_health_annotations(&stats);
    assert!(
        map.is_empty(),
        "diagnostic dimensions are not tools: {map:?}"
    );
}

#[test]
fn mixed_set_flags_only_dead_tools() {
    let stats = vec![
        stat("codegraph_explore", 6, 0),   // dead: flagged
        stat("camofox_create_tab", 14, 0), // dead: flagged
        stat("bash", 50, 48),              // healthy
        stat("exa_search", 3, 0),          // too few samples
        stat("phantom_tool_call", 9, 0),   // sentinel
    ];
    let map = build_tool_health_annotations(&stats);
    assert_eq!(map.len(), 2, "exactly the two dead tools: {map:?}");
    assert!(map.contains_key("codegraph_explore"));
    assert!(map.contains_key("camofox_create_tab"));
}

/// Full data-path round trip: failures recorded through the real ledger
/// come back as an annotation. Uses the same in-memory DB fixture as the
/// rsi tests.
#[tokio::test]
async fn ledger_round_trip_feeds_annotations() {
    let db = Database::connect_in_memory().await.expect("in-memory DB");
    db.run_migrations().await.expect("migrations");
    let repo = FeedbackLedgerRepository::new(db.pool().clone());

    // The #1706 evidence profile: a dead tool failing every attempt.
    for _ in 0..6 {
        repo.record("sess", "tool_failure", "codegraph_explore", 0.0, None)
            .await
            .expect("record failure");
    }
    // A healthy tool in the same window must stay unflagged.
    for _ in 0..8 {
        repo.record("sess", "tool_success", "bash", 1.0, None)
            .await
            .expect("record success");
    }

    let window_since = (chrono::Utc::now() - chrono::Duration::days(7))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let stats = repo
        .stats_by_dimension_since("tool_", Some(&window_since))
        .await
        .expect("stats query");

    let map = build_tool_health_annotations(&stats);
    let warning = map.get("codegraph_explore").expect("dead tool flagged");
    assert!(warning.contains("0/6"));
    assert!(!map.contains_key("bash"), "healthy tool unflagged: {map:?}");
}

// ---------------------------------------------------------------------------
// #860: the path is gated on the RSI master switch.
//
// The annotations have one consumer (the schema description suffix) and no
// user surface, and building them costs a full `tool_%` ledger scan. When RSI
// is off there is nothing to annotate FOR, so the path must not run at all.
// ---------------------------------------------------------------------------

fn config_with_agent(toml_fragment: &str) -> Config {
    toml::from_str(&format!("[agent]\n{toml_fragment}")).expect("agent config must parse")
}

#[test]
fn gate_is_closed_for_a_headless_daemon_without_an_explicit_key() {
    // The shipped state of this install: `rsi_enabled` absent, daemon runs
    // headless. This is the case that was paying 923,089 scanned rows every
    // five minutes to produce an empty map.
    let cfg = config_with_agent("");
    assert!(
        !tool_health_enabled(&cfg, true),
        "headless + absent key must gate the tool-health path off, same as RSI (#1063)"
    );
}

#[test]
fn gate_is_open_for_an_interactive_install() {
    // The TUI default keeps the feature users see. Nothing about #860 removes
    // the warning from interactive installs.
    let cfg = config_with_agent("");
    assert!(
        tool_health_enabled(&cfg, false),
        "interactive + absent key keeps annotations on"
    );
}

#[test]
fn explicit_key_wins_over_run_mode_in_both_directions() {
    // A knowing opt-in must arm the path on a daemon; an explicit opt-out must
    // disarm it on the TUI. If these ever diverge from RSI's resolution the
    // two features stop agreeing about what "RSI is off" means.
    let on = config_with_agent("rsi_enabled = true");
    assert!(
        tool_health_enabled(&on, true),
        "explicit rsi_enabled = true arms tool health on a headless daemon"
    );
    let off = config_with_agent("rsi_enabled = false");
    assert!(
        !tool_health_enabled(&off, false),
        "explicit rsi_enabled = false disarms tool health on the TUI"
    );
}

#[test]
fn gate_tracks_rsi_in_every_combination() {
    // Belt and braces: the gate is a SHARED switch, not a parallel one, so it
    // must agree with `rsi_effectively_enabled` for every (config, mode) pair.
    // A future divergence is a silent product change, not a refactor.
    use crate::brain::rsi::rsi_effectively_enabled;
    for fragment in ["", "rsi_enabled = true", "rsi_enabled = false"] {
        let cfg = config_with_agent(fragment);
        for headless in [true, false] {
            assert_eq!(
                tool_health_enabled(&cfg, headless),
                rsi_effectively_enabled(&cfg, headless),
                "gate must equal the RSI master switch for {fragment:?} headless={headless}"
            );
        }
    }
}

/// The gate must be the FIRST thing the accessor does.
///
/// `AgentService::tool_health_annotations` needs a live service and pool, so no
/// unit test drives it. The property #860 is about — the scan is never reached
/// while RSI is off — is therefore pinned at the source, the same way
/// `pending_resume_no_reinsert_test` pins its boot call sites. Comment lines are
/// filtered so prose naming the gate cannot trip the guard.
#[test]
fn annotations_accessor_consults_the_gate_before_the_scan() {
    const FEEDBACK_SRC: &str = include_str!("../brain/agent/service/feedback.rs");
    let code: Vec<&str> = FEEDBACK_SRC
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect();

    let gate = code
        .iter()
        .position(|line| line.contains("if !tool_health_enabled("))
        .expect(
            "tool_health_annotations must gate on tool_health_enabled; without it a \
             headless daemon keeps paying a full tool_% scan every refresh interval (#860)",
        );
    let scan = code
        .iter()
        .position(|line| line.contains("stats_by_dimension_since(\"tool_\""))
        .expect("the ledger scan call must still be present");

    assert!(
        gate < scan,
        "the RSI gate (line {}) must precede the ledger scan (line {}): an early \
         return after the spawn would gate nothing (#860)",
        gate + 1,
        scan + 1
    );
}
