//! #319: the shared post-delivery image re-entry (Slack / Discord / WhatsApp).
//!
//! The latch bounds the correction turn to ONE per user exchange, and the
//! decision helper is what each channel's delivery path calls after it has told
//! the user about an image it could not send. These lock the spend/refuse
//! semantics and the payload handed to the channel's enqueue path.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use uuid::Uuid;

use crate::brain::agent::QueuedUserMessage;
use crate::channels::image_reentry::{enqueue_image_reentry, ImageReentryLatch};
use crate::utils::image::{LocalImageFailure, LocalImageFailureReason};

/// A failed image send the way a channel handler reports it: the reference as
/// written in the reply, plus the reason the channel refused it.
fn delivery_failure(raw: &str) -> LocalImageFailure {
    LocalImageFailure {
        raw: raw.to_string(),
        resolved: None,
        reason: LocalImageFailureReason::DeliveryFailed,
    }
}

/// Records every message handed to the fake dispatch, and answers `true` — the
/// wired-callback case.
#[derive(Default)]
struct FakeDispatch {
    seen: Mutex<Vec<QueuedUserMessage>>,
}

impl FakeDispatch {
    fn take(&self) -> Vec<QueuedUserMessage> {
        std::mem::take(&mut *self.seen.lock().unwrap())
    }
}

#[test]
fn latch_admits_one_reentry_per_exchange() {
    let latch = ImageReentryLatch::new();
    let session = Uuid::new_v4();

    assert!(
        latch.try_spend(session),
        "the first failure claims the re-entry"
    );
    assert!(
        !latch.try_spend(session),
        "a second failure in the same exchange must not re-enter again"
    );
}

#[test]
fn clear_re_arms_the_next_exchange() {
    let latch = ImageReentryLatch::new();
    let session = Uuid::new_v4();

    assert!(latch.try_spend(session));
    latch.clear(session);
    assert!(
        latch.try_spend(session),
        "the next inbound user message re-arms the latch"
    );
}

#[test]
fn latch_is_per_session() {
    let latch = ImageReentryLatch::new();
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());

    assert!(latch.try_spend(a));
    assert!(
        latch.try_spend(b),
        "one session's budget must not spend another's"
    );
    assert!(!latch.try_spend(a));
    assert!(!latch.try_spend(b));
}

#[test]
fn clearing_an_untouched_session_is_harmless() {
    let latch = ImageReentryLatch::new();
    let session = Uuid::new_v4();

    latch.clear(session);
    assert!(latch.try_spend(session));
}

#[test]
fn no_failures_means_no_reentry_and_no_spend() {
    let latch = ImageReentryLatch::new();
    let session = Uuid::new_v4();
    let dispatched = Arc::new(AtomicBool::new(false));
    let flag = dispatched.clone();

    let enqueued = enqueue_image_reentry(&latch, session, &[], move |_msg| {
        flag.store(true, Ordering::SeqCst);
        true
    });

    assert!(!enqueued, "a clean turn dispatches nothing");
    assert!(
        !dispatched.load(Ordering::SeqCst),
        "dispatch must not be called"
    );
    assert!(
        latch.try_spend(session),
        "a clean turn must not burn the exchange's one re-entry"
    );
}

#[test]
fn a_spent_latch_refuses_a_second_failure_in_the_same_exchange() {
    let latch = ImageReentryLatch::new();
    let session = Uuid::new_v4();
    let failures = [delivery_failure("chart.png")];
    let fake = Arc::new(FakeDispatch::default());
    let first = fake.clone();

    assert!(enqueue_image_reentry(
        &latch,
        session,
        &failures,
        move |msg| {
            first.seen.lock().unwrap().push(msg);
            true
        }
    ));
    assert!(
        !enqueue_image_reentry(&latch, session, &failures, |msg| {
            fake.seen.lock().unwrap().push(msg);
            true
        }),
        "the exchange already spent its one re-entry"
    );
    assert_eq!(
        fake.take().len(),
        1,
        "exactly one correction turn per exchange"
    );
}

#[test]
fn reentry_payload_carries_the_nudge_with_path_and_reason() {
    let latch = ImageReentryLatch::new();
    let session = Uuid::new_v4();
    let failures = [delivery_failure("/tmp/chart.png")];
    let fake = Arc::new(FakeDispatch::default());
    let sink = fake.clone();

    assert!(enqueue_image_reentry(
        &latch,
        session,
        &failures,
        move |msg| {
            sink.seen.lock().unwrap().push(msg);
            true
        }
    ));

    let seen = fake.take();
    assert_eq!(seen.len(), 1);
    let msg = &seen[0];

    // The model is told which image failed and why ...
    assert!(
        msg.context_text.contains("/tmp/chart.png"),
        "context must name the failed path: {}",
        msg.context_text
    );
    assert!(
        msg.context_text
            .contains("the channel could not deliver the image"),
        "context must carry the reason: {}",
        msg.context_text
    );
    // ... and told not to re-emit a reference that was never the problem.
    assert!(msg.context_text.contains("do not re-emit it"));
    // History and the UI show the compact tag, not the scaffolding (#722).
    assert_ne!(msg.context_text, msg.display_text);
    assert!(
        msg.display_text
            .contains("1 image attachment(s) could not be delivered"),
        "display must count the failures: {}",
        msg.display_text
    );
    assert!(msg.display_text.contains("asking the model to report them"));
}

#[test]
fn a_failed_dispatch_still_spends_the_exchange() {
    let latch = ImageReentryLatch::new();
    let session = Uuid::new_v4();
    let failures = [delivery_failure("chart.png")];

    // An unwired surface: `enqueue_session_message` answers false.
    assert!(enqueue_image_reentry(
        &latch,
        session,
        &failures,
        |_msg| false
    ));
    assert!(
        !latch.try_spend(session),
        "the latch is spent before the dispatch, so a failed dispatch cannot chain turns"
    );
}
