//! Telegram resume: sender label and short session id formatting.

use crate::brain::agent::PushOrigin;
use crate::channels::telegram::TelegramState;
use crate::channels::telegram::flow::{QUEUED_PREVIEW_MAX, queued_preview};
use crate::channels::telegram::resume::*;
use uuid::Uuid;

/// Owner ruling 2026-08-28: a session sitting in a DM with the bot must
/// be labelled with the BOT's username, never the reader's own name —
/// the reader IS the chat's human side, so handing it back as the
/// sender is useless.
#[tokio::test]
async fn dm_session_labels_the_bot_not_the_reader() {
    let state = TelegramState::new();
    state.set_bot_username("test_bot".to_owned()).await;
    let sender = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
    state.register_session_chat(sender, 12345, None).await;
    let bot = teloxide::Bot::new("42:TEST");
    assert_eq!(
        sender_label(&state, &bot, sender, -100_999).await,
        "test_bot"
    );
}

/// Empty get_me cache (shouldn't happen post-boot): the DM arm must
/// degrade to the short session id, never to a getChat lookup that
/// would return the reader's own profile.
#[tokio::test]
async fn dm_session_without_cached_bot_username_falls_back_to_short_id() {
    let state = TelegramState::new();
    let sender = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
    state.register_session_chat(sender, 12345, None).await;
    let bot = teloxide::Bot::new("42:TEST");
    assert_eq!(
        sender_label(&state, &bot, sender, -100_999).await,
        short_session_id(sender)
    );
}

#[test]
fn roll_line_joins_label_and_first_body_line() {
    let line = build_notify_roll_line("HQ", "*bzzt* status: green");
    assert_eq!(line, "📨 notify from HQ: *bzzt* status: green");
}

/// #61: only the FIRST body line is the announcement — the full text
/// reaches the session via the queue; a multiline body must not stack
/// into the roll line.
#[test]
fn roll_line_uses_only_first_body_line() {
    let line = build_notify_roll_line("ops", "first\nsecond\nthird");
    assert_eq!(line, "📨 notify from ops: first");
}

/// #61 dedupe: a body that carries its own "📨 notify from …:" echo
/// (hand-typed probe or quoted notify) must not render the phrase twice —
/// the envelope already names the sender. Header-shaped prefix up to the
/// first ':' is stripped (Alexey 2026-09-05, r4 smoke duplication).
#[test]
fn roll_line_strips_leading_self_echo_with_colon() {
    let line = build_notify_roll_line(
        "CLI tooling",
        "📨 notify from Smoke probe — 🔍 SMOKE r4: land on the roll",
    );
    assert_eq!(line, "📨 notify from CLI tooling: land on the roll");
}

#[test]
fn roll_line_strips_quoted_session_notify_echo() {
    let line = build_notify_roll_line("HQ", "📨 notify from HQ: build broke");
    assert_eq!(line, "📨 notify from HQ: build broke");
}

/// A "📨 notify from" line with no ':' can't be told apart from prose —
/// it must pass through untouched rather than eat content.
#[test]
fn roll_line_keeps_echo_without_colon() {
    let line = build_notify_roll_line("ops", "📨 notify from nobody");
    assert_eq!(line, "📨 notify from ops: 📨 notify from nobody");
}

/// Clean bodies are byte-identical through the strip pass.
#[test]
fn roll_line_untouched_for_clean_body() {
    let line = build_notify_roll_line("HQ", "*bzzt* status: green");
    assert_eq!(line, "📨 notify from HQ: *bzzt* status: green");
}

/// #61: labels are user data (topic/chat names). Same neutralization
/// the receipt card applies — angle brackets become single guillemets
/// before the line hits roll chrome that renders into HTML.
#[test]
fn roll_line_neutralizes_angle_brackets_in_label() {
    let line = build_notify_roll_line("<script>chat", "hello");
    assert!(!line.contains('<'), "no raw angle brackets: {line}");
    assert!(
        line.starts_with("📨 notify from ‹script›chat: hello"),
        "guillemet-swapped label: {line}"
    );
}

/// #61: the cap counts CHARS, not bytes — a Cyrillic/emoji-heavy
/// notify must truncate on a char boundary (no panics, no mojibake)
/// and mark the cut with an ellipsis.
#[test]
fn roll_line_caps_multibyte_on_char_boundary() {
    let label = "э".repeat(100);
    let body = "ж".repeat(100);
    let line = build_notify_roll_line(&label, &body);
    let count = line.chars().count();
    assert!(
        count <= NOTIFY_ROLL_LINE_MAX + 1,
        "cap + ellipsis, got {count}"
    );
    assert!(line.ends_with('…'), "cut marked with ellipsis");
}

/// #61: under the cap the line is verbatim — no ellipsis, no loss.
#[test]
fn roll_line_under_cap_has_no_ellipsis() {
    let line = build_notify_roll_line("ops", "short body");
    assert!(!line.ends_with('…'));
    assert!(line.contains("short body"));
}

