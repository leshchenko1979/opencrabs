//! The per-turn call journal (#974).
//!
//! A side-effecting tool call is written to `tool_executions` BEFORE it is
//! dispatched and closed with its outcome when it returns. That ordering is
//! the whole point: the analytics row `record()` writes runs in a spawned task
//! *after* the tool returned, so a turn killed mid-call leaves no row at all —
//! the one case a journal exists for. With this seam a killed turn leaves a
//! `started` row behind, boot flips it to `unknown`
//! ([`ToolExecutionRepository::reconcile_started_to_unknown`]), and the
//! resumed turn can be told what it started instead of being asked to
//! remember.
//!
//! Only [`EffectClass::Mutating`] and [`EffectClass::Destructive`] calls are
//! journaled. Read-only work writes nothing: a DB write per `read_file` buys
//! no safety, because re-reading cannot duplicate an effect.
//!
//! The row is opened by [`open_call`] and CLOSED by [`close_call`] — never
//! inserted twice. That is why the id is minted here, before dispatch, and
//! returned to the caller: the completion has to address the row that already
//! exists. A `record()` after the fact would either be ignored (`INSERT OR
//! IGNORE`) and leave the row reading `started` forever, or double the call.

use crate::brain::tools::classify::{EffectClass, classify_call};
use crate::db::repository::{ToolExecutionRepository, tool_outcome_status};
use serde_json::Value;
use uuid::Uuid;

/// Journal a call that is about to be dispatched.
///
/// Returns the row id when the call was journaled, `None` for a read-only
/// call (nothing written) or when the journal itself failed — in which case
/// the caller's normal post-call analytics record still runs, so a DB hiccup
/// degrades to today's behaviour instead of losing the row.
pub(crate) async fn open_call(
    repo: &ToolExecutionRepository,
    message_id: Uuid,
    session_id: Uuid,
    turn_id: Uuid,
    tool_name: &str,
    arguments: &Value,
) -> Option<String> {
    if classify_call(tool_name, arguments) == EffectClass::ReadOnly {
        return None;
    }

    let row_id = Uuid::new_v4().to_string();
    match repo
        .record_started(
            &row_id,
            &message_id.to_string(),
            &session_id.to_string(),
            &turn_id.to_string(),
            tool_name,
            &arguments.to_string(),
        )
        .await
    {
        Ok(()) => Some(row_id),
        Err(e) => {
            tracing::error!(
                "[TOOL_EXEC] failed to journal '{}' before dispatch (#974): {:#}",
                tool_name,
                e
            );
            None
        }
    }
}

/// Close a row [`open_call`] opened, with the call's final outcome.
///
/// `success`/`exit_code` go through [`tool_outcome_status`], the same
/// predicate the analytics path uses, so a journaled call and a read-only
/// call are binned identically in every aggregate.
pub(crate) async fn close_call(
    repo: &ToolExecutionRepository,
    row_id: &str,
    success: bool,
    exit_code: Option<i32>,
    provider: Option<&str>,
    model: Option<&str>,
    duration_ms: Option<i64>,
) {
    let status = tool_outcome_status(success, exit_code);
    if let Err(e) = repo
        .complete_started(row_id, status, exit_code, provider, model, duration_ms)
        .await
    {
        // The row stays `started`; boot will report it as unknown rather than
        // pretend it completed. Logged loudly because that misreports a call
        // that DID finish.
        tracing::error!(
            "[TOOL_EXEC] failed to close journal row {} with '{}' (#974): {:#}",
            row_id,
            status,
            e
        );
    }
}

/// Fire-and-forget [`close_call`], mirroring the analytics `record` spawn:
/// the turn must not wait on the DB to finish a call it already has.
pub(crate) fn spawn_close(
    repo: ToolExecutionRepository,
    row_id: String,
    success: bool,
    exit_code: Option<i32>,
    provider: Option<String>,
    model: Option<String>,
    duration_ms: Option<i64>,
) {
    tokio::spawn(async move {
        close_call(
            &repo,
            &row_id,
            success,
            exit_code,
            provider.as_deref(),
            model.as_deref(),
            duration_ms,
        )
        .await;
    });
}

/// Fire-and-forget removal of a row whose call never reached a tool (#974).
///
/// A pre-execution miss is a model error, and the analytics path records
/// nothing for it (#687); leaving the journal row behind would make boot
/// report an unknown side effect that never existed.
pub(crate) fn spawn_discard(repo: ToolExecutionRepository, row_id: String) {
    tokio::spawn(async move {
        if let Err(e) = repo.discard_started(&row_id).await {
            tracing::error!(
                "[TOOL_EXEC] failed to discard journal row {} for a call that never dispatched (#974): {:#}",
                row_id,
                e
            );
        }
    });
}
