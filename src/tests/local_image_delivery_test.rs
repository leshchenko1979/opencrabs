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
// These pin the scanner the delivery sites call. The removal policy is the one
// place the file family deliberately differs from the image family: a resolved
// file leaves the text, but a REJECTED candidate stays byte-identical and is
// reported, because a link carries its own label and a silent strip would
// delete the reader's only clue about what was referenced.

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
    // Resolution and removal
    // -----------------------------------------------------------------------

    #[test]
    fn a_local_file_link_resolves_and_leaves_the_text() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf = write_file(dir.path(), "q3.pdf", PDF_BYTES);
        let scan = extract_local_files(&format!("before [Q3 report]({}) after", pdf.display()), None);
        assert_eq!(scan.text, "before  after");
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
    }

    #[test]
    fn two_files_attach_in_order_of_appearance() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = write_file(dir.path(), "a.pdf", PDF_BYTES);
        let b = write_file(dir.path(), "b.pdf", PDF_BYTES);
        let scan = extract_local_files(&format!("[A]({}) then [B]({})", a.display(), b.display()), None);
        assert_eq!(paths(&scan), vec![a, b]);
        assert_eq!(scan.text, "then");
    }

    #[test]
    fn an_angle_bracket_target_holds_spaces() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pdf = write_file(dir.path(), "Q3 final.pdf", PDF_BYTES);
        let scan = extract_local_files(&format!("x [Q3](<{}>) y", pdf.display()), None);
        assert_eq!(scan.text, "x  y");
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
        assert_eq!(scan.text, "x  y");
        assert_eq!(scan.attachments[0].caption.as_deref(), Some("Q3 report"));
    }

    #[test]
    fn a_relative_target_resolves_against_the_base_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("reports")).expect("mkdir");
        let pdf = write_file(&dir.path().join("reports"), "q3.pdf", PDF_BYTES);
        let scan = extract_local_files("see [report](reports/q3.pdf) here", Some(dir.path()));
        assert_eq!(scan.text, "see  here");
        assert_eq!(paths(&scan), vec![pdf]);
        assert_eq!(scan.attachments[0].caption.as_deref(), Some("report"));
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
