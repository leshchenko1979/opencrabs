//! The compaction boundary — one predicate for "where does the reload window
//! start".
//!
//! Three readers ask that one question: the SQL loader
//! ([`MessageRepository::load_compaction_window`]), the in-memory trim
//! ([`AgentService::messages_from_last_compaction`]) and the delta-scope
//! helpers on [`AgentContext`]. They used to answer it in three places — the
//! SQL path passed a `marker_prefix` plus an `excluded_prefix` string, the
//! in-memory path re-derived the delta banner and did its own `rposition` —
//! and two implementations of one rule drift (#854, #987). The classification
//! lives here; every reader calls it.
//!
//! # What counts as a marker
//!
//! A marker is a `user` row whose content BEGINS with
//! [`COMPACTION_MARKER_PREFIX`]. Anchoring at the row start (never `contains`)
//! is #175: an assistant row echoing the banner, or a lane reading it out of a
//! log, is not a marker. Audit over four live DBs: 1813 `user` marker rows, all
//! 1813 starting with the prefix; 243 `assistant` rows containing it, none of
//! them a marker.
//!
//! # Which markers a reload may anchor on
//!
//! | kind | banner | reload boundary? |
//! |---|---|---|
//! | [`MarkerKind::Restart`] | `[CONTEXT COMPACTION — The conversation…` | **yes** |
//! | [`MarkerKind::DeltaSegment`] | `[CONTEXT COMPACTION — DELTA SEGMENT. …` | no |
//! | [`MarkerKind::Suspended`] | `[CONTEXT COMPACTION SUSPENDED — …` | no |
//!
//! A **delta segment** EXTENDS the window its restart opened (#1928). Anchoring
//! a reload on the newest segment instead of the restart makes every segment a
//! fresh boundary, so the loader keeps only the newest one.
//!
//! A **suspended** row is the loop guard / non-reducing brake
//! (`CompactionOutcome::LoopGuard`): the harness DECLINED to compact, so
//! nothing was summarised and the history above it is still live context.
//! Anchoring a reload there would silently drop everything before it — exactly
//! the loss the row's own text says did not happen.
//!
//! That last rule is a deliberate behaviour change against the pre-#987 loader,
//! which excluded only the delta banner and therefore treated a suspended row
//! as a fresh start.
//!
//! [`MessageRepository::load_compaction_window`]: crate::db::MessageRepository::load_compaction_window
//! [`AgentService::messages_from_last_compaction`]: crate::brain::agent::AgentService::messages_from_last_compaction
//! [`AgentContext`]: crate::brain::agent::context::AgentContext

use crate::brain::agent::context::{COMPACTION_MARKER_PREFIX, SEGMENT_SENTINEL};
use crate::db::models::Message;

/// The loop guard's marker banner. `CompactionOutcome::LoopGuard` and
/// `CompactionOutcome::NonReducingBrake` both open with it, and neither
/// summarised anything.
pub(crate) const SUSPENDED_MARKER_PREFIX: &str = "[CONTEXT COMPACTION SUSPENDED";

/// What separates a marker's banner from its body: `"[CONTEXT COMPACTION — "`.
///
/// Composed from the constants rather than written out, so the delta banner is
/// never spelled a second time.
const BANNER_SEPARATOR: &str = " — ";

/// The three kinds of compaction marker row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarkerKind {
    /// A fresh start: history before this row was summarised away.
    Restart,
    /// A frozen segment appended to the window its restart opened.
    DeltaSegment,
    /// The harness declined to compact; nothing was summarised.
    Suspended,
}

impl MarkerKind {
    /// The wire name, used in the boundary log line.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Restart => "restart",
            Self::DeltaSegment => "delta_segment",
            Self::Suspended => "suspended",
        }
    }
}

/// Classify a row's content as a compaction marker, or `None` if it is not one.
///
/// Role is the caller's business — this looks only at the text, so the SQL path
/// (which filters on `role` in the query) and the in-memory path (which holds
/// typed messages) reach the same verdict from the same bytes.
///
/// Allocation-free on purpose: it runs once per marker row on every reload.
pub(crate) fn classify_user_marker(content: &str) -> Option<MarkerKind> {
    // Checked before the general prefix: the suspended banner is a SUPERSET of
    // `COMPACTION_MARKER_PREFIX`, so a plain prefix match would call it a
    // restart.
    if content.starts_with(SUSPENDED_MARKER_PREFIX) {
        return Some(MarkerKind::Suspended);
    }
    let rest = content.strip_prefix(COMPACTION_MARKER_PREFIX)?;
    match rest.strip_prefix(BANNER_SEPARATOR) {
        Some(body) if body.starts_with(SEGMENT_SENTINEL) => Some(MarkerKind::DeltaSegment),
        // Every other banner — the full-window summary, the truncated-history
        // notice, the RSI cycle seal, the cron boundary — dropped or froze the
        // history before it, so the reload starts here.
        _ => Some(MarkerKind::Restart),
    }
}

