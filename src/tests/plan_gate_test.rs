//! Dedicated coverage for the plan-mode tool gate
//! (`brain::tools::plan_gate`), driven by MCP-style [`ToolHints`].
//! Verifies, per state:
//!
//! - pre-init Editing allows read-only tools, sends mutators to
//!   RequireApproval
//! - post-init Editing sends mutators to RequireApproval, allows
//!   writes ONLY to the session `.md`
//! - Active freezes the `.md` against generic write tools; the seed
//!   window hard-denies mutators
//! - NoPlan allows everything except a write to the session `.md` (with no
//!   plan there is no JSON twin to mirror the prose into, so the file would
//!   be an orphan `/discard` never clears)
//!
//! The gate is pure: `hints.read_only` is the sole signal. No name
//! lists, no capability branching.

use crate::brain::tools::plan_gate::check_plan_gate;
use crate::brain::tools::r#trait::ToolHints;
use crate::config::profile::{home_for_profile, with_profile_home_async};
use crate::tui::plan::{PlanDocument, PlanStatus, PlanTask, TaskType};
use crate::utils::plan_files::{
    PlanModeState, create_design_md, plan_md_path, plan_mode_state, save_plan, set_pre_init_editing,
};
use serde_json::json;
use uuid::Uuid;

/// Read-only hint (read_file, grep, web_search, plan, etc.).
fn ro() -> ToolHints {
    ToolHints {
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    }
}

/// Mutator hint (bash, edit_file, telegram_send, spawn_agent, etc.).
fn mut_tool() -> ToolHints {
    ToolHints {
        read_only: false,
        destructive: true,
        idempotent: false,
        open_world: true,
    }
}

async fn in_temp_home<F, T>(f: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let profile = format!("plan-gate-test-{}", Uuid::new_v4());
    let out = with_profile_home_async(Some(&profile), f).await;
    let home = home_for_profile(Some(&profile));
    let _ = std::fs::remove_dir_all(&home);
    out
}

async fn make_post_init_editing(sid: Uuid) {
    let plan = PlanDocument::new(sid, "Design".to_string());
    save_plan(&plan).await.unwrap();
    create_design_md(sid, "Design").await.unwrap();
}

async fn make_active(sid: Uuid, with_md: bool) {
    let mut plan = PlanDocument::new(sid, "Exec".to_string());
    let mut task = PlanTask::new(1, "t1".to_string(), "d".to_string(), TaskType::Edit);
    // A started checklist: the seed window (all tasks Pending) has its own
    // stricter policy, covered separately below.
    task.start();
    plan.add_task(task);
    plan.status = PlanStatus::Active;
    save_plan(&plan).await.unwrap();
    if with_md {
        create_design_md(sid, "Exec").await.unwrap();
    }
}

#[tokio::test]
async fn no_plan_allows_all_but_session_md() {
    in_temp_home(async {
        let sid = Uuid::new_v4();
        for (name, hints) in [
            ("edit_file", &mut_tool()),
            ("bash", &mut_tool()),
            ("telegram_send", &mut_tool()),
            ("spawn_agent", &mut_tool()),
        ] {
            assert!(
                check_plan_gate(sid, name, hints, &json!({}))
                    .await
                    .is_allowed(),
                "{name} must pass with no plan"
            );
        }

        // A write aimed at the session plan .md has no JSON twin to mirror
        // into while no plan is live: the prose would be an orphan that the
        // card renders from and `/discard` reports as "no live plan" without
        // ever clearing. The .md is created by `plan` init, with its JSON.
        let md = plan_md_path(sid).await;
        for input in [
            json!({ "path": md.to_string_lossy() }),
            json!({ "file_path": md.to_string_lossy() }),
        ] {
            let decision = check_plan_gate(sid, "edit_file", &mut_tool(), &input).await;
            assert!(
                decision.is_denied(),
                "writing the session .md with no plan must be denied ({input}), got {decision:?}"
            );
        }

        // Read-only tools stay untouched even when they name the .md.
        assert!(
            check_plan_gate(
                sid,
                "read_file",
                &ro(),
                &json!({ "path": md.to_string_lossy() })
            )
            .await
            .is_allowed(),
            "read_file must stay allowed with no plan"
        );
    })
    .await;
}

