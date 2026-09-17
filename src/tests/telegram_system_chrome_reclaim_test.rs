//! System chrome is never reclaimed as the turn's answer (#1253).
//!
//! Incident: a group turn whose completion was exactly `<<react:👍>>`
//! (`response.content len=0`) arrived right after a provider switch. The
//! #478 reclaim popped the trailing folded run to use as the answer, that
//! run was the self-healing banner, and the user received
//!
//!     🔧 Switched to <fallback>/<model> — <primary> Rate limit exceeded: …
//!     🔄 Now using <fallback>/<model>
//!
//! as the reply. The reaction never landed either: once the reclaim had
//! made the empty final look non-empty, the react-only skip stopped
//! applying. Twice in a row, and it reads exactly like a dropped request.
//!
//! The cause was type-level: chrome and model narration were both
//! `FlowEntry::Text`, so the reclaim had no way to tell scaffolding from a
//! completion. These tests pin the split and the #478 behaviour it must
//! preserve.

use crate::channels::telegram::flow::{
    FlowEntry, compacted_flow_line, compacting_flow_line, last_folded_text,
    pop_trailing_folded_texts, progress_key, push_or_supersede,
};

/// The incident, reduced: chrome is the only trailing entry.
#[test]
fn a_banner_alone_is_never_reclaimed_as_the_answer() {
    let mut entries = vec![FlowEntry::System(
        "🔧 Switched to fallback/model — primary Rate limit exceeded".to_string(),
    )];

    // Port union: the fork's pop takes options_pending (#1226/#31); `false`
    // selects the stock pop these #1253 tests pin.
    let (host, trailer) = pop_trailing_folded_texts(&mut entries, false);
    assert!(
        host.is_none() && trailer.is_none(),
        "a react-only turn must keep its empty final empty"
    );
    assert_eq!(entries.len(), 1, "the banner stays in the block");
    assert!(matches!(entries[0], FlowEntry::System(_)));
}

#[test]
fn chrome_landing_after_the_answer_stays_behind() {
    let mut entries = vec![
        FlowEntry::Tool(0),
        FlowEntry::Text("the real answer".to_string()),
        FlowEntry::System("🔄 Now using fallback/model".to_string()),
    ];

    let (host, trailer) = pop_trailing_folded_texts(&mut entries, false);
    assert_eq!(host.as_deref(), Some("the real answer"));
    assert!(trailer.is_none(), "stock pop returns no trailer");
    assert!(matches!(entries[0], FlowEntry::Tool(0)));
    assert!(
        matches!(&entries[1], FlowEntry::System(t) if t.contains("Now using")),
        "chrome is not promoted and not deleted, it stays chrome"
    );
    assert_eq!(entries.len(), 2);
}

/// The incident order: the switch happens first, the model answers after.
#[test]
fn chrome_before_the_answer_does_not_block_the_reclaim() {
    let mut entries = vec![
        FlowEntry::System("🔧 Switched to fallback/model".to_string()),
        FlowEntry::Text("answer written after the switch".to_string()),
    ];

    let (host, trailer) = pop_trailing_folded_texts(&mut entries, false);
    assert_eq!(host.as_deref(), Some("answer written after the switch"));
    assert!(trailer.is_none(), "stock pop returns no trailer");
    assert_eq!(entries.len(), 1);
    assert!(matches!(entries[0], FlowEntry::System(_)));
}

/// #478 must survive the change: a multi-part model answer still joins.
#[test]
fn a_multipart_model_answer_is_still_joined_and_popped_whole() {
    let mut entries = vec![
        FlowEntry::Tool(0),
        FlowEntry::Text("part one".to_string()),
        FlowEntry::Text("part two".to_string()),
    ];

    let (host, trailer) = pop_trailing_folded_texts(&mut entries, false);
    assert_eq!(host.as_deref(), Some("part one\n\npart two"));
    assert!(trailer.is_none(), "stock pop returns no trailer");
    assert_eq!(entries.len(), 1, "only the tool row remains");
}

/// Interstitial narration is not the answer: the run ends at the last tool.
#[test]
fn the_reclaim_still_stops_at_the_last_tool_call() {
    let mut entries = vec![
        FlowEntry::Text("mid-turn narration".to_string()),
        FlowEntry::System("⏳ Retry 1/3 — timeout".to_string()),
        FlowEntry::Tool(1),
        FlowEntry::Text("closing answer".to_string()),
    ];

    let (host, trailer) = pop_trailing_folded_texts(&mut entries, false);
    assert_eq!(host.as_deref(), Some("closing answer"));
    assert!(trailer.is_none(), "stock pop returns no trailer");
    assert_eq!(
        entries.len(),
        3,
        "everything up to the tool row is untouched"
    );
}

