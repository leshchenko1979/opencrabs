//! Fetch remote image references so they ship as native channel attachments.
//!
//! A reply that writes `![alt](https://example.com/chart.png)` — or
//! `<<IMG:https://…>>` — names an image the user should SEE. Leaving it as
//! bare markdown relies on the client to render it, which fails for private
//! hosts, redirects, and anything the chat platform's own URL fetcher refuses.
//! Instead the bytes are fetched here, validated against the same magic-byte
//! rule a local file must pass, written to a temp path, and handed to the
//! delivery layer exactly like a local attachment — one code path downstream.
//!
//! Kept apart from [`super::image`] on purpose: that module is pure text
//! analysis (sync, no runtime, fully unit-testable), this one is network I/O.

use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;

use super::image::{
    LocalImage, LocalImageFailure, LocalImageFailureReason, LocalImageScan, image_extension,
    is_remote_url, is_supported_image,
};

/// Wall-clock ceiling for one remote fetch.
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
/// Ceiling on the connect phase alone, so a black-holed host fails fast.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
/// Per-image byte ceiling. Matches the size a chat platform accepts as a photo
/// without recompression surprises.
pub const MAX_REMOTE_IMAGE_BYTES: usize = 20 * 1024 * 1024;
/// Per-reply ceiling on how many remote images are fetched, so a reply that
/// dumps a gallery cannot stall delivery for minutes.
pub const MAX_REMOTE_IMAGES_PER_REPLY: usize = 10;
/// Redirect hops allowed on a fetch.
const MAX_REDIRECTS: usize = 5;

/// Fetch every remote image a scan collected, folding the successes into
/// `attachments` and the failures into `failures`.
///
/// Consumes and returns the scan so call sites stay a single expression:
/// `let scan = resolve_remote_images(extract_local_images(text, dir)).await;`
pub async fn resolve_remote_images(mut scan: LocalImageScan) -> LocalImageScan {
    if scan.remote.is_empty() {
        return scan;
    }
    let pending = std::mem::take(&mut scan.remote);
    let client = match build_client() {
        Ok(client) => client,
        Err(err) => {
            // No client at all (TLS backend unavailable): report every
            // candidate rather than silently dropping the images.
            tracing::warn!("remote image fetch unavailable: {err}");
            for url in pending {
                scan.failures
                    .push(remote_failure(url, LocalImageFailureReason::DownloadFailed));
            }
            return scan;
        }
    };
    for (index, url) in pending.into_iter().enumerate() {
        if index >= MAX_REMOTE_IMAGES_PER_REPLY {
            scan.failures.push(LocalImageFailure {
                raw: url,
                resolved: None,
                reason: LocalImageFailureReason::TooMany,
            });
            continue;
        }
        match fetch_one(&client, &url).await {
            // A fetched remote image carries no caption: the markdown title of a
            // remote reference is dropped at collection time (`remote` stays a
            // list of URLs), and the rich plane captions a remote reference
            // server-side anyway (CAP-2). Local files are the leg that needs the
            // carried title.
            Ok(path) => scan.attachments.push(LocalImage { path, caption: None }),
            Err(reason) => scan.failures.push(remote_failure(url, reason)),
        }
    }
    scan
}

/// Fetch a single remote image and materialise it as a local file.
///
/// `data:` URLs are decoded inline (no network); `http(s)` URLs are streamed
/// with the byte ceiling enforced as the body arrives, so an oversized or
/// endless response is cut off rather than buffered.
pub async fn fetch_remote_image(url: &str) -> Result<PathBuf, LocalImageFailureReason> {
    let client = build_client().map_err(|_| LocalImageFailureReason::DownloadFailed)?;
    fetch_one(&client, url).await
}

/// Shared client builder: bounded timeouts, no cookie jar, redirects capped.
fn build_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(FETCH_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
        .build()
        .map_err(|err| format!("image fetch client unavailable: {err}"))
}

async fn fetch_one(
    client: &reqwest::Client,
    url: &str,
) -> Result<PathBuf, LocalImageFailureReason> {
    if !is_remote_url(url) {
        // The caller only ever hands over remote targets; anything else is a
        // programming error, reported honestly rather than guessed at.
        return Err(LocalImageFailureReason::BadUrl);
    }
    if url.to_ascii_lowercase().starts_with("data:") {
        decode_data_url(url)
    } else {
        download(client, url).await
    }
}

/// Decode a `data:` URL (`data:image/png;base64,AAAA…`) into a temp file.
fn decode_data_url(url: &str) -> Result<PathBuf, LocalImageFailureReason> {
    let (header, body) = url.split_once(',').ok_or(LocalImageFailureReason::BadUrl)?;
    if !header.to_ascii_lowercase().contains("base64") {
        // Percent-encoded data URLs for binary images are vanishingly rare and
        // not worth a decoder; report rather than guess.
        return Err(LocalImageFailureReason::BadUrl);
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(body.trim())
        .map_err(|_| LocalImageFailureReason::BadUrl)?;
    materialise(&bytes)
}

/// Stream a remote body to a temp file, enforcing the size ceiling as the
/// bytes arrive so an oversized or endless response is cut off rather than
/// buffered whole.
async fn download(client: &reqwest::Client, url: &str) -> Result<PathBuf, LocalImageFailureReason> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|_| LocalImageFailureReason::DownloadFailed)?;
    if !response.status().is_success() {
        return Err(LocalImageFailureReason::DownloadFailed);
    }
    if let Some(len) = response.content_length()
        && len as usize > MAX_REMOTE_IMAGE_BYTES
    {
        return Err(LocalImageFailureReason::TooLarge);
    }

    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| LocalImageFailureReason::DownloadFailed)?
    {
        if bytes.len() + chunk.len() > MAX_REMOTE_IMAGE_BYTES {
            return Err(LocalImageFailureReason::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    materialise(&bytes)
}

/// Validate fetched bytes as an image and write them to a temp file with the
/// extension their signature implies. The single gate between "some bytes
/// arrived" and "this is attachable media".
fn materialise(bytes: &[u8]) -> Result<PathBuf, LocalImageFailureReason> {
    if bytes.is_empty() {
        return Err(LocalImageFailureReason::Empty);
    }
    if !is_supported_image(bytes) {
        return Err(LocalImageFailureReason::UnsupportedFormat);
    }
    let name = format!("remote-image.{}", image_extension(bytes));
    super::file_extract::save_to_temp(bytes, &name).map_err(|_| LocalImageFailureReason::Unreadable)
}

fn remote_failure(raw: String, reason: LocalImageFailureReason) -> LocalImageFailure {
    LocalImageFailure {
        raw,
        resolved: None,
        reason,
    }
}
