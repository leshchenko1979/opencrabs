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
    extract_markers_with_prefix(text, VID_PREFIX)
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

/// Reaction-path-only companion of [`extract_react_marker_lenient`] (#1670).
///
/// Removes every remaining marker-shaped occurrence whose payload fails
/// [`is_reaction_emoji`] — the empty `<<react:>>` (a model that forgot its
/// emoji), word payloads, keyword-less `<<noise>>` brackets. Such a marker
/// survives extraction as visible text and used to be delivered as a bubble;
/// in a forum group that bubble landed in General, in front of a dormant
/// topic (#1670). A reaction turn's output is a directive or the model's
/// narration — never prose discussing the feature — so unlike the strict
/// extractor this strips regardless of code spans and has no prose guard.
/// Valid markers are LEFT (the extractor already consumed them; leaving them
/// keeps this function idempotent-safe if ever called first). Unterminated
/// openings (`<<react:` with no terminator) are LEFT: there is no close to
/// anchor a strip to, and eating the rest of the text would be a worse leak.
pub fn strip_invalid_react_markers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let ch = text[i..].chars().next().expect("i lies on a char boundary");
        if ch == '<'
            && let Some(open_len) = match_react_open(&text[i..])
            && let Some((rel_end, term_len)) = find_react_close(&text[i + open_len..])
        {
            let payload = text[i + open_len..i + open_len + rel_end].trim();
            if !is_reaction_emoji(payload) {
                i += open_len + rel_end + term_len; // debris: drop the marker
                // A code-wrapped marker (`<<react:>>` in backticks) would
                // strand its delimiters as "``" — a still-visible empty
                // bubble. Pop one adjacent backtick from each side so the
                // whole span disappears and the turn degrades to silence.
                if out.ends_with('`') && text[i..].starts_with('`') {
                    out.pop();
                    i += 1;
                }
                // Collapse the spaces that framed the marker: debris in
                // mid-sentence must leave one gap, not two. (The extractor
                // never cared because its narration is dropped wholesale;
                // a stripped marker keeps the narration alive.)
                if out.ends_with(' ') && text[i..].starts_with(' ') {
                    i += 1;
                }
                continue;
            }
        }
        out.push(ch);
        i += ch.len_utf8();
    }
    out.trim().to_string()
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

/// The shape a resolved media reference presents to the media-array builder:
/// the id its rewritten `tg://` reference points at, and the path whose bytes
/// must be read for it.
///
/// Implemented by the three resolved-ref families ([`ResolvedImageRef`],
/// [`ResolvedVideoRef`], [`ResolvedFileRef`]) so the one read-and-push loop in
/// `channels::telegram::delivery::read_media_entries` can serve all six sites
/// that build a `MediaEntry` array. The trait carries ONLY what those sites
/// need from the entry — the id, the path, and (for documents) the part name —
/// never the entry itself, so the helper cannot accidentally clone a whole
/// resolved value into the array.
pub(crate) trait MediaSource {
    /// The id the rewritten reference points at: `tg://photo?id=<id>`,
    /// `tg://video?id=<id>` or `tg://document?id=<id>`.
    fn media_id(&self) -> &str;
    /// The absolute path whose bytes are uploaded for this entry.
    fn media_path(&self) -> &Path;
}

/// Marker prefix for the proprietary image form.
const IMG_PREFIX: &str = "<<IMG:";

/// Marker prefix for the proprietary video form (#465) — one home for the
/// literal, so the bare extractor above and the validated video scanner below
/// cannot drift apart on what a video marker is.
const VID_PREFIX: &str = "<<VID:";

/// The media-id prefix the image family gives a rewritten reference: the
/// `imgN` in `tg://photo?id=imgN`.
///
/// Named here rather than spelled at the call sites because the rich request's
/// `media` entries are matched to references BY ID (`rich/table.rs`), so this
/// prefix and [`VID_ID_PREFIX`] must never be able to produce the same id
/// inside one message — a collision would bind a reference to the wrong entry.
/// Two distinct literals in one place is the whole mitigation.
pub const IMG_ID_PREFIX: &str = "img";

/// The media-id prefix the video family gives a rewritten reference: the
/// `vidN` in `tg://video?id=vidN`. See [`IMG_ID_PREFIX`] for why the two live
/// side by side.
pub const VID_ID_PREFIX: &str = "vid";

/// The media-id prefix the file family gives a rewritten reference: the
/// `docN` in `tg://document?id=docN` (#1918). A third distinct literal for the
/// reason [`IMG_ID_PREFIX`] states: entries are matched to references BY ID
/// inside one message's media array, so a shared prefix would let a document
/// entry answer a picture's reference.
pub const DOC_ID_PREFIX: &str = "doc";

/// Why a local media candidate could not become an attachment.
///
/// One enum for every local-media family — images, video (#465) and files
/// (#1916). The reasons a video cannot be delivered are the ones listed here (a
/// missing path, a directory, an empty file, an unreadable file), so a second
/// near-identical enum would be a second home for the same question. The
/// variants that need a remote fetch arm or an image format gate —
/// `UnsupportedFormat`, `DownloadFailed`, `TooMany`, `BadUrl` — are emitted by
/// neither video nor the local-file family: there is no container gate on the
/// video path, no remote fetch arm to fail, and no format gate on a document.
/// `TooLarge` is shared: an image hits the per-image ceiling, a file the
/// `sendDocument` one.
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
    /// More bytes than the family's ceiling — a remote image past the per-image
    /// limit, or a local file past the `sendDocument` one.
    TooLarge,
    /// The reply referenced more remote images than the per-reply budget.
    TooMany,
    /// A remote reference is not a usable image URL (`data:` payload that is
    /// not base64, a base64 body that is not an image, an unparsable URL).
    BadUrl,
    /// The attachment was extracted and validated, but the channel refused to
    /// send it (API error, media-type rejection, platform size ceiling).
    /// Distinct from every other reason: the reference is not the problem.
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
            Self::TooLarge => "larger than the size limit",
            Self::TooMany => "too many remote images in one reply (per-reply limit reached)",
            Self::BadUrl => "not a usable image URL",
            Self::DeliveryFailed => "the channel could not deliver it",
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

/// A resolved local image together with the markdown title that captioned it.
///
/// Path and caption live in ONE value on purpose: the title belongs to the
/// picture written above it, and two parallel vectors would let them desync the
/// first time a candidate is dropped from one and not the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalImage {
    /// Absolute path to the image on disk.
    pub path: PathBuf,
    /// The markdown title — `![alt](target "title")` — to ship as the media
    /// caption. `None` when the reference carried no title.
    pub caption: Option<String>,
}

