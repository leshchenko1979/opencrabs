//! The correction injected when a turn claims work it never ran (#796, #797).
//!
//! The old wording stated what was missing ("your last response produced ZERO
//! tool_use blocks"), which argues against a position the model does not hold.
//! A model that believes it already ran `gh issue list` reads that as a
//! formatting complaint and keeps the belief. These pin the wording that
//! replaces it: the mechanism, and a check the model can actually apply.
//!
//! Fixtures are synthetic and carry no user identifiers.

#[cfg(feature = "telegram")]
use crate::brain::agent::service::nudge::mermaid_regen_nudge;
use crate::brain::agent::service::nudge::{no_tool_calls_nudge, uncalled_commands_nudge};

#[test]
fn a_fabricated_command_is_quoted_back() {
    // The whole point of #797: cite the fact, do not gesture at a category.
    let nudge = uncalled_commands_nudge(&["gh issue list --state open".to_string()]);
    assert!(
        nudge.contains("`gh issue list --state open`"),
        "the claimed command must appear verbatim: {nudge}"
    );
    assert!(
        nudge.contains("No such call ran this turn"),
        "the correction must state the fact plainly: {nudge}"
    );
}

#[test]
fn several_fabricated_commands_are_all_quoted() {
    let nudge = uncalled_commands_nudge(&[
        "git log --oneline -5".to_string(),
        "cargo test --all-features".to_string(),
    ]);
    assert!(nudge.contains("`git log --oneline -5`"), "{nudge}");
    assert!(nudge.contains("`cargo test --all-features`"), "{nudge}");
}

#[test]
fn the_named_variant_shares_the_mechanism_and_the_escape() {
    // It must not drift from the generic wording: same premise, same exit.
    let nudge = uncalled_commands_nudge(&["ls -la".to_string()]);
    assert!(
        nudge.contains("nothing runs inside your reasoning"),
        "{nudge}"
    );
    assert!(nudge.contains("imagined it"), "{nudge}");
    assert!(nudge.contains("genuinely done"), "{nudge}");
    assert!(nudge.starts_with("[System:"), "{nudge}");
    assert!(nudge.ends_with(']'), "{nudge}");
}

#[test]
fn every_variant_states_that_reasoning_cannot_execute() {
    // The mechanism is the load-bearing sentence. Without it the correction is
    // just a complaint about formatting, which is what failed before.
    for nudge in [no_tool_calls_nudge(true), no_tool_calls_nudge(false)] {
        assert!(
            nudge.contains("nothing runs inside your reasoning"),
            "must state the mechanism: {nudge}"
        );
        assert!(
            nudge.contains("imagined it"),
            "must name the false belief: {nudge}"
        );
    }
}

#[test]
fn every_variant_keeps_the_finished_escape() {
    // Without an exit that is not a tool call, a model that genuinely finished
    // gets nudged, calls something pointless to comply, and is nudged again.
    for nudge in [no_tool_calls_nudge(true), no_tool_calls_nudge(false)] {
        assert!(
            nudge.contains("genuinely done"),
            "real completion needs a non-tool exit: {nudge}"
        );
    }
}

#[test]
fn the_local_variant_avoids_the_word_stop() {
    // Qwen/Kimi/DeepSeek read "STOP" as "wait for further instruction" and
    // reply with an acknowledgement instead of calling anything.
    let nudge = no_tool_calls_nudge(true);
    assert!(
        !nudge.contains("STOP"),
        "local models treat STOP as an instruction to wait: {nudge}"
    );
}

#[test]
fn the_local_variant_names_the_structured_api() {
    // These models write `{"tool_call": {...}}` as message text believing that
    // IS the invocation, so the channel has to be named.
    let nudge = no_tool_calls_nudge(true);
    assert!(nudge.contains("structured tool-call API"), "{nudge}");
    assert!(
        nudge.contains("does not execute"),
        "must say that text-shaped calls do nothing: {nudge}"
    );
}

#[test]
fn a_nudge_is_framed_as_a_system_message() {
    // The loop injects these as user-role messages; the bracketed [System: ...]
    // framing is what keeps them from reading as the user's own words.
    for nudge in [no_tool_calls_nudge(true), no_tool_calls_nudge(false)] {
        assert!(nudge.starts_with("[System:"), "{nudge}");
        assert!(nudge.ends_with(']'), "{nudge}");
    }
}

// ── Mermaid regen nudge (#37) ──

#[cfg(feature = "telegram")]
#[test]
fn mermaid_regen_nudge_quotes_renderer_errors_and_counts_attempts() {
    let errors = vec!["Parse error on line 2: Lexical error".to_string()];
    let nudge = mermaid_regen_nudge(&errors, 1, 3);
    assert!(
        nudge.contains("Parse error on line 2: Lexical error"),
        "must quote the renderer's own error text: {nudge}"
    );
    assert!(nudge.contains("Regen attempt 1/3"), "{nudge}");
    assert!(
        nudge.contains("Correction rules:"),
        "must include unified correction rules: {nudge}"
    );
    assert!(nudge.contains("Mobile Layout & Aspect Ratio:"), "{nudge}");
    assert!(nudge.starts_with("[System:"), "{nudge}");
    assert!(nudge.ends_with(']'), "{nudge}");
}

