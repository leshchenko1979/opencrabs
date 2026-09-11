//! Layer 3 of the plan-template enforcement design (#155): the 🔍 Review
//! button on the Editing plan card, its isolated rewrite subagent, and the
//! grayed/delta states the card passes through around it.
//!
//! The button exists because a malformed plan body is a *presentation* failure
//! the model can fix if it is told what is wrong — so the design gives the
//! owner a one-tap way to have a fresh, cold-reading agent rewrite the plan
//! rather than discard it. Three properties matter and each is pinned here:
//!
//! 1. The Review button appears on the Editing card and is REPLACED (not
//!    merely disabled) while a review runs, so a second tap cannot start a
//!    concurrent rewrite of the same `.md`.
//! 2. The review worker is spawned with the exact label the spawn path grants
//!    its single write exception on — the two sides must never drift.
//! 3. The result is a one-line delta rendered as the card footer, derived from
//!    the worker's `DELTA:` line, never from the spawn acknowledgement.

use crate::channels::telegram::flow_chrome::PlanKb;
use crate::channels::telegram::plan_card::{
    PLAN_REVIEW_LABEL, PLAN_REVIEW_RUNNING_NOTE, format_plan_review_running_progress,
    plan_card_with_footer, plan_review_agent_id, plan_review_delta, plan_review_effective_kb,
    plan_review_footer_note, plan_review_spawn_input,
};
use uuid::Uuid;

