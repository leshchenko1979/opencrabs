use crate::brain::agent::context::*;
use crate::brain::provider::ContentBlock;
use crate::brain::provider::Message;
use uuid::Uuid;

#[test]
fn test_context_creation() {
    let session_id = Uuid::new_v4();
    let context = AgentContext::new(session_id, 4096);

    assert_eq!(context.session_id, session_id);
    assert_eq!(context.max_tokens, 4096);
    assert_eq!(context.token_count, 0);
    assert!(context.messages.is_empty());
}

#[test]
fn test_add_message() {
    let session_id = Uuid::new_v4();
    let mut context = AgentContext::new(session_id, 4096);

    let message = Message::user("Hello, how are you?");
    context.add_message(message);

    assert_eq!(context.messages.len(), 1);
    assert!(context.token_count > 0);
}

#[test]
fn test_system_brain() {
    let session_id = Uuid::new_v4();
    let context = AgentContext::new(session_id, 4096)
        .with_system_brain("You are a helpful assistant.".to_string());

    assert!(context.system_brain.is_some());
    assert!(context.token_count > 0);
}

#[test]
fn test_token_estimation() {
    let tokens = AgentContext::estimate_tokens("Hello world");
    assert!(tokens > 0);
    assert!(tokens < 10); // Should be around 2-3 tokens
}

#[test]
fn test_would_exceed_limit() {
    let session_id = Uuid::new_v4();
    let mut context = AgentContext::new(session_id, 100);

    let message = Message::user("Hello");
    context.add_message(message);

    assert!(!context.would_exceed_limit(10));
    assert!(context.would_exceed_limit(1000));
}

#[test]
fn test_usage_percentage() {
    let session_id = Uuid::new_v4();
    let mut context = AgentContext::new(session_id, 100);

    // Add message that uses ~50 tokens
    let long_text = "a".repeat(200); // ~50 tokens
    let message = Message::user(long_text);
    context.add_message(message);

    let usage = context.usage_percentage();
    assert!(usage > 0.0 && usage <= 100.0);
}

#[test]
fn test_trim_to_fit() {
    let session_id = Uuid::new_v4();
    let mut context = AgentContext::new(session_id, 100);

    // Add several messages with longer text to ensure they exceed limit
    for i in 0..5 {
        let long_text = format!(
            "This is a longer message {} that will use more tokens to ensure we actually need to trim",
            i
        );
        let message = Message::user(long_text);
        context.add_message(message);
    }

    let original_count = context.messages.len();
    context.trim_to_fit(10); // Require 10 tokens space, forcing trimming

    // Should have removed some messages
    assert!(context.messages.len() < original_count);
}

#[test]
fn test_compact_with_summary_keeps_recent() {
    let session_id = Uuid::new_v4();
    let mut context = AgentContext::new(session_id, 10000);

    // Add 10 messages
    for i in 0..10 {
        context.add_message(Message::user(format!("Message {}", i)));
    }
    assert_eq!(context.messages.len(), 10);

    // Use 80% of max_tokens as budget — should keep all short messages
    let budget = (10000.0 * 0.80) as usize;
    context.compact_with_summary("Summary of old messages".to_string(), budget);

    // First message should be the compaction summary
    if let Some(ContentBlock::Text { text }) = context.messages[0].content.first() {
        assert!(text.contains("CONTEXT COMPACTION"));
        assert!(text.contains("Summary of old messages"));
    } else {
        panic!("First message should be a text compaction summary");
    }

    // Last kept message should be Message 9
    if let Some(ContentBlock::Text { text }) = context.messages.last().unwrap().content.first() {
        assert!(text.contains("Message 9"));
    } else {
        panic!("Last message should be Message 9");
    }
}

