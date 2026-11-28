//! SubAgentManager — tracks all spawned child agents.
//!
//! Shared across the 5 subagent tools via `Arc<SubAgentManager>`.
//! Each child agent has its own session, cancel token, output channel,
//! and input channel for mid-execution messaging.

use crate::brain::agent::service::work_status::{WorkKind, WorkState, WorkStatus};
use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Minimum gap between two "your child needs you" notifies for one agent
/// (#859). A child that reports three blockers in a row has one problem, not
/// three, and every notify lands in the parent's context as a message it has
/// to read past to find the one that mattered. The ledger entry is never
/// throttled — only the wake.
const ATTENTION_FLOOR: Duration = Duration::from_secs(30);

/// State of a spawned sub-agent.
///
/// `AwaitingInput` is the "round complete, paused for follow-up" state.
/// Without it, `wait_agent` couldn't distinguish "still working" from
/// "done with this round, output ready" — both looked like `Running`,
/// so `handle.await` always timed out (sub-agent task only terminates
/// on input channel close or cancel, never on round boundary).
#[derive(Debug, Clone, PartialEq)]
pub enum SubAgentState {
    Running,
    AwaitingInput,
    Completed,
    Failed(String),
    Cancelled,
    /// The process that owned this child died before the child reached a
    /// terminal state — the restart case (#1080).
    ///
    /// Deliberately NOT [`Self::Failed`]. `WorkState` has carried this
    /// distinction since #26, and the projection into this enum used to
    /// collapse it: the one state whose meaning is "I do not know how far it
    /// got, and its transcript is probably intact" was handed to the parent as
    /// the one that means "it broke". Measured on this box 2026-11-28: 7
    /// children were reported `Failed("interrupted by restart")` with an empty
    /// output while 4.3 MB across 35 messages sat in `opencrabs.db`, and 4 were
    /// re-spawned for work that had already been done.
    ///
    /// Not terminal for resume purposes: the child's session, transcript and
    /// grant all survive, so `resume_agent` continues it exactly as it
    /// continues a `Completed` child.
    Interrupted,
}

impl SubAgentState {
    /// Whether this state means the child is no longer doing anything.
    ///
    /// Every terminal state is here, including `Interrupted`: nothing is
    /// executing that child any more. It is NOT "failed" — see [`Self::label`]
    /// — and it is still resumable; the two questions are different.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed(_) | Self::Cancelled | Self::Interrupted
        )
    }

    /// Whether `resume_agent` accepts this state.
    ///
    /// One predicate instead of a match repeated at the gate and at
    /// [`SubAgentManager::prepare_resume`] (#1080): two copies of this list is
    /// how a state gets accepted by one and refused by the other, and the
    /// refusal reads to the parent as "the child is gone".
    pub fn is_resumable(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed(_) | Self::Cancelled | Self::Interrupted
        )
    }

    /// The word a reader sees for this state (#1080).
    ///
    /// Lives on the enum for the same reason [`WorkState::label`] does: the
    /// progress renderer, `tasks_list` and the child watchdog each used to
    /// carry their own copy of this match, so a state added to one was silently
    /// rendered as a lie by the others. `Interrupted` is where that mattered —
    /// rendered as "failed" it tells a parent its work was destroyed when the
    /// transcript is usually intact and one `resume_agent` away.
    ///
    /// For `Interrupted` the word is the whole sentence on purpose: unlike
    /// every other state it is not a verdict but a gap in knowledge, and a
    /// reader who sees only the word must not be able to read it as a fault.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::AwaitingInput => "awaiting input",
            Self::Completed => "completed",
            Self::Failed(_) => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => {
                "interrupted by a restart; context preserved — resume_agent continues it"
            }
        }
    }

    /// The sentence a reader needs to act correctly on this state (#1080).
    ///
    /// `None` for every state whose label is already the whole truth. For
    /// `Interrupted` the label carries it too, and this returns the same
    /// sentence so the ROW-rendering callers can put it on its own line where
    /// a table column has no room for it.
    pub fn advice(&self) -> Option<&'static str> {
        match self {
            Self::Interrupted => {
                Some("interrupted by a restart; context preserved — resume_agent continues it")
            }
            _ => None,
        }
    }
}

/// A spawned child agent.
pub struct SubAgent {
    /// Unique identifier for this child
    pub id: String,

    /// Human-readable label (from the prompt summary)
    pub label: String,

