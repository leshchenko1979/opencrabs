//! Agent Context Management
//!
//! Manages conversation context including messages, system brain,
//! and token tracking.

use crate::brain::provider::{ContentBlock, Message, Role};
use crate::brain::tokenizer;
use crate::db::models::Message as DbMessage;
use std::path::PathBuf;
use uuid::Uuid;

/// Agent context for a conversation
#[derive(Debug, Clone)]
pub struct AgentContext {
    /// Session ID
    pub session_id: Uuid,

    /// System brain
    pub system_brain: Option<String>,

    /// Conversation messages
    pub messages: Vec<Message>,

    /// Tracked files in the conversation
    pub tracked_files: Vec<TrackedFile>,

    /// Current token count estimate
    pub token_count: usize,

    /// Maximum context tokens
    pub max_tokens: usize,

    /// The provider's own count of the last request it received, when it
    /// reported one.
    ///
    /// `token_count` is a tiktoken estimate of the system prompt plus the
    /// messages. It leaves out the tool schemas the provider also receives,
    /// and it disagrees with the provider's tokenizer on code and JSON: a
    /// 2.4MB Claude CLI request we estimated at ~660k came back counted as
    /// ~1.03M, over a 1M limit we believed we were at 66% of. Compaction
    /// measured against the estimate fired eight times without ever getting
    /// under the ceiling, because the ceiling was never where we thought.
    ///
    /// Once the provider tells us a real number we anchor on it and track our
    /// own estimate's movement since, which keeps the budget honest between
    /// calls without pretending we can tokenize the way they do. Stored as
    /// (their count, our estimate at that moment) so the delta works in both
    /// directions: trimming and compaction lower it, appending raises it.
    pub provider_anchor: Option<(usize, usize)>,
}

/// A file tracked in the conversation
#[derive(Debug, Clone)]
pub struct TrackedFile {
    pub id: Uuid,
    pub path: PathBuf,
    pub content: Option<String>,
    pub token_count: usize,
}

impl AgentContext {
    /// Create a new agent context for a session
    pub fn new(session_id: Uuid, max_tokens: usize) -> Self {
        Self {
            session_id,
            system_brain: None,
            messages: Vec::new(),
            tracked_files: Vec::new(),
            token_count: 0,
            max_tokens,
            provider_anchor: None,
        }
    }

    /// Set the system brain
    pub fn with_system_brain(mut self, prompt: String) -> Self {
        self.token_count += Self::estimate_tokens(&prompt);
        self.system_brain = Some(prompt);
        self
    }

    /// Add a message to the context
    pub fn add_message(&mut self, message: Message) {
        // Estimate tokens for the message
        let tokens = self.estimate_message_tokens(&message);
        self.token_count += tokens;
        self.messages.push(message);
    }

    /// Convert database messages to LLM messages
    pub fn from_db_messages(
        session_id: Uuid,
        db_messages: Vec<DbMessage>,
        max_tokens: usize,
    ) -> Self {
        let mut context = Self::new(session_id, max_tokens);

        for db_msg in db_messages {
            // Skip messages with empty content AND no captured reasoning —
            // Anthropic rejects empty text blocks. A non-empty thinking
            // column alone still justifies keeping the row so downstream
            // providers (e.g. Moonshot kimi) see the reasoning context.
            let has_content = !db_msg.content.is_empty();
            let has_thinking = db_msg
                .thinking
                .as_deref()
                .is_some_and(|t| !t.trim().is_empty());
            if !has_content && !has_thinking {
                continue;
            }

            let role = match db_msg.role.as_str() {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                "system" => Role::System,
                _ => Role::User, // Default fallback
            };

            // Rehydrate reasoning as a leading ContentBlock::Thinking so
            // the OpenAI-compatible encoder can emit it as
            // `reasoning_content` on assistant tool_call messages.
            // Without this, Moonshot kimi 400s on any resumed turn because
            // the required `reasoning_content` field is missing.
            let mut content: Vec<ContentBlock> = Vec::new();
            if role == Role::Assistant
                && has_thinking
                && let Some(thinking) = db_msg.thinking.as_deref()
            {
                content.push(ContentBlock::Thinking {
                    thinking: thinking.to_string(),
                    signature: None,
                });
            }
            if has_content {
                content.push(ContentBlock::Text {
                    text: db_msg.content,
                });
            }

            let message = Message { role, content };

            context.add_message(message);
        }

        context
    }

    /// Track a file in the conversation
    pub fn track_file(&mut self, file: TrackedFile) {
        self.token_count += file.token_count;
        self.tracked_files.push(file);
    }

    /// Check if context would exceed limit with additional tokens
    pub fn would_exceed_limit(&self, additional_tokens: usize) -> bool {
        self.token_count + additional_tokens > self.max_tokens
    }

