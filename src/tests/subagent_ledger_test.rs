//! Leg 1 of #852 — the persisted agent grant and the progress ledger.
//!
//! Two things are proved here, both about SURVIVING A RESTART:
//!
//! 1. A status record carries the three permission flags it was spawned with.
//!    Without that, a resumed agent is rebuilt from whatever the rebuild path
//!    assumes, and a `read_only` child restored writable is indistinguishable
//!    from a normal resume — the #1173 bug class.
//! 2. The progress ledger is an append-only sibling of the status file that
//!    dies with it.
//!
//! Every test sandboxes through [`isolate`] so nothing here can reach the real
//! `~/.opencrabs/tmp/detached`.

use crate::brain::agent::service::work_status::*;
use crate::brain::tools::subagent::ledger::{self, LedgerEntry, LedgerKind};
use std::fs;
use std::time::Duration;

/// Point the status dir at a per-test directory under the temp dir.
///
/// The tag is per test, not per file: `test_override` is thread-local but the
/// test runner reuses threads, so a shared tag would let one test read the
/// previous one's files.
fn isolate(tag: &str) {
    let dir = std::env::temp_dir().join(format!(
        "opencrabs-852-ledger-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    test_override::set(dir.join("detached"));
}

// ---------------------------------------------------------------------------
// The grant
// ---------------------------------------------------------------------------

#[test]
fn a_legacy_record_without_a_grant_reads_as_the_pre_852_behaviour() {
    // The shape every pre-#852 status file has: no `grant` key at all. It must
    // parse, and it must NOT be read as "no permissions" — a zeroed default
    // would say `allow_nested: false` and silently strip nesting from an agent
    // that had it. The fallback is the behaviour it was actually produced
    // under: no read-only flag, no brain, nesting allowed.
    let legacy = r#"{
        "id": "legacy-1",
        "kind": "agent",
        "session_id": "sess-child",
        "parent_session_id": "sess-parent",
        "label": "old worker",
        "task": "do the thing",
        "spawned_at": "2026-01-01T00:00:00Z",
        "state": "Completed"
    }"#;
    let status: WorkStatus = serde_json::from_str(legacy).expect("legacy record must parse");
    assert!(
        status.grant.is_none(),
        "an absent grant must stay absent — it is not invented"
    );
    let grant = status.effective_grant();
    assert!(!grant.read_only, "a legacy record was not read-only");
    assert!(!grant.include_brain, "a legacy record got no brain");
    assert!(
        grant.allow_nested,
        "a legacy record could nest — defaulting this false would strip a \
         permission the agent actually had"
    );
}

#[test]
fn the_grant_round_trips_through_the_status_file() {
    isolate("grant_roundtrip");
    let grant = AgentGrant::new(true, true, false);
    WorkStatus::new_agent(
        "grant-1",
        "restricted",
        "sess-child",
        "read only please",
        Some("sess-parent"),
        Some(&grant),
    )
    .expect("write status");

    let read_back = WorkStatus::read("grant-1").expect("status exists");
    assert_eq!(
        read_back.grant,
        Some(grant),
        "all three flags must survive the write — read_only alone is not the \
         grant (#1195 adds allow_nested, and losing it hands a worker nesting \
         rights it never had)"
    );
    assert_eq!(read_back.effective_grant(), grant);
}

#[test]
fn a_command_record_carries_no_grant() {
    isolate("command_grant");
    let s = WorkStatus::new_command("cmd-1", "sess-1", "ls", "ls -la", CommandPaths::none())
        .expect("write status");
    assert!(
        s.grant.is_none(),
        "commands have no grant — the field describes a sub-agent's permissions"
    );
    // And the absence is not serialised as `null`: a reader must be able to
    // tell "no grant key" from "grant explicitly empty".
    let raw = fs::read_to_string(status_path("cmd-1")).expect("status file");
    assert!(
        !raw.contains("\"grant\""),
        "an absent grant must not be written as a null key: {raw}"
    );
}

// ---------------------------------------------------------------------------
// The ledger
// ---------------------------------------------------------------------------

#[test]
fn the_ledger_is_a_jsonl_sibling_of_the_status_file() {
    isolate("path");
    assert_eq!(
        progress_path("abc").file_name().unwrap().to_str().unwrap(),
        "abc.progress.jsonl",
        "the extension must be jsonl: cleanup_stale sweeps on extension == \
         \"json\", and a ledger swept on its own schedule would outlive or \
         precede the agent it narrates"
    );
    assert_eq!(progress_path("abc").parent(), status_path("abc").parent());
}

#[test]
fn a_newline_in_the_text_cannot_split_one_entry_into_two() {
    // The whole point of sanitising: the ledger is line-oriented, and a raw
    // newline produces a truncated entry followed by a line that is not JSON.
    // A reader skipping malformed lines would lose the rest of the message
    // with nothing to show for it.
    isolate("newline");
    ledger::append(
        "nl-1",
        &LedgerEntry::new(LedgerKind::Note, "first line\nsecond line\r\n\tthird"),
    );

    let raw = fs::read_to_string(progress_path("nl-1")).expect("ledger written");
    assert_eq!(
        raw.lines().count(),
        1,
        "one entry is one line, whatever the text contains: {raw}"
    );
    let entries = ledger::read("nl-1", 10);
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].text, "first line second line   third",
        "newlines and tabs become spaces, and the text is trimmed"
    );
}

