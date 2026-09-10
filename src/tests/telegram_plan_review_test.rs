use std::sync::Arc;
use uuid::Uuid;

use crate::channels::telegram::flow_chrome::PlanKb;
use crate::channels::telegram::state::TelegramState;

#[tokio::test]
async fn test_plan_kb_review_button_variants() {
    let kb_normal = PlanKb::ApproveDiscard.keyboard();
    assert!(kb_normal.is_some(), "ApproveDiscard keyboard must exist");
    let rows = kb_normal.unwrap().inline_keyboard;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 3);
    assert_eq!(rows[0][0].text, "🔍 Review");
    assert_eq!(rows[0][0].callback_data.as_deref(), Some("plan:review"));
    assert_eq!(rows[0][1].text, "✅ Approve plan");
    assert_eq!(rows[0][1].callback_data.as_deref(), Some("plan:ok"));
    assert_eq!(rows[0][2].text, "🗑 Discard");
    assert_eq!(rows[0][2].callback_data.as_deref(), Some("plan:no"));

    let kb_reviewing = PlanKb::ReviewingApproveDiscard.keyboard();
    assert!(
        kb_reviewing.is_some(),
        "ReviewingApproveDiscard keyboard must exist"
    );
    let rev_rows = kb_reviewing.unwrap().inline_keyboard;
    assert_eq!(rev_rows.len(), 1);
    assert_eq!(rev_rows[0].len(), 3);
    assert_eq!(rev_rows[0][0].text, "⏳ Reviewing…");
    assert_eq!(rev_rows[0][0].callback_data.as_deref(), Some("plan:noop"));
    assert_eq!(rev_rows[0][1].text, "✅ Approve plan");
    assert_eq!(rev_rows[0][1].callback_data.as_deref(), Some("plan:ok"));
    assert_eq!(rev_rows[0][2].text, "🗑 Discard");
    assert_eq!(rev_rows[0][2].callback_data.as_deref(), Some("plan:no"));
}

#[tokio::test]
async fn test_telegram_state_plan_review_flags() {
    let state = TelegramState::new();
    let sid = Uuid::new_v4();

    assert!(!state.is_plan_reviewing(sid));
    assert!(state.plan_review_delta(sid).is_none());

    state.set_plan_reviewing(sid, true);
    assert!(state.is_plan_reviewing(sid));

    state.set_plan_review_delta(sid, "Rewrote 2 labels".to_string());
    assert_eq!(
        state.plan_review_delta(sid).as_deref(),
        Some("Rewrote 2 labels")
    );

    state.set_plan_reviewing(sid, false);
    assert!(!state.is_plan_reviewing(sid));
    assert_eq!(
        state.plan_review_delta(sid).as_deref(),
        Some("Rewrote 2 labels")
    );

    state.clear_plan_review_delta(sid);
    assert!(state.plan_review_delta(sid).is_none());
}
