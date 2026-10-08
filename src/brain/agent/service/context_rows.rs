//! Clean persisted rows before they become LLM context.
//!
//! Rows carry more than the conversation: `<!-- tools-v2: ... -->` ledgers
//! for TUI replay and cancel-persist, `<!-- reasoning -->` blocks holding
//! the model's deliberation, phantom-blocked sections, and the compaction
//! banner on any row that is not itself a compaction marker. Feeding those
//! back teaches a model to echo the ledger format verbatim and lets it read
//! its own discarded narration as history.
//!
//! This ran for API providers only, on the belief that a CLI subprocess
//! never saw the content. It did: the CLI prompt builder flattens every
//! text block verbatim into stdin, so a session that had run on API
//! providers handed the CLI its whole replay ledger and every other model's
//! reasoning as conversation, about five times the real text (#1522).
//! Cleaning touches the in-memory copy only. Rows are appended to, never
//! rewritten from this copy, so TUI replay keeps its ledgers.

use crate::brain::agent::context::COMPACTION_MARKER_PREFIX;
use crate::brain::agent::service::AgentService;
use crate::db::models::Message as DbMessage;

/// Strip replay ledgers, reasoning markers, phantom-blocked sections and the
/// compaction banner from `rows`, in place.
///
/// A row that IS a compaction marker (`role == "user"` and content beginning
/// with [`COMPACTION_MARKER_PREFIX`]) keeps its banner: that banner is the
/// identity `AgentContext::is_compaction_marker_msg` keys on, and #1649's
/// delta scope anchors on it (#767).
///
/// With `preserve_thinking`, an assistant row's reasoning is hoisted into
/// its `thinking` column so the encoder can return it as `reasoning_content`
/// for the models that require it (#654). Without it, the reasoning is
/// dropped with its markers: a CLI provider must never receive another
/// model's chain of thought as prompt text.
pub(crate) fn clean_rows_for_llm(rows: &mut [DbMessage], preserve_thinking: bool) {
    for msg in rows.iter_mut() {
        // #1172: phantom-blocked sections must never re-enter LLM context
        // (#86). Strip them before the generic artifact sweep, which would
        // only remove their markers and leave the narration standing.
        if msg.content.contains("<!-- phantom_blocked=1 -->") {
            msg.content = crate::utils::sanitize::strip_phantom_blocked(&msg.content);
        }
        if preserve_thinking && msg.role == "assistant" {
            let (cleaned, reasoning) = crate::utils::sanitize::hoist_reasoning_blocks(&msg.content);
            if let Some(reasoning) = reasoning {
                msg.content = cleaned;
                match msg.thinking.as_mut() {
                    Some(existing) if !existing.trim().is_empty() => {
                        existing.push_str("\n\n");
                        existing.push_str(&reasoning);
                    }
                    _ => msg.thinking = Some(reasoning),
                }
            }
        }
        if msg.content.contains("<!--") {
            msg.content = crate::utils::sanitize::strip_llm_artifacts(&msg.content);
        }
        // #767: a compaction MARKER keeps its banner through this clean.
        //
        // `is_compaction_marker_msg` keys on the banner prefix and #1649's
        // delta scope anchors on the marker it identifies, so stripping the
        // banner here silently reverted every RELOADED session to FullWindow:
        // the marker became an ordinary user row, `last_marker_index()` was
        // `None`, and the next compaction re-derived and re-billed every prior
        // summary. The in-memory apply path (`compact_with_summary`,
        // `compact_with_delta_summary`, `consolidate_segments`,
        // `hard_truncate_to`) has always kept its banner, so the two paths
        // disagreed about the same row; they now agree.
        //
        // The echo-prevention this strip was added for (`0c0bf4126`) still
        // holds where it was aimed: an assistant row echoing the banner — the
        // actual failure — is not a marker and is still stripped.
        let is_marker = msg.role == "user" && msg.content.starts_with(COMPACTION_MARKER_PREFIX);
        if !is_marker {
            AgentService::strip_compaction_banner(&mut msg.content);
        }
    }
}
