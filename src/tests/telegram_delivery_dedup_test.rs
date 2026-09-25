//! A rich send whose content has already landed in the destination must not
//! put a second copy of that content on the wire (#500).
//!
//! One turn's final text reached its topic twice as two live, byte-identical
//! messages 306 ms apart: the resume tail abandoned an edit loop that was
//! already inside its POST, so the loop's message landed (msg 11897) and the
//! tail's landed after it (msg 11898).
//!
//! The guard records only a send that actually landed — a fingerprint taken at
//! attempt time would let a failed send convince its own retry that it was a
//! duplicate. But recording only on landing is not sufficient on its own: the
//! first shipped version checked, then sent, then recorded, and because the
//! pacing wait sits *inside* the send, two senders could both pass the check
//! before either recorded. So admission is a **reservation** taken under the
//! lock, and a second sender is told `InFlight` rather than being allowed to
//! send. The test named
//! `a_second_sender_is_not_admitted_while_the_first_is_in_flight` is the one
//! that pins that; its predecessor asserted the opposite and was wrong.
//!
//! Every test uses its own chat id, so the process-wide map cannot carry state
//! between them.

use crate::channels::telegram::delivery_dedup::{self, Admission};
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
        matches!(
            delivery_dedup::reserve(s, chat, thread, text, Instant::now()),
            Admission::Send
        ),
        "nothing has been sent to this destination yet"
    );

    delivery_dedup::mark_landed(s, chat, thread, text, 4242, Instant::now());

    assert!(
        matches!(
            delivery_dedup::reserve(
                s,
                chat,
                thread,
                text,
                Instant::now() + Duration::from_millis(306)
            ),
            Admission::Landed(4242)
        ),
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
    delivery_dedup::mark_landed(s, chat, thread, text, 4243, stale);

    assert!(
        matches!(
            delivery_dedup::reserve(s, chat, thread, text, Instant::now()),
            Admission::Send
        ),
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

    delivery_dedup::mark_landed(session(3), chat, thread, text, 4244, Instant::now());

    assert!(
        matches!(
            delivery_dedup::reserve(session(4), chat, thread, text, Instant::now()),
            Admission::Send
        ),
        "another session's identical message is a message, not a duplicate"
    );
    assert!(
        matches!(
            delivery_dedup::reserve(session(3), chat, thread, text, Instant::now()),
            Admission::Landed(4244)
        ),
        "the session that sent it still sees its own copy as the duplicate"
    );
}

#[test]
fn the_same_text_in_a_different_thread_is_not_a_duplicate() {
    let s = session(5);
    let text = "text unique to this test";

    delivery_dedup::mark_landed(s, -100503, Some(11), text, 4245, Instant::now());

    assert!(
        matches!(
            delivery_dedup::reserve(s, -100503, Some(22), text, Instant::now()),
            Admission::Send
        ),
        "a different thread is a different destination"
    );
}

#[test]
fn the_same_text_in_a_different_chat_is_not_a_duplicate() {
    let s = session(6);
    let text = "text unique to this test";

    delivery_dedup::mark_landed(s, -100504, None, text, 4246, Instant::now());

    assert!(
        matches!(
            delivery_dedup::reserve(s, -100505, None, text, Instant::now()),
            Admission::Send
        ),
        "one lane's answer says nothing about another lane's chat"
    );
}

#[test]
fn different_text_at_the_same_destination_is_not_a_duplicate() {
    let s = session(7);
    let (chat, thread) = (-100506_i64, None);

    delivery_dedup::mark_landed(s, chat, thread, "the first answer", 4247, Instant::now());

    assert!(
        matches!(
            delivery_dedup::reserve(s, chat, thread, "a different answer", Instant::now()),
            Admission::Send
        ),
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

    delivery_dedup::mark_landed(s, chat, thread, text, 7777, Instant::now());

    assert!(matches!(
        delivery_dedup::reserve(s, chat, thread, text, Instant::now()),
        Admission::Landed(7777)
    ));
}

#[test]
fn a_second_sender_is_not_admitted_while_the_first_is_in_flight() {
    // THE regression test for the shipped defect. The first version of this
    // guard checked, then sent, then recorded; because the pacing wait lives
    // inside the send (4.5 s measured at the 06:09 recurrence), both racing
    // senders passed the check and both messages went out — the guard logged
    // zero suppressions in a day in which the mechanism recurred.
    //
    // A sender that has not landed yet must therefore HOLD the destination, and
    // a peer must be told so rather than being allowed to send. The predecessor
    // of this test asserted that both senders go out, which is the bug.
    let s = session(9);
    let (chat, thread) = (-100508_i64, None);
    let text = "text unique to this test";

    assert!(
        matches!(
            delivery_dedup::reserve(s, chat, thread, text, Instant::now()),
            Admission::Send
        ),
        "the first sender takes the destination"
    );

    assert!(
        matches!(
            delivery_dedup::reserve(s, chat, thread, text, Instant::now()),
            Admission::InFlight
        ),
        "the second sender must wait for the first to land, not send a copy"
    );

    delivery_dedup::mark_landed(s, chat, thread, text, 5150, Instant::now());

    assert!(
        matches!(
            delivery_dedup::reserve(s, chat, thread, text, Instant::now()),
            Admission::Landed(5150)
        ),
        "once the first lands, the waiter is answered with its message id"
    );
}

#[test]
fn a_failed_send_does_not_suppress_its_own_retry() {
    // The retry leg in `delivery.rs` re-sends the same markdown when Telegram
    // reports NO_MEDIA_FOUND. A reservation left behind by the failed attempt
    // would make that retry wait and then be suppressed, and the message would
    // never go out at all.
    let s = session(10);
    let (chat, thread) = (-100509_i64, None);
    let text = "text unique to this test";

    assert!(matches!(
        delivery_dedup::reserve(s, chat, thread, text, Instant::now()),
        Admission::Send
    ));

    delivery_dedup::release(s, chat, thread, text);

    assert!(
        matches!(
            delivery_dedup::reserve(s, chat, thread, text, Instant::now()),
            Admission::Send
        ),
        "a send that did not land must not block its own retry"
    );
}
