//! #982: a mermaid fence that fails to RENDER at delivery must reach the model.
//!
//! The preflight (`preflight_parse_errors` → `mermaid_regen_nudge`) covers only
//! what the loop can predict before the reply goes final: a deterministic parse
//! rejection. At delivery a fence can still fail — a transient 4xx, a 429
//! cooldown fallback, a renderer hiccup between the two calls — and
//! `replacement_for` swaps in a failure block and returns. The reader sees a ⚠️
//! block; until #982 the model was told nothing and went on claiming the
//! diagram shipped.
//!
//! `resolve_markdown_media_reporting` carries the renderer's note out of the
//! resolver, the rich senders carry it to the caller, and
//! `delivery::report_rich_plane_sacrifices` is the single place that turns it
//! into ONE synthetic correction turn.
//!
//! These pin the reporting half — the latch accounting and the wording that
//! reaches the model. The transport half (that a failing fence produces a note
//! at all) lives behind the renderer endpoint and is not scriptable from a
//! test, so it is pinned by construction: `resolve_markdown_media_reporting`
//! collects the note from the SAME `MermaidResult` that `replacement_for` turns
//! into the failure block the user sees.

use crate::channels::telegram::TelegramState;
use crate::channels::telegram::delivery::report_rich_plane_sacrifices;
use crate::channels::telegram::rich::SacrificedEntry;
use crate::channels::telegram::rich::mermaid::MediaKind;
use std::sync::Arc;
use uuid::Uuid;

fn note(text: &str) -> Vec<String> {
    vec![text.to_string()]
}

/// AC-2: a successful send whose fence failed buys EXACTLY ONE correction turn,
/// and the nudge it carries names the renderer note and the surface.
#[test]
fn a_failed_fence_buys_exactly_one_correction_turn() {
    let state = Arc::new(TelegramState::new());
    let sid = Uuid::new_v4();
    let notes = note("Parse error on line 2: Lexical error");

    report_rich_plane_sacrifices(&state, sid, &[], &notes, "your reply");

    let queued = state.drain_queued_items(sid);
    assert_eq!(
        queued.len(),
        1,
        "a diagram that shipped as a failure block must reach the model exactly once"
    );
    let nudge = &queued[0].msg.context_text;
    assert!(
        nudge.contains("Parse error on line 2: Lexical error"),
        "the correction must quote the renderer's own note: {nudge}"
    );
    assert!(
        nudge.contains("was NOT delivered as an image"),
        "the correction must state the diagram did not ship: {nudge}"
    );
    assert!(
        nudge.contains("in your reply"),
        "the correction must name the surface: {nudge}"
    );
    assert!(
        queued[0].msg.display_text.contains("diagram"),
        "the history tag must say a diagram was the subject: {:?}",
        queued[0].msg.display_text
    );

    // AC-2: the latch is spent. A second report inside the same exchange must
    // not queue a second synthetic turn — the correction turn itself delivers
    // through this same path, so an unlatched re-entry would chain forever.
    report_rich_plane_sacrifices(&state, sid, &[], &notes, "your reply");
    assert!(
        state.drain_queued_items(sid).is_empty(),
        "the shared post-delivery latch must admit ONE correction per exchange"
    );
}

/// AC-3: a send that degraded no fence queues nothing — and, crucially, does not
/// SPEND the latch, so a genuine failure later in the same exchange still heals.
#[test]
fn a_clean_send_queues_nothing_and_keeps_the_latch() {
    let state = Arc::new(TelegramState::new());
    let sid = Uuid::new_v4();

    report_rich_plane_sacrifices(&state, sid, &[], &[], "your reply");
    assert!(
        state.drain_queued_items(sid).is_empty(),
        "a reply with no failed fence must not nudge"
    );

    // The latch was never touched, so the first real loss still buys a turn.
    report_rich_plane_sacrifices(
        &state,
        sid,
        &[],
        &note("diagram renderer returned HTTP 503"),
        "your reply",
    );
    assert_eq!(
        state.drain_queued_items(sid).len(),
        1,
        "an idle report must not consume the exchange's one correction"
    );
}

/// The mermaid leg is a THIRD leg on the ONE shared latch, ordered after the
/// sacrificed arm: when both happen in one turn the turn still gets a single
/// correction, and the invisible loss (a dropped attachment) is the one it
/// speaks about.
#[test]
fn the_sacrificed_leg_wins_the_single_latch() {
    let state = Arc::new(TelegramState::new());
    let sid = Uuid::new_v4();
    let sacrificed = vec![SacrificedEntry {
        id: "doc0".to_string(),
        name: Some("q3-report.pdf".to_string()),
        kind: MediaKind::Document,
    }];

    report_rich_plane_sacrifices(
        &state,
        sid,
        &sacrificed,
        &note("Parse error on line 2: Lexical error"),
        "your reply",
    );

    let queued = state.drain_queued_items(sid);
    assert_eq!(
        queued.len(),
        1,
        "one turn gets one synthetic correction, not one per rich-plane loss"
    );
    assert!(
        queued[0]
            .msg
            .context_text
            .contains("dropped these attachments"),
        "the sacrificed arm owns the latch when it fires: {:?}",
        queued[0].msg.context_text
    );
}

/// The latch is per session: one session's failed diagram must not silence
/// another's.
#[test]
fn the_mermaid_latch_is_per_session() {
    let state = Arc::new(TelegramState::new());
    let spent = Uuid::new_v4();
    let fresh = Uuid::new_v4();
    let notes = note("Parse error on line 2: Lexical error");

    report_rich_plane_sacrifices(&state, spent, &[], &notes, "your reply");
    report_rich_plane_sacrifices(&state, fresh, &[], &notes, "your reply");

    assert_eq!(state.drain_queued_items(spent).len(), 1);
    assert_eq!(
        state.drain_queued_items(fresh).len(),
        1,
        "a second session must get its own correction turn"
    );
}