#[cfg(feature = "telegram")]
#[test]
fn mermaid_regen_nudge_carries_a_too_large_remedy() {
    // #658: the preflight collector reports the too-large class beside the
    // parse class now, and this nudge is where it reaches the model — so the
    // note is DERIVED from the classifier rather than transcribed here, and
    // the two cannot drift.
    use crate::channels::telegram::rich::ast::MermaidResult;
    use crate::channels::telegram::rich::mermaid::classify_render_failure;

    let MermaidResult::TooLarge(note) = classify_render_failure(
        414,
        "<html><head><title>414 Request-URI Too Large</title></head></html>",
    ) else {
        panic!("a 414 must classify as TooLarge");
    };
    // Checked BEFORE the note is moved into the nudge call below.
    assert!(
        !note.contains('<'),
        "the note itself must be markup-free: {note}"
    );
    let nudge = mermaid_regen_nudge(&[note], 1, 3);
    assert!(
        nudge.contains("shorten or split"),
        "the REAL remedy must reach the model — that is the whole point of the \
         class: {nudge}"
    );
    assert!(
        nudge.contains("syntax is valid"),
        "the note must warn against the syntax repair the old note implied: {nudge}"
    );
    // #658: the nudge embeds `unified_mermaid_rules()`, whose pre-existing prose
    // carries a literal '<br/>' ("only '<br/>' is allowed for line breaks").
    // So `!nudge.contains('<')` is unsatisfiable and would have failed CI — it
    // did. Scope the markup check to the QUOTED region (everything between the
    // header line and the correction rules), which is where a renderer body
    // would land. `html_title` already stripped it, so this asserts the seam.
    let quoted = nudge
        .split_once('\n')
        .and_then(|(_, rest)| rest.split_once("Correction rules:"))
        .map(|(quoted, _)| quoted.to_string())
        .expect("the nudge must carry a quoted region before the correction rules");
    assert!(
        !quoted.contains('<'),
        "no renderer markup may reach the model in the quoted region: {quoted}"
    );
    assert!(
        !nudge.contains("<html"),
        "no proxy markup may reach the model anywhere in the nudge: {nudge}"
    );
}

// ── Local image nudges (#286) ──

#[test]
fn local_image_regen_nudge_quotes_each_reference_and_the_base_dir() {
    use crate::brain::agent::service::nudge::local_image_regen_nudge;
    use crate::utils::image::{LocalImageFailure, LocalImageFailureReason};
    use std::path::PathBuf;

    let failures = vec![
        LocalImageFailure {
            raw: "chart.png".to_string(),
            resolved: Some(PathBuf::from("/srv/work/chart.png")),
            reason: LocalImageFailureReason::NotFound,
        },
        LocalImageFailure {
            raw: "https://example.com/gone.png".to_string(),
            resolved: None,
            reason: LocalImageFailureReason::DownloadFailed,
        },
    ];
    let nudge = local_image_regen_nudge(&failures, Some(std::path::Path::new("/srv/work")), 1, 2);
    assert!(nudge.starts_with("[System:"), "{nudge}");
    assert!(nudge.ends_with(']'), "{nudge}");
    assert!(nudge.contains("chart.png (file not found)"), "{nudge}");
    assert!(
        nudge.contains("https://example.com/gone.png (could not be downloaded"),
        "{nudge}"
    );
    assert!(
        nudge.contains("/srv/work"),
        "must name the base dir: {nudge}"
    );
    assert!(nudge.contains("Regen attempt 1/2"), "{nudge}");
}

#[test]
fn local_image_regen_nudge_without_a_base_dir_says_to_use_an_absolute_path() {
    use crate::brain::agent::service::nudge::local_image_regen_nudge;
    use crate::utils::image::{LocalImageFailure, LocalImageFailureReason};

    let failures = vec![LocalImageFailure {
        raw: "rel.png".to_string(),
        resolved: None,
        reason: LocalImageFailureReason::NotFound,
    }];
    let nudge = local_image_regen_nudge(&failures, None, 2, 2);
    assert!(nudge.contains("absolute path"), "{nudge}");
    assert!(nudge.contains("Regen attempt 2/2"), "{nudge}");
}

#[test]
fn local_image_delivery_failure_nudge_forbids_re_emitting_the_reference() {
    use crate::brain::agent::service::nudge::local_image_delivery_failure_nudge;
    use crate::utils::image::{LocalImageFailure, LocalImageFailureReason};
    use std::path::PathBuf;

    let failures = vec![LocalImageFailure {
        raw: "/srv/work/big.png".to_string(),
        resolved: Some(PathBuf::from("/srv/work/big.png")),
        reason: LocalImageFailureReason::DeliveryFailed,
    }];
    let nudge = local_image_delivery_failure_nudge(&failures);
    assert!(nudge.starts_with("[System:"), "{nudge}");
    assert!(nudge.ends_with(']'), "{nudge}");
    assert!(
        nudge.contains("/srv/work/big.png (the channel could not deliver the image)"),
        "{nudge}"
    );
    assert!(
        nudge.contains("do not re-emit it"),
        "a delivery failure is not fixed by rewriting the reference: {nudge}"
    );
}
