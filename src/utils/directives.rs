//! Text-directive recovery for options the model wrote as TEXT instead of
//! invoking `suggest_options` (#1774, #892).
//!
//! When the model fails to emit the structured tool call, it sometimes
//! imitates the marker style taught in prompts (`<<react:...>>`) and writes
//! the options into its final text. The residue then ships to channels as raw
//! JSON with dead markers: buttons never render because the tool never ran.
//!
//! TWO grammars are recognised, because the model generalises the
//! `name:payload` shape it is taught:
//!
//! - the marker PAIR — `<<suggest_options>>[ … ]<<suggest_options>>` (#1774),
//! - the COLON form — `<<suggest_options:[ … ]>>` (#892, observed live on
//!   Telegram 2026-11-19: the pair-only extractor missed it and the raw
//!   directive was delivered as prose).
//!
//! [`extract_leaked_suggestions`] recovers every block: the array is parsed
//! into real `SuggestionItem`s (so every surface's native renderer fires via
//! `ProgressEvent::SuggestedOptions`) and the whole block is stripped from
//! the delivered text. A block whose payload does not parse is stripped too:
//! raw `<<...>>` markers and raw JSON never reach a channel.
//!
//! One scanner serves both grammars ([`scan_leaked_blocks`]) so a future
//! variant is a new arm in one place, never a second delimiter branch bolted
//! beside the first.

use crate::brain::tools::suggest_options::{MAX_OPTIONS, RawSuggestionItem, SuggestionItem};

/// Marker stem shared by both grammars: the pair form opens with
/// `<<suggest_options>>` and the colon form with `<<suggest_options:`.
const OPEN: &str = "<<suggest_options";
/// Block terminator: the pair form closes with a second [`PAIR_MARKER`], the
/// colon form with this token.
const CLOSE: &str = ">>";
/// The complete pair-form token. A pair block is `PAIR_MARKER … PAIR_MARKER`.
const PAIR_MARKER: &str = "<<suggest_options>>";
/// How many `>>` candidates a colon block probes before settling for the
/// first one. A `>>` inside the payload (a label like `"Ship it >>"`) would
/// otherwise cut the block short and strand the remainder as raw text.
const MAX_COLON_TERMINATOR_TRIES: usize = 4;
/// How far past the stem an unrecognised `<<suggest_options…` delimiter may
/// reach for its `>>` before only the stem is dropped.
const ALIEN_DELIMITER_WINDOW: usize = 64;

/// One recovered marker block: its byte span in the source text and its inner
/// payload.
struct LeakedBlock<'a> {
    start: usize,
    end: usize,
    payload: &'a str,
    /// `true` when the block ran to a terminator (or to the end of the text),
    /// so removing it leaves a seam that is re-flowed with a blank line.
    /// `false` for a lone opening token, which is deleted in place — the
    /// behaviour the pair form has always had.
    terminated: bool,
}

/// Recover every leaked `suggest_options` block from final-response text.
///
/// Returns `(cleaned_text, Some(items))` when at least one block held a
/// parseable, non-empty options array, and `(cleaned_text, None)` in every
/// other case (no blocks, unparseable or empty payloads). The cleaned text
/// never contains a marker token; surrounding prose is preserved with a blank
/// line at each seam.
///
/// **Every occurrence is handled.** Two leaked blocks in one reply are both
/// stripped, and their items are MERGED in document order (capped at
/// [`MAX_OPTIONS`]) rather than the second being silently dropped — a reply
/// that leaks twice must not leak once.
pub fn extract_leaked_suggestions(text: &str) -> (String, Option<Vec<SuggestionItem>>) {
    let blocks = scan_leaked_blocks(text);
    if blocks.is_empty() {
        return (text.to_string(), None);
    }

    // The strip is unconditional; only the ITEMS depend on the payload
    // parsing, so a malformed block can never survive as raw text.
    let items: Vec<SuggestionItem> = blocks
        .iter()
        .filter_map(|block| parse_payload(block.payload))
        .flatten()
        .take(MAX_OPTIONS)
        .collect();

    let cleaned = strip_blocks(text, &blocks);
    let recovered = (!items.is_empty()).then_some(items);
    (cleaned, recovered)
}

