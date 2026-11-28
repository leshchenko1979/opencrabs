//! Sub-Agent / Swarm System Tests
//!
//! Covers SubAgentManager state machine, the tool operations
//! (spawn, wait, send_input, close, resume), lifecycle transitions, input
//! channel wiring, cancellation, and concurrent access.

// ─── SubAgentManager Unit Tests ────────────────────────────────────────────

mod manager {
    use crate::brain::tools::subagent::SubAgentManager;
    use crate::brain::tools::subagent::{SubAgent, SubAgentState};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    fn make_agent(id: &str, label: &str) -> SubAgent {
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        SubAgent {
            input_tx: Some(tx),
            ..SubAgent::new(
                id.to_string(),
                label.to_string(),
                Uuid::new_v4(),
                Uuid::new_v4(),
            )
        }
    }

    #[test]
    fn new_manager_is_empty() {
        let mgr = SubAgentManager::new();
        assert!(mgr.list().is_empty());
    }

    #[test]
    fn default_creates_empty_manager() {
        let mgr = SubAgentManager::default();
        assert!(mgr.list().is_empty());
    }

    #[test]
    fn generate_id_is_8_chars() {
        let id = SubAgentManager::generate_id();
        assert_eq!(id.len(), 8);
        // Should be hex chars from UUID
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }

    #[test]
    fn insert_and_get_state() {
        let mgr = SubAgentManager::new();
        let agent = make_agent("a1", "test-agent");
        mgr.insert(agent);

        assert_eq!(mgr.get_state("a1"), Some(SubAgentState::Running));
        assert!(mgr.exists("a1"));
    }

    #[test]
    fn get_state_missing_returns_none() {
        let mgr = SubAgentManager::new();
        assert_eq!(mgr.get_state("nonexistent"), None);
    }

    #[test]
    fn alive_counts_for_splits_working_and_awaiting_per_parent() {
        // #1183: the settle card counts only THIS session's children, split
        // working vs parked-awaiting-collection. The manager is process-global
        // (one instance wired into every channel agent), so an unfiltered
        // count would report another chat's fan-out as this chat's pending
        // work. Terminal agents never count.
        let mgr = SubAgentManager::new();
        let parent = Uuid::new_v4();
        let other_chat = Uuid::new_v4();

        let mut working = make_agent("w1", "working");
        working.parent_session_id = parent;
        let mut parked = make_agent("p1", "parked");
        parked.parent_session_id = parent;
        let mut done = make_agent("d1", "done");
        done.parent_session_id = parent;
        let mut foreign = make_agent("f1", "other-chat");
        foreign.parent_session_id = other_chat;

        mgr.insert(working);
        mgr.insert(parked);
        mgr.insert(done);
        mgr.insert(foreign);

        mgr.mark_awaiting_input("p1");
        mgr.mark_completed("d1", "finished".to_string());

        assert_eq!(mgr.alive_counts_for(parent), (1, 1));
        assert_eq!(mgr.alive_counts_for(other_chat), (1, 0));
        assert_eq!(mgr.alive_counts_for(Uuid::new_v4()), (0, 0));

        // Follow-up input flips the parked agent back to working: the split
        // must track the state machine, not a static snapshot.
        mgr.mark_running_again("p1");
        assert_eq!(mgr.alive_counts_for(parent), (2, 0));
    }

    #[test]
    fn get_output_initially_none() {
        let mgr = SubAgentManager::new();
        mgr.insert(make_agent("a1", "test"));
        assert_eq!(mgr.get_output("a1"), None);
    }

    #[test]
    fn update_output_preserves_running_state() {
        let mgr = SubAgentManager::new();
        mgr.insert(make_agent("a1", "test"));

        mgr.update_output("a1", "partial result".to_string());

        assert_eq!(mgr.get_state("a1"), Some(SubAgentState::Running));
        assert_eq!(mgr.get_output("a1"), Some("partial result".to_string()));
    }

    #[test]
    fn update_output_on_nonexistent_is_noop() {
        let mgr = SubAgentManager::new();
        mgr.update_output("ghost", "data".to_string());
        // No panic, no entry created
        assert!(!mgr.exists("ghost"));
    }

    #[test]
    fn mark_completed_sets_state_and_output() {
        let mgr = SubAgentManager::new();
        mgr.insert(make_agent("a1", "test"));

        mgr.mark_completed("a1", "final output".to_string());

        assert_eq!(mgr.get_state("a1"), Some(SubAgentState::Completed));
        assert_eq!(mgr.get_output("a1"), Some("final output".to_string()));
    }

    #[test]
    fn mark_completed_clears_input_tx() {
        let mgr = SubAgentManager::new();
        mgr.insert(make_agent("a1", "test"));
        assert!(mgr.get_input_tx("a1").is_some());

        mgr.mark_completed("a1", "done".to_string());
        assert!(mgr.get_input_tx("a1").is_none());
    }

    #[test]
    fn mark_failed_sets_state_and_clears_input() {
        let mgr = SubAgentManager::new();
        mgr.insert(make_agent("a1", "test"));

        mgr.mark_failed("a1", "something broke".to_string());

        assert_eq!(
            mgr.get_state("a1"),
            Some(SubAgentState::Failed("something broke".to_string()))
        );
        assert!(mgr.get_input_tx("a1").is_none());
    }

    #[test]
    fn cancel_running_agent_succeeds() {
        let mgr = SubAgentManager::new();
        let agent = make_agent("a1", "test");
        let token = agent.cancel_token.clone();
        mgr.insert(agent);

        assert!(mgr.cancel("a1"));
        assert_eq!(mgr.get_state("a1"), Some(SubAgentState::Cancelled));
        assert!(token.is_cancelled());
        assert!(mgr.get_input_tx("a1").is_none());
    }

    #[test]
    fn cancel_non_running_agent_returns_false() {
        let mgr = SubAgentManager::new();
        mgr.insert(make_agent("a1", "test"));
        mgr.mark_completed("a1", "done".to_string());

        assert!(!mgr.cancel("a1"));
        assert_eq!(mgr.get_state("a1"), Some(SubAgentState::Completed));
    }

    #[test]
    fn cancel_nonexistent_returns_false() {
        let mgr = SubAgentManager::new();
        assert!(!mgr.cancel("ghost"));
    }

    #[test]
    fn get_input_tx_returns_sender() {
        let mgr = SubAgentManager::new();
        mgr.insert(make_agent("a1", "test"));

        let tx = mgr.get_input_tx("a1");
        assert!(tx.is_some());
    }

