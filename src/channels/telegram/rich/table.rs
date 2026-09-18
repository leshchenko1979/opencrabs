//! GitHub-flavored pipe-table parsing.
//!
//! A table is a header row (`| a | b |`), a separator row of dashes with
//! optional alignment colons (`| :-- | --: |`), then zero or more body rows.
//! Parsing stops at the first line that is not a pipe row.

use super::ast::{Align, Inline, Table};
use super::inline::parse_inlines;
use super::mermaid::MediaEntry;
use regex::Regex;
use std::sync::LazyLock;

/// A row/header/separator boundary in a table the model collapsed onto one line:
/// the trailing `|` of one row meets the leading `|` of the next, giving an
/// empty gap between two pipes (`| |` or `||`). Matched non-greedily on a single
/// line (never crosses a real newline).
static COLLAPSED_ROW_BOUNDARY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\|[ \t]*\|").expect("collapsed-table boundary regex"));

/// Re-expand a table the model collapsed onto ONE line — header, separator, and
/// rows jammed together with no row-newlines — into a proper multi-line table so
/// [`try_parse`] can detect it and Telegram renders a grid instead of raw pipes
/// (#690). A line is treated as collapsed only when it carries BOTH a dash-only
/// separator cell AND ordinary content cells, a combination a well-formed
/// multi-line table never has on a single line — so prose, a lone separator row,
/// and already-expanded tables are left untouched. Idempotent.
pub(crate) fn reflow_collapsed_tables(text: &str) -> String {
    if !text.contains('|') {
        return text.to_string();
    }
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        if is_collapsed_table_line(line) {
            // #132: a prose label before the first pipe (e.g. "Состояние
            // данных: | Что | Статус |...") would stay glued to the split
            // header row — unparseable as a table AND rendered inside the
            // header cell. Detach everything before the first '|' onto its
            // own line so the table block starts clean.
            let pipe = line.find('|').unwrap_or(0);
            let (prefix, rest) = line.split_at(pipe);
            if !prefix.trim().is_empty() {
                out.push(prefix.trim_end().to_string());
            }
            out.push(
                COLLAPSED_ROW_BOUNDARY
                    .replace_all(rest, "|\n|")
                    .into_owned(),
            );
        } else {
            out.push(line.to_string());
        }
    }
    out.join("\n")
}

/// Whether a single line is a collapsed table: it has a dash-only separator cell
/// (`----`) AND at least one content cell on the same line. A proper multi-line
/// table has the separator alone on its own line, so this never fires on one.
fn is_collapsed_table_line(line: &str) -> bool {
    let mut has_dash = false;
    let mut has_content = false;
    for cell in line.split('|') {
        let c = cell.trim();
        if c.is_empty() {
            continue;
        }
        let core = c.trim_start_matches(':').trim_end_matches(':');
        if core.len() >= 2 && core.chars().all(|ch| ch == '-') {
            has_dash = true;
        } else {
            has_content = true;
        }
        if has_dash && has_content {
            return true;
        }
    }
    false
}

/// Insert one blank line before any table block whose preceding line is
/// non-blank (#95). Telegram's rich-markdown parser accepts a GFM table only
/// as a standalone block: a table that directly follows a text line renders
/// as raw pipes (probe matrix A/B/C/D — blank line present = rendered,
/// abutting = raw). Detection reuses [`try_parse`], so exactly the blocks the
/// AST parser accepts get normalized — prose with stray pipes is never
/// touched. Code fences (``` and ~~~) are never mutated, the pass is
/// idempotent, and pipe-free input returns unchanged.
pub(crate) fn ensure_blank_line_before_tables(text: &str) -> String {
    if !text.contains('|') {
        return text.to_string();
    }
    let lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 1);
    let mut in_fence = false;
    let mut i = 0;
    while i < lines.len() {
        let line = &lines[i];
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push(line.clone());
            i += 1;
            continue;
        }
        let table_parse = if in_fence { None } else { try_parse(&lines, i) };
        if let Some((_, next)) = table_parse {
            let prev_is_text = out.last().is_some_and(|prev| !prev.trim().is_empty());
            if prev_is_text {
                out.push(String::new());
            }
            while i < next {
                out.push(lines[i].clone());
                i += 1;
            }
            continue;
        }
        out.push(line.clone());
        i += 1;
    }
    let mut result = out.join("\n");
    if text.ends_with('\n') {
        result.push('\n');
    }
    result
}

