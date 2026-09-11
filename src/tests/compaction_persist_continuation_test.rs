//! Every compaction kind persists its continuation (Duty-4).
//!
//! Two failure classes are pinned here:
//!
//! 1. **The silent non-persist.** `apply_compaction_continuation` takes a
//! 1. **The silent non-persist.** `apply_compaction_continuation` takes a
//!    `persist` flag; only Manual /compact passed `true`. All five automatic
//!    kinds passed `false`, so the continuation (continue-instructions, plan recovery)
//!    reached in-memory context only and was never written to the DB. A restarted
//!    auto-compacted session replayed marker + bare summary and got no
//!    continuation at all. This guard reads the call sites themselves,
//!    like the single-continuation-path guard does.
//!    continuation rides the restart for free. The parity test pins that:
//!    a DB-shaped history that carries the marker + stamped continuation
//!    survives the replay with both stamps intact.

use std::path::Path;

const TOOL_LOOP: &str = "src/brain/agent/service/tool_loop.rs";

fn code_lines(text: &str) -> Vec<(usize, &str)> {
    text.lines()
        .enumerate()
        .map(|(i, line)| (i + 1, line.split("//").next().unwrap_or("")))
        .collect()
}

/// Every call to `apply_compaction_continuation` must pass `persist=true`.
/// The flag is positional (last argument before the `)`), so the guard walks
/// from each call site to the terminating `.await` and inspects that block.
#[test]
fn every_compaction_site_persists_its_continuation() {
    let text = std::fs::read_to_string(Path::new(TOOL_LOOP))
        .unwrap_or_else(|e| panic!("{TOOL_LOOP} must be readable ({e}); did the module move?"));
    let lines = code_lines(&text);

    let mut sites = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].1.contains("apply_compaction_continuation(")
            && !lines[i]
                .1
                .contains("async fn apply_compaction_continuation")
        {
            // Walk to the terminating `.await` (bounded walk, always < 20 lines).
            let start = i;
            let mut j = i;
            while j < lines.len() && !lines[j].1.contains(".await") {
                j += 1;
            }
            sites.push((start + 1, j + 1));
            i = j;
        }
        i += 1;
    }

    assert!(
        sites.len() == 6,
        "expected 6 compaction sites (Manual, Regular, MidLoop x2, Emergency, PostTool), \
         found {}: {:?} — the guard is measuring the wrong surface",
        sites.len(),
        sites
    );

    for (start, end) in &sites {
        let block: String = lines[*start - 1..*end]
            .iter()
            .map(|(_, l)| *l)
            .collect::<Vec<&str>>()
            .join("\n");
        assert!(
            !block.contains("false,"),
            "compaction site at line {start} passes persist=false — the continuation \
             (skill stamp, continue-instructions, plan recovery) is lost on restart: {block}"
        );
        assert!(
            block.contains("true,"),
            "compaction site at line {start} does not pass a positional persist flag \
             where the guard expects one — signature drift: {block}"
        );
    }
}

/// Restart parity: the loader replays everything after the marker, so the
/// persisted continuation (marker + stamped continuation) must survive the
/// replay with both stamps intact. Asserts against the loader itself, not a
/// copy of its marker string.
#[test]
fn restart_replay_carries_marker_and_stamped_continuation() {
    use crate::brain::agent::service::AgentService;
    use crate::brain::agent::service::compaction::CompactionOutcome;

    let row = |content: &str| crate::db::models::Message {
        id: uuid::Uuid::new_v4(),
        session_id: uuid::Uuid::nil(),
        role: "user".to_string(),
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
    };

    let all = vec![
        row("ancient history"),
        row(&CompactionOutcome::Summarised("we were fixing the parser".into()).marker("")),
        row(
            "[SYSTEM: Context was auto-compacted.\n\nSKILLS LOADED PRE-COMPACTION: grafana. \
             Session focus may have shifted — consider whether each is still relevant to \
             the IMMEDIATE TASK; reload only those that are.\n\n\
             LAZY TOOLS LOADED PRE-COMPACTION: session_search, telegram_send. Snapshot of \
             what was loaded at compaction time (older entries may have been evicted) — \
             re-surface any tool via tool_search when needed.]",
        ),
        row("after the compaction"),
    ];

    let kept = AgentService::messages_from_last_compaction(all);
    assert_eq!(
        kept.len(),
        3,
        "loader did not anchor on the compaction marker"
    );
    let replayed_continuation: String = kept
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<&str>>()
        .join("\n");

    assert!(
        replayed_continuation.contains("SKILLS LOADED PRE-COMPACTION"),
        "restarted session replays no skill stamp — the waking agent loses its skill inventory"
    );
    assert!(
        replayed_continuation.contains("LAZY TOOLS LOADED PRE-COMPACTION"),
        "restarted session replays no tool stamp — the waking agent loses its lazy-tool inventory"
    );
    assert!(
        replayed_continuation.contains("session_search, telegram_send"),
        "tool stamp names did not survive the replay"
    );
}
