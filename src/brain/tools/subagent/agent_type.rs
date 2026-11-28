//! Sub-agent tool-registry construction.
//!
//! Typed agent roles (`AgentType`) were removed (#1173): a child is now
//! either read-restricted or not, decided solely by the parent's explicit
//! `read_only` grant at spawn time. The old per-type allow-lists and canned
//! system-prompt preambles are gone — restricted children get their tool set
//! from [`restrict_registry_to_read_only`](crate::brain::tools::plan_gate::restrict_registry_to_read_only)
//! (#649) and one factual capability line instead of role-play text.
//!
//! Deprecated `agent_type` values from the typed API still resolve through
//! [`map_deprecated_agent_type`], which maps each known value to its
//! historical *effective* grant (Explore/Research were read-only; Plan carried
//! bash, so it was write-capable). Unknown values fail closed rather than
//! silently escalating to full write access.

use crate::brain::agent::service::work_status::AgentGrant;
use crate::brain::tools::catalog;
use crate::brain::tools::plan_gate;
use crate::brain::tools::registry::ToolRegistry;
use crate::brain::tools::tool_search::ToolSearchTool;
use std::sync::Arc;
use uuid::Uuid;

/// Tools that sub-agents must NEVER have access to (prevents recursion / dangerous ops).
///
/// #129 (owner ruling C, 2026-09-07): `session_notify` and `suggest_options`
/// are in this list because a sub-agent IS a headless session — its only
/// user-visible output is the final message relayed by the harness
/// (proven live-fire 2026-09-07; a parked session_notify verdict dies with
/// the one-shot process). Children inherit the parent registry verbatim, so
/// an interactive parent's copies of these tools would leak into the child
/// without this strip.
pub const ALWAYS_EXCLUDED: &[&str] = &[
    "spawn_agent",
    "resume_agent",
    "wait_agent",
    "send_input",
    "stop_agent",
    "evolve",
    "session_notify",
    "suggest_options",
    // #852: the parent-side ledger reader. Stripped here so a child cannot read
    // a SIBLING's progress — the tool is scoped by parent session, so a child
    // holding it would see nothing new anyway, but a tool that can only fail is
    // schema noise on every child request.
    "agent_progress",
    // `report_progress` is excluded for a different reason: it is never
    // inherited, only built. Every level of `build_child_registry` registers a
    // fresh instance, so excluding the name here means the instance a child
    // holds is always the one created at ITS level — one construction site to
    // reason about, rather than an implicit reliance on the tool happening to
    // be stateless.
    "report_progress",
];

/// Build the child registry a sub-agent actually runs with (#1075, #1079).
///
/// ONE constructor for both registry-build sites — `spawn_agent` and
/// `resume_agent`. Both defects this closes were the same shape: a site that
/// re-derived one of the two restrictions and forgot the other.
///
/// - `spawn_agent` applied the #649 Editing-parent strip but recorded a grant
///   built from the caller's `read_only` alone, so the strip was invisible to
///   `resume_agent` (#1079).
/// - `resume_agent` then rebuilt from that grant alone, so a spawn-time strip
///   vanished on resume (#1079).
///
/// Order is load-bearing and matches the documented precedence: the base
/// child registry, then the caller's `read_only` grant, then the
/// Editing-parent strip. The two strips are idempotent (both driven by the
/// same `read_only` hint), so stacking them can only narrow, never widen.
///
/// `governing_session` is whose plan state decides the second strip — the
/// spawning session, or on resume the child's recorded parent — NOT the
/// child's own session, which is fresh and resolves to `NoPlan`.
///
/// `is_plan_review_worker` is the typed #155 exception. It is a parameter of
/// type `bool` fed from a deliberate grant field, never a comparison against
/// the model-authored `label`.
pub async fn build_child_registry_gated(
    parent: &ToolRegistry,
    manager: &Arc<super::manager::SubAgentManager>,
    governing_session: Uuid,
    grant: &AgentGrant,
    is_plan_review_worker: bool,
) -> Arc<ToolRegistry> {
    let child = build_child_registry(parent, manager);

    // The caller's explicit grant (#1173): a read-only child stays read-only
    // for its whole life, every resume included.
    if grant.read_only {
        plan_gate::restrict_registry_to_read_only(&child);
        tracing::info!(
            "Child registry restricted to read-only: the spawn granted \
             read_only=true (#1173)"
        );
    }

    // #649: a child of a Plan-mode Editing parent must be read-only. The child
    // runs under a fresh session that resolves to NoPlan, so the per-call plan
    // gate never fires for it — this registry strip is the only thing standing
    // between it and the parent's write-freeze.
    if plan_gate::editing_parent_strips(governing_session, is_plan_review_worker).await {
        plan_gate::restrict_registry_to_read_only(&child);
        tracing::info!(
            "Child registry restricted to read-only: the governing session \
             {governing_session} is in Plan-mode Editing (#649)"
        );
    }

    child
}

