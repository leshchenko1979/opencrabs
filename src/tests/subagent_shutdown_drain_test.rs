//! The daemon's sub-agent shutdown drain (#1086).
//!
//! Reported: a `systemctl --user restart` killed the process mid-tool. The
//! daemon installed `tokio::signal::ctrl_c()` — SIGINT only — and `restart`
//! sends SIGTERM, so nothing ran at all: no child was cancelled, no
//! `JoinHandle` was collected (`take_join_handle` had no production caller),
//! and every live child's status file stayed `Running`, leaving the next boot's
//! reconcile to infer `Interrupted` from a dead process.
//!
//! These cases pin the drain's three properties: it cancels, it waits within a
//! bound, and it leaves a terminal record — but only for children that have not
//! already written one.

use crate::brain::agent::service::work_status::test_override;
use crate::brain::agent::service::work_status::{WorkState, WorkStatus};
use crate::brain::tools::subagent::{SubAgent, SubAgentManager, SubAgentState};
use std::fs;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

fn isolate(tag: &str) {
    let dir = std::env::temp_dir().join(format!(
        "opencrabs-1086-drain-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    test_override::set(dir.join("detached"));
}

fn seed_running(id: &str, parent: Uuid) -> WorkStatus {
    WorkStatus::new_agent(
        id,
        "worker",
        &Uuid::new_v4().to_string(),
        "do the thing",
        Some(&parent.to_string()),
        None,
    )
    .expect("status file")
}

fn read_state(id: &str) -> WorkState {
    WorkStatus::read(id)
        .unwrap_or_else(|| panic!("status record for {id} is gone"))
        .state
}

/// The common case: nothing live. No delay, no records, no noise.
#[tokio::test]
async fn an_empty_roster_returns_at_once() {
    let mgr = Arc::new(SubAgentManager::new());
    let started = std::time::Instant::now();
    let outcome = mgr.drain_for_shutdown(Duration::from_secs(30)).await;
    assert_eq!(outcome.live, 0);
    assert_eq!(outcome.settled, 0);
    assert_eq!(outcome.interrupted, 0);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "an empty roster must not sit out the deadline"
    );
}

/// A live child is cancelled, its task is awaited, and its `Running` record is
/// moved to `Interrupted` — the terminal state the child's own exit block
/// cannot write, because the process is going down under it.
#[tokio::test]
async fn a_live_child_is_cancelled_and_recorded_interrupted() {
    isolate("live");
    let mgr = Arc::new(SubAgentManager::new());
    let parent = Uuid::new_v4();
    let id = "drain0001";
    let _status = seed_running(id, parent);

    let agent = SubAgent::new(id, "worker", Uuid::new_v4(), parent);
    let token = agent.cancel_token.clone();
    // The child's shape: park on its own token, which is what a sub-agent task
    // does while it waits for a tool to return.
    let handle = tokio::spawn(async move {
        token.cancelled().await;
    });
    mgr.insert(SubAgent {
        join_handle: Some(handle),
        ..agent
    });

    let outcome = mgr.drain_for_shutdown(Duration::from_secs(5)).await;

    assert_eq!(outcome.live, 1, "the child was live when the drain began");
    assert_eq!(
        outcome.settled, 1,
        "its task ends on the cancel it was given"
    );
    assert_eq!(
        outcome.interrupted, 1,
        "and it had no terminal record of its own"
    );
    assert_eq!(
        mgr.get_state(id),
        Some(SubAgentState::Cancelled),
        "the roster must read cancelled, not still running"
    );
    assert_eq!(
        read_state(id),
        WorkState::Interrupted,
        "boot recovery must not have to guess what happened to this child"
    );
}

/// A wedged child does not hold the shutdown open: the deadline is absolute, so
/// the drain returns and records it interrupted anyway.
#[tokio::test]
async fn a_wedged_child_is_recorded_after_the_deadline() {
    isolate("wedged");
    let mgr = Arc::new(SubAgentManager::new());
    let parent = Uuid::new_v4();
    let id = "drain0002";
    let _status = seed_running(id, parent);

    // Never resolves and never observes its token: the worst case — a tool call
    // stuck on a socket. Only the deadline ends the wait.
    let handle = tokio::spawn(std::future::pending::<()>());
    mgr.insert(SubAgent {
        join_handle: Some(handle),
        ..SubAgent::new(id, "worker", Uuid::new_v4(), parent)
    });

    let started = std::time::Instant::now();
    let outcome = mgr.drain_for_shutdown(Duration::from_millis(120)).await;
    let elapsed = started.elapsed();

    assert_eq!(outcome.live, 1);
    assert_eq!(outcome.settled, 0, "a wedged task never settles");
    assert_eq!(
        outcome.interrupted, 1,
        "the deadline must still leave a terminal record behind"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "shutdown must not hang on a wedged child; waited {elapsed:?}"
    );
    assert_eq!(read_state(id), WorkState::Interrupted);
}

/// A child that wrote its OWN terminal record during the drain is left alone.
/// Overwriting it would trade a true outcome (it finished) for a false one
/// (the process killed it).
#[tokio::test]
async fn a_child_that_already_settled_keeps_its_own_outcome() {
    isolate("settled");
    let mgr = Arc::new(SubAgentManager::new());
    let parent = Uuid::new_v4();
    let id = "drain0003";
    let mut status = seed_running(id, parent);
    status
        .mark_completed("the answer".to_string())
        .expect("write");

    let handle = tokio::spawn(async {});
    mgr.insert(SubAgent {
        join_handle: Some(handle),
        ..SubAgent::new(id, "worker", Uuid::new_v4(), parent)
    });

    let outcome = mgr.drain_for_shutdown(Duration::from_secs(5)).await;

    assert_eq!(outcome.live, 1);
    assert_eq!(outcome.settled, 1);
    assert_eq!(
        outcome.interrupted, 0,
        "a completed child has an outcome worth keeping"
    );
    assert_eq!(
        read_state(id),
        WorkState::Completed,
        "the drain must never overwrite a terminal record it did not write"
    );
}

/// Draining twice is harmless: the second pass finds no live child, so a
/// shutdown path that runs again (a signal arriving twice, a retry) neither
/// re-cancels nor rewrites anything.
#[tokio::test]
async fn a_second_drain_is_a_no_op() {
    isolate("twice");
    let mgr = Arc::new(SubAgentManager::new());
    let parent = Uuid::new_v4();
    let id = "drain0004";
    let _status = seed_running(id, parent);

    let agent = SubAgent::new(id, "worker", Uuid::new_v4(), parent);
    let token = agent.cancel_token.clone();
    let handle = tokio::spawn(async move { token.cancelled().await });
    mgr.insert(SubAgent {
        join_handle: Some(handle),
        ..agent
    });

    let first = mgr.drain_for_shutdown(Duration::from_secs(5)).await;
    assert_eq!(first.live, 1);

    let second = mgr.drain_for_shutdown(Duration::from_secs(5)).await;
    assert_eq!(second.live, 0, "the roster has no live child left");
    assert_eq!(second.interrupted, 0);
}

// ── wiring: the daemon must actually reach the drain ─────────────────────
//
// The cases above prove the drain is correct. These prove it is USED: a
// perfect drain that nothing calls is exactly the state this defect was in —
// `take_join_handle` existed, was tested, and had no production caller.

const UI_SRC: &str = include_str!("../cli/ui.rs");
const CLI_SHUTDOWN_SRC: &str = include_str!("../cli/shutdown.rs");
const SPAWN_SRC: &str = include_str!("../brain/tools/subagent/spawn.rs");

/// `ctrl_c()` alone is SIGINT only, and `systemctl --user restart` sends
/// SIGTERM. The daemon must install a SIGTERM handler.
#[test]
fn the_daemon_handles_sigterm() {
    assert!(
        CLI_SHUTDOWN_SRC.contains("SignalKind::terminate()"),
        "the shutdown module must register a SIGTERM handler; without one a \
         service restart kills the process mid-tool with no unwinding at all"
    );
    assert!(
        CLI_SHUTDOWN_SRC.contains("SignalKind::interrupt()"),
        "SIGINT must be handled by the same mechanism, so both stop requests \
         reach the drain"
    );
    assert!(
        UI_SRC.contains("crate::cli::shutdown::wait_for_stop_signal()"),
        "the daemon loop must await the signal helper; a handler installed and \
         never awaited catches nothing"
    );
    assert!(
        !UI_SRC.contains("tokio::signal::ctrl_c()"),
        "the SIGINT-only wait must be gone from the daemon path"
    );
}

/// The order is the fix: flag, then drain, then locks — never locks first.
#[test]
fn the_drain_raises_the_flag_before_cancelling_and_releases_locks_last() {
    let body = CLI_SHUTDOWN_SRC
        .split("pub async fn drain_and_release")
        .nth(1)
        .expect("drain_and_release is gone from the shutdown module");
    let body = &body[..body.find("\n}").unwrap_or(body.len())];

    let flag_at = body
        .find("mark_shutting_down()")
        .expect("the drain must raise the shutdown flag");
    let cancel_at = body
        .find("drain_for_shutdown")
        .expect("the drain must call the manager drain");
    let locks_at = body
        .find("release_all_locks()")
        .expect("the drain must release the profile locks");

    assert!(
        flag_at < cancel_at,
        "the flag must be raised BEFORE any child token is cancelled: a cancel \
         with the flag still clear is indistinguishable from a user stop, and \
         the tool loop deletes the recovery row of a turn it believes was \
         abandoned (#481)"
    );
    assert!(
        cancel_at < locks_at,
        "profile locks must be released LAST, or a replacement process claims \
         the profile while children are still unwinding over the same rows"
    );
}

/// A shutdown cancel must not be recorded as a failure — and it must not be
/// recorded as a completion either.
#[test]
fn a_shutdown_cancel_is_recorded_interrupted_not_failed() {
    assert!(
        SPAWN_SRC.contains("interrupted_by_shutdown"),
        "the child exit block must tell a shutdown cancel from a real failure; \
         recording it as `Failed` writes a fault that never happened"
    );

    // The shutdown decision is taken from the TOKEN plus the flag, not from the
    // outcome's shape: the parked path breaks `Ok` on a cancel, so a branch that
    // matched on `outcome` first would mark a cut-off child `Completed` and push
    // its half-finished round to a dying parent.
    let decision = SPAWN_SRC
        .split("let interrupted_by_shutdown")
        .nth(1)
        .expect("the shutdown decision is gone from the exit block");
    let decision = &decision[..decision.find(';').unwrap_or(decision.len())];
    assert!(
        decision.contains("is_cancelled()"),
        "the decision must read the token; an outcome-shape test misses the \
         parked `Ok` break"
    );
    assert!(
        decision.contains("is_shutting_down()"),
        "the decision must read the shutdown flag; a token alone cannot tell a \
         parent stop from a process exit"
    );

    let branch = SPAWN_SRC
        .split("} else if interrupted_by_shutdown {")
        .nth(1)
        .expect("the shutdown branch is gone from the exit block");
    let branch = &branch[..branch.find("} else {").unwrap_or(branch.len())];
    assert!(
        branch.contains("mark_interrupted()"),
        "a shutdown-cancelled child must be recorded `Interrupted` through the \
         same WorkStatus path the rest of the exit block uses"
    );
    assert!(
        !branch.contains("mark_failed("),
        "the shutdown branch must not also mark the child failed"
    );
    assert!(
        !branch.contains("mark_completed("),
        "the shutdown branch must not mark the child completed"
    );
    assert!(
        !branch.contains("complete_and_deliver("),
        "a child cut off by shutdown must not push its half-finished round to \
         the parent as a result"
    );
    assert!(
        branch.contains("settle_worktree("),
        "the isolation tree must still settle: a shutdown is not a licence to \
         discard uncommitted work"
    );
}