    /// Estimate tokens for a message
    pub(crate) fn estimate_message_tokens(&self, message: &Message) -> usize {
        let mut tokens = 0;

        for content in &message.content {
            match content {
                ContentBlock::Text { text } => {
                    tokens += Self::estimate_tokens(text);
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    tokens += Self::estimate_tokens(name);
                    tokens += Self::estimate_tokens(&input.to_string());
                }
                ContentBlock::ToolResult { content, .. } => {
                    tokens += Self::estimate_tokens(content);
                }
                ContentBlock::Image { .. } => {
                    // Images use a fixed token count (approximate)
                    tokens += 1000;
                }
                ContentBlock::Thinking { thinking, .. } => {
                    tokens += Self::estimate_tokens(thinking);
                }
            }
        }

        // Add overhead for message structure
        tokens + 4
    }

    /// Token estimation using tiktoken cl100k_base BPE encoding.
    /// No more chars/N guessing — this gives real token counts.
    pub fn estimate_tokens(text: &str) -> usize {
        tokenizer::count_tokens(text)
    }

    /// Static version of estimate_message_tokens — usable without a &self reference.
    pub fn estimate_tokens_static(message: &Message) -> usize {
        let mut tokens = 0;
        for content in &message.content {
            match content {
                ContentBlock::Text { text } => {
                    tokens += Self::estimate_tokens(text);
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    tokens += Self::estimate_tokens(name);
                    tokens += Self::estimate_tokens(&input.to_string());
                }
                ContentBlock::ToolResult { content, .. } => {
                    tokens += Self::estimate_tokens(content);
                }
                ContentBlock::Image { .. } => {
                    tokens += 1000;
                }
                ContentBlock::Thinking { thinking, .. } => {
                    tokens += Self::estimate_tokens(thinking);
                }
            }
        }
        tokens + 4
    }

    /// The size of the next request, preferring the provider's count.
    ///
    /// Falls back to the local estimate until a provider has reported one, so
    /// a first turn behaves exactly as before.
    pub fn effective_token_count(&self) -> usize {
        match self.provider_anchor {
            // Their count, moved by however much our own estimate has shifted
            // since we took it. Every path that trims or appends already
            // maintains `token_count`, so none of them need to know about the
            // anchor for the budget to follow along.
            Some((reported, estimated_at_anchor)) => reported
                .saturating_add(self.token_count)
                .saturating_sub(estimated_at_anchor),
            None => self.token_count,
        }
    }

    /// Window fill for budget gates: provider anchor when set (schemas already
    /// in the provider count), else message estimate plus static request overhead.
    pub fn budget_token_count(&self, static_overhead: usize) -> usize {
        match self.provider_anchor {
            Some(_) => self.effective_token_count(),
            None => self.token_count.saturating_add(static_overhead),
        }
    }

    /// Drop a provider anchor whose reported-at-anchor gap exceeds `max_tokens *
    /// fraction` — typically bad usage receipts that double-count cache creation.
    ///
    /// Returns whether an anchor was cleared. After clearing, emergency
    /// truncation re-evaluates on the local estimate only.
    pub fn discard_inflated_provider_anchor(&mut self, max_tokens: usize, fraction: f64) -> bool {
        let Some((reported, estimated_at_anchor)) = self.provider_anchor else {
            return false;
        };
        let gap = reported.saturating_sub(estimated_at_anchor);
        let threshold = (max_tokens as f64 * fraction) as usize;
        if gap <= threshold {
            return false;
        }
        tracing::warn!(
            reported_at_anchor = reported,
            estimated_at_anchor,
            anchor_gap = gap,
            gap_threshold = threshold,
            max_tokens,
            "Dropping inflated provider budget anchor — re-evaluating context fill on local estimate"
        );
        self.provider_anchor = None;
        true
    }

    /// Anchor the budget on a size the provider actually reported.
    ///
    /// Callers must reject implausible reports first: an endpoint that adds a
    /// flat overhead to every call would otherwise drag the budget up and
    /// compact a context that never needed it.
    pub fn record_provider_reported_tokens(&mut self, reported: usize) {
        self.provider_anchor = Some((reported, self.token_count));
    }

    /// Get the current token usage percentage
    pub fn usage_percentage(&self) -> f64 {
        (self.effective_token_count() as f64 / self.max_tokens as f64) * 100.0
    }

    /// Returns true if a message consists entirely of ToolResult blocks.
    /// Such a message is "orphaned" if the preceding assistant(ToolUse) message
    /// was removed, and will cause the API to reject the conversation.
    pub(crate) fn is_orphaned_tool_result_msg(msg: &Message) -> bool {
        msg.role == Role::User
            && !msg.content.is_empty()
            && msg
                .content
                .iter()
                .all(|b| matches!(b, ContentBlock::ToolResult { .. }))
    }

