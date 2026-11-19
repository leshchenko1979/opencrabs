//! #826 AC-3: one refused media entry must not cost the whole media plane.
//!
//! Measured 2026-11-12 on this box, a reply carrying six documents and two
//! pictures was refused with `400 IMAGE_PROCESS_FAILED`, the sender downgraded
//! the WHOLE reply to the HTML dialect, and the reader got a second bubble in
//! which every document was a detached `📎` and both pictures were gone.
//!
//! Two facts from that log drive the fix and these tests:
//!
//! * the refusal is about ONE entry, and Telegram names a family, never an
//!   index — so the repair has to try entries rather than read an index out of
//!   the error;
//! * the identical retry is a HEDGE, and was mis-described here as a measured
//!   flake until 2026-11-13: the send that succeeded 0.9 s after the refusal
//!   was the HTML dialect (`len=2588`, no media array), not the refused body
//!   (`len=2176`) re-sent, so that gap measured the fallback. Nothing has
//!   measured `IMAGE_PROCESS_FAILED` as transient, and the one case blamed on
//!   it was a truncated PNG Telegram was right to refuse (#829, withdrawn).
//!   The retry is kept as cheap insurance and the tests pin the ORDER of the
//!   legs, not a flake theory.
//!
//! The classifier and the sacrifice order are pure, so they are pinned directly;
//! the ladder itself is pinned against a scripted HTTP server that answers a
//! refusal first and records every request body it is given.

use crate::channels::telegram::rich::api::{
    SacrificedEntry, is_media_entry_rejection, sacrifice_order, send_rich_with_media_target_id,
    unactionable_family,
};
use crate::channels::telegram::rich::mermaid::{MediaEntry, MediaKind};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x01\x02\x03\x04\x05\x06\x07";
const PDF_BYTES: &[u8] = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n<<>>\n%%EOF\n";

fn photo(id: &str) -> MediaEntry {
    MediaEntry {
        id: id.to_string(),
        url: None,
        bytes: Some(PNG_BYTES.to_vec()),
        kind: MediaKind::Photo,
        name: None,
    }
}

fn document(id: &str, name: &str) -> MediaEntry {
    MediaEntry {
        id: id.to_string(),
        url: None,
        bytes: Some(PDF_BYTES.to_vec()),
        kind: MediaKind::Document,
        name: Some(name.to_string()),
    }
}

/// A refusal that names a media entry, and the three that do not.
#[test]
fn only_a_media_entry_rejection_is_repairable_by_the_array() {
    for err in [
        "Telegram rich API error (400 Bad Request): Bad Request: IMAGE_PROCESS_FAILED",
        "Telegram rich API error (400 Bad Request): Bad Request: RICH_MESSAGE_PHOTO_INVALID",
        "Telegram rich API error (400 Bad Request): Bad Request: RICH_MESSAGE_PHOTO_NO_MEDIA_FOUND",
        "Telegram rich API error (400 Bad Request): Bad Request: RICH_MESSAGE_VIDEO_INVALID",
        "Telegram rich API error (400 Bad Request): Bad Request: RICH_MESSAGE_DOCUMENT_INVALID",
    ] {
        assert!(
            is_media_entry_rejection(err),
            "a refusal that names an entry must be repairable: {err}"
        );
    }
    for err in [
        "Telegram rich API error (400 Bad Request): Bad Request: RICH_MESSAGE_CONTENT_REQUIRED",
        "Telegram rich API error (400 Bad Request): Bad Request: can't parse entities",
        "Telegram rich API error (401 Unauthorized): Unauthorized",
    ] {
        assert!(
            !is_media_entry_rejection(err),
            "a refusal about the message body is not repaired by dropping media: {err}"
        );
    }
}

/// The first entry sacrificed is one of the family the error names.
#[test]
fn the_sacrificed_entry_comes_from_the_family_the_error_names() {
    let media = vec![
        document("doc0", "a.pdf"),
        photo("img0"),
        document("doc1", "b.pdf"),
    ];

    let order = sacrifice_order(&media, "Bad Request: IMAGE_PROCESS_FAILED");
    assert_eq!(
        order.first().copied(),
        Some(1),
        "a refused picture must be sacrificed before the documents travelling \
         beside it: {order:?}"
    );

    let order = sacrifice_order(&media, "Bad Request: RICH_MESSAGE_DOCUMENT_INVALID");
    assert_eq!(
        order.first().copied(),
        Some(0),
        "a document rejection must sacrifice a document first: {order:?}"
    );

    // Every index appears exactly once, so a retry can never silently skip an
    // entry that the next attempt would have needed.
    let mut sorted = order.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, vec![0, 1, 2], "the order must be a permutation");
}

// ── the ladder, against a scripted server ────────────────────────────────