/// #61 fold-dedupe: identical (sender, body) pairs fingerprint equal.
#[test]
fn notify_fingerprint_matches_same_notify() {
    let sender_uuid = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
    let s = NotifySender::Session(sender_uuid);
    assert_eq!(
        notify_fingerprint(&s, "same body"),
        notify_fingerprint(&s, "same body")
    );
}

/// #61 fold-dedupe: body or sender differences diverge; the tag byte
/// keeps a session uuid apart from a CLI label spelling the same text.
#[test]
fn notify_fingerprint_diverges_on_body_or_sender() {
    let u1 = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
    let u2 = Uuid::parse_str("99999999-2222-3333-4444-555555555555").unwrap();
    let s1 = NotifySender::Session(u1);
    let s2 = NotifySender::Session(u2);
    assert_ne!(
        notify_fingerprint(&s1, "body"),
        notify_fingerprint(&s1, "other body"),
        "body change diverges"
    );
    assert_ne!(
        notify_fingerprint(&s1, "body"),
        notify_fingerprint(&s2, "body"),
        "sender change diverges"
    );
    let cli = NotifySender::CliTooling("11111111-2222-3333-4444-555555555555");
    assert_ne!(
        notify_fingerprint(&s1, "body"),
        notify_fingerprint(&cli, "body"),
        "tag byte: uuid session != cli label with same text"
    );
    assert_eq!(
        notify_fingerprint(&cli, "body"),
        notify_fingerprint(&cli, "body"),
        "cli label stable"
    );
}

/// #554: the flow roll must quote the notify BODY, not the session_notify
/// transport envelope. The envelope is built at `subagent/notify.rs:266` —
/// machine header on line 1, body after it — so the old preview read
/// `📨 notify from 8b1e2c3d:` and the user learned only that *something*
/// arrived, plus a raw session-uuid prefix on an owner-facing surface.
#[test]
fn queued_preview_skips_the_session_notify_envelope() {
    let display = "📨 notify from 8b1e2c3d:\nthe build broke";
    let preview = queued_preview(display, &PushOrigin::SessionNotify);
    assert_eq!(preview, "the build broke");
    assert!(!preview.contains("notify from"), "envelope header must not leak");
    assert!(!preview.contains("8b1e2c3d"), "raw session id must not leak");
}

/// Control: the skip is gated on the ORIGIN, never on text shape alone — a
/// message that merely looks like an envelope is not silently gutted, because
/// only the `SessionNotify` arm produces one.
#[test]
fn queued_preview_keeps_non_envelope_text_unchanged() {
    assert_eq!(
        queued_preview("first line\nsecond line", &PushOrigin::Ingress),
        "first line",
        "non-envelope text previews its first line"
    );
    assert_eq!(
        queued_preview("📨 notify from 8b1e2c3d:\nbody", &PushOrigin::Ingress),
        "📨 notify from 8b1e2c3d:",
        "origin, not shape, decides the skip"
    );
}

/// #554: the two inline copies this helper replaced tested `len()` in BYTES
/// but sliced at char index 30 — so a 25-char Cyrillic line (50 bytes) took
/// the truncation arm, found nothing at index 30, and emitted the text
/// UNCHANGED with a spurious `…` appended. Both halves are pinned here.
#[test]
fn queued_preview_truncates_multibyte_on_char_boundary() {
    let short_multibyte = "Ж".repeat(25);
    assert_eq!(short_multibyte.chars().count(), 25);
    assert!(
        short_multibyte.len() > 30,
        "must exceed 30 BYTES — that is what tripped the old byte/char mix"
    );
    assert_eq!(
        queued_preview(&short_multibyte, &PushOrigin::SessionNotify),
        short_multibyte,
        "under the char cap: unchanged, and no spurious ellipsis"
    );

    let long_multibyte = "Ж".repeat(40);
    let preview = queued_preview(&long_multibyte, &PushOrigin::SessionNotify);
    assert_eq!(preview.chars().count(), QUEUED_PREVIEW_MAX + 1);
    assert!(preview.ends_with('…'), "over the cap earns exactly one ellipsis");
    assert_eq!(
        preview.chars().take(QUEUED_PREVIEW_MAX).collect::<String>(),
        "Ж".repeat(30),
        "the cut lands on exactly QUEUED_PREVIEW_MAX chars"
    );
}

/// Guards the roll's empty case: no body to preview must degrade to an empty
/// string, never a panic or a dangling envelope header.
#[test]
fn queued_preview_degrades_on_empty_body() {
    for text in ["", "\n  \n", "   ", "📨 notify from 8b1e2c3d:\n"] {
        assert!(
            queued_preview(text, &PushOrigin::SessionNotify).is_empty(),
            "empty/whitespace-only input previews empty"
        );
    }
    assert_eq!(
        queued_preview("📨 notify from 8b1e2c3d:", &PushOrigin::SessionNotify),
        "",
        "header with no body leaves nothing to quote"
    );
}
