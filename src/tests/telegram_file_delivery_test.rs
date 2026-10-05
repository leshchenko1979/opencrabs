//! Local-file delivery (#1916): what the delivery layer does with a markdown
//! link whose target is a real file on disk.
//!
//! The scanner's own contract — which references become a `LocalFile` and
//! which stay as literal text — is pinned in
//! `local_image_delivery_test.rs::local_file_links`. This file covers the
//! OTHER half, the leg that actually ships the bytes:
//!
//! 1. **The failure-entry contract.** A file the channel could not deliver
//!    comes back as an entry the caller can NAME, never as a silent drop. The
//!    two classes are distinct: a path that cannot be READ is a reference
//!    problem (`Unreadable`), while a file the API refuses is a delivery
//!    problem (`DeliveryFailed`).
//! 2. **One bubble per link, captioned by the label.** A resolved link ships
//!    exactly ONE `sendDocument` carrying the link label as its caption. This
//!    is the feature's whole promise — "render markdown for links to local
//!    files and send them to chat" — so it is asserted against the request the
//!    bot actually makes, never inferred from a rendered reply.
//!
//! Deliberately NOT covered here: the extraction rules, the regen ladder, and
//! the notice wording. Each has its own home.

use crate::channels::telegram::delivery::{
    DeliveredFile, document_part_name, file_message_link, link_file_markers, send_local_files,
};
use crate::utils::image::{LocalFile, LocalFileScan, LocalImageFailureReason};
use std::path::{Path, PathBuf};

const CHAT: i64 = 133_526_395;

/// A minimal PDF header — the bytes a real report starts with. The file family
/// has no format gate (Telegram renders no document preview, so there is
/// nothing to decode), which makes this a realistic fixture rather than a
/// required signature; nothing in the send path inspects it.
const PDF_BYTES: &[u8] = b"%PDF-1.7\n1 0 obj\n<<>>\nendobj\n";

/// A response shaped like a successful text send. Only the ROUTING is under
/// test, so the body mirrors the proven shape rather than inventing a media
/// object (same reasoning as `telegram_video_delivery_test.rs`).
const SEND_MESSAGE_OK: &str = r#"{"ok":true,"result":{"message_id":701,"date":1757166400,"chat":{"id":133526395,"type":"private"},"text":"ok"}}"#;

/// What Telegram answers for an upload it will not take.
const REFUSED: &str = r#"{"ok":false,"error_code":413,"description":"Request Entity Too Large"}"#;

/// A bot pointed at the mockito server.
///
/// MOCK PATHS ARE PASCALCASE FOR EVERY TELOXIDE REQUEST: teloxide builds the
/// method segment from the payload struct name, so `bot.send_document(..)`
/// hits `/botTESTTOKEN/SendDocument`, never the lowercase form the Bot API
/// docs use. A lowercase mock never matches — mockito serves its own unmatched
/// 501, whose empty body teloxide reports as `InvalidJson` and the send reads
/// as a network failure.
fn test_bot(server: &mockito::ServerGuard) -> teloxide::Bot {
    teloxide::Bot::with_client(
        "TESTTOKEN",
        reqwest_teloxide::Client::builder().build().unwrap(),
    )
    .set_api_url(server.url().parse().unwrap())
}

fn file_at(path: PathBuf, caption: Option<&str>) -> LocalFile {
    LocalFile {
        path,
        caption: caption.map(str::to_string),
        // `(0, 0)` is the documented "not from a scan" value (#1918).
        marker_span: (0, 0),
    }
}

/// The paths a send handed back, in the order it delivered them. Most tests
/// here are about WHICH files landed and in what order, so they read the paths
/// out of the richer return value; the message ids are asserted where they are
/// the point (the id-carrying test below).
fn delivered_paths(files: &[DeliveredFile]) -> Vec<PathBuf> {
    files.iter().map(|f| f.path.clone()).collect()
}

fn write_fixture(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write fixture");
    path
}