/// The shared scanner: yields every block, in document order, for BOTH
/// grammars. A malformed opening (`<<suggest_options` followed by neither
/// `>>` nor `:`) is dropped as a bare token so it can never render.
fn scan_leaked_blocks(text: &str) -> Vec<LeakedBlock<'_>> {
    let mut blocks = Vec::new();
    let mut cursor = 0usize;

    while let Some(rel) = text[cursor..].find(OPEN) {
        let start = cursor + rel;
        let after_open = start + OPEN.len();
        let rest = &text[after_open..];

        if rest.starts_with(CLOSE) {
            // Pair form: `<<suggest_options>> … <<suggest_options>>`.
            let body_start = after_open + CLOSE.len();
            match text[body_start..].find(PAIR_MARKER) {
                Some(rel_close) => {
                    let body_end = body_start + rel_close;
                    let end = body_end + PAIR_MARKER.len();
                    blocks.push(LeakedBlock {
                        start,
                        end,
                        payload: &text[body_start..body_end],
                        terminated: true,
                    });
                    cursor = end;
                }
                None => {
                    // Lone opening marker: drop the token, keep the text.
                    blocks.push(LeakedBlock {
                        start,
                        end: body_start,
                        payload: "",
                        terminated: false,
                    });
                    cursor = body_start;
                }
            }
        } else if rest.starts_with(':') {
            // Colon form: `<<suggest_options: … >>`. Without a terminator the
            // payload runs to the end of the text — there is no other place
            // it could stop, and leaving it would ship raw JSON.
            let body_start = after_open + 1;
            let (body_end, end) = match find_colon_terminator(text, body_start) {
                Some(rel_close) => {
                    let body_end = body_start + rel_close;
                    (body_end, body_end + CLOSE.len())
                }
                None => (text.len(), text.len()),
            };
            blocks.push(LeakedBlock {
                start,
                end,
                payload: &text[body_start..body_end],
                terminated: true,
            });
            cursor = end;
        } else {
            // Alien delimiter: `<<suggest_options` followed by something that
            // is neither grammar (`<<suggest_options_other>>`, a stray space
            // before `>>`). Drop the whole `<<…>>` span when it closes within
            // a short window, otherwise just the stem — no `<<` may render.
            let end = text[after_open..]
                .find(CLOSE)
                .filter(|rel| *rel <= ALIEN_DELIMITER_WINDOW)
                .map(|rel| after_open + rel + CLOSE.len())
                .unwrap_or(after_open);
            blocks.push(LeakedBlock {
                start,
                end,
                payload: "",
                terminated: false,
            });
            cursor = end;
        }
    }

    blocks
}

/// Offset (relative to `body_start`) of the `>>` that closes a colon block.
///
/// The first few candidates are probed and the first one whose payload parses
/// wins, so a `>>` inside the JSON cannot truncate the block. When none
/// parses, the first candidate is returned anyway: strip-on-failure.
fn find_colon_terminator(text: &str, body_start: usize) -> Option<usize> {
    let mut first: Option<usize> = None;
    let mut search = body_start;
    for _ in 0..MAX_COLON_TERMINATOR_TRIES {
        let Some(rel) = text[search..].find(CLOSE) else {
            break;
        };
        let abs = search + rel;
        if first.is_none() {
            first = Some(abs - body_start);
        }
        if parse_payload(&text[body_start..abs]).is_some() {
            return Some(abs - body_start);
        }
        search = abs + CLOSE.len();
    }
    first
}

/// Parse a block payload as an options array. An empty array is not
/// recoverable: it carries no buttons, and the block is stripped either way.
fn parse_payload(payload: &str) -> Option<Vec<SuggestionItem>> {
    serde_json::from_str::<Vec<RawSuggestionItem>>(payload.trim())
        .ok()
        .filter(|items| !items.is_empty())
        .map(|items| {
            items
                .into_iter()
                .map(RawSuggestionItem::into_suggestion_item)
                .collect::<Vec<SuggestionItem>>()
        })
}

/// Rebuild the text without the blocks, re-flowing each seam with one blank
/// line. Text between two removed blocks keeps a blank line on both sides, and
/// prose that only framed a block collapses cleanly.
fn strip_blocks(text: &str, blocks: &[LeakedBlock<'_>]) -> String {
    // Segment `i` sits between block `i - 1` and block `i`.
    let mut segments: Vec<&str> = Vec::with_capacity(blocks.len() + 1);
    let mut cursor = 0usize;
    for block in blocks {
        segments.push(&text[cursor..block.start]);
        cursor = block.end;
    }
    segments.push(&text[cursor..]);

    let mut out = String::with_capacity(text.len());
    let mut pending_break = false;
    for (i, segment) in segments.iter().enumerate() {
        let after_terminated = i > 0 && blocks[i - 1].terminated;
        let before_terminated = i < blocks.len() && blocks[i].terminated;

        let mut segment = *segment;
        if after_terminated {
            segment = segment.trim_start();
        }
        if before_terminated {
            segment = segment.trim_end();
        }
        if segment.is_empty() {
            continue;
        }
        if pending_break && !out.is_empty() {
            out.push_str("\n\n");
        }
        pending_break = false;
        out.push_str(segment);
        if before_terminated {
            pending_break = true;
        }
    }
    out
}
