//! #438 local half — the three behaviours the skill-curation machinery rests
//! on: the retained-set BUDGET (A6), the mechanical SHED order (A5), and the
//! compaction STREAK with its reset (S1).
//!
//! Split by what each behaviour needs. The budget and the shed order are pure
//! functions, so they are driven directly — no service, no mocks, no locks. The
//! streak lives in per-session state behind `AgentService`, so its tests go
//! through the production methods the tool loop itself calls
//! (`note_compaction_streak` on a compaction, `reset_compaction_streak` after a
//! committed tool result) rather than a re-implementation of them.
//!
//! What is NOT here: the gate that decides WHEN the shed runs (a degraded
//! streak) and the application of its result — `compaction_shed_test.rs` covers
//! those end to end. The guard's own boundary is in
//! `compaction_loop_guard_test.rs`; what this module adds is the property the
//! guard DEPENDS on — that a committed tool result really does put the streak
//! back to zero, so the brake releases instead of sticking.

use crate::brain::agent::service::AgentService;
use crate::brain::agent::service::compaction::compaction_loop_guard;
use crate::brain::provider::Provider;
use crate::brain::skills::{
    retained_set_budget_tokens, retained_set_tokens, shed_order, AuxiliaryFile,
    RETAINED_SET_BUDGET_RATIO, Skill, SkillSource,
};
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
    let mut parsed =
        Skill::parse(name, &raw, SkillSource::Builtin).expect("synthetic skill parses");
    parsed.auxiliary_files = aux
        .iter()
        .map(|(file, words)| AuxiliaryFile {
            name: (*file).to_string(),
            body: filler(*words),
        })
        .collect();
    parsed
}

/// The active-skill set, spelled out without the read to keep these tests pure.
fn active(names: &[&str]) -> HashSet<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

/// The per-session map of consumed auxiliary documents.
fn aux_map(entries: &[(&str, &str)]) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for (slug, file) in entries {
        map.entry((*slug).to_string())
            .or_default()
            .push((*file).to_string());
    }
    map
}

// ─── A6 · the retained-set budget ───────────────────────────────────────────

/// The budget is a RATIO of the window, not the flat 10 000 the design
/// happened to measure. A flat cap passes at 200 000 and is wrong at every
/// other window — the char/token unit trap one level up — so the second and
/// third assertions are what tell the two apart.
#[test]
fn the_budget_is_a_ratio_of_the_window_not_a_flat_count() {
    assert_eq!(
        RETAINED_SET_BUDGET_RATIO, 0.05,
        "the design's stated ratio: 10 000 tokens against a 200 000-token window"
    );
    assert_eq!(
        retained_set_budget_tokens(200_000),
        10_000,
        "the ratio applied to the very window the design measured"
    );
    assert_eq!(
        retained_set_budget_tokens(400_000),
        20_000,
        "double the window must buy double the budget — a flat cap would not move here"
    );
    assert_ne!(
        retained_set_budget_tokens(400_000),
        retained_set_budget_tokens(200_000)
    );
}

/// The small-window end: the derivation must not round a window up to a budget
/// it does not have, and a wider window can never buy a smaller budget.
#[test]
fn the_budget_never_invents_tokens_a_small_window_cannot_pay() {
    assert_eq!(retained_set_budget_tokens(0), 0);
    assert_eq!(
        retained_set_budget_tokens(1),
        0,
        "5 % of one token rounds to nothing"
    );
    assert_eq!(retained_set_budget_tokens(20), 1);

    let mut previous = 0usize;
    for window in (0..=200_000).step_by(9_973) {
        let budget = retained_set_budget_tokens(window);
        assert!(
            budget >= previous,
            "budget shrank from {previous} to {budget} at window {window}"
        );
        previous = budget;
    }
}

// ─── A5 · the shed order ────────────────────────────────────────────────────

