//! A rich send whose content has already landed in the destination must not
//! put a second copy of that content on the wire (#500).
//!
//! One turn's final text reached its topic twice as two live, byte-identical
//! messages 306 ms apart: the resume tail abandoned an edit loop that was
//! already inside its POST, so the loop's message landed (msg 11897) and the
//! tail's landed after it (msg 11898). Ten such pairs in one day of ordinary
//! traffic. The guard records only a send that actually landed — a fingerprint
//! taken at attempt time would let a failed send convince its own retry that it
//! was a duplicate.
//!
//! Every test uses its own chat id, so the process-wide map cannot carry state
//! between them.

use crate::channels::telegram::delivery_dedup;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Fixed session ids: a key is `(session, chat, thread, content)`, so tests
/// that must not see each other's entries differ in the chat id, and tests
/// about session identity differ in the session id.
fn session(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

#[test]
fn an_identical_send_inside_the_window_is_the_duplicate() {
    let s = session(1);
    let (chat, thread) = (-100500_i64, None);
    let text = "text unique to this test";

    assert!(
        delivery_dedup::claim(s, chat, thread, text, Instant::now()).is_none(),
        "nothing has been sent to this destination yet"
    );

    delivery_dedup::remember(s, chat, thread, text, 4242, Instant::now());

    assert_eq!(
        delivery_dedup::claim(
            s,
            chat,
            thread,
            text,
            Instant::now() + Duration::from_millis(306)
        ),
        Some(4242),
        "306 ms is the separation the duplicated pair was measured at, and a \
         suppressed send must answer with the id the content is already in"
    );
}

#[test]
fn a_repeat_after_the_window_is_a_fresh_send() {
    let s = session(2);
    let (chat, thread) = (-100501_i64, None);
    let text = "text unique to this test";

    // Recorded in the past rather than probed in the future: a future `now`
    // would prune every other test's entry from the shared map.
    let stale = Instant::now() - delivery_dedup::TTL - Duration::from_secs(1);
    delivery_dedup::remember(s, chat, thread, text, 4243, stale);

    assert!(
        delivery_dedup::claim(s, chat, thread, text, Instant::now()).is_none(),
        "a deliberate repeat of the same text is minutes apart at the closest, \
         so it must still be delivered"
    );
}

#[test]
fn the_same_text_from_a_different_session_is_not_a_duplicate() {
    // One topic is not one writer: two lanes can legitimately post the same
    // text to it, and only the turn that sent the first copy may be suppressed.
    let (chat, thread) = (-100502_i64, None);
    let text = "text unique to this test";

    delivery_dedup::remember(session(3), chat, thread, text, 4244, Instant::now());

    assert!(
        delivery_dedup::claim(session(4), chat, thread, text, Instant::now()).is_none(),
        "another session's identical message is a message, not a duplicate"
    );
    assert_eq!(
        delivery_dedup::claim(session(3), chat, thread, text, Instant::now()),
        Some(4244),
        "the session that sent it still sees its own copy as the duplicate"
    );
}

#[test]
fn the_same_text_in_a_different_thread_is_not_a_duplicate() {
    let s = session(5);
    let text = "text unique to this test";

    delivery_dedup::remember(s, -100503, Some(11), text, 4245, Instant::now());

    assert!(
        delivery_dedup::claim(s, -100503, Some(22), text, Instant::now()).is_none(),
        "a different thread is a different destination"
    );
}

#[test]
fn the_same_text_in_a_different_chat_is_not_a_duplicate() {
    let s = session(6);
    let text = "text unique to this test";

    delivery_dedup::remember(s, -100504, None, text, 4246, Instant::now());

    assert!(
        delivery_dedup::claim(s, -100505, None, text, Instant::now()).is_none(),
        "one lane's answer says nothing about another lane's chat"
    );
}

#[test]
fn different_text_at_the_same_destination_is_not_a_duplicate() {
    let s = session(7);
    let (chat, thread) = (-100506_i64, None);

    delivery_dedup::remember(s, chat, thread, "the first answer", 4247, Instant::now());

    assert!(
        delivery_dedup::claim(s, chat, thread, "a different answer", Instant::now()).is_none(),
        "only identical content is the duplicate"
    );
}

#[test]
fn a_suppressed_send_reports_the_id_the_content_already_landed_at() {
    // The whole point of returning an id rather than an error: the caller
    // records a delivered message instead of falling back to the HTML path and
    // delivering the duplicate this exists to prevent.
    let s = session(8);
    let (chat, thread) = (-100507_i64, None);
    let text = "text unique to this test";

    delivery_dedup::remember(s, chat, thread, text, 7777, Instant::now());

    assert_eq!(
        delivery_dedup::claim(s, chat, thread, text, Instant::now()),
        Some(7777)
    );
}

#[test]
fn two_senders_that_both_claim_before_either_lands_both_go_out() {
    // The boundary, stated rather than left implied. The guard suppresses a
    // send whose content has ALREADY landed; it records only on success. So if
    // both racing senders claim before either POST returns, neither is
    // suppressed and both messages still go out. The fix closes the window the
    // incident was measured in (the second send came 3.3 s after the first
    // landed) — it does not make two simultaneous in-flight POSTs atomic, and
    // pretending otherwise would be the kind of probe that cannot fail.
    let s = session(9);
    let (chat, thread) = (-100508_i64, None);
    let text = "text unique to this test";

    assert!(delivery_dedup::claim(s, chat, thread, text, Instant::now()).is_none());
    assert!(
        delivery_dedup::claim(s, chat, thread, text, Instant::now()).is_none(),
        "nothing has landed yet, so neither sender is suppressed"
    );
}
