/// Extract `<<IMG:path>>` markers from text.
///
/// Returns `(cleaned_text, vec_of_paths)` — the text has all markers removed
/// and trimmed, the vec contains the file paths in order of appearance.
pub fn extract_img_markers(text: &str) -> (String, Vec<String>) {
    extract_markers_with_prefix(text, "<<IMG:")
}

/// Extract `<<VID:path>>` markers from text — mirror of `extract_img_markers`
/// for video attachments. Used by channel handlers to strip the marker from
/// bot replies before display (the agent shouldn't normally echo it back, but
/// strip defensively so a leaking marker never lands in front of the user).
pub fn extract_vid_markers(text: &str) -> (String, Vec<String>) {
    extract_markers_with_prefix(text, "<<VID:")
}

/// Extract `<<react:emoji>>` directive from text.
///
/// Returns `(cleaned_text, Option<emoji>)` — valid directives are removed
/// (text trimmed) and the first extracted emoji is returned. Multiple valid
/// directives are all stripped but only the first emoji is returned.
///
/// The LLM outputs `<<react:👍>>` to signal a reaction-only response
/// (or a reaction alongside text). Channel handlers use the returned
/// emoji to call `set_message_reaction` on the user's message.
///
/// Both ends of the marker are matched tolerantly. The opening prefix: some
/// models escape the angle brackets and emit `<\react:` or `<\\react:` instead
/// of `<<react:`, and some drop the `react:` tag entirely and just double-
/// bracket the emoji, `<<✅>>` (see `match_react_open`). The closing terminator:
/// `>>`, an XML-style `</react>`, or a bare `>` all close the directive (see
/// `find_react_close`) — models trained on Cursor/Cline-style harnesses close
/// directives with `</tag>`, and that leaked mangled markers as raw text when
/// only `>>` was accepted. All of these normalize to the same extraction, so
/// the reaction still fires and the mangled marker never leaks into the chat as
/// raw text.
///
/// Unlike the `<<IMG:path>>` extractor this is deliberately strict, because
/// the marker can legitimately appear in PROSE when the agent talks about
/// the feature itself (docs, code review, this codebase). Two guards:
/// * the payload must look like an actual emoji (non-empty, ≤ 8 chars, no
///   ASCII) — `<<react:emoji>>` or `<<react:hello>>` written in prose stays
///   in the text and produces no reaction (a word payload once fired a bogus
///   REACTION_INVALID Telegram call and mutated the final text, breaking
///   exact-match dedup against the already-sent intermediate: both copies
///   of the message landed in the chat);
/// * occurrences inside backtick code spans are never treated as directives.
pub fn extract_react_marker(text: &str) -> (String, Option<String>) {
    extract_react_marker_inner(text, true)
}

/// Like [`extract_react_marker`] but ignores backtick code spans — a marker
/// inside `` `…` `` still fires and is stripped. Use ONLY where the marker is
/// known to be a genuine directive, not prose that might discuss the feature:
/// a reaction-notification turn's response, whose expected output IS a bare
/// `<<react:emoji>>`. Small models there wrap the marker in a code span and
/// narrate their reasoning, so the strict extractor misses it — leaving the
/// marker as visible `<code>` text and firing no reaction.
pub fn extract_react_marker_lenient(text: &str) -> (String, Option<String>) {
    extract_react_marker_inner(text, false)
}

fn extract_react_marker_inner(
    text_arg: &str,
    respect_code_spans: bool,
) -> (String, Option<String>) {
    // Pass 1: the plain scanner. Fires on canonical turns and preserves all
    // code-span semantics; the ONLY path taken when the text has no backticks.
    let plain = scan_react_text(text_arg, respect_code_spans);
    if plain.1.is_some() || !text_arg.contains('`') {
        return plain;
    }
    // Pass 2 (#1182): strict found nothing and backticks are present — try
    // orphan-fence recovery. A full junk-prefixed directive means the text is
    // a mangled REACTION TURN; recovery captures its emoji and returns the
    // remaining body. Prose about the feature disqualifies itself (real words
    // sit before the marker), so docs examples stay text. Recovery runs AFTER
    // strict, never instead of it: an empty prefix is legal fence-junk, so a
    // recovery-first order would eat every well-formed marker that merely
    // carries a trailing code fence.
    match recover_orphan_fenced_directive(text_arg) {
        Some((cleaned, emoji)) => (cleaned.trim().to_string(), Some(emoji)),
        None => plain,
    }
}

