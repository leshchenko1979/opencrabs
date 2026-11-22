//! Skill SELECTION (#901) — the ranked shortlist that rides along with a turn.
//!
//! The Track C scorecard measures selection@5 against the index; this pins the
//! shipped consumer of that measurement. A metric nothing reads is a number,
//! not a capability, so the entry path ranks the index against the user's
//! message and names the best few.
//!
//! The index here is SYNTHETIC on purpose: the live one has 35 entries whose
//! vocabulary changes with the profile, and a test that asserted "gatus-recovery
//! ranks first on this box" would pass or fail on which skills happen to be
//! installed. Controlled vocabulary is what makes the ranking itself testable.

use crate::brain::memory_recall::{RECALL_MIN_SCORE, SKILL_RECALL_MAX};
use crate::eval::skills_track::index_markdown;

/// The shipped selector at the COMPILED default floor (#912).
///
/// These tests pin the ranking and the silence gate at the floor the box ships
/// with when nothing is configured; the configured floor is covered by the
/// resolver's own tests in `memory_recall_floor_test.rs`.
fn recall_skills_from(index_md: &str, msg: &str) -> Option<String> {
    crate::brain::memory_recall::recall_skills_from(index_md, msg, RECALL_MIN_SCORE)
}

const ENTRIES: &[(&str, &str)] = &[
    (
        "gatus-recovery",
        "Gatus alert recovery runbook — handle endpoint failure alerts, SSH host mapping, disk cleanup",
    ),
    (
        "grafana",
        "Grafana ops — deploy dashboards, diagnose panel errors and readonly-db, fix panel SQL, verify panels",
    ),
    (
        "vds-deploy",
        "VDS deploy — direct commits to main, find and run deploy scripts, verify logs on target host",
    ),
    (
        "pageplate",
        "Project procedure for the pageplate repo — CI-only build gate, Python contract suite",
    ),
    (
        "writing-clearly",
        "Writing prose for humans — documentation, README, commit and PR descriptions",
    ),
    (
        "toc-thinking",
        "Facilitated Goldratt Theory of Constraints analysis — current reality tree, evaporating cloud",
    ),
    (
        "security-audit",
        "Run a comprehensive language-agnostic security and CVE audit, scored 0-100",
    ),
    (
        "copywriting",
        "Direct response copywriting frameworks, headline formulas, campaign templates",
    ),
];

fn index() -> String {
    index_markdown(ENTRIES.iter().copied())
}

/// The shortlist line is the second line of the block; line 0 is the header.
fn listed(out: &str) -> &str {
    out.lines().nth(1).unwrap_or_default()
}

/// A message whose vocabulary matches one entry ranks THAT entry first.
///
/// This is the whole claim of the feature: the order the agent reads is
/// relevance order, not alphabetical order.
#[test]
fn a_matching_message_ranks_the_right_skill_first() {
    let out = recall_skills_from(
        &index(),
        "the gatus endpoint is failing on the disk host, run host-diag",
    )
    .expect("a message matching a skill description must produce a shortlist");

    let line = listed(&out);
    assert!(
        line.starts_with("/gatus-recovery"),
        "gatus-recovery must rank first for a gatus endpoint failure, got {line:?}\n{out}"
    );
}

/// A message with no shared vocabulary produces NO hint.
///
/// Silence is the correct answer here: a shortlist invented from nothing is
/// noise paid on every turn, which is what the conservative floor is for.
#[test]
fn an_unmatched_message_produces_no_shortlist() {
    assert!(
        recall_skills_from(&index(), "zzqx vrubnitz plimth").is_none(),
        "no shared vocabulary must mean no shortlist"
    );
}

/// System-injected messages are not user intent and must not be ranked.
#[test]
fn a_system_message_never_produces_a_shortlist() {
    assert!(
        recall_skills_from(&index(), "[System: continue the gatus task]").is_none(),
        "a [System: ...] message is not a request for a skill"
    );
}