/// Classify a database row as a compaction marker, or `None` if it is not one.
///
/// Role lives here so every reader of a `messages` row asks the same question
/// of the same bytes: the SQL loader (which also filters on `role` in the query,
/// for the index), the in-memory trim and the LLM-row cleaner.
pub(crate) fn classify_row(message: &Message) -> Option<MarkerKind> {
    if message.role != "user" {
        return None;
    }
    classify_user_marker(&message.content)
}

/// Whether this row is a boundary a reload may anchor on.
///
/// The in-memory twin of the SQL loader's verdict; `compaction_boundary_test`
/// pins the two to the same answer over one fixture.
pub(crate) fn is_reload_boundary(message: &Message) -> bool {
    classify_row(message) == Some(MarkerKind::Restart)
}

/// Index of the row a rowid-ordered history should start at, if it has a
/// boundary at all.
///
/// `rows` must be in insertion order (`find_by_session` is `ORDER BY rowid
/// ASC`); `rposition` is then the newest boundary by insertion time, not by
/// `sequence`. `sequence` is not authoritative — `MessageService::
/// get_next_sequence` used to assign `count(*) + 1`, so a delete let a later
/// insert reuse an earlier number and an `ORDER BY sequence` boundary picked a
/// stale marker.
pub(crate) fn boundary_start_index(rows: &[Message]) -> Option<usize> {
    rows.iter().rposition(is_reload_boundary)
}

/// The marker vocabulary the SQL loader filters on.
///
/// SQL answers only the coarse question — "rows that might be a marker" — and
/// [`classify_user_marker`] decides the rest, so the boundary rule has exactly
/// one definition instead of a `WHERE` clause and a Rust predicate that have to
/// be kept in step by hand.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BoundarySpec {
    /// Role a marker row must carry.
    pub role: &'static str,
    /// Prefix a candidate row's content must begin with.
    pub marker_prefix: &'static str,
}

impl BoundarySpec {
    /// The compaction vocabulary: `user` rows opening with
    /// [`COMPACTION_MARKER_PREFIX`].
    pub(crate) fn canonical() -> Self {
        Self {
            role: "user",
            marker_prefix: COMPACTION_MARKER_PREFIX,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(role: &str, content: &str) -> Message {
        Message {
            id: uuid::Uuid::new_v4(),
            session_id: uuid::Uuid::new_v4(),
            role: role.to_string(),
            content: content.to_string(),
            sequence: 0,
            created_at: chrono::Utc::now(),
            token_count: None,
            cost: None,
            input_tokens: None,
            cache_creation_tokens: None,
            cache_read_tokens: None,
            thinking: None,
            duration_secs: None,
        }
    }

    /// The three banners are composed from the same constants the writers use,
    /// so this pins the classification against the real strings.
    #[test]
    fn classifies_every_banner_shape() {
        let restart = format!("{COMPACTION_MARKER_PREFIX} — The conversation was compacted.");
        let delta = format!("{COMPACTION_MARKER_PREFIX} — {SEGMENT_SENTINEL} only the tail");
        let suspended = format!("{SUSPENDED_MARKER_PREFIX} — Nothing was summarised.");

        assert_eq!(classify_user_marker(&restart), Some(MarkerKind::Restart));
        assert_eq!(classify_user_marker(&delta), Some(MarkerKind::DeltaSegment));
        assert_eq!(
            classify_user_marker(&suspended),
            Some(MarkerKind::Suspended)
        );

        // A body that merely QUOTES the sentinel is still a restart (#1928).
        let quoting = format!("{COMPACTION_MARKER_PREFIX} — recap: \"{SEGMENT_SENTINEL}\"");
        assert_eq!(classify_user_marker(&quoting), Some(MarkerKind::Restart));

        // Not a marker at all.
        assert_eq!(classify_user_marker("hello"), None);
        assert_eq!(classify_user_marker(&format!(" see {restart}")), None);
    }

    #[test]
    fn only_restart_rows_are_reload_boundaries() {
        let restart = format!("{COMPACTION_MARKER_PREFIX} — summary");
        let delta = format!("{COMPACTION_MARKER_PREFIX} — {SEGMENT_SENTINEL} segment");
        let suspended = format!("{SUSPENDED_MARKER_PREFIX} — guard");

        assert!(is_reload_boundary(&row("user", &restart)));
        assert!(!is_reload_boundary(&row("user", &delta)));
        assert!(!is_reload_boundary(&row("user", &suspended)));
        // An assistant echo is not a marker however exactly it quotes one (#175).
        assert!(!is_reload_boundary(&row("assistant", &restart)));
    }

    #[test]
    fn boundary_is_the_newest_restart_skipping_segments() {
        let restart = format!("{COMPACTION_MARKER_PREFIX} — first");
        let delta = format!("{COMPACTION_MARKER_PREFIX} — {SEGMENT_SENTINEL} segment");
        let rows = vec![
            row("user", "history"),
            row("user", &restart),
            row("user", &delta),
            row("assistant", "after segment"),
        ];
        assert_eq!(boundary_start_index(&rows), Some(1));

        // No restart anywhere: the whole history is the window.
        let no_restart = vec![row("user", "history"), row("user", &delta)];
        assert_eq!(boundary_start_index(&no_restart), None);
    }
}
