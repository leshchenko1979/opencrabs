//! Tests for the goal evidence pack (#299).
//!
//! The pack is what the judge is allowed to reason from, so the contract is
//! mechanical and read-only: it reports what the harness actually holds
//! (detached commands, open plan tasks, turn budget) and never fails. The
//! shared predicate `unresolved_tasks` is covered here too, because the plan
//! reminder and the evidence pack must never disagree about whether a plan is
//! finished.

use crate::brain::agent::service::background_tasks::BackgroundTaskManager;
use crate::brain::agent::service::{format_plan_reminder, unresolved_tasks};
use crate::brain::goal::evidence::{GoalEvidence, build_goal_evidence};
use crate::config::profile::{home_for_profile, with_profile_home_async};
use crate::tui::plan::{PlanDocument, PlanStatus, PlanTask, TaskStatus, TaskType};
use crate::utils::plan_files::save_plan;
use std::sync::Arc;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run `f` under a throwaway profile home so no test touches the real
/// `~/.opencrabs/agents/session/`.
async fn in_temp_home<F, T>(f: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let profile = format!("goal-evidence-test-{}", Uuid::new_v4());
    let out = with_profile_home_async(Some(&profile), f).await;
    let home = home_for_profile(Some(&profile));
    let _ = std::fs::remove_dir_all(&home);
    out
}

fn plan_with(session_id: Uuid, status: PlanStatus, tasks: &[(&str, TaskStatus)]) -> PlanDocument {
    let mut plan = PlanDocument::new(session_id, "Ship the thing".to_string());
    plan.status = status;
    for (i, (title, st)) in tasks.iter().enumerate() {
        let mut t = PlanTask::new(i + 1, title.to_string(), String::new(), TaskType::Edit);
        t.status = st.clone();
        plan.add_task(t);
    }
    plan
}

// ---------------------------------------------------------------------------
// unresolved_tasks — the shared predicate
// ---------------------------------------------------------------------------

/// Acceptance: Completed and Skipped are excluded; Pending and InProgress are
/// included. Failed and Blocked are outstanding work too.
#[test]
fn unresolved_tasks_excludes_resolved_statuses() {
    let plan = plan_with(
        Uuid::new_v4(),
        PlanStatus::Active,
        &[
            ("done one", TaskStatus::Completed),
            ("skipped one", TaskStatus::Skipped),
            ("in flight", TaskStatus::InProgress),
            ("not started", TaskStatus::Pending),
            ("broke", TaskStatus::Failed),
            ("waiting", TaskStatus::Blocked("dep".to_string())),
        ],
    );

    let unresolved = unresolved_tasks(&plan);
    assert_eq!(unresolved.len(), 4, "got: {unresolved:?}");
    assert!(
        !unresolved.iter().any(|t| t.contains("done one")),
        "Completed must be excluded"
    );
    assert!(
        !unresolved.iter().any(|t| t.contains("skipped one")),
        "Skipped must be excluded"
    );
    for title in ["in flight", "not started", "broke", "waiting"] {
        assert!(
            unresolved.iter().any(|t| t.contains(title)),
            "'{title}' must be reported as unresolved, got: {unresolved:?}"
        );
    }
}

/// Entries carry the task order and title, in plan order.
#[test]
fn unresolved_tasks_are_ordered_and_labelled() {
    let plan = plan_with(
        Uuid::new_v4(),
        PlanStatus::Active,
        &[
            ("first", TaskStatus::Pending),
            ("second", TaskStatus::Pending),
        ],
    );
    let unresolved = unresolved_tasks(&plan);
    assert!(unresolved[0].starts_with("1. first"), "got: {unresolved:?}");
    assert!(
        unresolved[1].starts_with("2. second"),
        "got: {unresolved:?}"
    );
}

/// An empty plan has nothing unresolved — the case that must not nag.
#[test]
fn empty_plan_has_nothing_unresolved() {
    let plan = plan_with(Uuid::new_v4(), PlanStatus::Active, &[]);
    assert!(unresolved_tasks(&plan).is_empty());
}