/// Single left-to-right scan extracting the first reaction marker, honouring
/// the code-span guard when `respect_code_spans` is set. Shared by both
/// passes of [`extract_react_marker_inner`].
fn scan_react_text(text: &str, respect_code_spans: bool) -> (String, Option<String>) {
    let mut out = String::with_capacity(text.len());
    let mut emoji: Option<String> = None;
    let mut in_code = false;
    let mut i = 0;

    while i < text.len() {
        let ch = text[i..].chars().next().expect("i lies on a char boundary");
        if ch == '`' {
            in_code = !in_code;
            out.push(ch);
            i += 1;
            continue;
        }
        if (!respect_code_spans || !in_code)
            && let Some(open_len) = match_react_open(&text[i..])
            && let Some((rel_end, term_len)) = find_react_close(&text[i + open_len..])
        {
            let payload = text[i + open_len..i + open_len + rel_end].trim();
            if is_reaction_emoji(payload) {
                if emoji.is_none() {
                    emoji = Some(payload.to_string());
                }
                i += open_len + rel_end + term_len; // past the terminator
                continue;
            }
        }
        out.push(ch);
        i += ch.len_utf8();
    }

    (out.trim().to_string(), emoji)
}

/// Recovery pass for reaction directives mangled by an orphan code fence (#1182).
///
/// Shape observed in production: the message OPENS with junk made only of
/// fence/lang tokens (`<`, backticks, `\`, `~`, whitespace, an ascii-alnum
/// language tag like `html`) and a valid directive sits inside that junk
/// region. That prefix shape cannot be legitimate prose about the feature
/// (real discussion puts the marker after words like "use" or ":"), so a full
/// match (open + terminator + emoji payload) found there means the text is a
/// mangled REACTION TURN: slice past the junk and drop the paired trailing
/// lone-fence line. Anything else returns borrowed and unchanged.
/// True when everything before the directive is orphan-fence debris: an
/// optional stray `<` or `\`, an opening ``` fence with an optional short
/// language tag, then only whitespace/backticks/angle-brackets. Any real
/// word (letters outside the lang-tag slot) disqualifies, so prose like
/// ``use `<<react:👍>>` to react`` keeps its code-span semantics (#1182).
fn is_fence_junk_prefix(prefix: &str) -> bool {
    let mut rest = prefix.trim_start_matches([' ', '\t', '\r', '\n']);
    if let Some(r) = rest.strip_prefix('<').or_else(|| rest.strip_prefix('\\')) {
        rest = r.trim_start_matches([' ', '\t', '\r', '\n']);
    }
    if let Some(r) = rest.strip_prefix("```") {
        rest = r;
        let tag_len = rest.chars().take_while(|c| c.is_alphanumeric()).count();
        if tag_len > 12 {
            return false;
        }
        rest = &rest[tag_len..];
    }
    rest.bytes()
        .all(|b| matches!(b, b'`' | b'<' | b'\\' | b' ' | b'\t' | b'\r' | b'\n'))
}

