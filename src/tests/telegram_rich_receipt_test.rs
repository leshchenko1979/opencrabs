//! #923 AC-1/AC-2/AC-3: a `200` on `sendRichMessage` is not a delivery
//! certificate — the response's `rich_message` is.
//!
//! The incident (msg 103595): a reply carried one local document, the rich send
//! answered `200 {"ok": true, "result": {"message_id": …, "rich_message": {…,
//! "documents": []}}}`, the document leg saw a success id and subtracted the
//! file from what still needed shipping, and the reader got a bubble with a
//! dangling reference and no attachment. Nothing failed. That is the whole
//! defect: the success id was read as "everything in the request landed".
//!
//! The measured pair that separates the two shapes is ADR-0001 §4.3 A/B, msgs
//! 96654 / 96655 — the SAME document sent twice, both HTTP 200:
//!
//! * 96654, embedded: `rich_message.documents` has one entry, 73 B
//!   `doc-sample.md`;
//! * 96655, linked (the reference the server could not resolve): `documents: []`
//!   and no error field anywhere.
//!
//! So the response itself carries the verdict, and [`RichMediaReceipt`] is only
//! the reading of it. These tests pin that reading and the one subtraction built
//! on it, because both are pure: a mis-read here turns into either a duplicate
//! document on every turn (if "unconfirmed" is treated as "lost") or a silently
//! vanished attachment (if "not bound" is treated as "delivered").
//!
//! The three-valued distinction is the load-bearing part:
//!
//! | response                                   | reading       | `document_bound` |
//! |--------------------------------------------|---------------|------------------|
//! | no `rich_message` object at all             | UNCONFIRMED   | `true` (keep the pre-#923 assumption) |
//! | `rich_message.documents: []`                | nothing bound | `false` (the incident) |
//! | `rich_message.documents: [{file_name, …}]`  | those bound   | `true` only for a matching name |

use crate::channels::telegram::delivery::documents_the_response_did_not_bind;
use crate::channels::telegram::rich::api::RichMediaReceipt;
use crate::utils::image::LocalFile;
use serde_json::json;
use std::path::PathBuf;

/// The name the bytes travel under: `document_part_name` of the incident's
/// `doc-sample.md`, which is also what the server echoes in `file_name`.
const SAMPLE: &str = "doc-sample.md";

fn readable(path: &str) -> LocalFile {
    LocalFile {
        path: PathBuf::from(path),
        caption: None,
        marker_span: None,
    }
}

/// AC-1: the incident's exact shape — a successful send whose `rich_message`
/// lists NO documents — must read as "this document did not bind".
///
/// `documents: []` is the response saying it attached nothing. Before #923 this
/// was indistinguishable from the 96654 shape, because only `message_id` was
/// read, and the caller subtracted the document from the to-ship set anyway.
#[test]
fn empty_documents_array_is_not_a_delivery_certificate() {
    let response = json!({
        "message_id": 103595,
        "rich_message": { "documents": [] }
    });
    let receipt = RichMediaReceipt::from_response(&response, 1);

    assert!(
        !receipt.is_unconfirmed(),
        "the response DID echo a rich_message, so the receipt is confirmed"
    );
    assert_eq!(
        receipt.bound_documents(),
        Some(0),
        "the response bound no document"
    );
    assert!(
        !receipt.document_bound(SAMPLE),
        "AC-1: `documents: []` with a document requested means the document is NOT bound — \
         this is the #923 incident (msg 103595) and must not be read as delivered"
    );
}