/// Result of scanning a reply for image references.
#[derive(Debug, Clone, Default)]
pub struct LocalImageScan {
    /// Reply text with every local image reference removed. Remote targets and
    /// references inside code spans are left untouched.
    pub text: String,
    /// Resolved and validated local images, in order of appearance.
    pub attachments: Vec<LocalImage>,
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
    // A `file://` URI names a local file (#1968). Without this arm the generic
    // path below would join the raw URI to `base_dir` and resolve to nothing,
    // because `file:` is neither remote nor a media ref.
    if let Some(path) = local_file_path_from_uri(trimmed) {
        return ImageTarget::Local(path);
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

/// Parse a markdown reference starting at `start` (a char boundary where the
/// text begins with `[`). `bracket_len` is the length of the opening bracket —
/// 2 for an image (`![alt](target)`), 1 for a link (`[label](target)`); the two
/// forms differ in nothing else. Accepts the angle-bracket form `(<target>)`
/// that markdown requires when the path holds spaces, and an optional `"title"`
/// / `'title'` after the target. Returns
/// `(end_byte_exclusive, label, raw_target, title)`.
///
/// The label is the `alt` text for an image and the link text for a link. It is
/// returned for both — inert on the image legs, and the caption carrier for a
/// document. The title is the caption carrier for an image
/// (`![alt](chart.png "Quarterly revenue")` renders "Quarterly revenue" as the
/// photo's caption), so it is captured in both forms rather than stepped over.
fn parse_markdown_ref(
    text: &str,
    start: usize,
    bracket_len: usize,
) -> Option<(usize, String, String, Option<String>)> {
    let open = match bracket_len {
        2 => "![",
        1 => "[",
        _ => return None,
    };
    debug_assert!(text[start..].starts_with(open));
    // `\![alt](path)` and `\[label](path)` are escaped literal text, not references.
    if start > 0 && text[..start].ends_with('\\') {
        return None;
    }
    let label_end = text[start + bracket_len..].find(']')?;
    let paren = start + bracket_len + label_end + 1;
    if !text[paren..].starts_with('(') {
        return None;
    }
    let label = text[start + bracket_len..start + bracket_len + label_end].to_string();
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
    // The title is the only caption carrier markdown offers, so it is captured
    // here instead of being stepped over.
    let mut title: Option<String> = None;
    if let Some(quote) = text[after_target..].chars().next()
        && (quote == '"' || quote == '\'')
    {
        let close = text[after_target + 1..].find(quote)?;
        let parsed = text[after_target + 1..after_target + 1 + close].trim();
        if !parsed.is_empty() {
            title = Some(parsed.to_string());
        }
        after_target = skip_whitespace(text, after_target + 1 + close + 1);
    }
    if !text[after_target..].starts_with(')') || target.trim().is_empty() {
        return None;
    }
    Some((after_target + 1, label, target, title))
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
///
/// `caption` is the markdown title the reference carried, bound to the image it
/// captions. A marker has no title form, so it always passes `None`.
fn record_candidate(
    raw: String,
    caption: Option<String>,
    base_dir: Option<&Path>,
    strip_unresolved: bool,
    remote: RemoteRefs,
    scan: &mut LocalImageScan,
) -> bool {
    match classify_image_target(&raw, base_dir) {
        ImageTarget::Local(path) => {
            match validate_local_image(&path) {
                Ok(()) => scan.attachments.push(LocalImage { path, caption }),
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

/// One resolved image reference and the media id assigned to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImageRef {
    /// The id the rewritten reference points at: `tg://photo?id=<id>`.
    pub id: String,
    /// The validated image, path and caption bound in one value as everywhere
    /// else in this module.
    pub image: LocalImage,
}

impl MediaSource for ResolvedImageRef {
    fn media_id(&self) -> &str {
        &self.id
    }
    fn media_path(&self) -> &Path {
        &self.image.path
    }
}

/// A text prepared for the rich media plane, in both forms a mid-turn
/// intermediate needs.
///
/// Two buffers come out of ONE walk, so a reference is classified and
/// validated (stat + header read) exactly once per intermediate rather than
/// once per form — and the two forms cannot disagree about which references
/// were images, because they were decided by the same predicate on the same
/// pass.
#[derive(Debug, Clone, Default)]
pub struct LocalImageRewrite {
    /// Input with every resolvable local reference replaced IN PLACE by
    /// `![<alt>](tg://photo?id=<prefix><n>)`. Refs inside code spans, remote
    /// targets and Telegram media refs are left byte-identical. This is the
    /// form the rich send carries, with its `media` array.
    pub rich: String,
    /// Input with every resolvable local reference removed — the shape
    /// [`strip_image_references`] already produces, for the fold path and for
    /// the HTML fallback, which has no media array and would otherwise ship a
    /// `tg://` reference as dead visible markdown.
    pub stripped: String,
    /// Resolved images in order of appearance, with the id each got.
    pub entries: Vec<ResolvedImageRef>,
    /// Rejected candidates, in order of appearance.
    pub failures: Vec<LocalImageFailure>,
}

/// Walk state for [`rewrite_local_images`], holding the per-reference policy so
/// the walk itself stays a single readable loop.
struct Rewriter<'a> {
    base_dir: Option<&'a Path>,
    id_prefix: &'a str,
    already_delivered: &'a [PathBuf],
    out: LocalImageRewrite,
}

impl Rewriter<'_> {
    /// File one parsed reference. Returns `true` when the reference was
    /// consumed and must leave BOTH text buffers — the same split
    /// [`record_candidate`] draws for the scan, so a reference that stays
    /// verbatim in the scan's text stays verbatim in both of ours.
    ///
    /// `strip_unresolved` distinguishes the two reference forms exactly as
    /// [`record_candidate`] does: a marker is machine syntax and always leaves
    /// the text, while a markdown reference whose target cannot be resolved
    /// stays verbatim, because it may be ordinary prose that merely looks like
    /// a reference.
    fn file(
        &mut self,
        raw: &str,
        alt: &str,
        caption: Option<String>,
        strip_unresolved: bool,
    ) -> bool {
        match classify_image_target(raw, self.base_dir) {
            ImageTarget::Local(path) => match validate_local_image(&path) {
                Ok(()) => {
                    // Comparison is on the RESOLVED ABSOLUTE path, which is
                    // what classify_image_target returns, so two spellings of
                    // one file (`~/x.png` and `/root/x.png`) dedup correctly.
                    if self.already_delivered.contains(&path) {
                        // The picture is already in the chat. Consume the
                        // reference, record nothing, report no failure: a
                        // delivered picture is not a lost one, and a second
                        // bubble for a picture the reader has would be noise.
                        return true;
                    }
                    let id = format!("{}{}", self.id_prefix, self.out.entries.len());
                    let alt = if alt.trim().is_empty() { "image" } else { alt };
                    let title = rich_title(caption.as_deref());
                    self.out.rich.push_str(&format!("![{alt}](tg://photo?id={id}{title})"));
                    self.out.entries.push(ResolvedImageRef {
                        id,
                        image: LocalImage { path, caption },
                    });
                }
                Err(reason) => self.out.failures.push(LocalImageFailure {
                    raw: raw.to_string(),
                    resolved: Some(path),
                    reason,
                }),
            },
            // Remote targets and Telegram media refs have nothing to embed
            // here: the rich plane's media array is built from local bytes, and
            // deleting a link nothing downstream will fetch is the #286 loss
            // this call site must not reintroduce.
            ImageTarget::Remote(_) | ImageTarget::MediaRef(_) => return strip_unresolved,
            ImageTarget::Unresolved => {
                if strip_unresolved {
                    self.out.failures.push(LocalImageFailure {
                        raw: raw.to_string(),
                        resolved: None,
                        reason: LocalImageFailureReason::NotFound,
                    });
                }
                return strip_unresolved;
            }
        }
        true
    }
}

/// Render a local image's caption as the markdown title of its rewritten
/// reference — the only caption channel this plane offers.
///
/// The rich media array has no caption field (Telegram ignores one on
/// `InputRichMessageMedia`), so the caption rides the markdown title instead:
/// measured 2026-09-27, a `tg://photo` reference titled `"CAP"` renders with
/// `caption: CAP` and the same reference untitled renders with none at all.
///
/// Emitted only when the caption is expressible with the one delimiter measured
/// to work. The returned string INCLUDES its separating space, because markdown
/// requires `![alt](target "title")` — without it the title fuses into the
/// target, the media id stops resolving, and the orphan shield escapes the `!`
/// so the image renders as literal text.
///
/// A caption containing `)` (terminates the reference), `"` (no alternative
/// delimiter is measured to work, and a malformed reference can fail the whole
/// message) or `\` (escape interaction) is dropped, which is exactly today's
/// caption-less behaviour: the image still arrives, merely unnamed. Newlines
/// fold to spaces because the reference must stay on one line.
fn rich_title(caption: Option<&str>) -> String {
    let Some(caption) = caption else {
        return String::new();
    };
    let folded = caption.split_whitespace().collect::<Vec<_>>().join(" ");
    if folded.is_empty() || folded.contains([')', '"', '\\']) {
        return String::new();
    }
    format!(" \"{folded}\"")
}

/// Rewrite a mid-turn intermediate for the rich media plane (#502).
///
/// The sibling of [`scan_image_references`]: same walk, same `code_regions`
/// guard, same [`classify_image_target`] + [`validate_local_image`]
/// classification, so a reference is judged by exactly one rule set no matter
/// which consumer asks. What differs is the output — a reference that resolves
/// becomes an EMBEDDED picture in `rich` rather than leaving the text entirely,
/// which is the whole point: the promotion path already carries a media array
/// and already uploads bytes via multipart `attach://`, it was simply never
/// shown the image.
///
/// `base_dir` is the session working directory, and it is passed `Some` by the
/// intermediate call sites (#502). Absolute and `~`-prefixed targets behave
/// exactly as before; a RELATIVE target now resolves against that base, the
/// same base the final-response leg already uses. Two consequences, both
/// intended: a relative reference that resolves is delivered instead of
/// shipping as dead markdown, and one that does not resolve is recorded as a
/// [`LocalImageFailure`] so the honest notice can name it instead of removing
/// it in silence.
///
/// `already_delivered` is a snapshot of the paths this turn has already put in
/// the chat. It is a plain slice rather than turn state so this module stays
/// channel-agnostic.
pub fn rewrite_local_images(
    text: &str,
    base_dir: Option<&Path>,
    id_prefix: &str,
    already_delivered: &[PathBuf],
) -> LocalImageRewrite {
    let regions = code_regions(text);
    let mut rw = Rewriter {
        base_dir,
        id_prefix,
        already_delivered,
        out: LocalImageRewrite {
            rich: String::with_capacity(text.len()),
            stripped: String::with_capacity(text.len()),
            entries: Vec::new(),
            failures: Vec::new(),
        },
    };
    let mut i = 0;

    while i < text.len() {
        if text[i..].starts_with(IMG_PREFIX)
            && let Some((end, raw)) = parse_marker_at(text, i, IMG_PREFIX)
        {
            if !raw.is_empty() {
                // A marker carries no alt and no title, exactly as the scan's
                // own call site passes `None` for the caption.
                rw.file(&raw, "", None, true);
            }
            i = end;
            continue;
        }
        if !regions[i]
            && text[i..].starts_with("![")
            && let Some((end, label, raw, caption)) = parse_markdown_ref(text, i, 2)
            && rw.file(&raw, &label, caption, false)
        {
            i = end;
            continue;
        }
        // Not consumed: fall through and copy the reference verbatim,
        // one char at a time, exactly as the scan does.
        let ch = text[i..].chars().next().expect("i lies on a char boundary");
        rw.out.rich.push(ch);
        rw.out.stripped.push(ch);
        i += ch.len_utf8();
    }

    rw.out.rich = rw.out.rich.trim().to_string();
    rw.out.stripped = rw.out.stripped.trim().to_string();
    rw.out
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
                record_candidate(raw, None, base_dir, true, remote, &mut scan);
            }
            i = end;
            continue;
        }
        if !regions[i]
            && text[i..].starts_with("![")
            && let Some((end, _label, raw, caption)) = parse_markdown_ref(text, i, 2)
            && record_candidate(raw, caption, base_dir, false, remote, &mut scan)
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
    media_failure_notice(failures, "Image", "an image")
}

/// [`failure_notice`] for the video family: the same shape, its own noun (#465).
///
/// A separate entry point rather than a caller-supplied noun because the
/// sentence names its family twice ("Image not attached — the reply referenced
/// an image …"), and a notice that says "Image" over a missing clip is a wrong
/// statement about what the reader is looking at. Both families share the
/// FAILURE SHAPE ([`LocalImageFailure`], one reason enum) — only the wording
/// differs, so the wording is where the split lives.
pub fn video_failure_notice(failures: &[LocalImageFailure]) -> Option<String> {
    media_failure_notice(failures, "Video", "a video")
}

fn media_failure_notice(
    failures: &[LocalImageFailure],
    noun: &str,
    article_noun: &str,
) -> Option<String> {
    if failures.is_empty() {
        return None;
    }
    let mut notice = format!(
        "⚠️ {noun} not attached — the reply referenced {article_noun} that could not be delivered:"
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
    append_media_notice(body, failure_notice(failures))
}

/// [`append_failure_notice`] for the video family (#465) — the same empty-body
/// rule, applied to [`video_failure_notice`].
pub fn append_video_failure_notice(body: &str, failures: &[LocalImageFailure]) -> String {
    append_media_notice(body, video_failure_notice(failures))
}

fn append_media_notice(body: &str, notice: Option<String>) -> String {
    match notice {
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

// ── Local video extraction (#465) ─────────────────────────────────────────
//
// The video family is the image family's sibling, deliberately. A reply can
// carry a video in the same two forms (`<<VID:path>>` and the markdown
// reference), it resolves against the same session working directory, and it
// feeds the same two consumers: a scan for the call sites that only sanitise
// text, and a validated rewrite for the rich media plane.
//
// Before #465 the marker had a parser and no delivery. `extract_vid_markers`
// returned bare paths that every caller discarded, and on Telegram the marker
// was not even stripped — `strip_image_references` reads only `<<IMG:` — so a
// `<<VID:…>>` in a reply shipped to the user as literal text while the video
// never arrived.
//
// Two rules are INHERITED from the image scanner rather than re-decided here,
// so one question keeps one home:
//   * the MARKER form has no code-span guard — a marker is machine syntax,
//     never prose (pinned for images by
//     `img_marker_inside_a_code_span_is_still_extracted`);
//   * the MARKDOWN form does — a fenced or backticked reference stays
//     byte-identical, because it may be documentation rather than a live
//     reference.
//
// One rule genuinely differs, and it is the family's declared non-goal: there
// is NO remote arm. A remote video would need a fetch step, a size policy and a
// container probe this plane does not have, so a remote target is left in the
// text as written — deleting a link nobody replaces is the #286 loss.

/// A resolved local video together with the markdown title that captioned it.
///
/// Path and caption travel as ONE value for the reason [`LocalImage`] states:
/// the title belongs to the video written above it, and parallel vectors would
/// desync the first time a candidate is dropped from one and not the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalVideo {
    /// Absolute path to the video on disk.
    pub path: PathBuf,
    /// The markdown title — `![alt](target "title")` — to ship as the media
    /// caption. `None` when the reference carried no title, and always `None`
    /// for the marker form, which has no title syntax.
    pub caption: Option<String>,
}

/// Result of scanning a reply for video references.
///
/// There is no `remote` field, unlike [`LocalImageScan`]: this plane has no
/// fetch step, so a remote video reference is left in the text rather than
/// recorded as awaited (the non-goal above).
#[derive(Debug, Clone, Default)]
pub struct LocalVideoScan {
    /// Reply text with every live local video reference removed. Remote targets
    /// and markdown references inside code spans are left untouched.
    pub text: String,
    /// Resolved and validated local videos, in order of appearance.
    pub attachments: Vec<LocalVideo>,
    /// Rejected local candidates, in order of appearance.
    pub failures: Vec<LocalImageFailure>,
}

/// One resolved video reference and the media id assigned to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedVideoRef {
    /// The id the rewritten reference points at: `tg://video?id=<id>`.
    pub id: String,
    /// The validated video, path and caption bound in one value as everywhere
    /// else in this module.
    pub video: LocalVideo,
}

impl MediaSource for ResolvedVideoRef {
    fn media_id(&self) -> &str {
        &self.id
    }
    fn media_path(&self) -> &Path {
        &self.video.path
    }
}

/// A text prepared for the rich media plane's video half (#465).
///
/// The same two-buffer contract as [`LocalImageRewrite`], and for the same
/// reason: one walk decides each reference once, so `rich` and `stripped`
/// cannot disagree about which references were videos.
#[derive(Debug, Clone, Default)]
pub struct LocalVideoRewrite {
    /// Input with every resolvable local reference replaced IN PLACE by
    /// `![<alt>](tg://video?id=<prefix><n>)`. Refs inside code spans, remote
    /// targets and existing Telegram media refs are left byte-identical. The
    /// id prefix is passed by the caller (`vid` at the delivery site) so the
    /// video namespace cannot collide with the image one inside a single
    /// message's `media` array, where entries are matched to references BY ID.
    pub rich: String,
    /// Input with every resolvable local reference removed — the shape the
    /// strip path produces, for the fold path and for the HTML fallback, which
    /// has no media array and would otherwise ship a `tg://` reference as dead
    /// visible markdown.
    pub stripped: String,
    /// Resolved videos in order of appearance, with the id each got.
    pub entries: Vec<ResolvedVideoRef>,
    /// Rejected candidates, in order of appearance.
    pub failures: Vec<LocalImageFailure>,
}

/// The markdown form's ownership test: ours only when the target resolves to a
/// file whose leading bytes are NOT a supported image.
///
/// A markdown reference is the one form the image family also reads, so exactly
/// one family must claim each reference or one file is judged twice and
/// delivered in the wrong plane. The partition is by CONTENT, not by extension
/// — an extension is a claim the bytes may not honour.
///
/// The unreadable/absent case answers `false` on purpose, which is the stricter
/// direction than the marker form takes: a missing target must stay the image
/// family's to report, or one dead reference would produce two failure notices.
/// A marker is read by nobody else, so it is always ours (see
/// [`record_video_candidate`]).
fn markdown_target_is_video(path: &Path) -> bool {
    match std::fs::File::open(path) {
        Ok(mut file) => {
            let mut head = [0u8; 12];
            let read = std::io::Read::read(&mut file, &mut head).unwrap_or(0);
            !is_supported_image(&head[..read])
        }
        Err(_) => false,
    }
}

/// Validate a resolved local video candidate: it must exist, be a regular file,
/// hold at least one byte, and be openable.
///
/// There is deliberately NO container gate here, where [`validate_local_image`]
/// has a format gate. A video's container decides WHICH send arm it takes — a
/// playable MPEG4 inside the ceiling goes as `sendVideo` and anything else as a
/// document (`telegram_video_media_kind`, D3) — never whether it can be
/// delivered at all. Rejecting an unplayable container here would delete a
/// referenced file from the reply and deliver nothing, which is the #286 loss;
/// the container is probed at the send site, where the arm is chosen.
pub fn validate_local_video(path: &Path) -> Result<(), LocalImageFailureReason> {
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
    std::fs::File::open(path).map_err(|_| LocalImageFailureReason::Unreadable)?;
    Ok(())
}

/// File one parsed video reference into the scan accumulators. Returns `true`
/// when the reference was consumed and must leave the reply text.
///
/// `is_marker` carries the family's ownership split. A marker (`<<VID:…>>`) is
/// video machine syntax that no other reader parses, so it is always ours: it
/// leaves the text whether it resolves or not, and an unresolvable one is
/// reported rather than shipped as a bare directive. A markdown reference is
/// contested with the image family, so it is ours only when
/// [`markdown_target_is_video`] says so — otherwise it stays verbatim for that
/// family, or as the prose it may simply be.
fn record_video_candidate(
    raw: String,
    caption: Option<String>,
    base_dir: Option<&Path>,
    is_marker: bool,
    scan: &mut LocalVideoScan,
) -> bool {
    match classify_image_target(&raw, base_dir) {
        ImageTarget::Local(path) => {
            if !is_marker && !markdown_target_is_video(&path) {
                return false;
            }
            match validate_local_video(&path) {
                Ok(()) => scan.attachments.push(LocalVideo { path, caption }),
                Err(reason) => scan.failures.push(LocalImageFailure {
                    raw,
                    resolved: Some(path),
                    reason,
                }),
            }
            true
        }
        // No remote arm (#465 non-goal): leaving the target in the text keeps
        // the link the user would otherwise lose to a deletion nobody replaces.
        // A `tg://`/`attach://` ref is resolved by Telegram against the rich
        // request's media array (#334), never against the filesystem.
        ImageTarget::Remote(_) | ImageTarget::MediaRef(_) => false,
        ImageTarget::Unresolved => {
            // Markers only: a markdown reference with no base directory to
            // resolve against stays verbatim, because it may be prose.
            if is_marker {
                scan.failures.push(LocalImageFailure {
                    raw,
                    resolved: None,
                    reason: LocalImageFailureReason::NotFound,
                });
            }
            is_marker
        }
    }
}

/// Scan a reply for local video references and hand back the text without them,
/// the validated videos, and the rejected candidates.
///
/// Unlike the image family there is no strip-only twin: video has no remote
/// arm, so a scan that collects attachments and a scan that only sanitises text
/// are one operation. A strip-only call site uses `.text` and ignores
/// `attachments` — the delivery path re-runs extraction on the final reply,
/// exactly as [`strip_image_references`]'s own doc states.
///
/// `base_dir` is the session working directory. A relative target resolves
/// against it; with no base directory a relative markdown target stays verbatim
/// while `~`-prefixed, absolute and marked targets still resolve (#286).
pub fn extract_local_videos(text: &str, base_dir: Option<&Path>) -> LocalVideoScan {
    let regions = code_regions(text);
    let mut scan = LocalVideoScan {
        text: String::with_capacity(text.len()),
        ..LocalVideoScan::default()
    };
    let mut i = 0;

    while i < text.len() {
        if text[i..].starts_with(VID_PREFIX)
            && let Some((end, raw)) = parse_marker_at(text, i, VID_PREFIX)
        {
            // An empty marker is dropped without a failure, matching the image
            // scanner: there is no path to report and nothing to deliver.
            if raw.is_empty() || record_video_candidate(raw, None, base_dir, true, &mut scan) {
                i = end;
                continue;
            }
        }
        if !regions[i]
            && text[i..].starts_with("![")
            && let Some((end, _label, raw, caption)) = parse_markdown_ref(text, i, 2)
            && record_video_candidate(raw, caption, base_dir, false, &mut scan)
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

/// Walk state for [`rewrite_local_videos`], the video twin of [`Rewriter`],
/// holding the per-reference policy so the walk stays a single readable loop.
struct VideoRewriter<'a> {
    base_dir: Option<&'a Path>,
    id_prefix: &'a str,
    already_delivered: &'a [PathBuf],
    out: LocalVideoRewrite,
}

impl VideoRewriter<'_> {
    /// File one parsed reference. Returns `true` when it was consumed and must
    /// leave BOTH text buffers — the same split [`record_video_candidate`]
    /// draws for the scan, so a reference verbatim in one is verbatim in all.
    fn file(&mut self, raw: &str, alt: &str, caption: Option<String>, is_marker: bool) -> bool {
        let ImageTarget::Local(path) = classify_image_target(raw, self.base_dir) else {
            // Remote target (no fetch arm), Telegram media ref (resolved
            // server-side, #334) or an unresolvable relative target: all three
            // stay in the text as written.
            return false;
        };
        if !is_marker && !markdown_target_is_video(&path) {
            return false;
        }
        match validate_local_video(&path) {
            Ok(()) => {
                // Comparison is on the RESOLVED ABSOLUTE path, which is what
                // `classify_image_target` returns, so two spellings of one file
                // (`~/clip.mp4` and `/root/clip.mp4`) dedup correctly.
                if self.already_delivered.contains(&path) {
                    // The video is already in the chat. Consume the reference,
                    // record nothing, report no failure: a delivered video is
                    // not a lost one, and a second bubble for a video the
                    // reader has would be noise.
                    return true;
                }
                let id = format!("{}{}", self.id_prefix, self.out.entries.len());
                let alt = if alt.trim().is_empty() { "video" } else { alt };
                let title = rich_title(caption.as_deref());
                self.out
                    .rich
                    .push_str(&format!("![{alt}](tg://video?id={id}{title})"));
                self.out.entries.push(ResolvedVideoRef {
                    id,
                    video: LocalVideo { path, caption },
                });
            }
            Err(reason) => self.out.failures.push(LocalImageFailure {
                raw: raw.to_string(),
                resolved: Some(path),
                reason,
            }),
        }
        true
    }
}

/// Rewrite a reply for the rich media plane's video half (#465).
///
/// The sibling of [`rewrite_local_images`]: same walk, same `code_regions`
/// guard on the markdown form, same classification, so a reference is judged by
/// exactly one rule set no matter which family asks. What differs is the
/// NAMESPACE — a resolved video becomes `![alt](tg://video?id=<prefix><n>)`
/// rather than a `tg://photo?id=…` reference — because the rich request's media
/// entry carries its own type and a video reference must point at a video
/// entry. Reference and entry come out of ONE pass for the reason the orphan
/// shield makes load-bearing: `neutralize_orphan_photo_refs` defuses a
/// `tg://video?id=` whose id is absent from `media`, so a ref built by a
/// different pass than its entry would degrade the video to a dead reference
/// silently.
///
/// `already_delivered` is a snapshot of the paths this turn has already put in
/// the chat. It is a plain slice rather than turn state so this module stays
/// channel-agnostic.
pub fn rewrite_local_videos(
    text: &str,
    base_dir: Option<&Path>,
    id_prefix: &str,
    already_delivered: &[PathBuf],
) -> LocalVideoRewrite {
    let regions = code_regions(text);
    let mut rw = VideoRewriter {
        base_dir,
        id_prefix,
        already_delivered,
        out: LocalVideoRewrite {
            rich: String::with_capacity(text.len()),
            stripped: String::with_capacity(text.len()),
            entries: Vec::new(),
            failures: Vec::new(),
        },
    };
    let mut i = 0;

    while i < text.len() {
        // Advances only on a consumption: an empty marker is dropped without a
        // failure (the scan's policy above), and a reference `file` declines
        // stays verbatim in both buffers.
        if text[i..].starts_with(VID_PREFIX)
            && let Some((end, raw)) = parse_marker_at(text, i, VID_PREFIX)
            && (raw.is_empty() || rw.file(&raw, "", None, true))
        {
            i = end;
            continue;
        }
        if !regions[i]
            && text[i..].starts_with("![")
            && let Some((end, label, raw, caption)) = parse_markdown_ref(text, i, 2)
            && rw.file(&raw, &label, caption, false)
        {
            i = end;
            continue;
        }
        // Not consumed: fall through and copy the reference verbatim, one
        // char at a time, exactly as the image rewriter does.
        let ch = text[i..].chars().next().expect("i lies on a char boundary");
        rw.out.rich.push(ch);
        rw.out.stripped.push(ch);
        i += ch.len_utf8();
    }

    rw.out.rich = rw.out.rich.trim().to_string();
    rw.out.stripped = rw.out.stripped.trim().to_string();
    rw.out
}

// ── Local file extraction (#1916) ─────────────────────────────────────────
//
// The file family is the image family's non-media sibling. A reply carries a
// markdown LINK to a file on disk (`[Q3 report](/root/reports/q3.pdf)`), and the
// reader is a channel user with no filesystem access — so the link is not a
// deliverable, the FILE is. Before #1916 a plain `[label](path)` link was parsed
// by the rich renderer as an inert link entity whose URL was the raw path:
// Telegram has no scheme to resolve, so the file never arrived and the link was
// dead.
//
// Two rules are INHERITED from the image scanner rather than re-decided:
//   * the MARKDOWN form is guarded by `code_regions` — a link inside a fenced or
//     backticked span stays byte-identical, because it may be documentation;
//   * a relative target with no base directory to resolve against stays
//     verbatim, because it may be ordinary prose that merely looks like a link.
//
// One rule genuinely differs, and it is the family's declared policy: a REJECTED
// candidate is left in the text byte-identical (never stripped), because a link
// carries its own label and a silent strip would delete the reader's only clue
// about what was referenced. The failure it records is what drives the
// self-healing nudge instead.
//
// #1918: a DELIVERED link is replaced by a visible `📎 <label>` marker
// rather than deleted. The prose keeps the file's NAME and, crucially, the
// POSITION it was referenced at, so the reader is told a document was lifted
// out here. The marker carries no URL, which is what makes it correct in EVERY
// chat kind: a real `t.me` message link exists only for groups and channels, so
// a DM has no form to offer and a URL-shaped placeholder would be dead there.

/// A resolved local file together with the markdown link label that named it.
///
/// Path and caption live in ONE value for the reason [`LocalImage`] states: the
/// label belongs to the link written around it, and two parallel vectors would
/// let them desync the first time a candidate is dropped from one and not the
/// other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFile {
    /// Absolute path to the file on disk.
    pub path: PathBuf,
    /// The markdown link label — `[label](target)` — to ship as the document
    /// caption. `None` when the label was empty.
    pub caption: Option<String>,
    /// Byte range in [`LocalFileScan::text`] occupied by this file's visible
    /// `📎 <label>` marker (#1918). The scanner records it as it emits the
    /// marker, so a later pass rewrites exactly that span: a label that also
    /// occurs elsewhere in the reply can never be mis-targeted. `None` for a
    /// value that did not come from a scan — the sentinel is unrepresentable
    /// rather than a `(0, 0)` a reader must remember to test for (#1918 review
    /// f7).
    pub marker_span: Option<std::ops::Range<usize>>,
}

/// Result of scanning a reply for links to local files.
#[derive(Debug, Clone, Default)]
pub struct LocalFileScan {
    /// Reply text with every DELIVERED local-file link replaced by a visible
    /// `📎 <label>` marker (#1918). A remote link, a non-file scheme, a
    /// reference inside a code span and a REJECTED candidate are all left
    /// byte-identical (see the module note above).
    pub text: String,
    /// Resolved and validated local files, in order of appearance.
    pub attachments: Vec<LocalFile>,
    /// Rejected local candidates, in order of appearance.
    pub failures: Vec<LocalImageFailure>,
}

/// What a markdown link target resolves to for the file family, classification
/// and validation in ONE step.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Resolution {
    /// A local file that exists, is a regular non-empty file, is openable and
    /// fits the document size ceiling.
    Local(PathBuf),
    /// A local path that failed validation, with the resolved path and why.
    Rejected {
        path: PathBuf,
        reason: LocalImageFailureReason,
    },
    /// Not a filesystem candidate at all — a remote URL, a Telegram media ref, a
    /// `mailto:`/`tel:`-style scheme, an in-page `#anchor`, or a relative path
    /// with no base directory to resolve it against. Left as written.
    Skip,
}

/// Resolve AND validate one link target for the file family.
///
/// ONE policy for both the scan ([`record_file_candidate`]) and the rewrite
/// ([`FileRewriter::file`]). The two used to spell classify → validate out
/// separately and were kept in step by hand — an invariant documented only in
/// prose, and exactly the kind that silently breaks. `~/…` goes through the
/// shared tilde expander, an absolute path is taken as-is, and a relative path
/// is joined to `base_dir` (the session working directory).
fn resolve_file_target(target: &str, base_dir: Option<&Path>) -> Resolution {
    let trimmed = target.trim();
    // An in-page anchor (`#section`) is not a file.
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Resolution::Skip;
    }
    // A `file://` URI names a LOCAL file — the spelling a model reaches for when
    // it means "this file on the box". Unwrap it to its path so it takes the same
    // validate path a bare path does, instead of falling to the generic scheme
    // guard below, which would skip it SILENTLY: no attachment, no marker, no
    // failure notice (#1968).
    if let Some(path) = local_file_path_from_uri(trimmed) {
        return validate_local_path(path);
    }
    // A link Telegram resolves itself, or one it resolves against the rich
    // request's media array (#334), is never ours; neither is any other URI
    // scheme (`mailto:`, `ftp:`, `tel:`), which must not be joined to the cwd
    // and reported as a missing file.
    if is_remote_url(trimmed) || is_telegram_media_ref(trimmed) || has_url_scheme(trimmed) {
        return Resolution::Skip;
    }
    let expanded = crate::brain::tools::error::expand_tilde(trimmed);
    let path = if expanded.is_absolute() {
        expanded
    } else {
        match base_dir {
            Some(dir) => dir.join(expanded),
            // A relative target with no base directory may be ordinary prose
            // that merely looks like a link: leave it as written.
            None => return Resolution::Skip,
        }
    };
    validate_local_path(path)
}

