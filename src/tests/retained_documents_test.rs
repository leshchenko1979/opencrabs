//! Retention of read `.md` documents across compaction (#831).
//!
//! The registration half is the safety boundary: a model names paths, and the
//! harness accepts only the ones this session actually read whole in this
//! process. These tests drive the public `retained_docs` surface — never the
//! private registry — so a change in the keying or the ordering is visible.

use crate::brain::tools::read_state::mark_document_read;
use crate::brain::tools::retained_docs::{
    DocStatus, MAX_DOC_BODY_BYTES, MAX_RETAINED_DOCUMENTS, has_retained_set, register,
    registered_for_session, render,
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

    let requested = vec![read.display().to_string(), unread.display().to_string()];
    let (registered, rejected) = register(sid, &requested);

    assert_eq!(registered.len(), 1, "only the read document is carried");
    assert_eq!(registered[0].path, read);
    assert_eq!(registered[0].status, DocStatus::Ok);
    assert!(
        registered[0].tokens > 0,
        "a measured row carries a token count"
    );
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
    assert!(!crate::brain::tools::read_state::was_document_read(
        sid, &skill
    ));
    assert!(!crate::brain::tools::read_state::was_document_read(
        sid, &brain
    ));

    let requested = vec![skill.display().to_string(), brain.display().to_string()];
    let (registered, rejected) = register(sid, &requested);

    assert!(registered.is_empty(), "neither path is a project document");
    assert_eq!(rejected.len(), 2);
    for line in &rejected {
        assert!(line.contains("(not a project document)"), "got: {line}");
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
    assert_eq!(
        first.0, second.0,
        "re-registering the same list changes nothing"
    );
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
    assert!(
        block.contains("obey the law"),
        "the BODY is what was retained"
    );
    assert!(
        block.starts_with("\n\n"),
        "the separator matches skill bodies"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// The budget is a leftover, so a smaller one has to produce LESS text. Pinned
/// as a comparison rather than an absolute count: the tokenizer may change, the
/// direction must not.
#[test]
fn render_respects_the_leftover_budget() {
    let sid = Uuid::new_v4();
    let dir = tmp_dir("render_budget");
    let body: String = (0..200)
        .map(|i| format!("line {i} of the project law\n"))
        .collect();
    let a = write_document(sid, &dir, "a.md", &body);
    let b = write_document(sid, &dir, "b.md", &body);
    let c = write_document(sid, &dir, "c.md", &body);
    // Cloned into the call: the assertions below compare against the names, and
    // `register` takes them by value.
    register(sid, &[a.clone(), b.clone(), c.clone()]);

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
    // Whatever did fit is the NEWEST read (c.md), never the oldest. Assert on
    // the injected BODY heading, not the bare name: a dropped document is now
    // NAMED in the not-retained list, so `contains("a.md")` is true either way.
    assert!(tight.contains(&format!("### {c}")), "got: {tight}");
    assert!(
        !tight.contains(&format!("### {a}")),
        "the oldest must be the one dropped, and its body must not appear: {tight}"
    );
    assert!(
        tight.contains(&a),
        "…but the dropped document is still NAMED, so the model can recover: {tight}"
    );

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
    // The oversize document's PATH is named in the not-retained section — the
    // model asked for it, so it must learn it was refused and why — while its
    // BODY stays out, which is the assertion above.
    assert!(
        block.contains("## Documents NOT retained"),
        "the oversize document must be reported, not dropped silently: {block}"
    );
    assert!(
        block.contains(&big_path),
        "the refused path must be named: {block}"
    );
    assert!(
        block.contains("file too large to retain"),
        "the refusal must carry its reason: {block}"
    );
    assert!(block.contains(&small_path));
    assert!(block.contains("short body"));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn render_names_registered_documents_when_the_budget_is_exhausted() {
    let sid = Uuid::new_v4();
    let dir = tmp_dir("render_exhausted");
    let path = write_document(sid, &dir, "law.md", "# law\nobey\n");
    register(sid, std::slice::from_ref(&path));

    // A registered document that does not fit must be NAMED, never silently
    // dropped: the model asked for it, so silence reads as "it is in context".
    let block = render(sid, 0).expect("a registered document always produces a block");
    assert!(block.contains("## Documents NOT retained"));
    assert!(block.contains(&path));
    assert!(
        !block.contains("## Retained documents"),
        "nothing fitted, so no retained section may appear"
    );
    assert!(!block.contains("obey"), "the body must not leak in");

    let tiny = render(sid, 1).expect("one token still names the path");
    assert!(tiny.contains(&path));

    // …and nothing is registered ⇒ nothing to render, at any budget.
    assert!(render(Uuid::new_v4(), 1_000_000).is_none());

    std::fs::remove_dir_all(&dir).ok();
}

/// A refusal the model never sees is a silent drop, and the manifest is model
/// output — so a hallucinated path would otherwise leave the model believing
/// the document it invented is in its context.
#[test]
fn render_reports_a_refused_path_instead_of_dropping_it_silently() {
    let sid = Uuid::new_v4();
    let dir = tmp_dir("render_refused");
    // Present on disk, named by the manifest, never read by this session.
    let unread = dir.join("architecture.md");
    std::fs::write(&unread, "# architecture\n").unwrap();

    let (registered, rejected) = register(sid, &[unread.display().to_string()]);
    assert!(registered.is_empty(), "an unread path is not carried");
    assert_eq!(rejected.len(), 1);
    assert!(
        has_retained_set(sid),
        "a refusal is something the turn-start assembly must still say"
    );

    let block = render(sid, 1_000_000).expect("a refusal is rendered, not swallowed");
    assert!(
        block.contains("## Documents NOT retained"),
        "the block must be headed as a refusal, got: {block}"
    );
    assert!(
        block.contains("architecture.md"),
        "the refused path must be named, got: {block}"
    );
    assert!(
        block.contains("(not read in this process)"),
        "the block must carry the reason, got: {block}"
    );
    assert!(
        !block.contains("## Retained documents"),
        "nothing was retained, so no retained-body header may appear: {block}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// The other half: a session that registered nothing at all stays silent, so
/// the turn-start brain is byte-identical to before #831.
#[test]
fn has_retained_set_is_false_for_a_session_that_registered_nothing() {
    assert!(!has_retained_set(Uuid::new_v4()));
    assert!(render(Uuid::new_v4(), 1_000_000).is_none());
}

/// A session can hit BOTH causes at once — bodies registered but none fitting
/// the leftover budget, plus paths refused at registration. One heading covers
/// both: a section header emitted per cause is pure noise in the injected
/// brain, and nothing else in the block would reveal the duplicate.
#[test]
fn render_emits_one_not_retained_header_when_both_causes_fire() {
    let sid = Uuid::new_v4();
    let dir = tmp_dir("render_both_causes");
    let law = dir.join("law.md");
    std::fs::write(&law, "# law\nOBEY-MARKER\n").unwrap();
    mark_document_read(sid, &law);
    let law = law.canonicalize().unwrap();
    // The rendered block carries the path as a STRING, so the assertion must
    // compare against one: `str::contains` takes a `Pattern`, and `&PathBuf`
    // is not one.
    let law_display = law.display().to_string();

    // Named by the manifest, present on disk, never read by this session.
    let unread = dir.join("never-read.md");
    std::fs::write(&unread, "# never read\n").unwrap();

    let (registered, rejected) =
        register(sid, &[law_display.clone(), unread.display().to_string()]);
    assert_eq!(registered.len(), 1, "the read document is carried");
    assert_eq!(rejected.len(), 1, "the unread one is refused");

    // Budget 0 ⇒ nothing fits, and a refusal is present at the same time.
    let block = render(sid, 0).expect("both causes produce a block");

    assert_eq!(
        block.matches("## Documents NOT retained").count(),
        1,
        "one heading must cover both causes, got: {block}"
    );
    assert!(
        block.contains(&law_display),
        "the unfitting body must be named: {block}"
    );
    assert!(
        block.contains("never-read.md"),
        "the refused path must be named: {block}"
    );
    assert!(
        !block.contains("OBEY-MARKER"),
        "no body may leak in when nothing fitted: {block}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

// ─── #951 · the document budget floor ───────────────────────────────────────

/// The effective document budget the turn-start assembly hands to `render`:
/// the retained set's leftover, floored at 10 % of the window (#951).
///
/// Mirrors the expression at the `render` call site in `tool_loop.rs`
/// deliberately — the arithmetic is the contract, and the tests below pin that
/// the floored value is what actually reaches `render`, not merely that a
/// constant is 0.10.
fn effective_doc_budget(max_tokens: usize, used: usize) -> (usize, usize) {
    let total = crate::brain::skills::retained_set_budget_tokens(max_tokens);
    let leftover = total.saturating_sub(used);
    (
        leftover,
        leftover.max(crate::brain::skills::retained_doc_budget_floor(max_tokens)),
    )
}

/// The measured failure: in a session carrying `opencrabs-dev` the skill bodies
/// alone are ~19 779 tokens against a 10 000-token envelope, so the leftover is
/// zero and every document body is refused on its first chunk.
#[test]
fn the_document_budget_floor_applies_when_the_leftover_is_zero() {
    let (leftover, budget) = effective_doc_budget(200_000, 19_779);
    assert_eq!(leftover, 0, "skills alone exhaust the 5 % envelope");
    assert_eq!(budget, 20_000, "documents get the 10 % floor, not nothing");
}

/// …and the floored budget is what `render` actually receives: the same body
/// refused at the zero leftover is injected at the floor.
#[test]
fn the_floor_is_what_turns_a_refused_body_into_a_retained_one() {
    let sid = Uuid::new_v4();
    let dir = tmp_dir("floor_render");
    let body: String = (0..400)
        .map(|i| format!("line {i} of the project law\n"))
        .collect();
    let path = write_document(sid, &dir, "law.md", &body);
    register(sid, std::slice::from_ref(&path));

    let (leftover, budget) = effective_doc_budget(200_000, 19_779);
    let starved = render(sid, leftover).expect("a registered document always produces a block");
    assert!(
        !starved.contains("## Retained documents"),
        "a zero leftover retains nothing: {starved}"
    );

    let floored = render(sid, budget).expect("the floor still produces a block");
    assert!(
        floored.contains("## Retained documents"),
        "the floor must make room for the body: {floored}"
    );
    assert!(
        floored.contains(&path),
        "the retained body is the registered document: {floored}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// A leftover larger than the floor must survive untouched: the floor is a
/// `max`, never an assignment. At today's ratios the 5 % envelope cannot leave
/// more than the 10 % floor, so the property is exercised on the arithmetic
/// itself — it is the guard a future ratio change must not break.
#[test]
fn the_document_budget_floor_never_lowers_a_larger_leftover() {
    for window in [200_000usize, 400_000, 1_000_000] {
        let floor = crate::brain::skills::retained_doc_budget_floor(window);
        let (_, budget) = effective_doc_budget(window, 0);
        assert!(
            budget >= floor,
            "window {window}: budget {budget} fell below the floor {floor}"
        );
        assert_eq!(
            (floor + 7_000).max(floor),
            floor + 7_000,
            "window {window}: a leftover above the floor must not be lowered"
        );
    }
}

/// 10 % of the window, not a flat count: 20 000 at 200k, 40 000 at 400k.
#[test]
fn the_document_budget_floor_is_a_ratio_of_the_window() {
    use crate::brain::skills::{RETAINED_DOC_BUDGET_FLOOR_RATIO, retained_doc_budget_floor};

    assert_eq!(
        RETAINED_DOC_BUDGET_FLOOR_RATIO, 0.10,
        "the floor is the stated 10 % of the window"
    );
    assert_eq!(retained_doc_budget_floor(200_000), 20_000);
    assert_eq!(
        retained_doc_budget_floor(400_000),
        40_000,
        "double the window must double the floor — a flat cap would not move here"
    );
    assert_eq!(retained_doc_budget_floor(0), 0);
    assert_eq!(
        retained_doc_budget_floor(4),
        0,
        "10 % of four tokens rounds to nothing"
    );

    let mut previous = 0usize;
    for window in (0..=400_000).step_by(9_973) {
        let floor = retained_doc_budget_floor(window);
        assert!(
            floor >= previous,
            "floor shrank from {previous} to {floor} at window {window}"
        );
        previous = floor;
    }
}

/// A session that registered nothing pays no floor at all: the turn-start
/// assembly never enters the re-injection branch, so the system brain is
/// byte-identical to before #831 — the floor cannot add a byte to it.
#[test]
fn a_session_with_no_registered_documents_is_unchanged_by_the_floor() {
    let sid = Uuid::new_v4();
    assert!(!has_retained_set(sid));
    let (leftover, budget) = effective_doc_budget(200_000, 19_779);
    assert_eq!(leftover, 0);
    assert_eq!(budget, 20_000);
    assert!(
        render(sid, budget).is_none(),
        "the floor must not conjure a block for a session that registered nothing"
    );
}