/// Acceptance: the reminder's behaviour is unchanged — it is still driven by
/// exactly this predicate.
#[test]
fn reminder_agrees_with_unresolved_tasks() {
    let all_resolved = plan_with(
        Uuid::new_v4(),
        PlanStatus::Active,
        &[("a", TaskStatus::Completed), ("b", TaskStatus::Skipped)],
    );
    assert!(unresolved_tasks(&all_resolved).is_empty());
    assert!(
        format_plan_reminder(&all_resolved).is_none(),
        "a fully resolved plan must not be reminded"
    );

    let one_open = plan_with(
        Uuid::new_v4(),
        PlanStatus::Active,
        &[("a", TaskStatus::Completed), ("b", TaskStatus::Pending)],
    );
    assert_eq!(unresolved_tasks(&one_open).len(), 1);
    assert!(format_plan_reminder(&one_open).is_some());
}

// ---------------------------------------------------------------------------
// GoalEvidence::render
// ---------------------------------------------------------------------------

#[test]
fn render_names_every_section() {
    let evidence = GoalEvidence {
        running_tasks: vec!["cargo test (running 12s)".to_string()],
        unresolved_tasks: vec!["3. wire the call site (Pending)".to_string()],
        tool_receipts: vec!["bash: git status — ok".to_string()],
        files_touched: vec!["~/notes.md".to_string()],
        turns_used: 4,
        max_turns: 20,
    };
    let rendered = evidence.render();
    // Every header the pack declares, asserted from the declaration itself:
    // adding a section without rendering it fails here, and so does dropping
    // one the judge prompt still quotes (#364).
    for section in GoalEvidence::SECTIONS {
        assert!(rendered.contains(section), "missing section {section}");
    }
    assert!(rendered.contains("- cargo test (running 12s)"));
    assert!(rendered.contains("- 3. wire the call site (Pending)"));
    assert!(rendered.contains("- bash: git status — ok"));
    assert!(rendered.contains("- ~/notes.md"));
    assert!(rendered.contains("TURN BUDGET: 4/20 turns used"));
}

/// An empty section says so explicitly: "nothing is in flight" is the fact the
/// judge needs, and a missing section would read as unknown.
#[test]
fn render_marks_empty_sections_as_none() {
    let rendered = GoalEvidence::default().render();
    assert!(rendered.contains("RUNNING BACKGROUND TASKS:\n- (none)"));
    assert!(rendered.contains("OPEN PLAN TASKS:\n- (none)"));
    assert!(rendered.contains("TOOL RECEIPTS:\n- (none)"));
    assert!(rendered.contains("FILES TOUCHED:\n- (none)"));
    assert!(rendered.contains("TURN BUDGET: 0/0 turns used"));
}

/// The collector leaves the turn budget unset — it cannot know it — and the
/// manager attaches it from the goal row. Until then a pack honestly reads
/// `0/0`, and `with_budget` is what makes the render carry the real count.
#[test]
fn with_budget_attaches_the_turn_count() {
    let mut evidence = GoalEvidence {
        running_tasks: vec!["cargo test (running 3s)".to_string()],
        ..GoalEvidence::default()
    };
    assert_eq!(evidence.turns_used, 0);
    assert_eq!(evidence.max_turns, 0);

    evidence = evidence.with_budget(7, 20);
    assert_eq!(evidence.turns_used, 7);
    assert_eq!(evidence.max_turns, 20);
    assert!(
        evidence.render().contains("TURN BUDGET: 7/20 turns used"),
        "got: {}",
        evidence.render()
    );
    assert!(
        evidence.render().contains("cargo test (running 3s)"),
        "attaching the budget must not drop the mechanical facts"
    );
}

// ---------------------------------------------------------------------------
// build_goal_evidence
// ---------------------------------------------------------------------------

/// No manager and no plan → an empty pack, never an error. A service without a
/// manager cannot detach a command at all, so `None` honestly means "nothing is
/// running" rather than "unknown".
#[tokio::test]
async fn no_manager_and_no_plan_yields_an_empty_pack() {
    in_temp_home(async {
        let sid = Uuid::new_v4();
        let evidence = build_goal_evidence(None, sid, Vec::new(), Vec::new()).await.with_budget(2, 20);
        assert!(evidence.running_tasks.is_empty());
        assert!(evidence.unresolved_tasks.is_empty());
        assert_eq!(evidence.turns_used, 2);
        assert_eq!(evidence.max_turns, 20);
    })
    .await;
}