/// If a table begins at `lines[start]` (a pipe row immediately followed by a
/// separator row), parse it and return the table plus the index just past it.
pub(super) fn try_parse(lines: &[String], start: usize) -> Option<(Table, usize)> {
    let header_line = lines.get(start)?;
    let sep_line = lines.get(start + 1)?;
    if !looks_like_row(header_line) || !is_separator(sep_line) {
        return None;
    }

    let header: Vec<Vec<Inline>> = split_cells(header_line)
        .into_iter()
        .map(|c| parse_inlines(c.trim()))
        .collect();
    let align = parse_alignment(sep_line, header.len());

    let mut rows = Vec::new();
    let mut i = start + 2;
    while i < lines.len() && looks_like_row(&lines[i]) {
        let row: Vec<Vec<Inline>> = split_cells(&lines[i])
            .into_iter()
            .map(|c| parse_inlines(c.trim()))
            .collect();
        rows.push(row);
        i += 1;
    }

    Some((
        Table {
            align,
            header,
            rows,
        },
        i,
    ))
}

/// A line that could be a table row: contains a pipe and isn't blank.
fn looks_like_row(line: &str) -> bool {
    let t = line.trim();
    t.contains('|') && !t.is_empty()
}

/// A separator row: every cell is dashes with optional leading/trailing colon.
fn is_separator(line: &str) -> bool {
    let cells = split_cells(line);
    if cells.is_empty() {
        return false;
    }
    cells.iter().all(|c| {
        let c = c.trim();
        let core = c.trim_start_matches(':').trim_end_matches(':');
        !core.is_empty() && core.chars().all(|ch| ch == '-')
    })
}

/// Split a pipe row into trimmed cell strings, dropping the empty cells that
/// flank a row written with outer pipes (`| a | b |`).
fn split_cells(line: &str) -> Vec<&str> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').collect()
}

/// Derive per-column [`Align`] from the separator row, padded/truncated to
/// `cols` so it always matches the header width.
fn parse_alignment(sep: &str, cols: usize) -> Vec<Align> {
    let mut align: Vec<Align> = split_cells(sep)
        .into_iter()
        .map(|c| {
            let c = c.trim();
            let left = c.starts_with(':');
            let right = c.ends_with(':');
            match (left, right) {
                (true, true) => Align::Center,
                (true, false) => Align::Left,
                (false, true) => Align::Right,
                (false, false) => Align::None,
            }
        })
        .collect();
    align.resize(cols, Align::None);
    align
}

/// Canonical rich markdown normalization entry point across all Telegram rich send
/// and edit operations (#280).
///
/// Pipeline:
/// 1. Decode named HTML entities (e.g. `&rarr;` -> `→`, `&bull;` -> `•`, `&mdash;` -> `—`, #258).
/// 2. Balance code fences (#240), shield unresolvable markdown images (#289),
///    shield bare leading hashes (#193, #243), reflow collapsed one-line tables
///    (#132, #690), infer missing table separators (#239), and ensure blank line
///    padding before tables (#95) via [`normalize_tables`].
/// 3. Enforce button layout fit constraints for interactive buttons ([`enforce_button_fit`]).
///
/// Idempotent and fence-safe.
pub(crate) fn normalize_rich_markdown(text: &str) -> String {
    normalize_rich_markdown_with_media(text, &[])
}