/// Validate an already-resolved local path into a [`Resolution`]. Shared by the
/// bare-path arm and the `file://` arm of [`resolve_file_target`] so the two
/// cannot drift apart.
fn validate_local_path(path: PathBuf) -> Resolution {
    match validate_local_file(&path) {
        Ok(()) => Resolution::Local(path),
        Err(reason) => Resolution::Rejected { path, reason },
    }
}

/// True when `raw` begins with a URI scheme (`mailto:`, `ftp:`, `tel:`, …).
/// `http(s)://`, `data:` and `tg://`/`attach://` are covered by the caller's
/// earlier checks; this catches every OTHER scheme so a `mailto:` link is left as
/// written rather than joined to the session cwd and reported as missing.
fn has_url_scheme(raw: &str) -> bool {
    let mut chars = raw.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() => {}
        _ => return false,
    }
    for ch in chars {
        if ch == ':' {
            return true;
        }
        if !(ch.is_ascii_alphanumeric() || ch == '+' || ch == '-' || ch == '.') {
            return false;
        }
    }
    false
}

/// Decode a `file://` URI into the local path it names, so a model that spells a
/// local file the URI way (`file:///root/x.md`) reaches the same resolver as a
/// bare path.
///
/// `file:` is an explicitly LOCAL scheme, but [`has_url_scheme`] treats it like
/// `mailto:`/`ftp:` and the file family's guard would skip it — silently, with no
/// attachment and no failure notice (the #1968 report: a model wrote
/// `[label](file:///root/…)` and the file never shipped). Telegram's
/// `sendDocument` has no URI form, so the spelling is unwrapped BEFORE the scheme
/// guard, never after it.
///
/// `url::Url` does the whole job — it is already a dependency, it enforces the
/// scheme, drops the authority (`file://localhost/…` and `file:///…` name the
/// same path) and percent-decodes `%20`. `None` for anything that is not a
/// `file:` URI, including a host that is neither empty nor `localhost`
/// (`file://server/share` is a UNC path this resolver cannot reach) and a
/// malformed escape.
pub(crate) fn local_file_path_from_uri(raw: &str) -> Option<PathBuf> {
    let url = url::Url::parse(raw).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    if !matches!(url.host(), None | Some(url::Host::Domain("localhost"))) {
        return None;
    }
    url.to_file_path().ok()
}

