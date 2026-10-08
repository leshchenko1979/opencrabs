//! Delivery-layer tests for image references (#286).
//!
//! The extraction/fetch layer has its own battery (`image_util_test.rs`); this
//! file covers the pieces the CHANNEL delivery sites depend on:
//!
//! - `telegram_media_kind` — the photo/document routing decision, which is a
//!   property of the byte length and not of the file.
//! - `failure_notice` — the text appended to a reply whose image could not be
//!   attached, so a missing picture is never silent.
//! - `strip_image_references` — the strip-only variant: a remote link SURVIVES
//!   in the text (nothing at that call site would fetch it), while a local
//!   marker/path still leaves.

use crate::utils::image::{
    LocalImageFailure, LocalImageFailureReason, LocalImageScan, append_failure_notice,
    extract_local_images, failure_notice, strip_image_references,
};
use std::path::{Path, PathBuf};

/// Minimal byte string that passes the magic-byte sniff for PNG.
const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x01\x02\x03\x04\x05\x06\x07";

const REMOTE_PNG: &str = "https://example.invalid/chart.png";

fn write_fixture(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

/// The resolved paths of a scan, in order — what the assertions below compare
/// on. Captions ride alongside each path in [`LocalImageScan::attachments`].
fn resolved_paths(scan: &LocalImageScan) -> Vec<PathBuf> {
    scan.attachments.iter().map(|a| a.path.clone()).collect()
}

// ---------------------------------------------------------------------------
// telegram_media_kind — the 10 MB photo ceiling
// ---------------------------------------------------------------------------

#[cfg(feature = "telegram")]
mod telegram_media_kind {
    use crate::channels::telegram::send::{
        TELEGRAM_PHOTO_MAX_BYTES, TelegramMediaKind, telegram_media_kind,
    };

    #[test]
    fn photo_ceiling_is_ten_megabytes() {
        assert_eq!(TELEGRAM_PHOTO_MAX_BYTES, 10 * 1024 * 1024);
    }

    #[test]
    fn a_small_image_is_a_photo() {
        assert_eq!(telegram_media_kind(1024), TelegramMediaKind::Photo);
    }

    #[test]
    fn exactly_the_ceiling_is_still_a_photo() {
        // `sendPhoto` accepts the ceiling itself; only MORE than it is refused.
        assert_eq!(
            telegram_media_kind(TELEGRAM_PHOTO_MAX_BYTES),
            TelegramMediaKind::Photo
        );
    }

    #[test]
    fn one_byte_over_the_ceiling_is_a_document() {
        assert_eq!(
            telegram_media_kind(TELEGRAM_PHOTO_MAX_BYTES + 1),
            TelegramMediaKind::Document
        );
    }

    #[test]
    fn a_large_image_is_a_document() {
        assert_eq!(
            telegram_media_kind(50 * 1024 * 1024),
            TelegramMediaKind::Document
        );
    }

    #[test]
    fn an_empty_file_is_a_photo() {
        // Degenerate but well-defined: the routing is a size test, and a
        // zero-length payload is under the ceiling.
        assert_eq!(telegram_media_kind(0), TelegramMediaKind::Photo);
    }
}

// ---------------------------------------------------------------------------
// failure_notice — the appended "image not attached" text
// ---------------------------------------------------------------------------

#[test]
fn no_failures_produces_no_notice() {
    assert_eq!(failure_notice(&[]), None);
}

#[test]
fn one_failure_names_the_reference_and_the_reason() {
    let failures = vec![LocalImageFailure {
        raw: "chart.png".to_string(),
        resolved: Some(PathBuf::from("/tmp/chart.png")),
        reason: LocalImageFailureReason::NotFound,
    }];
    let notice = failure_notice(&failures).expect("a notice for one failure");
    assert!(
        notice.contains("chart.png"),
        "the notice must quote the reference: {notice}"
    );
    assert!(
        notice.contains("file not found"),
        "the notice must carry the reason: {notice}"
    );
}

#[test]
fn every_failure_gets_its_own_bullet() {
    let failures = vec![
        LocalImageFailure {
            raw: "a.png".to_string(),
            resolved: None,
            reason: LocalImageFailureReason::NotFound,
        },
        LocalImageFailure {
            raw: "b.png".to_string(),
            resolved: None,
            reason: LocalImageFailureReason::Empty,
        },
        LocalImageFailure {
            raw: "c.png".to_string(),
            resolved: None,
            reason: LocalImageFailureReason::UnsupportedFormat,
        },
    ];
    let notice = failure_notice(&failures).expect("a notice");
    assert_eq!(notice.matches("\n- ").count(), 3, "one bullet per failure");
    for raw in ["a.png", "b.png", "c.png"] {
        assert!(notice.contains(raw), "missing {raw} in: {notice}");
    }
}

#[test]
fn a_delivery_failure_is_not_reported_as_a_reference_problem() {
    let failures = vec![LocalImageFailure {
        raw: "chart.png".to_string(),
        resolved: Some(PathBuf::from("/tmp/chart.png")),
        reason: LocalImageFailureReason::DeliveryFailed,
    }];
    let notice = failure_notice(&failures).expect("a notice");
    assert!(
        notice.contains("the channel could not deliver it"),
        "the DeliveryFailed wording must differ from a validation reason: {notice}"
    );
}

#[test]
fn appending_a_notice_keeps_the_body_and_adds_a_blank_line() {
    let body = "Here is the answer.";
    let failures = vec![LocalImageFailure {
        raw: "missing.png".to_string(),
        resolved: None,
        reason: LocalImageFailureReason::NotFound,
    }];
    let combined = append_failure_notice(body, &failures);
    assert!(combined.starts_with("Here is the answer.\n\n"));
    assert!(combined.contains("missing.png"));
}

#[test]
fn appending_without_failures_returns_the_body_untouched() {
    // The pass-through case: a site that always appends must not gain a
    // trailing newline when every image was delivered.
    assert_eq!(append_failure_notice("answer", &[]), "answer");
}

#[test]
fn an_empty_body_becomes_the_notice_alone() {
    // The reason the helper exists: a reply whose ONLY content was a broken
    // image reference must not deliver a blank-line-prefixed notice, and must
    // not be skipped by a downstream emptiness check.
    let failures = vec![LocalImageFailure {
        raw: "missing.png".to_string(),
        resolved: None,
        reason: LocalImageFailureReason::NotFound,
    }];
    let combined = append_failure_notice("   ", &failures);
    assert!(
        combined.starts_with("⚠️"),
        "an empty body must yield the notice itself: {combined}"
    );
    assert!(!combined.starts_with('\n'));
}

// ---------------------------------------------------------------------------
// #502 — the notice a PROMOTED intermediate bubble carries
// ---------------------------------------------------------------------------

/// The exact line a promoted bubble shows when a resolved entry could not be
/// read back (the file vanished between validation and the send). The wording
/// is load-bearing: it is what the owner reads in the chat, and step 8 of the
/// #502 plan pins it verbatim.
#[test]
fn a_promoted_bubble_names_an_unreadable_entry_with_the_real_wording() {
    let failures = vec![LocalImageFailure {
        raw: "/tmp/probe-502.png".to_string(),
        resolved: Some(PathBuf::from("/tmp/probe-502.png")),
        reason: LocalImageFailureReason::Unreadable,
    }];
    let notice = failure_notice(&failures).expect("a notice");

    assert_eq!(
        notice,
        "⚠️ Image not attached — the reply referenced an image that could not be delivered:\n- /tmp/probe-502.png (file could not be read)"
    );
}

/// A promoted bubble whose picture DID land still reports the one that did not:
/// the notice rides the prose the model wrote, rather than replacing it.
#[test]
fn a_partly_delivered_promoted_bubble_keeps_its_prose_and_appends_the_notice() {
    let body = "Here is the chart I promised.";
    let failures = vec![LocalImageFailure {
        raw: "/tmp/gone.png".to_string(),
        resolved: Some(PathBuf::from("/tmp/gone.png")),
        reason: LocalImageFailureReason::Unreadable,
    }];

    let combined = append_failure_notice(body, &failures);
    assert!(combined.starts_with(body));
    assert!(combined.contains("- /tmp/gone.png (file could not be read)"));
}

/// The dedup key is the NOTICE-FREE stripped body (#502): if the notice were
/// part of the recorded text, the same intermediate would stop matching its own
/// final response and the user would read the answer twice.
#[test]
fn the_dedup_record_is_the_notice_free_body() {
    let stripped = "Here is the chart I promised.";
    let failures = vec![LocalImageFailure {
        raw: "/tmp/gone.png".to_string(),
        resolved: None,
        reason: LocalImageFailureReason::NotFound,
    }];

    let delivered_body = append_failure_notice(stripped, &failures);
    assert_ne!(
        delivered_body, stripped,
        "the user-visible body does carry the notice"
    );
    assert_eq!(
        append_failure_notice(stripped, &[]),
        stripped,
        "the value recorded for dedup is the notice-free body"
    );
}

// ---------------------------------------------------------------------------
// strip_image_references — remote link survives, local reference does not
// ---------------------------------------------------------------------------

#[test]
fn strip_only_keeps_a_remote_link_in_the_text() {
    let text = format!("see ![chart]({REMOTE_PNG}) for details");
    let scan = strip_image_references(&text, None);
    assert_eq!(
        scan.text, text,
        "a strip-only site must not delete a link nothing would fetch"
    );
    assert!(scan.attachments.is_empty());
    assert!(
        scan.remote.is_empty(),
        "strip-only mode does not collect remote targets for fetching"
    );
}

#[test]
fn strip_only_still_removes_a_local_marker() {
    let scan = strip_image_references("before <<IMG:/tmp/nowhere.png>> after", None);
    assert_eq!(scan.text, "before  after");
}

#[test]
fn extract_collects_a_remote_link_instead_of_leaving_it() {
    let text = format!("see ![chart]({REMOTE_PNG}) for details");
    let scan = extract_local_images(&text, None);
    assert_eq!(
        scan.text, "see  for details",
        "the delivery path owns the remote reference"
    );
    assert_eq!(scan.remote, vec![REMOTE_PNG.to_string()]);
}

#[test]
fn strip_only_removes_a_validated_local_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let png = write_fixture(dir.path(), "chart.png", PNG_BYTES);
    let scan = strip_image_references(&format!("x ![chart]({}) y", png.display()), None);
    assert_eq!(scan.text, "x  y");
    assert_eq!(resolved_paths(&scan), vec![png]);
}