    #[test]
    fn get_input_tx_missing_returns_none() {
        let mgr = SubAgentManager::new();
        assert!(mgr.get_input_tx("ghost").is_none());
    }

    #[test]
    fn take_join_handle_returns_none_when_not_set() {
        let mgr = SubAgentManager::new();
        mgr.insert(make_agent("a1", "test")); // make_agent sets handle to None
        assert!(mgr.take_join_handle("a1").is_none());
    }

    #[test]
    fn set_and_take_join_handle() {
        let mgr = SubAgentManager::new();
        mgr.insert(make_agent("a1", "test"));

        let handle = tokio::runtime::Runtime::new().unwrap().spawn(async {});
        mgr.set_join_handle("a1", handle);

        assert!(mgr.take_join_handle("a1").is_some());
        // Second take returns None
        assert!(mgr.take_join_handle("a1").is_none());
    }

    #[test]
    fn prepare_resume_from_completed() {
        let mgr = SubAgentManager::new();
        mgr.insert(make_agent("a1", "test"));
        mgr.mark_completed("a1", "done".to_string());

        let new_token = CancellationToken::new();
        let (new_tx, _rx) = mpsc::unbounded_channel::<String>();

        assert!(mgr.prepare_resume("a1", new_token, new_tx));
        assert_eq!(mgr.get_state("a1"), Some(SubAgentState::Running));
        assert_eq!(mgr.get_output("a1"), None); // output cleared
        assert!(mgr.get_input_tx("a1").is_some()); // new channel set
    }

    #[test]
    fn prepare_resume_from_failed() {
        let mgr = SubAgentManager::new();
        mgr.insert(make_agent("a1", "test"));
        mgr.mark_failed("a1", "error".to_string());

        let new_token = CancellationToken::new();
        let (new_tx, _rx) = mpsc::unbounded_channel::<String>();

        assert!(mgr.prepare_resume("a1", new_token, new_tx));
        assert_eq!(mgr.get_state("a1"), Some(SubAgentState::Running));
    }

    #[test]
    fn prepare_resume_from_running_fails() {
        let mgr = SubAgentManager::new();
        mgr.insert(make_agent("a1", "test")); // Running

        let new_token = CancellationToken::new();
        let (new_tx, _rx) = mpsc::unbounded_channel::<String>();

        assert!(!mgr.prepare_resume("a1", new_token, new_tx));
        assert_eq!(mgr.get_state("a1"), Some(SubAgentState::Running));
    }

    /// A stopped agent IS resumable — #924 narrowed `stop_agent` to "end the
    /// work, keep the identity", so the record survives the stop and the
    /// parent can pick the child back up.
    ///
    /// The mutant this kills: the pre-#924 arm
    /// `Completed | Failed(_)` with `Cancelled` omitted. That is the shape this
    /// test asserted before the contract changed, and it reads as correct
    /// until someone tries to resume a child they deliberately stopped — which
    /// is exactly the flow `stop_agent` exists for. The failure it produces is
    /// silent: `resume_agent` returns "cannot be resumed" for an agent whose
    /// session and transcript are both still on disk.
    #[test]
    fn prepare_resume_from_stopped_succeeds() {
        // Kills the mutant that refuses to resume a Cancelled agent, which would make every stop a permanent loss of the child's context.
        let mgr = SubAgentManager::new();
        mgr.insert(make_agent("a1", "test"));
        mgr.cancel("a1");
        assert_eq!(mgr.get_state("a1"), Some(SubAgentState::Cancelled));

        let new_token = CancellationToken::new();
        let (new_tx, _rx) = mpsc::unbounded_channel::<String>();

        assert!(
            mgr.prepare_resume("a1", new_token, new_tx),
            "a stopped agent must stay resumable: the stop ended its work, not its identity"
        );
        assert_eq!(mgr.get_state("a1"), Some(SubAgentState::Running));
        assert!(mgr.get_input_tx("a1").is_some()); // new channel set
    }

    #[test]
    fn list_returns_all_agents() {
        let mgr = SubAgentManager::new();
        mgr.insert(make_agent("a1", "alpha"));
        mgr.insert(make_agent("a2", "beta"));
        mgr.insert(make_agent("a3", "gamma"));

        let list = mgr.list();
        assert_eq!(list.len(), 3);

        let ids: Vec<&str> = list.iter().map(|(id, _, _)| id.as_str()).collect();
        assert!(ids.contains(&"a1"));
        assert!(ids.contains(&"a2"));
        assert!(ids.contains(&"a3"));
    }

    #[test]
    fn get_session_id() {
        let mgr = SubAgentManager::new();
        let agent = make_agent("a1", "test");
        let expected_sid = agent.session_id;
        mgr.insert(agent);

        assert_eq!(mgr.get_session_id("a1"), Some(expected_sid));
    }

    #[test]
    fn get_session_id_missing_returns_none() {
        let mgr = SubAgentManager::new();
        assert_eq!(mgr.get_session_id("ghost"), None);
    }

