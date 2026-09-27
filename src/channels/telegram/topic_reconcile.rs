//! #572 — boot reconciliation of topic bindings.
//!
//! A topic can die while the daemon is down. Telegram sends `forum_topic_closed`
//! exactly once, as a service message — and a topic DELETED in that window sends
//! nothing at all: the pinned teloxide-core 0.13.0 carries no
//! `forum_topic_deleted` kind in any file, so a deletion is discovered, never
//! received. Either way the `session_bindings` row outlives the topic, and the
//! dead address keeps resolving to a live session.
//!
//! Two passes, in this order, and the ORDER is the design:
//!
//! 1. **Recorded facts first.** The store already knows about every closure the
//!    daemon was awake to see. Applying those is free, exact, and removes those
//!    topics from the probe set below.
//! 2. **Then a bounded probe** over the residue: `PROBE_BUDGET` topics per boot,
//!    quietest first (least recent `channel_messages` activity), through an
//!    invisible `sendChatAction` — the one call that addresses a topic without
//!    changing anything visible in it.
//!
//! Recency is a CANDIDATE SELECTOR, never a state verdict: a quiet topic may be
//! perfectly alive. Only a probe returning one of the shared
//! [`plan_card::is_message_gone_error`] markers is a verdict. Everything else —
//! network error, rate limit, unrecognised text — is INCONCLUSIVE and leaves the
//! binding, and the store, exactly as they were.

use super::TelegramState;
use super::handler::{TopicEvent, apply_topic_teardown, teardown_binding};
use crate::db::{ChannelMessageRepository, Pool, SessionBindingRepository};
use crate::services::{ServiceContext, SessionService};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use teloxide::prelude::Requester;
use teloxide::types::{ChatAction, ChatId, MessageId, ThreadId};
use teloxide::payloads::SendChatActionSetters;

/// Probes issued per boot. A boot task must not turn a restart into a sweep of
/// every binding the bot ever wrote.
pub(crate) const PROBE_BUDGET: usize = 8;

/// How many lifecycle facts pass 1 will read. Facts are written one per event,
/// so this is many boots' worth; the bound exists so a pathological store cannot
/// make boot read an unbounded number of rows.
const FACT_SCAN_LIMIT: i64 = 500;

/// Delay before the probe pass. Long enough for the channel's own startup to
/// register the bot handle the probe needs, and this task is not urgent —
/// a topic that has been dead for a day can stay dead for another minute.
const BOOT_WARMUP: Duration = Duration::from_secs(60);

/// What one invisible probe returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProbeOutcome {
    /// Telegram rejected the topic: it is closed or gone.
    Gone,
    /// The topic answered. It is alive.
    Live,
    /// Not a verdict — a transient or unrecognised failure. The caller must
    /// leave the binding alone; only [`ProbeOutcome::Gone`] acts.
    Inconclusive(String),
}

/// Classify a probe failure through the vocabulary the card path already uses
/// ([`super::plan_card::is_message_gone_error`]) rather than a second list that
/// could drift from it. Seven markers: message to edit not found / message
/// can't be edited / message_id_invalid / chat not found / topic_closed /
/// message thread not found / thread not found.
pub(crate) fn classify_probe_error(error: &str) -> ProbeOutcome {
    if super::plan_card::is_message_gone_error(error) {
        ProbeOutcome::Gone
    } else {
        ProbeOutcome::Inconclusive(error.to_string())
    }
}

/// What one reconciliation run did. Counts only — the caller logs them, and the
/// tests assert on them.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ReconcileReport {
    /// Pass 1: bindings torn down from a recorded fact.
    pub facts_applied: usize,
    /// Pass 2: probes actually issued (never more than the budget).
    pub probed: usize,
    /// Pass 2: probes that came back `Gone` and were torn down.
    pub torn_down: usize,
    /// Pass 2: probes that decided nothing.
    pub inconclusive: usize,
}

