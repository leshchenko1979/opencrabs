//! An incremental index must file a document in the same collection the full
//! reindex would (#1018 follow-up).
//!
//! `reindex` has always split them: daily notes into the memory collection,
//! brain files into the brain collection. `index_file` — the incremental path
//! used by the write hook and the freshness check — hardcoded memory. A brain
//! file written mid-session therefore landed in the wrong collection, where
//! `search_brain` never looks, while the brain row it should have updated went
//! untouched.
//!
//! Caught on a live index: `AGENTS.md` held two rows, the brain one live and a
//! memory-collection copy left inactive by the next reindex. The incremental
//! index had been doing nothing for brain search while appearing to succeed.

use crate::memory::index::{collection_for, is_dated_daily_note};
use crate::memory::{COLLECTION_BRAIN, COLLECTION_MEMORY};
use std::path::Path;

/// Every brain file routes to the brain collection, wherever it sits on disk.
#[test]
fn brain_files_route_to_the_brain_collection() {
    for name in crate::memory::BRAIN_FILES {
        let path = Path::new("/home/someone/.opencrabs").join(name);
        assert_eq!(
            collection_for(&path),
            COLLECTION_BRAIN,
            "{name} must index into the collection search_brain reads"
        );
    }
}

/// Daily notes stay in the memory collection.
#[test]
fn daily_notes_route_to_the_memory_collection() {
    for name in ["2026-08-12.md", "2026-03-02.md"] {
        let path = Path::new("/home/someone/.opencrabs/memory").join(name);
        assert_eq!(collection_for(&path), COLLECTION_MEMORY, "{name}");
    }
}

/// Anything else is memory, which is the safe default: a stray file in the
/// brain collection would surface in `search_brain` as though it were a rule.
#[test]
fn unknown_files_default_to_the_memory_collection() {
    for name in ["scratch.md", "AGENTVERSE.md", "notes/idea.md"] {
        let path = Path::new("/home/someone/.opencrabs").join(name);
        assert_eq!(collection_for(&path), COLLECTION_MEMORY, "{name}");
    }
}

/// The routing is by file name, so the same brain file reached through a
/// different parent path still lands in the brain collection. Profiles give
/// each workspace its own home, so the parent differs per profile.
#[test]
fn routing_is_by_name_not_by_parent_directory() {
    let a = Path::new("/home/someone/.opencrabs/AGENTS.md");
    let b = Path::new("/home/someone/.opencrabs/profiles/ops/AGENTS.md");
    assert_eq!(collection_for(a), collection_for(b));
    assert_eq!(collection_for(a), COLLECTION_BRAIN);
}

// ---------------------------------------------------------------- #869
//
// Dated daily notes are auto-appended compaction summaries. Measured on the ops
// profile: 146 of them, 230.8 MB, 91.7% of the whole store. `index_daily_notes`
// lets an install stop indexing them, in both indexes at once.

/// The exclusion is opt-in: an install that never sets the key keeps indexing.
#[test]
fn daily_notes_are_indexed_by_default() {
    assert!(
        crate::config::MemoryConfig::default().index_daily_notes,
        "the default must not silently stop indexing an existing install"
    );
    let parsed: crate::config::MemoryConfig =
        toml::from_str("").expect("an empty [memory] table must parse");
    assert!(
        parsed.index_daily_notes,
        "serde default must agree with Default::default()"
    );
}

/// Setting the key is the only way to turn it off, and it parses as a bool.
#[test]
fn daily_note_exclusion_parses_from_config() {
    let parsed: crate::config::MemoryConfig =
        toml::from_str("index_daily_notes = false").expect("[memory] table parses");
    assert!(!parsed.index_daily_notes);
}

/// Only the compaction-summary naming counts as a dated daily note.
#[test]
fn dated_daily_notes_are_recognised() {
    for key in ["2026-11-17.md", "2026-05-20.md", "9999-99-99.md"] {
        assert!(is_dated_daily_note(key), "{key} is a daily note");
    }
}

/// Everything else in `memory/` keeps indexing: hand-written notes, archive
/// keys (retired MEMORY.md sections), and any other shape.
#[test]
fn other_memory_files_are_not_dated_daily_notes() {
    for key in [
        "archive/2026-09.md",            // retired MEMORY.md section
        "2026-09-07-buttons-verdict.md", // hand-written, dated-looking
        "incident-subagent-model.md",    // hand-written
        "2026-9-7.md",                   // not the emitter's format
        "2026-11-17.md.bak",             // not markdown
        "notes/2026-11-17.md",           // not a bare basename
        "20261117.md",                   // no separators
    ] {
        assert!(!is_dated_daily_note(key), "{key} must stay indexed");
    }
}

/// The chokepoint is where the exclusion actually lands, so drive it with a real
/// store under a home override — the post-compaction hook and
/// `write_opencrabs_file` both come through here.
///
/// Both arms run: the opt-out must suppress a dated note, and the DEFAULT must
/// still index one. Without the second arm a gate that always refuses would
/// pass this test.
#[test]
fn the_chokepoint_honours_the_opt_out() {
    use crate::memory::db::Store;
    use crate::memory::index::index_file_sync_keyed;

    let run = |config: &str| {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("config.toml"), config).unwrap();
        crate::config::profile::with_home_override(home.path().to_path_buf(), || {
            let db = tempfile::tempdir().unwrap();
            let store = Store::open(&db.path().join("memory.db")).expect("test store opens");
            let dated =
                index_file_sync_keyed(&store, COLLECTION_MEMORY, "2026-11-17.md", "## summary")
                    .expect("indexing a dated note must not error");
            let hand_written =
                index_file_sync_keyed(&store, COLLECTION_MEMORY, "incident-x.md", "a note")
                    .expect("indexing a hand-written note must not error");
            let archive =
                index_file_sync_keyed(&store, COLLECTION_MEMORY, "archive/2026-09.md", "old")
                    .expect("indexing an archive key must not error");
            (dated, hand_written, archive)
        })
    };

    let (dated, hand_written, archive) = run("[memory]\nindex_daily_notes = false\n");
    assert!(!dated, "the opt-out must refuse a dated daily note");
    assert!(
        hand_written,
        "a hand-written note in memory/ must stay indexed"
    );
    assert!(
        archive,
        "archive keys are MEMORY.md sections, not daily notes"
    );

    let (dated, _, _) = run("[memory]\nindex_daily_notes = true\n");
    assert!(
        dated,
        "positive control: the default must still index a dated note"
    );
}
