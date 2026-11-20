//! The recall receipt line (#928).
//!
//! Passive recall runs on every turn and used to leave no trace anyone could
//! see: the only record was a `Recalled N chars` log line. The owner reading a
//! flow roll could not tell that MEMORY.md had fired, let alone which section.
//!
//! These pin the rendered line and its two truncation rules — a per-source
//! `+K` overflow marker and a hard character cap that must never split a
//! multi-byte heading — plus the silence rule: no recall, no row.

use crate::brain::memory_recall::{
    RECALL_LINE_MAX_CHARS, RECALL_LINE_NAMES, RecallSource, RecallSummary,
};

fn source<S: AsRef<str>>(names: &[S], omitted: usize) -> RecallSource {
    RecallSource {
        names: names.iter().map(|n| n.as_ref().to_string()).collect(),
        omitted,
    }
}

#[test]
fn the_line_names_every_source_that_fired() {
    // A line that named only MEMORY.md would be a half-truth: the aggregate
    // injects four sources and the owner has to see which of them fired.
    let summary = RecallSummary {
        skills: source(&["opencrabs-dev"], 0),
        memory: source(&["Cron", "Open Questions"], 2),
        active_skills: source(&["opencrabs-dev/editor.md"], 0),
        directives: source(&["AGENTS.md"], 0),
        chars: 1140,
    };

    assert_eq!(
        summary
            .render_line()
            .expect("recall fired, so there is a line"),
        "🧠 recall · skills /opencrabs-dev · memory «Cron», «Open Questions» +2 · \
         active skills «opencrabs-dev/editor.md» · directives «AGENTS.md» · 1,140c"
    );
}

#[test]
fn only_the_sources_that_fired_appear() {
    // The failure this prevents: a fixed template printing `skills` or
    // `directives` labels for sources that injected nothing.
    let summary = RecallSummary {
        memory: source(&["Cron"], 0),
        chars: 42,
        ..RecallSummary::default()
    };

    assert_eq!(
        summary.render_line().expect("memory fired"),
        "🧠 recall · memory «Cron» · 42c"
    );
}

#[test]
fn nothing_recalled_renders_no_line() {
    // Requirement 1: no empty chrome row, ever.
    assert!(RecallSummary::default().render_line().is_none());

    // ...and a summary that only carries a size is still "nothing recalled".
    let sized = RecallSummary {
        chars: 900,
        ..RecallSummary::default()
    };
    assert!(sized.is_empty());
    assert!(sized.render_line().is_none());
}

#[test]
fn the_overflow_marker_counts_what_is_not_named() {
    // Cap each source at two names; the rest is a count, not more names.
    let summary = RecallSummary {
        skills: source(&["a", "b", "c", "d", "e"], 0),
        memory: source(&["Cron"], 4),
        chars: 300,
        ..RecallSummary::default()
    };

    let line = summary.render_line().expect("skills fired");
    assert_eq!(RECALL_LINE_NAMES, 2);
    assert_eq!(
        line,
        "🧠 recall · skills /a, /b +3 · memory «Cron» +4 · 300c"
    );
}

#[test]
fn a_long_heading_truncates_on_a_char_boundary() {
    // Byte-slicing this would panic (or, with a lossy conversion, corrupt the
    // heading). The cut must land between chars and say it was cut.
    let long: String = "記憶メモリ".repeat(60);
    let summary = RecallSummary {
        memory: source(&[&long], 0),
        chars: 1_200,
        ..RecallSummary::default()
    };

    let line = summary.render_line().expect("memory fired");
    assert_eq!(
        line.chars().count(),
        RECALL_LINE_MAX_CHARS,
        "a truncated line fills the budget exactly: {line}"
    );
    assert!(
        line.ends_with('…'),
        "a clipped name must say it was cut: {line}"
    );
    // Only the HEADING slice is checked for multi-byte integrity: the line's
    // own chrome (🧠, «», …) is non-ASCII by design, so a whole-line filter
    // would flag the very markers it is meant to tolerate.
    let cut = line
        .strip_prefix("🧠 recall · memory «")
        .expect("line names the memory source")
        .strip_suffix('…')
        .expect("a clipped name must say it was cut");
    assert!(
        cut.chars().all(|c| "記憶メモリ".contains(c)),
        "truncation split a multi-byte char: {line}"
    );
    assert!(
        !line.contains('\u{FFFD}'),
        "a split char must never become a replacement char: {line}"
    );
    assert!(line.starts_with("🧠 recall · memory «"));
}

#[test]
fn the_line_never_exceeds_the_budget() {
    // Four sources, each at its two-name cap, with names long enough to blow
    // the budget: the tail sources drop whole rather than cutting a name.
    let long = |tag: &str| format!("{tag}{}", "x".repeat(40));
    let summary = RecallSummary {
        skills: source(&[&long("skill-a"), &long("skill-b")], 9),
        memory: source(&[&long("mem-a"), &long("mem-b")], 9),
        active_skills: source(&[&long("active-a"), &long("active-b")], 9),
        directives: source(&[&long("dir-a"), &long("dir-b")], 9),
        chars: 12_345,
    };

    let line = summary.render_line().expect("sources fired");
    assert!(
        line.chars().count() <= RECALL_LINE_MAX_CHARS,
        "{} chars: {line}",
        line.chars().count()
    );
    assert!(line.starts_with("🧠 recall · skills "));
    assert!(
        line.ends_with("c"),
        "the injected size survives the truncation: {line}"
    );
    assert!(
        !line.contains("directives «dir-a"),
        "a source that does not fit is dropped whole, never half-named: {line}"
    );
}

#[test]
fn the_size_is_grouped_for_reading() {
    let cases = [
        (0usize, "0"),
        (7, "7"),
        (999, "999"),
        (1_000, "1,000"),
        (1_140, "1,140"),
        (12_345, "12,345"),
        (1_234_567, "1,234,567"),
    ];
    for (n, expected) in cases {
        let summary = RecallSummary {
            memory: source(&["Cron"], 0),
            chars: n,
            ..RecallSummary::default()
        };
        let line = summary.render_line().expect("memory fired");
        assert!(
            line.ends_with(&format!("· {expected}c")),
            "{n} must read as {expected}: {line}"
        );
    }
}
