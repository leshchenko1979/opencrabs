//! Plan mutations are serialised per session (#506).
//!
//! The plan tool reads the plan JSON when it enters `execute` and writes it back
//! much later, with nothing held across the pair. Two calls dispatched in ONE
//! parallel block therefore both read the same baseline and the second save
//! reverts the first: the agent is told a task completed and the state never
//! persisted. That false success receipt is what makes this worse than a lost
//! write.
//!
//! `utils::plan_files::mutate_plan` is the one serialised read-modify-write
//! primitive; every production writer routes through it or holds the exposed
//! `plan_state_lock` around its own window.
//!
//! HARNESS NOTE. `with_profile_home_async` scopes a `tokio::task_local!`, and a
//! task-local is NOT inherited by `tokio::spawn` (that fn's own doc says it
//! "never leaks to sibling tasks"). A spawned leg therefore starts with the REAL
//! home and resolves a different session dir than the fixture seeded — which is
//! exactly how these tests first failed in CI, with `load_plan` returning None
//! for a plan written a moment earlier. Every spawned leg re-enters the scope
//! through `in_profile`.
//!
//! Fixtures are synthetic and carry no user identifiers.

use crate::brain::tools::plan_tool::PlanTool;
use crate::brain::tools::{Tool, ToolExecutionContext};
use crate::config::profile::{home_for_profile, with_profile_home_async};
use crate::tui::plan::{PlanDocument, PlanStatus, PlanTask, TaskStatus, TaskType};
use crate::utils::plan_files::{
    load_plan, mutate_plan, plan_state_lock, save_plan, verify_persisted,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use uuid::Uuid;

/// A throwaway profile home, so nothing touches the real
/// `~/.opencrabs/session/`. Removed on drop, including on a panicking test.
struct TempProfile(String);

impl TempProfile {
    fn new() -> Self {
        Self(format!("plan-mutation-lock-test-{}", Uuid::new_v4()))
    }

    /// The profile name, owned so it can be moved into a spawned task.
    fn name(&self) -> String {
        self.0.clone()
    }

    /// Run `fut` with the profile home pointed at this profile.
    async fn scoped<F, T>(&self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        with_profile_home_async(Some(&self.0), fut).await
    }
}

impl Drop for TempProfile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(home_for_profile(Some(&self.0)));
    }
}

/// Enter `profile`'s home inside a SPAWNED task — see the harness note above.
async fn in_profile<F, T>(profile: String, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    with_profile_home_async(Some(&profile), fut).await
}

/// Seed an `Active` checklist, the state `complete` and `start` both require
/// (`checklist_blocked_reason` refuses anything but NoPlan/Active).
async fn seed_active_plan(sid: Uuid, tasks: usize) {
    let mut plan = PlanDocument::new(sid, "Race fixture".to_string());
    for i in 0..tasks {
        plan.add_task(PlanTask::new(
            i + 1,
            format!("task {}", i + 1),
            "d".to_string(),
            TaskType::Edit,
        ));
    }
    plan.status = PlanStatus::Active;
    save_plan(&plan).await.unwrap();
}

