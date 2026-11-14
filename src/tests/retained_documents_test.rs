//! Retention of read `.md` documents across compaction (#831).
//!
//! The registration half is the safety boundary: a model names paths, and the
//! harness accepts only the ones this session actually read whole in this
//! process. These tests drive the public `retained_docs` surface — never the
//! private registry — so a change in the keying or the ordering is visible.

use crate::brain::tools::read_state::mark_document_read;
use crate::brain::tools::retained_docs::{
    DocStatus, MAX_DOC_BODY_BYTES, MAX_RETAINED_DOCUMENTS, register, registered_for_session, render,
};
use uuid::Uuid;

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("retained_docs_{tag}_{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write `<dir>/<name>.md` and record a whole-file read for `sid`.
fn read_document(sid: Uuid, dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("# {name}\nbody\n")).unwrap();
    mark_document_read(sid, &path);
    path.canonicalize().unwrap()
}

#[test]
fn register_accepts_only_documents_this_session_read() {
    let sid = Uuid::new_v4();
    let dir = tmp_dir("accept");
    let read = read_document(sid, &dir, "CLAUDE.md");
    // Present on disk, never read by this session.
    let unread = dir.join("AGENTS.md");
    std::fs::write(&unread, "# agents\n").unwrap();

    let requested = vec![
        read.display().to_string(),
        unread.display().to_string(),
    ];
    let (registered, rejected) = register(sid, &requested);

    assert_eq!(registered.len(), 1, "only the read document is carried");
    assert_eq!(registered[0].path, read);
    assert_eq!(registered[0].status, DocStatus::Ok);
    assert!(registered[0].tokens > 0, "a measured row carries a token count");
    assert_eq!(rejected.len(), 1);
    assert!(
        rejected[0].contains("(not read in this process)"),
        "a refusal must say WHY, got: {}",
        rejected[0]
    );
    assert_eq!(registered_for_session(sid), registered);

    std::fs::remove_dir_all(&dir).ok();
}

/// The document class exists so project law survives compaction. It must not
/// become a side door into the skill tree, the agent's own home, or a
/// confidential path — even when the session really did read one.
#[test]
fn register_rejects_skill_and_confidential_paths() {
    let sid = Uuid::new_v4();
    let home = crate::config::profile::resolve_profile_home();
    let skill = home.join("skills/opencrabs-dev/SKILL.md");
    let brain = home.join("MEMORY.md");

    // The tracking hook refuses these too, so the read leg can never have
    // recorded them — and the registration leg must refuse them on its own
    // terms rather than lean on an empty read list.
    mark_document_read(sid, &skill);
    mark_document_read(sid, &brain);
    assert!(!crate::brain::tools::read_state::was_document_read(sid, &skill));
    assert!(!crate::brain::tools::read_state::was_document_read(sid, &brain));

    let requested = vec![
        skill.display().to_string(),
        brain.display().to_string(),
    ];
    let (registered, rejected) = register(sid, &requested);

    assert!(registered.is_empty(), "neither path is a project document");
    assert_eq!(rejected.len(), 2);
    for line in &rejected {
        assert!(
            line.contains("(not a project document)"),
            "got: {line}"
        );
    }
}

#[test]
fn register_is_capped_at_max_retained_documents() {
    let sid = Uuid::new_v4();
    let dir = tmp_dir("cap");

    // One under the cap.
    let under: Vec<String> = (0..MAX_RETAINED_DOCUMENTS - 1)
        .map(|i| {
            read_document(sid, &dir, &format!("doc_{i}.md"))
                .display()
                .to_string()
        })
        .collect();
    let (registered, rejected) = register(sid, &under);
    assert_eq!(registered.len(), MAX_RETAINED_DOCUMENTS - 1);
    assert!(rejected.is_empty());

    // Exactly the cap.
    let at: Vec<String> = (0..MAX_RETAINED_DOCUMENTS)
        .map(|i| {
            read_document(sid, &dir, &format!("doc_{i}.md"))
                .display()
                .to_string()
        })
        .collect();
    let (registered, rejected) = register(sid, &at);
    assert_eq!(registered.len(), MAX_RETAINED_DOCUMENTS);
    assert!(rejected.is_empty());

    // One over the cap: the overflow is refused by name, never dropped silently.
    let over: Vec<String> = (0..MAX_RETAINED_DOCUMENTS + 1)
        .map(|i| {
            read_document(sid, &dir, &format!("doc_{i}.md"))
                .display()
                .to_string()
        })
        .collect();
    let (registered, rejected) = register(sid, &over);
    assert_eq!(registered.len(), MAX_RETAINED_DOCUMENTS);
    assert_eq!(rejected.len(), 1);
    assert!(rejected[0].contains("document cap"), "got: {}", rejected[0]);

    std::fs::remove_dir_all(&dir).ok();
}

/// A later compaction's manifest REPLACES the earlier set, and registering the
/// same list twice is a no-op — the summariser re-emitting its own answer must
/// not double anything or reorder it.
#[test]
fn register_is_idempotent() {
    let sid = Uuid::new_v4();
    let dir = tmp_dir("idem");
    let a = read_document(sid, &dir, "a.md");
    let b = read_document(sid, &dir, "b.md");
    let requested = vec![a.display().to_string(), b.display().to_string()];

    let first = register(sid, &requested);
    let second = register(sid, &requested);
    assert_eq!(first.0, second.0, "re-registering the same list changes nothing");
    assert_eq!(second.1.len(), 0);

    // …and a manifest that names only one of them supersedes, not merges.
    let (replaced, _) = register(sid, &requested[..1]);
    assert_eq!(replaced.len(), 1);
    assert_eq!(registered_for_session(sid).len(), 1);

    std::fs::remove_dir_all(&dir).ok();
}