    /// The brief this child was given (#960). Empty until the spawn path
    /// overrides it — `SubAgent::new` cannot know it, because the prompt is
    /// assembled at the call site.
    ///
    /// Carried on the AGENT and not only on the view so the rehydrate path can
    /// restore it from `WorkStatus.task` (#960): a parent that lost its context
    /// to compaction is exactly the caller that needs to read back what it
    /// asked for, and `agent_progress` is the only surface it has.
    pub task: String,

    /// Branch of the private worktree cut for this child (#960). `None` when
    /// no tree was cut — outside a repository, or a read-only agent that keeps
    /// the parent's directory.
    ///
    /// The values are derived at spawn and were previously discarded, so a
    /// parent that wanted to review a child's work in progress had to
    /// reconstruct the naming scheme from source.
    pub worktree_branch: Option<String>,
    /// Absolute path of the tree named by [`Self::worktree_branch`] (#960).
    pub worktree_path: Option<String>,

    /// Session ID the child operates on
    pub session_id: Uuid,

    /// Session that spawned this child (#1183). The manager is process-global
    /// (one instance wired into every channel agent), so without this field a
    /// per-session surface — the Telegram settle card — cannot tell which
    /// children belong to the chat that just settled. Drives
    /// [`SubAgentManager::alive_counts_for`]. Not the child's session: that
    /// one is fresh per agent and nothing is listening on it.
    pub parent_session_id: Uuid,

    /// Whether this child was spawned with a read-restricted tool registry.
    ///
    /// Frozen for the agent's lifetime (#1173): resume must rebuild the same
    /// restriction, never widen it. External code should query via the
    /// manager's `get_read_only` instead of reaching through the lock.
    pub read_only: bool,

    /// Whether this child was spawned with workspace brain files pre-injected (#145).
    ///
    /// Frozen for the agent's lifetime: resume re-attaches system brain when true.
    pub include_brain: bool,

    /// Whether this child may spawn further sub-agents or background tasks
    /// (#1195). Frozen for the agent's lifetime like [`SubAgent::read_only`]:
    /// enforcement live-queries the manager by the caller's session id, so
    /// resumes inherit the restriction without rebuilding anything. Plan
    /// workers are spawned with this false (worker purity); root sessions
    /// are never registered here and are always unrestricted.
    pub allow_nested: bool,

    /// Whether this child is the plan-review worker (#155), the single child
    /// exempt from the #649 Editing-parent read-only strip because rewriting
    /// the plan `.md` is its job.
    ///
    /// A TYPED fact, not a label comparison (#1075): the spawn's `label` is
    /// free text the model writes, so deciding a write privilege from it let
    /// any caller mint the exception by guessing a string. Set deliberately at
    /// spawn, persisted on the grant (#1079), restored on rehydrate, and read
    /// by every registry build.
    pub plan_review: bool,

    /// Current state
    pub state: SubAgentState,

    /// Cancel token — fire to terminate the child
    pub cancel_token: CancellationToken,

    /// Join handle for the background task (None after awaited)
    pub join_handle: Option<JoinHandle<()>>,

    /// Send follow-up input to the running child
    pub input_tx: Option<mpsc::UnboundedSender<String>>,

    /// Final output collected from the child (set on completion)
    pub output: Option<String>,

    /// Timestamp when spawned
    pub spawned_at: chrono::DateTime<chrono::Utc>,

    /// How many `wait_agent` calls are currently blocked on this agent.
    ///
    /// Decides whether a finished agent pushes its result into the parent
    /// session. A waiter already receives the output as its own tool result,
    /// so pushing as well would deliver it twice; with no waiter there is
    /// nothing to receive it and the output would otherwise be dropped
    /// (#1036). Read and written only under the manager's write lock, so the
    /// terminal transition and the decision cannot interleave.
    pub waiters: usize,
}

impl SubAgent {
    /// Canonical construction: identity plus fresh running state (#1197
    /// review DRY). Per-spawn variation (`read_only`, `allow_nested`,
    /// handles, tokens) is overridden at the call site via struct-update
    /// syntax, so adding a field means touching this one place instead of
    /// once per construction site - the failure mode that turned every
    /// flag addition into a repo-wide fixture sweep.
    pub fn new(
        id: impl Into<String>,
        label: impl Into<String>,
        session_id: Uuid,
        parent_session_id: Uuid,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            task: String::new(),
            worktree_branch: None,
            worktree_path: None,
            session_id,
            parent_session_id,
            read_only: false,
            include_brain: false,
            allow_nested: true,
            // The narrow default: an ordinary child, NOT the plan-review
            // worker. Only `spawn_agent` with `plan_review: true` widens this,
            // and the field rides the persisted grant (#1075, #1079).
            plan_review: false,
            state: SubAgentState::Running,
            cancel_token: CancellationToken::new(),
            join_handle: None,
            input_tx: None,
            output: None,
            spawned_at: chrono::Utc::now(),
            waiters: 0,
        }
    }
}