/// The other half of the ADR §4.3 A/B pair (msg 96654): the same send with the
/// document actually embedded. This must stay `true`, or every correctly
/// delivered document would be shipped a second time as a detached bubble.
#[test]
fn bound_document_confirms_by_name() {
    let response = json!({
        "message_id": 96654,
        "rich_message": {
            "documents": [{ "file_name": SAMPLE, "file_size": 73 }]
        }
    });
    let receipt = RichMediaReceipt::from_response(&response, 1);

    assert_eq!(receipt.bound_documents(), Some(1));
    assert!(
        receipt.document_bound(SAMPLE),
        "a bound entry naming this document confirms it"
    );
    assert!(
        receipt.document_bound(&SAMPLE.to_uppercase()),
        "the part name is matched case-insensitively: the same file spelled \
         differently is still that file, never a loss"
    );
}

/// A response that echoes media state, listing a DIFFERENT document, does not
/// confirm this one. `file_name` is the match key precisely so this is
/// answerable: `file_size` alone could not tell two same-size documents apart.
#[test]
fn a_bound_but_differently_named_document_does_not_confirm() {
    let response = json!({
        "message_id": 1,
        "rich_message": { "documents": [{ "file_name": "other.md", "file_size": 73 }] }
    });
    let receipt = RichMediaReceipt::from_response(&response, 1);

    assert!(
        !receipt.document_bound(SAMPLE),
        "a document the response does not name is not bound, even at the same size"
    );
}

/// A `rich_message` that omits `documents` entirely is the same statement as an
/// empty array: the server echoed a message and no attached documents.
#[test]
fn rich_message_without_documents_key_binds_nothing() {
    let response = json!({
        "message_id": 1,
        "rich_message": { "markdown": "hello" }
    });
    let receipt = RichMediaReceipt::from_response(&response, 1);

    assert!(!receipt.is_unconfirmed());
    assert_eq!(receipt.bound_documents(), Some(0));
    assert!(!receipt.document_bound(SAMPLE));
}

/// AC-1, the unreadable side: a response with no `rich_message` object at all
/// tells us nothing about the media. It must NOT be read as a loss — re-sending
/// every document on a response shape we do not understand would duplicate
/// attachments on every turn, which is worse than the bug being fixed.
#[test]
fn absent_rich_message_is_unconfirmed_and_assumes_bound() {
    let response = json!({ "message_id": 1 });
    let receipt = RichMediaReceipt::from_response(&response, 1);

    assert!(
        receipt.is_unconfirmed(),
        "no rich_message object = the server never echoed media state"
    );
    assert_eq!(
        receipt.bound_documents(),
        None,
        "an unconfirmed receipt cannot claim a count"
    );
    assert!(
        receipt.document_bound(SAMPLE),
        "AC-1: unconfirmed keeps the pre-#923 assumption (bound), so an unreadable \
         response never turns into a duplicate bubble"
    );
    assert!(receipt.document_bound("anything-else.pdf"));
}

/// A bound entry that omitted its `file_name` makes the whole response
/// unmatchable. The receipt declines to judge rather than manufacture a loss out
/// of a name it was never given.
#[test]
fn unnamed_bound_document_declines_to_judge() {
    let response = json!({
        "message_id": 1,
        "rich_message": { "documents": [{ "file_size": 73 }] }
    });
    let receipt = RichMediaReceipt::from_response(&response, 1);

    assert!(!receipt.is_unconfirmed());
    assert_eq!(receipt.bound_documents(), Some(1));
    assert!(
        receipt.document_bound(SAMPLE),
        "an entry with no name cannot be matched against, so nothing is proven lost"
    );
}

/// The plain-markdown and HTML-fallback planes send no media array at all, and
/// their receipt says exactly that — "confirmed, zero documents bound" — which
/// is different from "unconfirmed". Callers on those planes have no documents to
/// reconcile, so this must never look like a loss.
#[test]
fn no_media_array_is_confirmed_and_empty() {
    let receipt = RichMediaReceipt::no_media_array();

    assert!(!receipt.is_unconfirmed());
    assert_eq!(receipt.bound_documents(), Some(0));
    assert!(!receipt.document_bound(SAMPLE));
    assert_ne!(
        RichMediaReceipt::default(),
        RichMediaReceipt::no_media_array(),
        "an empty media array is a statement; no response at all is not — the two \
         must never collapse into one value, or the unconfirmed case would start \
         meaning \"nothing bound\""
    );
    assert!(
        RichMediaReceipt::default().is_unconfirmed(),
        "the Default is the unreadable response (used by the dedup-suppressed arm)"
    );
}