/// Media-aware twin of [`normalize_rich_markdown`] (#334): the same pipeline, with
/// THIS request's `media` array threaded down to the image shield so a
/// `tg://photo?id=<X>` / `attach://<X>` reference is judged against the entries the
/// request actually carries. Callers that own a media array MUST use this entry —
/// the media-free form reads every `tg`/`attach` ref as an orphan, because a request
/// carrying no media genuinely cannot resolve one.
pub(crate) fn normalize_rich_markdown_with_media(text: &str, media: &[MediaEntry]) -> String {
    let decoded = crate::channels::telegram::markdown::decode_named_entities(text);
    let normalized = normalize_tables_with_media(&decoded, media);
    // Both orphan guards run on EVERY rich path from here (#334, H6) — not just on the
    // plan card's HTML path. The image shield above covers markdown image syntax; these
    // cover a reference written as bare prose text (`tg://photo?id=X`, `attach://X`) and
    // a model-authored `<img>` / `<video>` / `<audio>` tag. Telegram fails the WHOLE
    // message when any of them cannot be resolved (#134, #334). Both are idempotent:
    // each rewrite drops the `//` its trigger requires, and `&lt;` contains no `<`.
    let neutralized = super::mermaid::neutralize_orphan_photo_refs(&normalized, media);
    let prose_shielded = super::mermaid::neutralize_prose_media_html(&neutralized);
    crate::channels::telegram::suggest_options::enforce_button_fit(&prose_shielded)
}

/// Single canonical table-normalization entry for the rich plane (#132):
/// balance unclosed / runaway code fences (#240), expand collapsed one-line
/// tables first (so [`try_parse`] can see them), infer missing table separators
/// (#239), then insert the blank line Telegram's rich parser demands before a
/// table block (#95). Every rich-build entry point and the structure-detection
/// gate call THIS — never the passes individually — so gate and renderer always
/// agree on the same text and a new send path inherits all fixes (#690, #980,
/// #1085 whack-a-mole retired). All passes are idempotent and fence-safe;
/// pipe-free input returns unchanged.
///
/// Also shields unresolvable markdown images (`![alt](path)`) so Telegram's rich parser
/// doesn't reject the whole message with `RICH_MESSAGE_PHOTO_URL_INVALID` (#289),
/// and shields bare leading hashes (e.g. `#174`) so Telegram's rich parser doesn't
/// promote them into headings without CommonMark's required trailing space (#193).
pub(crate) fn normalize_tables(text: &str) -> String {
    normalize_tables_with_media(text, &[])
}

/// Media-aware twin of [`normalize_tables`] (#334). Identical pass order, with
/// `media` reaching the image shield so `tg`/`attach` references are judged against
/// the media array of the request this text belongs to. A caller with no media array
/// uses [`normalize_tables`], which is this function with an empty slice.
pub(crate) fn normalize_tables_with_media(text: &str, media: &[MediaEntry]) -> String {
    let balanced = balance_code_fences(text);
    let images_shielded = shield_unresolvable_markdown_images(&balanced, media);
    let shielded = shield_bare_leading_hashes(&images_shielded);
    let reflowed = reflow_collapsed_tables(&shielded);
    let inferred = infer_missing_table_separators(&reflowed);
    ensure_blank_line_before_tables(&inferred)
}

