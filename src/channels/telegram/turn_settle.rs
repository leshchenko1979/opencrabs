//! Flow-block settle tail, shared by both delivery paths (#402).
//!
//! A turn that opens a processing-log flow block must stamp its terminal
//! outcome on that block and re-render it once, or the header stays on the
//! `⚙` spinner forever. Before this module the tail existed twice — copied
//! into `handler.rs` (`handle_message`) and `resume.rs`
//! (`resume_session_inner`) — and the two copies drifted.
//!
//! ## Why this is its own function
//!
//! Beyond the DRY win, the copy-paste hid a control-flow defect: both call
//! sites ran the settle tail ONLY when `deliver_final_response` returned
//! `Ok(true)` (`if !… { return Ok(()); }`). A delivery that returned `false`
//! (reaction-only ack, cleanup-only shape) or errored skipped the settle
//! entirely, so the block never left the spinner. Hoisting the tail here lets
//! both paths call it unconditionally, after delivery, whatever delivery
//! returned — see the call sites for the ordering contract.
//!
//! ## Infallibility contract
//!
//! [`settle_turn_block`] returns `()` and NEVER propagates a failure: it is
//! the last thing a turn does, and a turn that fails while settling would
//! leave exactly the stranded block this module exists to prevent. Every
//! callee in the chain (`refresh_sections`, `refresh_flow`,
//! `register_flow_state`, the two header-count helpers) is itself infallible,
//! so there is no error to propagate here. A future fallible callee must be
//! handled with a `warn!` and a continue, never by propagating outward.

use std::sync::Arc;

use teloxide::prelude::*;
use teloxide::types::MessageId;
use uuid::Uuid;

use super::delivery::{bg_indicator_for, subagent_counts_for};
use super::flow::{FlowOutcome, StreamingState, refresh_flow};
use super::flow_chrome::refresh_sections;
use super::governor::EditClass;
use super::TelegramState;
use crate::brain::agent::AgentService;

/// Stamp the settled outcome on the turn's flow block and re-render it once.
///
/// A no-op when no block was opened this turn (no tools or intermediates), so
/// plain tool-less turns stay a single clean response.
///
/// `streaming_msg_id` is the block message id captured before delivery; it is
/// carried for the tail trace so a log read names the exact block that
/// settled. The render itself rides `open_group_msg_id` inside `streaming`,
/// which `refresh_flow` reads directly.
///
/// `telegram_state` supplies the two registry reads the settled header needs:
/// the queued-item count and the flow-state registration that folds later
/// background-task acks into this card (#1377).
#[allow(clippy::too_many_arguments)] // the tail's own param set, mirroring deliver_final_response (#402)
pub(crate) async fn settle_turn_block(
    bot: &Bot,
    chat_id: ChatId,
    streaming: &Arc<std::sync::Mutex<StreamingState>>,
    agent: &Arc<AgentService>,
    telegram_state: &Arc<TelegramState>,
    session_id: Uuid,
    flow_outcome: FlowOutcome,
    streaming_msg_id: Option<MessageId>,
) {
    let (icon, verb) = flow_outcome.icon_verb();
    tracing::info!(
        "Telegram settle: session {} — outcome {} {} — block {}",
        session_id,
        icon,
        verb,
        streaming_msg_id.map(|m| m.0).unwrap_or(-1)
    );

    // Stamp the settled outcome on the block and re-render its header once, now
    // that delivery and folded-answer promotion have left the block in its
    // final shape (#480).
    {
        let mut s = streaming.lock().unwrap_or_else(|e| e.into_inner());
        s.flow_outcome = Some(flow_outcome);
        let (bg_indicator, bg_count) = bg_indicator_for(agent, session_id);
        s.bg_indicator = bg_indicator;
        s.bg_count = bg_count;
        // Sub-agents are the second background registry (#1183): the header
        // must wait on them too, split working vs awaiting collection, or a
        // turn ending with agents mid-work reads "✅ Finished".
        s.subagent_counts = subagent_counts_for(agent, session_id);
        s.queued_count = telegram_state.queued_items_count(session_id);
    }
    // Recompute sections now that the turn has settled: the plan Approve/Discard
    // keyboard attaches only at turn end (load_plan_state_section keys off
    // turn_active = flow_outcome.is_none(), now false), so it must be refreshed
    // here before the final render or the last in-flight tick's PlanKb::None
    // would leave the button off for good (#571).
    refresh_sections(streaming, agent, session_id).await;
    // Settle render (#1211 G2 Final): never dropped — if the edit bucket is
    // empty the payload queues latest-wins and the governor's drainer lands
    // it on refill.
    refresh_flow(bot, chat_id, streaming, EditClass::Final).await;
    // #1377: register the settled card's state handle so later background-task
    // completions fold their acks into THIS card instead of standalone
    // bubbles. Same contract on both delivery paths.
    if streaming
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .open_group_msg_id
        .is_some()
    {
        telegram_state
            .register_flow_state(session_id, Arc::clone(streaming))
            .await;
    }
}

/// Run the tail's delivery leg, then its settle leg, **unconditionally** (#402).
///
/// This is the injectable seam that pins the guarantee. `settle` runs whatever
/// `deliver` returned: `Ok(false)` (the reaction-only / cleanup-only shape) and
/// `Err` both used to `return Ok(())` before the settle, stranding the flow
/// block on the `⚙` spinner forever. The delivery result is returned
/// **unchanged**, so the caller can still propagate it after the settle.
///
/// Both legs are taken as futures rather than closures so the production call
/// sites read as a straight-line sequence; neither is polled until awaited, so
/// `settle` stays lazy with respect to `deliver`'s outcome.
///
/// # Do not reintroduce a shortcut here
///
/// An `if !delivered { return … }` or `delivered?` placed BEFORE `settle.await`
/// recreates the #402 defect and fails both
/// `settle_runs_when_delivery_returns_false` and
/// `settle_runs_on_delivery_error` in
/// `src/tests/telegram_options_reclaim_test.rs`.
pub(crate) async fn run_tail<T, E>(
    deliver: impl std::future::Future<Output = Result<T, E>>,
    settle: impl std::future::Future<Output = ()>,
) -> Result<T, E> {
    let delivered = deliver.await;
    // UNCONDITIONAL — the whole point of this function.
    settle.await;
    delivered
}
