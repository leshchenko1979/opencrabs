//! Raw Bot API client for `sendRichMessage` (Bot API 10.1, 2026-06).
//!
//! teloxide 0.17 has no binding for this method yet, so we call it directly
//! over HTTP. `InputRichMessage` takes the message as a `markdown` (or `html`)
//! string — Telegram parses it server-side into rich blocks (tables, headings,
//! nested lists, math) — so there is no block JSON to construct: we pass the
//! model's markdown straight through.

use super::mermaid;
use super::render_html::markdown_to_html_mermaid_p;
use crate::channels::telegram::governor::{EditClass, RichAdmission};
use crate::channels::telegram::suggest_options::enforce_button_fit;
use teloxide::types::ThreadId;

/// Send `html` as a native rich message and return the new message id
/// (#420 path A). The HTML input mode is parsed server-side into rich
/// blocks, so `<details><summary>` becomes a native RichBlockDetails
/// collapsible. (Earlier text here claimed the markdown input mode cannot
/// express details — wrong: live Bot API probes J/K, 2026-09-10, proved
/// markdown mode parses `<details><summary>` natively; the markdown+media
/// card path relies on that.) `reply_markup` is optional — pass `None`
/// for no keyboard.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_rich_html_id(
    api_url: &str,
    token: &str,
    chat_id: i64,
    thread_id: Option<ThreadId>,
    html: &str,
    reply_markup: Option<&serde_json::Value>,
    origin: &str,
    origin_detail: &str,
) -> anyhow::Result<i32> {
    let url = format!("{}/bot{token}/sendRichMessage", api_base(api_url));
    let mut body = build_body_html(chat_id, thread_id, html);
    if let Some(kb) = reply_markup {
        body["reply_markup"] = kb.clone();
    }
    let result = post_rich(&url, &body, origin, origin_detail, EditClass::Final).await?;
    result
        .get("message_id")
        .and_then(serde_json::Value::as_i64)
        .map(|id| id as i32)
        .ok_or_else(|| anyhow::anyhow!("sendRichMessage ok but response carried no message_id"))
}

/// Edit an existing rich message with markdown input (#1230). Telegram's
/// `editMessageText` accepts `rich_message.markdown`; editing from the raw
/// source (rather than pre-converted HTML) keeps pipe tables native through
/// the edit, exactly like the markdown rich send path. `reply_markup` is
/// optional — pass `None` to leave the keyboard unchanged.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn edit_rich_markdown(
    api_url: &str,
    token: &str,
    chat_id: i64,
    message_id: i32,
    markdown: &str,
    reply_markup: Option<&serde_json::Value>,
    origin: &str,
    origin_detail: &str,
) -> anyhow::Result<()> {
    let url = format!("{}/bot{token}/editMessageText", api_base(api_url));
    let mut body = serde_json::json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "rich_message": { "markdown": super::normalize_rich_markdown_with_media(markdown, &[]) },
    });
    if let Some(kb) = reply_markup {
        body["reply_markup"] = kb.clone();
    }
    post_and_check(&url, &body, origin, origin_detail, EditClass::Final).await
}

/// Edit an existing rich message with HTML input (#420 path A).
/// `reply_markup` is optional — pass `None` to leave the keyboard unchanged.
///
/// `class` carries the caller's drop ladder (#556). Only the flow passes a
/// real one, because its clock ticks are pure chrome; every other caller
/// passes [`EditClass::Final`] — a non-flow edit is content, and content is
/// never dropped.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn edit_rich_html(
    api_url: &str,
    token: &str,
    chat_id: i64,
    message_id: i32,
    html: &str,
    reply_markup: Option<&serde_json::Value>,
    origin: &str,
    origin_detail: &str,
    class: EditClass,
) -> anyhow::Result<()> {
    let url = format!("{}/bot{token}/editMessageText", api_base(api_url));
    let mut body = serde_json::json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "rich_message": { "html": enforce_button_fit(html) },
    });
    if let Some(kb) = reply_markup {
        body["reply_markup"] = kb.clone();
    }
    post_and_check(&url, &body, origin, origin_detail, class).await
}

/// Edit an existing rich message with markdown input + a `media` array (#98).
/// Same body convention as the send path (`rich_message: {markdown, media}`,
/// Bot API 10.2+, #1044): local PNG bytes upload via multipart `attach://`,
/// URL entries ride as server-side references. `reply_markup` is optional —
/// pass `None` to leave the keyboard unchanged.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn edit_rich_markdown_media(
    api_url: &str,
    token: &str,
    chat_id: i64,
    message_id: i32,
    markdown: &str,
    media: &[super::mermaid::MediaEntry],
    reply_markup: Option<&serde_json::Value>,
    origin: &str,
    origin_detail: &str,
) -> anyhow::Result<()> {
    let url = format!("{}/bot{token}/editMessageText", api_base(api_url));
    let body = build_body_markdown_media_edit(chat_id, message_id, markdown, media);
    let body = if let Some(kb) = reply_markup {
        let mut b = body;
        b["reply_markup"] = kb.clone();
        b
    } else {
        body
    };
    if media.iter().any(|m| m.bytes.is_some()) {
        post_rich_multipart(&url, media, &body, origin, origin_detail, EditClass::Final).await?;
    } else {
        post_and_check(&url, &body, origin, origin_detail, EditClass::Final).await?;
    }
    Ok(())
}

/// Build the `editMessageText` body with markdown input + a `media` array
/// (#98). Split out so the request shape is unit-testable without a live
/// bot, mirroring the send-path body shape (`build_body_markdown_media_target`).
pub(crate) fn build_body_markdown_media_edit(
    chat_id: i64,
    message_id: i32,
    markdown: &str,
    media: &[super::mermaid::MediaEntry],
) -> serde_json::Value {
    let media_arr: Vec<serde_json::Value> = media
        .iter()
        .map(|m| {
            // Local PNG bytes upload via multipart as `attach://<id>`; URL
            // entries keep the legacy server-side fetch reference.
            let source = match (&m.bytes, &m.url) {
                (Some(_), _) => format!("attach://{}", m.id),
                (None, Some(url)) => url.clone(),
                (None, None) => String::new(),
            };
            serde_json::json!({
                "id": m.id,
                // The type comes from the entry's own kind (#465). It used to
                // be the literal "photo" for every entry, which is exactly the
                // mistake the anim-lab MIX probe hit as
                // `RICH_MESSAGE_VIDEO_INVALID` — a video reference generated
                // by the photo helper.
                "media": { "type": m.kind.as_str(), "media": source },
            })
        })
        .collect();
    serde_json::json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "rich_message": { "markdown": super::normalize_rich_markdown_with_media(markdown, media), "media": media_arr },
    })
}

/// Lib-pass dead-code exempt: production callers went through the media
/// variant; this shape stays for the cfg(test) API suite (custom api_url).
#[allow(dead_code)]
/// Send `markdown` as a native rich message and return the new message id.
/// Used for intermediate streamed segments, which must be tracked for later
/// footer-append / dedup. Returns `Err` so the caller can fall back to HTML.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_rich_markdown_id(
    api_url: &str,
    token: &str,
    chat_id: i64,
    thread_id: Option<ThreadId>,
    markdown: &str,
    reply_to: Option<i32>,
    origin: &str,
    origin_detail: &str,
) -> anyhow::Result<i32> {
    send_rich_markdown_target_id(
        api_url,
        token,
        chat_id,
        thread_id,
        reply_to,
        markdown,
        origin,
        origin_detail,
    )
    .await
}

