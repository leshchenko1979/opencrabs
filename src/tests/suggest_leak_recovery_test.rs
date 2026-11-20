//! Tests for `utils::directives::extract_leaked_suggestions` — the #1774
//! recovery for `<<suggest_options>>` blocks the model wrote as TEXT instead
//! of invoking the tool.
//!
//! The incident: the marker pair around a JSON options array shipped to a
//! channel verbatim (dead markers + raw JSON, buttons never rendered because
//! the tool never ran). The extractor parses the array into real
//! `SuggestionItem`s so every surface's native renderer fires, and strips the
//! block from the delivered text. A block whose payload does not parse loses
//! its markers and payload too: raw markers and raw JSON never reach a
//! channel. Payload shapes verbatim from the #1774 issue capture.

use crate::brain::provider::custom_openai_compatible::extract_text_tool_calls;
use crate::brain::tools::suggest_options::{MAX_OPTIONS, SuggestionStyle};
use crate::utils::directives::{contains_leaked_suggestions, extract_leaked_suggestions};

// ── recovery: parseable blocks ───────────────────────────────────────────

#[test]
fn recovers_styled_array_from_marker_pair() {
    // Verbatim shape from the #1774 channel capture.
    let text = "Next step? <<suggest_options>>\n\
                [{\"label\":\"Run the gates\",\"style\":\"primary\"},\
                {\"label\":\"Park it\"}]\n\
                <<suggest_options>>";
    let (cleaned, items) = extract_leaked_suggestions(text);
    assert_eq!(cleaned, "Next step?");
    let items = items.expect("styled array must recover");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].label, "Run the gates");
    assert_eq!(items[0].style, SuggestionStyle::Primary);
    assert_eq!(items[1].label, "Park it");
    assert_eq!(items[1].style, SuggestionStyle::Default);
}

#[test]
fn recovers_bare_string_items() {
    let text = "<<suggest_options>>[\"Go\", \"Hold on\"]<<suggest_options>>";
    let (cleaned, items) = extract_leaked_suggestions(text);
    assert_eq!(cleaned, "");
    let items = items.expect("bare strings must recover");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].label, "Go");
    assert_eq!(items[1].label, "Hold on");
}

#[test]
fn recovers_pretty_printed_array() {
    let text = "<<suggest_options>>\n[\n  {\"label\": \"A\"},\n  \"B\"\n]\n<<suggest_options>>";
    let (_, items) = extract_leaked_suggestions(text);
    let items = items.expect("pretty-printed array must recover");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].label, "A");
    assert_eq!(items[1].label, "B");
}

#[test]
fn caps_at_max_options() {
    let payload: Vec<String> = (0..MAX_OPTIONS + 5).map(|i| format!("opt{i}")).collect();
    let text = format!("<<suggest_options>>[{}]<<suggest_options>>", {
        let quoted: Vec<String> = payload.iter().map(|s| format!("\"{s}\"")).collect();
        quoted.join(",")
    });
    let (_, items) = extract_leaked_suggestions(&text);
    let items = items.expect("oversized array still recovers (capped)");
    assert_eq!(items.len(), MAX_OPTIONS);
}

// ── text preservation ────────────────────────────────────────────────────

#[test]
fn preserves_surrounding_prose_with_clean_seam() {
    let text = "Intro line.\n<<suggest_options>>[\"A\"]<<suggest_options>>\nClosing line.";
    let (cleaned, items) = extract_leaked_suggestions(text);
    assert!(items.is_some());
    assert_eq!(cleaned, "Intro line.\n\nClosing line.");
}

#[test]
fn leaves_text_without_markers_untouched() {
    let text = "Plain prose with [\"not\", \"options\"] inside.";
    let (cleaned, items) = extract_leaked_suggestions(text);
    assert_eq!(cleaned, text);
    assert!(items.is_none());
}

// ── degradation: never ship markers or raw JSON ──────────────────────────

