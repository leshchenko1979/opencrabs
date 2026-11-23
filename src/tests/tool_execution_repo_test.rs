//! Tests for `ToolExecutionRepository`.
//!
//! Pins the empty-`tool_name` guard: on 2026-04-28 a model emitted 44
//! `tool_use` blocks with no name field; each dispatch errored but the
//! failure still got recorded with `tool_name = ""`, producing a blank
//! row in the /usage dashboard's "Core Tools" card. The repository now
//! refuses to insert empties.

use crate::db::Database;
use crate::db::repository::{STATUS_STARTED, ToolExecutionRepository};

async fn make_db() -> Database {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    db
}

#[tokio::test]
async fn record_skips_empty_tool_name() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    repo.record(
        "id-empty", "msg-1", "sess-1", "", "error", None, None, None, None,
    )
    .await
    .expect("empty record returns Ok, just skips the insert");
    let stats = repo.stats_by_tool(None).await.unwrap();
    assert!(stats.is_empty(), "empty tool_name must not land in DB");
}

#[tokio::test]
async fn record_skips_whitespace_only_tool_name() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    repo.record(
        "id-ws", "msg-1", "sess-1", "   \t  ", "error", None, None, None, None,
    )
    .await
    .expect("whitespace-only is treated as empty");
    let stats = repo.stats_by_tool(None).await.unwrap();
    assert!(stats.is_empty());
}

#[tokio::test]
async fn record_accepts_normal_tool_name() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    repo.record(
        "id-bash",
        "msg-1",
        "sess-1",
        "bash",
        "completed",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    repo.record(
        "id-grep",
        "msg-2",
        "sess-1",
        "grep",
        "completed",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let stats = repo.stats_by_tool(None).await.unwrap();
    assert_eq!(stats.len(), 2);
    let names: Vec<&str> = stats.iter().map(|s| s.tool_name.as_str()).collect();
    assert!(names.contains(&"bash"));
    assert!(names.contains(&"grep"));
}

#[tokio::test]
async fn stats_by_tool_filters_legacy_empty_rows() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    // Insert directly via the raw pool, bypassing the guard, to simulate
    // legacy pre-guard rows that already live in users' production DBs.
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| {
            conn.execute(
                "INSERT INTO tool_executions (id, message_id, session_id, tool_name, status) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params!["legacy-1", "msg-1", "sess-1", "", "error"],
            )
        })
        .await
        .unwrap()
        .unwrap();
    repo.record(
        "id-bash",
        "msg-2",
        "sess-1",
        "bash",
        "completed",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let stats = repo.stats_by_tool(None).await.unwrap();
    assert_eq!(
        stats.len(),
        1,
        "the legacy empty-name row must be filtered out by the SELECT"
    );
    assert_eq!(stats[0].tool_name, "bash");
}

// #687: garbage tool names from phantom tool calls must be rejected.
#[tokio::test]
async fn record_skips_garbage_tool_name_with_xml_fragment() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    let garbage = "i_apologize... i'll_call_write_opencrabs_file...";
    repo.record(
        "id-garbage",
        "msg-1",
        "sess-1",
        garbage,
        "error",
        None,
        None,
        None,
        None,
    )
    .await
    .expect("garbage record returns Ok, just skips the insert");
    let stats = repo.stats_by_tool(None).await.unwrap();
    assert!(stats.is_empty(), "garbage tool_name must not land in DB");
}

#[tokio::test]
async fn record_skips_tool_name_with_uppercase() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    repo.record(
        "id-upper", "msg-1", "sess-1", "Bash", "error", None, None, None, None,
    )
    .await
    .unwrap();
    let stats = repo.stats_by_tool(None).await.unwrap();
    assert!(stats.is_empty(), "uppercase tool_name must be rejected");
}

#[tokio::test]
async fn record_skips_overlong_tool_name() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    let long_name = "a".repeat(65);
    repo.record(
        "id-long", "msg-1", "sess-1", &long_name, "error", None, None, None, None,
    )
    .await
    .unwrap();
    let stats = repo.stats_by_tool(None).await.unwrap();
    assert!(stats.is_empty(), "tool_name >64 chars must be rejected");
}

#[tokio::test]
async fn record_accepts_tool_name_with_digits_and_underscores() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    repo.record(
        "id-ok",
        "msg-1",
        "sess-1",
        "web_search_v2",
        "success",
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    let stats = repo.stats_by_tool(None).await.unwrap();
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].tool_name, "web_search_v2");
}