/// The shortlist never grows past its cap, however much the message matches.
#[test]
fn the_shortlist_never_exceeds_the_cap() {
    let out = recall_skills_from(
        &index(),
        "deploy grafana dashboards, fix panel SQL, run a security audit, \
         write prose documentation, gatus alert recovery, copywriting headlines, \
         constraints analysis, pageplate repo contract suite",
    )
    .expect("a message matching many entries still produces a shortlist");

    let line = listed(&out);
    let count = line.split(", ").filter(|s| !s.trim().is_empty()).count();
    assert!(
        (1..=SKILL_RECALL_MAX).contains(&count),
        "the cap is {SKILL_RECALL_MAX}, got {count}: {line:?}"
    );
}

/// Entries are named in slash form, with no markdown heading leaking through.
///
/// The index is markdown (`## slug`), so a slug taken from a heading without
/// stripping the hashes would render as `/# slug` — a name the agent cannot
/// invoke and a reader cannot match to the index.
#[test]
fn the_shortlist_names_skills_by_their_slash_form() {
    let out = recall_skills_from(
        &index(),
        "deploy the dashboard to the target host and verify the logs",
    )
    .expect("this message matches the deploy and grafana entries");

    let line = listed(&out);
    for item in line.split(", ") {
        assert!(
            item.starts_with('/'),
            "every entry is a slash name, got {item:?}"
        );
        assert!(
            !item.contains('#'),
            "a markdown heading must not leak into a slug: {item:?}"
        );
    }
}

/// Character budget for the rendered block, measured on this fixture.
///
/// The index costs its characters ONCE in the system prompt, where it is
/// cacheable; this hint is paid on every turn, so its size is pinned. Five
/// slash names plus the header and footer measure ~195 chars here, and the
/// budget leaves room for longer real slugs without letting the block grow
/// silently into a second index.
const SHORTLIST_BUDGET_CHARS: usize = 320;

#[test]
fn the_shortlist_stays_within_its_character_budget() {
    let out = recall_skills_from(
        &index(),
        "deploy grafana dashboards, fix panel SQL, run a security audit, \
         write prose documentation, gatus alert recovery, copywriting headlines",
    )
    .expect("this message matches several entries");

    let n = out.chars().count();
    assert!(
        n <= SHORTLIST_BUDGET_CHARS,
        "the shortlist is paid every turn: {n} chars exceeds the \
         {SHORTLIST_BUDGET_CHARS}-char budget\n{out}"
    );
}

// ── Acceptance criteria for #901 ──────────────────────────────────────────

/// The ranked block is SMALLER than the index it ranks.
///
/// This is the whole economic case: the index is paid once in the system
/// prompt, and the per-turn hint has to be cheap enough that adding it is a net
/// win on prompt characters.
#[test]
fn the_shortlist_is_smaller_than_the_index_it_ranks() {
    let full_chars = crate::eval::skills_track::index_block_chars(ENTRIES.iter().copied());
    let out = recall_skills_from(&index(), "deploy grafana dashboards and fix the panel SQL")
        .expect("this message matches an entry");

    let shortlist_chars = out.chars().count();
    assert!(
        shortlist_chars < full_chars,
        "the shortlist ({shortlist_chars}) must be smaller than the full index \
         ({full_chars}), or ranking buys nothing"
    );
}

/// Ranking never REMOVES an entry: the full index is still emitted.
///
/// The shortlist is a hint, not a filter. A skill that misses the top five has
/// to stay reachable in the system prompt, which is the block the prompt
/// builder emits — so every slug must still be in the rendered index.
#[test]
fn every_skill_remains_in_the_full_index() {
    let full = index();
    for (slug, _) in ENTRIES {
        assert!(
            full.contains(&format!("## {slug}\n")),
            "ranking must not drop {slug} from the index the prompt carries\n{full}"
        );
    }
}