/// Recovery pass for reaction directives mangled by an orphan code fence (#1182).
///
/// Shape observed in production: the message OPENS with junk made only of
/// fence/lang tokens (`<`, backticks, `\`, whitespace, an ascii-alnum
/// language tag like `html`) and a valid directive sits inside that junk
/// region. That prefix shape cannot be legitimate prose about the feature
/// (real discussion puts the marker after words like "use" or ":"), so a full
/// match (open + terminator + emoji payload) found there means the text is a
/// mangled REACTION TURN. Returns the body after the directive with the paired
/// trailing lone-fence line dropped, plus the captured emoji; `None` when no
/// junk-prefixed directive exists.
fn recover_orphan_fenced_directive(text: &str) -> Option<(String, String)> {
    if !text.contains('`') {
        return None;
    }
    let scan_end = text
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(text.len()))
        .find(|&i| i >= 240)
        .unwrap_or(text.len());
    let mut found: Option<(usize, String)> = None;
    for (i, ch) in text[..scan_end].char_indices() {
        if ch != '<' {
            continue;
        }
        if !is_fence_junk_prefix(&text[..i]) {
            continue;
        }
        if let Some(open_len) = match_react_open(&text[i..])
            && let Some((rel_end, term_len)) = find_react_close(&text[i + open_len..])
            && is_reaction_emoji(text[i + open_len..i + open_len + rel_end].trim())
        {
            let emoji = text[i + open_len..i + open_len + rel_end]
                .trim()
                .to_string();
            found = Some((i + open_len + rel_end + term_len, emoji));
            break;
        }
    }
    let (after, emoji) = found?;
    let mut owned = text[after..].to_string();
    if let Some(pos) = owned.rfind('\n')
        && owned[pos + 1..].trim_start().starts_with("```")
    {
        owned.truncate(pos);
    }
    Some((owned, emoji))
}

/// Match a reaction-marker opening at the start of `s`, tolerating prefixes
/// mangled by models that escape the angle brackets. Accepts a leading `<`
/// followed by any run of `<` or `\` characters, then `react:`, so the
/// canonical `<<react:` as well as `<\react:`, `<\\react:`, and `<react:` all
/// match. Returns the byte length of the matched opening (through `react:`),
/// or `None` when `s` does not begin with a marker opening.
///
/// Also matches the keyword-LESS form `<<EMOJI>>`: some models drop the
/// `react:` tag entirely and just bracket the emoji. That form is accepted only
/// when the leading run has at least two `<` characters — a single-bracket
/// `<x>` is one char from HTML/emoticon noise (and one char from a bare-`>`
/// terminator), so it must stay prose. The payload is NOT validated here; the
/// caller's `is_reaction_emoji` guard still rejects word payloads, so
/// `<<hello>>` stays text and only a real `<<✅>>` fires.
///
/// All matched bytes are ASCII (`<`, `\`, `react:`), so the returned length
/// always lands on a char boundary.
fn match_react_open(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'<') {
        return None;
    }
    let mut j = 1;
    let mut angle_brackets = 1usize; // bytes[0] is '<'
    while let Some(c) = bytes.get(j) {
        match c {
            b'<' => {
                angle_brackets += 1;
                j += 1;
            }
            b'\\' => j += 1,
            _ => break,
        }
    }
    const TAG: &str = "react:";
    if s[j..].starts_with(TAG) {
        // Canonical keyword form: `<<react:` (any bracket count).
        Some(j + TAG.len())
    } else if angle_brackets >= 2 {
        // Keyword-less `<<EMOJI>>` — payload validated by the caller.
        Some(j)
    } else {
        None
    }
}

/// Find the earliest reaction-marker terminator in `s`, tolerating the strict
/// `>>` close as well as the `</react>` (XML-style close tag) and bare `>`
/// variants that models emit when they mangle the directive. Returns
/// `(offset, term_len)` — the byte offset where the terminator starts and its
/// byte length — or `None` when none is present.
///
/// When more than one candidate starts at the SAME offset the longest wins, so
/// a canonical `>>` is never mis-read as a bare `>` (which would strand the
/// trailing bracket in the output). All terminators are ASCII, so both the
/// offset and `offset + term_len` land on char boundaries.
fn find_react_close(s: &str) -> Option<(usize, usize)> {
    const TERMS: [&str; 3] = [">>", "</react>", ">"];
    let mut best: Option<(usize, usize)> = None;
    for term in TERMS {
        if let Some(pos) = s.find(term) {
            let better = match best {
                Some((bpos, blen)) => pos < bpos || (pos == bpos && term.len() > blen),
                None => true,
            };
            if better {
                best = Some((pos, term.len()));
            }
        }
    }
    best
}