#[test]
fn strip_only_reports_a_missing_local_file_as_a_failure() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("gone.png");
    let scan = strip_image_references(
        &format!("x ![chart]({}) y", missing.display()),
        Some(dir.path()),
    );
    assert_eq!(scan.text, "x  y", "dead markdown never reaches the user");
    assert_eq!(scan.failures.len(), 1);
    assert_eq!(scan.failures[0].reason, LocalImageFailureReason::NotFound);
}

#[test]
fn strip_only_still_removes_a_remote_marker() {
    // A `<<IMG:…>>` marker is machine syntax, never prose — even a remote
    // target leaves the text in strip-only mode, or the raw token leaks.
    let scan = strip_image_references(&format!("x <<IMG:{REMOTE_PNG}>> y"), None);
    assert_eq!(scan.text, "x  y");
    assert!(scan.remote.is_empty());
    assert!(scan.attachments.is_empty());
}

#[test]
fn a_remote_link_inside_a_code_span_is_left_alone() {
    let text = format!("`![chart]({REMOTE_PNG})`");
    for scan in [
        strip_image_references(&text, None),
        extract_local_images(&text, None),
    ] {
        assert_eq!(scan.text, text, "code spans are not delivery input");
        assert!(scan.remote.is_empty());
        assert!(scan.attachments.is_empty());
    }
}