/// The shortlist is the ADMISSION rule, NOT the scorecard's floor-`0.0` order.
///
/// `score()` orders the whole index at floor `0.0` and counts C1@5 from that.
/// That ordering answers "in what order", and its predicate is `score >= 0.0`
/// — which admits every entry sharing one stemmed term. It is the right
/// predicate for a ranking metric and the WRONG one for a shortlist: read as a
/// selection it fills all five slots with skills the message is not about,
/// which is the defect #939 measured.
///
/// So this test pins the opposite of what it used to: the shipped selector
/// applies [`Ranked::admitted_indices`], and the two paths are ALLOWED to
/// disagree. A regression to the floor-`0.0` predicate would restore the
/// five-of-five list while every other test in this file still passed — the
/// fixture is small enough that a full list is not obviously wrong.
#[test]
fn the_shortlist_is_the_admission_rule_not_the_floor_zero_order() {
    use crate::brain::memory_recall::{
        SKILL_RECALL_MIN_SUMMED_IDF, SKILL_RECALL_MIN_TERM_IDF, SKILL_RECALL_MIN_TERMS,
        SKILL_RECALL_RATIO,
    };
    use crate::brain::section_rank::Ranked;

    let md = index();
    let msg = "deploy grafana dashboards and fix the panel SQL";
    let ranked = Ranked::build(&md);

    let expected: Vec<String> = ranked
        .admitted_indices(
            msg,
            SKILL_RECALL_MAX,
            SKILL_RECALL_MIN_TERMS,
            SKILL_RECALL_MIN_TERM_IDF,
            SKILL_RECALL_MIN_SUMMED_IDF,
            SKILL_RECALL_RATIO,
        )
        .iter()
        .map(|&i| ranked.heading(i))
        .collect();

    let out = recall_skills_from(&md, msg).expect("this message matches an entry");
    let got: Vec<String> = listed(&out)
        .split(", ")
        .map(|s| s.trim_start_matches('/').to_string())
        .collect();

    assert_eq!(
        got, expected,
        "the shipped shortlist must be exactly the admission rule's output, \
         got {got:?} want {expected:?}"
    );

    // And the two paths must actually differ here, or the assertion above
    // would be satisfied by the very predicate #939 removed.
    let ordering = ranked.find_relevant(msg, SKILL_RECALL_MAX, usize::MAX, 0.0);
    assert!(
        ordering.sections.len() > got.len(),
        "this message must be one where floor-0.0 ordering fills more slots \
         than admission does ({} vs {}), otherwise the test cannot see the \
         regression it exists for",
        ordering.sections.len(),
        got.len()
    );
}

/// A message sharing only COMMON words with the index does not fill the list
/// (#939 acceptance).
///
/// Every one of "the", "skill", "is", "not", "to", "we", "will", "also",
/// "fix", "it" is a real word the index contains, so a predicate that admits
/// on one shared term names five skills for a message that is about nothing.
/// Admission requires agreement on more than one term, weighted by idf, so
/// filler words — which carry an idf near zero in the corpus being ranked —
/// cannot carry an entry onto the list. The bound is deliberately "no more
/// than two", not "none": a two-word coincidence is still possible, and
/// pinning silence exactly would make this test a hostage to the fixture's
/// vocabulary rather than to the rule.
#[test]
fn a_message_sharing_only_common_terms_does_not_fill_five_slots() {
    let out = recall_skills_from(
        &index(),
        "the skill is not relevant to the task, we will also fix it",
    );

    let names: Vec<&str> = match &out {
        Some(text) => listed(text)
            .split(", ")
            .filter(|s| !s.trim().is_empty())
            .collect(),
        None => Vec::new(),
    };

    assert!(
        names.len() <= 2,
        "a message with no subject must not fill the shortlist; got {} \
         entries {names:?}\n{out:?}",
        names.len()
    );
    assert!(
        names.len() < SKILL_RECALL_MAX,
        "the five-of-five list is exactly the symptom #939 reports"
    );
}