/// Balance unclosed or runaway code fences in markdown text (#240).
///
/// LLM responses occasionally emit an odd number of ```` ``` ```` or `~~~` delimiters,
/// or an unclosed bare code fence immediately preceding top-level Markdown structure
/// (such as section headings `## ` or tables `|---|`). Without balancing, Telegram's
/// server-side parser and the local fallback parser swallow all subsequent text into
/// a giant monospaced code block.
///
/// Rules:
/// 1. Track open code fence delimiter (`'` or `~`) and minimum run length (>=3).
/// 2. For bare/unlabeled code fences (no language tag), if a top-level ATX heading
///    (e.g. `## `) or GFM table separator row (`|---|`) is encountered while `in_fence`
///    is true, the runaway fence is closed immediately prior to that line. Explicitly
///    labeled fences (e.g. ` ```rust `, ` ```python `) remain open across `#` comments.
/// 3. If a code fence remains unclosed at EOF, automatically append the matching closing
///    delimiter on its own line.
pub(crate) fn balance_code_fences(text: &str) -> String {
    if !text.contains("```") && !text.contains("~~~") {
        return text.to_string();
    }

    let mut out: Vec<String> = Vec::new();
    let mut in_fence = false;
    let mut fence_char = ' ';
    let mut fence_len = 0;
    let mut fence_has_lang = false;

    let lines: Vec<&str> = text.split('\n').collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();

        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let ch = trimmed.chars().next().unwrap();
            let count = trimmed.chars().take_while(|&c| c == ch).count();
            if count >= 3 {
                let rest = trimmed[count..].trim();
                let has_backtick_in_info = ch == '`' && rest.contains('`');

                if !has_backtick_in_info {
                    if !in_fence {
                        in_fence = true;
                        fence_char = ch;
                        fence_len = count;
                        fence_has_lang = !rest.is_empty();
                        out.push(line.to_string());
                        i += 1;
                        continue;
                    } else if ch == fence_char && count >= fence_len && rest.is_empty() {
                        in_fence = false;
                        fence_has_lang = false;
                        out.push(line.to_string());
                        i += 1;
                        continue;
                    }
                }
            }
        }

        // Check for runaway bare fence: if inside an unlabeled fence and we hit a top-level
        // ATX heading or a GFM table delimiter row, close the fence early before this line.
        if in_fence && !fence_has_lang {
            let is_heading = trimmed.starts_with('#') && super::detect::is_atx_heading(trimmed);
            let is_table_sep = is_separator(trimmed);
            let is_table_start = if i + 1 < lines.len() {
                looks_like_row(trimmed) && is_separator(lines[i + 1].trim_start())
            } else {
                false
            };

            if is_heading || is_table_sep || is_table_start {
                out.push(fence_char.to_string().repeat(fence_len));
                in_fence = false;
                out.push(line.to_string());
                i += 1;
                continue;
            }
        }

        out.push(line.to_string());
        i += 1;
    }

    if in_fence {
        out.push(fence_char.to_string().repeat(fence_len));
    }

    let mut result = out.join("\n");
    if text.ends_with('\n') && !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

/// Infer missing table separator rows: when two consecutive pipe-bearing
/// lines look like header + body but the `|---|` separator between them is
/// absent, synthesize it (#239). Skips content inside code fences and
/// positions where a well-formed table already parses.
pub(crate) fn infer_missing_table_separators(text: &str) -> String {
    if !text.contains('|') {
        return text.to_string();
    }
    let lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 2);
    let mut in_fence = false;
    let mut fence_char = ' ';
    let mut fence_len = 0;
    let mut i = 0;

    while i < lines.len() {
        let line = &lines[i];
        let trimmed = line.trim_start();

        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let ch = trimmed.chars().next().unwrap();
            let count = trimmed.chars().take_while(|&c| c == ch).count();
            if !in_fence {
                in_fence = true;
                fence_char = ch;
                fence_len = count;
                out.push(line.clone());
                i += 1;
                continue;
            } else if ch == fence_char && count >= fence_len {
                in_fence = false;
                out.push(line.clone());
                i += 1;
                continue;
            }
        }

        if in_fence {
            out.push(line.clone());
            i += 1;
            continue;
        }

        // Check if this position is already a well-formed table with a valid separator
        if let Some((_, next)) = try_parse(&lines, i) {
            while i < next {
                out.push(lines[i].clone());
                i += 1;
            }
            continue;
        }

        // Check if lines[i] and lines[i+1] look like a table missing a separator
        if looks_like_row(line) && !is_separator(line) && i + 1 < lines.len() {
            let next_line = &lines[i + 1];
            if looks_like_row(next_line) && !is_separator(next_line) {
                let h_cells = split_cells(line);
                let n_cells = split_cells(next_line);
                if h_cells.len() >= 2 && n_cells.len() >= 2 {
                    // Line i is the header row!
                    out.push(line.clone());
                    let sep = format!("|{}|", vec!["---"; h_cells.len()].join("|"));
                    out.push(sep);
                    i += 1;
                    // Consume all consecutive body rows so we don't insert multiple separators
                    while i < lines.len() && looks_like_row(&lines[i]) && !is_separator(&lines[i]) {
                        out.push(lines[i].clone());
                        i += 1;
                    }
                    continue;
                }
            }
        }

        out.push(line.clone());
        i += 1;
    }

    let mut result = out.join("\n");
    if text.ends_with('\n') {
        result.push('\n');
    }
    result
}