    #[test]
    fn concurrent_access_is_safe() {
        use std::sync::Arc;
        use std::thread;

        let mgr = Arc::new(SubAgentManager::new());
        let mut handles = vec![];

        // Spawn 10 threads inserting concurrently
        for i in 0..10 {
            let mgr = mgr.clone();
            handles.push(thread::spawn(move || {
                let id = format!("agent-{}", i);
                mgr.insert(make_agent(&id, &format!("worker-{}", i)));
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(mgr.list().len(), 10);
    }

    #[test]
    fn input_channel_delivers_messages() {
        let mgr = SubAgentManager::new();
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let agent = SubAgent {
            input_tx: Some(tx),
            ..SubAgent::new(
                "a1".to_string(),
                "test".to_string(),
                Uuid::new_v4(),
                Uuid::new_v4(),
            )
        };
        mgr.insert(agent);

        // Send via manager's tx
        let sender = mgr.get_input_tx("a1").unwrap();
        sender.send("hello".to_string()).unwrap();
        sender.send("world".to_string()).unwrap();

        // Receive on the other end
        assert_eq!(rx.try_recv().unwrap(), "hello");
        assert_eq!(rx.try_recv().unwrap(), "world");
    }

    // ─── #191 parent scoping ───────────────────────────────────────────────

    /// Build a child owned by a specific parent session.
    fn make_agent_for(id: &str, label: &str, parent: Uuid) -> SubAgent {
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        SubAgent {
            input_tx: Some(tx),
            ..SubAgent::new(id.to_string(), label.to_string(), Uuid::new_v4(), parent)
        }
    }

    /// #191: the manager is process-global (one instance per channel factory),
    /// so a per-session surface must be able to ask for its OWN children.
    /// Pre-fix no such query existed and every caller iterated `list()`.
    #[test]
    fn list_for_parent_returns_only_that_sessions_children() {
        let mgr = SubAgentManager::new();
        let a = Uuid::from_u128(0xa);
        let b = Uuid::from_u128(0xb);
        mgr.insert(make_agent_for("a1", "mine", a));
        mgr.insert(make_agent_for("a2", "mine-too", a));
        mgr.insert(make_agent_for("b1", "theirs", b));

        // HashMap order is not deterministic — compare as a sorted set.
        let mut mine: Vec<String> = mgr
            .list_for_parent(a)
            .into_iter()
            .map(|(id, _, _)| id)
            .collect();
        mine.sort();
        assert_eq!(mine, vec!["a1".to_string(), "a2".to_string()]);

        let theirs: Vec<String> = mgr
            .list_for_parent(b)
            .into_iter()
            .map(|(id, _, _)| id)
            .collect();
        assert_eq!(
            theirs,
            vec!["b1".to_string()],
            "symmetric for the other session"
        );
    }

    /// Negative control: a session that spawned nothing gets nothing back —
    /// not "everything", and not an error.
    #[test]
    fn list_for_parent_is_empty_for_a_session_with_no_children() {
        let mgr = SubAgentManager::new();
        let a = Uuid::from_u128(0xa);
        let b = Uuid::from_u128(0xb);
        mgr.insert(make_agent_for("b1", "theirs", b));

        assert!(mgr.list_for_parent(a).is_empty());
        assert_eq!(mgr.list_for_parent(b).len(), 1);
    }

    /// `list()` is public API of the lib target and must keep its global
    /// meaning — the fix is a NEW scoped query, not a narrowed old one.
    #[test]
    fn list_still_returns_the_global_view() {
        let mgr = SubAgentManager::new();
        mgr.insert(make_agent_for("a1", "mine", Uuid::from_u128(0xa)));
        mgr.insert(make_agent_for("b1", "theirs", Uuid::from_u128(0xb)));

        assert_eq!(mgr.list().len(), 2, "scoping must not change list()");
    }

    /// Labels and states survive the filter — the roster renders them.
    #[test]
    fn list_for_parent_carries_label_and_state() {
        let mgr = SubAgentManager::new();
        let a = Uuid::from_u128(0xa);
        mgr.insert(make_agent_for("a1", "researcher", a));

        let rows = mgr.list_for_parent(a);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "researcher");
        assert_eq!(rows[0].2, SubAgentState::Running);
    }

    /// Unlike `alive_counts_for`, a finished child still appears: a caller
    /// asking what it spawned wants its completed children too.
    #[test]
    fn list_for_parent_includes_finished_children() {
        let mgr = SubAgentManager::new();
        let a = Uuid::from_u128(0xa);
        mgr.insert(make_agent_for("a1", "done", a));
        mgr.mark_completed("a1", "out".to_string());

        assert_eq!(mgr.list_for_parent(a).len(), 1);
        assert_eq!(
            mgr.alive_counts_for(a),
            (0, 0),
            "terminal agents are not alive"
        );
    }
}

// ─── SendInputTool Tests ───────────────────────────────────────────────────

mod send_input_tool {
    use crate::brain::tools::subagent::SendInputTool;
    use crate::brain::tools::subagent::{SubAgent, SubAgentManager};
    use crate::brain::tools::{Tool, ToolExecutionContext};
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use uuid::Uuid;

    fn test_context() -> ToolExecutionContext {
        let mut ctx = ToolExecutionContext::new(Uuid::new_v4())
            .with_working_directory(std::path::PathBuf::from("/tmp"))
            .with_auto_approve(true);
        ctx.timeout_secs = 30;
        ctx
    }

    fn make_running_agent(id: &str) -> (SubAgent, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let agent = SubAgent {
            input_tx: Some(tx),
            ..SubAgent::new(
                id.to_string(),
                "test".to_string(),
                Uuid::new_v4(),
                Uuid::new_v4(),
            )
        };
        (agent, rx)
    }

    #[tokio::test]
    async fn missing_agent_id_returns_error() {
        let mgr = Arc::new(SubAgentManager::new());
        let tool = SendInputTool::new(mgr);
        let ctx = test_context();

        let result = tool.execute(json!({"text": "hi"}), &ctx).await;
        assert!(result.is_err()); // InvalidInput error
    }

    #[tokio::test]
    async fn missing_text_returns_error() {
        let mgr = Arc::new(SubAgentManager::new());
        let tool = SendInputTool::new(mgr);
        let ctx = test_context();

        let result = tool.execute(json!({"agent_id": "a1"}), &ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn nonexistent_agent_returns_tool_error() {
        let mgr = Arc::new(SubAgentManager::new());
        let tool = SendInputTool::new(mgr);
        let ctx = test_context();

        let result = tool
            .execute(json!({"agent_id": "ghost", "text": "hi"}), &ctx)
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .as_ref()
                .unwrap()
                .contains("No sub-agent found")
        );
    }

    #[tokio::test]
    async fn send_to_completed_agent_returns_error() {
        let mgr = Arc::new(SubAgentManager::new());
        let (agent, _rx) = make_running_agent("a1");
        mgr.insert(agent);
        mgr.mark_completed("a1", "done".to_string());

        let tool = SendInputTool::new(mgr);
        let ctx = test_context();

        let result = tool
            .execute(json!({"agent_id": "a1", "text": "hi"}), &ctx)
            .await
            .unwrap();
        assert!(!result.success);
        // #1184 ripple: completed agents are pointed at resume_agent instead of
        // the old generic "not running" rejection.
        assert!(
            result
                .error
                .as_ref()
                .unwrap()
                .contains("use resume_agent to continue")
        );
    }

    #[tokio::test]
    async fn send_to_running_agent_succeeds() {
        let mgr = Arc::new(SubAgentManager::new());
        let (agent, mut rx) = make_running_agent("a1");
        mgr.insert(agent);

        let tool = SendInputTool::new(mgr);
        let ctx = test_context();

        let result = tool
            .execute(json!({"agent_id": "a1", "text": "do something"}), &ctx)
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("Input sent"));

        // Message actually arrived
        assert_eq!(rx.try_recv().unwrap(), "do something");
    }

    #[tokio::test]
    async fn send_after_channel_closed_returns_error() {
        let mgr = Arc::new(SubAgentManager::new());
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let agent = SubAgent {
            input_tx: Some(tx),
            ..SubAgent::new(
                "a1".to_string(),
                "test".to_string(),
                Uuid::new_v4(),
                Uuid::new_v4(),
            )
        };
        mgr.insert(agent);

        // Drop receiver to close channel
        drop(rx);

        let tool = SendInputTool::new(mgr);
        let ctx = test_context();

        let result = tool
            .execute(json!({"agent_id": "a1", "text": "hi"}), &ctx)
            .await;
        // Should be Err (ToolError::Execution) since send fails
        assert!(result.is_err());
    }
}

// ─── StopAgentTool Tests ──────────────────────────────────────────────────

mod stop_agent_tool {
    use crate::brain::tools::subagent::StopAgentTool;
    use crate::brain::tools::subagent::{SubAgent, SubAgentManager, SubAgentState};
    use crate::brain::tools::{Tool, ToolExecutionContext};
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use uuid::Uuid;

    /// The session these tests act as (#924). Fixed rather than random: the
    /// ownership gate resolves only the caller's OWN children, so a fixture
    /// parented to an independent `Uuid::new_v4()` is a stranger and
    /// `stop_agent` correctly refuses to touch it.
    fn parent_session() -> Uuid {
        Uuid::from_u128(0x924)
    }

    /// Point the status-file layer at a throwaway dir (#924).
    ///
    /// `resolve()` falls back to reading `<detached>/<id>.json` when the map has
    /// no entry, so a test asserting "this id is not found" is asserting against
    /// the REAL directory unless it isolates — one leftover `ghost.json` from any
    /// other run turns that assertion inside out. The override is thread-local
    /// and the runner reuses threads, hence a per-test tag.
    fn isolate(tag: &str) {
        let dir =
            std::env::temp_dir().join(format!("opencrabs-924-stop-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::brain::agent::service::work_status::test_override::set(dir.join("detached"));
    }

    fn test_context() -> ToolExecutionContext {
        let mut ctx = ToolExecutionContext::new(parent_session())
            .with_working_directory(std::path::PathBuf::from("/tmp"))
            .with_auto_approve(true);
        ctx.timeout_secs = 30;
        ctx
    }

    fn make_running_agent(id: &str) -> SubAgent {
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        SubAgent {
            input_tx: Some(tx),
            ..SubAgent::new(
                id.to_string(),
                "test".to_string(),
                Uuid::new_v4(),
                parent_session(),
            )
        }
    }

    #[tokio::test]
    async fn missing_agent_id_returns_error() {
        let mgr = Arc::new(SubAgentManager::new());
        let tool = StopAgentTool::new(mgr);
        let ctx = test_context();

        let result = tool.execute(json!({}), &ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn stop_nonexistent_returns_error() {
        // Kills the mutant that stops any id it can see: a stop must be scoped to the caller's own children, so a foreign or absent id answers identically and says so.
        isolate("nonexistent");
        let mgr = Arc::new(SubAgentManager::new());
        let tool = StopAgentTool::new(mgr);
        let ctx = test_context();

        let result = tool
            .execute(json!({"agent_id": "ghost"}), &ctx)
            .await
            .unwrap();
        assert!(!result.success);
        // The refusal names the OWNERSHIP boundary, not mere absence: an id
        // that does not exist and another session's id answer identically on
        // purpose, so this string is what tells the caller it was scoped.
        assert!(
            result
                .error
                .as_ref()
                .unwrap()
                .contains("belongs to this session")
        );
    }

    #[tokio::test]
    async fn stop_running_agent_cancels() {
        // Kills the mutant that marks a stop without firing the token (the child keeps working) and the one that deletes the record instead of ending its work.
        isolate("running");
        let mgr = Arc::new(SubAgentManager::new());
        let agent = make_running_agent("a1");
        let token = agent.cancel_token.clone();
        mgr.insert(agent);

        let tool = StopAgentTool::new(mgr.clone());
        let ctx = test_context();

        let result = tool.execute(json!({"agent_id": "a1"}), &ctx).await.unwrap();
        assert!(result.success);
        // The BEHAVIOUR, not the wording: the token actually fires and the
        // state actually moves.
        assert!(token.is_cancelled());
        assert_eq!(mgr.get_state("a1"), Some(SubAgentState::Cancelled));
        // Still tracked — stopping is not deleting (#924).
        assert!(mgr.exists("a1"));
        assert_eq!(
            mgr.get_output("a1"),
            None,
            "a stop produces no output; writing one would let `wait_agent` hand the parent a result the child never finished"
        );
    }

    #[tokio::test]
    async fn stop_already_completed_agent() {
        // Kills the mutant that rewrites a finished agent's outcome to Cancelled: a stop cannot retroactively change work that already landed.
        isolate("completed");
        let mgr = Arc::new(SubAgentManager::new());
        mgr.insert(make_running_agent("a1"));
        mgr.mark_completed("a1", "done".to_string());

        let tool = StopAgentTool::new(mgr.clone());
        let ctx = test_context();

        let result = tool.execute(json!({"agent_id": "a1"}), &ctx).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("already finished"));
        // State stays Completed: cancel() refuses anything not live, so a
        // stop cannot retroactively rewrite a finished agent's outcome.
        assert_eq!(mgr.get_state("a1"), Some(SubAgentState::Completed));
    }
}

// ─── WaitAgentTool Tests ───────────────────────────────────────────────────

mod wait_agent_tool {
    use crate::brain::tools::subagent::WaitAgentTool;
    use crate::brain::tools::subagent::{SubAgent, SubAgentManager};
    use crate::brain::tools::{Tool, ToolExecutionContext};
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use uuid::Uuid;

    /// The session these tests act as (#191). Fixed rather than random: the
    /// resolver is scoped to the caller's session now, so every fixture child
    /// must be parented to the SAME session the context carries — with two
    /// independent `Uuid::new_v4()`s the children are foreign and the scoped
    /// resolver correctly refuses to see them.
    fn parent_session() -> Uuid {
        Uuid::from_u128(0x191)
    }

    fn test_context() -> ToolExecutionContext {
        let mut ctx = ToolExecutionContext::new(parent_session())
            .with_working_directory(std::path::PathBuf::from("/tmp"))
            .with_auto_approve(true);
        ctx.timeout_secs = 30;
        ctx
    }

    fn make_running_agent(id: &str) -> SubAgent {
        let (tx, _rx) = mpsc::unbounded_channel::<String>();
        SubAgent {
            input_tx: Some(tx),
            ..SubAgent::new(
                id.to_string(),
                "test".to_string(),
                Uuid::new_v4(),
                parent_session(),
            )
        }
    }

    #[tokio::test]
    async fn missing_agent_id_returns_error() {
        let mgr = Arc::new(SubAgentManager::new());
        let tool = WaitAgentTool::new(mgr);
        let ctx = test_context();

        let result = tool.execute(json!({}), &ctx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn wait_nonexistent_returns_error() {
        let mgr = Arc::new(SubAgentManager::new());
        let tool = WaitAgentTool::new(mgr);
        let ctx = test_context();

        let result = tool
            .execute(json!({"agent_id": "ghost"}), &ctx)
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .as_ref()
                .unwrap()
                .contains("No sub-agent found")
        );
    }

    #[tokio::test]
    async fn wait_already_completed_returns_immediately() {
        let mgr = Arc::new(SubAgentManager::new());
        mgr.insert(make_running_agent("a1"));
        mgr.mark_completed("a1", "result data".to_string());

        let tool = WaitAgentTool::new(mgr);
        let ctx = test_context();

        let result = tool.execute(json!({"agent_id": "a1"}), &ctx).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("completed"));
        assert!(result.output.contains("result data"));
    }

    #[tokio::test]
    async fn wait_already_failed_returns_immediately() {
        let mgr = Arc::new(SubAgentManager::new());
        mgr.insert(make_running_agent("a1"));
        mgr.mark_failed("a1", "something broke".to_string());

        let tool = WaitAgentTool::new(mgr);
        let ctx = test_context();

        let result = tool.execute(json!({"agent_id": "a1"}), &ctx).await.unwrap();
        assert!(!result.success);
        let err = result.error.as_ref().unwrap();
        assert!(err.contains("failed"));
        assert!(err.contains("something broke"));
    }

    #[tokio::test]
    async fn wait_cancelled_returns_immediately() {
        let mgr = Arc::new(SubAgentManager::new());
        mgr.insert(make_running_agent("a1"));
        mgr.cancel("a1");

        let tool = WaitAgentTool::new(mgr);
        let ctx = test_context();

        let result = tool.execute(json!({"agent_id": "a1"}), &ctx).await.unwrap();
        assert!(!result.success);
        // Kills the mutant that reports a stopped agent as a dead end: #924
        // makes a stop end the WORK, not the agent, so the message has to
        // name the stop AND point at resume_agent. The old wording said
        // "was cancelled", which read as terminal and stranded the work.
        let err = result.error.as_ref().unwrap();
        assert!(err.contains("stopped"));
        assert!(err.contains("resume_agent"));
    }

    #[tokio::test]
    async fn wait_with_join_handle_completes() {
        let mgr = Arc::new(SubAgentManager::new());
        let mgr_clone = mgr.clone();
        mgr.insert(make_running_agent("a1"));

        // Create a task that completes quickly and marks agent done
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            mgr_clone.mark_completed("a1", "async result".to_string());
        });
        mgr.set_join_handle("a1", handle);

        let tool = WaitAgentTool::new(mgr);
        let ctx = test_context();

        let result = tool
            .execute(json!({"agent_id": "a1", "timeout_secs": 5}), &ctx)
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("async result"));
    }

    #[tokio::test]
    async fn wait_timeout_returns_progress_not_error() {
        // New semantics: on timeout we return success with a progress
        // preview (or "No output yet.") so the LLM sees work-in-flight
        // and can keep polling. The old "Timed out" error wording drove
        // LLMs to abandon the parent turn after a few retries — replaced
        // by "still running after {secs}s" success messaging.
        let mgr = Arc::new(SubAgentManager::new());
        mgr.insert(make_running_agent("a1"));

        let tool = WaitAgentTool::new(mgr);
        let ctx = test_context();

        let result = tool
            .execute(json!({"agent_id": "a1", "timeout_secs": 1}), &ctx)
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("still running"));
    }

    #[tokio::test]
    async fn wait_returns_round_output_on_awaiting_input() {
        // Replaces the old no-handle test. The real fix for the
        // 2026-04-17 deadlock: sub-agents transition to AwaitingInput
        // at a round boundary, and wait_agent returns immediately with
        // the buffered round output instead of blocking on task-join
        // (which never resolves between rounds).
        let mgr = Arc::new(SubAgentManager::new());
        mgr.insert(make_running_agent("a1"));
        mgr.update_output("a1", "round 1 results go here".to_string());
        mgr.mark_awaiting_input("a1");

        let tool = WaitAgentTool::new(mgr);
        let ctx = test_context();

        let result = tool
            .execute(json!({"agent_id": "a1", "timeout_secs": 30}), &ctx)
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("paused for input"));
        assert!(result.output.contains("round 1 results"));
    }
}

// ─── Lifecycle Integration Tests ───────────────────────────────────────────

mod lifecycle {
    use crate::brain::tools::subagent::{SubAgent, SubAgentManager, SubAgentState};
    use std::sync::Arc;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    fn make_agent(id: &str) -> (SubAgent, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let agent = SubAgent {
            input_tx: Some(tx),
            ..SubAgent::new(
                id.to_string(),
                "lifecycle-test".to_string(),
                Uuid::new_v4(),
                Uuid::new_v4(),
            )
        };
        (agent, rx)
    }