/// The order the prompt states and the harness enforces: auxiliary documents
/// before whole skills, most expensive first inside each tier. A whole skill
/// carries its body AND every document it has not yet consumed, so losing one
/// costs more than losing a single procedure for a task that is already done.
#[test]
fn shed_order_takes_auxiliary_documents_before_whole_skills() {
    let skills = vec![
        skill("alpha", 60, &[("big.md", 900)]),
        skill("beta", 300, &[]),
        skill("gamma", 20, &[("small.md", 40)]),
    ];
    let active = active(&["alpha", "beta", "gamma"]);
    let aux = aux_map(&[("alpha", "big.md"), ("gamma", "small.md")]);

    // Budget 0 drives the shed as far as it will go, so the WHOLE order is
    // observable rather than one step of it. Five candidates (three skills,
    // two documents) and the last entry is never shed, so exactly four go.
    let order = shed_order(&active, &skills, &aux, 0);

    assert_eq!(
        order,
        vec!["alpha/big.md", "gamma/small.md", "beta", "alpha"],
        "documents first and most expensive first, then whole skills by cost — \
         and the one survivor is the cheapest entry, gamma"
    );

    let first_whole = order
        .iter()
        .position(|spec| !spec.contains('/'))
        .expect("a set this far over budget must shed whole skills too");
    assert!(
        order[..first_whole].iter().all(|spec| spec.contains('/'))
            && order[first_whole..].iter().all(|spec| !spec.contains('/')),
        "no whole skill may be shed while a document is still there to pay for \
         it, got: {order:?}"
    );
}

/// The floor: the shed never returns the last remaining entry, so a compaction
/// can never leave a session with no skills at all. Driven at budget 0 — the
/// most aggressive budget there is — with a single active skill, where the
/// only thing that can stop the loop is the floor itself.
#[test]
fn shed_order_never_empties_the_retained_set() {
    let skills = vec![skill("alpha", 60, &[])];
    let active = active(&["alpha"]);

    assert!(
        shed_order(&active, &skills, &HashMap::new(), 0).is_empty(),
        "the only active skill must survive even a zero budget — an emptied set \
         blinds the next turn"
    );
}

/// The stopping condition is the MEASURED total, not a sum of estimates: a set
/// that one document pushes over budget loses that document and nothing else.
#[test]
fn shed_order_stops_as_soon_as_the_measured_total_is_inside_budget() {
    let skills = vec![
        skill("alpha", 20, &[("procedure.md", 600)]),
        skill("beta", 20, &[]),
    ];
    let active = active(&["alpha", "beta"]);
    let aux = aux_map(&[("alpha", "procedure.md")]);

    let with_aux = retained_set_tokens(&active, &skills, &aux);
    let without_aux = retained_set_tokens(&active, &skills, &HashMap::new());
    let budget = (without_aux + with_aux) / 2;
    assert!(
        without_aux <= budget && budget < with_aux,
        "test setup: the budget ({budget}) must sit between the set without its \
         document ({without_aux}) and with it ({with_aux}), or this test cannot \
         tell the document drop from a whole-skill drop"
    );

    assert_eq!(
        shed_order(&active, &skills, &aux, budget),
        vec!["alpha/procedure.md"],
        "dropping the document is enough, so the shed stops there rather than \
         carrying on into skills that were never over budget"
    );
}

/// The boundary the stopping condition rests on: it is STRICTLY over budget.
/// A set exactly at budget is inside it and is left alone; one token under and
/// the shed fires. Same input, adjacent budgets, opposite verdicts.
#[test]
fn shed_order_fires_only_above_the_budget_it_was_given() {
    let skills = vec![skill("alpha", 20, &[("procedure.md", 600)])];
    let active = active(&["alpha"]);
    let aux = aux_map(&[("alpha", "procedure.md")]);
    let measured = retained_set_tokens(&active, &skills, &aux);
    assert!(measured > 0, "test setup: the retained set must cost something");

    assert!(
        shed_order(&active, &skills, &aux, measured).is_empty(),
        "a set exactly AT budget is inside it — the comparison is strict"
    );
    assert_eq!(
        shed_order(&active, &skills, &aux, measured - 1),
        vec!["alpha/procedure.md"],
        "one token under budget and the shed fires, dropping the entry that pays \
         for the excess"
    );
}

// ─── S1 · the compaction streak and its reset ───────────────────────────────

