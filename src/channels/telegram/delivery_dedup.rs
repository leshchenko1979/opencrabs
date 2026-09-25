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
//! msg 11898.
//!
//! ## Why admission is a reservation, not a check-then-record
//!
//! The first version of this guard called `claim` (lock, test, unlock), then
//! sent, then `remember`ed (lock, insert). That does not close the race, and
//! the box measured it failing: shipped as `2d1a32f64`, it logged **zero**
//! suppressions in a day in which the mechanism recurred, and the per-join-event
//! duplicate rate was unchanged (6/147 = 4.1 % before, 1/27 = 3.7 % after).
//!
//! The reason is where the gap sits. `pace_rich` runs *inside*
//! `send_rich_with_mermaid_id` (`rich/api.rs:334`), so the distance between the
//! check and the record is the whole pacing wait plus the POST — measured at
//! **4.5 s** in the 06:09 recurrence (verdict 06:09:42.036, sends 06:09:46.584
//! and .638, both `len=2446 hash8=a1fa0f8e`). Two senders claiming within that
//! window both see "nothing landed yet" and both send. A check separated from
//! its record by an `await` is not a guard.
//!
//! So [`reserve`] *inserts* a pending slot under the lock and admits only the
//! caller that inserted it. A second sender sees [`Admission::InFlight`] and
//! waits for the peer's message id instead of sending. [`mark_landed`] fills in
//! the real id once Telegram confirms; [`release`] drops the reservation when
//! the send failed, so a genuine retry (the
//! `RICH_MESSAGE_PHOTO_NO_MEDIA_FOUND` retry in `delivery.rs`) is never
//! suppressed by its own failed attempt.
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
//! A suppressed send answers with the id the content already landed at rather
//! than an error. An error would read to the caller as a failed send and drive
//! the HTML fallback — which would deliver the duplicate this exists to prevent.

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

/// How long a sender that finds a peer already in flight waits for that peer's
/// message id before giving up and sending anyway. Covers the pacing wait plus
/// a POST (4.5 s + ~0.1 s measured) with a wide margin.
const WAIT_MAX: Duration = Duration::from_secs(20);

/// Polling step while waiting on an in-flight peer.
const WAIT_STEP: Duration = Duration::from_millis(100);

/// Bound on remembered sends. Far above the number of distinct messages in
/// flight across a busy box, and cheap to rebuild: a miss costs one duplicate,
/// which is exactly the pre-fix behaviour for that one call.
const MAX_TRACKED: usize = 256;

/// The session is part of the key because two lanes can legitimately post the
/// same text to one topic — one topic is not one writer.
type Key = (Uuid, i64, Option<i32>, u64);

/// What is known about a destination's content: either a sender owns it right
/// now, or it landed at a known message id.
enum Slot {
    Pending(Instant),
    Landed(Instant, i32),
}

impl Slot {
    fn at(&self) -> Instant {
        match self {
            Slot::Pending(t) => *t,
            Slot::Landed(t, _) => *t,
        }
    }
}

/// Verdict on a reservation attempt.
pub(crate) enum Admission {
    /// The caller inserted the reservation and owns the send.
    Send,
    /// This content already landed; answer with that message id.
    Landed(i32),
    /// Another sender holds the reservation and has not landed yet.
    InFlight,
}

fn tracked() -> &'static Mutex<HashMap<Key, Slot>> {
    static TRACKED: OnceLock<Mutex<HashMap<Key, Slot>>> = OnceLock::new();
    TRACKED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Fingerprint of the content a rich send carries — what the recipient reads.
fn fingerprint(markdown: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    markdown.hash(&mut hasher);
    hasher.finish()
}

/// The guard's content key: the markdown plus, when the send carries a media
/// array, each entry's id and byte length (#502).
///
/// Two promoted intermediates can carry IDENTICAL text — `![chart](tg://photo
/// ?id=img0)` is the same string whatever picture `img0` resolves to — and the
/// text alone would make the second one a "duplicate" whose media silently
/// differs from the first. With no media the key is the markdown verbatim, so
/// every existing caller and test sees the fingerprint it sees today.
pub(crate) fn content_key(markdown: &str, media: &[super::rich::mermaid::MediaEntry]) -> String {
    if media.is_empty() {
        return markdown.to_string();
    }
    let mut key = String::with_capacity(markdown.len() + media.len() * 16);
    key.push_str(markdown);
    for m in media {
        key.push('\0');
        key.push_str(&m.id);
        key.push(':');
        key.push_str(&m.bytes.as_ref().map_or(0, Vec::len).to_string());
    }
    key
}

fn key_of(session_id: Uuid, chat_id: i64, thread_id: Option<i32>, markdown: &str) -> Key {
    (session_id, chat_id, thread_id, fingerprint(markdown))
}

