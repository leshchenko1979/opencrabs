//! Compaction signal (#29): unit tests for the flow-line builders, the
//! pinned-header const, the ETA predictor, and the #444 system-provenance
//! contract (a ⏳ compaction banner is a SYSTEM line, never narration).
//! All pure — no agent, no mocks, no locks.

use std::time::Duration;

use crate::brain::agent::service::nudge::{in_pressure_warning_band, should_emit_pressure_warning};
use crate::channels::telegram::flow::{
    COMPACTING_HEADER_TEXT, FlowHeader, FlowLine, HeaderMarkup, compacted_flow_line,
    compacting_flow_line, flow_header_text, render_flow_html_chrome_pref, render_flow_rich,
    starts_with_icon,
};
use crate::channels::telegram::flow_chrome::{
    FlowSections, FooterParts, TelemetryMetrics, merged_footer, standalone_telemetry_line,
};
use crate::utils::plan_files::PlanModeState;

#[test]
fn compacting_line_without_prediction() {
    assert_eq!(compacting_flow_line(68.0, None), "⏳ compact: 68% 🧠");
}

#[test]
fn compacting_line_shows_observed_eta() {
    assert_eq!(
        compacting_flow_line(68.0, Some(Duration::from_secs(42))),
        "⏳ compact: 68% 🧠"
    );
}

#[test]
fn compacting_line_humanizes_minute_eta() {
    assert_eq!(
        compacting_flow_line(71.0, Some(Duration::from_secs(132))),
        "⏳ compact: 71% 🧠"
    );
}

#[test]
fn compacting_line_rounds_fill_level() {
    assert!(compacting_flow_line(67.7, None).contains("68% 🧠"));
    assert!(compacting_flow_line(99.4, None).contains("99% 🧠"));
}

#[test]
fn system_banner_stays_out_of_the_footer_preview() {
    // Provenance contract (#444): the ⏳ compaction banner is a SYSTEM line, so
    // `latest_activity_preview` / `latest_intermediary_thought` skip it and the
    // footer can never echo the banner back beside the pinned header — with the
    // compacting flag ON *or* OFF. Before #444 the banner was a `Text` line, so
    // the flag was the only thing holding the duplicate back and the idle
    // render showed it twice.
    let lines = [FlowLine::System(
        "⏳ Compacting context — 66% full…".to_string(),
    )];
    // Flag on (the silent compaction window): one banner, in the body log.
    let compacting = render_flow_html_chrome_pref(
        &lines,
        &FlowHeader::Live(Some(COMPACTING_HEADER_TEXT)),
        None,
        &FlowSections::default(),
        usize::MAX,
        5,
        None,
        true,
        None,
    );
    assert_eq!(
        compacting.matches("Compacting context").count(),
        1,
        "system banner is never echoed by the footer preview: {compacting:?}"
    );
    // Flag off (compaction done, header still pinned): STILL one banner —
    // provenance, not the flag, is what keeps the duplicate out.
    let idle = render_flow_html_chrome_pref(
        &lines,
        &FlowHeader::Live(Some("⚙️")),
        None,
        &FlowSections::default(),
        usize::MAX,
        5,
        None,
        false,
        None,
    );
    assert_eq!(
        idle.matches("Compacting context").count(),
        1,
        "the flag is no longer load-bearing; provenance alone suppresses it: {idle:?}"
    );
}

#[test]
fn system_banner_stays_out_of_the_rich_header_preview() {
    let lines = [FlowLine::System(
        "⏳ Compacting context — 66% full…".to_string(),
    )];
    // Flag on: the HEADER (first line) carries the pinned compaction string
    // exactly once — the activity preview is suppressed. The body keeps the
    // ⏳ START line (a real log entry, the fill-level carrier), so the count
    // is asserted on the header line, never the whole message.
    let rich = render_flow_rich(&lines, Some(COMPACTING_HEADER_TEXT), true);
    let header = rich.lines().next().unwrap_or_default();
    assert_eq!(
        header.matches("Compacting context").count(),
        1,
        "rich header dedupes too: {rich:?}"
    );
    assert!(
        rich.contains("⏳ Compacting context — 66% full…"),
        "body keeps the START line: {rich:?}"
    );
    // Flag off: STILL one — the SYSTEM line is skipped by the header preview on
    // provenance alone (#444), so the flag no longer decides this. Before #444
    // the banner was a `Text` line and the idle header showed it twice.
    let idle = render_flow_rich(&lines, Some(COMPACTING_HEADER_TEXT), false);
    let idle_header = idle.lines().next().unwrap_or_default();
    assert_eq!(
        idle_header.matches("Compacting context").count(),
        1,
        "system provenance, not the flag, suppresses the duplicate: {idle:?}"
    );
}