/// A plausible reaction emoji: non-empty, short (compound emoji with skin
/// tones / VS-16 / ZWJ stay under 8 chars), and containing no ASCII — which
/// rejects words and placeholders like "emoji" or "hello" that appear when
/// the marker is mentioned in prose rather than used as a directive.
fn is_reaction_emoji(payload: &str) -> bool {
    !payload.is_empty() && payload.chars().count() <= 8 && payload.chars().all(|c| !c.is_ascii())
}

/// Generic `<<PREFIX:path>>` marker extractor. Walks the text, removes every
/// `<<PREFIX:...>>` occurrence, and collects the inner paths in order. UTF-8
/// safe (works on byte indices that lie on char boundaries — `find`/`replace_range`
/// handle that correctly for the ASCII delimiters used here).
fn extract_markers_with_prefix(text: &str, prefix: &str) -> (String, Vec<String>) {
    let mut out = text.to_string();
    let mut paths = Vec::new();

    while let Some(start) = out.find(prefix) {
        let Some((end, path)) = parse_marker_at(&out, start, prefix) else {
            break;
        };
        if !path.is_empty() {
            paths.push(path);
        }
        out.replace_range(start..end, "");
    }

    (out.trim().to_string(), paths)
}

// ── Local image extraction (#286) ─────────────────────────────────────────
//
// A channel reply can carry an image in two forms: the proprietary marker
// `<<IMG:path>>` and the standard markdown reference `![alt](path)`. Models
// emit the markdown form by default, and a chat platform cannot read a host
// path — so the reference must be recognized, resolved against the session
// working directory, validated, and handed to the channel as real media.
// This section is that ONE place: every channel calls
// [`extract_local_images`] instead of parsing the text itself.

use std::path::{Path, PathBuf};

/// Marker prefix for the proprietary image form.
const IMG_PREFIX: &str = "<<IMG:";

/// Why a local image candidate could not become an attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalImageFailureReason {
    /// No filesystem entry at the resolved path.
    NotFound,
    /// The path exists but is not a regular file (directory, socket, …).
    NotAFile,
    /// The file exists and is regular but holds zero bytes.
    Empty,
    /// The leading bytes match no supported image format.
    UnsupportedFormat,
    /// The file exists but could not be opened or read.
    Unreadable,
    /// A remote reference could not be fetched (connect error, timeout, or a
    /// non-success HTTP status).
    DownloadFailed,
    /// A remote reference resolved to more bytes than the per-image limit.
    TooLarge,
    /// The reply referenced more remote images than the per-reply budget.
    TooMany,
    /// A remote reference is not a usable image URL (`data:` payload that is
    /// not base64, a base64 body that is not an image, an unparsable URL).
    BadUrl,
    /// The image was extracted and validated, but the channel refused to send
    /// it (API error, media-type rejection, platform size ceiling). Distinct
    /// from every other reason: the reference is not the problem.
    DeliveryFailed,
}

impl LocalImageFailureReason {
    /// Short human- and model-facing phrase used in nudges and notices.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "file not found",
            Self::NotAFile => "not a regular file",
            Self::Empty => "file is empty (0 bytes)",
            Self::UnsupportedFormat => "not a supported image format (png/jpeg/gif/webp/bmp)",
            Self::Unreadable => "file could not be read",
            Self::DownloadFailed => {
                "could not be downloaded (unreachable, timed out, or HTTP error)"
            }
            Self::TooLarge => "larger than the per-image size limit",
            Self::TooMany => "too many remote images in one reply (per-reply limit reached)",
            Self::BadUrl => "not a usable image URL",
            Self::DeliveryFailed => "the channel could not deliver the image",
        }
    }
}

/// One local image reference that was removed from the reply but could not be
/// delivered, with the reason the model needs in order to fix it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalImageFailure {
    /// The reference exactly as it appeared in the reply text.
    pub raw: String,
    /// The path the reference resolved to, when resolution succeeded.
    pub resolved: Option<PathBuf>,
    /// Why the candidate was rejected.
    pub reason: LocalImageFailureReason,
}