/// Telegram's `sendDocument` ceiling. Documents carry far more than a photo's
/// 10 MiB (`TELEGRAM_PHOTO_MAX_BYTES`), so the file family gets its own wall: a
/// larger file is reported as a failure rather than handed to an API that will
/// reject it.
pub const TELEGRAM_DOCUMENT_MAX_BYTES: u64 = 50 * 1024 * 1024;

/// Validate a resolved local-file candidate: it must exist, be a regular file,
/// hold at least one byte, be openable, and fit the document size ceiling.
///
/// There is deliberately NO format gate here, where [`validate_local_image`] has
/// one: any bytes can ship as a document, and rejecting an unrecognised format
/// would delete a referenced file from the reply and deliver nothing — the #286
/// loss this family exists to avoid.
pub fn validate_local_file(path: &Path) -> Result<(), LocalImageFailureReason> {
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
    if meta.len() > TELEGRAM_DOCUMENT_MAX_BYTES {
        return Err(LocalImageFailureReason::TooLarge);
    }
    std::fs::File::open(path).map_err(|_| LocalImageFailureReason::Unreadable)?;
    Ok(())
}

/// File one parsed link into the scan accumulators. Returns `true` when the
/// reference was consumed and must leave the reply text — which happens ONLY for
/// a resolved, validated file. A rejected candidate or a non-file target returns
/// `false`, so the link is copied through byte-identical: a remote link Telegram
/// resolves itself, and a missing one is a nudge, never a silent strip.
fn record_file_candidate(
    raw: &str,
    label: &str,
    target: &str,
    base_dir: Option<&Path>,
    scan: &mut LocalFileScan,
) -> bool {
    match resolve_file_target(target, base_dir) {
        Resolution::Local(path) => {
            let caption = if label.trim().is_empty() {
                None
            } else {
                Some(label.to_string())
            };
            scan.attachments.push(LocalFile {
                path,
                caption,
                // The span is set by the caller that emits the marker; a
                // record built here has no position in `scan.text` yet.
                marker_span: None,
            });
            true
        }
        Resolution::Rejected { path, reason } => {
            scan.failures.push(LocalImageFailure {
                raw: raw.to_string(),
                resolved: Some(path),
                reason,
            });
            false
        }
        Resolution::Skip => false,
    }
}