/// AC-2: the subtraction the intermediate leg performs — "which of the files I
/// asked for does the response NOT confirm?" — returns the incident's document
/// for `documents: []`, so it is shipped detached instead of vanishing.
#[test]
fn unbound_document_is_returned_for_resending() {
    let files = vec![readable("/tmp/doc-sample.md"), readable("/tmp/second.pdf")];
    let response = json!({ "message_id": 103595, "rich_message": { "documents": [] } });
    let receipt = RichMediaReceipt::from_response(&response, files.len());

    let unbound = documents_the_response_did_not_bind(&files, &receipt);
    assert_eq!(
        unbound.len(),
        2,
        "AC-2: a 200 with `documents: []` binds nothing, so every requested \
         document still needs shipping"
    );
    assert_eq!(unbound[0].path, PathBuf::from("/tmp/doc-sample.md"));
    assert_eq!(unbound[1].path, PathBuf::from("/tmp/second.pdf"));
}

/// AC-2, the delivered case: a bound document is subtracted (nothing to
/// re-send), and only the unconfirmed remainder ships. Getting this wrong in
/// either direction is the defect — duplicates here, a lost attachment there.
#[test]
fn bound_documents_are_subtracted_from_the_resend_set() {
    let files = vec![readable("/tmp/doc-sample.md"), readable("/tmp/second.pdf")];
    let response = json!({
        "message_id": 96654,
        "rich_message": { "documents": [{ "file_name": SAMPLE, "file_size": 73 }] }
    });
    let receipt = RichMediaReceipt::from_response(&response, files.len());

    let unbound = documents_the_response_did_not_bind(&files, &receipt);
    assert_eq!(
        unbound.len(),
        1,
        "the bound document is delivered; only the one the response does not \
         name still needs a bubble"
    );
    assert_eq!(unbound[0].path, PathBuf::from("/tmp/second.pdf"));
}

/// AC-2, the all-clear case: everything bound means nothing is re-sent.
#[test]
fn fully_bound_request_resends_nothing() {
    let files = vec![readable("/tmp/doc-sample.md")];
    let response = json!({
        "message_id": 96654,
        "rich_message": { "documents": [{ "file_name": SAMPLE, "file_size": 73 }] }
    });
    let receipt = RichMediaReceipt::from_response(&response, files.len());

    assert!(
        documents_the_response_did_not_bind(&files, &receipt).is_empty(),
        "a confirmed delivery must not be duplicated as a detached bubble"
    );
}

/// AC-2, the unreadable case end-to-end: an unconfirmed receipt re-sends
/// nothing, so a response shape change degrades to the old behaviour instead of
/// doubling every document.
#[test]
fn unconfirmed_receipt_resends_nothing() {
    let files = vec![readable("/tmp/doc-sample.md")];
    let response = json!({ "message_id": 1 });
    let receipt = RichMediaReceipt::from_response(&response, files.len());

    assert!(receipt.is_unconfirmed());
    assert!(
        documents_the_response_did_not_bind(&files, &receipt).is_empty(),
        "we cannot read the response, so we assume the request's documents landed"
    );
}

/// An empty request reconciles to nothing, whatever the response says — the
/// subtraction must not invent work out of a receipt that mentions documents
/// nobody asked for.
#[test]
fn empty_request_has_nothing_to_reconcile() {
    let response = json!({ "message_id": 1, "rich_message": { "documents": [] } });
    let receipt = RichMediaReceipt::from_response(&response, 0);

    assert!(documents_the_response_did_not_bind(&[], &receipt).is_empty());
}
