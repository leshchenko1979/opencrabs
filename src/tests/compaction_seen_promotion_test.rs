//! #947 — a skill READ into context is promoted to ACTIVE at the compaction
//! boundary, so its body survives the marker.
//!
//! The defect: SEEN and ACTIVE are deliberately separate registries, and
//! re-injection reads ACTIVE only. That is correct while the read is still in
//! conversation history — but compaction DELETES the read, which is precisely
//! the moment re-injection becomes necessary. A lane that loads its role file
//! with a plain `read_file` fires `mark_seen` and never `mark_active`, so it
//! was invisible to re-injection by construction: after the marker it worked
//! from the summariser's paraphrase of instructions it no longer held, while
//! the stamp still claimed the skill was loaded.
//!
//! The acceptance case drives the real boundary — read → `note_compaction` →
//! promote → rendered skill section — and asserts on the RENDERED text, not
//! the registry. Asserting on the registry alone would pass while the prompt
//! stayed empty, which is the exact shape of the bug.

use uuid::Uuid;

use crate::brain::skills::{Skill, SkillSource, active_skill_bodies};
use crate::brain::tools::seen_skills;

/// A synthetic skill whose body carries a marker we can look for in the
/// rendered section. Built from real frontmatter so `Skill::parse` produces
/// exactly what the loader would.
fn skill(name: &str) -> Skill {
    let raw = format!(
        "---\nname: {name}\ndescription: synthetic test skill {name}\n---\n\n\
         BODY-OF-{name}\n"
    );
    Skill::parse(name, &raw, SkillSource::Builtin).expect("synthetic skill parses")
}

/// The rendered skill section exactly as `run_tool_loop` builds it
/// (`tool_loop.rs`, "Re-inject active skill bodies into system brain").
fn render(session_id: Uuid, skills: &[Skill]) -> String {
    let active = seen_skills::active_for_session(session_id);
    let aux = seen_skills::aux_seen_for_session(session_id);
    active_skill_bodies(&active, skills, &aux)
}

// ---------------------------------------------------------------- promotion

/// A slug that was read but never activated becomes ACTIVE, and is reported
/// as newly promoted. This is the core of the fix.
#[test]
fn seen_skill_is_promoted_to_active() {
    let sid = Uuid::new_v4();
    let skills = vec![skill("role-reader")];

    seen_skills::mark_seen(sid, "role-reader");
    assert!(
        seen_skills::active_for_session(sid).is_empty(),
        "precondition: a read alone must NOT activate"
    );

    let promoted = seen_skills::promote_seen_to_active(sid);
    assert_eq!(promoted, vec!["role-reader".to_string()]);
    assert!(
        seen_skills::active_for_session(sid).contains("role-reader"),
        "the read skill must now be in the re-injection set"
    );

    // And the body is what the renderer will emit from here on.
    assert!(render(sid, &skills).contains("BODY-OF-role-reader"));
}

/// Promotion is idempotent: a second call promotes nothing and does not
/// duplicate entries. Compaction can fire repeatedly in one session.
#[test]
fn promotion_is_idempotent() {
    let sid = Uuid::new_v4();
    seen_skills::mark_seen(sid, "role-reader");

    let first = seen_skills::promote_seen_to_active(sid);
    assert_eq!(first.len(), 1);

    let second = seen_skills::promote_seen_to_active(sid);
    assert!(
        second.is_empty(),
        "nothing is newly promoted on a repeat call, got: {second:?}"
    );
    assert_eq!(
        seen_skills::active_for_session(sid).len(),
        1,
        "the ACTIVE set must not grow on a repeat call"
    );
}

/// A skill that is ALREADY active is not reported as newly promoted — the
/// return value names the change, so a caller can log only real movement.
#[test]
fn already_active_skill_is_not_reported_as_promoted() {
    let sid = Uuid::new_v4();
    seen_skills::mark_seen(sid, "both");
    seen_skills::mark_active(sid, "both");

    let promoted = seen_skills::promote_seen_to_active(sid);
    assert!(
        promoted.is_empty(),
        "an already-active skill is not a promotion, got: {promoted:?}"
    );
}