/// [`send_rich_markdown_id`] with an optional Telegram reply target.
/// `reply_to` is a message id in the same chat; when set the rich body
/// carries `reply_parameters` so the reply lands threaded to that message
/// (#1230).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_rich_markdown_target_id(
    api_url: &str,
    token: &str,
    chat_id: i64,
    thread_id: Option<ThreadId>,
    reply_to: Option<i32>,
    markdown: &str,
    origin: &str,
    origin_detail: &str,
) -> anyhow::Result<i32> {
    let url = format!("{}/bot{token}/sendRichMessage", api_base(api_url));
    let result = post_rich(
        &url,
        &build_body_target(chat_id, thread_id, reply_to, markdown),
        origin,
        origin_detail,
        EditClass::Final,
    )
    .await?;
    result
        .get("message_id")
        .and_then(serde_json::Value::as_i64)
        .map(|id| id as i32)
        .ok_or_else(|| anyhow::anyhow!("sendRichMessage ok but response carried no message_id"))
}

/// How many times a 429 is waited out before giving up and letting the caller
/// fall back. One retry was not enough: Telegram hands out multi-second waits
/// under load, so a single retry lands inside the same window it was told to
/// wait for and the send is abandoned while still rate limited.
const RICH_MAX_RETRIES: u32 = 3;

/// POST `body` to `url`, treating anything other than `{"ok":true,...}` as an
/// error (surfacing Telegram's `description`). Returns the `result` object.
///
/// A 429 is retried up to `RICH_MAX_RETRIES` times, honouring the server's
/// `retry_after`. The error returned always describes the LAST attempt: the
/// previous version rebound `status`/`text`/`parsed` inside the retry block,
/// so those bindings fell out of scope and a retry that failed for a new
/// reason was reported as the rate limit that preceded it (#927).
/// Extract correlation fields for a rich-API call from the request pair
/// (#1085 P1a): method from the URL, chat/thread from the body, len and
/// hash from the payload text. Works for sends, edits and deletes alike
/// (non-send calls simply log msg=0).
fn rich_send_fields<'a>(
    url: &'a str,
    body: &serde_json::Value,
) -> (&'a str, i64, Option<i32>, usize, String) {
    let method = url.rsplit('/').next().unwrap_or("?");
    let chat_id = body
        .get("chat_id")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    let thread = body
        .get("message_thread_id")
        .and_then(serde_json::Value::as_i64)
        .map(|t| t as i32);
    let text = body
        .pointer("/rich_message/markdown")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            body.pointer("/rich_message/html")
                .and_then(serde_json::Value::as_str)
        })
        .unwrap_or("");
    (
        method,
        chat_id,
        thread,
        text.len(),
        crate::channels::telegram::telemetry::content_hash8(text),
    )
}

/// #676 - the message this request targets, for the 429 refusal line.
///
/// A refusal currently names the chat but not the message, so an operator
/// cannot tell whether the refused calls are spread across the chat or
/// concentrated on one continuously-edited bubble. `editMessageText` and
/// `editRichMessage` bodies carry `message_id`; a `sendRichMessage` body has
/// none, so a send reports `-` rather than omitting the field - a constant
/// field shape is what makes the lines greppable.
pub(crate) fn target_message_id(body: &serde_json::Value) -> String {
    body.get("message_id")
        .and_then(serde_json::Value::as_i64)
        .map_or_else(|| "-".to_string(), |m| m.to_string())
}

/// Media references in a rich body that the body's OWN `media` array cannot
/// resolve — the offenders a rich rejection is about (#334, H4). Returns `None`
/// when the body carries no such reference, so an ordinary failure keeps the
/// metadata-only log shape and only a media-attributable rejection gains detail.
///
/// The extractor is shared with the neutralizer
/// ([`super::mermaid::unresolved_media_refs_by`]), so the log and the rewrite can
/// never disagree about what counts as an orphan reference.
pub(crate) fn rich_body_offenders(body: &serde_json::Value) -> Option<Vec<String>> {
    let text = body
        .pointer("/rich_message/markdown")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            body.pointer("/rich_message/html")
                .and_then(serde_json::Value::as_str)
        })?;
    let ids: Vec<String> = body
        .pointer("/rich_message/media")
        .and_then(serde_json::Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(serde_json::Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let refs = super::mermaid::unresolved_media_refs_by(text, |id| ids.iter().any(|k| k == id));
    (!refs.is_empty()).then_some(refs)
}

// ── The response's own account of what it bound (#923) ──────────────────────
//
// `sendRichMessage` answering `200 {"ok":true}` proves the REQUEST was
// accepted, never that every entry in it was attached. A document reference the
// server cannot use is discarded silently: the delivered message comes back with
// `rich_message.documents: []`, no error field, and nothing behind the
// reference (measured — ADR-0001 §4.3 A/B, msgs 96654/96655, and the #923
// incident, msg 103595). So the response's `rich_message` is the only statement
// about what landed, and everything below reduces it to the one question the
// delivery legs ask: was THIS document bound?
//
// The distinction between ABSENT and EMPTY is the whole point:
//
// | response                                  | reading                          |
// |-------------------------------------------|----------------------------------|
// | no `rich_message` object at all            | the server echoed no media state — UNCONFIRMED |
// | `rich_message.documents: []`               | nothing bound — the incident's shape |
// | `rich_message.documents: [{file_name, …}]` | those entries bound, and only those |

/// What a `sendRichMessage` response says the server actually BOUND (#923).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RichMediaReceipt {
    /// `None` = the response carried no `rich_message` object, so the server
    /// never echoed media state. Nothing can be confirmed, and callers keep the
    /// pre-#923 behaviour (assume bound) rather than re-sending every document
    /// on a response shape we do not understand.
    documents: Option<Vec<BoundDocument>>,
}

/// One document the response says it attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundDocument {
    /// The name the reader sees — the multipart part name echoed back. `None`
    /// when the response omitted it, which makes that entry unmatchable; the
    /// receipt then declines to judge rather than manufacture a loss.
    pub(crate) file_name: Option<String>,
}

impl RichMediaReceipt {
    /// A receipt for a send that carried NO media array at all — the plain
    /// markdown path and the HTML dialect fallback. It is not "unconfirmed":
    /// neither plane has media entries, so no document is attached, and the
    /// receipt says exactly that.
    pub(crate) fn no_media_array() -> Self {
        Self {
            documents: Some(Vec::new()),
        }
    }