/// A read-only view of an agent, live or rehydrated from disk (#852).
///
/// [`SubAgent`] owns a `JoinHandle`, an input channel and a cancel token, so it
/// is neither `Clone` nor a sensible thing to hand out from under the lock.
/// This is the copyable half: what a reader needs to answer "which agent is
/// this, and what is it doing".
#[derive(Debug, Clone)]
pub struct ResolvedAgent {
    pub id: String,
    pub label: String,
    /// The brief this child was given (#960). A parent reading back a child it
    /// spawned before a compaction has no other way to check the result against
    /// the ask.
    pub task: String,
    /// Branch of the private worktree cut for this child (#960), so a parent
    /// can review work in progress instead of guessing the naming scheme.
    pub worktree_branch: Option<String>,
    /// Absolute path of the tree named by [`Self::worktree_branch`] (#960).
    pub worktree_path: Option<String>,
    pub session_id: Uuid,
    pub parent_session_id: Uuid,
    pub read_only: bool,
    pub include_brain: bool,
    pub allow_nested: bool,
    /// Whether this agent is the plan-review worker (#155, #1075). Read from
    /// the persisted grant so a rehydrated worker keeps its one exception and
    /// every other child keeps its strip.
    pub plan_review: bool,
    pub state: SubAgentState,
    /// RFC3339, as recorded at spawn — a rehydrated agent keeps its original
    /// time rather than claiming to have just started.
    pub spawned_at: String,
}

impl ResolvedAgent {
    fn from_subagent(agent: &SubAgent) -> Self {
        Self {
            id: agent.id.clone(),
            label: agent.label.clone(),
            task: agent.task.clone(),
            worktree_branch: agent.worktree_branch.clone(),
            worktree_path: agent.worktree_path.clone(),
            session_id: agent.session_id,
            parent_session_id: agent.parent_session_id,
            read_only: agent.read_only,
            include_brain: agent.include_brain,
            allow_nested: agent.allow_nested,
            plan_review: agent.plan_review,
            state: agent.state.clone(),
            // Full precision, not `SecondsFormat::Secs`: this field is the
            // instant an agent started, and truncating it would hand every
            // reader a DIFFERENT moment than the record holds. Display
            // formatting belongs to the renderer (`elapsed_label`), which
            // only needs the instant to be true.
            spawned_at: agent.spawned_at.to_rfc3339(),
        }
    }
}

/// Manages all sub-agents for a parent agent instance.
pub struct SubAgentManager {
    agents: RwLock<HashMap<String, SubAgent>>,
    /// Last attention notify per agent id (#859). Transport state, not
    /// identity: deliberately absent from [`SubAgent`], so a rehydrated agent
    /// starts with a clean slate instead of inheriting a throttle window from
    /// a process that no longer exists.
    attention: Mutex<HashMap<String, Instant>>,
}

impl SubAgentManager {
    /// Create a new empty manager.
    pub fn new() -> Self {
        Self {
            agents: RwLock::new(HashMap::new()),
            attention: Mutex::new(HashMap::new()),
        }
    }

    /// Whether an attention notify for `id` may be sent now, recording the
    /// instant when it may (#859).
    ///
    /// `false` means one already went out inside [`ATTENTION_FLOOR`]. Nothing
    /// is lost by throttling: the entry that triggered this is in the ledger
    /// already, which is what the parent reads — only the wake is rationed.
    pub fn attention_allowed(&self, id: &str) -> bool {
        let mut last = self
            .attention
            .lock()
            .expect("subagent attention lock poisoned");
        let now = Instant::now();
        // `copied()` on purpose: the guard borrows `last`, and holding that
        // borrow across the `insert` below is a borrowck error the compiler
        // reports three lines away from the cause.
        if let Some(previous) = last.get(id).copied()
            && now.duration_since(previous) < ATTENTION_FLOOR
        {
            return false;
        }
        last.insert(id.to_string(), now);
        true
    }