/// The #939 acceptance case: the message that raised the issue must not name
/// skills it is not about.
///
/// The live index is not available in CI, so this fixture reproduces the shape
/// that made the defect visible — an index where a couple of entries share
/// incidental vocabulary with a long message about something else entirely.
/// The BEFORE half asserts the fixture still exhibits the defect under the old
/// predicate (five of five, noise included), so the AFTER half is a statement
/// about the fix rather than about a fixture that was never broken.
#[test]
fn the_issue_message_does_not_name_skills_it_is_not_about() {
    use crate::brain::section_rank::Ranked;

    const ISSUE_ENTRIES: &[(&str, &str)] = &[
        (
            "opencrabs-dev",
            "OpenCrabs source procedure — fork issues, per-task worktrees, the CI gate, \
             signed commits, deploy ship",
        ),
        (
            "md-index",
            "Surface every .md and .mdc file with its title and description from \
             frontmatter. Use when the user asks to index or list a document tree",
        ),
        (
            "avito-realty-bot",
            "Process law for the Avito realty bot — the agent that runs Vadim's Avito \
             messenger dialogues and the reply queue",
        ),
        (
            "ogre-contribute",
            "RETIRED — ogre is no longer installed or used on this host; the deck \
             pipeline runs on html-render",
        ),
        (
            "gatus-recovery",
            "Gatus alert recovery runbook — endpoint failure alerts, SSH host mapping, \
             disk cleanup",
        ),
        (
            "copywriting",
            "Direct response copywriting frameworks, headline formulas, campaign templates",
        ),
    ];

    // The message quoted in the issue, verbatim.
    const MSG: &str = "I think we are wrong to index the skill bodies - we will get a \
                       lot of false positives. We have descriptions specially written to \
                       decide if the skill is relevant. Also fix the ranker";

    let md = index_markdown(ISSUE_ENTRIES.iter().copied());
    let ranked = Ranked::build(&md);

    // BEFORE — the floor-0.0 ordering the selector used to take. This is the
    // symptom, and asserting it here keeps the fix honest: if the fixture ever
    // stops reproducing the defect, the test says so instead of passing
    // vacuously.
    let ordering: Vec<String> = ranked
        .find_relevant(MSG, SKILL_RECALL_MAX, usize::MAX, 0.0)
        .sections
        .iter()
        .map(|s| s.heading.trim().trim_start_matches('#').trim().to_string())
        .collect();
    assert_eq!(
        ordering.len(),
        SKILL_RECALL_MAX,
        "the old predicate must fill all {SKILL_RECALL_MAX} slots on this \
         fixture, or it no longer reproduces #939: {ordering:?}"
    );
    assert!(
        ordering.iter().any(|n| n == "avito-realty-bot"),
        "the old predicate must put the noise entry on the list: {ordering:?}"
    );

    // AFTER — the shipped selector.
    let out = recall_skills_from(&md, MSG).expect("this message matches an entry");
    let names: Vec<String> = listed(&out)
        .split(", ")
        .map(|s| s.trim_start_matches('/').to_string())
        .collect();

    for noise in ["avito-realty-bot", "ogre-contribute"] {
        assert!(
            !names.iter().any(|n| n == noise),
            "{noise} shares no subject with the message and must not be named, \
             got {names:?}\n{out}"
        );
    }
    assert!(
        names.len() < SKILL_RECALL_MAX,
        "the shortlist must not fill every slot for this message, got \
         {names:?}\n{out}"
    );
    assert!(
        names.iter().any(|n| n == "md-index"),
        "the entry the message IS about must survive the cut, got {names:?}\n{out}"
    );
}

/// A PARAPHRASED task — not a quote of the description — lands its skill in
/// the top five.
///
/// This is the acceptance criterion for selection@5 being real: the message
/// shares vocabulary with the description without copying it, so a ranker that
/// only matched verbatim text would fail here.
#[test]
fn a_paraphrased_task_puts_its_skill_in_the_top_five() {
    // "gatus-recovery" describes itself as "handle endpoint failure alerts, SSH
    // host mapping, disk cleanup". This message quotes none of that: it names
    // the symptom and asks for the procedure, so a ranker that only matched
    // verbatim text would fail here.
    let out = recall_skills_from(
        &index(),
        "an endpoint failure alert fired and the disk on that host is full — \
         which procedure handles the cleanup?",
    )
    .expect("a paraphrased match must produce a shortlist");

    let top5: Vec<&str> = listed(&out).split(", ").collect();
    assert!(
        top5.iter()
            .any(|s| s.trim_start_matches('/') == "gatus-recovery"),
        "gatus-recovery must be in the top five for a paraphrased endpoint \
         failure, got {top5:?}"
    );
}

