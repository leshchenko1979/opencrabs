//! Tests for `replied_to_bot_as_interlocutor` — the reply-addressing guard that
//! decides whether a reply counts as addressing the bot (#527).
//!
//! The defect being guarded: a bot-created forum topic's ROOT message is a
//! `forum_topic_created` SERVICE notice, and Telegram records the bot as its
//! sender *by construction* — whoever creates the topic is the sender. A bare
//! sender-id comparison therefore read any reply to the topic root as "replied
//! to the bot", so `respond_to = mention` answered plain messages in exactly
//! the topics it was set to stay quiet in.
//!
//! Fixtures are raw Telegram JSON run through the real `Update` deserializer,
//! so these tests exercise the same types the handler receives off the wire
//! rather than a hand-built struct that could drift from the wire shape.
//!
//! Negative half: the two `!asserts` on service notices are the cases that FAIL
//! on the pre-fix tree (an unguarded sender-id comparison returns `true` for
//! both), so the tests discriminate rather than merely pass.

use crate::channels::telegram::handler::replied_to_bot_as_interlocutor;
use teloxide::types::{Message, Update, UpdateKind};

/// The bot that owns the topic in the report.
const BOT_ID: i64 = 7357853620;
const CHAT_ID: i64 = -1003889257179;
/// The topic root id is also the thread id — that equality is the whole reason
/// a reply to the root is easy to mistake for a reply to the bot.
const TOPIC_ROOT_ID: i64 = 7638;

fn chat() -> serde_json::Value {
    serde_json::json!({"id": CHAT_ID, "type": "supergroup", "title": "Test topic group"})
}

fn bot() -> serde_json::Value {
    serde_json::json!({"id": BOT_ID, "is_bot": true, "first_name": "Bot"})
}

fn member() -> serde_json::Value {
    serde_json::json!({"id": 133526395, "is_bot": false, "first_name": "Member"})
}

/// Parse a raw Telegram update into the `Message` the handler would receive.
fn parse(update: serde_json::Value) -> Message {
    // teloxide's `Update` deserializer only works from STRING input (#354):
    // `from_value` turns every update into `UpdateKind::Error`.
    let u: Update =
        serde_json::from_str(&update.to_string()).expect("update must deserialize from a string");
    match u.kind {
        UpdateKind::Message(m) => m,
        _ => panic!("expected a message update"),
    }
}

/// A member's plain (non-mentioning) message in the topic, replying to `reply_to`.
fn member_reply(reply_to: serde_json::Value) -> Message {
    parse(serde_json::json!({
        "update_id": 1,
        "message": {
            "message_id": 7928,
            "message_thread_id": TOPIC_ROOT_ID,
            "date": 1758635762,
            "chat": chat(),
            "from": member(),
            "text": "plain message, no mention",
            "reply_to_message": reply_to,
        }
    }))
}

#[test]
fn a_reply_to_an_ordinary_bot_message_addresses_the_bot() {
    let real = serde_json::json!({
        "message_id": 7927,
        "message_thread_id": TOPIC_ROOT_ID,
        "date": 1758635760,
        "chat": chat(),
        "from": bot(),
        "text": "here is my answer",
    });
    assert!(
        replied_to_bot_as_interlocutor(&member_reply(real), Some(BOT_ID)),
        "the fix must not break the feature: replying to a real bot message still addresses the bot"
    );
}

#[test]
fn a_reply_to_the_topic_root_service_notice_does_not_address_the_bot() {
    // The reported case. Telegram authors the topic-creation notice as the bot,
    // but the bot did not speak — it created a container.
    let topic_root = serde_json::json!({
        "message_id": TOPIC_ROOT_ID,
        "message_thread_id": TOPIC_ROOT_ID,
        "date": 1758500000,
        "chat": chat(),
        "from": bot(),
        "forum_topic_created": {"name": "RedeVest", "icon_color": 7322096},
    });
    assert!(
        !replied_to_bot_as_interlocutor(&member_reply(topic_root), Some(BOT_ID)),
        "#527: the topic root is a service notice, not the bot speaking"
    );
}

#[test]
fn any_bot_authored_service_notice_is_not_the_bot_speaking() {
    // Guards the SHAPE of the rule, not just the one reported kind: because the
    // test is on the message KIND, every service variant is covered by
    // construction and a kind Telegram adds later cannot slip back through.
    let notice = serde_json::json!({
        "message_id": 7929,
        "message_thread_id": TOPIC_ROOT_ID,
        "date": 1758635761,
        "chat": chat(),
        "from": bot(),
        "video_chat_started": {},
    });
    assert!(
        !replied_to_bot_as_interlocutor(&member_reply(notice), Some(BOT_ID)),
        "a bot-authored service notice of any kind is not the bot speaking"
    );
}

#[test]
fn a_reply_to_another_member_does_not_address_the_bot() {
    let theirs = serde_json::json!({
        "message_id": 7926,
        "message_thread_id": TOPIC_ROOT_ID,
        "date": 1758635750,
        "chat": chat(),
        "from": member(),
        "text": "unrelated",
    });
    assert!(!replied_to_bot_as_interlocutor(
        &member_reply(theirs),
        Some(BOT_ID)
    ));
}

#[test]
fn a_message_with_no_reply_does_not_address_the_bot() {
    let m = parse(serde_json::json!({
        "update_id": 2,
        "message": {
            "message_id": 7930,
            "message_thread_id": TOPIC_ROOT_ID,
            "date": 1758635763,
            "chat": chat(),
            "from": member(),
            "text": "no reply at all",
        }
    }));
    assert!(!replied_to_bot_as_interlocutor(&m, Some(BOT_ID)));
}