#[test]
fn the_duplicate_check_looks_past_trailing_chrome() {
    let with_chrome = vec![
        FlowEntry::Tool(0),
        FlowEntry::Text("folded copy of the answer".to_string()),
        FlowEntry::System("🔄 Now using fallback/model".to_string()),
    ];
    assert_eq!(
        last_folded_text(&with_chrome).map(String::as_str),
        Some("folded copy of the answer"),
        "chrome after the answer must not hide it, or it renders twice"
    );

    let chrome_only = vec![
        FlowEntry::Tool(0),
        FlowEntry::System("🔧 alert".to_string()),
    ];
    assert!(
        last_folded_text(&chrome_only).is_none(),
        "chrome is never a duplicate of the final answer"
    );

    let across_a_tool = vec![
        FlowEntry::Text("older narration".to_string()),
        FlowEntry::Tool(0),
    ];
    assert!(
        last_folded_text(&across_a_tool).is_none(),
        "the search stops at the last tool call"
    );
}

/// #982 supersession still collapses a repeating counter, but only within
/// one provenance: chrome never overwrites a model line and vice versa.
#[test]
fn supersession_collapses_a_counter_within_one_kind_only() {
    let mut entries: Vec<FlowEntry> = Vec::new();
    push_or_supersede(&mut entries, "⏳ Retry 1/3 — timeout", true);
    push_or_supersede(&mut entries, "⏳ Retry 2/3 — timeout", true);
    assert_eq!(entries.len(), 1, "the counter advances in place (#982)");
    assert!(matches!(&entries[0], FlowEntry::System(t) if t.contains("2/3")));

    // Same progress key, different author: appends instead of overwriting.
    push_or_supersede(&mut entries, "⏳ Retry 3/3 — timeout", false);
    assert_eq!(entries.len(), 2, "a model line never overwrites chrome");
    assert!(matches!(entries[0], FlowEntry::System(_)));
    assert!(matches!(entries[1], FlowEntry::Text(_)));

    // Ordinary narration has no progress key and always appends.
    push_or_supersede(&mut entries, "here is what I found", false);
    assert_eq!(entries.len(), 3);
}

#[test]
fn compact_system_flow_entry_stays_folded_and_is_never_reclaimed_as_answer() {
    let start_line = compacting_flow_line(85.0, None);
    let done_line = compacted_flow_line(
        85.0,
        32.0,
        100_000,
        38_000,
        std::time::Duration::from_secs(5),
    );

    let mut entries = vec![
        FlowEntry::Tool(0),
        FlowEntry::System(start_line),
        FlowEntry::System(done_line),
    ];

    let (host, trailer) = pop_trailing_folded_texts(&mut entries, false);
    assert!(
        host.is_none() && trailer.is_none(),
        "compaction system lines must stay in the folded flow and never be popped as answer text"
    );
    assert_eq!(entries.len(), 3);
    assert!(matches!(entries[0], FlowEntry::Tool(0)));
    assert!(matches!(&entries[1], FlowEntry::System(s) if s.contains("compact: 85%")));
    assert!(matches!(&entries[2], FlowEntry::System(s) if s.contains("compact: 85% → 32%")));
}

#[test]
fn compaction_lines_supersede_in_place_under_compaction_progress_key() {
    let start_line = compacting_flow_line(88.0, None);
    let done_line = compacted_flow_line(
        88.0,
        40.0,
        120_000,
        50_000,
        std::time::Duration::from_secs(8),
    );

    assert_eq!(progress_key(&start_line), Some("compaction"));
    assert_eq!(progress_key(&done_line), Some("compaction"));

    let mut entries: Vec<FlowEntry> = Vec::new();
    push_or_supersede(&mut entries, &start_line, true);
    assert_eq!(entries.len(), 1);
    assert!(matches!(&entries[0], FlowEntry::System(s) if s.contains("compact: 88%")));

    push_or_supersede(&mut entries, &done_line, true);
    assert_eq!(
        entries.len(),
        1,
        "compacted line supersedes compacting line in place (#281)"
    );
    assert!(matches!(&entries[0], FlowEntry::System(s) if s.contains("compact: 88% → 40%")));
}
