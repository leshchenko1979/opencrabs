//! Mechanical evidence pack for the goal judge (#299).
//!
//! The judge gets three inputs: the declared criteria, this pack, and the
//! assistant's last response. The pack exists so the judge has something
//! *checkable* to reason from — it is assembled from state the harness already
//! holds (detached commands still running, plan tasks still open, the turn
//! budget) and never from the assistant's prose. The judge is told the pack
//! outranks the last response where the two disagree, so a status report
//! claiming success cannot outvote a still-running command.
//!
//! Everything here is read-only and cannot fail: a session with no manager, no
//! plan and no running command produces an honest "nothing is in flight" pack
//! rather than an error.

use uuid::Uuid;

use crate::brain::agent::service::background_tasks::BackgroundTaskManager;
use crate::tui::plan::PlanStatus;

/// The mechanical facts one judge call is allowed to reason from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoalEvidence {
    /// Detached commands still running for this session, oldest first.
    pub running_tasks: Vec<String>,
    /// Plan tasks that are neither `Completed` nor `Skipped`.
    pub unresolved_tasks: Vec<String>,
    /// Turns the goal has consumed so far. Zero until the manager attaches the
    /// real budget with [`GoalEvidence::with_budget`] — the collector cannot
    /// know it, the goal row owns it.
    pub turns_used: u32,
    /// Turns the goal may consume before it pauses. Zero until [`Self::with_budget`].
    pub max_turns: u32,
}

impl GoalEvidence {
    /// Attach the turn budget owned by the goal row (#299).
    ///
    /// The mechanical collector knows what the harness holds (processes, plan
    /// tasks); the budget lives in `goal_state` and is only readable once the
    /// goal is loaded. So the manager — which holds the loaded goal — attaches
    /// it here, and `render()` is only ever called on a budgeted pack.
    pub fn with_budget(&self, turns_used: u32, max_turns: u32) -> Self {
        Self {
            turns_used,
            max_turns,
            ..self.clone()
        }
    }
    /// Render the pack for the judge prompt.
    ///
    /// Every section is always present, empty ones included: an absent section
    /// and an empty one read the same to a model, and "(none)" is the fact the
    /// judge needs — there is nothing in flight to prove anything with.
    pub fn render(&self) -> String {
        let mut out = String::from("RUNNING BACKGROUND TASKS:\n");
        push_lines(&mut out, &self.running_tasks);
        out.push_str("OPEN PLAN TASKS:\n");
        push_lines(&mut out, &self.unresolved_tasks);
        out.push_str(&format!(
            "TURN BUDGET: {}/{} turns used",
            self.turns_used, self.max_turns
        ));
        out
    }
}

/// Append a section's items, or `(none)` when there are none.
fn push_lines(out: &mut String, items: &[String]) {
    if items.is_empty() {
        out.push_str("- (none)\n");
        return;
    }
    for item in items {
        out.push_str("- ");
        out.push_str(item);
        out.push('\n');
    }
}

/// Collect the evidence pack for a judge call.
///
/// `background` is `None` on services built without a manager — and a service
/// without a manager cannot detach a command at all (the detach gate in `bash`
/// is `background_manager.is_some()`), so `None` honestly means "nothing can be
/// running" rather than "unknown".
///
/// The plan leg only reports an `Active` plan: an `Editing` plan is a draft
/// awaiting approval, not outstanding work, and `load_plan` already returns
/// `None` for a completed or cancelled one.
///
/// The turn budget is deliberately NOT a parameter: the collector runs before
/// the goal is loaded and cannot know it. The manager attaches it with
/// [`GoalEvidence::with_budget`] once it holds the goal row.
pub async fn build_goal_evidence(
    background: Option<&BackgroundTaskManager>,
    session_id: Uuid,
) -> GoalEvidence {
    let running_tasks = background
        .map(|bm| {
            bm.running_tasks(session_id)
                .into_iter()
                .map(|t| format!("{} (running {}s)", t.label, t.started.elapsed().as_secs()))
                .collect()
        })
        .unwrap_or_default();

    // The gate here MIRRORS `format_plan_reminder` exactly (#299): the reminder
    // refuses to nag an Editing plan and one that is still pre-init, and the
    // evidence pack must agree — a draft awaiting approval is not outstanding
    // work, and a pack that counted it would hold the goal open forever.
    let unresolved_tasks = match crate::utils::plan_files::load_plan(session_id).await {
        Some(plan) if plan.status == PlanStatus::Active && !plan.pre_init_editing => {
            crate::brain::agent::service::unresolved_tasks(&plan)
        }
        _ => Vec::new(),
    };

    GoalEvidence {
        running_tasks,
        unresolved_tasks,
        ..GoalEvidence::default()
    }
}