/// A compaction raises the streak, restarts the turn clock, and remembers the
/// share the session was at — the input the prompt's trend signal (A4) reports.
#[tokio::test]
async fn a_compaction_raises_the_streak_and_restarts_the_turn_clock() {
    let svc = make_service().await;
    let sid = Uuid::new_v4();

    assert_eq!(
        svc.compaction_state(sid).compaction_streak,
        0,
        "a session that has never compacted has no streak"
    );

    svc.note_compaction_streak(sid, 71.0);
    svc.bump_turns_since_compaction(sid);
    svc.bump_turns_since_compaction(sid);
    assert_eq!(
        svc.compaction_state(sid).turns_since_compaction,
        2,
        "turns accumulate between compactions"
    );

    svc.note_compaction_streak(sid, 88.5);
    let state = svc.compaction_state(sid);
    assert_eq!(state.compaction_streak, 2, "the run is two compactions long");
    assert_eq!(
        state.turns_since_compaction, 0,
        "a compaction restarts the turn clock"
    );
    assert_eq!(
        state.share_at_last_compaction, 88.5,
        "the latest share is the one remembered"
    );
}

/// The reset a committed tool result performs clears the streak ONLY. Turns
/// elapsed and the share at the last compaction are separate facts and must
/// survive: clearing the clock would destroy the evidence that tells
/// "compacting every turn" from "compacted twice over a long session".
#[tokio::test]
async fn a_committed_tool_result_clears_the_streak_and_nothing_else() {
    let svc = make_service().await;
    let sid = Uuid::new_v4();

    svc.note_compaction_streak(sid, 60.0);
    svc.bump_turns_since_compaction(sid);
    svc.bump_turns_since_compaction(sid);
    svc.note_compaction_streak(sid, 64.0);
    svc.bump_turns_since_compaction(sid);

    let before = svc.compaction_state(sid);
    assert_eq!(before.compaction_streak, 2, "test setup: a run of two");

    svc.reset_compaction_streak(sid);

    let after = svc.compaction_state(sid);
    assert_eq!(after.compaction_streak, 0, "the tool result ends the run");
    assert_eq!(
        after.turns_since_compaction, before.turns_since_compaction,
        "turns elapsed is not the streak's to clear"
    );
    assert_eq!(
        after.share_at_last_compaction, before.share_at_last_compaction,
        "the last compaction still happened at the share it happened at"
    );
}

/// The property A3's brake depends on, driven through the real methods: the
/// guard fires on a run of two, a committed tool result releases it, and the
/// next compaction starts a fresh run of one. Without the reset the brake
/// would stick, and a session that recovered would stay braked forever.
#[tokio::test]
async fn the_guard_releases_when_a_tool_result_lands() {
    let svc = make_service().await;
    let sid = Uuid::new_v4();

    svc.note_compaction_streak(sid, 70.0);
    assert!(
        !compaction_loop_guard(svc.compaction_state(sid).compaction_streak),
        "one compaction is not a loop"
    );

    svc.note_compaction_streak(sid, 70.0);
    assert!(
        compaction_loop_guard(svc.compaction_state(sid).compaction_streak),
        "two compactions with no completed tool call between them is the #226 loop"
    );

    svc.reset_compaction_streak(sid);
    assert!(
        !compaction_loop_guard(svc.compaction_state(sid).compaction_streak),
        "a completed tool call is evidence of progress and must release the brake"
    );

    svc.note_compaction_streak(sid, 70.0);
    let state = svc.compaction_state(sid);
    assert_eq!(
        state.compaction_streak, 1,
        "the next compaction starts a fresh run of one"
    );
    assert!(!compaction_loop_guard(state.compaction_streak));
}

/// The tool loop resets and bumps on every turn, including for sessions that
/// have never compacted. Those calls must be no-ops rather than panics or
/// phantom entries: a bump that created an entry would leave a turn already on
/// the clock of a session that has not compacted at all.
#[tokio::test]
async fn resetting_a_session_that_never_compacted_is_a_no_op() {
    let svc = make_service().await;
    let sid = Uuid::new_v4();

    svc.reset_compaction_streak(sid);
    svc.bump_turns_since_compaction(sid);

    let state = svc.compaction_state(sid);
    assert_eq!(state.compaction_streak, 0);
    assert_eq!(
        state.turns_since_compaction, 0,
        "an absent session has no clock to bump"
    );
    assert_eq!(state.share_at_last_compaction, 0.0);
}