/// Break a chat client's URL autolinker on a marker label (#1938).
///
/// A dotted token such as `1918-fix-state.md` or `config.io` is rendered by the
/// Telegram client as a bare domain link — the marker then wears a fake `t.me`
/// preview and hijacks the reader's tap. A zero-width space inserted after a
/// dot that precedes an alphanumeric is invisible in every plane (the classic
/// HTML renderer, the rich plane's `alt`, and the linked `[label](url)` form
/// `link_file_markers` splices) while it breaks the client's `name.tld` pattern.
///
/// A backslash escape is deliberately NOT used: the classic plane has no escape,
/// so `1918-fix-state\.md` would render with the backslash visible. The dot
/// itself stays visible, so the reader still sees the file's own name.
///
/// `pub(crate)` exists for this crate's tests alone — every production caller
/// lives in this module (`file_marker_text`) — so it is fork-internal API and
/// deliberately NOT part of the upstream port surface (#1918 review f9).
pub(crate) fn disarm_autolink(text: &str) -> String {
    /// U+200B ZERO WIDTH SPACE — no ink in any renderer.
    const ZWSP: char = '\u{200b}';
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 8);
    for (i, &c) in chars.iter().enumerate() {
        out.push(c);
        if c == '.'
            && let Some(&next) = chars.get(i + 1)
            && next.is_ascii_alphanumeric()
        {
            out.push(ZWSP);
        }
    }
    out
}