#[test]
fn test_compact_with_verbatim_tail_charges_the_summary_on_top() {
    // #943: `compact_with_summary` subtracts the summary's size from the
    // budget, so passing a tail budget directly keeps only the 2-message
    // structural floor whenever the summary is bigger than the tail — which it
    // always is, because the summary budget is 3 000 tokens. The wrapper has to
    // add the summary's own size back, and the discriminator is that it keeps
    // far more than the floor.
    let session_id = Uuid::new_v4();
    let mut context = AgentContext::new(session_id, 200_000);
    for i in 0..10 {
        context.add_message(Message::user(format!("Message {}", i)));
    }

    // A summary far larger than the 2 000-token tail we ask for.
    let big_summary = "s".repeat(12_000);
    context.compact_with_verbatim_tail(big_summary, 2_000);

    assert!(
        context.messages.len() > 3,
        "the tail collapsed to the 2-message floor because the summary ate the \
         budget: {} messages kept",
        context.messages.len()
    );
    if let Some(ContentBlock::Text { text }) = context.messages[0].content.first() {
        assert!(
            text.contains("CONTEXT COMPACTION"),
            "summary is not the anchor"
        );
    } else {
        panic!("First message should be a text compaction summary");
    }
    if let Some(ContentBlock::Text { text }) = context.messages.last().unwrap().content.first() {
        assert!(
            text.contains("Message 9"),
            "the newest message was not kept: {text}"
        );
    } else {
        panic!("Last message should be text");
    }
}

#[test]
fn test_compact_with_summary_recalculates_tokens() {
    let session_id = Uuid::new_v4();
    let mut context = AgentContext::new(session_id, 10000);

    // Add many large messages to build up token count
    for i in 0..20 {
        let big_text = format!("Large message {} {}", i, "x".repeat(400));
        context.add_message(Message::user(big_text));
    }

    let tokens_before = context.token_count;
    assert!(tokens_before > 0);

    // Very small budget — should drop most messages
    context.compact_with_summary("Brief summary".to_string(), 500);

    // Token count should be recalculated and less than before
    assert!(context.token_count < tokens_before);
    assert!(context.token_count > 0);
}

#[test]
fn test_compact_with_summary_fewer_messages_than_keep() {
    let session_id = Uuid::new_v4();
    let mut context = AgentContext::new(session_id, 10000);

    // Add only 2 messages — large budget should keep them all
    context.add_message(Message::user("Hello".to_string()));
    context.add_message(Message::user("World".to_string()));

    context.compact_with_summary("Summary".to_string(), 8000);

    // Should have: 1 summary + 2 original = 3
    assert_eq!(context.messages.len(), 3);
}

#[test]
fn test_compact_with_summary_preserves_system_brain_tokens() {
    let session_id = Uuid::new_v4();
    let mut context = AgentContext::new(session_id, 10000)
        .with_system_brain("You are an AI assistant".to_string());

    for i in 0..5 {
        context.add_message(Message::user(format!("Msg {}", i)));
    }

    context.compact_with_summary("Summary".to_string(), 500);

    // Token count should include system brain tokens
    let brain_tokens = AgentContext::estimate_tokens("You are an AI assistant");
    assert!(context.token_count >= brain_tokens);
}

#[test]
fn test_compact_empty_context() {
    let session_id = Uuid::new_v4();
    let mut context = AgentContext::new(session_id, 10000);

    // No messages, compact should still work
    context.compact_with_summary("Summary of nothing".to_string(), 8000);

    // Should have just the summary message
    assert_eq!(context.messages.len(), 1);
}

// ── #1063: the context counts at its model's vocabulary ─────────────────────

/// The same three compound identifiers `brain_tokenizer_test.rs` pins at 16
/// (`cl100k_base`) / 13 (`o200k_base`).
///
/// Duplicated rather than shared: both files are `#[cfg(test)]` modules with no
/// common fixture module, so the counts are re-pinned HERE as well — a drift in
/// either copy turns both red instead of letting the two files quietly disagree.
const MODEL_FIXTURE: &str = "compactionTargetPct backpressureCeilingPct preTruncatePct";

/// A message costs its text plus 4 tokens of role/separator overhead, so the
/// two encodings land at 20 and 17 for the fixture above.
const FIXTURE_CL100K_MESSAGE: usize = 20;
const FIXTURE_O200K_MESSAGE: usize = 17;

/// A model id the shared Claude table resolves, so `uses_o200k` is true.
const CLAUDE_MODEL: &str = "claude-opus-4-7";