/// Promotion is session-scoped. One session compacting must never activate
/// another session's reads — that would leak one lane's instructions into
/// an unrelated lane's prompt.
#[test]
fn promotion_does_not_cross_sessions() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    seen_skills::mark_seen(a, "a-skill");
    seen_skills::mark_seen(b, "b-skill");

    seen_skills::promote_seen_to_active(a);

    assert!(seen_skills::active_for_session(a).contains("a-skill"));
    assert!(
        seen_skills::active_for_session(b).is_empty(),
        "session b never compacted and must stay untouched, got: {:?}",
        seen_skills::active_for_session(b)
    );
}

/// Reading only an AUXILIARY file does not promote the skill. The gate treats
/// an aux read as insufficient to satisfy the body requirement, and promotion
/// is narrower than the stamp union for the same reason: injecting a whole
/// body on the strength of one auxiliary file would cost tens of KB the
/// session never asked for.
#[test]
fn auxiliary_read_alone_does_not_promote() {
    let sid = Uuid::new_v4();
    seen_skills::mark_aux_seen(sid, "role-reader", "hq.md");

    let promoted = seen_skills::promote_seen_to_active(sid);
    assert!(
        promoted.is_empty(),
        "an auxiliary read is not a body read, got: {promoted:?}"
    );
    assert!(seen_skills::active_for_session(sid).is_empty());
}

// --------------------------------------------------------------- the gate

/// After promotion the gate must PASS even though `seen_since_compaction` is
/// false: `note_compaction` bumped the epoch, and the body is in context via
/// ACTIVE. Before the fix this combination blocked a call on a skill whose
/// full text was sitting in the system prompt.
#[test]
fn gate_passes_on_active_skill_after_compaction() {
    let sid = Uuid::new_v4();
    seen_skills::mark_seen(sid, "guard-skill");
    seen_skills::promote_seen_to_active(sid);

    // The marker invalidates the seen epoch — the pre-fix gate reads this
    // alone and blocks.
    seen_skills::note_compaction(sid);
    assert!(
        !seen_skills::seen_since_compaction(sid, "guard-skill"),
        "precondition: the epoch leg must be false after compaction"
    );
    assert!(
        seen_skills::active_for_session(sid).contains("guard-skill"),
        "precondition: the ACTIVE leg must be true after promotion"
    );
}

/// The control for the test above: with no promotion and no read, the gate's
/// two legs are both false — so the ACTIVE leg added by this fix cannot be
/// silently passing everything.
#[test]
fn unseen_and_inactive_skill_has_no_passing_leg() {
    let sid = Uuid::new_v4();
    assert!(!seen_skills::seen_since_compaction(sid, "guard-skill"));
    assert!(!seen_skills::active_for_session(sid).contains("guard-skill"));
}

// ------------------------------------------------------ the real boundary

/// The acceptance case, end to end: read → compaction → promote → render.
///
/// Both arms are asserted in one test on purpose. The left arm reproduces the
/// shipped bug (the body is gone after the marker); the right arm shows the
/// fix restoring it. A single arm would pass against a renderer that emits
/// everything unconditionally.
#[test]
fn read_then_compact_renders_the_body() {
    let sid = Uuid::new_v4();
    let skills = vec![skill("role-reader")];

    // (1) the session reads the role file
    seen_skills::mark_seen(sid, "role-reader");

    // (2) it compacts
    seen_skills::note_compaction(sid);

    // LEFT ARM — pre-fix behaviour: ACTIVE is empty, so nothing is re-injected
    // even though the read that carried the body has just been summarised away.
    assert!(
        !render(sid, &skills).contains("BODY-OF-role-reader"),
        "the defect: after compaction the body is absent from the prompt"
    );

    // (3) the boundary promotes the read set
    seen_skills::promote_seen_to_active(sid);

    // RIGHT ARM — the body is in the rendered section on the next turn.
    let rendered = render(sid, &skills);
    assert!(
        rendered.contains("BODY-OF-role-reader"),
        "the fix: the role body must be re-injected after the marker, got: {rendered:?}"
    );
}