    /// Remove any leading user messages that consist solely of ToolResult blocks.
    /// Called after trimming to prevent orphaned tool results at the start of history.
    fn drop_leading_orphan_tool_results(&mut self) {
        while self
            .messages
            .first()
            .is_some_and(Self::is_orphaned_tool_result_msg)
        {
            let tokens = self.estimate_message_tokens(&self.messages[0]);
            self.token_count = self.token_count.saturating_sub(tokens);
            self.messages.remove(0);
        }
    }

    /// Trim old messages if context is too large
    pub fn trim_to_fit(&mut self, required_space: usize) {
        while self.would_exceed_limit(required_space) && !self.messages.is_empty() {
            // Remove the oldest user/assistant message pair
            if let Some(first_msg) = self.messages.first() {
                let tokens = self.estimate_message_tokens(first_msg);
                self.token_count = self.token_count.saturating_sub(tokens);
                self.messages.remove(0);
            }
        }
        // Removing an assistant(tool_use) exposes an orphaned user(tool_result) — drop it
        self.drop_leading_orphan_tool_results();
    }

    /// Hard-truncate old messages until token count is at or below `target_tokens`.
    /// Keeps at least 2 messages (the most recent pair) to maintain conversation validity.
    pub fn hard_truncate_to(&mut self, target_tokens: usize) {
        while self.token_count > target_tokens && self.messages.len() > 2 {
            let tokens = self.estimate_message_tokens(&self.messages[0]);
            self.token_count = self.token_count.saturating_sub(tokens);
            self.messages.remove(0);
        }
        self.drop_leading_orphan_tool_results();
    }

    /// #1649 delta compaction: the messages a delta summariser may see.
    ///
    /// Everything AFTER the last compaction marker. The frozen segments
    /// (markers) never re-enter the summariser input — that re-derivation
    /// was the cumulative-growth mechanism: every compaction re-billed the
    /// previous summary into the new one (x8.6 over 5 compactions, 97.8% of
    /// byte growth cumulative).
    pub(crate) fn delta_since_last_marker(&self) -> Vec<Message> {
        match self.last_marker_index() {
            Some(idx) => self.messages[idx + 1..].to_vec(),
            None => self.messages.clone(),
        }
    }

    /// #833: the prior for what a continuation document costs, used until a
    /// session has written one and reported its own size.
    ///
    /// Measured median across 257 consecutive delta compactions (Nov 13–15):
    /// ~1,800 tokens each. A prior, never a floor — the observed size replaces
    /// it after the first compaction.
    pub(crate) const DEFAULT_SUMMARY_TOKENS: usize = 2_000;

    /// Where the floor a delta compaction lands on stops being headroom.
    ///
    /// A delta compaction leaves the frozen segments in place, so the level it
    /// lands on is `segments + document`. Once that sits above this share of
    /// the window, the scope cannot buy real room: the tool loop refills the
    /// gap in minutes, the gate fires again, and every pass adds one more
    /// document to the frozen block. Consolidation is then the only scope that
    /// moves the floor, because it replaces the segments themselves.
    ///
    /// This is the old `segments > max/2` rule restated in terms of what the
    /// compaction LANDS ON rather than what it summarises — the difference is
    /// the document, and it is what the old rule was reaching for.
    const CONSOLIDATION_FLOOR_RATIO: f64 = 0.5;

    /// A delta compaction replaces `delta` tokens with a document of roughly
    /// `expected` tokens, so it pays only while the delta is clearly bigger
    /// than the document replacing it. Below this ratio it is net ADDITIVE —
    /// the measured +1,850-token median growth that turned one compaction into
    /// twenty, none of which lowered the floor.
    const DELTA_MIN_PAYOFF_RATIO: f64 = 2.0;

    /// Hard cap on frozen segment *count* before consolidation (#1011).
    pub(crate) const CONSOLIDATION_MAX_FROZEN_SEGMENTS: usize = 5;

    /// Consolidate when the frozen block alone exceeds this share of the window (#1011),
    /// below the 50% floor-after-delta rule.
    const CONSOLIDATION_SEGMENTS_SHARE_RATIO: f64 = 0.35;

    /// Incremental consolidation merges at most this many oldest segments (#1012).
    pub(crate) const INCREMENTAL_CONSOLIDATION_PREFIX: usize = 3;

    /// Tokens held by the delta: everything after the last marker.
    ///
    /// The exact quantity a delta compaction can remove, as opposed to the
    /// tokens it costs to describe them.
    pub(crate) fn delta_token_count(&self) -> usize {
        self.delta_since_last_marker()
            .iter()
            .map(Self::estimate_tokens_static)
            .sum()
    }