impl LocalImageFailure {
    /// `raw (reason)` — the phrase quoted back to the model in a nudge.
    pub fn describe(&self) -> String {
        format!("{} ({})", self.raw, self.reason.as_str())
    }
}

/// Result of scanning a reply for image references.
#[derive(Debug, Clone, Default)]
pub struct LocalImageScan {
    /// Reply text with every local image reference removed. Remote targets and
    /// references inside code spans are left untouched.
    pub text: String,
    /// Resolved and validated local images, in order of appearance.
    pub attachments: Vec<PathBuf>,
    /// Remote image URLs awaiting a fetch, in order of appearance.
    pub remote: Vec<String>,
    /// Rejected local candidates, in order of appearance.
    pub failures: Vec<LocalImageFailure>,
}

/// Where an image reference points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageTarget {
    /// A network URL (`http://`, `https://`, `data:`) — the delivery layer
    /// fetches it so it ships as a native attachment like a local file.
    Remote(String),
    /// A local path, resolved to an absolute one.
    Local(PathBuf),
    /// A relative path with no base directory to resolve it against.
    Unresolved,
    /// A Telegram media reference (`tg://photo?id=…`, `tg://video|audio?id=…`,
    /// `attach://…`) — resolved SERVER-side against the rich request's `media`
    /// array, never against the filesystem (#334). Classifying one as a local
    /// path joined it to the session cwd, so the in-loop regen nudge quoted a
    /// nonsense `<cwd>/tg://photo?id=…` back to the model as actionable advice.
    MediaRef(String),
}

/// Per-byte predicate: `regions[i]` is true when byte `i` of `text` sits
/// inside a code span or fenced block. A backtick toggles the state, so a
/// fenced block (three backticks) and an inline span (one) both open and
/// close with the same rule — one home for "is this inside code", shared by
/// the markdown image scanner and the reaction-marker scanner.
pub fn code_regions(text: &str) -> Vec<bool> {
    let mut regions = vec![false; text.len()];
    let mut in_code = false;
    for (i, byte) in text.bytes().enumerate() {
        regions[i] = in_code;
        if byte == b'`' {
            in_code = !in_code;
        }
    }
    regions
}

/// Classify an image reference target and resolve local paths to absolute
/// ones: `~/…` through the shared tilde expander, absolute paths as-is, and
/// relative paths joined to `base_dir` (the session working directory).
pub fn classify_image_target(raw: &str, base_dir: Option<&Path>) -> ImageTarget {
    let trimmed = raw.trim();
    if is_remote_url(trimmed) {
        return ImageTarget::Remote(trimmed.to_string());
    }
    if is_telegram_media_ref(trimmed) {
        return ImageTarget::MediaRef(trimmed.to_string());
    }
    let expanded = crate::brain::tools::error::expand_tilde(trimmed);
    if expanded.is_absolute() {
        return ImageTarget::Local(expanded);
    }
    match base_dir {
        Some(dir) => ImageTarget::Local(dir.join(expanded)),
        None => ImageTarget::Unresolved,
    }
}

/// True when the reference is a Telegram media reference rather than a path.
/// These are resolved by Telegram against the `media` array of the rich request
/// that carries them, so a caller must never join one to the session cwd (#334).
pub(crate) fn is_telegram_media_ref(raw: &str) -> bool {
    let lower = raw.to_ascii_lowercase();
    lower.starts_with("tg://") || lower.starts_with("attach://")
}

/// True when the reference is a fetchable network URL rather than a local path.
/// `mailto:` and `ftp://` are deliberately absent: neither can yield image
/// bytes, so a reference carrying one is left in the text as written.
pub(crate) fn is_remote_url(raw: &str) -> bool {
    const SCHEMES: [&str; 3] = ["http://", "https://", "data:"];
    let lower = raw.to_ascii_lowercase();
    SCHEMES.iter().any(|scheme| lower.starts_with(scheme))
}

