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

use crate::brain::memory_recall::{SKILL_RECALL_MAX, recall_skills_from};
use crate::eval::skills_track::index_markdown;

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
        count >= 1 && count <= SKILL_RECALL_MAX,
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

/// The shortlist is the SCORECARD's order, not the floor-filtered order.
///
/// `score()` orders the whole index at floor `0.0` and counts C1@5 from that.
/// If the shipped selector built its list at `RECALL_MIN_SCORE` instead, a gold
/// skill ranked third could be dropped for not clearing the floor, and C1@5
/// would stop being a statement about the product. The floor decides only
/// whether a shortlist is emitted at all.
#[test]
fn the_shortlist_order_matches_the_ordering_the_scorecard_counts() {
    use crate::brain::section_rank::Ranked;

    let md = index();
    let msg = "deploy grafana dashboards and fix the panel SQL";

    // What the scorecard would order: the whole index, floor 0.0.
    let expected: Vec<String> = Ranked::build(&md)
        .find_relevant(msg, SKILL_RECALL_MAX, usize::MAX, 0.0)
        .sections
        .iter()
        .map(|s| s.heading.trim().trim_start_matches('#').trim().to_string())
        .collect();

    let out = recall_skills_from(&md, msg).expect("this message matches an entry");
    let got: Vec<String> = listed(&out)
        .split(", ")
        .map(|s| s.trim_start_matches('/').to_string())
        .collect();

    assert_eq!(
        got, expected,
        "the shipped shortlist must be the order C1@5 counts, or the measured \
         number describes the harness instead of the product"
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