    /// #1649/#833: decide what a new compaction summarises.
    ///
    /// - No marker yet → full window (byte-identical to the classic path).
    /// - Escalated → consolidate: the previous compaction did not lower the
    ///   fill, so repeating its scope would repeat its result.
    /// - The floor a delta compaction would land on is above the headroom
    ///   target → consolidate; a delta pass would leave that floor where it is.
    /// - The delta is too small to pay for the document replacing it →
    ///   consolidate.
    /// - Otherwise → delta since the last marker.
    ///
    /// The scope is chosen by WHAT IT CAN REMOVE, never by how much text it
    /// summarises: a delta compaction that removes 800 tokens and writes an
    /// 1,800-token document is a net loss however cheap the call was.
    pub(crate) fn compaction_scope_with(
        &self,
        expected_summary_tokens: usize,
        escalate: bool,
    ) -> CompactionScope {
        if self.last_marker_index().is_none() {
            return CompactionScope::FullWindow;
        }
        let expected = expected_summary_tokens.max(1);
        let segments = self.segments_token_count();
        let delta = self.delta_token_count();
        let floor_after_delta = segments.saturating_add(expected);
        let headroom_target = (self.max_tokens as f64 * Self::CONSOLIDATION_FLOOR_RATIO) as usize;

        let segment_count = self.frozen_segment_count();
        let segments_share_target =
            (self.max_tokens as f64 * Self::CONSOLIDATION_SEGMENTS_SHARE_RATIO) as usize;

        let (scope, reason) = if escalate {
            (
                CompactionScope::SegmentConsolidation,
                "escalated: the previous compaction did not lower the fill",
            )
        } else if segment_count > Self::CONSOLIDATION_MAX_FROZEN_SEGMENTS {
            (
                CompactionScope::SegmentConsolidation,
                "frozen segment count exceeds the early-consolidation cap",
            )
        } else if segments > segments_share_target {
            (
                CompactionScope::SegmentConsolidation,
                "frozen segments exceed the early segment-share threshold",
            )
        } else if floor_after_delta > headroom_target {
            (
                CompactionScope::SegmentConsolidation,
                "the frozen segments alone land above the headroom target",
            )
        } else if (delta as f64) < expected as f64 * Self::DELTA_MIN_PAYOFF_RATIO {
            (
                CompactionScope::SegmentConsolidation,
                "the delta is too small to pay for the document replacing it",
            )
        } else {
            (
                CompactionScope::DeltaSinceMarker,
                "the delta pays for its own document and leaves headroom",
            )
        };

        // #833: the decision was invisible for three days and 396 compactions,
        // because the only trace of a compaction was the level it left behind.
        // Log the inputs, not just the verdict: the next diagnosis reads why.
        tracing::info!(
            "Compaction scope: {scope:?} ({reason}) — window {} of {} tokens, \
             segments {segments}, delta {delta}, expected document {expected}, \
             floor after delta {floor_after_delta} (target {headroom_target})",
            self.token_count,
            self.max_tokens,
        );
        scope
    }

    /// The default-policy scope decision: the measured prior for a document
    /// size, no escalation. Callers that track a session's history use
    /// [`Self::compaction_scope_with`] with its own numbers.
    ///
    /// `cfg(test)`: the production path ALWAYS supplies a session's own history
    /// (`spawn_background_compaction` passes the size of the last document it
    /// wrote and whether that compaction reduced anything), so this form exists
    /// only for the probes that pin the default policy.
    #[cfg(test)]
    pub(crate) fn compaction_scope(&self) -> CompactionScope {
        self.compaction_scope_with(Self::DEFAULT_SUMMARY_TOKENS, false)
    }

    /// Tokens held by the frozen segment block: everything through the last
    /// marker, brain excluded.
    pub(crate) fn segments_token_count(&self) -> usize {
        match self.last_marker_index() {
            Some(idx) => self.messages[..=idx]
                .iter()
                .map(|m| self.estimate_message_tokens(m))
                .sum(),
            None => 0,
        }
    }

    /// Number of in-memory compaction markers (#1011 / #1016).
    pub(crate) fn frozen_segment_count(&self) -> usize {
        self.messages
            .iter()
            .filter(|m| Self::is_compaction_marker_msg(m))
            .count()
    }

    /// Indices of every frozen segment marker, in order (#1012).
    pub(crate) fn marker_indices(&self) -> Vec<usize> {
        self.messages
            .iter()
            .enumerate()
            .filter_map(|(i, m)| Self::is_compaction_marker_msg(m).then_some(i))
            .collect()
    }

    /// How many oldest segments an incremental consolidation merges (#1012).
    pub(crate) fn consolidation_prefix_count(segment_count: usize) -> usize {
        if segment_count <= Self::INCREMENTAL_CONSOLIDATION_PREFIX {
            segment_count
        } else {
            Self::INCREMENTAL_CONSOLIDATION_PREFIX
        }
    }

    /// `Some(n)` when only the oldest `n` segments are summarised (#1012).
    pub(crate) fn incremental_consolidation_prefix(&self) -> Option<usize> {
        let n = self.frozen_segment_count();
        if n == 0 {
            return None;
        }
        let prefix = Self::consolidation_prefix_count(n);
        if prefix < n { Some(prefix) } else { None }
    }

