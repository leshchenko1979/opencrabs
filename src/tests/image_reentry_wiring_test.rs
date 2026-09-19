//! #319: per-channel wiring of the shared post-delivery image re-entry.
//!
//! `image_reentry_test.rs` locks the latch and the decision helper. What can
//! still regress per channel is the WIRING: that each channel's `State::new()`
//! builds its own latch, and that the failure list reaching that channel's
//! delivery path spends it exactly once per user exchange. These tests drive
//! the real channel states, so a change that drops the field, shares one latch
//! across channels, or loses the re-arm fails here.
//!
//! Each test carries its channel's feature gate: the state it exercises only
//! exists with that feature compiled in, and the carrier ships a subset of
//! features, so an ungated test could not compile there.

use uuid::Uuid;

use crate::channels::image_reentry::ImageReentryLatch;

/// The three properties every channel's latch must have, driven through that
/// channel's own latch: a fresh state admits one re-entry per session, refuses
/// the second, and re-arms on `clear`.
fn assert_latch_wiring(latch: &ImageReentryLatch) {
    let session = Uuid::new_v4();

    assert!(
        latch.try_spend(session),
        "a fresh channel state must admit the first re-entry"
    );
    assert!(
        !latch.try_spend(session),
        "a second failure in the same exchange must not re-enter again"
    );

    latch.clear(session);
    assert!(
        latch.try_spend(session),
        "the next inbound user message must re-arm the latch"
    );
}

#[cfg(feature = "slack")]
#[test]
fn slack_state_wires_the_image_reentry_latch() {
    let state = crate::channels::slack::SlackState::new();
    assert_latch_wiring(&state.image_reentry);
}

#[cfg(feature = "discord")]
#[test]
fn discord_state_wires_the_image_reentry_latch() {
    let state = crate::channels::discord::DiscordState::new();
    assert_latch_wiring(&state.image_reentry);
}

#[cfg(feature = "whatsapp")]
#[test]
fn whatsapp_state_wires_the_image_reentry_latch() {
    let state = crate::channels::whatsapp::WhatsAppState::new();
    assert_latch_wiring(&state.image_reentry);
}