// ---------------------------------------------------------------------------
// the failure-entry contract
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unreadable_file_returns_a_failure_entry() {
    // The reference resolves to a path that is not there by the time the send
    // runs. The caller must be able to NAME it: a document the model announced
    // that vanishes with no notice is the #502 failure mode (silent in both
    // directions).
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("gone.pdf");
    let server = mockito::Server::new_async().await;
    let bot = test_bot(&server);

    let (delivered, failures) = send_local_files(
        uuid::Uuid::new_v4(),
        &bot,
        teloxide::types::ChatId(CHAT),
        None,
        &[file_at(missing.clone(), None)],
    )
    .await;

    assert!(delivered.is_empty(), "nothing was delivered");
    assert_eq!(failures.len(), 1, "the unreadable file must be reported");
    assert_eq!(failures[0].raw, missing.display().to_string());
    assert_eq!(
        failures[0].reason,
        LocalImageFailureReason::Unreadable,
        "a path that cannot be read is a REFERENCE problem, not a delivery one"
    );
}

#[tokio::test]
async fn a_file_the_channel_refuses_returns_a_delivery_failure() {
    // The file is readable and inside the ceiling, so the reference is fine —
    // the API is what says no. That distinction is what routes the correction:
    // a reference problem can be repaired by rewriting the link, a delivery
    // problem cannot.
    let dir = tempfile::tempdir().expect("tempdir");
    let report = write_fixture(dir.path(), "q3.pdf", PDF_BYTES);
    let mut server = mockito::Server::new_async().await;
    let document_mock = server
        .mock("POST", "/botTESTTOKEN/SendDocument")
        .with_status(413)
        .with_header("content-type", "application/json")
        .with_body(REFUSED)
        .expect(1)
        .create_async()
        .await;

    let bot = test_bot(&server);
    let (delivered, failures) = send_local_files(
        uuid::Uuid::new_v4(),
        &bot,
        teloxide::types::ChatId(CHAT),
        None,
        &[file_at(report.clone(), Some("Q3 report"))],
    )
    .await;

    document_mock.assert_async().await;
    assert!(
        delivered.is_empty(),
        "the channel refused it, so nothing was delivered"
    );
    assert_eq!(failures.len(), 1, "the refusal must be reported");
    assert_eq!(
        failures[0].reason,
        LocalImageFailureReason::DeliveryFailed,
        "a file the channel refused is a DELIVERY problem — the reference itself was fine"
    );
}