/// Validate a resolved local image candidate: it must exist, be a regular
/// file, hold at least one byte, and start with a supported image signature.
pub fn validate_local_image(path: &Path) -> Result<(), LocalImageFailureReason> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(LocalImageFailureReason::NotFound);
        }
        Err(_) => return Err(LocalImageFailureReason::Unreadable),
    };
    if !meta.is_file() {
        return Err(LocalImageFailureReason::NotAFile);
    }
    if meta.len() == 0 {
        return Err(LocalImageFailureReason::Empty);
    }
    let mut head = [0u8; 12];
    let mut file = std::fs::File::open(path).map_err(|_| LocalImageFailureReason::Unreadable)?;
    let read = std::io::Read::read(&mut file, &mut head)
        .map_err(|_| LocalImageFailureReason::Unreadable)?;
    if is_supported_image(&head[..read]) {
        Ok(())
    } else {
        Err(LocalImageFailureReason::UnsupportedFormat)
    }
}

/// Magic-byte check for the formats chat platforms accept as native images.
/// Minimum header lengths are enforced so short text that happens to start
/// with the same two ASCII letters (`BMW …`) is not read as an image.
pub(crate) fn is_supported_image(head: &[u8]) -> bool {
    const SIGNATURES: [&[u8]; 4] = [b"\x89PNG\r\n\x1a\n", b"\xff\xd8\xff", b"GIF87a", b"GIF89a"];
    if SIGNATURES.iter().any(|sig| head.starts_with(sig)) {
        return true;
    }
    if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        return true;
    }
    head.len() >= 14 && head.starts_with(b"BM")
}

/// The file extension for an image whose leading bytes are `head`, so a fetched
/// remote image lands on disk with a name channels and users recognise. Only
/// call with bytes [`is_supported_image`] has accepted.
pub(crate) fn image_extension(head: &[u8]) -> &'static str {
    if head.starts_with(b"\x89PNG") {
        "png"
    } else if head.starts_with(b"\xff\xd8\xff") {
        "jpg"
    } else if head.starts_with(b"GIF") {
        "gif"
    } else if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        "webp"
    } else {
        "bmp"
    }
}

/// Parse a `<<PREFIX:path>>` marker starting at `start`, which must lie on a
/// char boundary and begin `prefix`. Returns `(end_byte_exclusive, raw_path)`
/// with the payload trimmed, or `None` when no `>>` closes the marker before
/// the end of the text. The single marker-parsing rule: both the streaming
/// scanner below and [`extract_markers_with_prefix`] go through it.
fn parse_marker_at(text: &str, start: usize, prefix: &str) -> Option<(usize, String)> {
    debug_assert!(text[start..].starts_with(prefix));
    let rel_end = text[start..].find(">>")?;
    let payload_start = start + prefix.len();
    let payload_end = start + rel_end;
    if payload_start > payload_end {
        return None;
    }
    Some((
        payload_end + 2,
        text[payload_start..payload_end].trim().to_string(),
    ))
}

/// Parse a markdown image reference starting at `start` (a char boundary where
/// the text begins with `![`). Accepts `![alt](target)`, the angle-bracket form
/// `![alt](<target>)` that markdown requires when the path holds spaces, and an
/// optional `"title"` / `'title'` after the target. Returns
/// `(end_byte_exclusive, raw_target)`.
fn parse_markdown_image(text: &str, start: usize) -> Option<(usize, String)> {
    debug_assert!(text[start..].starts_with("!["));
    // `\![alt](path)` is escaped literal text, not a reference.
    if start > 0 && text[..start].ends_with('\\') {
        return None;
    }
    let alt_end = text[start + 2..].find(']')?;
    let paren = start + 2 + alt_end + 1;
    if !text[paren..].starts_with('(') {
        return None;
    }
    let cursor = skip_whitespace(text, paren + 1);
    let (target, mut after_target) = if text[cursor..].starts_with('<') {
        let close = text[cursor + 1..].find('>')?;
        let target = text[cursor + 1..cursor + 1 + close].to_string();
        (target, cursor + 1 + close + 1)
    } else {
        let mut end = cursor;
        while let Some(ch) = text[end..].chars().next() {
            if ch.is_whitespace() || ch == ')' {
                break;
            }
            end += ch.len_utf8();
        }
        (text[cursor..end].to_string(), end)
    };
    after_target = skip_whitespace(text, after_target);
    if let Some(quote) = text[after_target..].chars().next()
        && (quote == '"' || quote == '\'')
    {
        let close = text[after_target + 1..].find(quote)?;
        after_target = skip_whitespace(text, after_target + 1 + close + 1);
    }
    if !text[after_target..].starts_with(')') || target.trim().is_empty() {
        return None;
    }
    Some((after_target + 1, target))
}

