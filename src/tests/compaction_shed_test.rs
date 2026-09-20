//! #438 A5 — the mechanical shed: the harness enforcing the retained-set
//! budget it asked the model to respect.
//!
//! The compactor prompt states a token budget and asks the model to fit the
//! retained set inside it. One compaction that lands slightly over is the
//! model's call to make. A RUN of them — two compactions with no committed
//! tool result between them — is the #226 loop, where the floor is not coming
//! down and asking again will not bring it down. That run is the only case
//! where the harness sheds on the model's behalf.
//!
//! These drive the real `AgentService` against synthetic skills, because the
//! two things under test are the GATE (a run, not a single compaction) and the
//! ORDER (auxiliary documents before whole skills). `shed_order` itself is
//! pure; the gate and its application are not.

use crate::brain::agent::service::AgentService;
use crate::brain::provider::Provider;
use crate::brain::skills::{
    retained_set_budget_tokens, retained_set_tokens, AuxiliaryFile, RETAINED_SET_BUDGET_RATIO,
    Skill, SkillSource,
};
use crate::brain::tools::seen_skills;
use crate::db::Database;
use crate::services::ServiceContext;
use crate::tests::agent_service_mocks::MockProvider;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use uuid::Uuid;

/// Helper: create an in-memory AgentService for testing.
async fn make_service() -> AgentService {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let pool = db.pool().clone();
    let context = ServiceContext::new(pool);
    let provider: Arc<dyn Provider> = Arc::new(MockProvider);
    AgentService::new_for_test(provider, context).await
}

/// A run of short words — a body whose token count grows with the argument, so
/// a test can SIZE the retained set instead of guessing at it.
fn filler(words: usize) -> String {
    "lorem ipsum dolor sit amet ".repeat(words)
}

/// A synthetic skill: `body_words` of filler, plus auxiliary documents of
/// `aux` filler each.
fn skill(name: &str, body_words: usize, aux: &[(&str, usize)]) -> Skill {
    let raw = format!(
        "---\nname: {name}\ndescription: synthetic test skill {name}\n---\n\n{}\n",
        filler(body_words)
    );
    let mut parsed = Skill::parse(name, &raw, SkillSource::Builtin).expect("synthetic skill parses");
    parsed.auxiliary_files = aux
        .iter()
        .map(|(file, words)| AuxiliaryFile {
            name: (*file).to_string(),
            body: filler(*words),
        })
        .collect();
    parsed
}

/// The window size whose retained-set BUDGET lands strictly between `without`
/// and `with_aux` tokens — so the auxiliary document alone is what pushes the
/// set over, and dropping it is enough to come back inside.
///
/// Derived from the measured numbers rather than written down: a hard-coded
/// window would silently stop testing anything the day a body changes size.
/// The assertion is what keeps this honest — a budget that does not straddle
/// the two measurements would prove nothing about WHICH entry the shed picks.
fn window_with_budget_between(without: usize, with_aux: usize) -> (usize, usize) {
    let budget = (without + with_aux) / 2;
    let max_tokens = (budget as f64 / RETAINED_SET_BUDGET_RATIO).ceil() as usize;
    let enforced = retained_set_budget_tokens(max_tokens);
    assert!(
        enforced > without && enforced < with_aux,
        "test setup: the enforced budget ({enforced}) must sit strictly between the set \
         without its auxiliary document ({without}) and with it ({with_aux}), or this \
         test cannot distinguish the aux drop from a whole-skill drop"
    );
    (max_tokens, enforced)
}