// ---------------------------------------------------------------------------
// Post-delivery re-entry latch (#286)
// ---------------------------------------------------------------------------
//
// The correction turn the delivery site queues runs a full tool loop and
// delivers through the SAME path, so a failure in that turn would re-arm the
// re-entry and chain synthetic turns forever. These pin the bound: one
// re-entry per user exchange, re-armed by the user's next message.

#[cfg(feature = "telegram")]
mod reentry_latch {
    use crate::channels::telegram::TelegramState;
    use std::sync::Arc;
    use uuid::Uuid;

    #[test]
    fn the_latch_admits_exactly_one_reentry_per_exchange() {
        let state = Arc::new(TelegramState::new());
        let sid = Uuid::new_v4();

        assert!(
            state.try_spend_image_reentry(sid),
            "the first delivery failure must buy a correction turn"
        );
        for _ in 0..3 {
            assert!(
                !state.try_spend_image_reentry(sid),
                "a second failure inside the same exchange must not re-arm the \
                 re-entry — the correction turn would chain forever"
            );
        }
    }

    #[test]
    fn clearing_the_latch_re_arms_it_for_the_next_exchange() {
        let state = Arc::new(TelegramState::new());
        let sid = Uuid::new_v4();

        assert!(state.try_spend_image_reentry(sid));
        assert!(!state.try_spend_image_reentry(sid));

        // The user's next message ends the exchange: a genuine failure in the
        // turn it triggers deserves its own correction turn.
        state.clear_image_reentry(sid);
        assert!(
            state.try_spend_image_reentry(sid),
            "a later exchange must heal too, not be starved by an earlier failure"
        );
    }

    #[test]
    fn the_latch_is_per_session() {
        let state = Arc::new(TelegramState::new());
        let spent = Uuid::new_v4();
        let fresh = Uuid::new_v4();

        assert!(state.try_spend_image_reentry(spent));
        assert!(
            state.try_spend_image_reentry(fresh),
            "one session's spent re-entry must not silence another's"
        );
    }
}

// ---------------------------------------------------------------------------
// Local-file links (#1916) — the non-media sibling of the image family
// ---------------------------------------------------------------------------
//
// A `[label](path)` link is what the model writes when it means "the file is
// here". The reader is a channel user with NO filesystem access, so the link is
// not the deliverable — the FILE is. Before #1916 the link rendered as an inert
// entity whose URL was the raw path: Telegram had no scheme to resolve, so the
// file never arrived and the link was dead.
//
// These pin the scanner the delivery sites call. The marker policy is the one
// place the file family deliberately differs from the image family: a resolved
// file leaves the text, but the link that named it is replaced by a visible
// `📎 <label>` marker (#1918) rather than deleted, so the reader keeps the
// file's name and the position it was referenced at. A REJECTED candidate stays
// byte-identical and is reported, because a link carries its own label and a
// silent strip would delete the reader's only clue about what was referenced.

mod local_file_links {
    use crate::utils::image::{
        LocalFileScan, LocalImageFailure, LocalImageFailureReason, TELEGRAM_DOCUMENT_MAX_BYTES,
        append_file_failure_notice, extract_local_files, file_failure_notice, validate_local_file,
    };
    use std::path::{Path, PathBuf};