    #[test]
    fn full_lifecycle_spawn_complete_resume() {
        let mgr = Arc::new(SubAgentManager::new());

        // 1. Spawn
        let (agent, _rx) = make_agent("a1");
        mgr.insert(agent);
        assert_eq!(mgr.get_state("a1"), Some(SubAgentState::Running));

        // 2. Update output mid-run
        mgr.update_output("a1", "progress...".to_string());
        assert_eq!(mgr.get_output("a1"), Some("progress...".to_string()));
        assert_eq!(mgr.get_state("a1"), Some(SubAgentState::Running));

        // 3. Complete
        mgr.mark_completed("a1", "final result".to_string());
        assert_eq!(mgr.get_state("a1"), Some(SubAgentState::Completed));
        assert_eq!(mgr.get_output("a1"), Some("final result".to_string()));

        // 4. Resume
        let new_token = CancellationToken::new();
        let (new_tx, _new_rx) = mpsc::unbounded_channel::<String>();
        assert!(mgr.prepare_resume("a1", new_token, new_tx));
        assert_eq!(mgr.get_state("a1"), Some(SubAgentState::Running));
        assert_eq!(mgr.get_output("a1"), None); // cleared

        // 5. Fail this time
        mgr.mark_failed("a1", "crashed".to_string());
        assert_eq!(
            mgr.get_state("a1"),
            Some(SubAgentState::Failed("crashed".to_string()))
        );

        // 6. Resume again from failed
        let new_token2 = CancellationToken::new();
        let (new_tx2, _rx2) = mpsc::unbounded_channel::<String>();
        assert!(mgr.prepare_resume("a1", new_token2, new_tx2));
        assert_eq!(mgr.get_state("a1"), Some(SubAgentState::Running));
    }

