//! Remember what was last sent as a turn's rich message, so an identical
//! second send to the same destination is not delivered as a second message
//! (#500).
//!
//! One turn's final text reached its topic twice as two live, byte-identical
//! messages 306 ms apart (2026-09-21 18:44:07.987 and 18:44:08.293, chat
//! -1003993000918 thread 10780, both `len=3598 hash8=daa16ddb`). The turn's
//! edit loop had been abandoned: the resume tail cancels it, waits
//! `TAIL_AWAIT_BUDGET` (5 s) for it to join, and on expiry continues anyway
//! ("edit-loop join exceeded 5s; continuing"). Cancellation is cooperative, so
//! it cannot abort an `await` that is already inside a POST — the abandoned
//! loop's send landed as msg 11897 while the tail, whose `sent_intermediates`
//! snapshot had been read 3.3 s earlier (count=0), sent the same text again as
//! msg 11898. Ten such pairs in one day of ordinary traffic.
//!
//! The guard sits at [`send_rich_turn_guarded`], the one path both senders
//! share — the turn's final delivery (`delivery.rs`) and the intermediate
//! sender (`intermediates.rs`) — so neither can bypass the other. It is
//! deliberately NOT wired into `post_rich`, even though that is the single
//! point every rich call passes through: the flow block (`flow.rs`, the
//! rich-HTML plane) and the plan card (`plan_card.rs`, the markdown-media
//! plane) re-post themselves with `origin = "turn"` too, and they delete and
//! re-create *identical* content by design, so a guard there would answer with
//! the id of the message it had just deleted and take a live block off the
//! screen. It is not wired into `send_rich_with_mermaid_target_id` either:
//! the tool outbox (`send.rs`) calls that form directly, and a tool asked twice
//! for the same message must send it twice.
//!
//! Two properties are load-bearing:
//!
//! * A fingerprint is recorded only once a send has actually LANDED. Recording
//!   at attempt time would let a failed send convince its own retry (the
//!   `RICH_MESSAGE_PHOTO_NO_MEDIA_FOUND` retry in `delivery.rs`) that it was a
//!   duplicate, and the message would never go out at all.
//! * A suppressed send answers with the id the content already landed at
//!   rather than an error. An error would read to the caller as a failed send
//!   and drive the HTML fallback — which would deliver the duplicate this
//!   exists to prevent.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use teloxide::types::{ChatId, ThreadId};
use uuid::Uuid;

/// Window within which an identical send to the same destination is treated as
/// a duplicate. The widest separation measured between the members of a
/// duplicated pair is 4.954 s (2026-09-21, 2 277 rich sends scanned); a
/// deliberate repeat of the same text in one topic is minutes apart at the
/// closest.
pub(crate) const TTL: Duration = Duration::from_secs(60);

/// Bound on remembered sends. Far above the number of distinct messages in
/// flight across a busy box, and cheap to rebuild: a miss costs one duplicate,
/// which is exactly the pre-fix behaviour for that one call.
const MAX_TRACKED: usize = 256;

/// The session is part of the key because two lanes can legitimately post the
/// same text to one topic — one topic is not one writer.
type Key = (Uuid, i64, Option<i32>, u64);

fn tracked() -> &'static Mutex<HashMap<Key, (Instant, i32)>> {
    static TRACKED: OnceLock<Mutex<HashMap<Key, (Instant, i32)>>> = OnceLock::new();
    TRACKED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Fingerprint of the content a rich send carries — what the recipient reads.
fn fingerprint(markdown: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    markdown.hash(&mut hasher);
    hasher.finish()
}

/// The message id an identical send already landed at inside the window, or
/// `None` when this send is fresh and should go out.
///
/// Expired entries are pruned on every call, so the map cannot grow without
/// bound even when traffic runs one way.
pub(crate) fn claim(
    session_id: Uuid,
    chat_id: i64,
    thread_id: Option<i32>,
    markdown: &str,
    now: Instant,
) -> Option<i32> {
    let mut map = tracked().lock().unwrap_or_else(|e| e.into_inner());
    map.retain(|_, (at, _)| now.saturating_duration_since(*at) < TTL);
    map.get(&(session_id, chat_id, thread_id, fingerprint(markdown)))
        .map(|(_, id)| *id)
}

/// Record that a send with this content landed as `message_id`.
///
/// Called only after Telegram confirmed the send, never at attempt time.
pub(crate) fn remember(
    session_id: Uuid,
    chat_id: i64,
    thread_id: Option<i32>,
    markdown: &str,
    message_id: i32,
    now: Instant,
) {
    let mut map = tracked().lock().unwrap_or_else(|e| e.into_inner());
    let key = (session_id, chat_id, thread_id, fingerprint(markdown));
    if map.len() >= MAX_TRACKED && !map.contains_key(&key) {
        // Nothing here is worth an eviction policy: dropping the lot costs one
        // duplicate per live conversation, once.
        map.clear();
    }
    map.insert(key, (now, message_id));
}

/// Send a turn's rich markdown under the duplicate guard (#500).
///
/// Both senders that can produce the duplicate route through here, so neither
/// can bypass the other. A send suppressed as a duplicate answers with the id
/// the content is already in, so the caller records a delivered message rather
/// than falling back and sending it a third time.
pub(crate) async fn send_rich_turn_guarded(
    session_id: Uuid,
    bot: &teloxide::Bot,
    chat_id: ChatId,
    thread_id: Option<ThreadId>,
    markdown: &str,
) -> anyhow::Result<i32> {
    let thread = thread_id.map(|t| t.0.0);
    let hash8 = fingerprint(markdown) as u32;
    if let Some(landed_id) = claim(session_id, chat_id.0, thread, markdown, Instant::now()) {
        tracing::info!(
            "Telegram: duplicate rich send suppressed — this content already landed as \
             msg {landed_id} (session={session_id} chat={} thread={thread:?} hash8={hash8:08x})",
            chat_id.0
        );
        return Ok(landed_id);
    }

    let id = super::rich::send_rich_with_mermaid_id(
        bot.api_url().as_str(),
        bot.token(),
        chat_id.0,
        thread_id,
        markdown,
        None,
        "turn",
        "-",
    )
    .await?;

    // A send that returned no message id did not land, so it must not suppress
    // its own retry.
    if id != 0 {
        remember(session_id, chat_id.0, thread, markdown, id, Instant::now());
    }
    Ok(id)
}
