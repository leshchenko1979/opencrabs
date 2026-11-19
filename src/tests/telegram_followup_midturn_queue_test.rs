use std::sync::Arc;
use uuid::Uuid;

use crate::channels::telegram::TelegramState;
use crate::channels::telegram::handler::{build_midturn_queued_message, mark_voice_note};
use crate::channels::telegram::state::QueuedOrigin;

#[test]
fn followup_tap_during_active_turn_enqueues_reaction_and_drains_fifo() {
    let state = Arc::new(TelegramState::new());
    let sid = Uuid::new_v4();

    // 1. Session begins an active turn
    let _guard = state
        .try_begin_turn(sid)
        .expect("initial claim must succeed");

    // 2. Active turn guard denies concurrent turn start
    assert!(
        state.try_begin_turn(sid).is_none(),
        "mid-turn must return None"
    );

    // 3. User taps a follow-up suggestion button mid-turn (#136)
    let text = "Deploy to staging";
    let queued = build_midturn_queued_message(None, text, text);
    state.enqueue_reaction(sid, queued);

    // 4. Choice is safely enqueued in reaction queue with Ingress origin
    let drained = state.drain_reaction(sid);
    assert!(drained.is_some(), "queued choice must be drainable");
    let msg = drained.unwrap();
    assert_eq!(msg.display_text, text);
    assert!(msg.context_text.contains(text));
    assert_eq!(msg.origin, crate::brain::agent::PushOrigin::Ingress);
    assert!(
        state.drain_reaction(sid).is_none(),
        "queue must be empty after drain"
    );
}

#[test]
fn midturn_voice_note_is_queued_with_voice_marker() {
    let state = Arc::new(TelegramState::new());
    let sid = Uuid::new_v4();
    let _guard = state
        .try_begin_turn(sid)
        .expect("initial claim must succeed");

    // #893: a voice note that lands mid-turn is marked at the producer — both
    // the agent-facing text and the display/history text — so the queued row
    // the between-rounds drain persists still says it was spoken, not typed.
    // DM-owner shape: `display_text` is the bare text, so the marker leads it.
    let transcript = "This is the third one.";
    let text = mark_voice_note(transcript.to_string());
    let display_text = text.clone();
    let queued = build_midturn_queued_message(None, &text, &display_text);
    state.enqueue_reaction(sid, queued);

    let msg = state
        .drain_reaction(sid)
        .expect("queued voice note must be drainable");
    assert!(
        msg.display_text.starts_with("[Voice note]: "),
        "queued voice note must keep its marker, got {:?}",
        msg.display_text
    );
    assert!(
        msg.display_text.ends_with(transcript),
        "the marker must not replace the transcript, got {:?}",
        msg.display_text
    );
    assert!(
        msg.context_text
            .contains("[Voice note]: This is the third one."),
        "the model-facing payload must carry the marker too, got {:?}",
        msg.context_text
    );
    assert_eq!(msg.origin, crate::brain::agent::PushOrigin::Ingress);
}

#[test]
fn voice_marker_is_applied_once() {
    // The tmp-fallback pickup and the mid-turn enqueue both apply the marker,
    // so a second pass must be a no-op rather than `[Voice note]: [Voice note]: `.
    let once = mark_voice_note("And finally the fifth".to_string());
    assert_eq!(once, "[Voice note]: And finally the fifth");
    assert_eq!(mark_voice_note(once.clone()), once);
    // An empty payload is left empty — never a bare marker.
    assert_eq!(mark_voice_note(String::new()), "");
}

#[tokio::test]
async fn reaction_queue_callback_drains_midturn_followup_tap() {
    let state = Arc::new(TelegramState::new());
    let sid = Uuid::new_v4();

    let text = "Add tests for the new endpoint";
    let queued = build_midturn_queued_message(None, text, text);
    state.enqueue_reaction(sid, queued);

    let cb = state.reaction_queue_callback();
    let item = cb(sid).await;
    assert!(item.is_some(), "callback must drain the mid-turn choice");
    let msg = item.unwrap();
    assert_eq!(msg.display_text, text);
    assert!(msg.context_text.contains(text));
    assert!(
        cb(sid).await.is_none(),
        "callback must return None once drained"
    );
}

#[test]
fn end_of_turn_flush_drains_queued_followup_tap() {
    let state = Arc::new(TelegramState::new());
    let sid = Uuid::new_v4();

    let text = "Cancel workflow";
    let queued = build_midturn_queued_message(None, text, text);
    state.enqueue_reaction(sid, queued);

    // When the turn completes without draining in a tool round, flush_queued_after_turn
    // pulls all queued items for execution.
    let items = state.drain_queued_items(sid);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].origin, QueuedOrigin::Reaction);
    assert_eq!(items[0].msg.display_text, text);
    assert!(state.drain_queued_items(sid).is_empty());
}