/// Serve `statuses.len()` requests in order, capturing each full request
/// (head + body, body bounded by `Content-Length`) and replying with the
/// scripted status. `message_id` is handed out on a success.
async fn serve_script(listener: TcpListener, statuses: Vec<(u16, &'static str)>) -> Vec<Vec<u8>> {
    let mut captured: Vec<Vec<u8>> = Vec::new();
    for (index, (status, description)) in statuses.iter().enumerate() {
        let (mut sock, _) = listener.accept().await.expect("accept");
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        // Read until the head is complete AND the declared body has arrived.
        let request = loop {
            let read = tokio::time::timeout(Duration::from_secs(10), sock.read(&mut chunk)).await;
            let read = match read {
                Ok(Ok(n)) => n,
                Ok(Err(_)) | Err(_) => break buf.clone(),
            };
            if read == 0 {
                break buf.clone();
            }
            buf.extend_from_slice(&chunk[..read]);
            if let Some(head_end) = find_head_end(&buf) {
                let declared = declared_content_length(&buf[..head_end]);
                if buf.len() >= head_end + declared {
                    break buf.clone();
                }
            }
        };
        captured.push(request);

        let body = if *status == 200 {
            format!(r#"{{"ok":true,"result":{{"message_id":{}}}}}"#, 900 + index)
        } else {
            format!(r#"{{"ok":false,"error_code":{status},"description":"{description}"}}"#)
        };
        let resp = format!(
            "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            if *status == 200 { "OK" } else { "Bad Request" },
            body.len()
        );
        sock.write_all(resp.as_bytes()).await.expect("write");
        sock.flush().await.ok();
    }
    captured
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

fn declared_content_length(head: &[u8]) -> usize {
    let text = String::from_utf8_lossy(head).to_ascii_lowercase();
    for line in text.split("\r\n") {
        if let Some(rest) = line.strip_prefix("content-length:")
            && let Ok(n) = rest.trim().parse::<usize>()
        {
            return n;
        }
    }
    0
}

/// The identical retry: a refusal that clears on its own costs nothing at all.
#[tokio::test]
async fn a_refused_body_is_retried_identically_before_anything_is_dropped() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve_script(
        listener,
        vec![(400, "Bad Request: IMAGE_PROCESS_FAILED"), (200, "")],
    ));

    let media = vec![document("doc0", "a.pdf"), photo("img0")];
    let (id, sacrificed) = send_rich_with_media_target_id(
        &format!("http://127.0.0.1:{port}"),
        "TOKEN",
        4242,
        None,
        None,
        "![a.pdf](tg://document?id=doc0) and ![p](tg://photo?id=img0)",
        &media,
        "test",
        "-",
    )
    .await
    .expect("the identical retry must land the same body");

    let captured = tokio::time::timeout(Duration::from_secs(30), server)
        .await
        .expect("the scripted server must see every request")
        .expect("server task");
    assert_eq!(captured.len(), 2, "one refusal, one retry");
    assert!(id > 0, "the retry's own message id is returned");
    // #897 AC-3: the hedge path sacrifices nothing, so it must report nothing
    // — an empty list is what keeps a clean send from firing a false nudge.
    assert!(
        sacrificed.is_empty(),
        "a send that dropped no entry must report no sacrifice: {sacrificed:?}"
    );
    // Nothing was sacrificed: both entries still ride the second request.
    let second = String::from_utf8_lossy(&captured[1]);
    assert!(
        second.contains("attach://doc0") && second.contains("attach://img0"),
        "the identical retry must carry the SAME media array, not a reduced one"
    );
}

/// The sacrifice leg: a structural refusal costs one entry, never the plane.
///
/// The script refuses ONCE and then accepts, which is the measured shape this
/// whole issue is about: one entry is the problem, and the message is fine
/// without it. Refusing twice is a different question — two entries are then
/// implicated and the ladder spends its second sacrifice — so pinning the
/// happy repair here keeps the assertion about the entry that was actually
/// named.
#[tokio::test]
async fn a_document_rejection_costs_one_document_and_not_the_whole_message() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve_script(
        listener,
        vec![
            (400, "Bad Request: RICH_MESSAGE_DOCUMENT_INVALID"),
            (200, ""),
        ],
    ));

    let media = vec![
        document("doc0", "a.pdf"),
        document("doc1", "b.pdf"),
        photo("img0"),
    ];
    let (id, sacrificed) = send_rich_with_media_target_id(
        &format!("http://127.0.0.1:{port}"),
        "TOKEN",
        4242,
        None,
        None,
        "![a.pdf](tg://document?id=doc0) ![b.pdf](tg://document?id=doc1) ![p](tg://photo?id=img0)",
        &media,
        "test",
        "-",
    )
    .await
    .expect("a refusal about ONE entry must not sink the message");

    let captured = tokio::time::timeout(Duration::from_secs(30), server)
        .await
        .expect("the scripted server must see every request")
        .expect("server task");
    assert_eq!(captured.len(), 2, "one refusal, one sacrifice");
    assert!(id > 0);
    // #897 AC-1: the entry the ladder dropped must come back with the id,
    // named exactly as the retry log names it. Before #897 this list died at
    // the `tracing::warn!` and the caller saw a plain success.
    assert_eq!(
        sacrificed,
        vec![SacrificedEntry {
            id: "doc0".to_string(),
            name: Some("a.pdf".to_string()),
            kind: MediaKind::Document,
        }],
        "the sacrificed document must be reported with its id, name and family"
    );

    let second = String::from_utf8_lossy(&captured[1]);
    assert!(
        !second.contains("attach://doc0"),
        "the sacrificed document must not ride the retry: {second}"
    );
    assert!(
        second.contains("attach://doc1") && second.contains("attach://img0"),
        "every entry that was NOT implicated must still ride the retry"
    );
    assert!(
        second.contains("tg:document?id=doc0"),
        "the dead reference must be neutralised, or the retry 400s on it (§3.3, #334)"
    );
    assert!(
        second.contains("![b.pdf](tg://document?id=doc1)"),
        "the surviving document's own reference must be left byte-identical"
    );
}