    /// Read the media state off a successful `sendRichMessage` `result`.
    ///
    /// `requested_documents` feeds the one diagnostic line: a response that
    /// echoes no `rich_message` while documents were asked for is a shape we
    /// cannot read, and the gap must be visible in the log rather than assumed
    /// either way.
    ///
    /// The match key is the document's `file_name`, because that is the part
    /// name the bytes travelled under (`build_multipart_form` sets it from the
    /// entry's `name`) and the server echoes it back. The response's
    /// `file_size` is deliberately NOT a key: a name is what the reader is
    /// shown, and a size alone cannot say WHICH of two same-size documents
    /// bound.
    pub(crate) fn from_response(result: &serde_json::Value, requested_documents: usize) -> Self {
        let Some(rich) = result.get("rich_message") else {
            if requested_documents > 0 {
                tracing::debug!(
                    "Telegram: sendRichMessage response carried no rich_message object \
                     ({requested_documents} document entr{} requested) — delivery of them is \
                     UNCONFIRMED, keeping the pre-send assumption",
                    if requested_documents == 1 { "y" } else { "ies" }
                );
            }
            return Self::default();
        };
        let documents = rich
            .get("documents")
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(|d| BoundDocument {
                        file_name: d
                            .get("file_name")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            documents: Some(documents),
        }
    }

    /// Whether the response CONFIRMS this document is attached.
    ///
    /// Three answers, and only the last one is a loss:
    ///
    /// - the receipt is UNCONFIRMED (no `rich_message` at all) → `true`. The
    ///   pre-#923 assumption is kept, so a response shape we cannot read never
    ///   turns into a duplicate bubble on every turn.
    /// - a bound document carries this name → `true`.
    /// - no name matches, and every bound document carried a name → `false`:
    ///   the server echoed media state and this document is not in it. That is
    ///   the incident's shape (`documents: []` with a document requested).
    ///
    /// A bound document that omitted its `file_name` makes the response
    /// unmatchable, so the receipt answers `true` for every name rather than
    /// read a loss into an entry it was never given a name for.
    pub(crate) fn document_bound(&self, name: &str) -> bool {
        let Some(documents) = &self.documents else {
            return true;
        };
        if documents.iter().any(|d| {
            d.file_name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        }) {
            return true;
        }
        // An empty `documents` array reaches here too: nothing matched, and
        // nothing was listed without a name, so nothing bound.
        documents.iter().any(|d| d.file_name.is_none())
    }

    /// Whether the response echoed media state at all. Callers log this once
    /// per send so a shape change is visible without re-sending anything.
    pub(crate) fn is_unconfirmed(&self) -> bool {
        self.documents.is_none()
    }

    /// How many documents the response says it bound. `None` when unconfirmed.
    pub(crate) fn bound_documents(&self) -> Option<usize> {
        self.documents.as_ref().map(Vec::len)
    }
}

async fn post_rich(
    url: &str,
    body: &serde_json::Value,
    origin: &str,
    origin_detail: &str,
    class: EditClass,
) -> anyhow::Result<serde_json::Value> {
    let client = reqwest::Client::new();
    let mut attempt = 0u32;

    // An edit that would leave the message exactly as it is costs a round trip
    // to be told `message is not modified`, and used to be logged as a send
    // failure (#1443). Nothing to send, so this returns before the pacing gate
    // as well as before the request.
    let edit = super::edit_dedup::fingerprint(body).filter(|_| url.contains("editMessage"));
    if let Some((chat_id, message_id, fp)) = edit
        && super::edit_dedup::is_redundant(chat_id, message_id, fp)
    {
        tracing::debug!(
            "Rich edit skipped: message {message_id} in chat {chat_id} already \
             carries this content and markup"
        );
        return Ok(serde_json::Value::Null);
    }

    loop {
        // G4 (#1211): this endpoint has its own Telegram budget, so none of
        // the typing/edit/send governors sees its traffic. Paced here, at the
        // one place every rich send and edit passes through, ahead of the
        // reactive 429 handling below.
        {
            let (_, chat_id, thread, _, _) = rich_send_fields(url, body);
            match crate::channels::telegram::governor::pace_rich(
                teloxide::types::ChatId(chat_id),
                thread,
                class,
            )
            .await
            {
                RichAdmission::Now => {}
                RichAdmission::Dropped(dropped) => {
                    tracing::debug!(
                        "Telegram rate-limiter: {:?} rich call dropped before send for chat={chat_id}",
                        dropped
                    );
                    return Ok(serde_json::Value::Null);
                }
                RichAdmission::Deferred => {
                    anyhow::bail!("Telegram rich call deferred by active global 429 cooldown");
                }
            }
        }
        let resp = client.post(url).json(body).send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();

        if status.is_success()
            && parsed.get("ok").and_then(serde_json::Value::as_bool) == Some(true)
        {
            let result = parsed
                .get("result")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            // Correlation telemetry (#1085 P1a, review F5/F8): every rich
            // send that lands gets one line with full correlation fields.
            // Edits are NOT new sends — they must not pollute the
            // duplicate-attribution grep — so they log under their own kind.
            let (method, chat_id, thread, len, hash8) = rich_send_fields(url, body);
            let is_edit = url.contains("editMessage");
            let kind = if is_edit { "rich_edit" } else { "rich_api" };
            let msg_id = result
                .get("message_id")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0) as i32;
            crate::channels::telegram::telemetry::log_send_success(
                origin,
                origin_detail,
                "-",
                kind,
                method,
                chat_id,
                thread,
                msg_id,
                len,
                &hash8,
            );
            if let Some((chat_id, message_id, fp)) = edit {
                super::edit_dedup::remember(chat_id, message_id, fp);
            }
            return Ok(result);
        }

        if status.as_u16() == 429 && attempt < RICH_MAX_RETRIES {
            let retry_after =
                crate::channels::telegram::rate_limit::parse_retry_after_json(&parsed)
                    .map_or(5, |d| d.as_secs());
            attempt += 1;
            let (_, chat_id, _, _, _) = rich_send_fields(url, body);
            // #676 - name the message this refusal was editing, so the line
            // can be joined to the send telemetry it belongs to.
            let msg = target_message_id(body);
            if matches!(
                crate::channels::telegram::rate_limit::wait_out(
                    "rich API",
                    std::time::Duration::from_secs(retry_after),
                    &format!(" (attempt {attempt}/{RICH_MAX_RETRIES}) msg={msg}"),
                    Some(chat_id),
                )
                .await,
                crate::channels::telegram::rate_limit::WaitOutcome::Deferred,
            ) {
                anyhow::bail!("Telegram rich call deferred by long 429 window");
            }
            continue;
        }

        // Out of retries, or an error that retrying cannot fix. Report THIS
        // attempt so the caller logs why the send actually failed.
        let desc = parsed
            .get("description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(&text);
        // Telegram reporting the content is already identical means the
        // message is in the state we asked for. Nothing failed, so this must
        // not be logged as a failure or bubble as an error (#1443); the other
        // edit paths in this crate have always treated it as silent success.
        // Recording the fingerprint is what stops the next refresh asking.
        if super::edit_dedup::is_not_modified(desc) {
            if let Some((chat_id, message_id, fp)) = edit {
                super::edit_dedup::remember(chat_id, message_id, fp);
            }
            tracing::debug!(
                "Rich edit was a no-op: Telegram reports the content and markup are unchanged"
            );
            return Ok(serde_json::Value::Null);
        }
        if status.as_u16() == 429 {
            tracing::warn!(
                "Rich API still rate limited after {RICH_MAX_RETRIES} retries — falling back"
            );
        }
        {
            // Correlation telemetry (#1085 P1a): a failed rich send must
            // carry the same fields a successful one does. A rich rejection is
            // unattributable without the media refs it is about (#334, H4), so
            // they ride this error path — and only this one.
            let (method, chat_id, thread, len, hash8) = rich_send_fields(url, body);
            let offenders = rich_body_offenders(body);
            crate::channels::telegram::telemetry::log_send_failure_with_offenders(
                origin,
                origin_detail,
                "-",
                "rich_api",
                method,
                chat_id,
                thread,
                len,
                &hash8,
                &format!("({status}): {desc}"),
                offenders.as_deref(),
            );
        }
        anyhow::bail!("Telegram rich API error ({status}): {desc}")
    }
}

/// POST `body` and discard the result — for calls where only success matters.
/// `class` is the caller's drop ladder (#556); see [`edit_rich_html`].
async fn post_and_check(
    url: &str,
    body: &serde_json::Value,
    origin: &str,
    origin_detail: &str,
    class: EditClass,
) -> anyhow::Result<()> {
    post_rich(url, body, origin, origin_detail, class)
        .await
        .map(|_| ())
}

/// Build the `sendRichMessage` JSON request body. Split out so the request
/// shape is unit-testable without a live bot.
pub(crate) fn build_body(
    chat_id: i64,
    thread_id: Option<ThreadId>,
    markdown: &str,
    reply_to: Option<i32>,
) -> serde_json::Value {
    build_body_target(chat_id, thread_id, reply_to, markdown)
}

/// [`build_body`] with an optional Telegram reply target (#1230). When
/// `reply_to` is set, the body carries `reply_parameters: {message_id}`
/// so a rich reply lands threaded to that message.
pub(crate) fn build_body_target(
    chat_id: i64,
    thread_id: Option<ThreadId>,
    reply_to: Option<i32>,
    markdown: &str,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "chat_id": chat_id,
        // No media array rides this body (#334): a `tg`/`attach` ref here can never
        // resolve, so the shield is told so explicitly rather than judging by scheme.
        "rich_message": { "markdown": super::normalize_rich_markdown_with_media(markdown, &[]) },
    });
    if let Some(t) = thread_id {
        // ThreadId wraps a MessageId(i32).
        body["message_thread_id"] = serde_json::json!(t.0.0);
    }
    if let Some(mid) = reply_to {
        body["reply_parameters"] = serde_json::json!({ "message_id": mid });
    }
    body
}