/// The exact rows Telegram would receive, as `(text, callback_data)` pairs.
fn rows(kb: PlanKb) -> Vec<Vec<(String, String)>> {
    let Some(markup) = kb.keyboard() else {
        return Vec::new();
    };
    let value = serde_json::to_value(markup).expect("keyboard must serialize");
    value["inline_keyboard"]
        .as_array()
        .expect("inline_keyboard is an array")
        .iter()
        .map(|row| {
            row.as_array()
                .expect("row is an array")
                .iter()
                .map(|b| {
                    (
                        b["text"].as_str().unwrap_or_default().to_string(),
                        b["callback_data"].as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect()
        })
        .collect()
}

#[test]
fn editing_card_offers_review_approve_discard() {
    let rows = rows(PlanKb::ApproveDiscard);
    assert_eq!(rows.len(), 2, "the Editing card carries two button rows");
    assert_eq!(
        rows[0],
        vec![("✅ Approve plan".to_string(), "plan:ok".to_string())],
        "Row 1 carries Approve plan"
    );
    assert_eq!(
        rows[1],
        vec![
            ("🔍 Review".to_string(), "plan:review".to_string()),
            ("🗑 Discard".to_string(), "plan:no".to_string()),
        ],
        "Row 2 carries Review and Discard"
    );
}

#[test]
fn reviewing_card_disables_approve_and_review_with_noop() {
    let rows = rows(PlanKb::ReviewingApproveDiscard);
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0],
        vec![("⏳ Approve plan".to_string(), "plan:noop".to_string())],
        "Approve plan is disabled with plan:noop while review runs"
    );
    assert_eq!(
        rows[1],
        vec![
            ("⏳ Reviewing…".to_string(), "plan:noop".to_string()),
            ("🗑 Discard".to_string(), "plan:no".to_string()),
        ],
        "while a review runs the Review slot becomes a no-op ack, and Discard stays live"
    );
}

#[test]
fn running_review_disables_approve_and_review_leaves_discard_live() {
    let reviewing = rows(PlanKb::ReviewingApproveDiscard);
    assert_eq!(reviewing[0][0].1, "plan:noop", "Approve is disabled");
    assert_eq!(reviewing[1][0].1, "plan:noop", "Review is disabled");
    assert_eq!(reviewing[1][1].1, "plan:no", "Discard stays live");
}

#[test]
fn effective_keyboard_grays_only_the_editing_card() {
    assert_eq!(
        plan_review_effective_kb(PlanKb::ApproveDiscard, true),
        PlanKb::ReviewingApproveDiscard
    );
    assert_eq!(
        plan_review_effective_kb(PlanKb::ApproveDiscard, false),
        PlanKb::ApproveDiscard
    );
    // Any other card shape is passed through untouched: a review can only be
    // started from the Editing card, so graying anything else would be a
    // rendering lie.
    for kb in [PlanKb::None, PlanKb::DiscardOnly] {
        assert_eq!(plan_review_effective_kb(kb, true), kb);
        assert_eq!(plan_review_effective_kb(kb, false), kb);
    }
}

#[test]
fn footer_explains_a_running_review() {
    let note = plan_review_footer_note(PlanKb::ReviewingApproveDiscard, true, None, None)
        .expect("running review has a footer");
    assert_eq!(note, PLAN_REVIEW_RUNNING_NOTE);
    assert!(
        note.contains("Review"),
        "the footer must name the feature the owner just triggered"
    );
}

#[test]
fn footer_shows_the_last_delta_once_the_review_finishes() {
    let delta = "✨ Review: fixed 2 inline labels".to_string();
    assert_eq!(
        plan_review_footer_note(PlanKb::ApproveDiscard, false, None, Some(delta.clone())),
        Some(delta),
        "a finished review's delta stays on the card until the next one"
    );
}

#[test]
fn no_footer_without_a_review_or_a_delta() {
    assert_eq!(
        plan_review_footer_note(PlanKb::ApproveDiscard, false, None, None),
        None
    );
    // A blank delta is not a delta: rendering an empty footer would add a
    // stray blank line to every card.
    assert_eq!(
        plan_review_footer_note(PlanKb::ApproveDiscard, false, None, Some("   ".to_string())),
        None
    );
    assert_eq!(
        plan_review_footer_note(PlanKb::ApproveDiscard, false, None, Some(String::new())),
        None
    );
}

#[test]
fn running_review_footer_wins_over_a_stale_delta() {
    let note = plan_review_footer_note(
        PlanKb::ReviewingApproveDiscard,
        true,
        None,
        Some("✨ Review: old result".to_string()),
    )
    .expect("a running review always has a footer");
    assert_eq!(note, PLAN_REVIEW_RUNNING_NOTE);
}

#[test]
fn running_review_footer_shows_live_progress_note() {
    let progress_note = "🔍 Review subagent running (turn 3 · read_file)…".to_string();
    let note = plan_review_footer_note(
        PlanKb::ReviewingApproveDiscard,
        true,
        Some(progress_note.clone()),
        Some("✨ Review: old result".to_string()),
    )
    .expect("a running review with progress note renders that note");
    assert_eq!(note, progress_note);
}

#[test]
fn format_progress_snapshot_handles_various_states() {
    assert_eq!(
        format_plan_review_running_progress(None),
        PLAN_REVIEW_RUNNING_NOTE
    );

    let p_zero = crate::brain::tools::subagent::status::ProgressSnapshot {
        iteration: 0,
        last_tool: Some("read_file".to_string()),
        last_event: None,
        updated_at: None,
    };
    assert_eq!(
        format_plan_review_running_progress(Some(&p_zero)),
        PLAN_REVIEW_RUNNING_NOTE
    );

    let p_tool = crate::brain::tools::subagent::status::ProgressSnapshot {
        iteration: 4,
        last_tool: Some("read_file".to_string()),
        last_event: None,
        updated_at: None,
    };
    assert_eq!(
        format_plan_review_running_progress(Some(&p_tool)),
        "🔍 Review subagent running (turn 4 · read_file)…"
    );

    let p_notool = crate::brain::tools::subagent::status::ProgressSnapshot {
        iteration: 2,
        last_tool: None,
        last_event: None,
        updated_at: None,
    };
    assert_eq!(
        format_plan_review_running_progress(Some(&p_notool)),
        "🔍 Review subagent running (turn 2)…"
    );
}

#[test]
fn footer_never_bleeds_onto_a_non_editing_card() {
    // One renderer draws every plan card shape. A delta or a "reviewing…" note
    // on the Active checklist card (or on no card at all) would describe a
    // plan state that no longer exists — the same stale-chrome family the
    // goal-scoping filter fixed.
    for kb in [PlanKb::None, PlanKb::DiscardOnly] {
        assert_eq!(
            plan_review_footer_note(kb, false, None, Some("✨ Review: stale".to_string())),
            None,
            "a finished review's delta must not outlive the Editing card"
        );
        assert_eq!(
            plan_review_footer_note(kb, true, None, None),
            None,
            "a running review cannot be showing on a card that offers no \
             Review button"
        );
    }
}

#[test]
fn footer_is_appended_to_the_body_it_decorates() {
    let body = "✍️ **Editing plan**".to_string();
    let with = plan_card_with_footer(body.clone(), Some("✨ Review: done"));
    assert!(
        with.starts_with(&body),
        "the card body is preserved verbatim"
    );
    assert!(with.ends_with("✨ Review: done"));
    assert!(
        with.contains("\n\n"),
        "the footer is separated from the body, never glued onto the last line"
    );
    // No footer, or a blank one, leaves the body byte-identical — which is
    // what keeps the card's change-signature stable when nothing changed.
    assert_eq!(plan_card_with_footer(body.clone(), None), body);
    assert_eq!(plan_card_with_footer(body.clone(), Some("  ")), body);
    assert_eq!(plan_card_with_footer(body.clone(), Some("")), body);
}

#[test]
fn review_worker_is_spawned_under_the_label_the_write_grant_keys_on() {
    let session_id = Uuid::new_v4();
    let session_str = session_id.to_string();
    let input = plan_review_spawn_input(session_id, "review the plan".to_string());

    assert_eq!(
        input["label"].as_str(),
        Some(PLAN_REVIEW_LABEL),
        "the spawn path grants its single Editing-parent write exception on \
         this exact label; drift here silently strips the worker's only job"
    );
    assert_eq!(
        PLAN_REVIEW_LABEL,
        crate::brain::tools::subagent::PLAN_REVIEW_LABEL,
        "the card and the spawn grant must read one constant, not two copies"
    );
    assert_eq!(
        input["plan_session"].as_str(),
        Some(session_str.as_str()),
        "the worker rewrites the PARENT's plan, so its plan state resolves there"
    );
    assert_eq!(
        input["read_only"].as_bool(),
        Some(false),
        "rewriting the plan `.md` is the worker's entire purpose"
    );
    assert_eq!(input["prompt"].as_str(), Some("review the plan"));
}

#[test]
fn review_worker_id_is_read_from_the_spawn_acknowledgement() {
    let ack = "Spawned sub-agent 'plan-review' with id: abc-123\n\
               Session: 00000000-0000-0000-0000-000000000000\n\
               Access: read-write\n\
               Brain: none\n\
               Prompt: review the plan";
    assert_eq!(plan_review_agent_id(ack).as_deref(), Some("abc-123"));
}

#[test]
fn a_spawn_output_without_an_id_yields_no_worker_id() {
    // The card must not fabricate an id: a missing id means the poll cannot
    // start, and the caller reports that rather than hanging grayed forever.
    assert_eq!(plan_review_agent_id("spawn failed: no manager"), None);
    assert_eq!(plan_review_agent_id("with id: "), None);
    assert_eq!(plan_review_agent_id(""), None);
}

#[test]
fn delta_comes_from_the_workers_marked_summary_line() {
    let report = "I read the plan and rewrote it.\n\
                  Fixed two labels that sat on their own line.\n\
                  \n\
                  DELTA: moved 2 labels inline; no task text changed";
    assert_eq!(
        plan_review_delta(Some(report)),
        "✨ Review: moved 2 labels inline; no task text changed"
    );
}

#[test]
fn structured_report_parses_summary_open_questions_and_delta() {
    let report = "### Analysis\n\
                  Verified codebase ground truth.\n\
                  \n\
                  SUMMARY:\n\
                  - Verified src/channels/telegram/flow_chrome.rs carries PlanKb.\n\
                  - Resolved concurrency lock in TelegramState.\n\
                  \n\
                  OPEN_QUESTIONS:\n\
                  1. Should we support custom timeouts for long review runs?\n\
                  2. Do we need a dedicated log topic for review audits?\n\
                  \n\
                  DELTA: Hardened concurrency locks and added 2-row layout";

    let parsed = crate::channels::telegram::plan_card::parse_plan_review_report(Some(report));
    assert_eq!(
        parsed.card_delta,
        "✨ Review: Hardened concurrency locks and added 2-row layout"
    );
    assert_eq!(parsed.open_questions.len(), 2);
    assert_eq!(
        parsed.open_questions[0],
        "Should we support custom timeouts for long review runs?"
    );
    assert_eq!(
        parsed.open_questions[1],
        "Do we need a dedicated log topic for review audits?"
    );
    assert!(parsed.full_summary.is_some());
    assert!(
        parsed
            .full_summary
            .unwrap()
            .contains("Verified src/channels/telegram/flow_chrome.rs")
    );
}

#[test]
fn delta_is_the_last_marked_line_when_several_are_present() {
    let report = "DELTA: first guess\n\
                  some analysis\n\
                  DELTA: the real summary";
    assert_eq!(
        plan_review_delta(Some(report)),
        "✨ Review: the real summary"
    );
}

#[test]
fn delta_never_echoes_the_spawn_acknowledgement() {
    // The defect this pins: taking the last line of the spawn ack made the
    // card read "✨ Review: Prompt: You are an automated plan structure…".
    let ack = "Spawned sub-agent 'plan-review' with id: abc-123\n\
               Session: 00000000-0000-0000-0000-000000000000\n\
               Access: read-write\n\
               Brain: none\n\
               Prompt: You are an automated plan structure reviewer.";
    let delta = plan_review_delta(Some(ack));
    assert!(
        !delta.contains("Spawned sub-agent"),
        "the spawn acknowledgement is not a review result"
    );
    assert!(
        !delta.contains("Prompt:"),
        "the worker's own prompt must never be shown as its finding"
    );
}

#[test]
fn missing_or_unmarked_report_still_produces_a_visible_delta() {
    assert_eq!(
        plan_review_delta(None),
        "✨ Review finished but returned no report."
    );
    let unmarked = "I checked the plan and it was already fine.";
    let delta = plan_review_delta(Some(unmarked));
    assert!(delta.starts_with("✨ Review"), "the footer always speaks");
    assert!(
        !delta.contains(unmarked),
        "an unmarked report is summarised, not pasted whole into the card"
    );
}

#[test]
fn a_long_delta_is_truncated_to_fit_the_card() {
    let long = "x".repeat(500);
    let report = format!("DELTA: {long}");
    let delta = plan_review_delta(Some(&report));
    assert!(
        delta.chars().count() < 500,
        "the footer is one line on a phone screen, not a paragraph"
    );
    assert!(
        delta.ends_with('…') || delta.ends_with("..."),
        "a truncated delta must show that it was truncated"
    );
}