    /// Index of the first in-memory compaction marker, if any.
    pub(crate) fn first_marker_index(&self) -> Option<usize> {
        self.messages
            .iter()
            .position(Self::is_compaction_marker_msg)
    }

    /// Index of the last in-memory compaction marker, if any.
    pub(crate) fn last_marker_index(&self) -> Option<usize> {
        self.messages
            .iter()
            .rposition(Self::is_compaction_marker_msg)
    }

    /// An in-memory marker: a user message whose FIRST text block is a
    /// compaction marker — the in-memory twin of the #175 anchored-prefix rule
    /// the DB loader applies.
    ///
    /// This answers "is this a marker at all", which is what the delta scope and
    /// the segment accounting need: all three kinds anchor them. Whether a
    /// RELOAD may start here is a different question, answered by
    /// [`boundary_start_index`](crate::brain::agent::compaction_boundary::boundary_start_index).
    /// Both read the same classifier (#987), so the two paths cannot disagree
    /// about which rows are markers.
    pub(crate) fn is_compaction_marker_msg(m: &Message) -> bool {
        m.role == Role::User
            && matches!(m.content.first(), Some(ContentBlock::Text { text })
                if crate::brain::agent::compaction_boundary::classify_user_marker(text).is_some())
    }

    /// #1649: apply a DELTA summary — append a new frozen segment.
    ///
    /// Previous segments survive verbatim; everything after the last marker
    /// (the delta the summary describes) is replaced by the new marker. No
    /// prior summary text is re-derived or re-billed into the result.
    pub(crate) fn compact_with_delta_summary(&mut self, summary: String) -> String {
        let banner = delta_segment_marker_banner();
        self.compact_with_delta_summary_banner(summary, &banner)
    }