/// Build the `sendRichMessage` body with HTML input (#420 path A).
/// `InputRichMessage` accepts `markdown` or `html`; HTML is the mode that
/// can express RichBlockDetails via `<details><summary>`.
pub(crate) fn build_body_html(
    chat_id: i64,
    thread_id: Option<ThreadId>,
    html: &str,
) -> serde_json::Value {
    let mut body = serde_json::json!({
        "chat_id": chat_id,
        "rich_message": { "html": enforce_button_fit(html) },
    });
    if let Some(t) = thread_id {
        body["message_thread_id"] = serde_json::json!(t.0.0);
    }
    body
}

/// Send `markdown` with a `media` array as a native rich message
/// (Bot API 10.2+, #1044). The markdown references each image via
/// `tg://photo?id=<id>`; the `media` array maps each id to an image source.
///
/// Two delivery modes, switched per entry (#1044 media plumbing):
///
/// - `MediaEntry::url` — the legacy URL path: the source is a renderer URL
///   (e.g. mermaid.ink) that Telegram fetches server-side. Sent as a plain
///   JSON body.
/// - `MediaEntry::bytes` (the active path: pre-validated mermaid.ink render,
///   #1238) — the PNG bytes are uploaded to Telegram via multipart as
///   `attach://<id>` photo parts, so Telegram never touches a third-party
///   URL. The request must therefore be `multipart/form-data`, not JSON.
///
/// Carries `reply_parameters` when `reply_to` is set (#1230). Returns the
/// new message id. Returns `Err` on transport/API failure so the caller can
/// fall back to the HTML dialect.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_rich_markdown_media_target_id(
    api_url: &str,
    token: &str,
    chat_id: i64,
    thread_id: Option<ThreadId>,
    reply_to: Option<i32>,
    markdown: &str,
    media: &[super::mermaid::MediaEntry],
    reply_markup: Option<&serde_json::Value>,
    origin: &str,
    origin_detail: &str,
) -> anyhow::Result<i32> {
    send_rich_markdown_media_target_receipt(
        api_url,
        token,
        chat_id,
        thread_id,
        reply_to,
        markdown,
        media,
        reply_markup,
        origin,
        origin_detail,
    )
    .await
    .map(|(id, _receipt)| id)
}

/// [`send_rich_markdown_media_target_id`] with the response's own account of
/// what it bound (#923).
///
/// The receipt is read off the SAME `result` the id comes from, and against the
/// media list this call actually sent — which is the only list the response can
/// be talking about. `None` when the response carried no `rich_message`: see
/// [`RichMediaReceipt`] for why that is "unconfirmed", never "delivered".
#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_rich_markdown_media_target_receipt(
    api_url: &str,
    token: &str,
    chat_id: i64,
    thread_id: Option<ThreadId>,
    reply_to: Option<i32>,
    markdown: &str,
    media: &[super::mermaid::MediaEntry],
    reply_markup: Option<&serde_json::Value>,
    origin: &str,
    origin_detail: &str,
) -> anyhow::Result<(i32, RichMediaReceipt)> {
    let url = format!("{}/bot{token}/sendRichMessage", api_base(api_url));
    let body = build_body_markdown_media_target(
        chat_id,
        thread_id,
        reply_to,
        markdown,
        media,
        reply_markup,
    );

    let result = if media.iter().any(|m| m.bytes.is_some()) {
        // Local render → multipart upload of the PNG bytes (attach://).
        post_rich_multipart(&url, media, &body, origin, origin_detail, EditClass::Final).await?
    } else {
        // Legacy URL path → plain JSON; Telegram refetches the URL.
        post_rich(&url, &body, origin, origin_detail, EditClass::Final).await?
    };

    let requested_documents = media
        .iter()
        .filter(|m| m.kind == super::mermaid::MediaKind::Document)
        .count();
    let receipt = RichMediaReceipt::from_response(&result, requested_documents);
    let id = result
        .get("message_id")
        .and_then(serde_json::Value::as_i64)
        .map(|id| id as i32)
        .ok_or_else(|| anyhow::anyhow!("sendRichMessage ok but response carried no message_id"))?;
    Ok((id, receipt))
}

/// Scalar string form parts for a rich multipart request: the JSON body's
/// top-level fields Telegram needs as form parts. Split out so the part
/// shape is unit-testable without a live bot. `message_id` is edit-only:
/// `editMessageText` multipart rejects with `message to edit not found`
/// when the part is missing; `sendRichMessage` bodies carry no
/// `message_id`, so sends are unchanged.
pub(crate) fn multipart_scalar_fields(body: &serde_json::Value) -> Vec<(String, String)> {
    let mut fields: Vec<(String, String)> = Vec::new();
    if let Some(v) = body.get("chat_id") {
        fields.push(("chat_id".to_string(), v.to_string()));
    }
    if let Some(v) = body.get("message_id") {
        fields.push(("message_id".to_string(), v.to_string()));
    }
    if let Some(v) = body.get("message_thread_id") {
        fields.push(("message_thread_id".to_string(), v.to_string()));
    }
    if let Some(v) = body.get("reply_parameters") {
        fields.push(("reply_parameters".to_string(), v.to_string()));
    }
    if let Some(v) = body.get("rich_message") {
        fields.push(("rich_message".to_string(), v.to_string()));
    }
    // The keyboard must ride multipart edits too: Telegram CLEARS
    // reply_markup on an edit that omits it, so a media-bearing edit of the
    // plan card would silently strip Approve/Discard without this part.
    if let Some(v) = body.get("reply_markup") {
        fields.push(("reply_markup".to_string(), v.to_string()));
    }
    fields
}

