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
use std::fs;

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
