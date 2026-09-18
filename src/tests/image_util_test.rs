//! Tests for `utils::image` — `<<IMG:path>>` marker extraction.

use crate::utils::image::{
    ImageTarget, LocalImageFailure, LocalImageFailureReason, LocalImageScan, classify_image_target,
    code_regions, extract_img_markers, extract_local_images, failure_notice, image_extension,
    is_remote_url, is_supported_image, validate_local_image,
};
use crate::utils::image_fetch::{
    MAX_REMOTE_IMAGES_PER_REPLY, fetch_remote_image, resolve_remote_images,
};
use std::path::{Path, PathBuf};

#[test]
fn no_markers_returns_text_unchanged() {
    let (text, paths) = extract_img_markers("plain text");
    assert_eq!(text, "plain text");
    assert!(paths.is_empty());
}

#[test]
fn single_img_marker_extracts_path() {
    let (text, paths) = extract_img_markers("see <<IMG:/tmp/a.png>> here");
    assert_eq!(text, "see  here");
    assert_eq!(paths, vec!["/tmp/a.png"]);
}

#[test]
fn multiple_markers_in_order() {
    let input = "<<IMG:/x.jpg>> and <<IMG:/y.png>> done";
    let (text, paths) = extract_img_markers(input);
    assert_eq!(text, "and  done");
    assert_eq!(paths, vec!["/x.jpg", "/y.png"]);
}

#[test]
fn empty_path_skipped() {
    let (text, paths) = extract_img_markers("<<IMG:>> rest");
    assert_eq!(text, "rest");
    assert!(paths.is_empty());
}

#[test]
fn unclosed_marker_left_intact() {
    let input = "<<IMG:/no_close text";
    let (text, paths) = extract_img_markers(input);
    assert_eq!(text, input);
    assert!(paths.is_empty());
}

#[test]
fn marker_only_input_returns_empty() {
    let (text, paths) = extract_img_markers("<<IMG:/only.png>>");
    assert_eq!(text, "");
    assert_eq!(paths, vec!["/only.png"]);
}

#[test]
fn whitespace_around_path_trimmed() {
    let (_, paths) = extract_img_markers("<<IMG:  /tmp/photo.jpg  >>");
    assert_eq!(paths, vec!["/tmp/photo.jpg"]);
}

#[test]
fn adjacent_markers() {
    let (text, paths) = extract_img_markers("<<IMG:/a.png>><<IMG:/b.png>>");
    assert_eq!(text, "");
    assert_eq!(paths.len(), 2);
}

#[test]
fn empty_input() {
    let (text, paths) = extract_img_markers("");
    assert_eq!(text, "");
    assert!(paths.is_empty());
}

// ── #286: markdown references, remote URLs, and delivery validation ────────

/// A minimal byte blob that passes the PNG magic-byte gate.
const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x01\x02\x03\x04\x05\x06\x07";

/// Base64 of [`PNG_BYTES`] — the body of a `data:image/png;base64,…` URL.
const PNG_DATA_URL_BODY: &str = "iVBORw0KGgoAAQIDBAUGBw==";

fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

#[test]
fn markdown_image_resolves_and_leaves_the_text() {
    let dir = tempfile::tempdir().expect("tempdir");
    let png = write_file(dir.path(), "chart.png", PNG_BYTES);
    let scan = extract_local_images(&format!("before ![chart]({}) after", png.display()), None);
    assert_eq!(scan.text, "before  after");
    assert_eq!(scan.attachments, vec![png]);
    assert!(scan.remote.is_empty());
    assert!(scan.failures.is_empty());
}

#[test]
fn markdown_image_angle_bracket_target_holds_spaces() {
    let dir = tempfile::tempdir().expect("tempdir");
    let png = write_file(dir.path(), "a b.png", PNG_BYTES);
    let scan = extract_local_images(&format!("x ![alt](<{}>) y", png.display()), None);
    assert_eq!(scan.text, "x  y");
    assert_eq!(scan.attachments, vec![png]);
}

#[test]
fn markdown_image_accepts_a_title_after_the_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    let png = write_file(dir.path(), "chart.png", PNG_BYTES);
    let scan = extract_local_images(
        &format!("x ![alt]({} \"the chart\") y", png.display()),
        None,
    );
    assert_eq!(scan.text, "x  y");
    assert_eq!(scan.attachments, vec![png]);
}

#[test]
fn escaped_markdown_image_stays_literal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let text = format!("\\![alt]({})", dir.path().join("chart.png").display());
    let scan = extract_local_images(&text, Some(dir.path()));
    assert_eq!(scan.text, text);
    assert!(scan.attachments.is_empty());
    assert!(scan.failures.is_empty());
}