    /// [`Self::compact_with_delta_summary`] with a caller-chosen banner (#994).
    ///
    /// The banner is the segment's opening claim, and the classic one says the
    /// block SUMMARISES the delta. A caller welding something else into the
    /// segment — the truncated-summary notice — has to say so, or the marker
    /// lies in its first line. A custom banner MUST still carry
    /// [`SEGMENT_SENTINEL`] when the prior segments stay in force: the loader
    /// reads that sentinel to decide whether this row extends the window or
    /// restarts it (`compaction_boundary::classify_user_marker`).
    pub(crate) fn compact_with_delta_summary_banner(
        &mut self,
        summary: String,
        banner: &str,
    ) -> String {
        let Some(idx) = self.last_marker_index() else {
            // No prior marker: this is the session's first compaction —
            // identical to the classic full-window swap, verbatim tail
            // included (#943).
            let tail = self.verbatim_tail_tokens();
            return self.compact_with_verbatim_tail_banner(summary, tail, banner);
        };
        // Keep the frozen segments; drop the delta the summary covers.
        self.messages.truncate(idx + 1);
        let marker_text = marker_text(banner, &summary);
        let summary_msg = Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: marker_text.clone(),
            }],
        };
        self.messages.push(summary_msg);
        self.recount_tokens_after_compaction();
        self.provider_anchor = None;
        marker_text
    }

    /// #1649: replace every frozen segment with ONE consolidated summary.
    ///
    /// The only sanctioned re-derivation: the inputs are summaries already
    /// (bounded by the summariser's output budget), never raw history, and
    /// the result is a single marker again. The consolidated marker carries
    /// no segment sentinel, so the DB loader treats it as a boundary and
    /// drops every superseded marker row on reload.
    pub(crate) fn consolidate_segments(&mut self, summary: String) -> String {
        self.consolidate_segments_banner(summary, CONSOLIDATED_MARKER_BANNER)
    }

    /// [`Self::consolidate_segments`] with a caller-chosen banner (#994).
    /// Replace only the oldest `prefix_count` frozen segments with one marker (#1012).
    pub(crate) fn consolidate_oldest_segments_banner(
        &mut self,
        prefix_count: usize,
        summary: String,
        banner: &str,
    ) -> String {
        let indices = self.marker_indices();
        if prefix_count == 0 || prefix_count >= indices.len() {
            return self.consolidate_segments_banner(summary, banner);
        }
        let first = indices[0];
        let last_prefix = indices[prefix_count - 1];
        let tail: Vec<Message> = self.messages.split_off(last_prefix + 1);
        self.messages.truncate(first);
        let marker_text = marker_text(banner, &summary);
        let summary_msg = Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: marker_text.clone(),
            }],
        };
        self.messages.push(summary_msg);
        self.messages.extend(tail);
        self.recount_tokens_after_compaction();
        self.provider_anchor = None;
        marker_text
    }

    pub(crate) fn consolidate_oldest_segments(
        &mut self,
        prefix_count: usize,
        summary: String,
    ) -> String {
        self.consolidate_oldest_segments_banner(prefix_count, summary, CONSOLIDATED_MARKER_BANNER)
    }

    pub(crate) fn consolidate_segments_banner(&mut self, summary: String, banner: &str) -> String {
        let Some(first) = self.first_marker_index() else {
            // Nothing to consolidate — behave like the classic swap, verbatim
            // tail included (#943).
            let tail = self.verbatim_tail_tokens();
            return self.compact_with_verbatim_tail_banner(summary, tail, banner);
        };
        let last = self.last_marker_index().unwrap_or(first);
        let tail: Vec<Message> = self.messages.split_off(last + 1);
        self.messages.truncate(first);
        let marker_text = marker_text(banner, &summary);
        let summary_msg = Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: marker_text.clone(),
            }],
        };
        self.messages.push(summary_msg);
        self.messages.extend(tail);
        self.recount_tokens_after_compaction();
        self.provider_anchor = None;
        marker_text
    }

    /// #994: append a marker as a frozen segment, dropping NOTHING.
    ///
    /// The consolidation arm's twin of `compact_with_delta_summary_banner`: a
    /// segment marker EXTENDS the window its restart opened, so appending one
    /// records a note without touching the history above it. Used when the
    /// summariser was cut on a consolidation pass — replacing the segments with
    /// a no-summary notice would delete every accumulated summary, which is a
    /// worse outcome than a consolidation that did not happen.
    pub(crate) fn append_segment_marker(&mut self, marker: &str) -> String {
        self.messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: marker.to_string(),
            }],
        });
        self.recount_tokens_after_compaction();
        self.provider_anchor = None;
        marker.to_string()
    }

    /// Text of every frozen segment marker, in order (consolidation input).
    pub(crate) fn segment_marker_texts(&self) -> Vec<String> {
        self.messages
            .iter()
            .filter(|m| Self::is_compaction_marker_msg(m))
            .filter_map(|m| match m.content.first() {
                Some(ContentBlock::Text { text }) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// #1649: assemble the summariser input for this context's scope.
    ///
    /// One helper for BOTH the synchronous and background paths — parity by
    /// construction, not by duplication.
    ///
    /// `cfg(test)`: production goes through [`Self::compaction_input_with`]
    /// with the session's own document-size prior; this is the default-policy
    /// form the probes exercise.
    #[cfg(test)]
    pub(crate) fn compaction_input(&self) -> (CompactionScope, Vec<Message>, usize) {
        self.compaction_input_with(Self::DEFAULT_SUMMARY_TOKENS, false)
    }

    /// The same helper with the caller's own history: the size of the last
    /// document this session wrote, and whether its last compaction actually
    /// lowered the fill (#833).
    pub(crate) fn compaction_input_with(
        &self,
        expected_summary_tokens: usize,
        escalate: bool,
    ) -> (CompactionScope, Vec<Message>, usize) {
        let scope = self.compaction_scope_with(expected_summary_tokens, escalate);
        let prefix = if scope == CompactionScope::SegmentConsolidation {
            self.incremental_consolidation_prefix()
        } else {
            None
        };
        self.compaction_input_for_scope(scope, prefix)
    }

    /// The input for a scope already decided.
    ///
    /// Split from the decision so the SHAPE of each scope is testable without
    /// the policy that picks it — a policy change must not silently rewrite
    /// what a delta summary is allowed to see.
    pub(crate) fn compaction_input_for_scope(
        &self,
        scope: CompactionScope,
        consolidation_prefix: Option<usize>,
    ) -> (CompactionScope, Vec<Message>, usize) {
        match scope {
            CompactionScope::FullWindow => (scope, self.messages.clone(), self.token_count),
            CompactionScope::DeltaSinceMarker => {
                let delta = self.delta_since_last_marker();
                let tokens = delta.iter().map(AgentContext::estimate_tokens_static).sum();
                (scope, delta, tokens)
            }
            CompactionScope::SegmentConsolidation => {
                // The frozen segments merge into one message, and the live
                // tail after the last marker rides along verbatim. Dropping
                // the tail left the summariser blind to the newest owner
                // instructions (C12: a correction at seq 1740 sat after the
                // last marker and never reached the consolidation input).
                let all_markers = self.segment_marker_texts();
                let texts: Vec<&str> = if let Some(prefix) = consolidation_prefix {
                    all_markers
                        .iter()
                        .take(prefix)
                        .map(String::as_str)
                        .collect()
                } else {
                    all_markers.iter().map(String::as_str).collect()
                };
                let joined = texts.join("\n\n---\n\n");
                let mut messages = vec![Message::user(joined)];
                messages.extend(self.delta_since_last_marker());
                let tokens = messages
                    .iter()
                    .map(AgentContext::estimate_tokens_static)
                    .sum();
                (scope, messages, tokens)
            }
        }
    }

    /// Recalculate `token_count` after a compaction swap (brain + messages).
    fn recount_tokens_after_compaction(&mut self) {
        self.token_count = 0;
        if let Some(brain) = &self.system_brain {
            self.token_count += Self::estimate_tokens(brain);
        }
        for msg in &self.messages {
            self.token_count += self.estimate_message_tokens(msg);
        }
    }

    /// Compact the context by replacing old messages with a summary.
    ///
    /// Keeps the most recent messages that fit within the token budget
    /// and prepends a summary of everything that was trimmed.
    /// `keep_token_budget` is the ceiling for the marker PLUS the kept
    /// messages: `summary_tokens` is subtracted from it before the reverse
    /// walk, so a caller that wants N tokens of raw history must pass
    /// `N + summary_tokens`. Use [`Self::compact_with_verbatim_tail`] rather
    /// than doing that arithmetic by hand (#943) — passing N alone leaves
    /// nothing whenever the summary is larger than N, which it usually is.
    /// Returns the exact marker text it pushed into the context (banner +
    /// summary). A caller that persists a compaction row MUST write THIS
    /// string rather than rebuild a banner of its own: two construction sites
    /// drifting apart is #1928 — the in-memory apply wrote a delta segment's
    /// sentinel banner that the DB row never carried, so the loader counted
    /// every segment as a fresh boundary and kept only the newest.
    pub fn compact_with_summary(&mut self, summary: String, keep_token_budget: usize) -> String {
        self.compact_with_banner(summary, keep_token_budget, FULL_WINDOW_MARKER_BANNER)
    }

    /// [`Self::compact_with_summary`] with a caller-chosen banner (#994).
    ///
    /// The banner is the marker's opening claim — the classic one promises a
    /// structured summary below it. A caller welding something else into the
    /// window (the truncated-summary notice) has to be able to say what
    /// actually happened, or the marker's first line is a lie.
    pub(crate) fn compact_with_banner(
        &mut self,
        summary: String,
        keep_token_budget: usize,
        banner: &str,
    ) -> String {
        // Walk backwards from end, keeping messages until we hit the budget
        let summary_tokens = Self::estimate_tokens(&summary) + 50; // +50 for the marker text
        let available = keep_token_budget.saturating_sub(summary_tokens);
        let mut running = 0usize;
        let mut keep_count = 0usize;
        for msg in self.messages.iter().rev() {
            let t = self.estimate_message_tokens(msg);
            if running + t > available {
                break;
            }
            running += t;
            keep_count += 1;
        }
        // Caller can request a clean compaction (only the summary survives)
        // by passing `keep_token_budget == 0` — in that case we honour zero
        // kept messages. Otherwise keep at least the most recent pair so
        // valid API request structure is preserved.
        if keep_token_budget > 0 {
            keep_count = keep_count.max(2.min(self.messages.len()));
        }
        let mut keep_start = self.messages.len().saturating_sub(keep_count);

        // Advance past any leading orphaned tool_result messages in the kept slice.
        // If the assistant(tool_use) that precedes them is being dropped, they'd be invalid.
        while keep_start < self.messages.len()
            && Self::is_orphaned_tool_result_msg(&self.messages[keep_start])
        {
            keep_start += 1;
        }

        let kept_messages: Vec<Message> = self.messages.drain(keep_start..).collect();

        // Clear all old messages
        self.messages.clear();

        // Prepend the compaction summary as a user message (so the LLM sees the context)
        let marker_text = marker_text(banner, &summary);
        let summary_msg = Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: marker_text.clone(),
            }],
        };
        self.messages.push(summary_msg);

        // Re-add kept messages
        self.messages.extend(kept_messages);

        // Recalculate token count
        self.token_count = 0;
        if let Some(brain) = &self.system_brain {
            self.token_count += Self::estimate_tokens(brain);
        }
        for msg in &self.messages {
            self.token_count += self.estimate_message_tokens(msg);
        }

        // Drop the pre-compaction provider anchor: the anchor was taken against
        // the uncompacted prompt and would inflate the post-compaction budget
        // with a stale delta (#211).
        self.provider_anchor = None;
        marker_text
    }

    /// #943: the verbatim tail budget for this context's window.
    ///
    /// The single place the tail arithmetic is resolved, so the full-window
    /// arm, the delta arm and the two "no prior marker" fallbacks cannot drift
    /// apart — they all used to pass `0` and keep nothing.
    pub(crate) fn verbatim_tail_tokens(&self) -> usize {
        crate::brain::agent::service::request_budget::compaction_verbatim_tail_tokens(
            self.max_tokens,
        )
    }

    /// #943: compact while keeping the most recent exchange(s) VERBATIM.
    ///
    /// [`Self::compact_with_summary`] charges its budget against the summary as
    /// well as the kept messages (`available = budget - summary_tokens`), so a
    /// caller that wants `tail_tokens` of raw history has to pass the summary's
    /// own size ON TOP of it. Passing the tail budget alone silently collapses
    /// the tail to zero whenever the summary is larger than it — which it
    /// always is, because the summary budget is 3 000 tokens. That arithmetic
    /// lives here, once, so no caller has to rediscover it.
    ///
    /// The `+ 50` mirrors the marker allowance inside
    /// `compact_with_summary`.
    pub(crate) fn compact_with_verbatim_tail(
        &mut self,
        summary: String,
        tail_tokens: usize,
    ) -> String {
        self.compact_with_verbatim_tail_banner(summary, tail_tokens, FULL_WINDOW_MARKER_BANNER)
    }

    /// [`Self::compact_with_verbatim_tail`] with a caller-chosen banner (#994).
    pub(crate) fn compact_with_verbatim_tail_banner(
        &mut self,
        summary: String,
        tail_tokens: usize,
        banner: &str,
    ) -> String {
        let summary_tokens = Self::estimate_tokens(&summary) + 50;
        self.compact_with_banner(summary, tail_tokens.saturating_add(summary_tokens), banner)
    }
}