/// The file name and MIME a byte entry's multipart part carries, derived from
/// the bytes themselves (#502).
///
/// Split out so the identity is unit-testable without a live bot — a
/// `reqwest::multipart::Form` exposes no accessor, so the part shape could not
/// be asserted through the form it builds. Deriving both halves from the magic
/// bytes is what stops a JPEG shipping as `<id>.png` with `image/png`, which
/// the hardcoded version did for every format a local reference can point at
/// (`is_supported_image` accepts PNG, JPEG, GIF, WEBP and BMP).
pub(crate) fn media_part_identity(
    id: &str,
    bytes: &[u8],
    kind: super::mermaid::MediaKind,
    name: Option<&str>,
) -> (String, &'static str) {
    // An entry's bytes reach the API as a multipart file part, so the part's
    // own name and MIME have to agree with the `type` the entry declares
    // (#465): a video uploaded as `image/bmp` (the image sniffer's fallback
    // for bytes it cannot read) is a mislabelled part on the one path that
    // carries video inline.
    if kind == super::mermaid::MediaKind::Video {
        return (format!("{id}.mp4"), "video/mp4");
    }
    // #1918: a DOCUMENT's name cannot be recovered from its bytes, so the
    // producer carries it (`name`, Step 1's `document_part_name`). Telegram
    // shows the part name as the file name in the chat, which is the whole
    // point: without it the reader sees a bare `file`. The MIME follows the
    // extension so a `.pdf` ships as `application/pdf` rather than as an
    // unknown binary.
    if kind == super::mermaid::MediaKind::Document {
        let file_name = name.unwrap_or(id).to_string();
        let mime = document_mime(&file_name);
        return (file_name, mime);
    }
    let ext = crate::utils::image::image_extension(bytes);
    let mime = match ext {
        "png" => "image/png",
        "jpg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/bmp",
    };
    (format!("{id}.{ext}"), mime)
}

/// The MIME a document part declares, from its file name's extension (#1918).
///
/// A deliberately short register of the types a reply actually links to, with
/// `application/octet-stream` as the honest fallback: the file family validates
/// ANY bytes (`validate_local_file` has no format gate), so an unknown extension
/// is a normal case, not an error — and a wrong `Content-Type` is worse than a
/// generic one.
fn document_mime(file_name: &str) -> &'static str {
    match std::path::Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("pdf") => "application/pdf",
        Some("txt") => "text/plain",
        Some("md") => "text/markdown",
        Some("csv") => "text/csv",
        Some("json") => "application/json",
        Some("xml") => "application/xml",
        Some("html") => "text/html",
        Some("zip") => "application/zip",
        Some("gz") => "application/gzip",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        _ => "application/octet-stream",
    }
}

/// Build the multipart/form-data request for a `sendRichMessage` whose media
/// array references uploaded PNG bytes via `attach://<id>`. Scalar parts come
/// from [`multipart_scalar_fields`]; each byte entry becomes a file part
/// named exactly `<id>` so Telegram's `attach://<id>` reference resolves
/// (§ Bot API multipart media convention). The JSON body is still passed in
/// for correlation telemetry ([`rich_send_fields`] reads its `rich_message`
/// pointer).
fn build_multipart_form(
    media: &[super::mermaid::MediaEntry],
    body: &serde_json::Value,
) -> reqwest::multipart::Form {
    let mut form = reqwest::multipart::Form::new();
    for (name, value) in multipart_scalar_fields(body) {
        form = form.text(name, value);
    }
    for m in media {
        if let Some(bytes) = &m.bytes {
            let (file_name, mime) = media_part_identity(&m.id, bytes, m.kind, m.name.as_deref());
            let part = reqwest::multipart::Part::bytes(bytes.clone())
                .file_name(file_name)
                .mime_str(mime)
                .expect("a literal image mime is always valid");
            form = form.part(m.id.clone(), part);
        }
    }
    form
}

/// POST `form` (multipart, for `attach://` bytes upload) to `url`, mirroring
/// [`post_rich`]'s retry / rate-limit / telemetry semantics exactly. Accepts
/// only the media-with-bytes send; every non-`ok` response is an error that
/// surfaces Telegram's `description`.
async fn post_rich_multipart(
    url: &str,
    media: &[super::mermaid::MediaEntry],
    body: &serde_json::Value,
    origin: &str,
    origin_detail: &str,
    class: EditClass,
) -> anyhow::Result<serde_json::Value> {
    let client = reqwest::Client::new();
    let mut attempt = 0u32;

    loop {
        {
            let (_, chat_id, thread, _, _) = rich_send_fields(url, body);
            match crate::channels::telegram::governor::pace_rich(
                teloxide::types::ChatId(chat_id),
                thread,
                class,
            )
            .await
            {
                RichAdmission::Now => {}
                RichAdmission::Dropped(dropped) => {
                    tracing::debug!(
                        "Telegram rate-limiter: {:?} rich call dropped before send for chat={chat_id}",
                        dropped
                    );
                    return Ok(serde_json::Value::Null);
                }
                RichAdmission::Deferred => {
                    anyhow::bail!("Telegram rich call deferred by active global 429 cooldown");
                }
            }
        }
        // Form is not Clone, so rebuild it from media+body each attempt.
        let form = build_multipart_form(media, body);
        let resp = client.post(url).multipart(form).send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();

        if status.is_success()
            && parsed.get("ok").and_then(serde_json::Value::as_bool) == Some(true)
        {
            let result = parsed
                .get("result")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let (method, chat_id, thread, len, hash8) = rich_send_fields(url, body);
            let msg_id = result
                .get("message_id")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0) as i32;
            crate::channels::telegram::telemetry::log_send_success(
                origin,
                origin_detail,
                "-",
                "rich_api",
                method,
                chat_id,
                thread,
                msg_id,
                len,
                &hash8,
            );
            return Ok(result);
        }

        if status.as_u16() == 429 && attempt < RICH_MAX_RETRIES {
            let retry_after =
                crate::channels::telegram::rate_limit::parse_retry_after_json(&parsed)
                    .map_or(5, |d| d.as_secs());
            attempt += 1;
            let (_, chat_id, _, _, _) = rich_send_fields(url, body);
            // #676 - name the message this refusal was editing, so the line
            // can be joined to the send telemetry it belongs to.
            let msg = target_message_id(body);
            if matches!(
                crate::channels::telegram::rate_limit::wait_out(
                    "rich API",
                    std::time::Duration::from_secs(retry_after),
                    &format!(" (attempt {attempt}/{RICH_MAX_RETRIES}) msg={msg}"),
                    Some(chat_id),
                )
                .await,
                crate::channels::telegram::rate_limit::WaitOutcome::Deferred,
            ) {
                anyhow::bail!("Telegram rich call deferred by long 429 window");
            }
            continue;
        }

        let desc = parsed
            .get("description")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(&text);
        if status.as_u16() == 429 {
            tracing::warn!(
                "Rich API (multipart) still rate limited after {RICH_MAX_RETRIES} retries — falling back"
            );
        }
        {
            let (method, chat_id, thread, len, hash8) = rich_send_fields(url, body);
            let offenders = rich_body_offenders(body);
            crate::channels::telegram::telemetry::log_send_failure_with_offenders(
                origin,
                origin_detail,
                "-",
                "rich_api",
                method,
                chat_id,
                thread,
                len,
                &hash8,
                &format!("({status}): {desc}"),
                offenders.as_deref(),
            );
        }
        anyhow::bail!("Telegram rich API error ({status}): {desc}")
    }
}

