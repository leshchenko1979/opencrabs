//! Session-scoped fully-read path tracking (#1168) and document tracking (#831).

use crate::brain::tools::read::ReadTool;
use crate::brain::tools::read_state::*;
use crate::brain::tools::write::WriteTool;
use crate::brain::tools::{Tool, ToolExecutionContext};
use serde_json::json;
use uuid::Uuid;

fn ctx_in(dir: &std::path::Path) -> ToolExecutionContext {
    let mut ctx = ToolExecutionContext::new(Uuid::new_v4());
    ctx.working_directory = dir.to_path_buf();
    ctx
}

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("read_state_{tag}_{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn tracks_per_session_and_path() {
    let sid = Uuid::new_v4();
    let other_sid = Uuid::new_v4();
    let dir = std::env::temp_dir().join(format!("read_state_{}", sid.simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("f.txt");
    std::fs::write(&file, "x").unwrap();

    assert!(!was_fully_read(sid, &file));
    mark_fully_read(sid, &file);
    assert!(was_fully_read(sid, &file));

    // Another session has not read it.
    assert!(!was_fully_read(other_sid, &file));
    // Relative spelling of the same file resolves to the same key.
    let rel = dir.join("./f.txt");
    assert!(was_fully_read(sid, &rel));

    std::fs::remove_dir_all(&dir).ok();
}

/// The hook fires on a whole-file read of a `.md` and on nothing else.
#[tokio::test]
async fn tracks_only_markdown_documents() {
    let dir = tmp_dir("md_only");
    let md = dir.join("CLAUDE.md");
    let txt = dir.join("notes.txt");
    std::fs::write(&md, "# project law\n").unwrap();
    std::fs::write(&txt, "not law\n").unwrap();

    let ctx = ctx_in(&dir);
    let sid = ctx.session_id;
    let md_key = md.canonicalize().unwrap();

    ReadTool
        .execute(json!({ "path": md.to_string_lossy() }), &ctx)
        .await
        .unwrap();
    assert!(
        was_document_read(sid, &md),
        "a whole-file .md read must be tracked as a document"
    );
    assert_eq!(documents_for_session(sid), vec![md_key]);

    ReadTool
        .execute(json!({ "path": txt.to_string_lossy() }), &ctx)
        .await
        .unwrap();
    assert!(
        !was_document_read(sid, &txt),
        "a non-.md read is not a document"
    );
    assert_eq!(
        documents_for_session(sid).len(),
        1,
        "the .txt read must not add a document"
    );

    // A windowed read of the same .md is not a whole-file read: use a fresh
    // session so the earlier whole-file read cannot mask it.
    let ctx2 = ctx_in(&dir);
    ReadTool
        .execute(
            json!({ "path": md.to_string_lossy(), "start_line": 0, "line_count": 1 }),
            &ctx2,
        )
        .await
        .unwrap();
    assert!(
        !was_document_read(ctx2.session_id, &md),
        "a windowed read must not register a document"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn document_order_is_insertion_order_and_reread_is_idempotent() {
    let dir = tmp_dir("order");
    let a = dir.join("a.md");
    let b = dir.join("b.md");
    let c = dir.join("c.md");
    for f in [&a, &b, &c] {
        std::fs::write(f, "x\n").unwrap();
    }
    let sid = Uuid::new_v4();

    mark_document_read(sid, &a);
    mark_document_read(sid, &b);
    mark_document_read(sid, &c);
    assert_eq!(
        documents_for_session(sid),
        vec![a.clone(), b.clone(), c.clone()]
    );

    // Re-reading the oldest must not duplicate it or move it to the tail.
    mark_document_read(sid, &a);
    assert_eq!(
        documents_for_session(sid),
        vec![a.clone(), b.clone(), c.clone()],
        "a re-read neither duplicates nor reorders"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn document_cap_drops_the_oldest() {
    let dir = tmp_dir("cap");
    let sid = Uuid::new_v4();
    let paths: Vec<std::path::PathBuf> = (0..MAX_TRACKED_DOCUMENTS + 1)
        .map(|i| {
            let p = dir.join(format!("doc_{i}.md"));
            std::fs::write(&p, "x\n").unwrap();
            p
        })
        .collect();

    // Boundary: one below the cap keeps everything.
    for p in &paths[..MAX_TRACKED_DOCUMENTS - 1] {
        mark_document_read(sid, p);
    }
    assert_eq!(documents_for_session(sid).len(), MAX_TRACKED_DOCUMENTS - 1);

    // Exactly at the cap.
    mark_document_read(sid, &paths[MAX_TRACKED_DOCUMENTS - 1]);
    assert_eq!(documents_for_session(sid).len(), MAX_TRACKED_DOCUMENTS);
    assert!(was_document_read(sid, &paths[0]));

    // One past it: the oldest is evicted, the newest is kept.
    let newest = &paths[MAX_TRACKED_DOCUMENTS];
    mark_document_read(sid, newest);
    let kept = documents_for_session(sid);
    assert_eq!(kept.len(), MAX_TRACKED_DOCUMENTS);
    assert!(!was_document_read(sid, &paths[0]), "the oldest is dropped");
    assert!(was_document_read(sid, newest), "the newest is kept");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn skill_and_home_paths_are_not_documents() {
    let dir = tmp_dir("excluded");
    let skill = dir.join("skills").join("opencrabs-dev").join("SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(&skill, "# skill\n").unwrap();
    assert!(
        !is_document_candidate(&skill),
        "a skill body belongs to the skill class, not the document class"
    );

    let brain = crate::config::profile::resolve_profile_home().join("MEMORY.md");
    assert!(
        !is_document_candidate(&brain),
        "the agent's own home is the prompt builder's job"
    );

    let confidential = dir.join("client_secret_notes.md");
    std::fs::write(&confidential, "x\n").unwrap();
    assert!(
        !is_document_candidate(&confidential),
        "the document class must not route around the confidential denylist"
    );

    let plain = dir.join("CLAUDE.md");
    std::fs::write(&plain, "# law\n").unwrap();
    assert!(is_document_candidate(&plain));

    std::fs::remove_dir_all(&dir).ok();
}

/// #952: the predicate names WHICH cause refused a path, and the boolean it
/// backs delegates to that cause — so the two can never disagree.
#[test]
fn document_rejection_names_each_cause() {
    let dir = tmp_dir("reject_causes");

    // The commonest refusal, and the only one about the file rather than the
    // class it belongs to.
    let log = dir.join("marker.log");
    std::fs::write(&log, "x\n").unwrap();
    assert_eq!(document_rejection(&log), Some(DocReject::NotMarkdown));

    // A skill body: refused by construction, not out of scope.
    let skill = dir.join("skills").join("opencrabs-dev").join("SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(&skill, "# skill\n").unwrap();
    assert_eq!(document_rejection(&skill), Some(DocReject::SkillBody));

    // The agent's own home.
    let brain = crate::config::profile::resolve_profile_home().join("MEMORY.md");
    assert_eq!(document_rejection(&brain), Some(DocReject::ProfileHome));

    // The OC-08 denylist.
    let secret = dir.join("client_secret_notes.md");
    std::fs::write(&secret, "x\n").unwrap();
    assert_eq!(document_rejection(&secret), Some(DocReject::Confidential));

    // A plain project `.md` is a candidate, and the delegation agrees.
    let law = dir.join("CLAUDE.md");
    std::fs::write(&law, "# law\n").unwrap();
    assert_eq!(document_rejection(&law), None);
    assert!(is_document_candidate(&law));

    // Four distinct descriptions, and the skill cause names the skill class.
    let causes = [
        DocReject::NotMarkdown,
        DocReject::SkillBody,
        DocReject::ProfileHome,
        DocReject::Confidential,
    ];
    let mut described: Vec<&str> = causes.iter().map(|c| c.describe()).collect();
    described.sort_unstable();
    described.dedup();
    assert_eq!(described.len(), 4, "each cause names itself: {described:?}");
    assert!(
        DocReject::SkillBody.describe().contains("skill"),
        "a skill body must name the skill class: {}",
        DocReject::SkillBody.describe()
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A written file was not read: `write_file` feeds the #1168 set, never this one.
#[tokio::test]
async fn write_file_does_not_register_a_document() {
    let dir = tmp_dir("written");
    let path = dir.join("design.md");
    let ctx = ctx_in(&dir);
    let sid = ctx.session_id;

    let result = WriteTool
        .execute(
            json!({ "path": path.to_string_lossy(), "content": "# design\n" }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(result.success);

    assert!(
        was_fully_read(sid, &path),
        "the write still unlocks a later overwrite (#1168)"
    );
    assert!(
        !was_document_read(sid, &path),
        "a file this session WROTE is not a document it READ"
    );
    assert!(documents_for_session(sid).is_empty());

    std::fs::remove_dir_all(&dir).ok();
}
