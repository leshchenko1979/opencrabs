//! Mechanical evidence pack for the goal judge (#299).
//!
//! The judge gets three inputs: the declared criteria, this pack, and the
//! assistant's last response. The pack exists so the judge has something
//! *checkable* to reason from — it is assembled from state the harness already
//! holds (detached commands still running, plan tasks still open, the tools that
//! ran this turn and whether they succeeded, the files they wrote, the turn
//! budget) and never from the assistant's prose. The judge is told the pack
//! outranks the last response where the two disagree, so a status report
//! claiming success cannot outvote a still-running command.
//!
//! Everything here is read-only and cannot fail: a session with no manager, no
//! plan and no tool call produces an honest "nothing is in flight" pack rather
//! than an error.
//!
//! #364 added the two receipt legs. Before it the prompt advertised five
//! sections and the pack rendered three, so every action criterion came back
//! `NO_EVIDENCE` — the judge was reasoning correctly from a pack that could not
//! carry the receipts it was told to expect.

use uuid::Uuid;

use crate::brain::agent::service::background_tasks::BackgroundTaskManager;
use crate::tui::plan::PlanStatus;
use crate::utils::string::truncate_str;

/// How many tool receipts a rendered pack carries (#364).
///
/// The pack goes into every judge prompt, so it is bounded: a turn that runs
/// fifty commands must not inflate the call. The MOST RECENT receipts are kept
/// — a criterion is usually about what just ran, and the earliest calls in a
/// long turn are the ones the assistant has already built on.
pub const MAX_TOOL_RECEIPTS: usize = 40;

/// How many touched files a rendered pack carries (#364).
pub const MAX_FILES_TOUCHED: usize = 40;

/// Byte cap for one receipt or path line (#364).
///
/// A receipt is a single command label; one long enough to blow past this is a
/// minified script or a heredoc, and its tail is not what the judge needs.
pub const MAX_RECEIPT_BYTES: usize = 240;

/// The mechanical facts one judge call is allowed to reason from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoalEvidence {
    /// Detached commands still running for this session, oldest first.
    pub running_tasks: Vec<String>,
    /// Plan tasks that are neither `Completed` nor `Skipped`.
    pub unresolved_tasks: Vec<String>,
    /// One line per tool call this turn, oldest first — see [`receipt_line`].
    pub tool_receipts: Vec<String>,
    /// Paths written or edited this turn, deduped — see [`merge_files_touched`].
    pub files_touched: Vec<String>,
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

    /// The section headers `render()` emits, in order (#364).
    ///
    /// This is the contract `JUDGE_SYSTEM` quotes: the prompt names these
    /// sections and nothing else, and a test pins the two together so neither
    /// can advertise a leg the other does not carry. Keeping the list here
    /// rather than in the prompt means a new leg is added in one place.
    pub const SECTIONS: [&'static str; 5] = [
        "RUNNING BACKGROUND TASKS:",
        "OPEN PLAN TASKS:",
        "TOOL RECEIPTS:",
        "FILES TOUCHED:",
        "TURN BUDGET:",
    ];

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
        out.push_str("TOOL RECEIPTS:\n");
        push_capped_lines(&mut out, &self.tool_receipts, MAX_TOOL_RECEIPTS, "receipt(s)");
        out.push_str("FILES TOUCHED:\n");
        push_capped_lines(&mut out, &self.files_touched, MAX_FILES_TOUCHED, "file(s)");
        out.push_str(&format!(
            "TURN BUDGET: {}/{} turns used",
            self.turns_used, self.max_turns
        ));
        out
    }
}

/// Append a section's items, or `(none)` when there are none.
fn push_lines(out: &mut String, items: &[String]) {
    push_capped_lines(out, items, usize::MAX, "");
}

/// As [`push_lines`], but bounded to the most recent `cap` entries (#364).
///
/// The dropped count is stated in the pack rather than silently swallowed: a
/// judge shown 40 receipts when there were 90 would read the pack as the whole
/// turn, and the whole point of the evidence channel is that it does not lie
/// about what it holds.
fn push_capped_lines(out: &mut String, items: &[String], cap: usize, noun: &str) {
    if items.is_empty() {
        out.push_str("- (none)\n");
        return;
    }
    let omitted = items.len().saturating_sub(cap);
    if omitted > 0 {
        out.push_str(&format!("- … and {} earlier {} omitted\n", omitted, noun));
    }
    for item in &items[omitted..] {
        out.push_str("- ");
        out.push_str(&truncate_line(item));
        out.push('\n');
    }
}

/// Cap one line on a char boundary so a long command cannot dominate the pack.
fn truncate_line(item: &str) -> String {
    if item.len() <= MAX_RECEIPT_BYTES {
        return item.to_string();
    }
    format!("{}…", truncate_str(item, MAX_RECEIPT_BYTES))
}

/// One receipt line: the tool's own summary plus whether it succeeded (#364).
///
/// The summary is produced by exactly one function (`format_tool_summary`), so
/// this line is what carries the tool's identity into the pack — `bash: <label>`
/// for a command, `Read <path>` for a read, and so on. Success is the tool's own
/// verdict, not the assistant's: a failed command is a receipt too, and a
/// `FAILED` line is what lets the judge mark a criterion UNMET instead of
/// merely unevidenced.
pub fn receipt_line(description: &str, success: bool) -> String {
    if success {
        format!("{} — ok", description)
    } else {
        format!("{} — FAILED", description)
    }
}

/// Extract the paths a turn's tool summaries report as written or edited (#364).
///
/// `format_tool_summary` is the single producer of these prefixes — `Write `
/// for `write_file`, `Edit ` for `edit_file` — so this leg can only go empty if
/// that function changes shape; `goal_evidence_receipts_test` pins the prefixes
/// so such an edit fails a test instead of silently emptying the section.
///
/// Deduped in call order: a file edited five times is one touched file.
pub fn files_touched_from(descriptions: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for description in descriptions {
        let path = description
            .strip_prefix("Write ")
            .or_else(|| description.strip_prefix("Edit "));
        if let Some(path) = path
            && !out.iter().any(|p| p == path)
        {
            out.push(path.to_string());
        }
    }
    out
}

/// Extend an accumulating touched-file list from this iteration's tool
/// summaries, preserving call order and dropping duplicates (#364).
///
/// The loop calls this once per iteration, so a file edited across three
/// iterations appears once — the judge needs the set of files touched, not a
/// count of writes.
pub fn merge_files_touched(target: &mut Vec<String>, descriptions: &[String]) {
    for path in files_touched_from(descriptions) {
        if !target.iter().any(|p| p == &path) {
            target.push(path);
        }
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
/// The two receipt legs are PARAMETERS (#364), not something this collector
/// looks up: they are turn-scoped, and only the tool loop holds them. The
/// collector is called from the judge hook, which is where the loop hands them
/// over — reaching into the harness from here would make the pack depend on
/// state the collector cannot see.
///
/// The turn budget is deliberately NOT a parameter: the collector runs before
/// the goal is loaded and cannot know it. The manager attaches it with
/// [`GoalEvidence::with_budget`] once it holds the goal row.
pub async fn build_goal_evidence(
    background: Option<&BackgroundTaskManager>,
    session_id: Uuid,
    tool_receipts: Vec<String>,
    files_touched: Vec<String>,
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
        tool_receipts,
        files_touched,
        ..GoalEvidence::default()
    }
}