/// Reconcile topic bindings against recorded facts, then against the Bot API.
///
/// The `probe` is injected so the two passes and the budget are testable without
/// a network: production passes [`probe_topic`], the tests pass a scripted
/// closure. Only the `Gone` arm acts — the `Live` and `Inconclusive` arms are
/// deliberately empty, which is what makes "an inconclusive probe never retires
/// a binding and never writes a fact" a property of the code's shape and not of
/// a reviewer's attention.
pub(crate) async fn reconcile<F, Fut>(
    bindings: &SessionBindingRepository,
    messages: &ChannelMessageRepository,
    session_svc: &SessionService,
    budget: usize,
    probe: F,
) -> ReconcileReport
where
    F: Fn(i64, i32) -> Fut,
    Fut: std::future::Future<Output = ProbeOutcome>,
{
    let mut report = ReconcileReport::default();

    // ---- Pass 1: recorded facts. Runs FIRST, and the topics it decides are
    // removed from the candidate set below, so a fact is never re-probed and a
    // probe is never spent on a topic the store already answered for.
    let facts = messages
        .topic_lifecycle_facts(FACT_SCAN_LIMIT)
        .await
        .unwrap_or_default();
    let mut decided: HashSet<(i64, i32)> = HashSet::new();
    for fact in facts {
        let (Ok(chat), Some(thread)) = (
            fact.channel_chat_id.parse::<i64>(),
            fact.thread_id.as_deref().and_then(|t| t.parse::<i32>().ok()),
        ) else {
            continue;
        };
        // Rows arrive newest-first, so the FIRST row for a topic is its current
        // state: inserting fails for every later (older) row about it.
        if !decided.insert((chat, thread)) {
            continue;
        }
        if fact.message_type == "topic_reopened" {
            // Alive again. The fact is the record; nothing is torn down.
            continue;
        }
        // No fact is written here on purpose: this pass APPLIES a fact that
        // already exists, and writing one would append a row per boot.
        let recorded = fact.message_type.clone();
        if teardown_binding(bindings, session_svc, chat, thread).await {
            report.facts_applied += 1;
            tracing::info!(
                "#572 reconcile: applied recorded fact {recorded} — channel=telegram chat={chat} topic={thread}"
            );
        }
    }

    // ---- Pass 2: bounded probe over the residue, quietest first.
    for (chat, thread) in probe_candidates(bindings, messages)
        .await
        .into_iter()
        .filter(|c| !decided.contains(c))
        .take(budget)
    {
        report.probed += 1;
        match probe(chat, thread).await {
            ProbeOutcome::Gone => {
                tracing::info!(
                    "#572 reconcile: probe → GONE — channel=telegram chat={chat} topic={thread}, tearing down"
                );
                // Disappeared without an event we could receive, so THIS is the
                // only place the fact can be written. Recorded through the same
                // core as a closure, under its own message_type.
                apply_topic_teardown(
                    TopicEvent::Deleted,
                    chat,
                    thread,
                    None,
                    None,
                    None,
                    messages,
                    bindings,
                    session_svc,
                )
                .await;
                report.torn_down += 1;
            }
            // Neither of these decides anything, and the log line says so: a
            // binding left standing with no reason visible is indistinguishable
            // from a binding the sweep forgot.
            ProbeOutcome::Live => {
                tracing::info!(
                    "#572 reconcile: probe → live — channel=telegram chat={chat} topic={thread}, binding kept"
                );
                report.inconclusive += 1;
            }
            ProbeOutcome::Inconclusive(why) => {
                tracing::info!(
                    "#572 reconcile: probe → inconclusive ({why}) — channel=telegram chat={chat} topic={thread}, binding kept"
                );
                report.inconclusive += 1;
            }
        }
    }

    report
}

/// Bound topic bindings, quietest first — the probe's candidate order.
///
/// Quietest = least recent `channel_messages` row for that thread, with a thread
/// that has never carried a row sorting first. This is a heuristic for spending
/// a small budget where a dead topic is most likely; it decides nothing about
/// state.
async fn probe_candidates(
    bindings: &SessionBindingRepository,
    messages: &ChannelMessageRepository,
) -> Vec<(i64, i32)> {
    let mut candidates: Vec<(i64, i32, i64)> = Vec::new();
    for binding in bindings
        .all_for_channel("telegram")
        .await
        .unwrap_or_default()
    {
        let (Ok(chat), Some(thread)) = (binding.chat_id.parse::<i64>(), binding.thread_id) else {
            continue;
        };
        let last_activity = messages
            .recent(
                Some("telegram"),
                &binding.chat_id,
                1,
                Some(&thread.to_string()),
                None,
            )
            .await
            .ok()
            .and_then(|rows| rows.first().map(|row| row.created_at.timestamp()))
            .unwrap_or(0);
        candidates.push((chat, thread, last_activity));
    }
    candidates.sort_by_key(|(_, _, last_activity)| *last_activity);
    candidates
        .into_iter()
        .map(|(chat, thread, _)| (chat, thread))
        .collect()
}

/// The real probe: a typing action in the topic. Invisible by construction — a
/// chat action leaves no message and changes no topic state — and it fails with
/// the topic-gone vocabulary when the thread is closed or deleted.
async fn probe_topic(bot: &teloxide::Bot, chat_id: i64, thread: i32) -> ProbeOutcome {
    match bot
        .send_chat_action(ChatId(chat_id), ChatAction::Typing)
        .message_thread_id(ThreadId(MessageId(thread)))
        .await
    {
        Ok(_) => ProbeOutcome::Live,
        Err(error) => classify_probe_error(&error.to_string()),
    }
}

/// Boot task: one reconciliation shortly after start. Best-effort and never
/// fatal — a failed reconcile costs a stale binding, not correctness.
pub(crate) fn spawn(pool: Pool, telegram_state: Arc<TelegramState>) {
    tokio::spawn(async move {
        tokio::time::sleep(BOOT_WARMUP).await;
        let Some(bot) = telegram_state.bot().await else {
            // info!, not debug!: this is the one path where reconciliation
            // silently does nothing, and an auditor must be able to see that it
            // did not run.
            tracing::info!(
                "#572 topic reconcile: no bot handle at boot — recorded facts were skipped, probe pass not run"
            );
            return;
        };
        let bindings = SessionBindingRepository::new(pool.clone());
        let messages = ChannelMessageRepository::new(pool.clone());
        let session_svc = SessionService::new(ServiceContext::new(pool.clone()));
        let report = reconcile(
            &bindings,
            &messages,
            &session_svc,
            PROBE_BUDGET,
            |chat, thread| {
                let bot = bot.clone();
                async move { probe_topic(&bot, chat, thread).await }
            },
        )
        .await;
        tracing::info!(
            "#572 topic reconcile: facts_applied={} probed={} torn_down={} inconclusive={}",
            report.facts_applied,
            report.probed,
            report.torn_down,
            report.inconclusive,
        );
    });
}