    /// Generate a short agent ID (first 8 chars of a UUID).
    pub fn generate_id() -> String {
        Uuid::new_v4().to_string()[..8].to_string()
    }

    /// Register a new sub-agent.
    pub fn insert(&self, agent: SubAgent) {
        let id = agent.id.clone();
        self.agents
            .write()
            .expect("subagent manager lock poisoned")
            .insert(id, agent);
    }

    /// Get a clone of the agent's state.
    pub fn get_state(&self, id: &str) -> Option<SubAgentState> {
        self.agents
            .read()
            .expect("subagent manager lock poisoned")
            .get(id)
            .map(|a| a.state.clone())
    }

    /// Get the agent's read-only grant (#1173).
    ///
    /// `None` = no such agent; `Some(true)` = restricted for life, including
    /// every resume; `Some(false)` = full (minus always-excluded) registry.
    pub fn get_read_only(&self, id: &str) -> Option<bool> {
        self.agents
            .read()
            .expect("subagent manager lock poisoned")
            .get(id)
            .map(|a| a.read_only)
    }

    /// Whether the agent was spawned with workspace brain files pre-injected (#145).
    pub fn get_include_brain(&self, id: &str) -> Option<bool> {
        self.agents
            .read()
            .expect("subagent manager lock poisoned")
            .get(id)
            .map(|a| a.include_brain)
    }

    /// Whether the agent may spawn further sub-agents or background tasks
    /// (#1195). Same contract as [`Self::get_read_only`]: frozen for life, so a
    /// resume rebuilds the same nesting grant instead of assuming the widest.
    pub fn get_allow_nested(&self, id: &str) -> Option<bool> {
        self.agents
            .read()
            .expect("subagent manager lock poisoned")
            .get(id)
            .map(|a| a.allow_nested)
    }

    /// Whether the agent is the plan-review worker (#155, #1075).
    ///
    /// The single typed read for the Editing-parent write exception. Callers
    /// must use this rather than comparing the agent's label — the label is
    /// model-authored text and a privilege must not hang on it.
    pub fn get_plan_review(&self, id: &str) -> Option<bool> {
        self.agents
            .read()
            .expect("subagent manager lock poisoned")
            .get(id)
            .map(|a| a.plan_review)
    }

    /// May the agent operating on `session_id` spawn further sub-agents or
    /// background tasks? (#1195) Unregistered sessions (roots, main chats)
    /// are always unrestricted; registered children answer from their frozen
    /// `allow_nested` grant.
    pub fn nesting_allowed_for_session(&self, session_id: Uuid) -> bool {
        self.agents
            .read()
            .expect("subagent manager lock poisoned")
            .values()
            .find(|a| a.session_id == session_id)
            .map(|a| a.allow_nested)
            .unwrap_or(true)
    }

    /// Which agent, if any, is running on `session_id`? (#852)
    ///
    /// The reverse of [`Self::get_session_id`], and the lookup a child needs to
    /// name itself: a sub-agent knows its own session id and nothing else, so
    /// this is what turns "I am this session" into "I am this agent's ledger".
    /// Unregistered sessions (roots, main chats) return `None`.
    pub fn find_by_session(&self, session_id: Uuid) -> Option<String> {
        self.agents
            .read()
            .expect("subagent manager lock poisoned")
            .values()
            .find(|a| a.session_id == session_id)
            .map(|a| a.id.clone())
    }

