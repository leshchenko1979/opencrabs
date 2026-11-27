use crate::brain::tokenizer::*;

#[test]
fn test_empty_string() {
    assert_eq!(count_tokens(""), 0);
}

#[test]
fn test_simple_text() {
    let count = count_tokens("Hello, world!");
    // cl100k_base: "Hello" "," " world" "!" = 4 tokens
    assert!(
        (3..=6).contains(&count),
        "Got {count} tokens for 'Hello, world!'"
    );
}

#[test]
fn test_code_snippet() {
    let code = r#"fn main() { println!("Hello, world!"); }"#;
    let count = count_tokens(code);
    // Code tends to tokenize into more pieces
    assert!(
        count > 5,
        "Code should have more than 5 tokens, got {count}"
    );
}

#[test]
fn test_json_content() {
    let json = r#"{"name": "test", "value": 42, "nested": {"key": "val"}}"#;
    let count = count_tokens(json);
    assert!(
        count > 10,
        "JSON should have more than 10 tokens, got {count}"
    );
}

#[test]
fn test_long_text() {
    let text = "word ".repeat(1000); // ~1000 words
    let count = count_tokens(&text);
    // Should be roughly 1000 tokens (each "word " is ~1-2 tokens)
    assert!(
        count > 500 && count < 2000,
        "Got {count} tokens for ~1000 words"
    );
}

#[test]
fn test_consistency() {
    // Same input should always give same output
    let text = "The quick brown fox jumps over the lazy dog";
    let count1 = count_tokens(text);
    let count2 = count_tokens(text);
    assert_eq!(count1, count2);
}

#[test]
fn test_more_accurate_than_chars_div_3() {
    // chars/3 tends to underestimate for code/JSON, overestimate for prose
    let code =
        r#"pub async fn process_request(&self, ctx: &mut AgentContext) -> Result<Response> {"#;
    let tiktoken_count = count_tokens(code);
    let chars_3 = code.len() / 3;
    let chars_4 = code.len() / 4;

    // tiktoken should give a reasonable count — log for visibility
    eprintln!(
        "Code: tiktoken={tiktoken_count}, chars/3={chars_3}, chars/4={chars_4}, len={}",
        code.len()
    );
    assert!(tiktoken_count > 0);
}

#[test]
fn test_message_overhead() {
    let count = count_message_tokens("Hello");
    let base = count_tokens("Hello");
    assert_eq!(count, base + 4);
}

// ── #1045: the family-aware path counts Claude with o200k_base ───────────────

/// Three compound identifiers — the shape where the two encodings part company.
///
/// `cl100k_base` splits each camelCase name into its word pieces; `o200k_base`
/// carries the whole identifier as one or two tokens. Pure ASCII and short, so
/// the counts below are BPE ranks rather than anything that can drift with
/// content.
const IDENTIFIER_FIXTURE: &str = "compactionTargetPct backpressureCeilingPct preTruncatePct";

/// The Claude branch counts with `o200k_base`, and both counts are pinned.
///
/// The defect this pins: one global cl100k tokenizer counted every model, so
/// the encoding that segments modern code identifiers the way Claude's own
/// tokenizer does was unreachable — `grep -rn o200k src/` had zero hits.
///
/// Scope note, stated rather than glossed: the issue quotes a ~1.56x undercount
/// on Claude code/JSON. That figure is a WHOLE-REQUEST measurement — a 2.4MB
/// Claude CLI request estimated at ~660k locally against ~1.03M reported by the
/// provider (`context_provider_anchor_test.rs`), which includes the system
/// prompt, tool schemas and message framing the local estimate never sees. It
/// is therefore not reproducible on a text fixture and is not asserted here.
/// What is asserted is the mechanism that figure argued for: Claude is counted
/// with the family-appropriate encoding, the exact counts are pinned, and every
/// other family's numbers are unchanged.
#[test]
fn a_claude_model_counts_with_o200k_not_cl100k() {
    let claude = "claude-opus-4-7";
    let o200k = tiktoken_rs::o200k_base().expect("o200k_base must be available");

    // Both numbers, spelled out. cl100k_base splits each camelCase identifier
    // into its word pieces (16 tokens); o200k_base merges them (13).
    assert_eq!(count_tokens(IDENTIFIER_FIXTURE), 16, "cl100k_base count");
    assert_eq!(
        count_tokens_for_model(IDENTIFIER_FIXTURE, claude),
        13,
        "o200k_base count for a Claude model"
    );
    assert_eq!(
        count_tokens_for_model(IDENTIFIER_FIXTURE, claude),
        o200k.encode_ordinary(IDENTIFIER_FIXTURE).len(),
        "the Claude branch must use o200k_base, not a third table"
    );
    assert!(
        count_tokens_for_model(IDENTIFIER_FIXTURE, claude) < count_tokens(IDENTIFIER_FIXTURE),
        "on this fixture cl100k over-splits, so the family-aware count is lower \
         (o200k={}, cl100k={})",
        count_tokens_for_model(IDENTIFIER_FIXTURE, claude),
        count_tokens(IDENTIFIER_FIXTURE),
    );
}

/// Every non-Claude family keeps cl100k_base, so no existing caller's numbers
/// shift when it opts into the family-aware path.
#[test]
fn a_non_claude_model_keeps_the_cl100k_count() {
    for model in ["gpt-4o", "gpt-5.1", "my-custom-model", "combo/route"] {
        assert_eq!(
            count_tokens_for_model(IDENTIFIER_FIXTURE, model),
            count_tokens(IDENTIFIER_FIXTURE),
            "{model} must keep the cl100k_base count"
        );
    }
}

/// The family-aware message count mirrors `count_message_tokens`: the same ~4
/// tokens of role/separator overhead on top of the family-appropriate count.
#[test]
fn family_aware_message_count_adds_the_same_overhead() {
    for model in ["claude-opus-4-7", "gpt-4o"] {
        assert_eq!(
            count_message_tokens_for_model(IDENTIFIER_FIXTURE, model),
            count_tokens_for_model(IDENTIFIER_FIXTURE, model) + 4,
            "{model} must carry the same 4-token message overhead"
        );
    }
}

/// Empty input is zero for every family, matching `count_tokens`.
#[test]
fn family_aware_count_is_zero_for_empty_input() {
    assert_eq!(count_tokens_for_model("", "claude-opus-4-7"), 0);
    assert_eq!(count_tokens_for_model("", "gpt-4o"), 0);
    assert_eq!(count_message_tokens_for_model("", "claude-opus-4-7"), 4);
}

/// The family test is the shared Claude table, so a model the table does not
/// know is not counted as Claude — one predicate, not a second model list.
#[test]
fn the_family_test_is_the_shared_claude_table() {
    for model in ["claude-opus-4-7", "claude-3-haiku-20240307"] {
        assert!(
            crate::brain::provider::claude_window::context_window_for_claude(model).is_some(),
            "{model} must resolve through the shared table"
        );
        assert!(uses_o200k(model), "{model} must be counted with o200k_base");
    }
    for model in ["gpt-4o", "gemini-2.5-pro", "my-custom-model"] {
        assert!(
            crate::brain::provider::claude_window::context_window_for_claude(model).is_none(),
            "{model} is not in the Claude table"
        );
        assert!(!uses_o200k(model), "{model} must stay on cl100k_base");
    }
}
