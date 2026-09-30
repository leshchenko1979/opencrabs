//! Send-correlation telemetry for the Telegram surface (#1085).

use crate::channels::telegram::telemetry::*;

#[test]
fn hash8_is_stable_and_8_hex_chars() {
    let a = content_hash8("hello world");
    let b = content_hash8("hello world");
    assert_eq!(a, b);
    assert_eq!(a.len(), 8);
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn hash8_separates_different_content() {
    assert_ne!(content_hash8("hello world"), content_hash8("hello worlD"));
}

#[test]
fn hash8_handles_empty_and_multibyte() {
    assert_eq!(content_hash8("").len(), 8);
    // PT-PT and emoji content must not panic (multibyte boundary safety).
    assert_eq!(content_hash8("ção 🦀 açúcar").len(), 8);
}

// ---------------------------------------------------------------------------
// #721 (E1): outbound-REQUEST telemetry — typing / delete / reaction.
//
// These lines answer a different question from the landing lines: not "what
// reached the chat" but "what did we put ON THE WIRE". A request that fails is
// still a request, so the line is emitted before the API call and its presence
// says nothing about the outcome. The field order is the contract — a reader
// parses by position — so it is pinned exactly.
// ---------------------------------------------------------------------------

#[test]
fn request_line_pins_the_field_order_for_a_targeted_request() {
    let line = request_line(
        "turn",
        "typing loop",
        "-",
        "delete",
        "deleteMessage",
        -1001234567890,
        Some(7),
        Some(42),
    );
    assert_eq!(
        line,
        "Telegram request: origin=turn detail=typing loop session=- kind=delete path=deleteMessage chat=-1001234567890 thread=Some(7) msg=42"
    );
}

/// A chat action targets no message. `msg` is then the create-shaped dash
/// (the convention #676 established for rich creates), NOT an absent field:
/// every line carries every field, so a reader never has to guess a shape.
#[test]
fn request_line_uses_the_create_shape_when_a_request_targets_no_message() {
    let line = request_line(
        "turn",
        "typing tick",
        "-",
        "typing",
        "sendChatAction",
        -1001234567890,
        None,
        None,
    );
    assert!(
        line.ends_with("chat=-1001234567890 thread=None msg=-"),
        "a chat action targets no message, so msg must be the create-shaped dash; got: {line}"
    );
}

/// `session` is the originating session id where the site knows it and `-`
/// where it genuinely cannot — the existing `log_send_success` convention,
/// unchanged.
#[test]
fn request_line_keeps_the_dash_session_fallback() {
    let line = request_line(
        "system",
        "probe_topic",
        "-",
        "typing",
        "sendChatAction",
        555,
        Some(3),
        None,
    );
    assert!(line.contains("session=- kind=typing"), "got: {line}");
}

/// A request has no body, so the two body-describing fields of the send
/// schema (`len`, `hash8`) are DROPPED rather than carried as dead dashes.
#[test]
fn request_line_carries_no_message_body_fields() {
    let line = request_line("tool", "delete", "-", "delete", "deleteMessage", 1, None, Some(2));
    assert!(!line.contains("len="), "a request has no body; got: {line}");
    assert!(
        !line.contains("hash8="),
        "a request has no body; got: {line}"
    );
}

/// The prefix is what makes a request greppable WITHOUT matching a landing,
/// and vice versa: `oc-log-search 'Telegram request:'` must never return a
/// send that landed.
#[test]
fn request_line_prefix_cannot_be_confused_with_a_landing_line() {
    let line = request_line(
        "turn",
        "typing loop",
        "-",
        "typing",
        "sendChatAction",
        1,
        None,
        None,
    );
    assert!(line.starts_with("Telegram request: "), "got: {line}");
    assert!(!line.contains("Telegram send ok:"), "got: {line}");
    assert!(!line.contains("Telegram send failed:"), "got: {line}");
}