// ---------------------------------------------------------------------------
// the promise: one document bubble, captioned by the link label
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_resolved_link_ships_exactly_one_document_captioned_by_its_label() {
    // The positive control for the two tests above, and the feature's core
    // assertion at once: the instrument must be able to return an empty
    // failure list (or "it reported a failure" proves nothing about which
    // inputs fail), and the caption must ride the request as the link LABEL —
    // not the path, which the user cannot open anyway.
    let dir = tempfile::tempdir().expect("tempdir");
    let report = write_fixture(dir.path(), "q3.pdf", PDF_BYTES);
    let mut server = mockito::Server::new_async().await;
    // `expect(1)` is the whole point: a second bubble for one link is the
    // #502/#360 duplicate, one media family over. The body match pins the
    // caption, since the label is the only part of the link the user gets to
    // read in the chat.
    let document_mock = server
        .mock("POST", "/botTESTTOKEN/SendDocument")
        .match_body(mockito::Matcher::Regex("Q3 report".to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(SEND_MESSAGE_OK)
        .expect(1)
        .create_async()
        .await;

    let bot = test_bot(&server);
    let (delivered, failures) = send_local_files(
        uuid::Uuid::new_v4(),
        &bot,
        teloxide::types::ChatId(CHAT),
        None,
        &[file_at(report.clone(), Some("Q3 report"))],
    )
    .await;

    document_mock.assert_async().await;
    assert_eq!(
        delivered_paths(&delivered),
        vec![report],
        "the delivered path comes back"
    );
    assert!(failures.is_empty(), "a delivered file reports no failure");
}

#[tokio::test]
async fn a_link_without_a_label_ships_the_document_with_no_caption() {
    // `[](path)` — an empty label is not a caption. Sending `Some("")` would
    // make Telegram render an empty caption bubble; the scanner already drops
    // the empty label, and this pins that the send path does not resurrect it.
    let dir = tempfile::tempdir().expect("tempdir");
    let report = write_fixture(dir.path(), "q3.pdf", PDF_BYTES);
    let mut server = mockito::Server::new_async().await;
    let document_mock = server
        .mock("POST", "/botTESTTOKEN/SendDocument")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(SEND_MESSAGE_OK)
        .expect(1)
        .create_async()
        .await;

    let bot = test_bot(&server);
    let (delivered, failures) = send_local_files(
        uuid::Uuid::new_v4(),
        &bot,
        teloxide::types::ChatId(CHAT),
        None,
        &[file_at(report.clone(), None)],
    )
    .await;

    document_mock.assert_async().await;
    assert_eq!(delivered_paths(&delivered), vec![report]);
    assert!(failures.is_empty());
}

#[tokio::test]
async fn each_link_ships_its_own_bubble_in_order() {
    // Two links are two documents, not one — and the order the reply wrote
    // them in is the order they arrive, so the reply's prose still reads
    // top-to-bottom against the bubbles beside it.
    let dir = tempfile::tempdir().expect("tempdir");
    let first = write_fixture(dir.path(), "q3.pdf", PDF_BYTES);
    let second = write_fixture(dir.path(), "q4.pdf", PDF_BYTES);
    let mut server = mockito::Server::new_async().await;
    let document_mock = server
        .mock("POST", "/botTESTTOKEN/SendDocument")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(SEND_MESSAGE_OK)
        .expect(2)
        .create_async()
        .await;

    let bot = test_bot(&server);
    let (delivered, failures) = send_local_files(
        uuid::Uuid::new_v4(),
        &bot,
        teloxide::types::ChatId(CHAT),
        None,
        &[
            file_at(first.clone(), Some("Q3")),
            file_at(second.clone(), Some("Q4")),
        ],
    )
    .await;

    document_mock.assert_async().await;
    assert_eq!(
        delivered_paths(&delivered),
        vec![first, second],
        "delivered in the reply's order"
    );
    assert!(failures.is_empty());
}

// ---------------------------------------------------------------------------
// the document carries its real name (#1937)
// ---------------------------------------------------------------------------

#[test]
fn document_part_name_uses_the_paths_final_component() {
    // `InputFile::memory` ships no name and teloxide's fallback returns an
    // empty string, so Telegram labels the document `file`. This helper is the
    // one place that decides what name travels instead.
    assert_eq!(
        document_part_name(Path::new("/tmp/q3-report.pdf")),
        "q3-report.pdf",
        "the file's own name is the name that travels"
    );
    // A dotless name travels as-is: the name is the path's last component, not
    // a value derived from an extension.
    assert_eq!(document_part_name(Path::new("/tmp/README")), "README");
    // The path parser strips a trailing slash, so a directory reference still
    // yields its last real component rather than an empty name.
    assert_eq!(document_part_name(Path::new("/tmp/reports/")), "reports");
    // A path with no final component keeps the empty fallback — today's
    // behaviour, unchanged: the call site passes an empty name exactly as
    // before, and Telegram applies its own `file` label.
    assert_eq!(document_part_name(Path::new("/")), "");
}

#[tokio::test]
async fn the_document_request_carries_the_files_own_name() {
    // The name must REACH Telegram, not merely be computed: the multipart part
    // header is where the filename travels, and `file` is what an unnamed part
    // gets instead. Pinning the request is what makes this a delivery test
    // rather than a unit test of the helper above.
    let dir = tempfile::tempdir().expect("tempdir");
    let report = write_fixture(dir.path(), "q3-report.pdf", PDF_BYTES);
    let mut server = mockito::Server::new_async().await;
    let document_mock = server
        .mock("POST", "/botTESTTOKEN/SendDocument")
        .match_body(mockito::Matcher::Regex("q3-report\\.pdf".to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(SEND_MESSAGE_OK)
        .expect(1)
        .create_async()
        .await;

    let bot = test_bot(&server);
    let (delivered, failures) = send_local_files(
        uuid::Uuid::new_v4(),
        &bot,
        teloxide::types::ChatId(CHAT),
        None,
        &[file_at(report.clone(), Some("Q3 report"))],
    )
    .await;

    document_mock.assert_async().await;
    assert_eq!(delivered_paths(&delivered), vec![report]);
    assert!(failures.is_empty());
}

// ---------------------------------------------------------------------------
// the delivered file names the bubble it landed in (#1918)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_delivered_file_carries_the_message_id_the_send_produced() {
    // #1918: the rich plane links each `📎 <label>` marker to the bubble that
    // carries its file, so the send must hand back WHICH message it produced.
    // The id is read off the send's own response body (`SEND_MESSAGE_OK` ->
    // `message_id` 701), never invented — a wrong id would point the marker at
    // an unrelated message, which is worse than no link at all.
    let dir = tempfile::tempdir().expect("tempdir");
    let report = write_fixture(dir.path(), "q3.pdf", PDF_BYTES);
    let mut server = mockito::Server::new_async().await;
    let document_mock = server
        .mock("POST", "/botTESTTOKEN/SendDocument")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(SEND_MESSAGE_OK)
        .expect(1)
        .create_async()
        .await;

    let bot = test_bot(&server);
    let (delivered, failures) = send_local_files(
        uuid::Uuid::new_v4(),
        &bot,
        teloxide::types::ChatId(CHAT),
        None,
        &[file_at(report.clone(), Some("Q3 report"))],
    )
    .await;

    document_mock.assert_async().await;
    assert_eq!(delivered.len(), 1, "one link, one document bubble");
    assert_eq!(
        delivered[0].message_id, 701,
        "the id is the one the send produced, read off its own response"
    );
    assert_eq!(
        delivered[0].path, report,
        "and the record still names the file that bubble carries"
    );
    assert!(failures.is_empty());
}

// ---------------------------------------------------------------------------
// the message link (#1918, link part 3)
// ---------------------------------------------------------------------------

/// A supergroup — one of the two kinds a `t.me` message link exists for.
/// `is_forum` is set because that is the shape the ops forum itself has, and
/// the link's middle segment only ever appears for a topic inside one.
fn supergroup() -> teloxide::types::ChatKind {
    teloxide::types::ChatKind::Public(teloxide::types::ChatPublic {
        title: Some("l1979 ops".to_string()),
        kind: teloxide::types::PublicChatKind::Supergroup(
            teloxide::types::PublicChatSupergroup {
                username: None,
                is_forum: true,
            },
        ),
    })
}

/// A channel — the other kind that has a message link.
fn channel() -> teloxide::types::ChatKind {
    teloxide::types::ChatKind::Public(teloxide::types::ChatPublic {
        title: Some("l1979 announcements".to_string()),
        kind: teloxide::types::PublicChatKind::Channel(
            teloxide::types::PublicChatChannel { username: None },
        ),
    })
}

/// A basic (non-super) group — no message-link form exists for it.
fn basic_group() -> teloxide::types::ChatKind {
    teloxide::types::ChatKind::Public(teloxide::types::ChatPublic {
        title: Some("plain group".to_string()),
        kind: teloxide::types::PublicChatKind::Group,
    })
}

/// A one-to-one chat — no message-link form exists for it either.
fn private_chat() -> teloxide::types::ChatKind {
    teloxide::types::ChatKind::Private(teloxide::types::ChatPrivate {
        username: None,
        first_name: Some("Alexey".to_string()),
        last_name: None,
    })
}

/// The ops forum's real shape: chat id `-100…`, so the link addresses it by the
/// INTERNAL id with the `-100` marker stripped.
const FORUM_CHAT_ID: i64 = -1_001_234_567_890;

#[test]
fn a_supergroup_bubble_links_by_internal_id() {
    let link = file_message_link(&supergroup(), FORUM_CHAT_ID, None, 91_047);
    assert_eq!(
        link.as_deref(),
        Some("https://t.me/c/1234567890/91047"),
        "the marker's link drops the -100 marker and names the bubble itself"
    );
}

#[test]
fn a_topic_bubble_links_with_the_topic_in_the_middle() {
    let thread = teloxide::types::ThreadId(teloxide::types::MessageId(321));
    let link = file_message_link(&supergroup(), FORUM_CHAT_ID, Some(thread), 91_047);
    assert_eq!(
        link.as_deref(),
        Some("https://t.me/c/1234567890/321/91047"),
        "inside a forum topic the link carries the topic, so the reader lands \
         on the bubble rather than at the top of the thread"
    );
}

#[test]
fn a_channel_bubble_links_by_internal_id() {
    let link = file_message_link(&channel(), FORUM_CHAT_ID, None, 5);
    assert_eq!(link.as_deref(), Some("https://t.me/c/1234567890/5"));
}

#[test]
fn a_private_chat_and_a_basic_group_have_no_message_link() {
    assert_eq!(
        file_message_link(&private_chat(), FORUM_CHAT_ID, None, 5),
        None,
        "a private chat has no t.me message-link form"
    );
    assert_eq!(
        file_message_link(&basic_group(), FORUM_CHAT_ID, None, 5),
        None,
        "a basic group has no t.me message-link form either"
    );
}

#[test]
fn a_topic_does_not_rescue_a_kind_that_has_no_link() {
    // The thread is only ever the MIDDLE segment of a link that already exists;
    // it cannot conjure one for a kind that has no link form at all.
    let thread = teloxide::types::ThreadId(teloxide::types::MessageId(321));
    assert_eq!(
        file_message_link(&basic_group(), FORUM_CHAT_ID, Some(thread), 5),
        None
    );
}

// ---------------------------------------------------------------------------
// the marker becomes a link (#1918, link part 4)
// ---------------------------------------------------------------------------

/// A scan over `body` holding one attachment per `(path, marker)` pair, the
/// span being where that marker sits in `body`. The real scanner records exactly
/// this as it emits each marker, so a test body reads as the reply and the
/// markers as the emitted ones — no re-derivation of the scanner's own rules.
fn scan_over(body: &str, files: &[(&Path, &str)]) -> LocalFileScan {
    let mut attachments = Vec::new();
    let mut from = 0;
    for &(path, marker) in files {
        let start = body[from..]
            .find(marker)
            .expect("the test body carries the marker it names")
            + from;
        let end = start + marker.len();
        attachments.push(LocalFile {
            path: path.to_path_buf(),
            caption: Some(marker.trim_start_matches("📎 ").to_string()),
            marker_span: (start, end),
        });
        from = end;
    }
    LocalFileScan {
        text: body.to_string(),
        attachments,
        failures: Vec::new(),
    }
}

const Q3: &str = "/tmp/reports/q3-report.pdf";
const Q4: &str = "/tmp/reports/q4-report.pdf";

#[test]
fn a_delivered_file_links_its_marker() {
    let body = "Here is the report: 📎 Q3 report — thanks.";
    let scan = scan_over(body, &[(Path::new(Q3), "📎 Q3 report")]);
    let links = vec![(
        PathBuf::from(Q3),
        "https://t.me/c/1234567890/321/91047".to_string(),
    )];
    assert_eq!(
        link_file_markers(body, &scan, &links),
        "Here is the report: [📎 Q3 report](https://t.me/c/1234567890/321/91047) — thanks.",
        "the marker becomes a link and the prose around it is untouched"
    );
}

#[test]
fn each_marker_links_to_its_own_bubble() {
    let body = "First 📎 Q3 report then 📎 Q4 report.";
    let scan = scan_over(
        body,
        &[(Path::new(Q3), "📎 Q3 report"), (Path::new(Q4), "📎 Q4 report")],
    );
    let links = vec![
        (PathBuf::from(Q3), "https://t.me/c/1234567890/1001".to_string()),
        (PathBuf::from(Q4), "https://t.me/c/1234567890/1002".to_string()),
    ];
    assert_eq!(
        link_file_markers(body, &scan, &links),
        "First [📎 Q3 report](https://t.me/c/1234567890/1001) then \
         [📎 Q4 report](https://t.me/c/1234567890/1002).",
        "each marker carries the id of the bubble that file landed in — one link \
         for both would send the reader to the wrong document"
    );
}

#[test]
fn a_dm_or_an_unsent_file_keeps_the_plain_marker() {
    let body = "Report: 📎 Q3 report.";
    let scan = scan_over(body, &[(Path::new(Q3), "📎 Q3 report")]);
    // A DM has no message-link form, so the caller builds no link at all and
    // the marker is passed through untouched.
    assert_eq!(
        file_message_link(&private_chat(), FORUM_CHAT_ID, None, 91_047),
        None,
        "a DM offers no link for the caller to pass"
    );
    assert_eq!(
        link_file_markers(body, &scan, &[]),
        body,
        "with no link to pass, the marker is exactly what the scanner emitted"
    );
    // The other way it stays plain: the file never landed, so its path is absent
    // from the links even though another file's is present.
    let links = vec![(
        PathBuf::from(Q4),
        "https://t.me/c/1234567890/1001".to_string(),
    )];
    assert_eq!(
        link_file_markers(body, &scan, &links),
        body,
        "an undelivered file keeps its plain marker"
    );
}

#[test]
fn a_file_that_did_not_come_from_a_scan_has_no_marker_to_link() {
    // `(0, 0)` is the documented "not from a scan" span. A record built by hand
    // must not make the rewrite cut into the head of the text.
    let body = "Report: 📎 Q3 report.";
    let scan = LocalFileScan {
        text: body.to_string(),
        attachments: vec![file_at(PathBuf::from(Q3), Some("Q3 report"))],
        failures: Vec::new(),
    };
    let links = vec![(
        PathBuf::from(Q3),
        "https://t.me/c/1234567890/1001".to_string(),
    )];
    assert_eq!(link_file_markers(body, &scan, &links), body);
}

#[test]
fn a_marker_whose_text_moved_is_left_plain() {
    // `text` starts as a copy of the scan's own buffer and is then rewritten —
    // the artifact strip, the secret redaction, the dedup ladder. When a rewrite
    // moves the marker, the recorded span no longer names it, and splicing there
    // would cut a link over unrelated words: the marker stays plain instead.
    let body = "Report: 📎 Q3 report.";
    let scan = scan_over(body, &[(Path::new(Q3), "📎 Q3 report")]);
    let shifted = format!("(redacted) {body}");
    let links = vec![(
        PathBuf::from(Q3),
        "https://t.me/c/1234567890/1001".to_string(),
    )];
    assert_eq!(
        link_file_markers(&shifted, &scan, &links),
        shifted,
        "a span that no longer names the marker is not cut"
    );
}

// ---------------------------------------------------------------------------
// the two halves of the link leg meet (#1918)
// ---------------------------------------------------------------------------

#[test]
fn the_link_form_and_the_marker_rewrite_agree() {
    // Each half is pinned on its own above: `file_message_link` builds the
    // address, `link_file_markers` splices it in. Neither test can see the
    // SEAM — a change that made the builder emit a shape the splicer mangles
    // would leave both green. This one drives the pair the way delivery does:
    // the link the chat kind implies, over the bubble id the send produced,
    // lands in the body as one well-formed markdown link.
    let body = "Report attached: 📎 Q3 report.";
    let scan = scan_over(body, &[(Path::new(Q3), "📎 Q3 report")]);
    let delivered = [DeliveredFile {
        path: PathBuf::from(Q3),
        message_id: 91047,
    }];
    let link = file_message_link(&supergroup(), FORUM_CHAT_ID, None, delivered[0].message_id)
        .expect("a supergroup bubble has a message link");
    let links: Vec<(PathBuf, String)> = delivered
        .iter()
        .map(|file| (file.path.clone(), link.clone()))
        .collect();
    assert_eq!(
        link_file_markers(body, &scan, &links),
        "Report attached: [📎 Q3 report](https://t.me/c/1234567890/91047).",
        "the marker points at the bubble the send produced, addressed by the \
         chat kind the message arrived in"
    );
}