/// The visible text left where a delivered file link was (#1918).
///
/// The link label is the natural marker — it is the words the author chose — and
/// an empty label falls back to the file's own name so the marker is never
/// blank. Deliberately NOT a URL: a `t.me` message link exists only for groups
/// and channels, so a DM or a basic group has no form to offer, while a marker
/// needs no link form at all.
///
/// The result is passed through [`disarm_autolink`] (#1938) so a dotted name
/// reads as a filename in both planes rather than as a domain link.
///
/// `pub(crate)` exists for this crate's tests alone — every production caller
/// lives in this module (`push_file_marker`, `push_document_ref`) — so it is
/// fork-internal API and deliberately NOT part of the upstream port surface
/// (#1918 review f9).
pub(crate) fn file_marker_text(label: &str, target: &str) -> String {
    let trimmed = label.trim();
    let text = if !trimmed.is_empty() {
        trimmed.to_string()
    } else {
        std::path::Path::new(target)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string())
    };
    disarm_autolink(&text)
}

/// Consume a model-written `📎 ` run immediately before a reference (#2001).
///
/// The `📎 <label>` marker is the HARNESS's to write, and the channel
/// capability line tells the model the reference "is replaced in your reply by
/// a visible marker naming the file (📎 <label>)". A model that matches the
/// output it was told to expect therefore prefixes its own `📎 ` before
/// `[label](path)` — and both emission sites replace only the REFERENCE span,
/// copying that prefix through, so the reader gets the marker twice
/// (`📎 📎 <label>` on the text plane, `📎 ![📎 <label>](tg://document?id=…)` on
/// the rich one). Dropping the model's prefix before emitting the harness one
/// makes the emit idempotent on both planes.
///
/// Only trailing whitespace and `📎` tokens are consumed, so ordinary prose
/// ending in a space is untouched, and the loop terminates because every
/// iteration strictly shortens the buffer. Trailing whitespace is dropped with
/// the token, so `text 📎 [x](p)` leaves `text ` — the marker lands exactly
/// where the model's was.
fn consume_marker_prefix(buf: &mut String) {
    loop {
        let trimmed = buf.trim_end_matches([' ', '\t']);
        if !trimmed.ends_with('📎') {
            break;
        }
        // Hoisted so the immutable borrow ends before `truncate` takes `&mut`.
        let cut = trimmed.len() - '📎'.len_utf8();
        buf.truncate(cut);
    }
}