/// Build a child registry from the parent's, minus recursive/dangerous tools.
///
/// This is the FULL-access child baseline. Callers do NOT use this directly to
/// build a child's registry any more (#1075, #1079) — they call
/// [`build_child_registry_gated`], which applies the caller's `read_only` grant
/// and the Editing-parent strip on top of this baseline in one place.
///
/// Returns an [`Arc`] because callers hand the registry to `AgentService`
/// wrapped anyway — and because [`ToolSearchTool`] must be RE-BOUND to this
/// child Arc (#1210): the parent's search-tool instance captured the parent's
/// registry at construction. Copying that instance here made a child's
/// `tool_search` activate schemas in the PARENT's registry under the child's
/// session id, while the child's request builder read active tools from its
/// OWN registry — so a discovered tool's schema never rode on the child's
/// next request, and well-behaved models looped re-searching forever instead
/// of emitting calls for undeclared tools (observed live on two models across
/// two providers; see upstream #1210). Re-binding per level also keeps the
/// fix correct for nested spawns: each generation's search tool points at
/// its own registry.
pub fn build_child_registry(
    parent: &ToolRegistry,
    manager: &Arc<super::manager::SubAgentManager>,
) -> Arc<ToolRegistry> {
    let child = Arc::new(ToolRegistry::new());

    for name in parent.list_tools() {
        if ALWAYS_EXCLUDED.contains(&name.as_str()) {
            continue;
        }

        if name == catalog::TOOL_SEARCH_NAME {
            // Fresh instance bound to THIS registry, so the child's searches
            // land where the child's requests read (#1210).
            // Bound WEAKLY: the registry owns this tool, so handing it a
            // strong Arc back closes a registry -> tool -> registry cycle and
            // the child registry is never freed. One leaked registry per
            // spawn is a steep price for the rebinding.
            child.register(Arc::new(ToolSearchTool::new(&child)));
            continue;
        }

        if let Some(tool) = parent.get(&name) {
            child.register(tool);
        }
    }

    // #852: the child's own channel back to the parent. Registered AFTER the
    // copy loop and never inherited from the parent, because the tool resolves
    // its agent id from the calling session — a copy taken from an ancestor's
    // registry would bind a grandchild's reports to the child's ledger.
    child.register(Arc::new(super::progress::ReportProgressTool::new(
        manager.clone(),
    )));

    child
}

/// Resolve a deprecated `agent_type` string to its historical effective grant.
///
/// Returns `Ok(read_only)` where `read_only` reflects what the old typed role
/// actually allowed, or `Err(explanation)` for unrecognized values — failing
/// closed so a typo'd type can never silently become a full-write child.
///
/// Every call site MUST surface the deprecation loudly (warn log + schema-era
/// note in the spawn result); this function intentionally does not log, so
/// tests can assert on call-site behavior.
pub fn map_deprecated_agent_type(raw: &str) -> Result<bool, String> {
    match raw.trim().to_lowercase().as_str() {
        // Historically read-only roles (explore aliases included).
        "explore" | "search" | "find" | "research" | "web" | "lookup" => Ok(true),
        // Historically write-capable roles — note `plan` carried bash, so it
        // was NOT read-only despite its analysis-only description.
        "general" | "plan" | "architect" | "design" | "code" | "implement" | "write" => Ok(false),
        other => Err(format!(
            "Unknown agent_type '{other}'. Typed agent roles were removed \
             (#1173); pass read_only=true/false explicitly instead."
        )),
    }
}