    /// A stopped agent IS resumable (#924), and a live one is not.
    ///
    /// Before #924 a stop was a dead end: `cancel()` parked the agent in a state
    /// `prepare_resume` refused, so the only way to continue the work was to
    /// spawn a fresh child with none of the old context. Stopping was therefore
    /// destructive in practice while claiming to be reversible.
    ///
    /// Two-sided on purpose: widening the gate must not let a caller hijack an
    /// agent that is still working, so the Running arm is asserted to REFUSE.
    #[test]
    fn stopped_agent_can_resume_but_a_live_one_cannot() {
        // Kills the mutant that resumes a RUNNING agent, silently swapping its token and orphaning the task already holding the old one.
        let mgr = Arc::new(SubAgentManager::new());
        let (agent, _rx) = make_agent("a1");
        mgr.insert(agent);

        // Control: a running agent refuses resumption — resuming it would
        // silently swap its token and orphan the task already using the old one.
        let (tok, tx) = (
            CancellationToken::new(),
            mpsc::unbounded_channel::<String>().0,
        );
        assert!(!mgr.prepare_resume("a1", tok, tx));
        assert_eq!(mgr.get_state("a1"), Some(SubAgentState::Running));

        // The case under test: stop, then resume.
        mgr.cancel("a1");
        assert_eq!(mgr.get_state("a1"), Some(SubAgentState::Cancelled));

        let new_token = CancellationToken::new();
        let (new_tx, _rx2) = mpsc::unbounded_channel::<String>();
        assert!(
            mgr.prepare_resume("a1", new_token, new_tx),
            "a stopped agent must be resumable — a stop is not a deletion (#924)"
        );
        assert_eq!(mgr.get_state("a1"), Some(SubAgentState::Running));
    }

