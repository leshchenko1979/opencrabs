//! #124 — topic-scoped tmp media pickup (isolation leak fix).
//!
//! The tmp photo/voice pickup scan keyed on chat id only; in a forum chat
//! with many topics/sessions a photo dropped in one topic was injected as
//! `<<IMG:>>` into every other topic's mention turns for the 300s window
//! (cross-session data exposure). The fix scopes saves + scans on the
//! forum topic and adds consume-once semantics via `.injected` sentinels.

use crate::channels::telegram::media::{split_topic_from_tmp_body, tmp_file_injected};

#[test]
fn topic_scoped_body_parses_topic_and_ts() {
    let (topic, ts_body) = split_topic_from_tmp_body("t30134-1788774443.jpg");
    assert_eq!(topic, Some(30134));
    assert_eq!(ts_body, "1788774443.jpg");
}

#[test]
fn legacy_body_parses_as_no_topic() {
    // Pre-#124 filenames carry no topic segment — these must stay visible
    // ONLY to topic-less (None) scans, never to a real topic's scan.
    let (topic, ts_body) = split_topic_from_tmp_body("1788774443.jpg");
    assert_eq!(topic, None);
    assert_eq!(ts_body, "1788774443.jpg");
}

#[test]
fn topic_0_is_a_real_scope_not_legacy() {
    // A file saved with topic Some(0) is t0-scoped; a None scan must not
    // match it (and vice versa) — the Option discriminates, not the value.
    let (topic, _) = split_topic_from_tmp_body("t0-1788774443.jpg");
    assert_eq!(topic, Some(0));
}

#[test]
fn garbage_body_parses_as_no_topic() {
    // Non-numeric topic segments degrade to legacy form rather than panic.
    let (topic, ts_body) = split_topic_from_tmp_body("txx-1788774443.jpg");
    assert_eq!(topic, None);
    assert_eq!(ts_body, "txx-1788774443.jpg");
}

#[test]
fn injected_sentinel_is_detected() {
    let dir = std::env::temp_dir().join(format!(
        "oc124-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_millis()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let photo = dir.join("photo-123-t5-100.jpg");
    std::fs::write(&photo, b"x").expect("write photo");
    assert!(
        !tmp_file_injected(&photo),
        "unpicked photo must not read as injected"
    );

    std::fs::write(dir.join("photo-123-t5-100.jpg.injected"), b"").expect("write sentinel");
    assert!(
        tmp_file_injected(&photo),
        "sentinel presence must mark the photo injected"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