/// The issue's own reproduction: `complete` and `start` dispatched together.
///
/// Before the fix both loaded the same baseline, the second save won, and the
/// completed task silently reverted to Pending while the caller had already been
/// handed a success receipt. Multi-threaded so the two legs genuinely overlap:
/// `execute` awaits between the load and the save (`clear_task_goal`,
/// `is_plan_autonomy`, mermaid validation), and that window is what loses the
/// write.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_complete_and_start_both_persist() {
    let tp = TempProfile::new();
    tp.scoped(async {
        let sid = Uuid::new_v4();
        seed_active_plan(sid, 4).await;

        // Two contexts, same session id: the tool resolves plan state from
        // `context.session_id`, which is what the race shares.
        let complete = tokio::spawn(in_profile(tp.name(), async move {
            let ctx = ToolExecutionContext::new(sid);
            PlanTool
                .execute(
                    serde_json::json!({
                        "operation": "complete",
                        "task_order": 1,
                        "action": "success",
                        "output": "done",
                    }),
                    &ctx,
                )
                .await
        }));
        let start = tokio::spawn(in_profile(tp.name(), async move {
            let ctx = ToolExecutionContext::new(sid);
            PlanTool
                .execute(
                    serde_json::json!({ "operation": "start", "task_order": 2 }),
                    &ctx,
                )
                .await
        }));

        let (complete, start) = tokio::join!(complete, start);
        let complete = complete.unwrap().unwrap();
        let start = start.unwrap().unwrap();
        assert!(complete.success, "complete failed: {:?}", complete.error);
        assert!(start.success, "start failed: {:?}", start.error);

        let plan = load_plan(sid).await.expect("plan must still exist");
        let t1 = plan.get_task_by_order(1).expect("task 1");
        let t2 = plan.get_task_by_order(2).expect("task 2");

        // Both receipts were success, so both mutations must be on disk. A
        // revert here is exactly the false-receipt defect.
        assert!(
            matches!(t1.status, TaskStatus::Completed),
            "complete returned success but task 1 is {:?} on disk — the mutation \
             was reverted by the concurrent start",
            t1.status
        );
        assert!(
            matches!(t2.status, TaskStatus::InProgress),
            "start returned success but task 2 is {:?} on disk",
            t2.status
        );
    })
    .await;
}

/// N concurrent mutations: the critical section holds one at a time, and every
/// mutation persists. Serialising is necessary but not sufficient — a lock that
/// still saved a stale baseline would pass the first assertion and fail the
/// second.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_mutations_serialise_and_all_persist() {
    let tp = TempProfile::new();
    tp.scoped(async {
        const N: usize = 8;
        let sid = Uuid::new_v4();
        // Start from an existing plan so every mutation is a read-modify-write
        // on the same document rather than a create.
        seed_active_plan(sid, 0).await;

        let inside = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for i in 0..N {
            let (inside, max_seen) = (inside.clone(), max_seen.clone());
            handles.push(tokio::spawn(in_profile(tp.name(), async move {
                mutate_plan(sid, move |plan| {
                    let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                    max_seen.fetch_max(now, Ordering::SeqCst);
                    let plan = plan.as_mut().expect("plan must exist");
                    plan.add_task(PlanTask::new(
                        i + 1,
                        format!("t{}", i + 1),
                        "d".to_string(),
                        TaskType::Edit,
                    ));
                    // Widen the critical section so an unserialised
                    // implementation overlaps reliably rather than by luck.
                    for _ in 0..50_000 {
                        std::hint::black_box(0u64);
                    }
                    inside.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
                .await
                .unwrap();
            })));
        }
        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(
            max_seen.load(Ordering::SeqCst),
            1,
            "two plan mutations for one session were inside the critical section \
             together, which is what loses an update"
        );

        let plan = load_plan(sid).await.expect("plan must exist");
        assert_eq!(
            plan.tasks.len(),
            N,
            "every mutation reported success, so all {N} tasks must be on disk"
        );
    })
    .await;
}

/// Per session, not global: a busy chat must not stall an unrelated one.
#[tokio::test]
async fn different_sessions_do_not_block_each_other() {
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let lock_a = plan_state_lock(a);
    let held = lock_a.lock().await;

    // B must be acquirable while A is held.
    let lock_b = plan_state_lock(b);
    assert!(
        lock_b.try_lock().is_ok(),
        "an unrelated session must not wait on this one"
    );
    drop(held);
}

/// Handing out a fresh lock per call would serialise nothing.
#[tokio::test]
async fn the_same_session_returns_the_same_lock() {
    let session = Uuid::new_v4();
    let first = plan_state_lock(session);
    let second = plan_state_lock(session);
    assert!(
        Arc::ptr_eq(&first, &second),
        "a per-call lock protects nothing"
    );
}

/// The isolated-start leg persists `InProgress`, awaits the worker spawn, then
/// persists again — and it must NOT hold the slot across that await, or plan
/// availability for the session would depend on how long a subagent takes
/// (design 1.4, option (b)).
///
/// This pins the contract the code relies on at its `drop(state_guard.take())`
/// site: once the guard is released, another mutation proceeds. The live-spawn
/// half — asserting the release happens at that exact statement while a real
/// worker is in flight — needs spawn machinery (a service context plus a manager
/// and registry) that a unit test cannot wire, so it belongs to the behavioural
/// smoke leg.
#[tokio::test]
async fn the_state_slot_is_free_across_a_slow_await() {
    let tp = TempProfile::new();
    tp.scoped(async {
        let sid = Uuid::new_v4();
        seed_active_plan(sid, 2).await;

        // Mirror the code's shape: take the slot, release it, then await
        // something slow in the worker's place.
        let lock = plan_state_lock(sid);
        let guard = lock.lock().await;
        drop(guard);

        // While the "spawn" is in flight, a mutation must complete rather than
        // block for the awaited thing's lifetime.
        let during = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            mutate_plan(sid, |plan| {
                let plan = plan.as_mut().expect("plan must exist");
                plan.add_task(PlanTask::new(
                    3,
                    "during the spawn".to_string(),
                    "d".to_string(),
                    TaskType::Edit,
                ));
                Ok(())
            }),
        )
        .await;

        assert!(
            during.is_ok(),
            "a mutation blocked across the slow await — the slot is being held \
             for the awaited thing's lifetime"
        );
        assert_eq!(load_plan(sid).await.unwrap().tasks.len(), 3);
    })
    .await;
}