/// Byte offset of the first non-whitespace char at or after `from`.
fn skip_whitespace(text: &str, from: usize) -> usize {
    let mut cursor = from;
    while let Some(ch) = text[cursor..].chars().next() {
        if !ch.is_whitespace() {
            break;
        }
        cursor += ch.len_utf8();
    }
    cursor
}

/// What a scan does with a REMOTE image reference (`http(s)://`, `data:`).
///
/// The two call-site families want opposite things, and getting it wrong is
/// silent: a delivery-bound scan must remove the reference because it is about
/// to ship the image itself, while a strip-only scan has no fetch step and
/// would delete the link outright.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteRefs {
    /// Collect the URL and remove the reference from the text. The delivery
    /// layer fetches it and ships it as native media; leaving it in place would
    /// show the user the same picture twice.
    Fetch,
    /// Leave the reference in the text and collect nothing. Used by strip-only
    /// call sites, which have nothing to fetch with — deleting a link there
    /// would lose it, and recording it as "awaiting a fetch" would be a lie.
    KeepInText,
}

/// File one parsed image reference into the scan accumulators. Returns `true`
/// when the reference was consumed and must leave the reply text.
///
/// `strip_unresolved` separates the two forms: a marker (`<<IMG:…>>`) always
/// leaves the text, because a marker is never prose; a markdown reference whose
/// relative target has no base directory to resolve against stays verbatim, as
/// it may be ordinary prose that merely looks like a reference.
///
/// A marker's remote target always leaves the text whatever `remote` says — a
/// marker is machine syntax, never prose, so there is no link to preserve.
fn record_candidate(
    raw: String,
    base_dir: Option<&Path>,
    strip_unresolved: bool,
    remote: RemoteRefs,
    scan: &mut LocalImageScan,
) -> bool {
    match classify_image_target(&raw, base_dir) {
        ImageTarget::Local(path) => {
            match validate_local_image(&path) {
                Ok(()) => scan.attachments.push(path),
                Err(reason) => scan.failures.push(LocalImageFailure {
                    raw,
                    resolved: Some(path),
                    reason,
                }),
            }
            true
        }
        ImageTarget::Remote(url) => {
            // Only a delivery scan has a fetch step, and `LocalImageScan::remote`
            // means "awaiting a fetch" — so a strip-only scan records nothing and
            // removal falls back to the marker/markdown distinction.
            if remote == RemoteRefs::Fetch {
                scan.remote.push(url);
                true
            } else {
                strip_unresolved
            }
        }
        ImageTarget::MediaRef(_) => {
            // Not a file and not fetchable by us: Telegram resolves it against the
            // `media` array of the rich request that carries it (#334). Recording it
            // as a local failure is what produced the cwd-joined nonsense nudge.
            // Nothing to attach, nothing to fetch — leave the text as written.
            strip_unresolved
        }
        ImageTarget::Unresolved => {
            if strip_unresolved {
                scan.failures.push(LocalImageFailure {
                    raw,
                    resolved: None,
                    reason: LocalImageFailureReason::NotFound,
                });
            }
            strip_unresolved
        }
    }
}