/// Escape bare leading `#` (not followed by space, or not a valid ATX heading)
/// with a backslash (`\#`) so Telegram's native rich parser does not promote
/// lines like `#174`, list items like `- #224`, or quotes like `> #236`
/// into `<h1>` headers (#193, #243, adolfousier/opencrabs#1257).
///
/// Leaves code fences (``` and ~~~) and valid ATX headings untouched.
pub(crate) fn shield_bare_leading_hashes(text: &str) -> String {
    if !text.contains('#') {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len() + 16);
    let mut in_fence = false;
    let mut fence_char = ' ';
    let mut fence_len = 0;

    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }

        let trimmed = line.trim_start();

        // Check for code fence start/end
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let ch = trimmed.chars().next().unwrap();
            let count = trimmed.chars().take_while(|&c| c == ch).count();
            if !in_fence {
                in_fence = true;
                fence_char = ch;
                fence_len = count;
                out.push_str(line);
                continue;
            } else if ch == fence_char && count >= fence_len {
                in_fence = false;
                out.push_str(line);
                continue;
            }
        }

        if in_fence {
            out.push_str(line);
            continue;
        }

        // Outside fence: check if line starts with bare # or has a bare # following
        // blockquote, bullet, numbered list, or checkbox prefixes (#243).
        if let Some(prefix_len) = find_bare_hash_offset(line) {
            out.push_str(&line[..prefix_len]);
            out.push('\\');
            out.push_str(&line[prefix_len..]);
        } else {
            out.push_str(line);
        }
    }

    out
}

/// Find the byte offset in `line` where a bare leading `#` begins (after optional
/// blockquote, list bullet, ordered number, or checkbox markers).
/// Returns `None` if the line contains no bare `#`, starts with a valid ATX heading (`# `),
/// or is already escaped (`\#`).
fn find_bare_hash_offset(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        return None;
    }

    let mut rest = trimmed;

    // 1. Strip blockquote prefixes (> or >> or > >)
    while let Some(after) = rest.strip_prefix('>') {
        rest = after.trim_start();
    }

    // 2. Strip unordered list bullets (-, *, +) or ordered list markers (1., 1))
    if let Some(after) = rest
        .strip_prefix("- ")
        .or_else(|| rest.strip_prefix("* "))
        .or_else(|| rest.strip_prefix("+ "))
    {
        rest = after.trim_start();
        if let Some(after_cb) = rest
            .strip_prefix("[ ] ")
            .or_else(|| rest.strip_prefix("[x] "))
            .or_else(|| rest.strip_prefix("[X] "))
        {
            rest = after_cb.trim_start();
        }
    } else if let Some(after_num) = strip_ordered_list_prefix(rest) {
        rest = after_num;
        if let Some(after_cb) = rest
            .strip_prefix("[ ] ")
            .or_else(|| rest.strip_prefix("[x] "))
            .or_else(|| rest.strip_prefix("[X] "))
        {
            rest = after_cb.trim_start();
        }
    }

    // 3. Check if remaining text starts with bare # (not an ATX heading and not already escaped)
    if rest.starts_with('#') && !super::is_atx_heading(rest) && !rest.starts_with("\\#") {
        let prefix_len = line.len() - rest.len();
        Some(prefix_len)
    } else {
        None
    }
}

/// Strip standard CommonMark ordered list prefix (1-9 digits followed by `.` or `)` and whitespace).
fn strip_ordered_list_prefix(s: &str) -> Option<&str> {
    let digits = s.chars().take_while(|c| c.is_ascii_digit()).count();
    if (1..=9).contains(&digits) {
        let after_digits = &s[digits..];
        let after_delim = after_digits
            .strip_prefix('.')
            .or_else(|| after_digits.strip_prefix(')'))?;
        if after_delim.starts_with(' ') || after_delim.starts_with('\t') {
            return Some(after_delim.trim_start());
        }
    }
    None
}