/// #787: a detached run's row is written twice — once when the run is SPAWNED
/// (no code yet) and once when it ENDS. The second write is the whole point:
/// without it every detached run stays recorded as a successful `bash` call
/// with no exit code, whatever it went on to do.
#[tokio::test]
async fn update_outcome_writes_the_runs_real_result() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    // The spawn-time row, exactly as `tool_loop` writes it: the run has not
    // exited, so there is no code, and the spawn itself succeeded.
    repo.record(
        "run-1", "msg-1", "sess-1", "bash", "success", None, None, None, None,
    )
    .await
    .unwrap();

    let touched = repo
        .update_outcome("run-1", Some(7), "success")
        .await
        .expect("update succeeds");
    assert_eq!(touched, 1, "the row exists, so exactly one row is touched");

    let pool = db.pool().clone();
    let row: (Option<i32>, String) = pool
        .get()
        .await
        .unwrap()
        .interact(move |conn| {
            conn.query_row(
                "SELECT exit_code, status FROM tool_executions WHERE id = 'run-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.0, Some(7), "the run's real exit code lands on the row");
    assert_eq!(row.1, "success", "#763's split is not restated here");
}

/// The insert is spawned and can lose the race against a fast run, so a zero
/// is a RETRY signal, not a verdict — the caller must be able to tell the two
/// apart, which is why the count is returned rather than swallowed.
#[tokio::test]
async fn update_outcome_on_a_missing_row_reports_zero() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    let touched = repo
        .update_outcome("never-inserted", Some(0), "success")
        .await
        .expect("a missing row is not an error");
    assert_eq!(touched, 0, "no row yet is reported, never invented");
}

// ── Safe auto-resume: the turn journal ──────────────────────────────────────
//
// `record()` runs in a spawned task AFTER the tool returns, so a turn killed
// mid-call leaves no row at all. `record_started` opens the row BEFORE
// dispatch; `update_outcome` closes it; `unknown_for_turn` is the boot read
// that finds the rows nobody closed. These legs pin the round trip and the
// retention boundary.

/// Insert a row with an explicit `created_at` so the sweep's cutoff can be
/// exercised without waiting on the clock.
async fn insert_row(db: &Database, id: &str, status: &str, created_at: i64) {
    let id = id.to_string();
    let status = status.to_string();
    db.pool()
        .get()
        .await
        .unwrap()
        .interact(move |conn| {
            conn.execute(
                "INSERT INTO tool_executions \
                     (id, message_id, session_id, turn_id, tool_name, arguments, status, created_at) \
                     VALUES (?1, 'msg-1', 'sess-1', 'turn-1', 'bash', '{}', ?2, ?3)",
                rusqlite::params![id, status, created_at],
            )
        })
        .await
        .unwrap()
        .unwrap();
}

/// The round trip boot depends on: a row written BEFORE the tool runs is the
/// only evidence an interrupted call leaves, and it must read back as unknown.
#[tokio::test]
async fn record_started_round_trips_into_unknown_for_turn() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    repo.record_started(
        "call-1",
        "msg-1",
        "sess-1",
        "turn-1",
        "bash",
        r#"{"command":"git push origin main"}"#,
    )
    .await
    .unwrap();

    let unknown = repo.unknown_for_turn("turn-1").await.unwrap();
    assert_eq!(
        unknown.len(),
        1,
        "the started row must be the one unknown call"
    );
    assert_eq!(unknown[0].id, "call-1");
    assert_eq!(unknown[0].tool_name, "bash");
    assert_eq!(
        unknown[0].arguments.as_deref(),
        Some(r#"{"command":"git push origin main"}"#),
        "the digest needs the arguments, not just the tool name"
    );

    let status: String = db
        .pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| {
            conn.query_row(
                "SELECT status FROM tool_executions WHERE id = 'call-1'",
                [],
                |r| r.get(0),
            )
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        status, STATUS_STARTED,
        "the journal marker is `started`, not an outcome"
    );

    assert!(
        repo.unknown_for_turn("turn-2").await.unwrap().is_empty(),
        "the read is scoped to one turn"
    );
}

/// A row closed by `update_outcome` is NOT unknown, and a sibling turn's row
/// never leaks into this turn's digest.
#[tokio::test]
async fn unknown_for_turn_excludes_calls_that_reported_an_outcome() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    repo.record_started(
        "call-done",
        "msg-1",
        "sess-1",
        "turn-1",
        "write_opencrabs_file",
        r#"{"path":"MEMORY.md"}"#,
    )
    .await
    .unwrap();
    let touched = repo
        .update_outcome("call-done", None, "success")
        .await
        .unwrap();
    assert_eq!(
        touched, 1,
        "update_outcome closes the journal row it opened"
    );

    repo.record_started("call-open", "msg-1", "sess-1", "turn-1", "bash", "{}")
        .await
        .unwrap();
    repo.record_started("call-other", "msg-2", "sess-1", "turn-2", "bash", "{}")
        .await
        .unwrap();

    let unknown = repo.unknown_for_turn("turn-1").await.unwrap();
    assert_eq!(
        unknown.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        vec!["call-open"],
        "only the unfinished call of THIS turn is unknown"
    );
}