/// **The criterion.** Two compactions with no committed tool result between
/// them, over a set that only its auxiliary document pushes over budget: the
/// shed drops that document and nothing else. The skill it belongs to stays
/// active, and no whole slug is ever named.
#[tokio::test]
async fn degraded_streak_sheds_the_aux_document_before_any_whole_skill() {
    let svc = make_service().await;
    let sid = Uuid::new_v4();

    // `alpha` carries a consumed auxiliary document — the expensive entry.
    // `beta` is a plain skill, there to prove a shed that only needs the aux
    // document does not touch anything else.
    let skills = vec![
        skill("alpha", 20, &[("procedure.md", 600)]),
        skill("beta", 20, &[]),
    ];
    svc.register_active_skill(sid, "alpha");
    svc.register_active_skill(sid, "beta");
    seen_skills::mark_aux_seen(sid, "alpha", "procedure.md");

    let active: HashSet<String> = svc.active_skills_for_session(sid);
    let aux = seen_skills::aux_seen_for_session(sid);
    let with_aux = retained_set_tokens(&active, &skills, &aux);
    let without_aux = retained_set_tokens(&active, &skills, &HashMap::new());
    let (max_tokens, budget) = window_with_budget_between(without_aux, with_aux);

    // No compaction at all, then ONE: the model's call to make. The set must be
    // untouched after both, not merely reported as untouched.
    assert!(
        svc.shed_retained_set(sid, max_tokens, &skills).is_empty(),
        "a session that has not compacted is not degraded and must never be shed"
    );
    svc.note_compaction_streak(sid, 42.0);
    assert!(
        svc.shed_retained_set(sid, max_tokens, &skills).is_empty(),
        "a single over-budget compaction is the model's call, not the harness's \
         (the threshold is two in a row — SHED_STREAK_THRESHOLD)"
    );
    assert_eq!(
        svc.active_skills_for_session(sid),
        active,
        "nothing may be dropped below the streak threshold"
    );

    // A SECOND compaction with no committed tool result between them is the
    // #226 loop: the harness enforces the budget itself.
    svc.note_compaction_streak(sid, 42.0);
    let shed = svc.shed_retained_set(sid, max_tokens, &skills);

    assert_eq!(
        shed,
        vec!["alpha/procedure.md".to_string()],
        "the auxiliary document is the cheapest thing to lose and must go first"
    );
    assert!(
        shed.iter().all(|spec| spec.contains('/')),
        "no whole skill may be shed while an auxiliary document can pay for it, got: {shed:?}"
    );
    assert!(
        svc.active_skills_for_session(sid).contains("alpha"),
        "the skill that owns the shed auxiliary document stays active"
    );
    assert!(
        svc.active_skills_for_session(sid).contains("beta"),
        "an untouched skill must not be collateral damage"
    );
    assert!(
        !seen_skills::aux_seen_for_session(sid).contains_key("alpha"),
        "the shed must be APPLIED, not merely reported: the dropped document must \
         stop being re-injected on the next turn"
    );

    // And the set really is back inside the budget it was shed against — the
    // shed's own stopping condition, checked against the measured total.
    let after = retained_set_tokens(
        &svc.active_skills_for_session(sid),
        &skills,
        &seen_skills::aux_seen_for_session(sid),
    );
    assert!(
        after <= budget,
        "the shed must bring the set inside the budget it enforced: {after} > {budget}"
    );
}

/// The shed is gated on BOTH halves — a degraded streak AND an over-budget
/// set. A degraded streak alone must be inert, and so must an over-budget set
/// on a session that has compacted only once. Without this the gate could pass
/// its criterion test while firing on every compacted session.
#[tokio::test]
async fn a_degraded_streak_with_a_set_inside_budget_sheds_nothing() {
    let svc = make_service().await;
    let skills = vec![skill("alpha", 20, &[("procedure.md", 600)])];

    // Half two, missing: degraded streak, comfortable budget.
    let comfortable_window = 200_000;
    let sid_roomy = Uuid::new_v4();
    svc.register_active_skill(sid_roomy, "alpha");
    seen_skills::mark_aux_seen(sid_roomy, "alpha", "procedure.md");
    svc.note_compaction_streak(sid_roomy, 42.0);
    svc.note_compaction_streak(sid_roomy, 42.0);

    let active: HashSet<String> = svc.active_skills_for_session(sid_roomy);
    let aux = seen_skills::aux_seen_for_session(sid_roomy);
    let retained = retained_set_tokens(&active, &skills, &aux);
    let budget = retained_set_budget_tokens(comfortable_window);
    assert!(
        retained < budget,
        "test setup: the retained set ({retained}) must sit inside this window's budget \
         ({budget}), or the case under test is not the case being exercised"
    );

    assert!(
        svc.shed_retained_set(sid_roomy, comfortable_window, &skills)
            .is_empty(),
        "an over-budget set is only half the condition — a set already inside the \
         budget must be left alone, streak or no streak"
    );

    // Half one, missing: over-budget, nothing active to shed.
    let sid_empty = Uuid::new_v4();
    svc.note_compaction_streak(sid_empty, 42.0);
    svc.note_compaction_streak(sid_empty, 42.0);
    assert!(
        svc.shed_retained_set(sid_empty, 1_000, &skills).is_empty(),
        "a session holding no active skills has nothing to shed"
    );

    // Both sessions kept what they had.
    assert_eq!(
        svc.active_skills_for_session(sid_roomy),
        active,
        "no entry may be dropped when the set is inside budget"
    );
    assert!(
        seen_skills::aux_seen_for_session(sid_roomy).contains_key("alpha"),
        "the auxiliary document must survive an in-budget shed check"
    );
    assert!(
        svc.active_skills_for_session(sid_empty).is_empty(),
        "the empty session must still be empty"
    );
}