    #[tokio::test]
    async fn input_channel_survives_multiple_sends() {
        let mgr = Arc::new(SubAgentManager::new());
        let (agent, mut rx) = make_agent("a1");
        mgr.insert(agent);

        let tx = mgr.get_input_tx("a1").unwrap();
        for i in 0..100 {
            tx.send(format!("msg-{}", i)).unwrap();
        }

        for i in 0..100 {
            assert_eq!(rx.try_recv().unwrap(), format!("msg-{}", i));
        }
    }

    #[test]
    fn multiple_agents_independent_state() {
        let mgr = Arc::new(SubAgentManager::new());

        let (a1, _) = make_agent("a1");
        let (a2, _) = make_agent("a2");
        let (a3, _) = make_agent("a3");
        mgr.insert(a1);
        mgr.insert(a2);
        mgr.insert(a3);

        mgr.mark_completed("a1", "done-1".to_string());
        mgr.mark_failed("a2", "error-2".to_string());
        // a3 still running

        assert_eq!(mgr.get_state("a1"), Some(SubAgentState::Completed));
        assert_eq!(
            mgr.get_state("a2"),
            Some(SubAgentState::Failed("error-2".to_string()))
        );
        assert_eq!(mgr.get_state("a3"), Some(SubAgentState::Running));
    }
}

// ─── Child Registry & Grant Tests (#1173) ─────────────────────────────────

mod agent_type {
    use crate::brain::tools::subagent::map_deprecated_agent_type;

    /// Build a mock parent registry with common tools for filtering tests.
    fn mock_parent_registry() -> crate::brain::tools::ToolRegistry {
        use std::sync::Arc;
        let reg = crate::brain::tools::ToolRegistry::new();
        reg.register(Arc::new(crate::brain::tools::read::ReadTool));
        reg.register(Arc::new(crate::brain::tools::write::WriteTool));
        reg.register(Arc::new(crate::brain::tools::edit::EditTool));
        reg.register(Arc::new(crate::brain::tools::bash::BashTool));
        reg.register(Arc::new(crate::brain::tools::glob::GlobTool));
        reg.register(Arc::new(crate::brain::tools::grep::GrepTool));
        reg.register(Arc::new(crate::brain::tools::ls::LsTool));
        reg.register(Arc::new(
            crate::brain::tools::web_search::WebSearchTool::default(),
        ));
        reg
    }

    #[test]
    fn child_registry_inherits_full_parent() {
        let parent = mock_parent_registry();
        let mgr = std::sync::Arc::new(crate::brain::tools::subagent::SubAgentManager::new());
        let registry = crate::brain::tools::subagent::build_child_registry(&parent, &mgr);
        let tools = registry.list_tools();
        assert!(tools.contains(&"read_file".to_string()));
        assert!(tools.contains(&"write_file".to_string()));
        assert!(tools.contains(&"edit_file".to_string()));
        assert!(tools.contains(&"bash".to_string()));
        assert!(tools.contains(&"glob".to_string()));
        assert!(tools.contains(&"grep".to_string()));
    }

    #[test]
    fn child_registry_excludes_recursive_tools() {
        use std::sync::Arc;
        let parent = mock_parent_registry();
        let mgr = Arc::new(crate::brain::tools::subagent::SubAgentManager::new());
        parent.register(Arc::new(
            crate::brain::tools::subagent::SpawnAgentTool::new(
                mgr.clone(),
                Arc::new(crate::brain::tools::ToolRegistry::new()),
            ),
        ));
        let registry = crate::brain::tools::subagent::build_child_registry(&parent, &mgr);
        let tools = registry.list_tools();
        assert!(
            !tools.contains(&"spawn_agent".to_string()),
            "recursive spawn must never reach a child"
        );
    }

    #[test]
    fn deprecated_read_only_types_map_true() {
        for s in [
            "explore",
            "search",
            "find",
            "research",
            "web",
            "lookup",
            "EXPLORE",
            "  research  ",
        ] {
            assert_eq!(map_deprecated_agent_type(s), Ok(true), "input: {s}");
        }
    }

    #[test]
    fn deprecated_write_capable_types_map_false() {
        // `plan` carried bash historically, so it was NEVER read-only.
        for s in [
            "general",
            "plan",
            "architect",
            "design",
            "code",
            "implement",
            "write",
        ] {
            assert_eq!(map_deprecated_agent_type(s), Ok(false), "input: {s}");
        }
    }