/// #953 — a term carried by much of the index is not evidence.
///
/// The report was that the same handful of skills came back on every turn and
/// that they were irrelevant to the message. The cause was the admission
/// predicate counting PRESENCE: `the`, `for`, `check` and `deploy` sit in
/// enough descriptions that a message about none of them still agreed with
/// several entries on two "terms", and `min_terms` was satisfied by filler
/// alone. The same filler also cleared `min_summed_idf`, because the sum was
/// over every matched term however generic.
///
/// The fix is a per-term idf floor: a term has to be specific to a handful of
/// entries before it counts toward EITHER predicate.
///
/// The assertion is made at the predicate rather than through the shipped
/// selector because on a fixture this small the ratio tail already drops the
/// noise — `noise-check-*` scores about 0.23 against `gatus-recovery`'s 0.42,
/// under `ratio * best` — so the end-to-end list is identical before and after
/// and would pin nothing. `ratio = 0.0` switches that tail off and leaves
/// exactly the two content predicates under test.
///
/// The BEFORE half asserts the fixture still exhibits the defect at the old
/// floor, so a fixture that drifted into passing vacuously says so instead.
#[test]
fn a_message_matching_only_common_terms_is_not_admitted() {
    use crate::brain::memory_recall::{
        SKILL_RECALL_MIN_SUMMED_IDF, SKILL_RECALL_MIN_TERM_IDF, SKILL_RECALL_MIN_TERMS,
    };
    use crate::brain::section_rank::Ranked;

    const ENTRIES_953: &[(&str, &str)] = &[
        (
            "gatus-recovery",
            "Gatus alert recovery runbook — endpoint failure alerts, SSH host \
             mapping, disk cleanup",
        ),
        ("noise-check-a", "Check for the deploy"),
        ("noise-check-b", "Check for the deploy queue"),
        ("noise-check-c", "Check for the deploy log"),
        ("noise-check-d", "Check for the deploy notes"),
        (
            "grafana",
            "Grafana ops — dashboards, panel errors, readonly-db, panel SQL",
        ),
        (
            "pageplate",
            "Project procedure for the pageplate repo — CI-only build gate, \
             Python contract suite",
        ),
        (
            "writing-clearly",
            "Writing prose for humans — documentation, README, commit and PR \
             descriptions",
        ),
        (
            "toc-thinking",
            "Facilitated Goldratt Theory of Constraints analysis — current \
             reality tree",
        ),
        (
            "security-audit",
            "Run a comprehensive language-agnostic security and CVE audit, \
             scored 0-100",
        ),
        (
            "copywriting",
            "Direct response copywriting frameworks, headline formulas, \
             campaign templates",
        ),
        (
            "vds-deploy",
            "VDS deploy — direct commits to main, find and run deploy scripts, \
             verify logs",
        ),
    ];

    // A message that IS about the gatus runbook, and that mentions `check` and
    // `deploy` the way any operator message does.
    const MSG: &str = "which gatus cleanup runbook should I check for the deploy";

    let md = index_markdown(ENTRIES_953.iter().copied());
    let ranked = Ranked::build(&md);

    let names_for = |min_term_idf: f64| -> Vec<String> {
        ranked
            .admitted_indices(
                MSG,
                usize::MAX,
                SKILL_RECALL_MIN_TERMS,
                min_term_idf,
                SKILL_RECALL_MIN_SUMMED_IDF,
                0.0,
            )
            .iter()
            .map(|&i| ranked.heading(i))
            .collect()
    };

    // BEFORE — presence-only admission. Each `noise-check-*` entry agrees with
    // the message on `check`, `for`, `the` and `deploy`, which carry 1.609 of
    // summed idf together, so the old floor admits all four.
    let before = names_for(0.0);
    for noise in [
        "noise-check-a",
        "noise-check-b",
        "noise-check-c",
        "noise-check-d",
    ] {
        assert!(
            before.iter().any(|n| n == noise),
            "the presence-only predicate must admit {noise}, or this fixture no \
             longer reproduces #953: {before:?}"
        );
    }

    // AFTER — the shipped floor. Filler is no longer evidence, so only the
    // entry the message is about survives.
    let after = names_for(SKILL_RECALL_MIN_TERM_IDF);
    assert_eq!(
        after,
        vec!["gatus-recovery".to_string()],
        "only the entry the message is about may survive the content floor, \
         got {after:?}"
    );
}