/// Registration happens while a compaction is applied; the read happens at the
/// NEXT turn's brain assembly. Nothing is threaded between them, so the store
/// has to be process-wide and keyed by session — the two calls below share no
/// local state at all, and a per-instance map would return nothing here.
#[test]
fn register_survives_across_call_sites() {
    let sid = Uuid::new_v4();
    let dir = tmp_dir("across");
    let path = read_document(sid, &dir, "law.md");
    register(sid, &[path.display().to_string()]);

    let carried = registered_for_session(sid);
    assert_eq!(carried.len(), 1);
    assert_eq!(carried[0].path, path);

    // Another session's registration never leaks into this one.
    let other = Uuid::new_v4();
    assert!(registered_for_session(other).is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

/// Write `<dir>/<name>.md` with `body`, record the read, and hand back the
/// canonical path. Registration is the CALLER's job: `register` replaces the
/// session's set, so a helper that registered per file would leave only the
/// last one in place.
fn write_document(sid: Uuid, dir: &std::path::Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    mark_document_read(sid, &path);
    path.canonicalize().unwrap().display().to_string()
}

#[test]
fn render_header_and_body_are_present_for_a_small_document() {
    let sid = Uuid::new_v4();
    let dir = tmp_dir("render_small");
    let path = write_document(sid, &dir, "law.md", "# law\nobey the law\n");
    register(sid, std::slice::from_ref(&path));

    let block = render(sid, 100_000).expect("a document that fits is rendered");

    assert!(block.contains("## Retained documents"), "got: {block}");
    assert!(block.contains(&format!("### {path}")), "got: {block}");
    assert!(block.contains("obey the law"), "the BODY is what was retained");
    assert!(block.starts_with("\n\n"), "the separator matches skill bodies");

    std::fs::remove_dir_all(&dir).ok();
}

/// The budget is a leftover, so a smaller one has to produce LESS text. Pinned
/// as a comparison rather than an absolute count: the tokenizer may change, the
/// direction must not.
#[test]
fn render_respects_the_leftover_budget() {
    let sid = Uuid::new_v4();
    let dir = tmp_dir("render_budget");
    let body: String = (0..200).map(|i| format!("line {i} of the project law\n")).collect();
    let a = write_document(sid, &dir, "a.md", &body);
    let b = write_document(sid, &dir, "b.md", &body);
    let c = write_document(sid, &dir, "c.md", &body);
    register(sid, &[a, b, c]);

    let generous = render(sid, 1_000_000).expect("room for everything");
    assert_eq!(generous.matches("### ").count(), 3, "all three bodies fit");

    let tight = render(sid, 2_000).expect("room for at least one body");
    assert!(
        tight.len() < generous.len(),
        "a smaller budget must emit less: {} vs {}",
        tight.len(),
        generous.len()
    );
    let tight_count = tight.matches("### ").count();
    assert!(
        (1..3).contains(&tight_count),
        "a mid budget carries some but not all bodies, got {tight_count}"
    );
    // Whatever did fit is the NEWEST read (c.md), never the oldest.
    assert!(tight.contains("c.md"), "got: {tight}");
    assert!(!tight.contains("a.md"), "the oldest must be the one dropped");

    std::fs::remove_dir_all(&dir).ok();
}

/// A body over the per-document cap is skipped whole. Half a document invites
/// the agent to act on law it only saw the first half of.
#[test]
fn render_skips_a_body_over_the_cap() {
    let sid = Uuid::new_v4();
    let dir = tmp_dir("render_cap");
    // One line past the cap, with a marker the assertion can look for.
    let mut big = "BEGIN-OVERSIZE-MARKER\n".to_string();
    while big.len() <= MAX_DOC_BODY_BYTES + 1 {
        big.push_str("padding line for the oversize document\n");
    }
    let big_path = write_document(sid, &dir, "big.md", &big);
    let small_path = write_document(sid, &dir, "small.md", "# small\nshort body\n");
    register(sid, &[big_path.clone(), small_path.clone()]);

    let block = render(sid, 1_000_000).expect("the small one still renders");

    assert!(
        !block.contains("BEGIN-OVERSIZE-MARKER"),
        "an oversize body must not be injected, not even partially"
    );
    assert!(!block.contains(&big_path));
    assert!(block.contains(&small_path));
    assert!(block.contains("short body"));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn render_returns_none_when_the_budget_is_exhausted() {
    let sid = Uuid::new_v4();
    let dir = tmp_dir("render_none");
    let path = write_document(sid, &dir, "law.md", "# law\nobey\n");
    register(sid, &[path]);

    assert!(render(sid, 0).is_none(), "zero room injects nothing");
    assert!(render(sid, 1).is_none(), "one token cannot hold a header");
    // …and nothing is registered ⇒ nothing to render, at any budget.
    assert!(render(Uuid::new_v4(), 1_000_000).is_none());

    std::fs::remove_dir_all(&dir).ok();
}
