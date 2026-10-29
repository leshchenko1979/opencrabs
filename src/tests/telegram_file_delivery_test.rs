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
//! 2. **One bubble per link, uncaptioned.** A resolved link ships exactly ONE
//!    `sendDocument`, and that bubble carries NO caption (#780): the caption a
//!    file carries is the link label, which the reader already has as the
//!    `📎 <label>` marker in the body directly above it — so it said the same
//!    thing twice, one line apart. This is the feature's whole promise —
//!    "render markdown for links to local files and send them to chat" — so it
//!    is asserted against the request the bot actually makes, never inferred
//!    from a rendered reply.
//!
//! Deliberately NOT covered here: the extraction rules, the regen ladder, and
//! the notice wording. Each has its own home.

use crate::channels::telegram::delivery::{
    DeliveredFile, delivered_file_links, document_part_name, file_message_link, link_file_markers,
    send_local_files,
};
use crate::utils::image::{
    LocalFile, LocalFileScan, LocalImageFailureReason, disarm_autolink, file_marker_text,
};
use crate::utils::sanitize::strip_llm_artifacts;
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
        // `None` is the documented "not from a scan" value (#1918).
        marker_span: None,
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
// the promise: one document bubble per link, and no caption on it
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_resolved_link_ships_exactly_one_document_with_no_caption() {
    // The positive control for the two tests above, and the feature's core
    // assertion at once: the instrument must be able to return an empty failure
    // list (or "it reported a failure" proves nothing about which inputs fail),
    // and the bubble must carry NO caption (#780).
    //
    // THE ABSENCE IS NOT ASSERTED BY AN `expect(0)` MOCK ALONE — that mock
    // cannot fire here, and reading its zero as a verdict is a false pass.
    // mockito picks, among the mocks a request matches, the FIRST that still
    // NEEDS hits (`is_missing_hits`, server.rs `handle_request`), and only falls
    // back to the LAST match once every one is satisfied. `expect(0)` sets
    // `expected_hits_at_most = 0`, so its `is_missing_hits` is `0 < 0` — false,
    // always. A co-matching `expect(1)` control (`0 < 1` — true) therefore
    // always wins the request, the captioned mock stays at zero, and its
    // `expect(0)` passes whether or not a caption was sent. That is why every
    // other `expect(0)` in this suite sits on a DIFFERENT path, where it is the
    // sole match.
    //
    // Both mocks below are therefore keyed so that at most ONE of them can match
    // any given request, and each fails with its own message:
    //   - the positive control matches ONLY the uncaptioned shape — the document
    //     part is the payload's LAST field, so its bytes are followed directly
    //     by the multipart terminator (`--<boundary>--`). teloxide omits a `None`
    //     field entirely (its multipart serializer makes `serialize_none`
    //     `unimplemented!`: "`None`s are not serialized"), so a caption would
    //     push a part in between and the pattern stops matching;
    //   - the captioned mock matches the label text, which reaches the wire only
    //     if a caption was actually attached.
    // A captioned send therefore fails BOTH: the control sees zero requests, and
    // the captioned mock sees one.
    let dir = tempfile::tempdir().expect("tempdir");
    let report = write_fixture(dir.path(), "q3.pdf", PDF_BYTES);
    let mut server = mockito::Server::new_async().await;
    // The caption a file carries IS the link label, and the reader already has
    // the label as the `📎 <label>` marker in the body directly above this
    // bubble — so a caption here would print the same string twice, one line
    // apart.
    let captioned_mock = server
        .mock("POST", "/botTESTTOKEN/SendDocument")
        .match_body(mockito::Matcher::Regex("Q3 report".to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(SEND_MESSAGE_OK)
        .expect(0)
        .create_async()
        .await;
    // The positive control: exactly ONE bubble for one link, and it carries the
    // document as the LAST part. A second bubble is the #502/#360 duplicate, one
    // media family over. `endobj\n` is the tail of the PDF fixture above; the
    // terminator right after it is what a caption would displace.
    let document_mock = server
        .mock("POST", "/botTESTTOKEN/SendDocument")
        .match_body(mockito::Matcher::Regex(
            r"endobj\n\r\n--[0-9A-Za-z_-]+--".to_string(),
        ))
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
    captioned_mock.assert_async().await;
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

/// The ids that decide the link form (#771).
///
/// The `-100` prefix is what marks a chat as a supergroup or a channel — the two
/// kinds a `t.me` message link exists for — so the fixtures are IDS, not
/// `ChatKind`s: the delivery layer reads the id, and that is exactly what the
/// resume path still has once the message that carried the kind is gone.
const FORUM_CHAT_ID: i64 = -1_001_234_567_890;
/// A one-to-one chat: positive id, so no message-link form.
const DM_CHAT_ID: i64 = 133_526_395;
/// A basic (non-super) group: negative but WITHOUT the `-100` marker, so no
/// message-link form either.
const BASIC_GROUP_CHAT_ID: i64 = -987_654_321;

#[test]
fn a_supergroup_bubble_links_by_internal_id() {
    let link = file_message_link(FORUM_CHAT_ID, None, 91_047);
    assert_eq!(
        link.as_deref(),
        Some("https://t.me/c/1234567890/91047"),
        "the marker's link drops the -100 marker and names the bubble itself"
    );
}

#[test]
fn a_topic_bubble_links_with_the_topic_in_the_middle() {
    let thread = teloxide::types::ThreadId(teloxide::types::MessageId(321));
    let link = file_message_link(FORUM_CHAT_ID, Some(thread), 91_047);
    assert_eq!(
        link.as_deref(),
        Some("https://t.me/c/1234567890/321/91047"),
        "inside a forum topic the link carries the topic, so the reader lands \
         on the bubble rather than at the top of the thread"
    );
}

#[test]
fn a_channel_links_by_the_same_rule_as_a_supergroup() {
    // #771: a channel and a supergroup are indistinguishable by id — both carry
    // the `-100` marker — and they share one link form, so one rule covers both.
    // The old `ChatKind` match spelled the two arms out separately and produced
    // the same URL either way; the id states it once.
    let link = file_message_link(FORUM_CHAT_ID, None, 5);
    assert_eq!(link.as_deref(), Some("https://t.me/c/1234567890/5"));
}

#[test]
fn a_dm_and_a_basic_group_have_no_message_link() {
    // #802, the owner report: "I saw the links… when I click them nothing
    // happens". Two rounds of learning live in this one assertion.
    //
    // The first round read that report as "the DM marker should be a link too",
    // and `t.me/<bot_username>/<message_id>` looks like the form for it: the URL
    // answers `200` and carries a `tg://resolve?domain=<bot>&post=<id>` page for
    // ANY id. That `200` proves the shape parses, not that a message sits there
    // — a bot DM has no public preview, so every id returns the same page. The
    // reachability read is MTProto, and in the owner's DM it answered "Message
    // not found or inaccessible" for the very id the Bot API had just returned.
    //
    // The reason is that a bot DM carries TWO message-id spaces. Measured
    // 2026-10-29, one bubble read from both sides: `sendMessage` answered
    // `message_id 152540` for chat `133526395`, while MTProto from the owner's
    // account has that same message at `975245` — and the gap DRIFTS between
    // probes (822701, then 822705), so no arithmetic recovers one from the
    // other. `t.me/<bot>/<id>` resolves its id in the CLIENT's space, which a
    // bot never sees. A DM therefore has no link this side can build, and the
    // marker stays plain text: the document bubble below it — carrying the real
    // file name — is the tappable affordance.
    assert_eq!(
        file_message_link(DM_CHAT_ID, None, 5),
        None,
        "a DM's client-space id is not the id a bot is given, so no link is \
         offered rather than one that goes nowhere"
    );
    // A basic group has no form either: it is not a `-100` chat, and a bot's own
    // history does not address it.
    assert_eq!(
        file_message_link(BASIC_GROUP_CHAT_ID, None, 5),
        None,
        "a basic group has no t.me message-link form"
    );
    // The thread is only ever the MIDDLE segment of a link that already exists;
    // it cannot conjure one for a chat that has no link form at all.
    let thread = teloxide::types::ThreadId(teloxide::types::MessageId(321));
    assert_eq!(
        file_message_link(BASIC_GROUP_CHAT_ID, Some(thread), 5),
        None,
        "a topic cannot rescue a chat that has no link form"
    );
}

#[test]
fn a_delivered_file_links_with_no_inbound_message() {
    // #771, the regression: this is the whole point of the fix. The old build
    // site was `inbound.map(|message| …)`, so a resumed turn — which has no
    // message — produced NO links and every `📎` marker stayed plain even though
    // the document bubble sat right below it. The links now derive from the chat
    // id alone, so the same call that a live turn makes works here with nothing
    // but the delivered ids.
    let delivered = [
        DeliveredFile {
            path: PathBuf::from(Q3),
            message_id: 91_047,
        },
        DeliveredFile {
            path: PathBuf::from(Q4),
            message_id: 91_048,
        },
    ];
    assert_eq!(
        delivered_file_links(&delivered, FORUM_CHAT_ID, None),
        vec![
            (
                PathBuf::from(Q3),
                "https://t.me/c/1234567890/91047".to_string()
            ),
            (
                PathBuf::from(Q4),
                "https://t.me/c/1234567890/91048".to_string()
            ),
        ],
        "a resume turn has the chat id and the delivered ids, and that is all a \
         link needs — the inbound message was never the input it wanted"
    );
    // And the same list, spliced by the SAME rewrite the floor uses, puts the
    // link in the body: the two halves of the leg agree on the resume path too.
    let body = "Reports: 📎 Q3 report and 📎 Q4 report.";
    let scan = scan_over(
        body,
        &[(Path::new(Q3), "📎 Q3 report"), (Path::new(Q4), "📎 Q4 report")],
    );
    assert_eq!(
        link_file_markers(
            body,
            &scan,
            &delivered_file_links(&delivered, FORUM_CHAT_ID, None)
        ),
        "Reports: [📎 Q3 report](https://t.me/c/1234567890/91047) and \
         [📎 Q4 report](https://t.me/c/1234567890/91048).",
        "with no inbound message the markers still point at the bubbles that \
         hold the files"
    );
    // A DM gets no link at all (#802) — see
    // `a_dm_and_a_basic_group_have_no_message_link` for the measurement that
    // settled it. This fix removes the message dependency; it does not invent an
    // address this side cannot give.
    assert!(delivered_file_links(&delivered, DM_CHAT_ID).is_empty());
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
            marker_span: Some(start..end),
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
    // A DM has no address this side can give (#802), so the caller builds no
    // link and the marker is passed through untouched.
    assert_eq!(
        file_message_link(DM_CHAT_ID, None, 91_047),
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
fn a_marker_that_moved_still_links() {
    // #773: the recorded span is a record of WHAT the scanner wrote, not of
    // WHERE it now sits. `text` starts as a copy of the scan's own buffer and
    // is then rewritten — the artifact strip, the secret redaction, the dedup
    // ladder — and any of those can move a marker. The old guard compared the
    // recorded span against the marker's bytes, read the mismatch as "the
    // marker is gone" and left it plain; on a reply carrying an em dash that
    // was the norm, not the exception (#1745). Searching for the marker's own
    // bytes finds it where it actually is.
    let body = "Report: 📎 Q3 report.";
    let scan = scan_over(body, &[(Path::new(Q3), "📎 Q3 report")]);
    let moved = format!("(redacted) {body}");
    let links = vec![(
        PathBuf::from(Q3),
        "https://t.me/c/1234567890/1001".to_string(),
    )];
    assert_eq!(
        link_file_markers(&moved, &scan, &links),
        "(redacted) Report: [📎 Q3 report](https://t.me/c/1234567890/1001).",
        "the span is stale but the marker is still there exactly once, so it \
         links to the bubble that holds the file"
    );
}

#[test]
fn a_marker_whose_text_moved_is_left_plain() {
    // The other half of the same rule: a rewrite that does not merely move the
    // marker but REMOVES it — the redaction pass, the dedup ladder — leaves no
    // bytes to find, and nothing may be spliced over whatever took their place.
    // The refusal the old guard made for the wrong reason is still the right
    // answer here.
    let body = "Report: 📎 Q3 report.";
    let scan = scan_over(body, &[(Path::new(Q3), "📎 Q3 report")]);
    let redacted = body.replace("📎 Q3 report", "[redacted]");
    let links = vec![(
        PathBuf::from(Q3),
        "https://t.me/c/1234567890/1001".to_string(),
    )];
    assert_eq!(
        link_file_markers(&redacted, &scan, &links),
        redacted,
        "no marker bytes survive the rewrite, so there is nothing to link"
    );
}

#[test]
fn a_marker_after_a_real_rewrite_still_links() {
    // The live defect (#773) driven through the REAL transform rather than a
    // hand-made shift. `strip_llm_artifacts` runs on every delivered reply and
    // its `normalize_dashes` pass rewrites " —" to ":" (#1745), SHORTENING the
    // buffer, so every span past the first em dash goes stale by exactly the
    // bytes the dash gave up. Measured on this lane's own smoke reply: scan
    // text 1165 B with the marker at span [889, 911), 1153 B after the
    // normalize — the marker really at 885 — while the guard read byte 889.
    let scan_text = "Here is the report — the one you asked for: 📎 Q3 report.";
    let scan = scan_over(scan_text, &[(Path::new(Q3), "📎 Q3 report")]);
    let rewritten = strip_llm_artifacts(scan_text);

    assert_ne!(rewritten, scan_text, "the transform has to actually fire");
    assert!(
        rewritten.len() < scan_text.len(),
        "the em dash gives up bytes, so the buffer shortens: {rewritten:?}"
    );
    // The span the scanner recorded no longer names the marker — the exact
    // condition the old guard refused on, so this test fails on the pre-fix
    // code instead of passing for the wrong reason.
    let span = scan.attachments[0]
        .marker_span
        .clone()
        .expect("the scan records the span it emitted the marker at");
    let real = rewritten
        .find("📎 Q3 report")
        .expect("the marker itself survives the rewrite");
    assert!(
        real < span.start,
        "the rewrite moved the marker to {real}, left of the recorded span {span:?}"
    );
    let links = vec![(
        PathBuf::from(Q3),
        "https://t.me/c/1234567890/1001".to_string(),
    )];
    assert_eq!(
        link_file_markers(&rewritten, &scan, &links),
        "Here is the report: the one you asked for: \
         [📎 Q3 report](https://t.me/c/1234567890/1001).",
        "the marker links even though the transform moved it out from under its \
         own span"
    );
}

#[test]
fn an_ambiguous_marker_is_left_plain() {
    // #2001: the model can write the marker's own bytes into its prose — the
    // very `📎 <label>` the scanner emits for the file it attached. Two
    // occurrences are indistinguishable by search, and the one the recorded
    // span names is not necessarily the scanner's, so cutting the first hit
    // would drop a document link over the model's own words. Both stay plain.
    let body = "I wrote 📎 Q3 report in the draft. Attached: 📎 Q3 report";
    let marker = "📎 Q3 report";
    // The span names the marker the SCANNER emitted — the second one; the
    // model's copy sits earlier in the prose.
    let emitted_at = body.rfind(marker).expect("the body carries it twice");
    let scan = LocalFileScan {
        text: body.to_string(),
        attachments: vec![LocalFile {
            path: PathBuf::from(Q3),
            caption: Some("Q3 report".to_string()),
            marker_span: Some(emitted_at..emitted_at + marker.len()),
        }],
        failures: Vec::new(),
    };
    let links = vec![(
        PathBuf::from(Q3),
        "https://t.me/c/1234567890/1001".to_string(),
    )];
    assert_eq!(
        link_file_markers(body, &scan, &links),
        body,
        "an ambiguous marker is not cut — the link would land on whichever copy \
         the search happened to reach first"
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
    let link = file_message_link(FORUM_CHAT_ID, None, delivered[0].message_id)
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

// ---------------------------------------------------------------------------
// the marker is not a domain (#1938)
// ---------------------------------------------------------------------------

#[test]
fn a_dotted_label_does_not_autolink_as_a_domain() {
    // The live specimen: the marker `📎 1918-fix-state.md` was rendered by the
    // Telegram client as a link to the Moldova ccTLD `1918-fix-state.md`. The
    // dot is followed by a zero-width space, which no renderer shows ink for
    // but which breaks the client's `name.tld` pattern.
    let out = file_marker_text("1918-fix-state.md", "/tmp/1918-fix-state.md");
    assert_eq!(
        out, "1918-fix-state.\u{200b}md",
        "the dot the client keys on is followed by an invisible breaker"
    );
    assert!(
        !out.contains(".md"),
        "no bare `.md` survives for the client's TLD detector to match"
    );
    assert_eq!(
        out.replace('\u{200b}', ""),
        "1918-fix-state.md",
        "the breaker is invisible: strip it and the reader's own words are back"
    );
}

#[test]
fn a_label_without_a_dotted_token_is_left_alone() {
    // Nothing to disarm: the disarmer must not touch a marker it has no reason
    // to change, or every label would carry noise.
    assert_eq!(file_marker_text("Q3 report", "/tmp/q3-report.pdf"), "Q3 report");
    assert_eq!(file_marker_text("notes", "/tmp/notes"), "notes");
}

#[test]
fn a_trailing_dot_is_not_disarmed() {
    // `sentence.` is not a domain: the dot precedes a space or the end of the
    // string, so there is no TLD to break. The two disarmed arms below are the
    // ONLY direct controls on `disarm_autolink`'s positive branch — the
    // scan-level tests reach it only through a marker, so the bare helper's
    // multi-dot and version-number shapes are pinned here and nowhere else.
    assert_eq!(disarm_autolink("the report."), "the report.");
    assert_eq!(disarm_autolink("a.b.c"), "a.\u{200b}b.\u{200b}c");
    assert_eq!(disarm_autolink("v1.2"), "v1.\u{200b}2");
}

#[test]
fn a_disarmed_marker_survives_the_link_splice() {
    // The seam: the HTML plane rewrites the marker in place into a markdown
    // link. The breaker rides in the LABEL — the words between the brackets —
    // so the address is untouched and the client still cannot autolink the
    // label it is handed. Kept as the only test that carries a ZWSP THROUGH
    // `link_file_markers`; the other link tests use a clean label, so a splice
    // that stripped the breaker would pass every one of them.
    let label = file_marker_text("1918-fix-state.md", "/tmp/1918-fix-state.md");
    let body = format!("State: 📎 {label}");
    let scan = scan_over(&body, &[(Path::new(Q3), &format!("📎 {label}"))]);
    let links = vec![(
        PathBuf::from(Q3),
        "https://t.me/c/3936827469/91639".to_string(),
    )];
    assert_eq!(
        link_file_markers(&body, &scan, &links),
        "State: [📎 1918-fix-state.\u{200b}md](https://t.me/c/3936827469/91639)",
        "the marker becomes a link and the label inside it stays disarmed"
    );
}

// ---------------------------------------------------------------------------
// the fallback links a marker to an intermediate's bubble (#1939)
// ---------------------------------------------------------------------------

#[test]
fn the_fallback_links_a_marker_to_the_intermediates_bubble() {
    // When the rich plane owned the documents, the final leg's `delivered` list
    // is EMPTY — nothing was sent there — so the floor builds no link and the
    // `📎` marker would stay plain even though the document IS in the chat. Its
    // address is the intermediate's own bubble, recovered by
    // `intermediate_file_links`. This drives the whole leg the way delivery
    // does: recover the pair, build the chat kind's link over the intermediate's
    // id, and splice it with the SAME rewrite the floor uses.
    use crate::channels::telegram::flow::SentBubble;
    use crate::channels::telegram::intermediates::intermediate_file_links;

    let body = "Report: 📎 Q3 report.";
    let scan = scan_over(body, &[(Path::new(Q3), "📎 Q3 report")]);
    // The promoted intermediate delivered the document inside its own media
    // array, so it records EMPTY ids (#617) and the id of the bubble it sent.
    let intermediate = SentBubble {
        text: body.to_string(),
        ids: Vec::new(),
        delivered_files: vec![(PathBuf::from(Q3), 91_047)],
    };
    let recovered = intermediate_file_links(std::slice::from_ref(&intermediate), body);
    assert_eq!(recovered, vec![(PathBuf::from(Q3), 91_047)]);

    let links: Vec<(PathBuf, String)> = recovered
        .into_iter()
        .filter_map(|(path, id)| {
            file_message_link(FORUM_CHAT_ID, None, id).map(|link| (path, link))
        })
        .collect();
    assert_eq!(
        link_file_markers(body, &scan, &links),
        "Report: [📎 Q3 report](https://t.me/c/1234567890/91047).",
        "the marker points at the bubble that actually holds the document"
    );

    // A DM has no message-link form (#802), so the same recovery yields no link
    // and the marker stays plain rather than pointing nowhere.
    let dm_links: Vec<(PathBuf, String)> = vec![(PathBuf::from(Q3), 91_047)]
        .into_iter()
        .filter_map(|(path, id)| {
            file_message_link(DM_CHAT_ID, None, id).map(|link| (path, link))
        })
        .collect();
    assert!(dm_links.is_empty());
    assert_eq!(link_file_markers(body, &scan, &dm_links), body);
}

// ---------------------------------------------------------------------------
// the intermediate's document floor (#779)
// ---------------------------------------------------------------------------

#[test]
fn every_file_the_rich_plane_cannot_take_still_has_a_delivery_leg() {
    // #779 — the dead anchor the owner hit on bubble 93966; #801 — why it can
    // never be repaired by teaching the rich plane to carry the file. A
    // markdown reference comes back from the file walk as the plain
    // `📎 <label>` marker and records NO rich entry. Every file list the
    // intermediate built was derived from those entries, so the document had no
    // leg on EITHER plane: the marker was emitted with nothing behind it and no
    // address to link to.
    //
    // #1968 detached markdown alone, on the narrow ground that Android will not
    // open an inlined `.md`; #801 widened the rule to every document kind,
    // because Telegram answers a `{"type":"document"}` media entry with
    // `ok:true` and then delivers nothing at all (measured 2026-10-28, five
    // multipart probes — only `photo` attached). The skipped set is therefore
    // EVERY resolved file, and the three arms below are the predicate's whole
    // contract: what the rich plane did not take AND the chat does not already
    // hold.
    use crate::channels::telegram::intermediates::files_the_rich_plane_skipped;
    use crate::utils::image::{extract_local_files, rewrite_local_files};

    let dir = tempfile::tempdir().expect("tempdir");
    let doc = write_fixture(
        dir.path(),
        "harvest-workorder.md",
        b"# work order\n\nNothing is blocked on you.\n",
    );
    let text = format!(
        "Work order, full:\n[harvest-workorder.md]({})",
        doc.display()
    );

    // The marker's visible text is the label passed through `disarm_autolink`,
    // which inserts a ZWSP after every `.` that precedes an ASCII alphanumeric
    // (#1938) so a dotted filename does not autolink as a domain. The `.md` in
    // this label is exactly that shape, so the expected marker carries the ZWSP
    // — asserting the raw label would fail on the byte that makes it a filename.
    let marker = "📎 harvest-workorder.\u{200b}md";

    // The plane-independent read: the scan resolves the reference and replaces
    // it with the marker the reader actually sees.
    let scan = extract_local_files(&text, Some(dir.path()));
    assert_eq!(scan.attachments.len(), 1, "the scan resolves the document");
    assert!(
        scan.text.contains(marker),
        "the scan emits the visible marker, got {:?}",
        scan.text
    );

    // The rich walk: the marker survives and no entry is recorded — the exact
    // asymmetry that produced the dead anchor, and the ONLY shape this walk can
    // ever produce now.
    let rich = rewrite_local_files(&text, Some(dir.path()), &[]);
    assert!(
        rich.entries.is_empty(),
        "the rich plane can never take a document, so it records no entry"
    );
    assert!(
        rich.rich.contains(marker),
        "the rich body carries the marker, got {:?}",
        rich.rich
    );
    assert!(
        !rich.rich.contains("tg://"),
        "and no reference: nothing on the rich plane answers one, got {:?}",
        rich.rich
    );

    // ARM 1 — the bug. A list built from `entries` is empty here, so the
    // document would ship nowhere and the marker above would stay plain.
    let skipped = files_the_rich_plane_skipped(&scan, &rich.entries, &[]);
    assert_eq!(
        skipped.len(),
        1,
        "a document the rich plane skipped must still have a delivery leg"
    );
    assert_eq!(skipped[0].path, doc, "and it is the resolved document");

    // ARM 2 — the retired #1968 arm, kept as the guard on the widening: this
    // walk has ONE behaviour, so the skipped set does not depend on any body or
    // flag the caller might have passed. A mutant that reintroduces an
    // inlining-on path records an entry here and orphans the document.
    let again = rewrite_local_files(&rich.rich, Some(dir.path()), &[]);
    assert!(
        again.entries.is_empty(),
        "the walk is idempotent: no arm may ever record a document entry"
    );
    assert_eq!(
        files_the_rich_plane_skipped(&scan, &again.entries, &[]).len(),
        1,
        "and the floor still owns the file after a second pass"
    );

    // ARM 3 — the turn's delivered-media list. This is not only the
    // #502/#1916 double-send rule: the REWRITER's own `already_delivered` leg
    // consumes such a reference and records nothing, so without this filter the
    // same path would come back as a skipped file and ship twice.
    let pre_delivered = rewrite_local_files(&text, Some(dir.path()), std::slice::from_ref(&doc));
    assert!(
        pre_delivered.entries.is_empty() && !pre_delivered.rich.contains(marker),
        "a reference the rewriter consumed as already delivered leaves neither marker nor entry"
    );
    assert!(
        files_the_rich_plane_skipped(&scan, &pre_delivered.entries, std::slice::from_ref(&doc))
            .is_empty(),
        "a path already in the chat must not ship again"
    );
}