    const PDF_BYTES: &[u8] = b"%PDF-1.7\n1 0 obj\n<<>>\nendobj\n";

    fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("write fixture");
        path
    }

    fn paths(scan: &LocalFileScan) -> Vec<PathBuf> {
        scan.attachments.iter().map(|f| f.path.clone()).collect()
    }

    fn failure(raw: &str) -> LocalImageFailure {
        LocalImageFailure {
            raw: raw.to_string(),
            resolved: None,
            reason: LocalImageFailureReason::NotFound,
        }
    }

    // -----------------------------------------------------------------------
    // Resolution and the marker
    // -----------------------------------------------------------------------

    #[test]
    fn a_local_file_link_resolves_and_leaves_the_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf = write_file(dir.path(), "q3.pdf", PDF_BYTES);
        let scan = extract_local_files(&format!("before [Q3 report]({}) after", pdf.display()), None);
        assert_eq!(
            scan.text, "before 📎 Q3 report after",
            "the reference leaves a visible marker where the link was"
        );
        assert_eq!(paths(&scan), vec![pdf]);
        assert_eq!(scan.attachments[0].caption.as_deref(), Some("Q3 report"));
        assert!(scan.failures.is_empty());
    }

    #[test]
    fn an_empty_label_yields_no_caption() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf = write_file(dir.path(), "q3.pdf", PDF_BYTES);
        let scan = extract_local_files(&format!("[]({})", pdf.display()), None);
        assert_eq!(paths(&scan), vec![pdf]);
        assert_eq!(
            scan.attachments[0].caption, None,
            "an empty label is not a caption"
        );
        assert_eq!(
            scan.text, "📎 q3.\u{200b}pdf",
            "an empty label still leaves a marker — it falls back to the file name, \
             disarmed of the autolinker (#1938) so the name is not read as a domain"
        );
    }

    #[test]
    fn two_files_attach_in_order_of_appearance() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = write_file(dir.path(), "a.pdf", PDF_BYTES);
        let b = write_file(dir.path(), "b.pdf", PDF_BYTES);
        let scan = extract_local_files(&format!("[A]({}) then [B]({})", a.display(), b.display()), None);
        assert_eq!(paths(&scan), vec![a, b]);
        assert_eq!(scan.text, "📎 A then 📎 B");
    }

    #[test]
    fn an_angle_bracket_target_holds_spaces() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf = write_file(dir.path(), "Q3 final.pdf", PDF_BYTES);
        let scan = extract_local_files(&format!("x [Q3](<{}>) y", pdf.display()), None);
        assert_eq!(scan.text, "x 📎 Q3 y");
        assert_eq!(paths(&scan), vec![pdf]);
    }

    #[test]
    fn a_title_after_the_target_is_ignored_and_the_label_captions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf = write_file(dir.path(), "q3.pdf", PDF_BYTES);
        let scan = extract_local_files(
            &format!("x [Q3 report]({} \"quarterly\") y", pdf.display()),
            None,
        );
        assert_eq!(scan.text, "x 📎 Q3 report y");
        assert_eq!(scan.attachments[0].caption.as_deref(), Some("Q3 report"));
    }

    #[test]
    fn a_relative_target_resolves_against_the_base_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("reports")).expect("mkdir");
        let pdf = write_file(&dir.path().join("reports"), "q3.pdf", PDF_BYTES);
        let scan = extract_local_files("see [report](reports/q3.pdf) here", Some(dir.path()));
        assert_eq!(scan.text, "see 📎 report here");
        assert_eq!(paths(&scan), vec![pdf]);
        assert_eq!(scan.attachments[0].caption.as_deref(), Some("report"));
    }

    #[test]
    fn a_media_bearing_body_loses_its_raw_file_path() {
        // #1918: the rich body is rebuilt from the model's reply, so delivery
        // runs THIS pass over that buffer too. A body that owns a photo must
        // not ship a dead local path beside it.
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf = write_file(dir.path(), "q3.pdf", PDF_BYTES);
        let png = write_file(dir.path(), "chart.png", super::PNG_BYTES);
        let rich = format!(
            "![chart]({})\n\nsee [Q3 report]({}) for the numbers",
            png.display(),
            pdf.display()
        );
        let body = extract_local_files(&rich, None).text;
        assert!(
            !body.contains(&format!("]({})", pdf.display())),
            "no raw local path may survive into the rich body: {body}"
        );
        assert!(body.contains("📎 Q3 report"), "{body}");
        assert!(
            body.contains(&format!("![chart]({})", png.display())),
            "the image family's reference is untouched: {body}"
        );
    }

    // -----------------------------------------------------------------------
    // The marker's span — where it sits, so a later pass can target it
    // -----------------------------------------------------------------------

    #[test]
    fn each_marker_span_slices_back_to_its_own_marker() {
        // #1918: the scanner records WHERE each `📎 <label>` marker sits, so the
        // rich-plane rewrite can target exactly that span rather than a label
        // that may occur again in the prose. Two delivered links and one
        // REJECTED candidate pin the alignment: the rejected link leaves no
        // marker and so records no span.
        let dir = tempfile::tempdir().expect("tempdir");
        let a = write_file(dir.path(), "q3.pdf", PDF_BYTES);
        let b = write_file(dir.path(), "q4.pdf", PDF_BYTES);
        let missing = dir.path().join("gone.pdf");
        let scan = extract_local_files(
            &format!(
                "first [Q3 report]({}) then [Q4 report]({}) and [Gone]({})",
                a.display(),
                b.display(),
                missing.display()
            ),
            Some(dir.path()),
        );

        assert_eq!(
            paths(&scan),
            vec![a, b],
            "only the two live files attach"
        );
        assert_eq!(scan.failures.len(), 1, "the missing file is reported");
        assert_eq!(
            scan.attachments.len(),
            2,
            "one span per DELIVERED marker, none for the rejected candidate"
        );

        let markers: Vec<&str> = scan
            .attachments
            .iter()
            .map(|f| {
                let span = f.marker_span.clone().expect("a scanned marker has a span");
                &scan.text[span]
            })
            .collect();
        assert_eq!(
            markers,
            vec!["📎 Q3 report", "📎 Q4 report"],
            "each span is exactly its own marker; whole text: {:?}",
            scan.text
        );
    }

    #[test]
    fn a_marker_span_survives_the_leading_trim() {
        // The final `trim()` shifts every index left by the leading whitespace
        // it drops, so the recorded spans must be rebased. A body that OPENS
        // with whitespace is the only shape that exercises that rebase: without
        // it the sliced span is off by the trimmed run.
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf = write_file(dir.path(), "q3.pdf", PDF_BYTES);
        let scan = extract_local_files(
            &format!("\n\n  leading space [Q3 report]({})", pdf.display()),
            Some(dir.path()),
        );

        assert_eq!(
            scan.text, "leading space 📎 Q3 report",
            "the leading run is trimmed off the shipped text"
        );
        let span = scan.attachments[0]
            .marker_span
            .clone()
            .expect("a scanned marker has a span");
        assert_eq!(
            &scan.text[span.clone()],
            "📎 Q3 report",
            "the span is rebased onto the TRIMMED text (raw span {span:?})"
        );
    }

    #[test]
    fn a_model_written_marker_prefix_is_not_doubled() {
        // #2001: the capability line tells the model the reference becomes a
        // `📎 <label>` marker, so a model may prefix that marker itself. The
        // harness consumes the model's own `📎 ` before emitting its own, so
        // the reader sees exactly ONE marker rather than `📎 📎 <label>`.
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf = write_file(dir.path(), "q3.pdf", PDF_BYTES);
        let scan = extract_local_files(
            &format!("before 📎 [Q3 report]({}) after", pdf.display()),
            None,
        );
        assert_eq!(
            scan.text, "before 📎 Q3 report after",
            "the model's own 📎 is consumed, not doubled"
        );
        assert_eq!(scan.text.matches('📎').count(), 1, "exactly one marker");
        assert_eq!(paths(&scan), vec![pdf]);
    }

    // -----------------------------------------------------------------------
    // What the family must LEAVE ALONE
    // -----------------------------------------------------------------------

    #[test]
    fn a_relative_target_without_a_base_dir_stays_literal() {
        let text = "see [report](reports/q3.pdf) here";
        let scan = extract_local_files(text, None);
        assert_eq!(scan.text, text, "no base directory to resolve against");
        assert!(scan.attachments.is_empty());
        assert!(scan.failures.is_empty());
    }

    #[test]
    fn a_remote_link_is_left_alone() {
        let text = "see [the site](https://example.com/a) for details";
        let scan = extract_local_files(text, None);
        assert_eq!(scan.text, text, "Telegram resolves a real URL itself");
        assert!(scan.attachments.is_empty());
        assert!(scan.failures.is_empty(), "a URL is not a missing file");
    }

    #[test]
    fn an_image_reference_is_not_claimed_by_the_file_family() {
        let dir = tempfile::tempdir().expect("tempdir");
        let png = write_file(dir.path(), "chart.png", super::PNG_BYTES);
        let text = format!("x ![chart]({}) y", png.display());
        let scan = extract_local_files(&text, None);
        assert_eq!(scan.text, text, "the image family owns `![…](…)`");
        assert!(scan.attachments.is_empty());
        assert!(scan.failures.is_empty());
    }

    #[test]
    fn a_link_inside_a_code_span_is_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf = write_file(dir.path(), "q3.pdf", PDF_BYTES);
        let text = format!("`[Q3 report]({})`", pdf.display());
        let scan = extract_local_files(&text, None);
        assert_eq!(
            scan.text, text,
            "a code span is documentation, not a deliverable"
        );
        assert!(scan.attachments.is_empty());
        assert!(scan.failures.is_empty());
    }

    #[test]
    fn a_link_inside_a_code_fence_is_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf = write_file(dir.path(), "q3.pdf", PDF_BYTES);
        let text = format!("```\n[Q3 report]({})\n```", pdf.display());
        let scan = extract_local_files(&text, None);
        assert_eq!(scan.text, text);
        assert!(scan.attachments.is_empty());
    }

    #[test]
    fn an_escaped_link_stays_literal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf = write_file(dir.path(), "q3.pdf", PDF_BYTES);
        let text = format!("\\[Q3 report]({})", pdf.display());
        let scan = extract_local_files(&text, None);
        assert_eq!(scan.text, text);
        assert!(scan.attachments.is_empty());
    }

    #[test]
    fn a_non_file_scheme_or_anchor_is_left_alone() {
        for text in [
            "mail [me](mailto:a@b.c)",
            "call [me](tel:+1234)",
            "see [anchor](#section)",
            "see [nothing]()",
        ] {
            let scan = extract_local_files(text, None);
            assert_eq!(scan.text, text, "{text} must survive byte-identical");
            assert!(scan.attachments.is_empty(), "{text}");
            assert!(scan.failures.is_empty(), "{text}");
        }
    }

    #[test]
    fn prose_that_merely_looks_like_a_link_is_untouched() {
        for text in ["see [1] and [2] for details", "a [b] c", "[standalone]"] {
            let scan = extract_local_files(text, None);
            assert_eq!(scan.text, text);
            assert!(scan.attachments.is_empty());
            assert!(scan.failures.is_empty());
        }
    }

    // -----------------------------------------------------------------------
    // Rejections — reported AND left in the text
    // -----------------------------------------------------------------------

    #[test]
    fn a_missing_file_is_reported_and_the_link_survives() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("gone.pdf");
        let raw = format!("[Q3 report]({})", missing.display());
        let text = format!("x {raw} y");
        let scan = extract_local_files(&text, Some(dir.path()));
        assert_eq!(
            scan.text, text,
            "a rejected link keeps its label — it is the reader's only clue"
        );
        assert!(scan.attachments.is_empty());
        assert_eq!(scan.failures.len(), 1);
        assert_eq!(scan.failures[0].reason, LocalImageFailureReason::NotFound);
        assert_eq!(scan.failures[0].raw, raw);
        assert_eq!(scan.failures[0].resolved.as_deref(), Some(missing.as_path()));
    }

    #[test]
    fn a_directory_is_not_a_regular_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sub = dir.path().join("reports");
        std::fs::create_dir(&sub).expect("mkdir");
        let scan = extract_local_files(&format!("[x]({})", sub.display()), None);
        assert!(scan.attachments.is_empty());
        assert_eq!(scan.failures[0].reason, LocalImageFailureReason::NotAFile);
    }

    #[test]
    fn an_empty_file_is_reported_as_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let empty = write_file(dir.path(), "empty.pdf", b"");
        let scan = extract_local_files(&format!("[x]({})", empty.display()), None);
        assert!(scan.attachments.is_empty());
        assert_eq!(scan.failures[0].reason, LocalImageFailureReason::Empty);
    }

    #[test]
    fn a_file_past_the_document_ceiling_is_reported_as_too_large() {
        let dir = tempfile::tempdir().expect("tempdir");
        let big = dir.path().join("big.pdf");
        // A sparse file: `set_len` costs no disk and no time, where writing
        // 50 MiB of fixture bytes would.
        std::fs::File::create(&big)
            .expect("create")
            .set_len(TELEGRAM_DOCUMENT_MAX_BYTES + 1)
            .expect("set_len");
        let scan = extract_local_files(&format!("[x]({})", big.display()), None);
        assert!(scan.attachments.is_empty());
        assert_eq!(scan.failures[0].reason, LocalImageFailureReason::TooLarge);
    }

    #[test]
    fn validate_accepts_a_readable_non_empty_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf = write_file(dir.path(), "q3.pdf", PDF_BYTES);
        assert!(validate_local_file(&pdf).is_ok());
    }

    // -----------------------------------------------------------------------
    // The notice — the honest line when the nudge budget is spent
    // -----------------------------------------------------------------------

    #[test]
    fn no_failures_produces_no_file_notice() {
        assert!(file_failure_notice(&[]).is_none());
    }

    #[test]
    fn a_file_notice_names_the_reference_and_the_reason() {
        let notice = file_failure_notice(&[failure("[Q3 report](/root/reports/q3.pdf)")])
            .expect("notice");
        assert!(notice.contains("File not attached"), "{notice}");
        assert!(notice.contains("a file"), "{notice}");
        assert!(
            notice.contains("[Q3 report](/root/reports/q3.pdf)"),
            "{notice}"
        );
        assert!(notice.contains("file not found"), "{notice}");
    }

    #[test]
    fn appending_a_file_notice_keeps_the_body_and_adds_a_blank_line() {
        let body = append_file_failure_notice("hello", &[failure("[x](/nope)")]);
        assert!(body.starts_with("hello\n\n⚠️ File not attached"), "{body}");
    }

    #[test]
    fn an_empty_body_becomes_the_file_notice_alone() {
        let body = append_file_failure_notice("   ", &[failure("[x](/nope)")]);
        assert!(body.starts_with("⚠️ File not attached"), "{body}");
    }

    #[test]
    fn appending_without_failures_returns_the_body_untouched() {
        assert_eq!(append_file_failure_notice("hello", &[]), "hello");
    }
}