/// Try to take the reservation for this content, or report who already holds it.
///
/// Expired entries are pruned on every call, so the map cannot grow without
/// bound even when traffic runs one way.
pub(crate) fn reserve(
    session_id: Uuid,
    chat_id: i64,
    thread_id: Option<i32>,
    markdown: &str,
    now: Instant,
) -> Admission {
    let mut map = tracked().lock().unwrap_or_else(|e| e.into_inner());
    map.retain(|_, slot| now.saturating_duration_since(slot.at()) < TTL);
    let key = key_of(session_id, chat_id, thread_id, markdown);
    match map.get(&key) {
        Some(Slot::Landed(_, id)) => Admission::Landed(*id),
        Some(Slot::Pending(_)) => Admission::InFlight,
        None => {
            if map.len() >= MAX_TRACKED {
                // Nothing here is worth an eviction policy: dropping the lot
                // costs one duplicate per live conversation, once.
                map.clear();
            }
            map.insert(key, Slot::Pending(now));
            Admission::Send
        }
    }
}

/// Record that the reserved send landed as `message_id`.
///
/// Called only after Telegram confirmed the send, never at attempt time.
pub(crate) fn mark_landed(
    session_id: Uuid,
    chat_id: i64,
    thread_id: Option<i32>,
    markdown: &str,
    message_id: i32,
    now: Instant,
) {
    let mut map = tracked().lock().unwrap_or_else(|e| e.into_inner());
    let key = key_of(session_id, chat_id, thread_id, markdown);
    if map.len() >= MAX_TRACKED && !map.contains_key(&key) {
        map.clear();
    }
    map.insert(key, Slot::Landed(now, message_id));
}

/// Drop the reservation after a send that did not land, so a genuine retry of
/// this content is not suppressed by its own failed attempt.
pub(crate) fn release(session_id: Uuid, chat_id: i64, thread_id: Option<i32>, markdown: &str) {
    let mut map = tracked().lock().unwrap_or_else(|e| e.into_inner());
    map.remove(&key_of(session_id, chat_id, thread_id, markdown));
}

/// Send a turn's rich markdown under the duplicate guard (#500).
///
/// Both senders that can produce the duplicate route through here, so neither
/// can bypass the other. A send suppressed as a duplicate answers with the id
/// the content is already in, so the caller records a delivered message rather
/// than falling back and sending it a third time.
///
/// `media` is the resolved local-media array the rich send carries (#502);
/// every existing caller passes `&[]` and gets today's behaviour byte-for-byte.
/// The guard keys on [`content_key`] — the markdown plus the media identity —
/// so two sends with the same text but different pictures are NOT duplicates
/// of each other.
pub(crate) async fn send_rich_turn_guarded(
    session_id: Uuid,
    bot: &teloxide::Bot,
    chat_id: ChatId,
    thread_id: Option<ThreadId>,
    markdown: &str,
    media: &[super::rich::mermaid::MediaEntry],
) -> anyhow::Result<i32> {
    let thread = thread_id.map(|t| t.0.0);
    let key = content_key(markdown, media);
    let hash8 = fingerprint(&key) as u32;

    let mut waited = Duration::ZERO;
    let owned = loop {
        match reserve(session_id, chat_id.0, thread, &key, Instant::now()) {
            Admission::Landed(id) => {
                tracing::info!(
                    "Telegram: duplicate rich send suppressed — this content already landed as \
                     msg {id} (session={session_id} chat={} thread={thread:?} hash8={hash8:08x})",
                    chat_id.0
                );
                return Ok(id);
            }
            Admission::InFlight => {
                if waited >= WAIT_MAX {
                    // The peer reserved but never landed. Sending anyway is the
                    // pre-fix behaviour for this one call; dropping the content
                    // would be worse.
                    tracing::warn!(
                        "Telegram: rich send waited {WAIT_MAX:?} on an in-flight duplicate that \
                         never landed — sending anyway (session={session_id} chat={} \
                         thread={thread:?} hash8={hash8:08x})",
                        chat_id.0
                    );
                    break false;
                }
                tokio::time::sleep(WAIT_STEP).await;
                waited += WAIT_STEP;
            }
            Admission::Send => break true,
        }
    };

    let sent = super::rich::send_rich_with_media_target_id(
        bot.api_url().as_str(),
        bot.token(),
        chat_id.0,
        thread_id,
        None,
        markdown,
        media,
        "turn",
        "-",
    )
    .await;

    match sent {
        Ok(id) if id != 0 => {
            mark_landed(session_id, chat_id.0, thread, &key, id, Instant::now());
            Ok(id)
        }
        // A send that returned no message id did not land, so it must not
        // suppress its own retry.
        Ok(id) => {
            if owned {
                release(session_id, chat_id.0, thread, &key);
            }
            Ok(id)
        }
        Err(e) => {
            if owned {
                release(session_id, chat_id.0, thread, &key);
            }
            Err(e)
        }
    }
}