// ── #896: a family the message does not carry is unactionable ────────────

/// The gate itself: the named family must be PRESENT for a sacrifice to test it.
///
/// Measured 2026-11-18T20:41:26.873754Z, chat `-1003936827469` thread `36841`: a
/// document-only reply was refused `RICH_MESSAGE_PHOTO_INVALID`, the ladder
/// sacrificed `doc0 (passive-recall-precision.md)`, and the retry landed
/// `msg=100166` at `len=2828` against the refused `len=2829` — one byte lighter,
/// i.e. the document reference was the only thing removed. The reader got a dead
/// `📄` where the attachment should have been, and Telegram had never named a
/// document at all.
#[test]
fn a_family_the_message_does_not_carry_is_unactionable() {
    let docs_only = vec![document("doc0", "a.pdf"), document("doc1", "b.pdf")];

    assert_eq!(
        unactionable_family(&docs_only, "Bad Request: RICH_MESSAGE_PHOTO_INVALID"),
        Some(MediaKind::Photo),
        "a PHOTO_ refusal on a message with no photo entry names a family that is \
         not here — there is nothing to sacrifice and nothing to learn"
    );
    assert_eq!(
        sacrifice_order(&docs_only, "Bad Request: RICH_MESSAGE_PHOTO_INVALID"),
        Vec::<usize>::new(),
        "an unactionable family must yield an EMPTY order, not a list of innocent \
         documents to burn"
    );

    // The gate must not fire when the named family IS present — that is the
    // #826 repair, and narrowing must not undo it.
    let with_photo = vec![document("doc0", "a.pdf"), photo("img0")];
    assert_eq!(
        unactionable_family(&with_photo, "Bad Request: RICH_MESSAGE_PHOTO_INVALID"),
        None,
        "a photo refusal on a message that HAS a photo is actionable"
    );
    assert_eq!(
        sacrifice_order(&with_photo, "Bad Request: RICH_MESSAGE_PHOTO_INVALID").first(),
        Some(&1),
        "and the photo must still be sacrificed before the document"
    );
}

/// AC-1 + AC-2 (#896): the measured scenario must sacrifice ZERO documents and
/// must not produce a lighter-but-"successful" retry.
///
/// One request is scripted — the refusal. If the ladder sacrifices anything it
/// makes a second request; if it falls through to the HTML dialect the fallback
/// also makes a second request, so the discriminator is not the count but the
/// BODY: a sacrifice retry re-sends the rich markdown with `attach://` parts
/// (minus the burned entry), while the fallback carries no media array at all.
#[tokio::test]
async fn a_photo_refusal_on_a_document_only_reply_sacrifices_nothing() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(serve_script(
        listener,
        vec![(400, "Bad Request: RICH_MESSAGE_PHOTO_INVALID"), (200, "")],
    ));

    let media = vec![document("doc0", "a.pdf"), document("doc1", "b.pdf")];
    let (id, sacrificed) = send_rich_with_media_target_id(
        &format!("http://127.0.0.1:{port}"),
        "TOKEN",
        4242,
        None,
        None,
        "![a.pdf](tg://document?id=doc0) ![b.pdf](tg://document?id=doc1)",
        &media,
        "test",
        "-",
    )
    .await
    .expect("the reply must still land — a refusal it cannot repair falls back, it does not fail");

    let captured = tokio::time::timeout(Duration::from_secs(30), server)
        .await
        .expect("the scripted server must see every request")
        .expect("server task");

    assert!(id > 0, "the reply still landed");
    assert_eq!(
        sacrificed,
        Vec::<SacrificedEntry>::new(),
        "AC-1: a PHOTO_ refusal with no photo entry must sacrifice ZERO documents"
    );
    assert!(
        captured.len() >= 2,
        "the refusal and the fallback must both be visible: {}",
        captured.len()
    );
    let second = String::from_utf8_lossy(&captured[1]);
    assert!(
        !second.contains("attach://"),
        "AC-2: the second request must be the HTML fallback, never a retry that \
         drops only a document reference — that retry is the len N → len N-1 shape: {second}"
    );
    assert!(
        !second.to_ascii_lowercase().contains("\"media\""),
        "AC-2: the fallback carries no media array at all — nothing was sacrificed, \
         so no rich retry with a burned entry may exist: {second}"
    );
}