/// The boot reconciler stamps `started` → `unknown`; the read must see both,
/// so a resumed turn does not depend on whether the reconciler ran first.
#[tokio::test]
async fn unknown_for_turn_reads_a_row_the_reconciler_stamped_unknown() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    repo.record_started("call-1", "msg-1", "sess-1", "turn-1", "bash", "{}")
        .await
        .unwrap();

    db.pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| {
            conn.execute(
                "UPDATE tool_executions SET status = 'unknown' WHERE id = 'call-1'",
                [],
            )
        })
        .await
        .unwrap()
        .unwrap();

    let unknown = repo.unknown_for_turn("turn-1").await.unwrap();
    assert_eq!(
        unknown.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        vec!["call-1"],
        "a reconciled row is still unknown work"
    );
}

/// `INSERT OR IGNORE`, not an upsert: a retried dispatch must not double the
/// row or overwrite what the first attempt recorded.
#[tokio::test]
async fn record_started_is_idempotent_and_keeps_the_first_row() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    repo.record_started(
        "call-1",
        "msg-1",
        "sess-1",
        "turn-1",
        "bash",
        r#"{"command":"git status"}"#,
    )
    .await
    .unwrap();
    repo.record_started(
        "call-1",
        "msg-1",
        "sess-1",
        "turn-1",
        "bash",
        r#"{"command":"git push"}"#,
    )
    .await
    .expect("a duplicate id is ignored, not an error");

    let unknown = repo.unknown_for_turn("turn-1").await.unwrap();
    assert_eq!(unknown.len(), 1, "one call id, one row");
    assert_eq!(
        unknown[0].arguments.as_deref(),
        Some(r#"{"command":"git status"}"#),
        "the first journal entry is authoritative; a replay must not rewrite it"
    );
}

/// The name guard is shared with `record`: a journaled call whose name the
/// analytics path rejects would be a row that can never be completed usefully.
#[tokio::test]
async fn record_started_refuses_blank_and_malformed_tool_names() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    repo.record_started("blank", "msg-1", "sess-1", "turn-1", "", "{}")
        .await
        .unwrap();
    repo.record_started("upper", "msg-1", "sess-1", "turn-1", "Bash", "{}")
        .await
        .unwrap();

    assert!(repo.unknown_for_turn("turn-1").await.unwrap().is_empty());
    let count: i64 = db
        .pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| conn.query_row("SELECT COUNT(*) FROM tool_executions", [], |r| r.get(0)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(count, 0, "neither refused call may leave a row");
}

/// A HYPHENATED name is a real tool name and must journal (#974).
///
/// The name guard used to accept only `[a-z0-9_]`, so every `oc-*` tool was
/// silently dropped — while `classify::classify_call` classes `oc-ship-chain`
/// and `oc-deploy` as DESTRUCTIVE. The two disagreed exactly where it costs
/// most: the destructive calls the restart park gate exists to catch were the
/// ones that never got a journal row, so a restart mid-ship could not park.
/// This leg is the mutant-killer for that guard: restore the underscore-only
/// set and it goes red.
#[tokio::test]
async fn record_started_journals_a_hyphenated_oc_tool_name() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    repo.record_started(
        "ship",
        "msg-1",
        "sess-1",
        "turn-1",
        "oc-ship-chain",
        r#"{"sha":"deadbeef"}"#,
    )
    .await
    .unwrap();

    let unknown = repo.unknown_for_turn("turn-1").await.unwrap();
    assert_eq!(
        unknown.len(),
        1,
        "an oc-* name must journal — the park gate reads these rows"
    );
    assert_eq!(unknown[0].tool_name, "oc-ship-chain");
}

/// Retention for the journal's own debris: unresolved rows past the cutoff go,
/// completed rows are analytics and stay (the message prune owns those).
#[tokio::test]
async fn sweep_older_than_deletes_only_stale_unresolved_rows() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let stale = now - 86_400;

    insert_row(&db, "stale-started", "started", stale).await;
    insert_row(&db, "stale-unknown", "unknown", stale).await;
    insert_row(&db, "stale-done", "success", stale).await;
    insert_row(&db, "fresh-started", "started", now).await;

    let deleted = repo.sweep_older_than(now - 3_600).await.unwrap();
    assert_eq!(
        deleted, 2,
        "both stale unresolved rows are swept; the completed and fresh rows are not"
    );

    let ids: Vec<String> = db
        .pool()
        .get()
        .await
        .unwrap()
        .interact(|conn| {
            conn.prepare("SELECT id FROM tool_executions ORDER BY id")?
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        ids,
        vec!["fresh-started".to_owned(), "stale-done".to_owned()],
        "completed rows are never the journal sweep's business"
    );
}

#[tokio::test]
async fn sweep_older_than_on_a_clean_table_reports_zero() {
    let db = make_db().await;
    let repo = ToolExecutionRepository::new(db.pool().clone());
    let deleted = repo.sweep_older_than(4_000_000_000).await.unwrap();
    assert_eq!(deleted, 0);
}