/// Neutralize unresolvable Markdown image references (e.g. `![alt](path)`)
/// where target URL is not a valid HTTP(S) link, `tg://photo?id=`, or `attach://` (#289).
///
/// Telegram's rich message API parses `![alt](url)` into a media attachment entity.
/// If the URL is a local filesystem path (e.g. `/tmp/fig.png` or `docs/img.png`) or an
/// arbitrary string not resolvable by Telegram, the entire send/edit is rejected with:
/// `(400 Bad Request): Bad Request: RICH_MESSAGE_PHOTO_URL_INVALID`.
///
/// Escaping the leading `!` to `\!` neutralizes Telegram's photo entity parsing while
/// preserving the text as a readable markdown link `\![alt](path)` (or plain text).
/// Valid remote image URLs (`http://`, `https://`) are preserved by SCHEME alone —
/// Telegram fetches those itself. A Telegram media reference (`tg://photo?id=<X>`,
/// `tg://video?id=<X>`, `tg://audio?id=<X>`, `attach://<X>`) is preserved only when
/// `<X>` names an entry in THIS request's `media` array: that array is the sole
/// authority for whether such a reference resolves (#334). A reference judged valid
/// by scheme alone is rejected server-side with `RICH_MESSAGE_PHOTO_NO_MEDIA_FOUND`,
/// taking the whole message down (live: 4 events on 2026-09-18, all plan-card paths).
///
/// Fence-safe: leaves code fences (``` and ~~~) and inline code spans untouched.
pub(crate) fn shield_unresolvable_markdown_images(text: &str, media: &[MediaEntry]) -> String {
    if !text.contains("![") {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len() + 16);
    let mut in_fence = false;
    let mut fence_char = ' ';
    let mut fence_len = 0;

    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }

        let trimmed = line.trim_start();

        // Check for code fence start/end
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let ch = trimmed.chars().next().unwrap();
            let count = trimmed.chars().take_while(|&c| c == ch).count();
            if !in_fence {
                in_fence = true;
                fence_char = ch;
                fence_len = count;
                out.push_str(line);
                continue;
            } else if ch == fence_char && count >= fence_len {
                in_fence = false;
                out.push_str(line);
                continue;
            }
        }

        if in_fence || !line.contains("![") {
            out.push_str(line);
            continue;
        }

        // Process line outside code fences
        shield_line_unresolvable_images(line, &mut out, media);
    }

    out
}

/// Scan a single line for `![alt](target)` constructs outside inline code spans
/// and escape `!` if the target is not a resolvable photo reference. `media` is
/// the media array of the SAME request, so a `tg`/`attach` reference is judged
/// against the entries that request actually carries — see
/// [`is_valid_telegram_photo_url`].
fn shield_line_unresolvable_images(line: &str, out: &mut String, media: &[MediaEntry]) {
    let bytes = line.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        // Skip inline code spans: `...`
        if bytes[i] == b'`' {
            let code_start = i;
            let code_delim_len = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            let delim = &line[code_start..code_start + code_delim_len];
            let after_open = code_start + code_delim_len;
            if let Some(close_idx) = line[after_open..].find(delim) {
                let code_end = after_open + close_idx + code_delim_len;
                out.push_str(&line[code_start..code_end]);
                i = code_end;
                continue;
            } else {
                // Unclosed backtick - output rest
                out.push_str(&line[code_start..]);
                break;
            }
        }

        // Check if `![` starts here
        if bytes[i] == b'!' && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            // Check if already escaped: preceded by `\` (and not `\\`)
            let is_escaped = i > 0 && bytes[i - 1] == b'\\' && (i < 2 || bytes[i - 2] != b'\\');
            if is_escaped {
                out.push('!');
                i += 1;
                continue;
            }

            // Look for matching `](target)`
            if let Some((target, match_end)) = parse_markdown_image_at(line, i) {
                if is_valid_telegram_photo_url(target, media) {
                    // Valid URL - keep as is
                    out.push_str(&line[i..match_end]);
                } else {
                    // Unresolvable URL - escape leading `!` to `\!`
                    out.push('\\');
                    out.push_str(&line[i..match_end]);
                }
                i = match_end;
                continue;
            }
        }

        // Normal character - find next special char (` or !)
        let next_special = line[i..].find(['`', '!']).unwrap_or(line[i..].len());
        if next_special == 0 {
            // Current char is ` or ! handled above; advance single char
            if let Some(ch) = line[i..].chars().next() {
                out.push(ch);
                i += ch.len_utf8();
            } else {
                break;
            }
        } else {
            out.push_str(&line[i..i + next_special]);
            i += next_special;
        }
    }
}