/// Append the text-plane marker for a resolved file — `📎 <label>` — and return
/// the byte span it occupies in `out` (#1918). A `📎 ` the model wrote itself is
/// consumed first, so the marker is emitted once whichever side wrote it
/// (#2001). The single home of the marker's shape, shared with the rich plane's
/// [`push_document_ref`].
fn push_file_marker(out: &mut String, label: &str, target: &str) -> std::ops::Range<usize> {
    consume_marker_prefix(out);
    let start = out.len();
    out.push_str("📎 ");
    out.push_str(&file_marker_text(label, target));
    start..out.len()
}

/// Append the rich-plane reference for a resolved file —
/// `![📎 <label>](tg://document?id=<id>)` — whose alt carries the same
/// `📎 <label>` the text plane's marker does, so the reader's anchor is
/// identical in both planes. A model-written `📎 ` is consumed first for the
/// same reason as [`push_file_marker`].
fn push_document_ref(out: &mut String, label: &str, target: &str, id: &str) {
    consume_marker_prefix(out);
    let alt = file_marker_text(label, target);
    out.push_str(&format!("![📎 {alt}](tg://document?id={id})"));
}

/// Scan a reply for markdown links to local files and hand back the text with
/// every DELIVERED link removed, the validated files, and the rejected
/// candidates.
///
/// `base_dir` is the session working directory: a relative target resolves
/// against it. With no base directory a relative link stays verbatim while
/// `~`-prefixed and absolute targets still resolve.
///
/// Marker semantics differ from the image family on purpose. A resolved file
/// leaves the text and becomes an attachment, and the link that named it is
/// replaced by a visible `📎 <label>` marker (#1918) — not deleted. The
/// marker keeps the file's name and the position it was referenced at, and it
/// carries no URL, so it is a valid rich-plane primitive in every chat kind. A
/// REJECTED candidate stays in the text byte-identical AND is reported as a
/// failure, because a link carries its own label and a silent strip would delete
/// the reader's only clue about what was referenced — the failure is what drives
/// the self-healing nudge.
pub fn extract_local_files(text: &str, base_dir: Option<&Path>) -> LocalFileScan {
    let regions = code_regions(text);
    let mut scan = LocalFileScan {
        text: String::with_capacity(text.len()),
        ..LocalFileScan::default()
    };
    let mut i = 0;

    while i < text.len() {
        if !regions[i]
            && text[i..].starts_with('[')
            // `![alt](path)` is the image family's reference, not a file link:
            // its `[` is preceded by `!`, and copying the `!` first must not let
            // the link parser claim the span on the next iteration.
            && !text[..i].ends_with('!')
            && let Some((end, label, target, _title)) = parse_markdown_ref(text, i, 1)
            && record_file_candidate(&text[i..end], &label, &target, base_dir, &mut scan)
        {
            // #1918: the reference becomes a visible marker, not a hole — the
            // label is the marker text, and an empty label falls back to the
            // file's own name so the reader always has something to anchor on.
            // No URL is emitted, so the marker renders in every chat kind.
            // #2001: a `📎 ` the model wrote itself is consumed first, so the
            // marker is not doubled. The emission itself has one home, shared
            // with the rich plane.
            let span = push_file_marker(&mut scan.text, &label, &target);
            // The attachment pushed by `record_file_candidate` is the one this
            // marker belongs to — the scan is single-threaded and in order.
            if let Some(record) = scan.attachments.last_mut() {
                record.marker_span = Some(span);
            }
            i = end;
            continue;
        }
        let ch = text[i..].chars().next().expect("i lies on a char boundary");
        scan.text.push(ch);
        i += ch.len_utf8();
    }

    // `trim()` strips leading whitespace, which shifts every recorded span left
    // by that many bytes. Rebase the spans BEFORE trimming so a `marker_span` is
    // always an index into the FINAL `scan.text`. Only the LEADING run matters:
    // a marker begins with `📎` and ends with a non-whitespace label, so every
    // span lies wholly inside `trim_start()..trim_end()`, and any trailing
    // whitespace sits after the last marker and never moves an index.
    let lead = scan.text.len() - scan.text.trim_start().len();
    if lead > 0 {
        for record in &mut scan.attachments {
            if let Some(span) = &mut record.marker_span {
                span.start -= lead;
                span.end -= lead;
            }
        }
    }
    scan.text = scan.text.trim().to_string();
    scan
}

/// A resolved local file together with the media-array id its rewritten
/// reference points at: `tg://document?id=<id>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFileRef {
    /// The id the rewritten reference points at.
    pub id: String,
    /// The validated file, path and caption bound in one value as everywhere
    /// else in this module.
    pub file: LocalFile,
}

impl MediaSource for ResolvedFileRef {
    fn media_id(&self) -> &str {
        &self.id
    }
    fn media_path(&self) -> &Path {
        &self.file.path
    }
}

