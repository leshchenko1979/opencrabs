//! Tests for the plan hold (#834): an Active checklist with open tasks holds
//! the turn open on its own, bounded so that a model which cannot advance the
//! checklist ends the turn instead of being re-prompted forever.
//!
//! The defect this pins: `evaluate_after_turn` returns `Done { "no active
//! goal" }` above the mechanical hold that reads `evidence.unresolved_tasks`,
//! and a plan never creates a goal — so the evidence was collected every turn
//! and then discarded by an early return. What remains today is a prose
//! reminder nothing enforces.
//!
//! Two units are covered here, because together they are the behaviour:
//! `plan_hold_allowed` decides WHETHER to re-enter the loop (turn-local state),
//! `format_plan_hold` renders WHAT the model is told (pure).

use crate::brain::agent::service::context::format_plan_hold;
use crate::brain::agent::service::tool_loop::plan_hold_allowed;
use crate::brain::goal::evidence::GoalEvidence;

fn tasks(n: usize) -> Vec<String> {
    (1..=n).map(|i| format!("{i}. task {i} (pending)")).collect()
}

/// The first nudge is always allowed: a checklist that just went Active has
/// open tasks and no baseline, and that is the case the whole feature exists
/// for. Without this the turn would still end on the first text-only response.
#[test]
fn first_hold_fires() {
    assert!(plan_hold_allowed(&tasks(3), None));
    assert!(plan_hold_allowed(&tasks(1), None));
}

/// The bound: a second hold with the same open count is refused. Nothing was
/// completed since the last nudge, so re-entering the loop would buy the same
/// stall again — the turn ends and the user sees the model's own text.
#[test]
fn second_hold_with_unchanged_count_does_not_fire() {
    let open = tasks(3);
    let baseline = Some(open.len());
    assert!(
        !plan_hold_allowed(&open, baseline),
        "an unchanged open count must not hold again"
    );
}

/// A completed task pays for the next hold. `complete` lowers the count and
/// `start` does not, so the count is a progress signal rather than an activity
/// signal — which is exactly why it can carry the bound.
#[test]
fn second_hold_after_a_completed_task_fires() {
    let before = tasks(3);
    let after = tasks(2);
    assert!(
        plan_hold_allowed(&after, Some(before.len())),
        "a decreased open count must hold again"
    );
}

/// No open tasks, no hold. `build_goal_evidence` yields an empty vector for a
/// session with no plan, a plan in `Editing`, a pre-init design draft and a
/// finished checklist — every one of those must end its turn exactly as it does
/// today, so the emptiness check is the first condition and not an assumption.
#[test]
fn empty_list_never_fires() {
    assert!(!plan_hold_allowed(&[], None));
    assert!(!plan_hold_allowed(&[], Some(0)));
    assert!(!plan_hold_allowed(&[], Some(4)));
    assert!(
        format_plan_hold(&[]).is_none(),
        "the renderer must produce nothing to inject"
    );
}

/// #567's shape: a session waiting on a running detached command has evidence
/// but no open plan tasks. The hold must stay silent — the goal machinery's
/// `Deferred` arm owns that case, and a plan hold here would re-prompt a model
/// that is correctly waiting.
#[test]
fn running_task_with_no_open_plan_tasks_never_fires() {
    let evidence = GoalEvidence {
        running_tasks: vec!["oc-deploy ship (running 412s)".to_string()],
        ..GoalEvidence::default()
    };
    assert!(evidence.unresolved_tasks.is_empty());
    assert!(!plan_hold_allowed(&evidence.unresolved_tasks, None));
}

/// The rendered prompt names every open task and carries the honest exit. The
/// exit sentence is load-bearing: if saying "blocked" is more expensive than
/// inventing progress, the hold becomes a re-reporting loop.
#[test]
fn rendered_hold_names_every_open_task() {
    let open = tasks(3);
    let text = format_plan_hold(&open).expect("non-empty input must render");
    for task in &open {
        assert!(text.contains(task), "missing {task:?} in:\n{text}");
    }
    assert!(text.contains("3 tasks open"), "got:\n{text}");
    assert!(
        text.contains("say what blocks you and stop"),
        "the honest exit must be offered:\n{text}"
    );
    assert!(
        text.contains("not from the user"),
        "the harness attribution must survive:\n{text}"
    );
}

/// Singular wording, so a one-task hold does not read as machine-generated
/// boilerplate the model can dismiss.
#[test]
fn single_open_task_reads_as_one_task() {
    let text = format_plan_hold(&tasks(1)).expect("non-empty input must render");
    assert!(text.contains("1 task open"), "got:\n{text}");
}