/// The canonical compaction-marker prefix. Every marker row — DB or
/// in-memory — starts with this (the #175 anchored-prefix invariant).
pub(crate) const COMPACTION_MARKER_PREFIX: &str = "[CONTEXT COMPACTION";

/// Assemble a marker row: the banner, then the body (#994).
///
/// An empty body means the marker carries NO document — the truncated-summary
/// notice (#994) is a banner and nothing else, and a trailing blank paragraph
/// would be bytes the window pays for on every turn with nothing in them.
pub(crate) fn marker_text(banner: &str, body: &str) -> String {
    if body.trim().is_empty() {
        banner.to_string()
    } else {
        format!("{banner}\n\n{body}")
    }
}

/// The classic full-window marker's opening claim (#1928, #994).
///
/// Extracted from `compact_with_summary` so a caller welding something other
/// than a summary into the window can supply a banner that is TRUE for what it
/// welds — the truncated-summary notice (#994). Byte-identical to the string
/// the function used to inline.
pub(crate) const FULL_WINDOW_MARKER_BANNER: &str = "[CONTEXT COMPACTION — The conversation was automatically compacted. \
     Below is a structured summary of everything before this point.]";

/// The consolidation marker's opening claim (#1649).
pub(crate) const CONSOLIDATED_MARKER_BANNER: &str = "[CONTEXT COMPACTION — The session's accumulated compaction summaries \
     were consolidated into this single self-contained summary. It \
     replaces every earlier one; nothing before this point survives \
     except this marker.]";