#[test]
fn strips_unparseable_block_entirely() {
    let text = "Answer.\n<<suggest_options>>[{\"label\": broken<<suggest_options>>";
    let (cleaned, items) = extract_leaked_suggestions(text);
    assert!(items.is_none());
    assert_eq!(cleaned, "Answer.");
    assert!(!cleaned.contains("suggest_options"));
    assert!(!cleaned.contains("broken"));
}

#[test]
fn strips_empty_array_block() {
    let text = "<<suggest_options>>[]<<suggest_options>>";
    let (cleaned, items) = extract_leaked_suggestions(text);
    assert!(items.is_none());
    assert_eq!(cleaned, "");
}

#[test]
fn drops_unclosed_marker_token() {
    let text = "Working on it. <<suggest_options>>";
    let (cleaned, items) = extract_leaked_suggestions(text);
    assert!(items.is_none());
    assert_eq!(cleaned, "Working on it. ");
    assert!(!cleaned.contains("suggest_options"));
}

#[test]
fn object_without_label_is_not_recoverable() {
    // RawSuggestionItem requires a string or {label}: an alien object array
    // must NOT become suggestions, and must not ship as markers.
    let text = "<<suggest_options>>[{\"command\": \"ls\"}]<<suggest_options>>";
    let (cleaned, items) = extract_leaked_suggestions(text);
    assert!(items.is_none());
    assert_eq!(cleaned, "");
}

// ── colon form (#892) ────────────────────────────────────────────────────
//
// The model also writes `<<suggest_options:[…]>>` — no marker pair. Observed
// live 2026-11-19 12:04:40Z in the session store (`channel_messages` id
// `d7352bd1-42d9-4979-a5a9-3ccd4b345ad1`) and delivered to Telegram as raw
// prose because the pair-only extractor never looked for it. Bytes below are
// the stored ones, single braces.

/// The exact leaked directive from the #892 capture.
const OBSERVED_COLON: &str = "<<suggest_options:[{\"label\":\"File and fix it\",\
\"style\":\"primary\"},{\"label\":\"Leave it as is\"}]>>";

#[test]
fn recovers_the_observed_colon_payload() {
    let (cleaned, items) = extract_leaked_suggestions(OBSERVED_COLON);
    assert_eq!(cleaned, "");
    assert!(!cleaned.contains("<<"));
    let items = items.expect("observed colon payload must recover");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].label, "File and fix it");
    assert_eq!(items[0].style, SuggestionStyle::Primary);
    assert_eq!(items[1].label, "Leave it as is");
    assert_eq!(items[1].style, SuggestionStyle::Default);
}

#[test]
fn colon_form_preserves_surrounding_prose() {
    let text = format!("I fixed the leak.\n\n{OBSERVED_COLON}\n\nWant me to file it?");
    let (cleaned, items) = extract_leaked_suggestions(&text);
    assert_eq!(cleaned, "I fixed the leak.\n\nWant me to file it?");
    assert!(!cleaned.contains("<<"));
    assert!(!cleaned.contains("suggest_options"));
    assert_eq!(items.expect("colon payload must recover").len(), 2);
}

#[test]
fn colon_form_takes_bare_strings() {
    let (cleaned, items) = extract_leaked_suggestions("<<suggest_options:[\"Go\", \"Hold on\"]>>");
    assert_eq!(cleaned, "");
    let items = items.expect("bare strings must recover");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].label, "Go");
    assert_eq!(items[1].label, "Hold on");
}

#[test]
fn colon_form_caps_at_max_options() {
    let quoted: Vec<String> = (0..MAX_OPTIONS + 5)
        .map(|i| format!("\"opt{i}\""))
        .collect();
    let text = format!("<<suggest_options:[{}]>>", quoted.join(","));
    let (_, items) = extract_leaked_suggestions(&text);
    assert_eq!(
        items.expect("oversized array still recovers").len(),
        MAX_OPTIONS
    );
}