    #[test]
    fn unknown_type_fails_closed() {
        for s in ["", "foobar", "Plan-X", "explor"] {
            let err = map_deprecated_agent_type(s).unwrap_err();
            assert!(err.contains("Unknown agent_type"), "input: {s}: {err}");
        }
    }

    #[test]
    fn restricted_registry_strips_mutating_tools() {
        let parent = mock_parent_registry();
        let mgr = std::sync::Arc::new(crate::brain::tools::subagent::SubAgentManager::new());
        let reg = crate::brain::tools::subagent::build_child_registry(&parent, &mgr);
        crate::brain::tools::plan_gate::restrict_registry_to_read_only(&reg);
        let tools = reg.list_tools();
        assert!(tools.contains(&"read_file".to_string()));
        assert!(tools.contains(&"glob".to_string()));
        assert!(!tools.contains(&"write_file".to_string()));
        assert!(!tools.contains(&"edit_file".to_string()));
        assert!(!tools.contains(&"bash".to_string()));
    }

    #[test]
    fn read_only_grant_frozen_in_manager() {
        use crate::brain::tools::subagent::{SubAgent, SubAgentManager};
        use uuid::Uuid;
        let mgr = SubAgentManager::new();
        mgr.insert(SubAgent {
            read_only: true,
            input_tx: None,
            ..SubAgent::new(
                "ro1".to_string(),
                "ro".to_string(),
                Uuid::new_v4(),
                Uuid::new_v4(),
            )
        });
        assert_eq!(mgr.get_read_only("ro1"), Some(true));
        assert_eq!(mgr.get_read_only("missing"), None);
    }
}

// ─── #924: a stop and a shutdown cancel the same token ─────────────────────
//
// The spawn loop has ONE cancel path and TWO reasons to take it: the parent
// called `stop_agent`, or the daemon is going down. From inside the child they
// are indistinguishable — a `CancellationToken` carries no reason — so the exit
// branch asks `is_deliberate_stop`, which reads the shutdown flag to separate
// them. Getting it wrong is expensive in both directions, which is why all
// three cases are pinned rather than just the happy one:
//
//   * a stop read as a shutdown is recorded `Interrupted`, and the parent's own
//     decision comes back as a crash report;
//   * a shutdown read as a stop is recorded `Cancelled`, which is a TERMINAL
//     state that `prepare_resume` accepts — so a child interrupted mid-work
//     looks like one that was deliberately parked.
mod deliberate_stop {
    use crate::brain::agent::service::shutdown;
    use crate::brain::tools::subagent::spawn::is_deliberate_stop;
    use tokio_util::sync::CancellationToken;

    /// The flag is process-global, so every case runs under the ONE lock the
    /// shutdown module owns — see `shutdown::test_guard`.
    fn with_clean_flag<T>(f: impl FnOnce() -> T) -> T {
        let _guard = shutdown::test_guard();
        shutdown::reset_for_test();
        let out = f();
        shutdown::reset_for_test();
        out
    }

    /// An untouched token is not a stop.
    ///
    /// The mutant this kills: `token.is_cancelled() || !is_shutting_down()`
    /// (a plausible slip when writing the condition) — which reports every
    /// running child as deliberately stopped the instant anything asks.
    #[test]
    fn an_uncancelled_token_is_not_a_stop() {
        // Kills the mutant that treats every unfinished agent as a deliberate stop, regardless of whether its token ever fired.
        with_clean_flag(|| {
            let token = CancellationToken::new();
            assert!(!is_deliberate_stop(&token));
        });
    }

    /// A cancel with no shutdown in progress IS the parent's stop.
    ///
    /// The mutant this kills: dropping the shutdown half of the condition
    /// (`token.is_cancelled()` alone). That reads correctly for this case and
    /// only fails the next one, which is exactly why the pair exists.
    #[test]
    fn a_cancel_while_running_is_a_deliberate_stop() {
        // Kills the mutant that records a parent's own stop as a failure, blaming the parent for a decision it made on purpose.
        with_clean_flag(|| {
            let token = CancellationToken::new();
            token.cancel();
            assert!(
                is_deliberate_stop(&token),
                "the parent stopped this child; recording it as a failure blames the parent's own decision"
            );
        });
    }

    /// The same cancel DURING a shutdown is not a stop.
    ///
    /// The mutant this kills: `token.is_cancelled()` alone, and any version
    /// that reads the flag before the token (which would answer "stop" for a
    /// child that was merely cancelled a moment earlier). Both would mark a
    /// shutdown-interrupted child `Cancelled` — terminal, resumable, and a
    /// silent lie about why it stopped.
    #[test]
    fn a_cancel_during_shutdown_is_not_a_deliberate_stop() {
        // Kills the mutant that reads a shutdown-time cancel as a deliberate stop, marking resumable work as a decision nobody made.
        with_clean_flag(|| {
            let token = CancellationToken::new();
            token.cancel();
            shutdown::mark_shutting_down();
            assert!(
                !is_deliberate_stop(&token),
                "a shutdown cancelled this child — calling it a stop marks a resumable record for work that was interrupted"
            );
        });
    }
}
// ─── #1080: an interrupted child is not a failed one ───────────────────────
//
// The defect these pin: after a daemon restart every live child was
// reconciled to `Failed("stale after restart")`, and a parent reading that
// concluded the work was DESTROYED and re-spawned it. Measured on this box
// 2026-11-28: 7 children reported `Interrupted` with an empty `output_full`
// while 4.3 MB across 35 messages sat intact in `opencrabs.db`, and 4 children
// were re-spawned for work already done. `WorkState` has seven variants and
// `SubAgentState` had five; the projection between them collapsed the one
// state whose meaning is "I do not know, the work may be intact" into the one
// that means "it broke".
mod interrupted_state {
    use crate::brain::agent::service::work_status::test_override;
    use crate::brain::agent::service::work_status::{AgentGrant, WorkState, WorkStatus};
    use crate::brain::tools::subagent::{SubAgentManager, SubAgentState};
    use std::fs;
    use uuid::Uuid;