/// Try parsing `![alt](target)` starting at offset `start` where `line[start..start+2] == "!["`.
/// Returns `Some((target_url, byte_end_offset))` on valid image syntax.
fn parse_markdown_image_at(line: &str, start: usize) -> Option<(&str, usize)> {
    let after_bang = start + 1; // points to `[`
    let rest = &line[after_bang..];
    if !rest.starts_with('[') {
        return None;
    }

    // Find matching `]` for alt text (handling nested brackets or simple scan)
    let mut bracket_depth = 0;
    let mut close_bracket_pos = None;
    for (idx, ch) in rest.char_indices() {
        if ch == '[' {
            bracket_depth += 1;
        } else if ch == ']' {
            bracket_depth -= 1;
            if bracket_depth == 0 {
                close_bracket_pos = Some(idx);
                break;
            }
        }
    }

    let close_bracket = close_bracket_pos?;
    let after_bracket = after_bang + close_bracket + 1;
    if after_bracket >= line.len() || !line[after_bracket..].starts_with('(') {
        return None;
    }

    let target_start = after_bracket + 1;
    // Find matching `)` for target URL (simple scan until `)`)
    let close_paren = line[target_start..].find(')')?;
    let target = line[target_start..target_start + close_paren].trim();
    let match_end = target_start + close_paren + 1;

    Some((target, match_end))
}

/// Check whether `target` is a valid Telegram photo URL/ref **for this request**.
///
/// - `http://` / `https://` — valid by scheme; Telegram fetches the URL itself.
/// - `tg://photo?id=<X>` / `tg://video?id=<X>` / `tg://audio?id=<X>` / `attach://<X>` —
///   valid **iff** `<X>` names an entry in `media`, the media array of the very request
///   this body belongs to. Scheme alone is NOT sufficient: a reference with no matching
///   entry is rejected with `RICH_MESSAGE_PHOTO_NO_MEDIA_FOUND`, which fails the whole
///   message rather than that one image (#334).
/// - anything else — invalid (a local path, a bare string).
///
/// An empty `media` therefore means every `tg`/`attach` reference is an orphan, which is
/// the correct reading for the builders that carry no media array at all.
fn is_valid_telegram_photo_url(target: &str, media: &[MediaEntry]) -> bool {
    let trimmed = target.trim();
    // Strip optional enclosing `<...>`
    let url = if trimmed.starts_with('<') && trimmed.ends_with('>') && trimmed.len() >= 2 {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };

    if url.starts_with("http://") || url.starts_with("https://") {
        return true;
    }

    // `attach://<X>` — the id is everything after the scheme.
    if let Some(id) = url.strip_prefix("attach://") {
        return media_id_present(id, media);
    }

    // `tg://photo?id=<X>` and siblings (`tg://video?id=`, `tg://audio?id=`).
    if let Some(id) = telegram_media_id(url) {
        return media_id_present(id, media);
    }

    false
}

/// Extract `<X>` from a `tg://<kind>?id=<X>` media reference, or `None` when
/// `url` is not one. The id runs to the first `&` (Telegram media refs carry a
/// single `id` param in practice; a trailing query fragment is not part of it).
fn telegram_media_id(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("tg://")?;
    let (kind, query) = rest.split_once('?')?;
    if !matches!(kind, "photo" | "video" | "audio") {
        return None;
    }
    let id = query.strip_prefix("id=")?;
    Some(id.split('&').next().unwrap_or(id))
}

/// Whether `id` names an entry in this request's media array.
fn media_id_present(id: &str, media: &[MediaEntry]) -> bool {
    !id.is_empty() && media.iter().any(|m| m.id == id)
}