#[test]
fn overlong_text_is_truncated_rather_than_dropped() {
    isolate("truncate");
    let long = "x".repeat(1000);
    ledger::append("tr-1", &LedgerEntry::new(LedgerKind::Note, long));

    let entries = ledger::read("tr-1", 10);
    assert_eq!(entries.len(), 1, "a long entry is still one entry");
    assert!(
        entries[0].text.chars().count() <= 401,
        "capped at 400 chars plus the ellipsis, got {}",
        entries[0].text.chars().count()
    );
    assert!(entries[0].text.ends_with('…'), "a cut must be visible as a cut");
}

#[test]
fn a_torn_final_line_does_not_hide_the_history_before_it() {
    // A crash mid-write leaves half a line. The entries before it are real and
    // must survive; only the torn line is dropped.
    isolate("torn");
    ledger::append("torn-1", &LedgerEntry::new(LedgerKind::Note, "kept one"));
    ledger::append("torn-1", &LedgerEntry::new(LedgerKind::Note, "kept two"));
    let mut raw = fs::read_to_string(progress_path("torn-1")).unwrap();
    raw.push_str("{\"ts\":\"2026-11-15T00:00:00Z\",\"kind\":\"no");
    fs::write(progress_path("torn-1"), raw).unwrap();

    let entries = ledger::read("torn-1", 10);
    assert_eq!(
        entries.len(),
        2,
        "both complete entries survive the torn tail, got {entries:?}"
    );
    assert_eq!(entries[1].text, "kept two");
}

#[test]
fn read_returns_the_tail_in_chronological_order() {
    isolate("tail");
    for i in 0..10 {
        ledger::append("tail-1", &LedgerEntry::new(LedgerKind::Note, format!("step {i}")));
    }
    let entries = ledger::read("tail-1", 3);
    assert_eq!(entries.len(), 3, "limit takes the last N, not the first N");
    assert_eq!(entries[0].text, "step 7");
    assert_eq!(entries[2].text, "step 9", "oldest first");
}

#[test]
fn an_absent_ledger_is_an_empty_history_not_an_error() {
    isolate("absent");
    assert!(ledger::read("never-existed", 10).is_empty());
    assert!(ledger::is_empty("never-existed"));
}

#[test]
fn the_ledger_dies_with_its_status_record() {
    // A ledger has no lifetime of its own, and the stale sweep never sees a
    // `.jsonl` — so if the record's deletion does not take the ledger with it,
    // the file accumulates forever.
    isolate("paired_delete");
    let mut s = WorkStatus::new_agent("gone-1", "old", "sess", "task", None, None).unwrap();
    s.mark_completed("done".into()).unwrap();
    ledger::append("gone-1", &LedgerEntry::new(LedgerKind::Note, "did the thing"));
    assert!(progress_path("gone-1").exists(), "fixture must have a ledger");

    let old_ts = chrono::Utc::now()
        .checked_sub_signed(chrono::Duration::days(8))
        .unwrap()
        .to_rfc3339();
    let raw = fs::read_to_string(status_path("gone-1")).unwrap();
    let mut parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    parsed["finish"]["completed_at"] = serde_json::json!(old_ts);
    fs::write(status_path("gone-1"), serde_json::to_string(&parsed).unwrap()).unwrap();

    cleanup_stale(Duration::from_secs(7 * 86400)).expect("sweep");
    assert!(
        !status_path("gone-1").exists(),
        "fixture precondition: the status record is swept"
    );
    assert!(
        !progress_path("gone-1").exists(),
        "the ledger must be removed with the record it narrates"
    );
}

#[test]
fn a_live_agents_ledger_is_never_swept() {
    // Control for the test above: the sweep must not touch a ledger whose
    // record is still live, or a running agent's history vanishes mid-run.
    isolate("paired_keep");
    WorkStatus::new_agent("live-1", "busy", "sess", "task", None, None).unwrap();
    ledger::append("live-1", &LedgerEntry::new(LedgerKind::Note, "working"));

    cleanup_stale(Duration::from_secs(7 * 86400)).expect("sweep");
    assert!(status_path("live-1").exists(), "a live record stays");
    assert!(
        progress_path("live-1").exists(),
        "and so does the history it is still writing"
    );
}