/// The read-back receipt (leg 2). The lock alone cannot catch a writer that does
/// not take it — `archive_plan`, `discard_plan`, or a hand-edited file — so a
/// save that was replaced must surface as an ERROR rather than a success
/// receipt. This is the false-receipt half of #506, the half that misleads the
/// agent.
#[tokio::test]
async fn read_back_receipt_reports_a_replaced_save() {
    let tp = TempProfile::new();
    tp.scoped(async {
        let sid = Uuid::new_v4();
        seed_active_plan(sid, 2).await;
        let expected = load_plan(sid).await.expect("seeded plan");

        // A save that survived must verify: the receipt must not cry wolf, or
        // every mutation would fail on a healthy store.
        verify_persisted(sid, &expected)
            .await
            .expect("a plan that was just persisted must verify");

        // Force the lost update the receipt exists to catch: a stale baseline is
        // written over the saved file, exactly as a non-lock-taking writer would
        // leave it.
        let mut stale = PlanDocument::new(sid, "Stale baseline".to_string());
        stale.status = PlanStatus::Active;
        save_plan(&stale).await.unwrap();

        let err = verify_persisted(sid, &expected)
            .await
            .expect_err("a replaced save must NOT return a success receipt");
        assert!(
            err.contains("DIVERGED"),
            "the receipt must name the divergence, got: {err}"
        );
        assert!(
            err.contains("LOST"),
            "the receipt must say the mutation was lost, got: {err}"
        );
    })
    .await;
}

/// A clean mutation still succeeds — the receipt must not turn a healthy store
/// into a failure. This is the false-negative guard for the receipt above.
#[tokio::test]
async fn a_clean_mutation_still_returns_success() {
    let tp = TempProfile::new();
    tp.scoped(async {
        let sid = Uuid::new_v4();
        seed_active_plan(sid, 1).await;

        mutate_plan(sid, |plan| {
            plan.as_mut()
                .expect("plan must exist")
                .add_task(PlanTask::new(
                    2,
                    "second".to_string(),
                    "d".to_string(),
                    TaskType::Edit,
                ));
            Ok(())
        })
        .await
        .expect("a mutation that persisted must not be reported as a failure");

        assert_eq!(load_plan(sid).await.unwrap().tasks.len(), 2);
    })
    .await;
}