    /// Resolve an agent id for `caller_parent_session_id`, rehydrating from
    /// disk if the map no longer holds it (#852).
    ///
    /// Before this, an agent that outlived the process was unreachable: the
    /// map is in memory, so `resume_agent` answered "no sub-agent found" for an
    /// agent whose status file was sitting on disk the whole time.
    ///
    /// Scoped, like every other per-session surface here: a caller only ever
    /// sees its own children, so a rehydrated agent cannot become another
    /// chat's agent (#191). Lazy — nothing calls this at boot, so a restart
    /// revives nothing that nobody asks for.
    ///
    /// Idempotent: the presence check is repeated under the write lock, because
    /// a peer may have rehydrated the same id between this call's read and its
    /// write.
    pub fn resolve(&self, id: &str, caller_parent_session_id: Uuid) -> Option<ResolvedAgent> {
        {
            let agents = self.agents.read().expect("subagent manager lock poisoned");
            if let Some(agent) = agents.get(id) {
                if agent.parent_session_id != caller_parent_session_id {
                    return None;
                }
                return Some(ResolvedAgent::from_subagent(agent));
            }
        }

        let status = WorkStatus::read(id)?;
        // A detached command carries no grant and no parent session; it is not
        // an agent and must never be rehydrated as one.
        if status.kind != WorkKind::Agent {
            return None;
        }
        // A legacy record with no recorded parent is REFUSED, not adopted: the
        // caller would otherwise gain authority over an agent it never spawned.
        let parent_session_id: Uuid = status.parent_session_id.as_deref()?.parse().ok()?;
        if parent_session_id != caller_parent_session_id {
            return None;
        }
        let session_id: Uuid = status.session_id.parse().ok()?;

        // Never `Running`: nothing is executing this agent any more, and
        // `alive_counts_for` would report a dead agent as working.
        //
        // Matched by reference: `WorkState` is not `Copy`, and moving it out
        // would leave `status` partially moved for the `effective_grant()` and
        // `label` reads below.
        let state = match &status.state {
            WorkState::Completed => SubAgentState::Completed,
            // #924: a stopped agent rehydrates as STOPPED, not failed. The
            // distinction is the whole point of the state — `resume_agent`
            // accepts both, but a caller reading the state would otherwise be
            // told its own deliberate stop was a crash.
            WorkState::Cancelled => SubAgentState::Cancelled,
            // #1080: the seventh `WorkState` finally reaches this enum. This
            // arm used to read `Interrupted => Failed("interrupted by
            // restart")`, which is the projection that made parents duplicate
            // work: `Interrupted` means "the process died, how far it got is
            // unknown", and rendering it as `Failed` asserts the work broke.
            // The transcript usually survived — 4.3 MB of it, on the box that
            // filed the issue — and `resume_agent` continues it.
            WorkState::Interrupted => SubAgentState::Interrupted,
            _ => SubAgentState::Failed("stale after restart".into()),
        };

        let grant = status.effective_grant();
        let mut agent = SubAgent::new(
            id.to_string(),
            status.label.clone(),
            session_id,
            parent_session_id,
        );
        agent.read_only = grant.read_only;
        // #960: the brief lives on disk in `WorkStatus.task`, and this branch is
        // the ONLY way a parent recovers it — the live copy died with the
        // process that spawned the child.
        agent.task = status.task.clone();
        // #960: the tree outlives the process that cut it, so a rehydrated
        // child can still be reviewed where it worked.
        agent.worktree_branch = status.worktree_branch.clone();
        agent.worktree_path = status.worktree_path.clone();
        agent.include_brain = grant.include_brain;
        agent.allow_nested = grant.allow_nested;
        // #1075/#1079: the plan-review exception rides the persisted grant, so
        // a worker rehydrated after a restart keeps the one write its job
        // needs — and every other child stays stripped. Absent from an older
        // record, `serde(default)` reads `false` (the narrow reading).
        agent.plan_review = grant.plan_review;
        agent.state = state;
        agent.spawned_at = chrono::DateTime::parse_from_rfc3339(&status.spawned_at)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now());