#[tokio::test]
async fn pre_init_gates_mutators_allows_reads_and_plan() {
    in_temp_home(async {
        let sid = Uuid::new_v4();
        set_pre_init_editing(sid).await.unwrap();

        // Mutators go to RequireApproval in pre-init Editing.
        for (name, hints) in [
            ("edit_file", &mut_tool()),
            ("bash", &mut_tool()),
            ("execute_code", &mut_tool()),
            ("telegram_send", &mut_tool()),
            ("browser_click", &mut_tool()),
            ("spawn_agent", &mut_tool()),
            ("evolve", &mut_tool()),
        ] {
            assert!(
                check_plan_gate(sid, name, hints, &json!({}))
                    .await
                    .needs_approval(),
                "{name} must require approval pre-init"
            );
        }

        // Brain-file writes are mutators too.
        let deny = check_plan_gate(
            sid,
            "write_opencrabs_file",
            &mut_tool(),
            &json!({"path": "MEMORY.md"}),
        )
        .await;
        assert!(
            deny.needs_approval(),
            "opencrabs write must require approval pre-init"
        );

        // Read-only tools flow through.
        for (name, hints) in [("read_file", &ro()), ("web_search", &ro()), ("plan", &ro())] {
            assert!(
                check_plan_gate(sid, name, hints, &json!({}))
                    .await
                    .is_allowed(),
                "{name} must be allowed pre-init"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn post_init_gates_mutators_to_md_only() {
    in_temp_home(async {
        let sid = Uuid::new_v4();
        make_post_init_editing(sid).await;
        let md = plan_md_path(sid).await;

        // Mutators go to approval (RequireApproval), not hard deny.
        for (name, hints) in [
            ("bash", &mut_tool()),
            ("execute_code", &mut_tool()),
            ("slack_send", &mut_tool()),
            ("browser_eval", &mut_tool()),
            ("resume_agent", &mut_tool()),
            ("cron_manage", &mut_tool()),
        ] {
            assert!(
                check_plan_gate(sid, name, hints, &json!({}))
                    .await
                    .needs_approval(),
                "{name} must require approval post-init"
            );
        }

        // The session .md is the only writable file for a mutator.
        let ok = check_plan_gate(
            sid,
            "write_file",
            &mut_tool(),
            &json!({"path": md.to_string_lossy()}),
        )
        .await;
        assert!(ok.is_allowed(), "session .md write must pass, got: {ok:?}");

        // edit_file on the .md passes too (path key is shared).
        assert!(
            check_plan_gate(
                sid,
                "edit_file",
                &mut_tool(),
                &json!({"path": md.to_string_lossy()})
            )
            .await
            .is_allowed()
        );

        // Project writes go to approval.
        assert!(
            check_plan_gate(
                sid,
                "edit_file",
                &mut_tool(),
                &json!({"path": "/tmp/project/main.rs"})
            )
            .await
            .needs_approval()
        );

        // Other ~/.opencrabs writes go to approval too.
        assert!(
            check_plan_gate(
                sid,
                "write_opencrabs_file",
                &mut_tool(),
                &json!({"path": "MEMORY.md"})
            )
            .await
            .needs_approval()
        );

        // A mutator with no recognizable target goes to approval.
        assert!(
            check_plan_gate(sid, "generate_document", &mut_tool(), &json!({}))
                .await
                .needs_approval()
        );

        // Read-only tools flow through.
        for (name, hints) in [("read_file", &ro()), ("grep", &ro()), ("plan", &ro())] {
            assert!(
                check_plan_gate(sid, name, hints, &json!({}))
                    .await
                    .is_allowed(),
                "{name} must be allowed post-init"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn active_freezes_md_only() {
    in_temp_home(async {
        let sid = Uuid::new_v4();
        make_active(sid, true).await;
        let md = plan_md_path(sid).await;

        // The design .md is frozen against generic write tools.
        assert!(
            check_plan_gate(
                sid,
                "edit_file",
                &mut_tool(),
                &json!({"path": md.to_string_lossy()})
            )
            .await
            .is_denied(),
            "Active .md write must be frozen"
        );

        // Everything else follows the normal approval policy.
        assert!(
            check_plan_gate(
                sid,
                "edit_file",
                &mut_tool(),
                &json!({"path": "/tmp/project/main.rs"})
            )
            .await
            .is_allowed()
        );
        assert!(
            check_plan_gate(sid, "bash", &mut_tool(), &json!({"command": "ls"}))
                .await
                .is_allowed()
        );
        assert!(
            check_plan_gate(sid, "telegram_send", &mut_tool(), &json!({}))
                .await
                .is_allowed()
        );
        assert!(
            check_plan_gate(sid, "spawn_agent", &mut_tool(), &json!({}))
                .await
                .is_allowed()
        );
    })
    .await;
}

#[tokio::test]
async fn active_checklist_without_md_gates_nothing_on_writes() {
    in_temp_home(async {
        // Checklist-track plans have no design .md; nothing to freeze.
        let sid = Uuid::new_v4();
        make_active(sid, false).await;
        assert!(
            check_plan_gate(
                sid,
                "edit_file",
                &mut_tool(),
                &json!({"path": "/tmp/project/main.rs"})
            )
            .await
            .is_allowed()
        );
        assert!(
            check_plan_gate(sid, "bash", &mut_tool(), &json!({"command": "ls"}))
                .await
                .is_allowed()
        );
    })
    .await;
}

#[tokio::test]
async fn seed_window_blocks_mutators_allows_plan_and_reads() {
    in_temp_home(async {
        // Approved design plan whose checklist has not started yet (empty
        // tasks): only read-only tools and the plan tool flow through.
        let sid = Uuid::new_v4();
        let mut plan = PlanDocument::new(sid, "Seeding".to_string());
        plan.status = PlanStatus::Active;
        save_plan(&plan).await.unwrap();
        create_design_md(sid, "Seeding").await.unwrap();

        assert!(
            check_plan_gate(sid, "plan", &ro(), &json!({"operation": "add_tasks"}))
                .await
                .is_allowed()
        );
        assert!(
            check_plan_gate(sid, "read_file", &ro(), &json!({}))
                .await
                .is_allowed()
        );
        // Mutators are hard-denied in the seed window.
        assert!(
            check_plan_gate(sid, "bash", &mut_tool(), &json!({"command": "ls"}))
                .await
                .is_denied()
        );
        assert!(
            check_plan_gate(
                sid,
                "edit_file",
                &mut_tool(),
                &json!({"path": "/tmp/p/main.rs"})
            )
            .await
            .is_denied()
        );
        assert!(
            check_plan_gate(sid, "spawn_agent", &mut_tool(), &json!({}))
                .await
                .is_denied(),
            "spawn must be blocked in seed window (mutator)"
        );

        // Partial seed (tasks added, none started) stays blocked too.
        let mut plan = crate::utils::plan_files::load_plan(sid).await.unwrap();
        plan.add_task(PlanTask::new(
            1,
            "t1".to_string(),
            "d".to_string(),
            TaskType::Edit,
        ));
        save_plan(&plan).await.unwrap();
        assert!(
            check_plan_gate(sid, "bash", &mut_tool(), &json!({"command": "ls"}))
                .await
                .is_denied()
        );

        // Once a task starts, the seed window closes and normal Active
        // policy applies (only the .md stays frozen).
        let mut plan = crate::utils::plan_files::load_plan(sid).await.unwrap();
        plan.tasks[0].start();
        save_plan(&plan).await.unwrap();
        assert!(
            check_plan_gate(sid, "bash", &mut_tool(), &json!({"command": "ls"}))
                .await
                .is_allowed()
        );
        assert!(
            check_plan_gate(
                sid,
                "edit_file",
                &mut_tool(),
                &json!({"path": "/tmp/p/main.rs"})
            )
            .await
            .is_allowed()
        );
    })
    .await;
}

// ── restrict_registry_to_read_only (#649) ───────────────────────────────
// A subagent spawned while the parent is Editing must be read-only. The
// filter strips non-read-only tools from the child registry via the MCP
// `read_only` hint (child runs under a fresh NoPlan session the per-call
// gate would not catch), keeping only read/search/network tools.

struct CapTool {
    name: &'static str,
    caps: Vec<crate::brain::tools::r#trait::ToolCapability>,
}

#[async_trait::async_trait]
impl crate::brain::tools::Tool for CapTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "cap test tool"
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({ "type": "object" })
    }
    fn capabilities(&self) -> Vec<crate::brain::tools::r#trait::ToolCapability> {
        self.caps.clone()
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: &crate::brain::tools::ToolExecutionContext,
    ) -> crate::brain::tools::error::Result<crate::brain::tools::ToolResult> {
        Ok(crate::brain::tools::ToolResult::success("ok".to_string()))
    }
}

#[test]
fn read_only_filter_strips_mutators_keeps_reads() {
    use crate::brain::tools::ToolRegistry;
    use crate::brain::tools::plan_gate::restrict_registry_to_read_only;
    use crate::brain::tools::r#trait::ToolCapability;
    use std::sync::Arc;

    let registry = ToolRegistry::new();
    // Read-only surface (no destructive caps) must survive.
    registry.register(Arc::new(CapTool {
        name: "read_file",
        caps: vec![ToolCapability::ReadFiles],
    }));
    registry.register(Arc::new(CapTool {
        name: "grep",
        caps: vec![ToolCapability::ReadFiles],
    }));
    registry.register(Arc::new(CapTool {
        name: "http_request",
        caps: vec![ToolCapability::Network],
    }));
    registry.register(Arc::new(CapTool {
        name: "suggest_options",
        caps: vec![],
    }));
    // Mutators (destructive trio) must be stripped by the read_only hint.
    registry.register(Arc::new(CapTool {
        name: "edit_file",
        caps: vec![ToolCapability::WriteFiles],
    }));
    registry.register(Arc::new(CapTool {
        name: "bash",
        caps: vec![ToolCapability::ExecuteShell],
    }));
    registry.register(Arc::new(CapTool {
        name: "spawn_agent",
        caps: vec![ToolCapability::SystemModification],
    }));

    restrict_registry_to_read_only(&registry);

    assert!(registry.has_tool("read_file"), "reads must survive");
    assert!(registry.has_tool("grep"), "search must survive");
    assert!(
        registry.has_tool("http_request"),
        "network read must survive"
    );
    assert!(
        registry.has_tool("suggest_options"),
        "the suggestion tool must survive"
    );
    assert!(!registry.has_tool("edit_file"), "writes must be stripped");
    assert!(!registry.has_tool("bash"), "bash must be stripped");
    assert!(
        !registry.has_tool("spawn_agent"),
        "spawn must be stripped so no non-restricted grandchild can be minted"
    );
}

// ── build_child_registry_gated (#1075, #1079) ────────────────────────────
//
// One shared constructor for every child registry. The three escalation paths
// this closes were all the same shape — a site that re-derived ONE of the two
// restrictions and forgot the other:
//
// - #1075: `team_create` never asked the parent's plan state, so a team member
//   of an Editing parent got write+bash while a spawned child of the same
//   parent was stripped.
// - #1079: `resume_agent` rebuilt from a grant that recorded only the caller's
//   `read_only`, so a spawn-time strip vanished on resume.
//
// The tests below drive the constructor directly: that is where the rule now
// lives, and a test through `team_create`/`resume_agent` would need a provider
// and a live spawn. The call sites are pinned separately, below.

/// A parent registry with one read tool and two mutators, which is the smallest
/// surface that can tell "stripped" from "full".
fn gated_parent_registry() -> crate::brain::tools::ToolRegistry {
    use crate::brain::tools::r#trait::ToolCapability;
    use std::sync::Arc;

    let registry = crate::brain::tools::ToolRegistry::new();
    registry.register(Arc::new(CapTool {
        name: "read_file",
        caps: vec![ToolCapability::ReadFiles],
    }));
    registry.register(Arc::new(CapTool {
        name: "bash",
        caps: vec![ToolCapability::ExecuteShell],
    }));
    registry.register(Arc::new(CapTool {
        name: "write_file",
        caps: vec![ToolCapability::WriteFiles],
    }));
    registry
}

fn assert_stripped(registry: &crate::brain::tools::ToolRegistry, why: &str) {
    assert!(
        registry.has_tool("read_file"),
        "{why}: reads must survive the strip"
    );
    assert!(
        !registry.has_tool("bash"),
        "{why}: bash must be stripped (#649)"
    );
    assert!(
        !registry.has_tool("write_file"),
        "{why}: writes must be stripped (#649)"
    );
}

fn assert_full(registry: &crate::brain::tools::ToolRegistry, why: &str) {
    assert!(registry.has_tool("read_file"), "{why}: reads present");
    assert!(registry.has_tool("bash"), "{why}: bash present");
    assert!(registry.has_tool("write_file"), "{why}: writes present");
}

#[tokio::test]
async fn team_member_under_an_editing_parent_is_stripped() {
    // #1075: the parent's plan state governs, and the member's own fresh
    // session resolves to NoPlan — which is exactly why the strip has to be
    // asked of the GOVERNING session, not the child's.
    in_temp_home(async {
        let parent_sid = Uuid::new_v4();
        make_post_init_editing(parent_sid).await;

        let parent = gated_parent_registry();
        let manager = std::sync::Arc::new(crate::brain::tools::subagent::SubAgentManager::new());
        // The caller granted FULL access; only the plan state narrows it.
        let grant = crate::brain::agent::service::work_status::AgentGrant::new(false, false, true);
        let child = crate::brain::tools::subagent::build_child_registry_gated(
            &parent, &manager, parent_sid, &grant, false,
        )
        .await;

        assert_stripped(
            &child,
            "a team member of a Plan-mode Editing parent (#1075)",
        );
    })
    .await;
}

#[tokio::test]
async fn resumed_child_under_an_editing_parent_is_stripped() {
    // #1079: the persisted grant says `read_only: false` (the spawn-time strip
    // was never recorded), so only re-derivation against the PARENT's plan
    // state can keep the child restricted.
    in_temp_home(async {
        let parent_sid = Uuid::new_v4();
        make_post_init_editing(parent_sid).await;

        let parent = gated_parent_registry();
        let manager = std::sync::Arc::new(crate::brain::tools::subagent::SubAgentManager::new());
        let grant = crate::brain::agent::service::work_status::AgentGrant::new(false, false, true);
        let child = crate::brain::tools::subagent::build_child_registry_gated(
            &parent, &manager, parent_sid, &grant, false,
        )
        .await;

        assert_stripped(
            &child,
            "a resumed child whose parent is in Plan-mode Editing (#1079)",
        );
    })
    .await;
}

#[tokio::test]
async fn resumed_child_whose_parent_left_editing_is_not_stripped() {
    // The other half of the re-derivation: the strip tracks the parent's
    // CURRENT state, so a parent that approved or discarded its plan hands the
    // child its write access back. Freezing `read_only: true` into the grant at
    // spawn would have locked the child out for life.
    in_temp_home(async {
        let parent_sid = Uuid::new_v4();
        make_post_init_editing(parent_sid).await;

        let parent = gated_parent_registry();
        let manager = std::sync::Arc::new(crate::brain::tools::subagent::SubAgentManager::new());
        let grant = crate::brain::agent::service::work_status::AgentGrant::new(false, false, true);

        // While Editing: stripped.
        let during = crate::brain::tools::subagent::build_child_registry_gated(
            &parent, &manager, parent_sid, &grant, false,
        )
        .await;
        assert_stripped(&during, "precondition: the parent is Editing");

        // The parent leaves Editing (discard drops the plan JSON).
        crate::utils::plan_files::discard_plan(parent_sid).await;
        assert_eq!(
            plan_mode_state(parent_sid).await,
            PlanModeState::NoPlan,
            "precondition: the parent has left Editing"
        );

        let after = crate::brain::tools::subagent::build_child_registry_gated(
            &parent, &manager, parent_sid, &grant, false,
        )
        .await;
        assert_full(
            &after,
            "a resumed child whose parent has left Plan mode (#1079)",
        );
    })
    .await;
}

#[tokio::test]
async fn the_plan_review_worker_keeps_write_under_an_editing_parent() {
    // #155, and the reason the exception is a TYPED flag rather than a label
    // comparison (#1075): the worker must write the plan `.md` while its parent
    // is Editing, and only a deliberately-set grant may say so.
    in_temp_home(async {
        let parent_sid = Uuid::new_v4();
        make_post_init_editing(parent_sid).await;

        let parent = gated_parent_registry();
        let manager = std::sync::Arc::new(crate::brain::tools::subagent::SubAgentManager::new());
        let grant = crate::brain::agent::service::work_status::AgentGrant::plan_review_worker(
            false, true, true,
        );
        let child = crate::brain::tools::subagent::build_child_registry_gated(
            &parent,
            &manager,
            parent_sid,
            &grant,
            grant.plan_review,
        )
        .await;

        assert_full(
            &child,
            "the plan-review worker under an Editing parent (#155)",
        );
    })
    .await;
}

#[tokio::test]
async fn the_callers_read_only_grant_still_strips_outside_plan_mode() {
    // The #1173 grant is unchanged by this fix: with no plan at all, a
    // `read_only` child is still stripped.
    in_temp_home(async {
        let parent_sid = Uuid::new_v4();
        let parent = gated_parent_registry();
        let manager = std::sync::Arc::new(crate::brain::tools::subagent::SubAgentManager::new());
        let grant = crate::brain::agent::service::work_status::AgentGrant::new(true, false, true);
        let child = crate::brain::tools::subagent::build_child_registry_gated(
            &parent, &manager, parent_sid, &grant, false,
        )
        .await;
        assert_stripped(&child, "a read_only child with no plan (#1173)");
    })
    .await;
}

// ── The call sites route through the constructor (#1075, #1079) ───────────
// The rule lives in one place now, so the regression to catch is a site
// drifting back to a hand-rolled strip — which is how all three paths were
// born. Source-shape pins, in the style of the #1064 team pins.

#[test]
fn every_production_child_registry_site_uses_the_gated_constructor() {
    for (file, src) in [
        ("spawn.rs", include_str!("../brain/tools/subagent/spawn.rs")),
        (
            "resume.rs",
            include_str!("../brain/tools/subagent/resume.rs"),
        ),
        (
            "team/create.rs",
            include_str!("../brain/tools/subagent/team/create.rs"),
        ),
    ] {
        assert!(
            !src.contains("plan_gate::restrict_registry_to_read_only(&child_registry)"),
            "{file} must not strip a child registry by hand (#1075, #1079): the \
             two strips belong in build_child_registry_gated, and a site that \
             re-implements one of them is how every escalation path was born"
        );
        assert!(
            src.contains("build_child_registry_gated("),
            "{file} must build its child registry through build_child_registry_gated \
             (#1075, #1079)"
        );
    }
}

#[test]
fn the_plan_review_exception_is_not_a_label_comparison() {
    let spawn_src = include_str!("../brain/tools/subagent/spawn.rs");
    assert!(
        !spawn_src.contains("label == super::PLAN_REVIEW_LABEL"),
        "a write privilege must never be decided by comparing model-authored \
         text (#1075); the typed `plan_review` grant field carries it instead"
    );

    let resume_src = include_str!("../brain/tools/subagent/resume.rs");
    assert!(
        !resume_src.contains("PLAN_REVIEW_LABEL"),
        "resume must take the exception from the persisted grant, never the \
         child's label (#1075, #1079)"
    );
    assert!(
        resume_src.contains("get_plan_review("),
        "resume must read the persisted typed flag (#1079)"
    );
}

#[test]
fn the_plan_review_privilege_is_not_read_from_model_input() {
    // #1075, the second half. Moving the exception from a `label` comparison to
    // a typed field is only a real fix if the MODEL cannot set that field: the
    // `spawn_agent` JSON schema is model-authored, so an input key named
    // `plan_review` would let any spawn mint the Editing-parent write exception
    // by emitting it in the tool call. That is the same hole with the string
    // swapped for a boolean. The privilege must be stamped on the TRUSTED
    // execution context by the one caller that mints the worker.
    let spawn_src = include_str!("../brain/tools/subagent/spawn.rs");
    assert!(
        !spawn_src.contains(r#"input.get("plan_review")"#),
        "spawn must never read the privilege out of the model's tool input (#1075)"
    );
    assert!(
        !spawn_src.contains("let plan_review = match input"),
        "spawn must not parse the privilege from input at all (#1075)"
    );
    assert!(
        spawn_src.contains("context.plan_review_worker"),
        "spawn must take the exception from the trusted execution context (#1075)"
    );
}

#[test]
fn the_plan_review_flag_defaults_to_the_narrow_reading() {
    use crate::brain::agent::service::work_status::AgentGrant;

    // A record written before #1075 has no `plan_review` key at all. It must
    // deserialise (a bare `bool` would fail the whole file) and read as
    // `false` — an ordinary child. Reading it as the worker would resurrect the
    // very escalation this closes.
    let legacy_json = r#"{"read_only":false,"include_brain":true,"allow_nested":true}"#;
    let grant: AgentGrant = serde_json::from_str(legacy_json).expect("old records must still load");
    assert!(!grant.plan_review, "absence must not read as the exception");
    assert!(grant.include_brain && grant.allow_nested);

    // `legacy()` keeps its exact pre-existing semantics, on the narrow side.
    assert_eq!(AgentGrant::legacy(), AgentGrant::new(false, false, true));
    assert!(!AgentGrant::legacy().plan_review);

    // `new` never mints the exception; only the named constructor does.
    assert!(!AgentGrant::new(true, true, false).plan_review);
    assert!(AgentGrant::plan_review_worker(true, true, false).plan_review);

    // And it round-trips.
    let worker = AgentGrant::plan_review_worker(false, true, false);
    let json = serde_json::to_string(&worker).expect("serialise");
    assert_eq!(
        serde_json::from_str::<AgentGrant>(&json).expect("deserialise"),
        worker
    );
}