// ---------------------------------------------------------------------------
// The rich plane's document twin (#1918)
// ---------------------------------------------------------------------------

/// `rewrite_local_files` — the document twin of `rewrite_local_images`. Where
/// `extract_local_files` emits the `📎 <label>` MARKER the HTML plane needs, this
/// one replaces a resolvable link IN PLACE with the `tg://document?id=docN`
/// reference the rich plane's media array answers. The two walk the same
/// references in the same order and share `resolve_file_target`, so they can
/// never disagree about which links are files.
mod local_file_rewrite {
    use crate::utils::image::{DOC_ID_PREFIX, rewrite_local_files};
    use std::path::{Path, PathBuf};

    const PDF_BYTES: &[u8] = b"%PDF-1.7\n1 0 obj\n<<>>\nendobj\n";
    const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x01\x02\x03\x04\x05\x06\x07";

    fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("write fixture");
        path
    }

    #[test]
    fn a_local_file_link_becomes_a_document_reference_in_place() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf = write_file(dir.path(), "q3-report.pdf", PDF_BYTES);
        let rw = rewrite_local_files(
            &format!("before [Q3 report]({}) after", pdf.display()),
            Some(dir.path()),
            DOC_ID_PREFIX,
            &[],
        );
        assert_eq!(
            rw.rich, "before ![📎 Q3 report](tg://document?id=doc0) after",
            "the link is replaced AT its position, never stripped"
        );
        assert_eq!(rw.entries.len(), 1);
        assert_eq!(rw.entries[0].id, "doc0");
        assert_eq!(rw.entries[0].file.path, pdf);
        assert_eq!(rw.entries[0].file.caption.as_deref(), Some("Q3 report"));
        assert_eq!(
            rw.entries[0].file.marker_span, None,
            "a file that came from the rewrite has no position in any scan buffer"
        );
    }

    #[test]
    fn two_files_get_their_own_ids_in_order_of_appearance() {
        let dir = tempfile::tempdir().expect("tempdir");
        let q3 = write_file(dir.path(), "q3.pdf", PDF_BYTES);
        let q4 = write_file(dir.path(), "q4.pdf", PDF_BYTES);
        let rw = rewrite_local_files(
            &format!("[Q3]({}) and [Q4]({})", q3.display(), q4.display()),
            Some(dir.path()),
            DOC_ID_PREFIX,
            &[],
        );
        assert_eq!(
            rw.rich,
            "![📎 Q3](tg://document?id=doc0) and ![📎 Q4](tg://document?id=doc1)"
        );
        let ids: Vec<&str> = rw.entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["doc0", "doc1"]);
        let paths: Vec<PathBuf> = rw.entries.iter().map(|e| e.file.path.clone()).collect();
        assert_eq!(paths, vec![q3, q4]);
    }

    #[test]
    fn an_empty_label_falls_back_to_the_files_own_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf = write_file(dir.path(), "q3-report.pdf", PDF_BYTES);
        let rw = rewrite_local_files(
            &format!("[]({})", pdf.display()),
            Some(dir.path()),
            DOC_ID_PREFIX,
            &[],
        );
        assert_eq!(
            rw.rich,
            "![📎 q3-report.\u{200b}pdf](tg://document?id=doc0)",
            "the reader always has something to anchor on — the name, disarmed of \
             the autolinker (#1938) so the marker is not read as a domain"
        );
        assert_eq!(rw.entries.len(), 1);
        assert!(
            rw.entries[0].file.caption.is_none(),
            "an empty label is no caption — the name is the marker's text, not its caption"
        );
    }

    #[test]
    fn a_dotted_label_is_disarmed_in_the_rich_alt_too() {
        // The live specimen (#1938, topic `Telegram: Rich Text`): the label
        // `1918-fix-state.md` was autolinked by the client as the Moldova
        // ccTLD. The rich plane carries the same marker text in the `alt`, so
        // the disarmer has to reach this plane as well as the HTML marker.
        let dir = tempfile::tempdir().expect("tempdir");
        let md = write_file(dir.path(), "1918-fix-state.md", b"# state\n");
        let rw = rewrite_local_files(
            &format!("[1918-fix-state.md]({})", md.display()),
            Some(dir.path()),
            DOC_ID_PREFIX,
            &[],
        );
        assert_eq!(
            rw.rich,
            "![📎 1918-fix-state.\u{200b}md](tg://document?id=doc0)",
            "the alt is disarmed exactly as the HTML marker is"
        );
        assert_eq!(
            rw.entries[0].file.caption.as_deref(),
            Some("1918-fix-state.md"),
            "the CAPTION is not a marker and stays verbatim — only the visible \
             marker text is disarmed"
        );
    }

    #[test]
    fn a_link_inside_a_code_span_is_left_byte_identical() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf = write_file(dir.path(), "q3.pdf", PDF_BYTES);
        let text = format!("`[Q3]({})`", pdf.display());
        let rw = rewrite_local_files(&text, Some(dir.path()), DOC_ID_PREFIX, &[]);
        assert_eq!(rw.rich, text, "a code span is documentation, not a link");
        assert!(rw.entries.is_empty());
    }

    #[test]
    fn a_missing_file_stays_byte_identical_and_is_not_recorded() {
        // A rejected candidate is left as written and is NOT an entry: the
        // reference must survive so the reader still sees what was named. The
        // failure is the SCAN's to report — one predicate, one home.
        let text = "before [Q3 report](/definitely/not/here-q3.pdf) after";
        let rw = rewrite_local_files(text, None, DOC_ID_PREFIX, &[]);
        assert_eq!(rw.rich, text);
        assert!(rw.entries.is_empty());
    }

    #[test]
    fn a_relative_target_without_a_base_dir_stays_literal() {
        // It may be ordinary prose that merely looks like a link, so with no
        // working directory to resolve against it is left alone.
        let text = "see [report](reports/q3.pdf) here";
        let rw = rewrite_local_files(text, None, DOC_ID_PREFIX, &[]);
        assert_eq!(rw.rich, text);
        assert!(rw.entries.is_empty());
    }

    #[test]
    fn a_remote_link_stays_byte_identical() {
        let text = "see [the site](https://example.com/a) for details";
        let rw = rewrite_local_files(text, None, DOC_ID_PREFIX, &[]);
        assert_eq!(rw.rich, text);
        assert!(rw.entries.is_empty());
    }

    #[test]
    fn an_already_delivered_file_is_consumed_without_a_second_reference() {
        // The document is already in the chat (a promoted intermediate sent
        // it). The reference is CONSUMED — nothing is left to answer — but no
        // entry is recorded: a delivered document is not a lost one, and a
        // reference with no entry would be neutralised by the shield anyway.
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf = write_file(dir.path(), "q3.pdf", PDF_BYTES);
        let rw = rewrite_local_files(
            &format!("before [Q3]({}) after", pdf.display()),
            Some(dir.path()),
            DOC_ID_PREFIX,
            std::slice::from_ref(&pdf),
        );
        assert_eq!(rw.rich, "before  after");
        assert!(rw.entries.is_empty());
    }

    #[test]
    fn a_model_written_marker_prefix_is_not_doubled_in_the_rich_plane() {
        // #2001, rich plane: a model's own `📎 ` before the reference must not
        // survive beside the harness alt, or the reader gets
        // `📎 ![📎 <label>](tg://document?id=…)`.
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf = write_file(dir.path(), "q3.pdf", PDF_BYTES);
        let rw = rewrite_local_files(
            &format!("before 📎 [Q3 report]({}) after", pdf.display()),
            Some(dir.path()),
            DOC_ID_PREFIX,
            &[],
        );
        assert_eq!(
            rw.rich.matches('📎').count(),
            1,
            "exactly one marker: {:?}",
            rw.rich
        );
        assert_eq!(
            rw.rich,
            "before ![📎 Q3 report](tg://document?id=doc0) after",
            "the model's own 📎 is consumed, not doubled"
        );
        assert_eq!(rw.entries.len(), 1);
    }

    #[test]
    fn the_image_familys_reference_is_not_a_file_link() {
        // `![alt](path)` belongs to the image family: the `!` guard is what
        // keeps one reference from being claimed twice. Without it the file
        // walk would eat a picture the image walk had already resolved.
        let dir = tempfile::tempdir().expect("tempdir");
        let png = write_file(dir.path(), "pic.png", PNG_BYTES);
        let text = format!("![pic]({})", png.display());
        let rw = rewrite_local_files(&text, Some(dir.path()), DOC_ID_PREFIX, &[]);
        assert_eq!(rw.rich, text);
        assert!(rw.entries.is_empty());
    }

    #[test]
    fn the_id_prefix_is_the_callers_choice() {
        // Entries are matched to references BY ID inside one message's media
        // array, so the prefix is a parameter: the delivery site passes `doc`
        // so a document entry can never answer a picture's reference.
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf = write_file(dir.path(), "q3.pdf", PDF_BYTES);
        let rw = rewrite_local_files(
            &format!("[Q3]({})", pdf.display()),
            Some(dir.path()),
            "xyz",
            &[],
        );
        assert_eq!(rw.rich, "![📎 Q3](tg://document?id=xyz0)");
        assert_eq!(rw.entries[0].id, "xyz0");
    }

    #[test]
    fn leading_whitespace_is_trimmed_off_the_rich_body() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf = write_file(dir.path(), "q3.pdf", PDF_BYTES);
        let rw = rewrite_local_files(
            &format!("\n\n  before [Q3]({})", pdf.display()),
            Some(dir.path()),
            DOC_ID_PREFIX,
            &[],
        );
        assert_eq!(rw.rich, "before ![📎 Q3](tg://document?id=doc0)");
    }
}
