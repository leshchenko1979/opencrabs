//! Regression test for #321 Part C — the cause chain must reach the log.
//!
//! The 2026-09-18 22:16 write-failure burst was unfalsifiable: every failing
//! log site rendered the error with plain `{e}`, and `anyhow`'s `Display`
//! prints ONLY the outermost `.context(...)` string. The real cause — a
//! SQLite `database is locked` buried one frame down — never appeared in the
//! line, so the operator could not tell a lock timeout from a schema error.
//!
//! `{e:#}` (alternate `Display`) renders the whole chain instead. These tests
//! pin that property against a REAL SQLite lock error — not a hand-made
//! string — and then guard the source so a revert cannot silently reintroduce
//! the blind spot.

use std::sync::{Arc, Mutex};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;

/// Renders an `anyhow::Error` the way `tracing` does for a `{:#}` field.
fn render_chained(e: &anyhow::Error) -> String {
    format!("{e:#}")
}

/// Renders an `anyhow::Error` the way `tracing` does for a plain `{}` field.
fn render_plain(e: &anyhow::Error) -> String {
    format!("{e}")
}

/// Produce a genuine `SQLITE_BUSY` ("database is locked") from two real
/// connections on one temp file database, wrapped in a `.context(...)` the way
/// the repository methods wrap it.
///
/// Returns `(wrapped, tempdir)`. The `TempDir` must stay alive for as long as
/// the error is inspected — dropping it deletes the database file.
fn locked_write_error() -> (anyhow::Error, tempfile::TempDir) {
    use anyhow::Context;

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cause_chain.db");

    // Holder: acquire the write lock and keep it.
    let holder = rusqlite::Connection::open(&path).expect("open holder");
    holder
        .execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE t (v INTEGER);
             BEGIN IMMEDIATE;
             INSERT INTO t VALUES (1);",
        )
        .expect("holder takes the write lock");

    // Contender: no patience, so the busy error surfaces immediately.
    let contender = rusqlite::Connection::open(&path).expect("open contender");
    contender
        .busy_timeout(std::time::Duration::from_millis(0))
        .expect("busy_timeout");
    let raw = contender
        .execute("INSERT INTO t VALUES (2)", [])
        .expect_err("contender must hit the lock");

    // Same shape the repositories produce: typed rusqlite error, then a
    // context frame added on top by `.context(...)`.
    let wrapped = Err::<(), rusqlite::Error>(raw)
        .context("Failed to record feedback")
        .expect_err("the wrapped error is the subject of this fixture");
    (wrapped, dir)
}

#[test]
fn real_lock_error_is_buried_by_context_under_plain_display() {
    let (err, _dir) = locked_write_error();

    let plain = render_plain(&err);
    assert!(
        plain.contains("Failed to record feedback"),
        "the outer context must be present; got: {plain}"
    );
    assert!(
        !plain.contains("database is locked"),
        "plain Display must hide the cause (that is the defect Part C fixes); got: {plain}"
    );
}

#[test]
fn real_lock_error_is_visible_under_alternate_display() {
    let (err, _dir) = locked_write_error();

    let chained = render_chained(&err);
    assert!(
        chained.contains("Failed to record feedback"),
        "the outer context must still be present; got: {chained}"
    );
    assert!(
        chained.contains("database is locked"),
        "the SQLite cause must reach the rendered string; got: {chained}"
    );
}

/// The predicate in `src/db/retry.rs` is the sibling half of this issue
/// (Part B): a chain-blind predicate cannot retry what it cannot see. Pinning
/// it here keeps the two halves honest against the same fixture.
#[test]
fn the_lock_predicate_agrees_with_the_real_error() {
    let (err, _dir) = locked_write_error();

    let sqlite = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<rusqlite::Error>())
        .expect("the rusqlite cause must be reachable through the chain");
    assert!(
        crate::db::retry::is_database_locked(sqlite),
        "a real SQLITE_BUSY must classify as locked"
    );

    // The chain-blind text test that `retry_db_operation` applies to a
    // generic `E: Display` (B1/B2 of #321) sees only the top-level string, so
    // it does NOT fire on this real lock error — which is why the typed
    // entry point is required. Documented here so the two halves of the
    // issue are pinned against the same fixture.
    let top_level = render_plain(&err).to_lowercase();
    assert!(
        !top_level.contains("locked") && !top_level.contains("busy"),
        "a lock signal leaking into the top-level Display would mask the defect; got: {top_level}"
    );
}

// ---------------------------------------------------------------------------
// Captured-event half: prove the rendered tracing field carries the cause.
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct EventCapture {
    messages: Arc<Mutex<Vec<String>>>,
}

impl EventCapture {
    fn messages(&self) -> Vec<String> {
        self.messages.lock().unwrap().clone()
    }
}

impl<S: tracing::Subscriber> Layer<S> for EventCapture {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        self.messages
            .lock()
            .unwrap()
            .push(visitor.message.unwrap_or_default());
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: Option<String>,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        }
    }
}

#[test]
fn the_logged_line_carries_the_sqlite_cause() {
    let (err, _dir) = locked_write_error();

    let capture = EventCapture::default();
    // No level filter: a bare `Registry` is unfiltered, so the event below is
    // dispatched. Same shape as the capture precedent in
    // `src/tests/tracing_session_id_test.rs`.
    let subscriber = tracing_subscriber::registry().with(capture.clone());

    tracing::subscriber::with_default(subscriber, || {
        // ERROR, not the `debug!` the production feedback site uses: another
        // test in this binary installs a global subscriber capped at WARN
        // (`governor_gates_test.rs`), which lowers `LevelFilter::current()`
        // process-wide and would silently drop a `debug!` callsite depending
        // on test scheduling. ERROR survives any hint at or above it, so this
        // stays deterministic while still proving the property under test —
        // an inline-captured `{err:#}` reaches the emitted tracing message.
        tracing::error!("feedback ledger write failed: {err:#}");
    });

    let messages = capture.messages();
    assert_eq!(messages.len(), 1, "exactly one event must be captured");
    assert!(
        messages[0].contains("database is locked"),
        "the emitted log line must name the cause; got: {}",
        messages[0]
    );
}

// ---------------------------------------------------------------------------
// Source guard: a revert to `{e}` must fail CI, not just go unnoticed.
// ---------------------------------------------------------------------------

/// `(file, log-message fragment that must render its cause)`. One entry per
/// distinct cause-hiding site fixed by #321 Part C.
const CAUSE_BEARING_SITES: &[(&str, &str)] = &[
    (
        "src/brain/agent/service/tool_loop.rs",
        "failed to append iteration content to DB:",
    ),
    (
        "src/brain/agent/service/tool_loop.rs",
        "[TOOL_EXEC] Failed to record tool execution:",
    ),
    (
        "src/brain/agent/service/feedback.rs",
        "feedback ledger write failed:",
    ),
];

#[test]
fn every_cause_bearing_log_site_renders_the_chain() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));

    for (rel, fragment) in CAUSE_BEARING_SITES {
        let text =
            std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"));

        let mut hits = 0usize;
        for line in text.lines() {
            if !line.contains(fragment) {
                continue;
            }
            hits += 1;
            assert!(
                line.contains("{e:#}") || line.contains("{:#}"),
                "{rel}: cause-hiding log site reverted — `{fragment}` must render the \
                 chain via `{{e:#}}` or `{{:#}}`, found: {}",
                line.trim()
            );
        }
        assert!(
            hits > 0,
            "{rel}: expected at least one site carrying `{fragment}`; the guard is \
             pointing at a message that no longer exists, so update CAUSE_BEARING_SITES"
        );
    }
}