/// An Active plan's open tasks reach the pack.
#[tokio::test]
async fn active_plan_open_tasks_are_reported() {
    in_temp_home(async {
        let sid = Uuid::new_v4();
        let plan = plan_with(
            sid,
            PlanStatus::Active,
            &[
                ("shipped", TaskStatus::Completed),
                ("still open", TaskStatus::InProgress),
            ],
        );
        save_plan(&plan).await.expect("plan saved");

        let evidence = build_goal_evidence(None, sid, Vec::new(), Vec::new()).await;
        assert_eq!(evidence.unresolved_tasks.len(), 1);
        assert!(evidence.unresolved_tasks[0].contains("still open"));
    })
    .await;
}

/// A fully resolved Active plan reports nothing open — the goal must not be
/// held back by work that is already done.
#[tokio::test]
async fn fully_resolved_active_plan_reports_nothing_open() {
    in_temp_home(async {
        let sid = Uuid::new_v4();
        let plan = plan_with(
            sid,
            PlanStatus::Active,
            &[
                ("done", TaskStatus::Completed),
                ("dropped", TaskStatus::Skipped),
            ],
        );
        save_plan(&plan).await.expect("plan saved");

        let evidence = build_goal_evidence(None, sid, Vec::new(), Vec::new()).await;
        assert!(evidence.unresolved_tasks.is_empty());
    })
    .await;
}

/// An Editing plan is a draft awaiting approval, not outstanding work, so it
/// must not appear as evidence.
///
/// The fixture carries `pre_init_editing`, which is the shape a real draft has:
/// `load_plan_from_path` normalizes a bare Editing plan that already has tasks
/// and no design `.md` into Active, so without the flag this would not be
/// testing the draft case at all.
#[tokio::test]
async fn editing_plan_is_not_evidence() {
    in_temp_home(async {
        let sid = Uuid::new_v4();
        let mut plan = plan_with(
            sid,
            PlanStatus::Editing,
            &[("draft task", TaskStatus::Pending)],
        );
        plan.pre_init_editing = true;
        save_plan(&plan).await.expect("plan saved");

        let evidence = build_goal_evidence(None, sid, Vec::new(), Vec::new()).await;
        assert!(
            evidence.unresolved_tasks.is_empty(),
            "an unapproved draft is not outstanding work"
        );
    })
    .await;
}

/// The reminder's gate and the evidence pack's gate are the SAME predicate
/// (#299). `format_plan_reminder` refuses a pre-init plan, so the pack must too
/// — otherwise the goal is held open by a plan the reminder deliberately
/// ignores, and the two consumers disagree about whether work is outstanding.
#[tokio::test]
async fn evidence_agrees_with_the_reminder_on_a_pre_init_plan() {
    in_temp_home(async {
        let sid = Uuid::new_v4();
        let mut plan = plan_with(
            sid,
            PlanStatus::Active,
            &[("never started", TaskStatus::Pending)],
        );
        plan.pre_init_editing = true;
        save_plan(&plan).await.expect("plan saved");

        let loaded = crate::utils::plan_files::load_plan(sid)
            .await
            .expect("plan loads");
        assert!(
            format_plan_reminder(&loaded).is_none(),
            "the reminder stays silent on a pre-init plan"
        );

        let evidence = build_goal_evidence(None, sid, Vec::new(), Vec::new()).await;
        assert!(
            evidence.unresolved_tasks.is_empty(),
            "the pack must agree with the reminder, not contradict it"
        );
    })
    .await;
}

/// A detached command that is genuinely running shows up in the pack with its
/// label and elapsed time — the case the judge must treat as "not finished".
#[tokio::test]
async fn a_running_detached_command_is_reported() {
    let sid = Uuid::new_v4();
    let mgr = Arc::new(BackgroundTaskManager::new());
    mgr.clone().spawn_command(
        sid,
        std::env::temp_dir(),
        "sleep probe".to_string(),
        "sleep 5".to_string(),
    );

    let evidence = build_goal_evidence(Some(mgr.as_ref()), sid, Vec::new(), Vec::new()).await;
    assert_eq!(
        evidence.running_tasks.len(),
        1,
        "the detached command should be in flight, got: {:?}",
        evidence.running_tasks
    );
    assert!(evidence.running_tasks[0].contains("sleep probe"));
    assert!(evidence.running_tasks[0].contains("running"));
    assert!(
        evidence.render().contains("sleep probe"),
        "the pack handed to the judge must name the running command"
    );
}