#[test]
fn unterminated_markdown_image_stays_literal() {
    let text = "![alt](/tmp/chart.png";
    let scan = extract_local_images(text, None);
    assert_eq!(scan.text, text);
    assert!(scan.attachments.is_empty());
    assert!(scan.failures.is_empty());
}

#[test]
fn empty_markdown_target_stays_literal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let text = "![alt]()";
    let scan = extract_local_images(text, Some(dir.path()));
    assert_eq!(scan.text, text);
    assert!(scan.failures.is_empty());
}

#[test]
fn markdown_image_inside_a_code_span_stays_literal() {
    let scan = extract_local_images("see `![alt](https://h/x.png)` here", None);
    assert_eq!(scan.text, "see `![alt](https://h/x.png)` here");
    assert!(scan.remote.is_empty());
    assert!(scan.attachments.is_empty());
}

#[test]
fn markdown_image_inside_a_code_fence_stays_literal() {
    let text = "```\n![alt](https://h/x.png)\n```";
    let scan = extract_local_images(text, None);
    assert_eq!(scan.text, text);
    assert!(scan.remote.is_empty());
}

#[test]
fn remote_markdown_image_is_collected_not_left_as_markdown() {
    let scan = extract_local_images("look ![x](https://example.com/a.png) here", None);
    assert_eq!(scan.text, "look  here");
    assert_eq!(scan.remote, vec!["https://example.com/a.png"]);
    assert!(scan.attachments.is_empty());
    assert!(scan.failures.is_empty());
}

#[test]
fn data_url_markdown_image_counts_as_remote() {
    let url = format!("data:image/png;base64,{PNG_DATA_URL_BODY}");
    let scan = extract_local_images(&format!("![x]({url})"), None);
    assert_eq!(scan.remote, vec![url]);
}

#[test]
fn non_image_schemes_stay_literal() {
    let text = "![a](mailto:x@y.z) and ![b](ftp://h/x.png) and ![c](file:///x.png)";
    let scan = extract_local_images(text, None);
    assert_eq!(scan.text, text);
    assert!(scan.remote.is_empty());
    assert!(scan.failures.is_empty());
}

#[test]
fn relative_markdown_target_without_a_base_dir_stays_literal() {
    let text = "![a](rel.png)";
    let scan = extract_local_images(text, None);
    assert_eq!(scan.text, text);
    assert!(scan.attachments.is_empty());
    assert!(scan.failures.is_empty());
}

#[test]
fn relative_markdown_target_resolves_against_the_base_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let png = write_file(dir.path(), "rel.png", PNG_BYTES);
    let scan = extract_local_images("![a](rel.png)", Some(dir.path()));
    assert_eq!(scan.text, "");
    assert_eq!(scan.attachments, vec![png]);
}

#[test]
fn missing_local_markdown_image_is_stripped_and_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let scan = extract_local_images("before ![a](missing.png) after", Some(dir.path()));
    assert_eq!(scan.text, "before  after");
    assert!(scan.attachments.is_empty());
    assert_eq!(scan.failures.len(), 1);
    assert_eq!(scan.failures[0].raw, "missing.png");
    assert_eq!(
        scan.failures[0].resolved,
        Some(dir.path().join("missing.png"))
    );
    assert_eq!(scan.failures[0].reason, LocalImageFailureReason::NotFound);
}

#[test]
fn img_marker_with_a_remote_url_is_collected() {
    let scan = extract_local_images("see <<IMG:https://cdn.example.com/p.png>> now", None);
    assert_eq!(scan.text, "see  now");
    assert_eq!(scan.remote, vec!["https://cdn.example.com/p.png"]);
}

#[test]
fn img_marker_with_an_absolute_path_is_attached() {
    let dir = tempfile::tempdir().expect("tempdir");
    let png = write_file(dir.path(), "shot.png", PNG_BYTES);
    let scan = extract_local_images(&format!("see <<IMG:{}>> now", png.display()), None);
    assert_eq!(scan.text, "see  now");
    assert_eq!(scan.attachments, vec![png]);
}

#[test]
fn img_marker_with_a_relative_path_and_no_base_dir_is_a_failure() {
    let scan = extract_local_images("see <<IMG:rel.png>> now", None);
    assert_eq!(scan.text, "see  now");
    assert_eq!(scan.failures.len(), 1);
    assert_eq!(scan.failures[0].raw, "rel.png");
    assert_eq!(scan.failures[0].resolved, None);
    assert_eq!(scan.failures[0].reason, LocalImageFailureReason::NotFound);
}

