//! Ephemeral group replies: Bot API 10.2 `receiver_user_id` (#756).
//!
//! Covers the two pure pieces: who a reply is scoped to, and the request
//! shape sent to `sendMessage`. The transport itself needs a live bot.

use crate::channels::telegram::ephemeral::{build_body, build_rich_body, receiver_for};
use teloxide::types::{MessageId, ThreadId};

#[test]
fn outcome_carries_a_declared_refusal_distinct_from_rejection() {
    // #635 — the structural half of step 3. A throttle is not the server
    // stating an opinion, so the refusal must be its own variant: folding it
    // into Rejected is what would cache a busy minute as a capability verdict.
    let src = include_str!("../channels/telegram/ephemeral.rs");
    assert!(
        src.contains("Deferred,"),
        "Outcome must carry a Deferred variant"
    );
    assert!(
        !src.contains("tokio::time::sleep(retry_after)"),
        "the raw ungated sleep must be gone - it slept without a permit and \
         without arming the cooldown every other path obeys"
    );
    assert!(
        src.contains("wait_out(method, retry_after"),
        "the 429 must go through the shared bounded policy"
    );
    assert!(
        src.contains("acquire_global_permit()"),
        "the ephemeral path must reach a permit"
    );
    // The Deferred arm must not be folded into the capability cache.
    assert!(
        !src.contains("Outcome::Deferred => {\n            RICH_SCOPING"),
        "a deferred send must never be cached as RICH_UNSUPPORTED"
    );
}

#[test]
fn a_deferred_ephemeral_send_falls_back_to_public_rather_than_dropping() {
    // #635 step 3's done-when: a CooldownActive refusal on the ephemeral path
    // must produce a public send, not a dropped message. Two halves hold that,
    // and either alone would not: the mapping `Deferred => false` (the same
    // value Transport returns) AND the caller's existing fall-through to the
    // public HTML path. So both are pinned here.
    let ephemeral = include_str!("../channels/telegram/ephemeral.rs");
    let caller = include_str!("../channels/telegram/commands_tg.rs");
    assert!(
        ephemeral.contains("Outcome::Deferred => false,"),
        "Deferred must map to false - the value the caller already treats as \
         'not delivered, fall through'"
    );
    assert!(
        caller.contains("try_send_rich("),
        "the caller must still consult try_send_rich"
    );
    assert!(
        caller.contains("send_html_chunks("),
        "the caller must keep the public HTML fallback that a false return \
         reaches - without it a deferred send would be dropped"
    );
}

#[test]
fn dm_never_scopes_a_reply() {
    // A DM has nobody to hide the reply from, and asking for an untested
    // parameter there would risk the one path that already works.
    assert_eq!(receiver_for(true, 12345), None);
}

#[test]
fn group_scopes_the_reply_to_the_invoker() {
    assert_eq!(receiver_for(false, 12345), Some(12345));
}

#[test]
fn body_carries_receiver_user_id() {
    let body = build_body(-100200, None, 12345, "hello", false);
    assert_eq!(body["chat_id"], -100200);
    assert_eq!(body["text"], "hello");
    assert_eq!(body["receiver_user_id"], 12345);
}

#[test]
fn plain_body_has_no_parse_mode() {
    // Acks like "✅ New session started." are literal text, and an HTML parse
    // mode would swallow any `<` or `&` they happen to contain.
    let body = build_body(-100200, None, 12345, "a < b & c", false);
    assert!(body.get("parse_mode").is_none());
}

#[test]
fn html_body_sets_parse_mode() {
    let body = build_body(-100200, None, 12345, "<b>hi</b>", true);
    assert_eq!(body["parse_mode"], "HTML");
}

#[test]
fn rich_body_is_the_public_body_plus_the_receiver() {
    // The scoped rich attempt must be byte-for-byte the public rich request
    // apart from the scoping field, or the two paths render differently.
    let public = crate::channels::telegram::rich::api::build_body(-100200, None, "# hi", None);
    let scoped = build_rich_body(-100200, None, 12345, "# hi");
    assert_eq!(scoped["rich_message"], public["rich_message"]);
    assert_eq!(scoped["chat_id"], public["chat_id"]);
    assert_eq!(scoped["receiver_user_id"], 12345);
}

#[test]
fn rich_body_keeps_the_forum_topic() {
    let body = build_rich_body(-100200, Some(ThreadId(MessageId(77))), 12345, "# hi");
    assert_eq!(body["message_thread_id"], 77);
    assert_eq!(body["receiver_user_id"], 12345);
}

#[test]
fn thread_id_targets_the_forum_topic() {
    let body = build_body(-100200, Some(ThreadId(MessageId(77))), 12345, "hi", true);
    assert_eq!(body["message_thread_id"], 77);
}

#[test]
fn no_thread_id_omits_the_field() {
    // Sending `message_thread_id: null` to a non-forum chat is an API error,
    // so the field has to be absent rather than explicitly empty.
    let body = build_body(-100200, None, 12345, "hi", true);
    assert!(body.get("message_thread_id").is_none());
}