#[test]
fn compacted_line_under_a_minute() {
    assert_eq!(
        compacted_flow_line(68.0, 26.0, 132_000, 51_000, Duration::from_secs(42)),
        "🧹 compact: 68% → 26% 🧠"
    );
}

#[test]
fn compacted_line_multi_minute() {
    assert_eq!(
        compacted_flow_line(71.0, 24.0, 94_559, 34_197, Duration::from_secs(132)),
        "🧹 compact: 71% → 24% 🧠"
    );
}

#[test]
fn compacted_line_floors_subsecond_elapsed_to_1s() {
    // A sub-second summarizer call still reads as a real duration — "0s"
    // would look like the line was printed before the work happened.
    assert_eq!(
        compacted_flow_line(66.0, 30.0, 90_000, 40_000, Duration::from_millis(300)),
        "🧹 compact: 66% → 30% 🧠"
    );
}

#[test]
fn header_const_is_number_free() {
    // Design invariant (#29): the pinned header NEVER carries a number —
    // compaction progress is unknowable, so any digit reads as a fake
    // progress bar. The fill level lives on the START body line instead.
    assert_eq!(COMPACTING_HEADER_TEXT, "⏳ Compacting context…");
    assert!(!COMPACTING_HEADER_TEXT.contains('%'));
    assert!(!COMPACTING_HEADER_TEXT.chars().any(|c| c.is_ascii_digit()));
}

#[test]
fn pressure_band_boundaries() {
    // [55, 65) — the ceiling is exclusive: AT 65% compaction itself fires,
    // the nudge is for the approach.
    assert!(!in_pressure_warning_band(54.9));
    assert!(in_pressure_warning_band(55.0));
    assert!(in_pressure_warning_band(64.9));
    assert!(!in_pressure_warning_band(65.0));
}

#[test]
fn pressure_warning_once_per_entry() {
    // In-band, not yet emitted → warn; already emitted → silence until the
    // flag re-arms below the floor; below the floor → never warn.
    assert!(should_emit_pressure_warning(60.0, false).is_some());
    assert!(should_emit_pressure_warning(60.0, true).is_none());
    assert!(should_emit_pressure_warning(40.0, false).is_none());
}

// ── gear-strip (#29 fix round, owner directive): the standing ⚙️ chrome
// prefix is dropped whenever another icon follows it ──

#[test]
fn starts_with_icon_classification() {
    // Icon glyphs drop the standing gear; word text (Latin, Cyrillic) keeps it.
    assert!(starts_with_icon("⏳ Compacting context…"));
    assert!(starts_with_icon("✅ bash git status"));
    assert!(starts_with_icon("❌ grep pattern"));
    assert!(!starts_with_icon("bash gh pr list"));
    assert!(!starts_with_icon("Reading the handler."));
    assert!(!starts_with_icon("Чтение лога"));
    assert!(!starts_with_icon(""));
}