#[test]
fn empty_img_marker_is_dropped_without_a_failure() {
    let scan = extract_local_images("<<IMG:>>", None);
    assert_eq!(scan.text, "");
    assert!(scan.failures.is_empty());
}

/// The marker form has never had a code-span guard (unlike the markdown form
/// and the reaction directive): a `<<IMG:…>>` inside backticks is still a
/// directive, because a marker is never prose. Pinned so the asymmetry stays
/// deliberate rather than accidental.
#[test]
fn img_marker_inside_a_code_span_is_still_extracted() {
    let scan = extract_local_images("`<<IMG:https://h/x.png>>`", None);
    assert_eq!(scan.text, "``");
    assert_eq!(scan.remote, vec!["https://h/x.png"]);
}

#[test]
fn remote_schemes_are_classified_case_insensitively() {
    for raw in ["http://h/x.png", "https://h/x.png", "HTTPS://H/X.PNG"] {
        assert_eq!(
            classify_image_target(raw, None),
            ImageTarget::Remote(raw.to_string()),
            "{raw}"
        );
        assert!(is_remote_url(raw), "{raw}");
    }
    let data = "data:image/png;base64,AAAA";
    assert_eq!(
        classify_image_target(data, None),
        ImageTarget::Remote(data.to_string())
    );
    assert!(is_remote_url("DATA:image/png;base64,AAAA"));
}

#[test]
fn non_image_schemes_are_not_remote() {
    for raw in [
        "mailto:x@y.z",
        "ftp://h/x.png",
        "file:///x.png",
        "/tmp/x.png",
        "rel/x.png",
        "http:/h/x.png",
        "",
    ] {
        assert!(!is_remote_url(raw), "{raw}");
        assert_eq!(
            classify_image_target(raw, None),
            ImageTarget::Unresolved,
            "{raw}"
        );
    }
}

#[test]
fn absolute_and_tilde_paths_classify_as_local() {
    assert_eq!(
        classify_image_target("/abs/x.png", None),
        ImageTarget::Local(PathBuf::from("/abs/x.png"))
    );
    assert_eq!(
        classify_image_target("  /abs/x.png  ", None),
        ImageTarget::Local(PathBuf::from("/abs/x.png"))
    );
    match classify_image_target("~/pics/x.png", None) {
        ImageTarget::Local(path) => {
            assert!(path.is_absolute(), "{path:?}");
            assert!(path.ends_with("pics/x.png"), "{path:?}");
        }
        other => panic!("expected a local path, got {other:?}"),
    }
}

#[test]
fn relative_paths_resolve_against_the_base_dir_or_report_unresolved() {
    assert_eq!(
        classify_image_target("rel/x.png", Some(Path::new("/base"))),
        ImageTarget::Local(PathBuf::from("/base/rel/x.png"))
    );
    assert_eq!(
        classify_image_target("rel/x.png", None),
        ImageTarget::Unresolved
    );
}

#[test]
fn supported_image_signatures_are_accepted() {
    assert!(is_supported_image(b"\x89PNG\r\n\x1a\n"));
    assert!(is_supported_image(b"\xff\xd8\xff\xe0"));
    assert!(is_supported_image(b"GIF87a...."));
    assert!(is_supported_image(b"GIF89a...."));
    assert!(is_supported_image(b"RIFF\x00\x00\x00\x00WEBPVP8 "));
    assert!(is_supported_image(
        b"BM\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"
    ));
}

#[test]
fn unsupported_signatures_are_rejected() {
    assert!(!is_supported_image(b""));
    assert!(!is_supported_image(b"plain text"));
    assert!(!is_supported_image(b"PNG"));
    assert!(!is_supported_image(b"BMW short"));
    assert!(!is_supported_image(b"RIFF\x00\x00\x00\x00WEB"));
}

#[test]
fn image_extension_follows_the_signature() {
    assert_eq!(image_extension(b"\x89PNG\r\n\x1a\n"), "png");
    assert_eq!(image_extension(b"\xff\xd8\xff\xe0"), "jpg");
    assert_eq!(image_extension(b"GIF89a...."), "gif");
    assert_eq!(image_extension(b"RIFF\x00\x00\x00\x00WEBPVP8 "), "webp");
    assert_eq!(
        image_extension(b"BM\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"),
        "bmp"
    );
}