        let mut agents = self.agents.write().expect("subagent manager lock poisoned");
        // The view is taken from the entry the MAP ends up holding: if a peer
        // rehydrated this id between the read above and this write, the map's
        // copy is the live one and returning ours would describe an agent
        // nobody is tracking.
        let stored = agents.entry(id.to_string()).or_insert(agent);
        Some(ResolvedAgent::from_subagent(stored))
    }

    /// Get the agent's output if completed.
    pub fn get_output(&self, id: &str) -> Option<String> {
        self.agents
            .read()
            .expect("subagent manager lock poisoned")
            .get(id)
            .and_then(|a| a.output.clone())
    }

    /// Get the input sender for a running agent.
    pub fn get_input_tx(&self, id: &str) -> Option<mpsc::UnboundedSender<String>> {
        self.agents
            .read()
            .expect("subagent manager lock poisoned")
            .get(id)
            .and_then(|a| a.input_tx.clone())
    }

    /// Cancel a running or paused agent.
    pub fn cancel(&self, id: &str) -> bool {
        let mut agents = self.agents.write().expect("subagent manager lock poisoned");
        if let Some(agent) = agents.get_mut(id)
            && matches!(
                agent.state,
                SubAgentState::Running | SubAgentState::AwaitingInput
            )
        {
            agent.cancel_token.cancel();
            agent.state = SubAgentState::Cancelled;
            agent.input_tx = None;
            return true;
        }
        false
    }

    /// Take the join handle (for awaiting completion).
    pub fn take_join_handle(&self, id: &str) -> Option<JoinHandle<()>> {
        let mut agents = self.agents.write().expect("subagent manager lock poisoned");
        agents.get_mut(id).and_then(|a| a.join_handle.take())
    }

    /// Update output for a running agent without changing state.
    pub fn update_output(&self, id: &str, output: String) {
        let mut agents = self.agents.write().expect("subagent manager lock poisoned");
        if let Some(agent) = agents.get_mut(id) {
            agent.output = Some(output);
        }
    }

    /// Mark the agent as paused, waiting for a follow-up input. Pairs with
    /// `mark_running_again` when input arrives. Only transitions from
    /// `Running`; no-op if the agent already terminated.
    pub fn mark_awaiting_input(&self, id: &str) {
        let mut agents = self.agents.write().expect("subagent manager lock poisoned");
        if let Some(agent) = agents.get_mut(id)
            && agent.state == SubAgentState::Running
        {
            agent.state = SubAgentState::AwaitingInput;
        }
    }

    /// Flip back to Running when follow-up input has been consumed.
    pub fn mark_running_again(&self, id: &str) {
        let mut agents = self.agents.write().expect("subagent manager lock poisoned");
        if let Some(agent) = agents.get_mut(id)
            && agent.state == SubAgentState::AwaitingInput
        {
            agent.state = SubAgentState::Running;
        }
    }

    /// Register a `wait_agent` call as blocked on `id`.
    ///
    /// Paired with [`Self::leave_wait`]. Taken before the first state check so
    /// an agent finishing during the wait still sees the waiter and skips the
    /// push. Returns false when no such agent exists, so the caller does not
    /// leave a decrement outstanding.
    pub fn enter_wait(&self, id: &str) -> bool {
        let mut agents = self.agents.write().expect("subagent manager lock poisoned");
        match agents.get_mut(id) {
            Some(agent) => {
                agent.waiters += 1;
                true
            }
            None => false,
        }
    }

    /// Release a wait registered by [`Self::enter_wait`].
    pub fn leave_wait(&self, id: &str) {
        let mut agents = self.agents.write().expect("subagent manager lock poisoned");
        if let Some(agent) = agents.get_mut(id) {
            agent.waiters = agent.waiters.saturating_sub(1);
        }
    }

    /// Update agent state and output after completion.
    ///
    /// Returns whether the result should be pushed into the parent session:
    /// true when nobody is waiting on this agent, since then no tool result
    /// will ever carry the output and it would simply be dropped. The check
    /// happens under the same write lock as the transition, so a `wait_agent`
    /// arriving concurrently either registers first (and we do not push) or
    /// arrives after and reads the finished state directly.
    pub fn mark_completed(&self, id: &str, output: String) -> bool {
        let mut agents = self.agents.write().expect("subagent manager lock poisoned");
        match agents.get_mut(id) {
            Some(agent) => {
                agent.state = SubAgentState::Completed;
                agent.output = Some(output);
                agent.input_tx = None;
                agent.waiters == 0
            }
            None => false,
        }
    }

    /// Mark the child completed AND deliver its final output to the spawning
    /// session in one step - the single completion path for every spawner
    /// (#1197 review DRY). Delivery fires only when nobody is collecting via
    /// `wait_agent` ([`Self::mark_completed`]'s bool): a waiter reads the
    /// result inline, and pushing anyway would duplicate it in the parent
    /// chat. Returns whether the result was delivered.
    ///
    /// #1080: the terminal state reaches DISK here too. Before this the
    /// transition existed only in the in-memory map, so a child that finished
    /// after a resume read `Interrupted` to every disk-based surface
    /// (`task_output`, `tasks_list`, `agent_progress`) while it had in fact
    /// succeeded — the parent's transcript said one thing and the record it
    /// could re-read said another. Callers that already wrote the terminal
    /// record through their own handle (spawn.rs, resume.rs) are left alone:
    /// the write is skipped when the record is already `Completed`, so this is
    /// a no-op for them and the fix for a legacy record whose spawner owns no
    /// handle.
    pub fn complete_and_deliver(
        &self,
        id: &str,
        output: String,
        parent_session_id: uuid::Uuid,
        label: &str,
    ) -> bool {
        let should_push = self.mark_completed(id, output.clone());
        // A record that does not exist is not created here: a legacy spawner may
        // never have written one, and inventing a file would make it visible to
        // every disk-based surface as an agent it never was.
        if let Some(mut status) = WorkStatus::read(id)
            && status.state != WorkState::Completed
            && let Err(e) = status.mark_completed(output.clone())
        {
            tracing::error!(
                "Sub-agent {id} completed but its status could not be written, so it will \
                 keep reading as unfinished on disk: {e}"
            );
        }
        if should_push {
            crate::brain::tools::subagent::spawn::push_result(
                parent_session_id,
                label,
                id,
                Ok(&output),
            );
        }
        should_push
    }

    /// Update agent state after failure. Returns whether to push, on the same
    /// terms as [`Self::mark_completed`]: a failure nobody is waiting on is
    /// still a result the parent needs.
    pub fn mark_failed(&self, id: &str, error: String) -> bool {
        let mut agents = self.agents.write().expect("subagent manager lock poisoned");
        match agents.get_mut(id) {
            Some(agent) => {
                agent.state = SubAgentState::Failed(error);
                agent.input_tx = None;
                agent.waiters == 0
            }
            None => false,
        }
    }

    /// Re-register a completed, failed or stopped agent for resumption (new
    /// handle/token/channels).
    ///
    /// `Cancelled` is resumable (#924): a stop ends the WORK, not the agent.
    /// Its session, transcript and grant all survive, so refusing to continue
    /// one would strand exactly the context a parent is most likely to want
    /// back — it stopped the child because the direction was wrong, not
    /// because the child was worthless.
    ///
    /// `Interrupted` is resumable for the same reason and by the same evidence
    /// (#1080): the restart ended the process, and the child's session and
    /// transcript outlive it. This used to accept it only by accident, because
    /// the projection from `WorkState::Interrupted` landed in `Failed(_)`.
    /// Now that the state is its own variant, the acceptance has to be stated.
    pub fn prepare_resume(
        &self,
        id: &str,
        cancel_token: CancellationToken,
        input_tx: mpsc::UnboundedSender<String>,
    ) -> bool {
        let mut agents = self.agents.write().expect("subagent manager lock poisoned");
        if let Some(agent) = agents.get_mut(id)
            && agent.state.is_resumable()
        {
            agent.state = SubAgentState::Running;
            agent.cancel_token = cancel_token;
            agent.input_tx = Some(input_tx);
            agent.output = None;
            return true;
        }
        false
    }

    /// Set the join handle after spawning a resume task.
    pub fn set_join_handle(&self, id: &str, handle: JoinHandle<()>) {
        let mut agents = self.agents.write().expect("subagent manager lock poisoned");
        if let Some(agent) = agents.get_mut(id) {
            agent.join_handle = Some(handle);
        }
    }

    /// List all agents with their states.
    pub fn list(&self) -> Vec<(String, String, SubAgentState)> {
        self.agents
            .read()
            .expect("subagent manager lock poisoned")
            .values()
            .map(|a| (a.id.clone(), a.label.clone(), a.state.clone()))
            .collect()
    }

    /// List agents belonging to one parent session (#191).
    ///
    /// Mirrors [`Self::alive_counts_for`] for the same reason: the manager is
    /// process-global (one instance wired into every channel agent), so an
    /// unfiltered [`Self::list`] reports another chat's fan-out as this chat's
    /// in-flight work. A surface that frames those rows as *the caller's*
    /// sub-agents then makes a lane adopt — or, worse, silently suppress —
    /// work that is not its own.
    ///
    /// Unlike `alive_counts_for`, terminal agents ARE included: a caller
    /// asking what it spawned wants its finished children too.
    pub fn list_for_parent(&self, parent_session_id: Uuid) -> Vec<(String, String, SubAgentState)> {
        self.agents
            .read()
            .expect("subagent manager lock poisoned")
            .values()
            .filter(|a| a.parent_session_id == parent_session_id)
            .map(|a| (a.id.clone(), a.label.clone(), a.state.clone()))
            .collect()
    }

    /// Every agent on the roster, as a copyable view (#1056).
    ///
    /// [`Self::list`] returns a 3-tuple with no `spawned_at` and no
    /// `parent_session_id`, so a liveness reader built on it would have no
    /// clock to compare and no session to deliver to. [`Self::resolve`] carries
    /// both but refuses an id whose parent differs from the caller's, which a
    /// process-wide sweep cannot satisfy per child. This is the read that
    /// answers "what is on this roster" without a caller identity — one lock,
    /// every entry, the same view `resolve` hands out.
    pub fn resolved_all(&self) -> Vec<ResolvedAgent> {
        self.agents
            .read()
            .expect("subagent manager lock poisoned")
            .values()
            .map(ResolvedAgent::from_subagent)
            .collect()
    }

    /// How many callers are blocked in [`Self::wait_agent`] on `id` (#1056).
    ///
    /// Deliberately NOT a field on [`ResolvedAgent`]: that view is also built
    /// from a rehydrated file record, which has no waiters, so a field there
    /// would report a confident `0` for an agent that has one. `None` for an
    /// unknown id, so a caller can tell "nobody waits" from "no such agent".
    pub fn waiter_count(&self, id: &str) -> Option<usize> {
        self.agents
            .read()
            .expect("subagent manager lock poisoned")
            .get(id)
            .map(|a| a.waiters)
    }

    /// Alive-agent counts for one parent session (#1183): `(working,
    /// awaiting)` — children mid-round (`Running`) vs parked at a round
    /// boundary with output ready to collect (`AwaitingInput`). Terminal
    /// agents never count, and neither do children spawned by OTHER sessions:
    /// the settle card is per-chat while the manager is process-global, so an
    /// unfiltered count would report another chat's fan-out as this chat's
    /// pending work.
    pub fn alive_counts_for(&self, parent_session_id: Uuid) -> (usize, usize) {
        self.agents
            .read()
            .expect("subagent manager lock poisoned")
            .values()
            .filter(|a| a.parent_session_id == parent_session_id)
            .fold((0, 0), |(working, awaiting), a| match a.state {
                SubAgentState::Running => (working + 1, awaiting),
                SubAgentState::AwaitingInput => (working, awaiting + 1),
                _ => (working, awaiting),
            })
    }

    /// Check if an agent exists.
    pub fn exists(&self, id: &str) -> bool {
        self.agents
            .read()
            .expect("subagent manager lock poisoned")
            .contains_key(id)
    }

    /// Get the session_id for a sub-agent (needed for resume).
    pub fn get_session_id(&self, id: &str) -> Option<Uuid> {
        self.agents
            .read()
            .expect("subagent manager lock poisoned")
            .get(id)
            .map(|a| a.session_id)
    }

    /// Get the label for a sub-agent (needed for result delivery on paths
    /// other than spawn — #1197).
    pub fn get_label(&self, id: &str) -> Option<String> {
        self.agents
            .read()
            .expect("subagent manager lock poisoned")
            .get(id)
            .map(|a| a.label.clone())
    }

    /// Get the parent session for a sub-agent (needed for result delivery
    /// on resume completion — #1197).
    pub fn get_parent_session_id(&self, id: &str) -> Option<Uuid> {
        self.agents
            .read()
            .expect("subagent manager lock poisoned")
            .get(id)
            .map(|a| a.parent_session_id)
    }

    /// Format running/paused sub-agents as a compaction preamble block.
    ///
    /// Returns `None` when there are no non-terminal agents. The block is
    /// appended to the compaction summary so the post-compaction agent can
    /// still call `wait_agent`, `send_input`, `resume_agent`, and
    /// `stop_agent` on them (#936).
    pub fn format_running_for_compaction(&self) -> Option<String> {
        let active: Vec<(String, String, SubAgentState, Uuid)> = {
            let agents = self.agents.read().expect("subagent manager lock poisoned");
            agents
                .values()
                .filter(|a| {
                    matches!(
                        a.state,
                        SubAgentState::Running | SubAgentState::AwaitingInput
                    )
                })
                .map(|a| (a.id.clone(), a.label.clone(), a.state.clone(), a.session_id))
                .collect()
        };
        if active.is_empty() {
            return None;
        }
        let mut block = String::from(
            "## Running Sub-Agents\n\n\
             The following sub-agents were alive when compaction fired. \
             They are still running in the background. Use the IDs below with \
             `wait_agent`, `send_input`, `resume_agent`, or `stop_agent`. \
             Do NOT spawn duplicates.\n\n\
             | Agent ID | Label | State | Session ID |\n\
             |----------|-------|-------|------------|\n",
        );
        for (id, label, state, session_id) in &active {
            let state_str = match state {
                SubAgentState::Running => "Running",
                SubAgentState::AwaitingInput => "AwaitingInput",
                _ => continue,
            };
            block.push_str(&format!(
                "| `{id}` | {label} | {state_str} | `{session_id}` |\n",
            ));
        }
        Some(block)
    }
}

impl Default for SubAgentManager {
    fn default() -> Self {
        Self::new()
    }
}