#[test]
fn live_header_drops_gear_before_icon_status() {
    // The pinned compaction status leads with ⏳ → the header renders bare.
    assert_eq!(
        flow_header_text(
            11,
            &FlowHeader::Live(Some("1:05")),
            Some(COMPACTING_HEADER_TEXT),
            HeaderMarkup::Html
        ),
        "<b>⏳ Compacting context…</b> • <i>11 tool calls</i> • <i>1:05</i>"
    );
    // Plain status keeps the tool pick (#509 shape).
    assert_eq!(
        flow_header_text(
            3,
            &FlowHeader::Live(Some("0:12")),
            Some("Reading logs"),
            HeaderMarkup::Html
        ),
        "⛏️ <b>Reading logs</b> • <i>3 tool calls</i> • <i>0:12</i>"
    );
}
#[test]
fn live_footer_drops_gear_before_icon_activity() {
    // In-flight activity strips tool outcome glyphs (✅/❌) so unsettled turns
    // maintain the running ⛏ pick, while system pins like ⏳ remain bare (#1052).
    let tool_done = merged_footer(
        &FooterParts {
            outcome: None,
            compacting: false,
            plan_mode: None,
            working_on: None,
            thought: None,
            activity: Some("✅ bash git status"),
            tool_count: 1,
            has_log: true,
            ctx: Some("ctx: 8K/200K 4%"),
            elapsed_secs: 0,
            bg: None,
            has_goal: false,
        },
        HeaderMarkup::Markdown,
    );
    assert_eq!(tool_done, "⚙️ • 0:00 ⏱️ • 4% 🧠");
    let icon = merged_footer(
        &FooterParts {
            outcome: None,
            compacting: false,
            plan_mode: None,
            working_on: None,
            thought: None,
            activity: Some("⏳ Compacting context…"),
            tool_count: 1,
            has_log: true,
            ctx: None,
            elapsed_secs: 0,
            bg: None,
            has_goal: false,
        },
        HeaderMarkup::Markdown,
    );
    assert_eq!(icon, "⚙️ • 0:00 ⏱️");
    let plain = merged_footer(
        &FooterParts {
            outcome: None,
            compacting: false,
            plan_mode: None,
            working_on: None,
            thought: None,
            activity: Some("Reading the handler."),
            tool_count: 2,
            has_log: true,
            ctx: None,
            elapsed_secs: 0,
            bg: None,
            has_goal: false,
        },
        HeaderMarkup::Markdown,
    );
    assert_eq!(plain, "⚙️ • 0:00 ⏱️");
    let plan_edit = merged_footer(
        &FooterParts {
            outcome: None,
            compacting: false,
            plan_mode: Some(PlanModeState::PostInitEditing),
            working_on: None,
            thought: None,
            activity: None,
            tool_count: 0,
            has_log: false,
            ctx: Some("ctx: 10K/200K 5%"),
            elapsed_secs: 30,
            bg: None,
            has_goal: false,
        },
        HeaderMarkup::Markdown,
    );
    assert_eq!(plan_edit, "✍️ • 0:30 ⏱️ • 5% 🧠");
    let finished = merged_footer(
        &FooterParts {
            outcome: Some(("✅", "Finished")),
            compacting: false,
            plan_mode: None,
            working_on: None,
            thought: None,
            activity: None,
            tool_count: 5,
            has_log: true,
            ctx: Some("ctx: 20K/200K 10%"),
            elapsed_secs: 75,
            bg: None,
            has_goal: false,
        },
        HeaderMarkup::Markdown,
    );
    assert_eq!(finished, "✅ • 1:15 ⏱️ • 10% 🧠");
}

#[test]
fn icon_led_segment_retires_the_bare_cog_fallback() {
    // With an icon-led pin as the only narration (activity suppressed by the
    // compaction dedupe, zero tool calls), state icon + clock leads followed by
    // the icon. #398: the pin and the flag travel together — `compacting: true`
    // is what puts ⏳ in segment 1, so it agrees with the header instead of
    // falling through to the generic ⚙️.
    let out = merged_footer(
        &FooterParts {
            outcome: None,
            compacting: true,
            plan_mode: None,
            working_on: Some(COMPACTING_HEADER_TEXT),
            thought: None,
            activity: None,
            tool_count: 0,
            has_log: true,
            ctx: None,
            elapsed_secs: 65,
            bg: None,
            has_goal: false,
        },
        HeaderMarkup::Markdown,
    );
    assert_eq!(out, "⏳ • 1:05 ⏱️");
}

/// #398: while compacting, the live burst outranks a standing plan stage — the
/// card must not say `✍️ Editing` under an `⏳ Compacting context…` header.
/// Fails on the pre-fix tree: there the plan-state sniff alone drives segment 1.
#[test]
fn compacting_icon_beats_the_editing_pen() {
    let parts = FooterParts {
        outcome: None,
        compacting: true,
        plan_mode: Some(PlanModeState::PostInitEditing),
        elapsed_secs: 30,
        ..Default::default()
    };
    let telem = TelemetryMetrics {
        tool_count: 0,
        elapsed_secs: 30,
        detached_tasks: 0,
        subagents: 0,
        queued_messages: 0,
    };
    let line = standalone_telemetry_line(&parts, Some(&telem), HeaderMarkup::Markdown);
    assert_eq!(line, "⏳ • 0:30 ⏱️");
    assert!(!line.contains("✍️"), "editing pen must not win while compacting");
}

/// #398: a settled card is terminal text, so the outcome icon outranks a
/// still-set `compacting` flag (the settle path clears it anyway — ordering
/// outcome first makes the ladder safe under both readings).
#[test]
fn settled_icon_outranks_a_stale_compacting_flag() {
    let out = merged_footer(
        &FooterParts {
            outcome: Some(("✅", "Done")),
            compacting: true,
            elapsed_secs: 0,
            ..Default::default()
        },
        HeaderMarkup::Markdown,
    );
    assert_eq!(out, "✅ • 0:00 ⏱️");
}

/// #398: the no-metrics path (`merged_footer`, `telemetry = None`) carries the
/// same compacting icon — the fix must not live only on the telemetry branch.
#[test]
fn fallback_path_carries_the_compacting_icon() {
    let out = merged_footer(
        &FooterParts {
            outcome: None,
            compacting: true,
            elapsed_secs: 0,
            ..Default::default()
        },
        HeaderMarkup::Markdown,
    );
    assert_eq!(out, "⏳ • 0:00 ⏱️");
}