#[test]
fn strips_unparseable_colon_block_entirely() {
    let text = "Answer.\n<<suggest_options:[{\"label\": broken>>";
    let (cleaned, items) = extract_leaked_suggestions(text);
    assert!(items.is_none());
    assert_eq!(cleaned, "Answer.");
    assert!(!cleaned.contains("<<"));
    assert!(!cleaned.contains("broken"));
}

#[test]
fn strips_empty_colon_block() {
    let (cleaned, items) = extract_leaked_suggestions("Answer.\n<<suggest_options:[]>>");
    assert!(items.is_none());
    assert_eq!(cleaned, "Answer.");
    assert!(!cleaned.contains("<<"));
}

#[test]
fn strips_both_colon_blocks_in_one_reply() {
    // Requirement: a reply that leaks twice must not leak once. Items are
    // MERGED in document order (documented on `extract_leaked_suggestions`).
    let text = "Intro.\n<<suggest_options:[\"Go\"]>>\nMiddle.\n\
                <<suggest_options:[\"Stop\", \"Wait\"]>>\nOutro.";
    let (cleaned, items) = extract_leaked_suggestions(text);
    assert_eq!(cleaned, "Intro.\n\nMiddle.\n\nOutro.");
    assert!(!cleaned.contains("<<"));
    assert!(!cleaned.contains("suggest_options"));
    let items = items.expect("both blocks contribute items");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, ["Go", "Stop", "Wait"]);
}

#[test]
fn strips_two_mixed_grammar_blocks_in_one_reply() {
    let text = "A <<suggest_options:[\"One\"]>> B <<suggest_options>>[\"Two\"]\
                <<suggest_options>> C";
    let (cleaned, items) = extract_leaked_suggestions(text);
    assert_eq!(cleaned, "A\n\nB\n\nC");
    assert!(!cleaned.contains("<<"));
    let items = items.expect("both grammars recover");
    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, ["One", "Two"]);
}

#[test]
fn colon_terminator_inside_a_label_does_not_truncate_the_block() {
    // A `>>` inside the JSON would cut the block short and strand the tail as
    // raw text; the scanner probes successive terminators.
    let text = "<<suggest_options:[{\"label\":\"Ship it >>\"},{\"label\":\"Hold\"}]>>";
    let (cleaned, items) = extract_leaked_suggestions(text);
    assert_eq!(cleaned, "");
    let items = items.expect("payload with an embedded >> must recover");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].label, "Ship it >>");
}

#[test]
fn recovers_unterminated_colon_block_and_strips_its_payload() {
    // No closing `>>`: the payload runs to the end of the text. The items are
    // still recovered and the raw JSON never ships.
    let (cleaned, items) = extract_leaked_suggestions("Done. <<suggest_options:[\"A\"]");
    assert_eq!(cleaned, "Done.");
    assert!(!cleaned.contains("<<"));
    assert!(!cleaned.contains('['));
    assert_eq!(items.expect("unterminated payload still parses").len(), 1);
}

#[test]
fn drops_alien_suggest_options_delimiter() {
    let (cleaned, items) = extract_leaked_suggestions("Hm <<suggest_options_other>> done");
    assert!(items.is_none());
    assert!(!cleaned.contains("<<suggest_options"));
    assert!(cleaned.ends_with("done"));
}

// ── provider pass: no double-handling with the settle-point recovery ─────

#[test]
fn provider_text_leak_pass_leaves_marker_grammars_to_the_settle_point() {
    // `suggest_options` is in KNOWN_TOOL_NAMES (#892), so the provider pass
    // now RUNS on this text. It must not claim either marker grammar: the
    // pair form has no `</suggest_options>` closer and the colon form has no
    // `<param>value</param>` body, so `extract_claude_style_tool_calls`
    // matches nothing and the settle-point extractor remains the only strip.
    let check = |text: &str| {
        let (calls, cleaned) = extract_text_tool_calls(text);
        assert!(calls.is_empty(), "no tool call must be minted from {text}");
        assert_eq!(cleaned, text, "provider pass must not touch {text}");
    };
    check(OBSERVED_COLON);
    check("<<suggest_options>>[\"Go\", \"Hold on\"]<<suggest_options>>");
}