/// The delta-segment marker's opening claim (#1649).
///
/// A function, not a const, because the banner embeds [`SEGMENT_SENTINEL`] —
/// the loader's signal that this row EXTENDS the window rather than restarting
/// it — and a const cannot interpolate one.
pub(crate) fn delta_segment_marker_banner() -> String {
    format!(
        "[CONTEXT COMPACTION — {SEGMENT_SENTINEL} This block summarises only the \
         messages since the previous compaction marker. The earlier frozen \
         segments above remain in force unchanged; do not re-derive or merge \
         them.]"
    )
}

/// Rides in a DELTA-SEGMENT marker's banner (#1649). Segments extend the
/// window instead of restarting it, so the DB loader skips them when it
/// looks for the reload boundary: it anchors on the LAST marker WITHOUT
/// this sentinel — the full-window, hard-truncate, consolidation, RSI and
/// cron markers all restart history, and only they bound the reload.
/// Inverted on purpose: tagging the one new marker type keeps every legacy
/// marker (and every legacy DB stream) behaving exactly as before.
pub(crate) const SEGMENT_SENTINEL: &str = "DELTA SEGMENT.";

/// What a compaction summarises (#1649).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CompactionScope {
    /// No marker in the window yet — summarise everything (the classic
    /// full-window behaviour, byte-identical prompt and apply).
    FullWindow,
    /// Prior markers exist — summarise ONLY the messages since the last one
    /// and append the result as a new frozen segment.
    DeltaSinceMarker,
    /// The frozen segments alone exceed half the window — consolidate the
    /// segments into one fresh summary that supersedes them.
    SegmentConsolidation,
}

impl CompactionScope {
    /// The scope's name as a channel renders it (#833 follow-up).
    ///
    /// The label lives HERE, beside the enum, because the event that carries it
    /// crosses into the public `ProgressEvent` surface and this enum is
    /// `pub(crate)` — a `pub` field cannot name a `pub(crate)` type without
    /// tripping `private_interfaces` under `-D warnings` (measured: gate run
    /// 43740608570). The event therefore carries the LABEL, not the policy
    /// type, and every channel renders the same word for the same scope.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::FullWindow => "full",
            Self::DeltaSinceMarker => "delta",
            Self::SegmentConsolidation => "consolidate",
        }
    }
}