/// Build the `sendRichMessage` body with markdown input + a `media` array
/// (Bot API 10.2+, #1044). Split out so the request shape is unit-testable
/// without a live bot. Matches the validated prototype (message 1073):
/// `rich_message: {markdown, media: [{id, media: {type, media:url}}]}`,
/// where `type` is the entry's own kind (#465) — `photo` for a picture,
/// `video` for a video the rich plane carries inline.
/// Carries `reply_parameters` when `reply_to` is set (#1230).
pub(crate) fn build_body_markdown_media_target(
    chat_id: i64,
    thread_id: Option<ThreadId>,
    reply_to: Option<i32>,
    markdown: &str,
    media: &[super::mermaid::MediaEntry],
    reply_markup: Option<&serde_json::Value>,
) -> serde_json::Value {
    let media_arr: Vec<serde_json::Value> = media
        .iter()
        .map(|m| {
            // Local PNG bytes upload via multipart as `attach://<id>`; URL
            // entries keep the legacy server-side fetch reference.
            let source = match (&m.bytes, &m.url) {
                (Some(_), _) => format!("attach://{}", m.id),
                (None, Some(url)) => url.clone(),
                (None, None) => String::new(),
            };
            serde_json::json!({
                "id": m.id,
                // The type comes from the entry's own kind (#465). It used to
                // be the literal "photo" for every entry, which is exactly the
                // mistake the anim-lab MIX probe hit as
                // `RICH_MESSAGE_VIDEO_INVALID` — a video reference generated
                // by the photo helper.
                "media": { "type": m.kind.as_str(), "media": source },
            })
        })
        .collect();
    let mut body = serde_json::json!({
        "chat_id": chat_id,
        "rich_message": { "markdown": super::normalize_rich_markdown_with_media(markdown, media), "media": media_arr },
    });
    if let Some(t) = thread_id {
        body["message_thread_id"] = serde_json::json!(t.0.0);
    }
    if let Some(mid) = reply_to {
        body["reply_parameters"] = serde_json::json!({ "message_id": mid });
    }
    // Inline keyboard (plan card Approve/Discard) rides every markdown+media
    // send, exactly like the html twin — omitting it clears a previous one.
    if let Some(kb) = reply_markup {
        body["reply_markup"] = kb.clone();
    }
    body
}

/// The API base with any trailing separator removed.
///
/// `Bot::api_url()` returns a parsed `Url`, and the URL spec normalises an
/// empty path to `/`, so `as_str()` yields `https://api.telegram.org/`.
/// Concatenating that produced `https://api.telegram.org//bot<token>/method`,
/// which Telegram rejects — every rich send failed and silently fell back to
/// plain HTML, so tool blocks stopped rendering as rich and completions
/// arrived as separate messages (#1117).
///
/// The tests that shipped with the parameterisation could not catch it:
/// mockito's `server.url()` has no trailing slash, so they exercised a shape
/// production never uses. teloxide builds these with `path_segments_mut()`
/// for the same reason.
fn api_base(api_url: &str) -> &str {
    api_url.trim_end_matches('/')
}

/// Send `markdown` as a native rich message, rendering any mermaid fences as
/// embedded images first (#1044). When a mermaid fence is present and the
/// feature is enabled, fences are resolved once and the message goes out via
/// the markdown dialect with a `media` array (so pipe tables stay native);
/// if that send fails (e.g. a Bot API server < 10.2 without the `media`
/// field) it falls back to the HTML dialect. With no mermaid fence it stays
/// byte-identical to the plain rich-markdown path. Returns `Err` on
/// transport failure so the caller can fall back.
pub(crate) async fn send_rich_with_mermaid(
    api_url: &str,
    token: &str,
    chat_id: i64,
    thread_id: Option<ThreadId>,
    markdown: &str,
    origin: &str,
    origin_detail: &str,
) -> anyhow::Result<()> {
    send_rich_with_mermaid_id(
        api_url,
        token,
        chat_id,
        thread_id,
        markdown,
        None,
        origin,
        origin_detail,
    )
    .await
    .map(|_| ())
}