    /// A status directory of this test's own. The override is thread-local and
    /// the test runner reuses threads, so the dir is cleared first.
    fn isolate(tag: &str) {
        let dir = std::env::temp_dir().join(format!(
            "opencrabs-1080-interrupted-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        test_override::set(dir.join("detached"));
    }

    /// Write the record a restart leaves behind: an agent status whose state is
    /// `Interrupted`, as `mark_interrupted` writes it.
    fn interrupted_on_disk(id: &str, parent: Uuid) -> WorkStatus {
        let child = Uuid::new_v4();
        let mut status = WorkStatus::new_agent(
            id,
            "worker",
            &child.to_string(),
            "do the thing",
            Some(&parent.to_string()),
            Some(&AgentGrant::legacy()),
        )
        .expect("write status");
        status.mark_interrupted().expect("mark interrupted");
        status
    }

    /// The mapping itself: `WorkState::Interrupted` must land on
    /// `SubAgentState::Interrupted`, never on `Failed`.
    ///
    /// This is the assertion that would have caught the defect at its source.
    /// On the pre-fix tree `manager.rs:386` read
    /// `WorkState::Interrupted => SubAgentState::Failed("interrupted by
    /// restart")`, so every rehydrated child — which is EVERY child after a
    /// restart — was announced to its parent as a failure.
    #[test]
    fn interrupted_work_state_maps_to_interrupted_not_failed() {
        isolate("mapping");
        let parent = Uuid::new_v4();
        let status = interrupted_on_disk("map-1", parent);
        assert_eq!(
            status.state,
            WorkState::Interrupted,
            "precondition: the record on disk says Interrupted"
        );

        let manager = SubAgentManager::new();
        let view = manager.resolve("map-1", parent).expect("resolve");

        assert_eq!(
            view.state,
            SubAgentState::Interrupted,
            "WorkState::Interrupted must project to SubAgentState::Interrupted — \
             collapsing it into Failed is what made parents re-spawn intact work"
        );
        assert!(
            !matches!(view.state, SubAgentState::Failed(_)),
            "an interrupted child did not fail; the restart killed the process"
        );
        // And the map holds the same verdict the view reported, so a later
        // `get_state` cannot disagree with what the parent was told.
        assert_eq!(manager.get_state("map-1"), Some(SubAgentState::Interrupted));
    }

    /// `Interrupted` is accepted by the resume precondition.
    ///
    /// The state was never blocking — `resume.rs` matched `Failed(_)`, which
    /// is what the projection produced — but now that it is its own variant
    /// the acceptance has to be STATED, or correcting the projection would
    /// have started refusing exactly the child whose work survived.
    #[test]
    fn interrupted_state_is_accepted_by_the_resume_gate() {
        assert!(
            SubAgentState::Interrupted.is_resumable(),
            "Interrupted must be resumable: the child's session, transcript and \
             grant all survive the restart, so `resume_agent` continues it"
        );
        // `prepare_resume` is the manager-side half of the same gate, and it
        // must agree with the tool-side predicate. The child is put into the
        // map by the REAL resolve path — a record on disk, as a restart leaves
        // it — rather than by poking the state directly, so this pins the
        // production route the resume tool actually takes (#852 + #1080).
        isolate("gate");
        let parent = Uuid::new_v4();
        interrupted_on_disk("gate-1", parent);
        let manager = SubAgentManager::new();
        let view = manager.resolve("gate-1", parent).expect("resolve");
        assert_eq!(
            view.state,
            SubAgentState::Interrupted,
            "precondition: the rehydrated child is Interrupted"
        );

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        assert!(
            manager.prepare_resume("gate-1", tokio_util::sync::CancellationToken::new(), tx),
            "prepare_resume must accept an Interrupted child — the same rule as \
             the tool-side gate, or one half refuses what the other accepts"
        );
        assert_eq!(
            manager.get_state("gate-1"),
            Some(SubAgentState::Running),
            "a resumed child is Running again; leaving it Interrupted would make \
             the next `wait_agent` report a restart that already happened"
        );
    }

    /// The three states must stay distinguishable where a parent READS them:
    /// the label, the resume advice, and the terminal predicate.
    ///
    /// A parent's decision is made off these strings, so a variant that renders
    /// like `Failed` is functionally `Failed` no matter what the enum says.
    #[test]
    fn interrupted_renders_distinctly_from_failed() {
        let interrupted = SubAgentState::Interrupted;
        let failed = SubAgentState::Failed("stale after restart".to_string());

        assert_ne!(
            interrupted.label(),
            failed.label(),
            "rendering Interrupted as `failed` is the whole defect at the text layer"
        );
        // `label()` is a `&'static str`, so the sentence lives on the enum
        // rather than being formatted per call site. Asserted by content, not
        // by equality: the wording may be sharpened without breaking the
        // contract this test actually protects.
        let label = interrupted.label();
        assert!(
            label.contains("interrupted") && label.contains("resume_agent"),
            "the label itself must name the state and the verb that continues it, got: {label}"
        );
        assert!(
            !label.starts_with("failed"),
            "the label must not open with a verdict it did not reach, got: {label}"
        );
        let advice = interrupted
            .advice()
            .expect("an interrupted child must say what to do next");
        assert!(
            advice.contains("resume_agent"),
            "the advice must name the verb that continues the work, got: {advice}"
        );
        assert!(
            failed.advice().is_none(),
            "a genuine failure has no `continue it` advice to give"
        );
        // Terminal for LIVENESS, resumable for CONTINUATION: the two questions
        // are different and the enum has to answer both correctly.
        assert!(interrupted.is_terminal(), "nothing is executing it");
        assert!(interrupted.is_resumable(), "but its context survives");
    }

    /// A resumed round must not inherit the previous round's result (#1080,
    /// cause 2). `begin_round` is what `resume_agent` now calls before its
    /// first turn.
    #[test]
    fn a_new_round_clears_the_previous_rounds_output() {
        isolate("begin_round");
        let child = Uuid::new_v4();
        let mut status = WorkStatus::new_agent(
            "round-1",
            "worker",
            &child.to_string(),
            "first round",
            Some(&Uuid::new_v4().to_string()),
            Some(&AgentGrant::legacy()),
        )
        .expect("write status");
        status.mark_running().expect("running");
        status
            .mark_completed("ROUND ONE RESULT".to_string())
            .expect("complete");

        let reread = WorkStatus::read("round-1").expect("record");
        assert_eq!(reread.state, WorkState::Completed);
        assert_eq!(
            reread
                .finish
                .as_ref()
                .and_then(|f| f.output_full.as_deref()),
            Some("ROUND ONE RESULT"),
            "precondition: the first round's result is on disk"
        );

        let mut handle = WorkStatus::read("round-1").expect("record");
        handle.begin_round().expect("begin round");

        let reread = WorkStatus::read("round-1").expect("record");
        assert_eq!(
            reread.state,
            WorkState::Running,
            "a resumed round is RUNNING, not the completion of the round before it"
        );
        assert!(
            reread.finish.is_none(),
            "the previous round's finish stamp — output_full with it — must be \
             cleared, or `task_output` hands the parent the OLD answer as the \
             new one: a silent wrong answer, not a crash"
        );
    }
}
