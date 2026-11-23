//! Per-agent progress ledger (#852).
//!
//! A sub-agent's live progress used to be one mutable snapshot
//! (`WorkStatus.progress`, overwritten every round), so a parent could see
//! *what the agent is doing now* and never *what it has done*. This is the
//! history beside that snapshot: one append-only JSONL line per entry, written
//! by the child and read by the parent while the child is still running.
//!
//! Deliberately not a state source. `WorkStatus.state` and `.progress` stay
//! authoritative — nothing here decides whether an agent is alive. The ledger
//! only narrates.

use crate::brain::agent::service::work_status::progress_path;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;

/// How long one entry's text may be. Long enough for a real finding, short
/// enough that a chatty child cannot turn its ledger into a transcript.
const MAX_TEXT: usize = 400;

/// What kind of entry this is.
///
/// `Blocker` and `Question` are the two a parent must not miss; the rest are
/// narrative. The distinction lives in the kind, not in the text, so a reader
/// can filter without parsing prose.
///
/// Four kinds, each with one job, because a dead option is worse than a
/// missing one (#984): `tool_call` used to sit in this enum and was chosen
/// zero times in 62 entries across 47 agents, so all it did was give the model
/// a fifth way to pick something uninformative. Anything worth recording about
/// a call is either narration (`Note`) or a choice (`Decision`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerKind {
    /// Free-form narration: what the agent just did or found.
    Note,
    /// A decision taken, with its reason.
    Decision,
    /// The agent cannot proceed and needs something changed.
    Blocker,
    /// The agent needs an answer before it can continue.
    Question,
}

impl LedgerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Decision => "decision",
            Self::Blocker => "blocker",
            Self::Question => "question",
        }
    }
}

/// One line of the ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// RFC3339, stamped by the writer — the reader never has to guess when a
    /// line was produced.
    pub ts: String,
    pub kind: LedgerKind,
    pub text: String,
}

impl LedgerEntry {
    pub fn new(kind: LedgerKind, text: impl Into<String>) -> Self {
        Self {
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            kind,
            text: text.into(),
        }
    }
}

/// Make one entry safe to sit on a single line.
///
/// A raw newline would split the record in two: the first half parses as an
/// entry with a truncated text and the second is not JSON at all, so a reader
/// skipping malformed lines loses the rest of the message without a trace.
/// Tabs and carriage returns are flattened for the same reason — the ledger is
/// line-oriented, and nothing else may claim a line boundary.
fn sanitize(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| match c {
            '\n' | '\r' | '\t' => ' ',
            other => other,
        })
        .collect();
    let trimmed = flat.trim();
    if trimmed.chars().count() <= MAX_TEXT {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(MAX_TEXT).collect();
    format!("{cut}…")
}

/// Append one entry to an agent's ledger.
///
/// Errors are logged and swallowed: a progress write must never fail a child's
/// turn. A missing ledger line is a cosmetic loss; a failed turn is not.
pub fn append(id: &str, entry: &LedgerEntry) {
    let path = progress_path(id);
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(target: "subagent", "Could not create ledger dir {}: {e}", parent.display());
        return;
    }
    let line = match serde_json::to_string(&LedgerEntry {
        text: sanitize(&entry.text),
        ..entry.clone()
    }) {
        Ok(line) => line,
        Err(e) => {
            tracing::warn!(target: "subagent", "Could not encode ledger entry for {id}: {e}");
            return;
        }
    };
    let result = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| writeln!(f, "{line}"));
    if let Err(e) = result {
        tracing::warn!(target: "subagent", "Could not append to ledger {}: {e}", path.display());
    }
}

/// The last `limit` entries, oldest first.
///
/// Malformed lines are skipped rather than fatal: a torn final line is the
/// expected shape after a crash mid-write, and one bad line must not hide the
/// history before it. An absent ledger is an empty history, not an error.
pub fn read(id: &str, limit: usize) -> Vec<LedgerEntry> {
    let path = progress_path(id);
    let Ok(data) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let entries: Vec<LedgerEntry> = data
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<LedgerEntry>(line).ok())
        .collect();
    let start = entries.len().saturating_sub(limit);
    entries[start..].to_vec()
}

/// Whether an agent has recorded anything at all.
pub fn is_empty(id: &str) -> bool {
    read(id, 1).is_empty()
}