#[allow(clippy::too_many_arguments)]
/// Same as [`send_rich_with_mermaid`] but returns the new message id together
/// with any media entries the retry ladder sacrificed (#897).
pub(crate) async fn send_rich_with_mermaid_id(
    api_url: &str,
    token: &str,
    chat_id: i64,
    thread_id: Option<ThreadId>,
    markdown: &str,
    reply_to: Option<i32>,
    origin: &str,
    origin_detail: &str,
) -> anyhow::Result<(i32, Vec<SacrificedEntry>)> {
    send_rich_with_mermaid_target_id(
        api_url,
        token,
        chat_id,
        thread_id,
        reply_to,
        markdown,
        origin,
        origin_detail,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
/// Like [`send_rich_with_mermaid_id`] but carries an optional Telegram reply
/// target (`reply_parameters`) on the rich send, so a rich reply lands
/// threaded to an existing message (#1230).
pub(crate) async fn send_rich_with_mermaid_target_id(
    api_url: &str,
    token: &str,
    chat_id: i64,
    thread_id: Option<ThreadId>,
    reply_to: Option<i32>,
    markdown: &str,
    origin: &str,
    origin_detail: &str,
) -> anyhow::Result<(i32, Vec<SacrificedEntry>)> {
    send_rich_with_media_target_id(
        api_url,
        token,
        chat_id,
        thread_id,
        reply_to,
        markdown,
        &[],
        origin,
        origin_detail,
    )
    .await
    // This form carries no media array of the caller's, so its receipt is the
    // "nothing to bind" one and the two-element shape stays as it was. The legs
    // that need the receipt (#923) call `send_rich_with_media_target_id`.
    .map(|(id, sacrificed, _receipt)| (id, sacrificed))
}

/// Rejection names that indict ONE media entry rather than the message body.
///
/// Telegram answers a `media` array it cannot use with a NAME, never an index:
/// `IMAGE_PROCESS_FAILED` when its own image processor refuses the bytes,
/// `*_INVALID` when a reference and its entry disagree about the kind, and
/// `*_NO_MEDIA_FOUND` when the reference has no entry to resolve against. Every
/// one of them is repairable by changing the ARRAY, so none is a verdict on the
/// text — which is what the pre-#826 code assumed when it downgraded the whole
/// reply to the HTML dialect on any error at all.
const MEDIA_ENTRY_REJECTIONS: [&str; 6] = [
    "IMAGE_PROCESS_FAILED",
    "PHOTO_INVALID",
    "PHOTO_NO_MEDIA_FOUND",
    "VIDEO_INVALID",
    "DOCUMENT_INVALID",
    "MEDIA_INVALID",
];

/// How many entries may be sacrificed before the HTML dialect is the better
/// answer. Each sacrifice is a round trip, and a rejection that survives two of
/// them is not about a single entry.
const MAX_MEDIA_SACRIFICES: usize = 2;

/// Whether `err` names a media entry as the reason the rich send was refused.
pub(crate) fn is_media_entry_rejection(err: &str) -> bool {
    MEDIA_ENTRY_REJECTIONS
        .iter()
        .any(|marker| err.contains(marker))
}

/// The family a rejection name points at, so the first entry sacrificed is the
/// one Telegram was talking about.
fn rejection_family(err: &str) -> Option<super::mermaid::MediaKind> {
    use super::mermaid::MediaKind;
    if err.contains("IMAGE_PROCESS_FAILED") || err.contains("PHOTO_") {
        Some(MediaKind::Photo)
    } else if err.contains("VIDEO_") {
        Some(MediaKind::Video)
    } else if err.contains("DOCUMENT_") {
        Some(MediaKind::Document)
    } else {
        None
    }
}

/// The family a refusal names when `media` carries no entry of it (#896).
///
/// Telegram names a family and never an index, so a family-naming refusal is a
/// hypothesis the ARRAY can test — but only while the array holds a candidate
/// for it. With none, the hypothesis is unactionable: the refusal is not about
/// any entry in this message, and sacrificing an innocent document to satisfy
/// it trades a delivered attachment for a body Telegram refuses again.
///
/// Measured 2026-11-18T20:41:26.873754Z: `RICH_MESSAGE_PHOTO_INVALID` on a
/// document-only reply sacrificed `doc0 (passive-recall-precision.md)`, and the
/// retry landed `msg=100166` at `len=2828` against the refused `len=2829` —
/// exactly one byte lighter, i.e. the document reference was the only thing
/// removed and the reader got a dead `📄` where the file should have been.
pub(crate) fn unactionable_family(
    media: &[super::mermaid::MediaEntry],
    err: &str,
) -> Option<super::mermaid::MediaKind> {
    let family = rejection_family(err)?;
    if media.iter().any(|entry| entry.kind == family) {
        None
    } else {
        Some(family)
    }
}

/// Indices into `media`, most-likely-guilty first.
///
/// The rejection names a family and never an entry, so this order is a
/// hypothesis the retry TESTS: entries of the family the error names come
/// first, then every other entry in array order. A refused photo therefore
/// costs a photo, not the six documents travelling beside it (#826).
///
/// An EMPTY order means the hypothesis is unactionable — the named family has
/// no entry here (#896). The ladder reads that as "nothing to sacrifice" and
/// falls through to the HTML dialect, because a family the message does not
/// carry cannot be repaired by deleting something the message does carry.
pub(crate) fn sacrifice_order(media: &[super::mermaid::MediaEntry], err: &str) -> Vec<usize> {
    if unactionable_family(media, err).is_some() {
        return Vec::new();
    }
    let family = rejection_family(err);
    let mut order: Vec<usize> = (0..media.len()).collect();
    order.sort_by_key(|i| match family {
        Some(kind) if media[*i].kind != kind => 1,
        _ => 0,
    });
    order
}

/// One media entry the retry ladder dropped from a reply that still delivered
/// (#897).
///
/// Sacrificing an entry is the right trade — it keeps the rest of the message
/// on the rich plane — but until #897 the fact died at the ladder's own
/// `tracing::warn!`: the caller saw a plain `Ok(id)`, its failure vectors
/// stayed empty, and nothing told the model the attachment was gone. Carrying
/// the entries out alongside the message id is what lets delivery report the
/// loss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SacrificedEntry {
    /// The entry's media id (`imgN`, `docN`, `diagN`) — the id its
    /// `tg://…?id=` reference in the body carried.
    pub(crate) id: String,
    /// The multipart part name Telegram would have shown (`q3-report.pdf`),
    /// when the producer had one. `None` for photo and video entries, whose
    /// part name is derived from their bytes.
    pub(crate) name: Option<String>,
    /// Which media family the entry belonged to, so the caller extends the
    /// matching failure vector.
    pub(crate) kind: super::mermaid::MediaKind,
}

impl SacrificedEntry {
    /// The entry as the retry ladder logs it and the correction nudge quotes
    /// it: `doc0 (q3-report.pdf)`, or the bare id when the producer carried
    /// no name.
    pub(crate) fn label(&self) -> String {
        match self.name.as_deref() {
            Some(name) => format!("{} ({name})", self.id),
            None => self.id.clone(),
        }
    }

    /// The same label, from the entry that was dropped.
    fn of(entry: &super::mermaid::MediaEntry) -> Self {
        Self {
            id: entry.id.clone(),
            name: entry.name.clone(),
            kind: entry.kind,
        }
    }
}

/// Retry a refused rich media send with offending entries removed (#826 AC-3):
/// one entry Telegram will not process must not cost the whole media plane.
///
/// Two legs, in the order of what is actually known:
///
/// 1. One identical retry, as a HEDGE — not a measurement. The claim this leg
///    originally rested on ("the SAME body was accepted 0.9 s later, so
///    `IMAGE_PROCESS_FAILED` is a flake") was FALSE, corrected 2026-11-13 from
///    the same log: the refused request carried six documents and two pictures
///    at `len=2176`, and the send that succeeded 0.9 s later was the HTML
///    dialect at `len=2588` with NO media array — the fallback, not a retry of
///    the same body. Nothing has ever measured this rejection as transient, and
///    the one case attributed to it turned out to be a corrupt input file
///    (#829: a truncated PNG that Telegram correctly refused). The retry stays
///    because one extra request is cheap insurance against a processor that is
///    momentarily unhappy, and it is labelled a hedge so no reader inherits a
///    flake theory the bytes do not support.
/// 2. Entries are then sacrificed one at a time in [`sacrifice_order`], each
///    sacrifice rewriting the body's now-dead reference into its
///    non-triggering form — a reference its array cannot answer is exactly the
///    whole-message 400 §3.3 and #334 are about. THIS leg is the one the
///    evidence supports: a bad file costs its own entry, never the plane.
///
/// `Err` means every leg failed and the caller falls back to the HTML dialect.
/// `Ok((id, sacrificed))` means the message landed — and names every entry the
/// ladder had to drop to get it there (#897), so the caller can report the loss
/// instead of letting a successful send read as a complete one.
#[allow(clippy::too_many_arguments)]
async fn retry_rich_media_around_offender(
    api_url: &str,
    token: &str,
    chat_id: i64,
    thread_id: Option<ThreadId>,
    reply_to: Option<i32>,
    markdown: &str,
    media: &[super::mermaid::MediaEntry],
    origin: &str,
    origin_detail: &str,
    err: &str,
) -> anyhow::Result<(i32, Vec<SacrificedEntry>, RichMediaReceipt)> {
    use super::mermaid::neutralize_orphan_photo_refs;

    if err.contains("IMAGE_PROCESS_FAILED") {
        tracing::warn!(
            "rich media send refused ({err}) — retrying the identical request once \
             before sacrificing any entry (hedge: the refusal may be transient)"
        );
        if let Ok((id, receipt)) = send_rich_markdown_media_target_receipt(
            api_url,
            token,
            chat_id,
            thread_id,
            reply_to,
            markdown,
            media,
            None,
            origin,
            origin_detail,
        )
        .await
        {
            // The hedge sacrifices nothing — the identical body landed whole.
            return Ok((id, Vec::new(), receipt));
        }
    }

    let order = sacrifice_order(media, err);
    if order.is_empty() {
        // #896 AC-1: the refusal names a family this message does not carry, so
        // no sacrifice can test it. Spending an innocent entry here would trade
        // a delivered attachment for a body Telegram refuses again — the caller
        // falls back to the HTML dialect instead, losing tables, not files.
        let carried: Vec<String> = media
            .iter()
            .map(|entry| SacrificedEntry::of(entry).label())
            .collect();
        match unactionable_family(media, err) {
            // AC-3: the next occurrence must be diagnosable from the log alone —
            // name the family Telegram named AND what was carried instead.
            Some(family) => anyhow::bail!(
                "rich media retry unactionable: the refusal names {family:?} but this message \
                 carries no {family:?} entry — nothing sacrificed (carrying [{}]); \
                 falling back to html",
                carried.join(", ")
            ),
            None => anyhow::bail!("rich media retry found no entry to sacrifice ({err})"),
        }
    }
    let mut dropped: Vec<usize> = Vec::new();
    for index in order.into_iter().take(MAX_MEDIA_SACRIFICES) {
        dropped.push(index);
        let kept: Vec<super::mermaid::MediaEntry> = media
            .iter()
            .enumerate()
            .filter(|(i, _)| !dropped.contains(i))
            .map(|(_, entry)| entry.clone())
            .collect();
        // The same list the log line has always printed, kept alive for the
        // caller (#897) instead of dying here.
        let sacrificed: Vec<SacrificedEntry> = dropped
            .iter()
            .map(|i| SacrificedEntry::of(&media[*i]))
            .collect();
        let lost: Vec<String> = sacrificed.iter().map(SacrificedEntry::label).collect();
        tracing::warn!(
            "rich media send refused ({err}) — retrying without {} entr{} [{}]; \
             the dropped reference is neutralised so it cannot 400 the retry",
            dropped.len(),
            if dropped.len() == 1 { "y" } else { "ies" },
            lost.join(", ")
        );
        // The reference for a dropped entry is now unanswered by the array, so
        // it is rewritten into its inert form before the retry goes out (§3.3).
        let body = neutralize_orphan_photo_refs(markdown, &kept);
        let retried: anyhow::Result<(i32, RichMediaReceipt)> = if kept.is_empty() {
            send_rich_markdown_target_id(
                api_url,
                token,
                chat_id,
                thread_id,
                reply_to,
                &body,
                origin,
                origin_detail,
            )
            .await
            // Nothing is left to bind, so the reduced send's own account is the
            // one that matters — and it is read off THIS response, not the
            // refusal that preceded it (#923).
            .map(|id| (id, RichMediaReceipt::no_media_array()))
        } else {
            send_rich_markdown_media_target_receipt(
                api_url,
                token,
                chat_id,
                thread_id,
                reply_to,
                &body,
                &kept,
                None,
                origin,
                origin_detail,
            )
            .await
        };
        if let Ok((id, receipt)) = retried {
            return Ok((id, sacrificed, receipt));
        }
    }

    // AC-3 (#896): the next occurrence must be diagnosable from the log alone —
    // name the family the refusals named AND every entry this ladder destroyed
    // before giving up, so a dead `📄` reference in the shipped reply can be
    // traced back to the sacrifice that created it.
    let destroyed: Vec<String> = dropped
        .iter()
        .map(|index| SacrificedEntry::of(&media[*index]).label())
        .collect();
    anyhow::bail!(
        "rich media retry exhausted after {MAX_MEDIA_SACRIFICES} sacrifices: \
         family {:?}, destroyed [{}] ({err})",
        rejection_family(err),
        destroyed.join(", ")
    )
}

#[allow(clippy::too_many_arguments)]
/// [`send_rich_with_mermaid_target_id`] with a caller-supplied media array
/// (#502): the composition core both senders now share.
///
/// `local_media` carries entries whose ids the CALLER has already rewritten
/// into `markdown` as `tg://photo?id=<id>` references — the media array is the
/// sole authority for such a reference (#334), so a ref without its entry would
/// be shielded away and the picture lost. Local entries use the `imgN` id
/// namespace and cannot collide with the resolver's `diagN`.
///
/// The two media sources are merged, and the plain-markdown early return keys
/// on the MERGED array: a message with no fence but with local media still has
/// something to embed, and taking the old early return would send the `tg://`
/// references as dead text — the exact silent loss this fixes.
///
/// Returns the message id together with every media entry the retry ladder had
/// to sacrifice to get the message out (#897). A clean send answers with an
/// empty list; a send that lost an attachment answers with it named, so the
/// caller can report the loss rather than record a plain success.
///
/// #923: the third element is the response's own account of which documents it
/// BOUND. `sacrificed` covers entries this ladder REMOVED after a refusal; the
/// receipt covers the other half — an entry the server discarded silently while
/// answering 200, which nothing could see before.
pub(crate) async fn send_rich_with_media_target_id(
    api_url: &str,
    token: &str,
    chat_id: i64,
    thread_id: Option<ThreadId>,
    reply_to: Option<i32>,
    markdown: &str,
    local_media: &[super::mermaid::MediaEntry],
    origin: &str,
    origin_detail: &str,
) -> anyhow::Result<(i32, Vec<SacrificedEntry>, RichMediaReceipt)> {
    // Resolve every fence once: valid diagrams become markdown media
    // references, broken ones become legible failure blocks. Non-fence text
    // is left byte-identical. Runs ONLY when a fence is present, exactly as
    // before — the local-image rewrite happened upstream of this call, on text
    // the fence resolver had not touched, and each pass recomputes its own
    // offsets on its own input.
    let (resolved, mut media) = if mermaid::should_render_mermaid(markdown) {
        let (text, media) = mermaid::resolve_markdown_media(markdown).await;
        (text, media)
    } else {
        (markdown.to_string(), Vec::new())
    };
    media.extend(local_media.iter().cloned());

    // Nothing to embed at all (no fence, no local media, or every fence failed)
    // → send it as plain rich markdown with no `media` field.
    if media.is_empty() {
        // No media array at all: nothing could bind, and the receipt says so
        // rather than leaving a caller to guess (#923).
        return send_rich_markdown_target_id(
            api_url,
            token,
            chat_id,
            thread_id,
            reply_to,
            &resolved,
            origin,
            origin_detail,
        )
        .await
        .map(|id| (id, Vec::new(), RichMediaReceipt::no_media_array()));
    }

    // Primary: markdown dialect + media array keeps pipe tables native.
    match send_rich_markdown_media_target_receipt(
        api_url,
        token,
        chat_id,
        thread_id,
        reply_to,
        &resolved,
        &media,
        None,
        origin,
        origin_detail,
    )
    .await
    {
        Ok((id, receipt)) => Ok((id, Vec::new(), receipt)),
        Err(e) => {
            // #826 AC-3: a media entry Telegram refuses to process is not a
            // verdict on the message. Before #826 every error here — a single
            // refused picture included — downgraded the WHOLE reply to the HTML
            // dialect, which flattens tables and detaches every document. The
            // retry ladder tries to keep the rich plane instead.
            let err_text = format!("{e}");
            if is_media_entry_rejection(&err_text) && !media.is_empty() {
                match retry_rich_media_around_offender(
                    api_url,
                    token,
                    chat_id,
                    thread_id,
                    reply_to,
                    &resolved,
                    &media,
                    origin,
                    origin_detail,
                    &err_text,
                )
                .await
                {
                    Ok((id, sacrificed, receipt)) => return Ok((id, sacrificed, receipt)),
                    Err(retry_err) => {
                        tracing::warn!(
                            "rich media retry did not recover ({retry_err}); \
                             falling back to html dialect"
                        );
                    }
                }
            }
            // Fallback: HTML dialect (Bot API < 10.2 or a media-field
            // rejection). Tables degrade there, but the message still lands.
            //
            // The `_p` renderer, NOT the bare one (#629). This is the rich
            // HTML dialect, where a bare newline is INSIGNIFICANT whitespace:
            // the bare renderer joins blocks with `\n\n` and emits a
            // paragraph as bare inline text (render_html.rs:29 / :61), so
            // every block of the reply collapsed into one wall of text —
            // measured on a 4 483-char final reply, 3 newlines over 4 123
            // chars of prose and 0 pipes. `render_html_p` gives each block its
            // own `<p>` and renders soft breaks as `<br>`, which is what the
            // plan-card prose path already uses for this same dialect, so the
            // card survived a 429 fallback and the final reply did not.
            tracing::warn!("rich markdown+media send failed ({e}); falling back to html dialect");
            let html = markdown_to_html_mermaid_p(markdown).await;
            send_rich_html_id(
                api_url,
                token,
                chat_id,
                thread_id,
                &html,
                None,
                origin,
                origin_detail,
            )
            .await
            // The HTML plane has no media array, so NOTHING is attached and
            // the receipt says so. Before #923 this path returned a bare
            // success, and the caller's floor had already subtracted the
            // documents against the REQUEST — so a fallback that carried no
            // entry dropped them silently, one plane over from the incident.
            .map(|id| (id, Vec::new(), RichMediaReceipt::no_media_array()))
        }
    }
}