/// A text prepared for the rich media plane's document half (#1918).
///
/// The document twin of [`rewrite_local_images`], and it carries ONE text buffer
/// where that one carries two. The file family's other plane is the MARKER form
/// (`📎 <label>`), which [`extract_local_files`] already produces for the HTML
/// body and the dedup record — and it is the only honest shape there: that plane
/// has no media array to answer a `tg://document` reference, and the marker is
/// the reader's sole anchor to the document. A strip buffer here would be a
/// second home for a shape the scan owns, and a wrong one. Rejected candidates
/// are the scan's to report for the same reason: it walks the same references,
/// so a copy of that list here would be a second home for one predicate.
#[derive(Debug, Clone, Default)]
pub struct LocalFileRewrite {
    /// Input with every resolvable local-file link replaced IN PLACE by
    /// `![📎 <label>](tg://document?id=<prefix><n>)`. Links inside code spans,
    /// remote targets and existing Telegram media refs are left byte-identical.
    /// The id prefix is passed by the caller (`doc` at the delivery site) so the
    /// document namespace cannot collide with the image or video ones inside a
    /// single message's `media` array, where entries are matched to references
    /// BY ID.
    pub rich: String,
    /// Resolved files in order of appearance, with the id each got.
    pub entries: Vec<ResolvedFileRef>,
}

/// Walk state for [`rewrite_local_files`], holding the per-reference policy so
/// the walk itself stays a single readable loop.
struct FileRewriter<'a> {
    base_dir: Option<&'a Path>,
    id_prefix: &'a str,
    already_delivered: &'a [PathBuf],
    /// #1968: when false, a MARKDOWN document is left as the plain
    /// `📎 <label>` marker instead of being rewritten to an inlined
    /// `tg://document` reference. Telegram's Android client opens a `.md`
    /// ATTACHMENT but not a document inlined into a rich message. No entry is
    /// recorded for it either, so the file falls to the detached file floor
    /// rather than to the rich plane's media array.
    inline_markdown_documents: bool,
    rich: String,
    entries: Vec<ResolvedFileRef>,
}

impl FileRewriter<'_> {
    /// File one parsed link. Returns `true` when the reference was consumed and
    /// must leave the buffer — which happens ONLY for a resolved, validated
    /// file. A rejected candidate or a non-file target returns `false`, so the
    /// link is copied through byte-identical: a remote link Telegram resolves
    /// itself, and a missing one is a nudge, never a silent strip. This is
    /// [`record_file_candidate`]'s policy, kept identical so the scan and the
    /// rewrite never disagree about which links were files — and the scan is
    /// what reports the rejection, so returning `false` loses nothing.
    fn file(&mut self, label: &str, target: &str) -> bool {
        match resolve_file_target(target, self.base_dir) {
            Resolution::Local(path) => {
                // Comparison is on the RESOLVED ABSOLUTE path, which is what
                // resolve_file_target returns, so two spellings of one file
                // dedup correctly — the same rule the image family uses.
                if self.already_delivered.contains(&path) {
                    // The document is already in the chat (a promoted
                    // intermediate sent it). Consume the reference, record
                    // nothing: a delivered document is not a lost one.
                    return true;
                }
                // #1968: the markdown inline gate. A markdown document is left
                // as the plain `📎 <label>` marker the TEXT plane already
                // shows — same buffer shape, same disarmed label — and no entry
                // is recorded, so the rich plane never inlines it and the
                // detached file floor delivers it instead. Telegram's Android
                // client opens a `.md` ATTACHMENT but not a document inlined
                // into a rich message, so the marker is the reader's anchor
                // either way and nothing is lost by declining the inline form.
                // Emitted through [`push_file_marker`], the single home of the
                // marker's shape, so a model-written `📎 ` is consumed rather
                // than doubled (#2001).
                if !self.inline_markdown_documents && is_markdown_path(&path) {
                    push_file_marker(&mut self.rich, label, target);
                    return true;
                }
                let id = format!("{}{}", self.id_prefix, self.entries.len());
                let caption = if label.trim().is_empty() {
                    None
                } else {
                    Some(label.to_string())
                };
                // The alt carries the same `📎 <label>` the text plane's
                // marker does, so the reader's anchor is identical in both
                // planes; an empty label falls back to the file's own name.
                // #2001: the emission consumes a model-written `📎 ` first, so
                // the alt never doubles the marker (`📎 ![📎 <label>](…)`).
                push_document_ref(&mut self.rich, label, target, &id);
                self.entries.push(ResolvedFileRef {
                    id,
                    file: LocalFile {
                        path,
                        caption,
                        // Not from a scan: no position in any scan buffer.
                        marker_span: None,
                    },
                });
            }
            // A remote link, an in-page anchor or any other URI scheme has
            // nothing to embed here: the rich plane's media array is built from
            // local bytes, and deleting a link nothing downstream will fetch is
            // the #286 loss this call site must not reintroduce. A rejected
            // candidate is the SCAN's to report — it walks the same references,
            // so the rewriter stays silent and returns `false`.
            Resolution::Rejected { .. } | Resolution::Skip => return false,
        }
        true
    }
}

/// Whether a local path names a MARKDOWN document, by extension.
///
/// The one predicate behind `channels.telegram.inline_markdown_documents` (#1968): a
/// markdown document is the single kind Telegram's Android client cannot open
/// when the rich plane inlines it, so it is the kind that flag detaches. Kept
/// here rather than inline at the call site so the delivery gate and any
/// future consumer answer the question the same way.
pub fn is_markdown_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
}

/// Scan a reply for markdown links to local files and hand back the rich form
/// with each resolvable link replaced in place by a `tg://document` media
/// reference (#1918), together with the validated files and the id each got.
///
/// The document twin of [`rewrite_local_images`], and it obeys the same two
/// rules the scan does: a link inside a code span stays byte-identical, and a
/// relative target with no base directory stays verbatim because it may be
/// ordinary prose that merely looks like a link. A rejected candidate is left
/// byte-identical — the file family's declared policy, since a link carries its
/// own label and a silent strip would delete the reader's only clue about what
/// was referenced.
///
/// `inline_markdown_documents` mirrors `channels.telegram.inline_markdown_documents`
/// (#1968): when false (the default), a markdown document is left as the plain
/// `📎 <label>` marker instead of a `tg://document` reference and records no
/// entry, so the caller delivers it detached.
pub fn rewrite_local_files(
    text: &str,
    base_dir: Option<&Path>,
    id_prefix: &str,
    already_delivered: &[PathBuf],
    inline_markdown_documents: bool,
) -> LocalFileRewrite {
    let regions = code_regions(text);
    let mut rw = FileRewriter {
        base_dir,
        id_prefix,
        already_delivered,
        inline_markdown_documents,
        rich: String::with_capacity(text.len()),
        entries: Vec::new(),
    };
    let mut i = 0;

    while i < text.len() {
        if !regions[i]
            && text[i..].starts_with('[')
            // `![alt](path)` is the image family's reference, not a file link:
            // its `[` is preceded by `!`, and copying the `!` first must not let
            // the link parser claim the span on the next iteration.
            && !text[..i].ends_with('!')
            && let Some((end, label, target, _title)) = parse_markdown_ref(text, i, 1)
            && rw.file(&label, &target)
        {
            i = end;
            continue;
        }
        let ch = text[i..].chars().next().expect("i lies on a char boundary");
        rw.rich.push(ch);
        i += ch.len_utf8();
    }

    LocalFileRewrite {
        rich: rw.rich.trim().to_string(),
        entries: rw.entries,
    }
}

/// The honest user-visible line for files that could not be delivered, used when
/// the self-healing nudge budget is exhausted. `None` when there is nothing to
/// report.
pub fn file_failure_notice(failures: &[LocalImageFailure]) -> Option<String> {
    media_failure_notice(failures, "File", "a file")
}

/// [`file_failure_notice`] appended to a reply body — the empty-body rule of
/// [`append_failure_notice`], applied to the file family.
pub fn append_file_failure_notice(body: &str, failures: &[LocalImageFailure]) -> String {
    append_media_notice(body, file_failure_notice(failures))
}