/// The numerator half of the compaction gate (#1063): a context that knows it
/// is serving a Claude model counts its messages with `o200k_base`, the
/// vocabulary closest to that provider's own tokenizer. A context that knows
/// nothing keeps the family-blind `cl100k_base` — the pre-#1063 number.
#[test]
fn a_claude_context_counts_a_camel_case_message_with_o200k() {
    let session_id = Uuid::new_v4();

    let mut blind = AgentContext::new(session_id, 200_000);
    blind.add_message(Message::user(MODEL_FIXTURE));
    assert_eq!(
        blind.token_count, FIXTURE_CL100K_MESSAGE,
        "no model: 16 text tokens + 4 overhead"
    );

    let mut claude = AgentContext::new(session_id, 200_000).with_model(CLAUDE_MODEL);
    claude.add_message(Message::user(MODEL_FIXTURE));
    assert_eq!(
        claude.token_count, FIXTURE_O200K_MESSAGE,
        "claude: 13 text tokens + 4 overhead"
    );
}

/// The negative control: a model the Claude table does not know keeps the
/// cl100k number, so `uses_o200k` returning `false` is observable in a count
/// and not merely in a predicate.
#[test]
fn a_non_claude_context_keeps_the_cl100k_count() {
    assert!(!crate::brain::tokenizer::uses_o200k("gpt-4o"));

    let mut gpt = AgentContext::new(Uuid::new_v4(), 200_000).with_model("gpt-4o");
    gpt.add_message(Message::user(MODEL_FIXTURE));
    assert_eq!(
        gpt.token_count, FIXTURE_CL100K_MESSAGE,
        "gpt-4o must stay on cl100k_base"
    );
}

/// `with_model` re-derives a total that was counted before the model was
/// known — the `from_db_messages` shape — instead of leaving a cl100k figure
/// on an o200k turn.
#[test]
fn with_model_recounts_what_was_already_counted() {
    let mut context =
        AgentContext::new(Uuid::new_v4(), 200_000).with_system_brain(MODEL_FIXTURE.to_string());
    context.add_message(Message::user(MODEL_FIXTURE));
    assert_eq!(
        context.token_count,
        16 + FIXTURE_CL100K_MESSAGE,
        "brain (16, no overhead) + message (20)"
    );

    let recount = context.with_model(CLAUDE_MODEL);
    assert_eq!(
        recount.token_count,
        13 + FIXTURE_O200K_MESSAGE,
        "brain (13) + message (17) after the recount"
    );
}

/// The static counter is held as a function pointer at three production sites
/// (`.map(AgentContext::estimate_tokens_static)`), so its signature cannot
/// grow a model. The model-aware form is a separate function, and both are
/// pinned here: the pointer keeps the family-blind number, the `_for` form
/// gives the Claude one.
#[test]
fn the_static_counter_stays_a_function_pointer_and_is_family_aware() {
    let pointer: fn(&Message) -> usize = AgentContext::estimate_tokens_static;
    let message = Message::user(MODEL_FIXTURE);

    assert_eq!(
        pointer(&message),
        FIXTURE_CL100K_MESSAGE,
        "the pointer is family-blind by construction"
    );
    assert_eq!(
        AgentContext::estimate_tokens_static_for(&message, CLAUDE_MODEL),
        FIXTURE_O200K_MESSAGE,
        "a site that knows its model must call the _for form"
    );
}

/// The wired production path, not just the primitive: `delta_token_count`
/// feeds the compaction scope decision (`delta` vs consolidate), and it now
/// counts through the context's own model instead of the family-blind form.
#[test]
fn the_compaction_scope_count_is_family_aware() {
    let mut blind = AgentContext::new(Uuid::new_v4(), 200_000);
    blind.add_message(Message::user(MODEL_FIXTURE));
    assert_eq!(blind.delta_token_count(), FIXTURE_CL100K_MESSAGE);

    let mut claude = AgentContext::new(Uuid::new_v4(), 200_000).with_model(CLAUDE_MODEL);
    claude.add_message(Message::user(MODEL_FIXTURE));
    assert_eq!(claude.delta_token_count(), FIXTURE_O200K_MESSAGE);
}
