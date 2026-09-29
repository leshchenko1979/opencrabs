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
//! Both halves discriminate, and they fail on DIFFERENT trees — that is what
//! makes this file a regression guard rather than a passing suite:
//!
//! * **Negative half** — the two `!asserts` on service notices FAIL on the
//!   **pre-`#527` tree**: an unguarded sender-id comparison returns `true` for
//!   a reply to the topic root.
//! * **Positive half** — the rich-message and dice cases FAIL on the **`#527`
//!   tree** (`457ff4ffc`): its `matches!(kind, MessageKind::Common(_))` test
//!   returns `false` for a rich-rendered bot message, which is the `#661`
//!   regression. They pass again once the kind test is inverted.
//!
//! So neither direction can be satisfied by reverting the other: the fix must
//! accept content the typed parse cannot name (#661) WITHOUT re-accepting
//! service notices (#527).

use crate::channels::telegram::handler::replied_to_bot_as_interlocutor;
use teloxide::types::{Message, MessageKind, Update, UpdateKind};

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

/// `#661`: the bot's reply went out via `sendRichMessage`, which is Bot API 10.1
/// and has NO teloxide 0.17 binding — the daemon calls it over raw HTTP and
/// Telegram normalises the source into `rich_message.blocks` server-side. So the
/// wire message carries `rich_message` and no `text`/media key.
///
/// That payload matches no `MediaKind` variant (each demands its own key), so
/// `MessageKind::Common` fails to deserialize and the untagged enum settles on
/// its LAST variant, `Empty {}`. `#527` rejected `Empty` outright, so every
/// reply to a rich-rendered bot message read as "not directed at the bot" and
/// the reply was silently dropped.
#[test]
fn a_reply_to_a_rich_rendered_bot_message_addresses_the_bot() {
    // Official Bot API rich shape: `rich_message.blocks`, and no `text` field.
    let outer = member_reply(serde_json::json!({
        "message_id": 7931,
        "message_thread_id": TOPIC_ROOT_ID,
        "date": 1758635770,
        "chat": chat(),
        "from": bot(),
        "rich_message": { "blocks": [
            { "type": "paragraph", "text": "Deploy finished: all green" }
        ]},
    }));

    // Pin the MECHANISM, not merely the outcome. If a future teloxide gains a
    // binding for rich messages the payload lands elsewhere and this assertion
    // says so, rather than letting the predicate pass for an unknown reason.
    let landed = outer.reply_to_message().expect("reply target is present");
    assert!(
        matches!(&landed.kind, MessageKind::Empty {}),
        "a rich payload must land on Empty {{}} — no MediaKind variant accepts \
         `rich_message`, so the untagged parse falls through Common; landed on {:?}",
        landed.kind
    );

    assert!(
        replied_to_bot_as_interlocutor(&outer, Some(BOT_ID)),
        "#661: a reply to a rich-rendered bot message addresses the bot; the \
         #527 tree returns false here, which is the regression"
    );
}

/// The wider class `#661` closes: `Dice`, `Invoice`, `SuccessfulPayment` and
/// `PassportData` are NAMED variants, not `Common`, so the `#527` test rejected
/// them too. No `sendDice` sender exists in this tree yet, so the case is
/// unreachable live today — it is pinned because the predicate must classify by
/// what a message IS, not by whether the typed parse happened to name it.
#[test]
fn a_reply_to_a_bot_dice_message_addresses_the_bot() {
    let outer = member_reply(serde_json::json!({
        "message_id": 7933,
        "message_thread_id": TOPIC_ROOT_ID,
        "date": 1758635772,
        "chat": chat(),
        "from": bot(),
        "dice": {"emoji": "🎲", "value": 4},
    }));

    // The named variant, unlike the rich payload, IS representable — so this
    // case pins the other half of the class rather than repeating the first.
    let landed = outer.reply_to_message().expect("reply target is present");
    assert!(
        matches!(&landed.kind, MessageKind::Dice(_)),
        "a `dice` payload must land on MessageKind::Dice, not Empty; landed on {:?}",
        landed.kind
    );

    assert!(
        replied_to_bot_as_interlocutor(&outer, Some(BOT_ID)),
        "a bot-authored named content variant is the bot speaking, not a service notice"
    );
}