#[test]
fn provider_text_leak_pass_recognises_an_element_style_suggest_options_call() {
    // The value of the KNOWN_TOOL_NAMES entry: a leaked Claude-style call
    // naming `suggest_options` is now visible to the name-anchored pass.
    let text = "<suggest_options><options>[\"A\",\"B\"]</options></suggest_options>";
    let (calls, cleaned) = extract_text_tool_calls(text);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "suggest_options");
    assert!(cleaned.is_empty());
}

// ── stored-row repair (#894) ─────────────────────────────────────────────
//
// The strip at turn settle repairs DELIVERY. The assistant row is a separate
// artifact and it is the one the model reads back: appended per iteration, so
// its content is the whole turn's concatenation while `final_text` is only the
// last iteration. A row left carrying the raw marker becomes a worked example
// the model imitates on later turns (16 leaks in one session, 58 rows across
// five), so the settle hook repairs the row's OWN content.

#[test]
fn pre_check_finds_both_marker_grammars_and_nothing_else() {
    assert!(contains_leaked_suggestions(
        "pick one <<suggest_options>>[\"A\",\"B\"]<<suggest_options>>"
    ));
    assert!(contains_leaked_suggestions("<<suggest_options:[\"A\"]>>"));
    // The bare stem alone still counts: a lone marker is stripped too.
    assert!(contains_leaked_suggestions("trailing <<suggest_options"));

    // Negative controls — the common turn must take the no-op path.
    assert!(!contains_leaked_suggestions("plain answer, no markers"));
    assert!(!contains_leaked_suggestions(""));
    // A lookalike that is not the directive must not trigger a rewrite.
    assert!(!contains_leaked_suggestions("suggest_options was called"));
}

#[test]
fn row_repair_strips_the_marker_and_keeps_every_earlier_iteration() {
    // Shape of a real assistant row after two iterations: the first produced
    // prose, the second leaked the directive. Rebuilding the row from
    // `final_text` would drop the first iteration; the repair must only strip.
    let stored = "Iteration one: read the file and found the cause.\n\n\
                  Iteration two: here are the next steps.\n\
                  <<suggest_options>>\n\
                  [{\"label\":\"Run the gates\"},{\"label\":\"Park it\"}]\n\
                  <<suggest_options>>";
    assert!(contains_leaked_suggestions(stored));

    let (repaired, _items) = extract_leaked_suggestions(stored);

    assert!(
        !repaired.contains("<<suggest_options"),
        "repaired row must carry no raw marker, got: {repaired}"
    );
    assert!(
        !repaired.contains("Run the gates"),
        "the recovered payload must leave the row too, got: {repaired}"
    );
    assert!(
        repaired.contains("Iteration one: read the file and found the cause."),
        "earlier iterations must survive the repair, got: {repaired}"
    );
    assert!(
        repaired.contains("Iteration two: here are the next steps."),
        "the leaking iteration's prose must survive, got: {repaired}"
    );
    // The repair is idempotent: a second pass finds nothing to strip.
    assert_eq!(extract_leaked_suggestions(&repaired).0, repaired);
}

#[test]
fn row_repair_also_cleans_an_unparseable_payload() {
    // A malformed block (one trailing comma) recovers no items, but its
    // markers and payload must still leave the row — otherwise the model
    // reads raw JSON back as an example of "how I format options".
    let stored = "Options:\n<<suggest_options>>[{\"label\":\"A\"},]<<suggest_options>>";
    let (repaired, items) = extract_leaked_suggestions(stored);

    assert!(items.is_none(), "a malformed payload recovers no items");
    assert!(!repaired.contains("<<suggest_options"));
    assert!(!repaired.contains("{\"label\":\"A\"}"));
}