/// Scan a reply for image references in both forms and hand back the text
/// without them, the validated local attachments, the remote URLs awaiting a
/// fetch, and the rejected candidates.
///
/// `base_dir` is the session working directory: a relative path resolves
/// against it. With no base directory (a strip-only call site that has no
/// session handle) a relative markdown target stays verbatim in the text,
/// while `~`-prefixed and absolute targets still resolve and deliver.
///
/// Removal semantics: a validated image leaves the text and becomes an
/// attachment; a rejected local candidate leaves the text and becomes a
/// failure (dead markdown never reaches the user, and the failure is what
/// drives the self-healing nudge). Remote targets and anything inside a code
/// span are collected as [`LocalImageScan::remote`] rather than left as bare
/// markdown — the delivery layer fetches them so they ship as native media.
///
/// For a call site with no fetch step, use [`strip_image_references`]: it keeps
/// a remote reference in the text instead of deleting a link nobody will
/// replace.
pub fn extract_local_images(text: &str, base_dir: Option<&Path>) -> LocalImageScan {
    scan_image_references(text, base_dir, RemoteRefs::Fetch)
}

/// Strip-only variant of [`extract_local_images`] for call sites that sanitise
/// text without delivering anything (intermediate bubbles, command detection,
/// reaction prompts). Local markers and validated paths still leave the text —
/// the delivery path re-runs extraction on the final reply — but a remote
/// reference stays in place, because nothing downstream will fetch it.
pub fn strip_image_references(text: &str, base_dir: Option<&Path>) -> LocalImageScan {
    scan_image_references(text, base_dir, RemoteRefs::KeepInText)
}

fn scan_image_references(
    text: &str,
    base_dir: Option<&Path>,
    remote: RemoteRefs,
) -> LocalImageScan {
    let regions = code_regions(text);
    let mut scan = LocalImageScan {
        text: String::with_capacity(text.len()),
        ..LocalImageScan::default()
    };
    let mut i = 0;

    while i < text.len() {
        if text[i..].starts_with(IMG_PREFIX)
            && let Some((end, raw)) = parse_marker_at(text, i, IMG_PREFIX)
        {
            if !raw.is_empty() {
                record_candidate(raw, base_dir, true, remote, &mut scan);
            }
            i = end;
            continue;
        }
        if !regions[i]
            && text[i..].starts_with("![")
            && let Some((end, raw)) = parse_markdown_image(text, i)
            && record_candidate(raw, base_dir, false, remote, &mut scan)
        {
            i = end;
            continue;
        }
        let ch = text[i..].chars().next().expect("i lies on a char boundary");
        scan.text.push(ch);
        i += ch.len_utf8();
    }

    scan.text = scan.text.trim().to_string();
    scan
}

/// The honest user-visible line for images that could not be delivered, used
/// when the self-healing nudge budget is exhausted. `None` when there is
/// nothing to report.
pub fn failure_notice(failures: &[LocalImageFailure]) -> Option<String> {
    if failures.is_empty() {
        return None;
    }
    let mut notice = String::from(
        "⚠️ Image not attached — the reply referenced an image that could not be delivered:",
    );
    for failure in failures {
        notice.push_str("\n- ");
        notice.push_str(&failure.describe());
    }
    Some(notice)
}

/// Append [`failure_notice`] to a reply body, separated by a blank line.
///
/// Every channel's delivery site uses this shape, and the empty-body case is
/// why it lives here rather than being spelled out at each site: a reply whose
/// ONLY content was a broken image reference leaves an empty body, and
/// `format!("{body}\n\n{notice}")` would then deliver the notice as a
/// blank-line-prefixed message (or, worse, be skipped by a downstream
/// emptiness check and say nothing at all). An empty body becomes the notice.
pub fn append_failure_notice(body: &str, failures: &[LocalImageFailure]) -> String {
    match failure_notice(failures) {
        None => body.to_string(),
        Some(notice) => {
            if body.trim().is_empty() {
                notice
            } else {
                format!("{}\n\n{notice}", body.trim_end())
            }
        }
    }
}