#[test]
fn validate_accepts_a_real_image_and_reports_each_rejection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let png = write_file(dir.path(), "ok.png", PNG_BYTES);
    assert_eq!(validate_local_image(&png), Ok(()));

    let missing = dir.path().join("nope.png");
    assert_eq!(
        validate_local_image(&missing),
        Err(LocalImageFailureReason::NotFound)
    );
    assert_eq!(
        validate_local_image(dir.path()),
        Err(LocalImageFailureReason::NotAFile)
    );
    let empty = write_file(dir.path(), "empty.png", b"");
    assert_eq!(
        validate_local_image(&empty),
        Err(LocalImageFailureReason::Empty)
    );
    let text = write_file(dir.path(), "text.png", b"not an image at all");
    assert_eq!(
        validate_local_image(&text),
        Err(LocalImageFailureReason::UnsupportedFormat)
    );
}

#[test]
fn failure_notice_is_none_without_failures() {
    assert!(failure_notice(&[]).is_none());
}

#[test]
fn failure_notice_names_each_reference_and_reason() {
    let failures = vec![
        LocalImageFailure {
            raw: "missing.png".to_string(),
            resolved: None,
            reason: LocalImageFailureReason::NotFound,
        },
        LocalImageFailure {
            raw: "notes.txt".to_string(),
            resolved: None,
            reason: LocalImageFailureReason::UnsupportedFormat,
        },
    ];
    let notice = failure_notice(&failures).expect("a notice");
    assert!(notice.contains("⚠️"), "{notice}");
    assert!(notice.contains("missing.png (file not found)"), "{notice}");
    assert!(
        notice.contains("notes.txt (not a supported image format"),
        "{notice}"
    );
}

#[test]
fn code_regions_marks_only_the_span_interior() {
    assert_eq!(
        code_regions("a`bc`d"),
        vec![false, false, true, true, true, false]
    );
}

#[tokio::test]
async fn data_url_decodes_and_materialises_a_local_file() {
    let url = format!("data:image/png;base64,{PNG_DATA_URL_BODY}");
    let path = fetch_remote_image(&url).await.expect("data url decodes");
    let bytes = std::fs::read(&path).expect("materialised file is readable");
    assert!(is_supported_image(&bytes));
    assert_eq!(path.extension().and_then(|e| e.to_str()), Some("png"));
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn data_url_without_a_base64_body_is_a_bad_url() {
    let err = fetch_remote_image("data:image/png,%89PNG")
        .await
        .expect_err("percent-encoded data urls are refused");
    assert_eq!(err, LocalImageFailureReason::BadUrl);
}

#[tokio::test]
async fn data_url_whose_payload_is_not_an_image_is_rejected() {
    let err = fetch_remote_image("data:image/png;base64,aGVsbG8gd29ybGQ=")
        .await
        .expect_err("non-image bytes are refused");
    assert_eq!(err, LocalImageFailureReason::UnsupportedFormat);
}

#[tokio::test]
async fn data_url_with_an_empty_payload_reports_empty() {
    let err = fetch_remote_image("data:image/png;base64,")
        .await
        .expect_err("an empty payload is refused");
    assert_eq!(err, LocalImageFailureReason::Empty);
}

#[tokio::test]
async fn local_paths_are_refused_by_the_fetch_layer() {
    let err = fetch_remote_image("/tmp/x.png")
        .await
        .expect_err("only remote references are fetchable");
    assert_eq!(err, LocalImageFailureReason::BadUrl);
}

#[tokio::test]
async fn scan_without_remote_targets_is_returned_untouched() {
    let scan = LocalImageScan {
        text: "hi".to_string(),
        attachments: vec![PathBuf::from("/tmp/a.png")],
        ..LocalImageScan::default()
    };
    let out = resolve_remote_images(scan).await;
    assert_eq!(out.text, "hi");
    assert_eq!(out.attachments, vec![PathBuf::from("/tmp/a.png")]);
    assert!(out.failures.is_empty());
}

#[tokio::test]
async fn remote_budget_marks_the_overflow_as_too_many() {
    let url = format!("data:image/png;base64,{PNG_DATA_URL_BODY}");
    let mut scan = LocalImageScan::default();
    for _ in 0..(MAX_REMOTE_IMAGES_PER_REPLY + 2) {
        scan.remote.push(url.clone());
    }
    let scan = resolve_remote_images(scan).await;
    assert_eq!(scan.attachments.len(), MAX_REMOTE_IMAGES_PER_REPLY);
    assert_eq!(scan.failures.len(), 2);
    assert!(
        scan.failures
            .iter()
            .all(|f| f.reason == LocalImageFailureReason::TooMany)
    );
    assert!(scan.remote.is_empty());
    for path in &scan.attachments {
        let _ = std::fs::remove_file(path);
    }
}