/// #398 regression fence: the compacting TEXT is not the signal — the FLAG is.
/// Same narration as the fence test above, flag false, so the gear must stay.
/// Fails if anyone re-implements the fix by sniffing the display string.
#[test]
fn stale_compacting_text_without_the_flag_still_shows_the_gear() {
    let out = merged_footer(
        &FooterParts {
            outcome: None,
            compacting: false,
            working_on: Some(COMPACTING_HEADER_TEXT),
            elapsed_secs: 65,
            ..Default::default()
        },
        HeaderMarkup::Markdown,
    );
    assert_eq!(out, "⚙️ • 1:05 ⏱️");
}

// ── #399: the plan stage in segment 1, typed rather than sniffed ─────────

/// #399: an Active plan OUTSIDE the seed window carries no label at all, which
/// is exactly why it used to render the generic cog and read as "no plan". The
/// typed field carries the stage regardless, so the clipboard shows. Fails on
/// the pre-fix tree: there the sniff finds no label and falls through to the cog.
#[test]
fn active_plan_outside_the_seed_window_shows_the_clipboard() {
    let out = merged_footer(
        &FooterParts {
            plan_mode: Some(PlanModeState::Active),
            elapsed_secs: 65,
            ..Default::default()
        },
        HeaderMarkup::Markdown,
    );
    assert_eq!(out, "📋 • 1:05 ⏱️");
    assert!(
        !out.contains("⚙️"),
        "a live checklist must not read as no plan at all: {out}"
    );
}

/// #399: the owner's overlap rule — a plan awaiting approval already HAS its
/// tasks (`PostInitEditing` is the mode a pending-approval checklist derives
/// to), and the hand outranks the clipboard beside it. Names the enum rather
/// than a rendered string, because the ladder is the unit under test; the
/// derivation has its own `plan_files` coverage.
#[test]
fn editing_plan_outranks_a_checklist_in_the_same_plan() {
    let out = merged_footer(
        &FooterParts {
            plan_mode: Some(PlanModeState::PostInitEditing),
            elapsed_secs: 65,
            ..Default::default()
        },
        HeaderMarkup::Markdown,
    );
    assert_eq!(out, "✍️ • 1:05 ⏱️");
    assert!(
        !out.contains("📋"),
        "the hand outranks the standing checklist: {out}"
    );
}

/// #399 negative control: the clipboard must never leak onto a card with no
/// plan at all — `None` is the one state that keeps the generic cog.
#[test]
fn no_plan_keeps_the_generic_cog() {
    let out = merged_footer(
        &FooterParts {
            plan_mode: None,
            elapsed_secs: 65,
            ..Default::default()
        },
        HeaderMarkup::Markdown,
    );
    assert_eq!(out, "⚙️ • 1:05 ⏱️");
}

/// #399: rung 1 is terminal. A settled card shows its outcome even while an
/// Active plan is live, so the new rung cannot resurrect a stage icon once the
/// turn has ended.
#[test]
fn settled_card_never_shows_a_plan_stage_icon() {
    let out = merged_footer(
        &FooterParts {
            outcome: Some(("✅", "Done")),
            plan_mode: Some(PlanModeState::Active),
            elapsed_secs: 65,
            ..Default::default()
        },
        HeaderMarkup::Markdown,
    );
    assert_eq!(out, "✅ • 1:05 ⏱️");
}

/// #399: the ladder is ONE expression behind both render paths, so the
/// no-metrics path and the metrics path must agree on the new rung. A rung
/// added to only one of them is the drift this fences.
#[test]
fn fallback_path_agrees_with_the_metrics_path() {
    let parts = FooterParts {
        plan_mode: Some(PlanModeState::Active),
        elapsed_secs: 65,
        ctx: Some("ctx: 12K/200K 6%"),
        ..Default::default()
    };
    let no_metrics = merged_footer(&parts, HeaderMarkup::Markdown);
    let metrics = standalone_telemetry_line(
        &parts,
        Some(&TelemetryMetrics {
            tool_count: 0,
            elapsed_secs: 65,
            detached_tasks: 0,
            subagents: 0,
            queued_messages: 0,
        }),
        HeaderMarkup::Markdown,
    );
    assert!(
        no_metrics.starts_with("📋 • "),
        "no-metrics path lost the rung: {no_metrics}"
    );
    assert!(
        metrics.starts_with("📋 • "),
        "metrics path lost the rung: {metrics}"
    );
}
