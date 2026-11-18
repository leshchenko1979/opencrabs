//! Epistemic confidence tracking.
//!
//! Moved out of `src/brain/tools/epistemic.rs`: tests live under `src/tests/`,
//! never inline beside the logic they exercise (#1076).

use crate::brain::tools::epistemic::*;
use chrono::Utc;

#[test]
fn test_confidence_ordering() {
    assert!(Confidence::Verified > Confidence::Inferred);
    assert!(Confidence::Inferred > Confidence::Uncertain);
    assert!(Confidence::Uncertain > Confidence::Expired);
    assert!(Confidence::Expired > Confidence::Contradicted);
}

#[test]
fn test_confidence_decay() {
    assert_eq!(Confidence::Verified.decay(), Confidence::Verified);
    assert_eq!(Confidence::Inferred.decay(), Confidence::Uncertain);
    assert_eq!(Confidence::Uncertain.decay(), Confidence::Expired);
    assert_eq!(Confidence::Expired.decay(), Confidence::Expired);
    assert_eq!(Confidence::Contradicted.decay(), Confidence::Contradicted);
}

#[test]
fn test_add_belief_no_contradiction() {
    let mut store = EpistemicStore::new();
    let result = store.add_belief("test:key", "value1", Confidence::Verified, "test");
    assert_eq!(result, ContradictionResult::NoContradiction);
    assert_eq!(store.get_belief("test:key").unwrap().value, "value1");
}

#[test]
fn test_add_belief_contradiction() {
    let mut store = EpistemicStore::new();
    store.add_belief("test:key", "value1", Confidence::Verified, "test");
    let result = store.add_belief("test:key", "value2", Confidence::Inferred, "test2");

    assert!(matches!(result, ContradictionResult::Contradicted { .. }));

    // Old belief should be marked as contradicted
    let contradicted: Vec<_> = store.list_contradictions();
    assert_eq!(contradicted.len(), 1);
    assert_eq!(contradicted[0].value, "value1");

    // New belief should be active
    assert_eq!(store.get_belief("test:key").unwrap().value, "value2");
}

#[test]
fn test_verify_belief() {
    let mut store = EpistemicStore::new();
    store.add_belief("test:key", "value", Confidence::Uncertain, "test");
    assert!(store.verify_belief("test:key"));
    assert_eq!(
        store.get_belief("test:key").unwrap().confidence,
        Confidence::Verified
    );
}

#[test]
fn test_decay_logic() {
    let mut store = EpistemicStore::new();

    // Add a belief with old last_verified
    let mut belief = Belief {
        key: "test:old".to_string(),
        value: "old_value".to_string(),
        confidence: Confidence::Inferred,
        source: Source {
            origin: "test".to_string(),
            recorded_at: Utc::now() - chrono::Duration::days(45),
            last_verified: Utc::now() - chrono::Duration::days(45),
        },
        notes: None,
        hits: 0,
        last_used: None,
    };
    store.beliefs.insert("test:old".to_string(), belief.clone());

    // Add a recent belief
    belief.key = "test:recent".to_string();
    belief.confidence = Confidence::Inferred;
    belief.source.recorded_at = Utc::now();
    belief.source.last_verified = Utc::now();
    store.beliefs.insert("test:recent".to_string(), belief);

    // Apply decay with 30-day threshold
    let decayed = store.apply_decay(30);

    // Only the old belief should decay
    assert_eq!(decayed.len(), 1);
    assert!(decayed[0].contains("test:old"));
    assert_eq!(
        store.get_belief("test:old").unwrap().confidence,
        Confidence::Uncertain
    );
    assert_eq!(
        store.get_belief("test:recent").unwrap().confidence,
        Confidence::Inferred
    );
}

#[test]
fn test_verified_beliefs_dont_decay() {
    let mut store = EpistemicStore::new();

    let belief = Belief {
        key: "test:verified".to_string(),
        value: "verified_value".to_string(),
        confidence: Confidence::Verified,
        source: Source {
            origin: "test".to_string(),
            recorded_at: Utc::now() - chrono::Duration::days(100),
            last_verified: Utc::now() - chrono::Duration::days(100),
        },
        notes: None,
        hits: 0,
        last_used: None,
    };
    store.beliefs.insert("test:verified".to_string(), belief);

    let decayed = store.apply_decay(30);
    assert!(decayed.is_empty());
    assert_eq!(
        store.get_belief("test:verified").unwrap().confidence,
        Confidence::Verified
    );
}

#[test]
fn test_serialization_roundtrip() {
    let mut store = EpistemicStore::new();
    store.add_belief("test:key", "value", Confidence::Inferred, "test:origin");

    let toml_str = toml::to_string_pretty(&store).unwrap();
    let loaded: EpistemicStore = toml::from_str(&toml_str).unwrap();

    assert_eq!(loaded.get_belief("test:key").unwrap().value, "value");
    assert_eq!(
        loaded.get_belief("test:key").unwrap().confidence,
        Confidence::Inferred
    );
}

#[test]
fn test_list_by_key_prefix() {
    let mut store = EpistemicStore::new();
    store.add_belief(
        "plan:task:1:abc",
        "failed",
        Confidence::Contradicted,
        "test",
    );
    store.add_belief("plan:task:2:def", "done", Confidence::Verified, "test");
    store.add_belief(
        "memory:truelens:ip",
        "1.2.3.4",
        Confidence::Inferred,
        "test",
    );

    let plan_beliefs = store.list_by_key_prefix("plan:task:");
    assert_eq!(plan_beliefs.len(), 2);

    let memory_beliefs = store.list_by_key_prefix("memory:");
    assert_eq!(memory_beliefs.len(), 1);
    assert_eq!(memory_beliefs[0].key, "memory:truelens:ip");

    let empty = store.list_by_key_prefix("nonexistent:");
    assert!(empty.is_empty());
}

/// #1641: touch_belief increments hits and sets last_used.
#[test]
fn test_touch_belief() {
    let mut store = EpistemicStore::new();
    store.add_belief("test:key", "value", Confidence::Inferred, "test");

    let belief = store.get_belief("test:key").unwrap();
    assert_eq!(belief.hits, 0);
    assert!(belief.last_used.is_none());

    // First touch
    assert!(store.touch_belief("test:key"));
    let belief = store.get_belief("test:key").unwrap();
    assert_eq!(belief.hits, 1);
    assert!(belief.last_used.is_some());

    // Second touch
    assert!(store.touch_belief("test:key"));
    let belief = store.get_belief("test:key").unwrap();
    assert_eq!(belief.hits, 2);

    // Non-existent key
    assert!(!store.touch_belief("nonexistent"));
}

/// #1641: recently-used beliefs don't decay even if never re-verified.
/// The decay clock uses last_used (falling back to recorded_at), not
/// last_verified.
#[test]
fn test_decay_uses_last_used_not_last_verified() {
    let mut store = EpistemicStore::new();

    // Old belief that was recently touched — should NOT decay
    let mut belief = Belief {
        key: "test:old_but_used".to_string(),
        value: "still_valid".to_string(),
        confidence: Confidence::Inferred,
        source: Source {
            origin: "test".to_string(),
            recorded_at: Utc::now() - chrono::Duration::days(60),
            last_verified: Utc::now() - chrono::Duration::days(60),
        },
        notes: None,
        hits: 5,
        last_used: Some(Utc::now() - chrono::Duration::days(2)),
    };
    store
        .beliefs
        .insert("test:old_but_used".to_string(), belief.clone());

    // Old belief never touched — SHOULD decay
    belief.key = "test:old_unused".to_string();
    belief.value = "stale".to_string();
    belief.hits = 0;
    belief.last_used = None;
    store.beliefs.insert("test:old_unused".to_string(), belief);

    let decayed = store.apply_decay(30);

    // Only the unused one decayed
    assert_eq!(decayed.len(), 1);
    assert!(decayed[0].contains("test:old_unused"));

    // The recently-used one is still Inferred
    assert_eq!(
        store.get_belief("test:old_but_used").unwrap().confidence,
        Confidence::Inferred
    );

    // The unused one dropped to Uncertain
    assert_eq!(
        store.get_belief("test:old_unused").unwrap().confidence,
        Confidence::Uncertain
    );
}

/// #1641: backfill indexes MEMORY.md sections into section-anchored beliefs.
#[test]
fn test_backfill_from_content() {
    let mut store = EpistemicStore::new();

    let markdown = r#"# MEMORY.md - Long-Term Memory

Some preamble text that should be skipped.

## Rules

- NEVER push without explicit user approval
- Use cargo clippy --all-features, NEVER cargo check

## Integrations

- Telegram bot connected via @opencrabs_bot
- Discord server linked

## Short

tiny

## Another Rule

This section has enough content to be indexed as a belief.
It contains important operational context for the agent.
"#;

    let added = store.backfill_from_content(markdown);

    // Preamble (no heading) and "Short" (body < 20 chars) are skipped
    assert_eq!(added, 3);

    // Section-anchored keys
    assert!(store.get_belief("MEMORY.md##Rules").is_some());
    assert!(store.get_belief("MEMORY.md##Integrations").is_some());
    assert!(store.get_belief("MEMORY.md##Another Rule").is_some());

    // Skipped sections
    assert!(store.get_belief("MEMORY.md##Short").is_none());

    // Beliefs have correct confidence and zero hits
    let rules = store.get_belief("MEMORY.md##Rules").unwrap();
    assert_eq!(rules.confidence, Confidence::Inferred);
    assert_eq!(rules.hits, 0);
    assert!(rules.last_used.is_none());
}

/// #1641: backfill does not overwrite existing beliefs.
#[test]
fn test_backfill_no_overwrite() {
    let mut store = EpistemicStore::new();

    // Pre-existing verified belief
    store.add_belief(
        "MEMORY.md##Rules",
        "custom value",
        Confidence::Verified,
        "user",
    );

    let markdown = r#"## Rules

- Some rule that would conflict with the existing belief

## New Section

This is a brand new section with enough content to be indexed.
"#;

    let added = store.backfill_from_content(markdown);

    // Only "New Section" was added; "Rules" was skipped (already exists)
    assert_eq!(added, 1);

    // Original belief preserved
    let rules = store.get_belief("MEMORY.md##Rules").unwrap();
    assert_eq!(rules.value, "custom value");
    assert_eq!(rules.confidence, Confidence::Verified);
}

/// #1641: cold facts (0 hits, 90+ days) are deleted.
#[test]
fn test_delete_cold_beliefs() {
    let mut store = EpistemicStore::new();

    // Cold: 0 hits, 100 days old
    let cold = Belief {
        key: "test:cold".to_string(),
        value: "unused".to_string(),
        confidence: Confidence::Inferred,
        source: Source {
            origin: "test".to_string(),
            recorded_at: Utc::now() - chrono::Duration::days(100),
            last_verified: Utc::now() - chrono::Duration::days(100),
        },
        notes: None,
        hits: 0,
        last_used: None,
    };
    store.beliefs.insert("test:cold".to_string(), cold);

    // Warm: has hits, old
    let warm = Belief {
        key: "test:warm".to_string(),
        value: "used".to_string(),
        confidence: Confidence::Inferred,
        source: Source {
            origin: "test".to_string(),
            recorded_at: Utc::now() - chrono::Duration::days(100),
            last_verified: Utc::now() - chrono::Duration::days(100),
        },
        notes: None,
        hits: 3,
        last_used: Some(Utc::now() - chrono::Duration::days(100)),
    };
    store.beliefs.insert("test:warm".to_string(), warm);

    // Recent: 0 hits, but only 10 days old
    let recent = Belief {
        key: "test:recent".to_string(),
        value: "new".to_string(),
        confidence: Confidence::Inferred,
        source: Source {
            origin: "test".to_string(),
            recorded_at: Utc::now() - chrono::Duration::days(10),
            last_verified: Utc::now() - chrono::Duration::days(10),
        },
        notes: None,
        hits: 0,
        last_used: None,
    };
    store.beliefs.insert("test:recent".to_string(), recent);

    let deleted = store.delete_cold_beliefs(90);

    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0], "test:cold");
    assert!(store.get_belief("test:cold").is_none());
    assert!(store.get_belief("test:warm").is_some());
    assert!(store.get_belief("test:recent").is_some());
}

/// #1641: list_cold_beliefs returns candidates without deleting.
#[test]
fn test_list_cold_beliefs_dry_run() {
    let mut store = EpistemicStore::new();

    let cold = Belief {
        key: "test:cold".to_string(),
        value: "unused".to_string(),
        confidence: Confidence::Inferred,
        source: Source {
            origin: "test".to_string(),
            recorded_at: Utc::now() - chrono::Duration::days(100),
            last_verified: Utc::now() - chrono::Duration::days(100),
        },
        notes: None,
        hits: 0,
        last_used: None,
    };
    store.beliefs.insert("test:cold".to_string(), cold);

    let candidates = store.list_cold_beliefs(90);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].key, "test:cold");

    // Still exists — dry run didn't delete
    assert!(store.get_belief("test:cold").is_some());
}

// ---------------------------------------------------------------------------
// #887 — Expired level, touch rehabilitation, line-level namespace prune.
// ---------------------------------------------------------------------------

/// Build an untouched belief whose decay clock has already run out.
fn stale_belief(key: &str, confidence: Confidence) -> Belief {
    let long_ago = Utc::now() - chrono::Duration::days(60);
    Belief {
        key: key.to_string(),
        value: format!("value for {key}"),
        confidence,
        source: Source {
            origin: "test".to_string(),
            recorded_at: long_ago,
            last_verified: long_ago,
        },
        notes: None,
        hits: 0,
        last_used: None,
    }
}

/// #887: the full decay ladder, walked one pass at a time. Both terminals
/// hold: `Expired` and `Contradicted` never move again, `Verified` never
/// moves at all.
#[test]
fn test_decay_chain_walks_to_expired_and_stops() {
    let mut store = EpistemicStore::new();
    for level in [
        Confidence::Verified,
        Confidence::Inferred,
        Confidence::Uncertain,
        Confidence::Expired,
        Confidence::Contradicted,
    ] {
        let key = format!("test:chain:{}", level.label());
        let belief = stale_belief(&key, level);
        store.beliefs.insert(key, belief);
    }

    // Pass 1: Inferred -> Uncertain, Uncertain -> Expired, terminals hold.
    let decayed = store.apply_decay(30);
    assert_eq!(decayed.len(), 2, "only the two non-terminal rows move");
    assert_eq!(
        store.get_belief("test:chain:inferred").unwrap().confidence,
        Confidence::Uncertain
    );
    assert_eq!(
        store.get_belief("test:chain:uncertain").unwrap().confidence,
        Confidence::Expired
    );
    assert_eq!(
        store.get_belief("test:chain:expired").unwrap().confidence,
        Confidence::Expired
    );
    assert_eq!(
        store.get_belief("test:chain:contradicted").unwrap().confidence,
        Confidence::Contradicted
    );
    assert_eq!(
        store.get_belief("test:chain:verified").unwrap().confidence,
        Confidence::Verified
    );

    // Pass 2: the former Inferred row reaches the Expired terminal.
    let decayed = store.apply_decay(30);
    assert_eq!(decayed.len(), 1);
    assert!(decayed[0].contains("test:chain:inferred"));
    assert_eq!(
        store.get_belief("test:chain:inferred").unwrap().confidence,
        Confidence::Expired
    );

    // Pass 3: nothing left to decay — both terminals are stable.
    assert!(store.apply_decay(30).is_empty());
}

/// #887: decay no longer manufactures contradictions, so the contradiction
/// list reports real value conflicts only.
#[test]
fn test_decay_never_produces_contradictions() {
    let mut store = EpistemicStore::new();
    let belief = stale_belief("MEMORY.md##Old Rule", Confidence::Inferred);
    store.beliefs.insert("MEMORY.md##Old Rule".to_string(), belief);

    // Age it out entirely.
    store.apply_decay(30);
    store.apply_decay(30);
    assert_eq!(
        store.get_belief("MEMORY.md##Old Rule").unwrap().confidence,
        Confidence::Expired
    );
    assert!(
        store.list_contradictions().is_empty(),
        "an expired belief is stale, not contradicted"
    );
}

/// #887: an injection rehabilitates Expired -> Uncertain, and only that.
/// `Contradicted` must survive a touch — a recall does not resolve a value
/// conflict — and `Verified` is already at the top.
#[test]
fn test_touch_rehabilitates_expired_only() {
    let mut store = EpistemicStore::new();
    for level in [
        Confidence::Verified,
        Confidence::Expired,
        Confidence::Contradicted,
    ] {
        let key = format!("test:touch:{}", level.label());
        let belief = stale_belief(&key, level);
        store.beliefs.insert(key, belief);
    }

    for level in [
        Confidence::Verified,
        Confidence::Expired,
        Confidence::Contradicted,
    ] {
        let key = format!("test:touch:{}", level.label());
        assert!(store.touch_belief(&key));
        let touched = store.get_belief(&key).unwrap();
        assert_eq!(touched.hits, 1, "{key} still counts the hit");
        assert!(touched.last_used.is_some(), "{key} still stamps last_used");
    }

    assert_eq!(
        store.get_belief("test:touch:expired").unwrap().confidence,
        Confidence::Uncertain,
        "injection is weak evidence of relevance"
    );
    assert_eq!(
        store
            .get_belief("test:touch:contradicted")
            .unwrap()
            .confidence,
        Confidence::Contradicted,
        "a recall never clears a real value conflict"
    );
    assert_eq!(
        store.get_belief("test:touch:verified").unwrap().confidence,
        Confidence::Verified
    );

    // The rehabilitated row now decays normally again (one step per pass,
    // back toward Expired) instead of being stuck.
    {
        let belief = store
            .beliefs
            .get_mut("test:touch:expired")
            .expect("rehabilitated row");
        belief.last_used = Some(Utc::now() - chrono::Duration::days(60));
    }
    store.apply_decay(30);
    assert_eq!(
        store.get_belief("test:touch:expired").unwrap().confidence,
        Confidence::Expired
    );
}

/// #887: the retired `memory:<hash>` line-level namespace is pruned, the
/// live `MEMORY.md##<title>` section namespace survives, and a second run
/// is a no-op.
#[test]
fn test_prune_line_level_memory_beliefs() {
    let mut store = EpistemicStore::new();

    // Line-level rows: the write-only namespace this prune exists for.
    let line_keys = [
        "memory:0f3a1b2c3d4e5f60",
        "memory:9a8b7c6d5e4f3021",
        "memory:unhashed-legacy-key",
    ];
    for key in line_keys {
        let belief = stale_belief(key, Confidence::Contradicted);
        store.beliefs.insert(key.to_string(), belief);
    }

    // Section rows: the live namespace, touched by recall and read by the
    // archive pass. These must survive untouched.
    for key in ["MEMORY.md##Rules", "MEMORY.md##War Stories"] {
        let belief = stale_belief(key, Confidence::Inferred);
        store.beliefs.insert(key.to_string(), belief);
    }
    // Unrelated namespaces survive too.
    let other = stale_belief("plan:task:1:abc", Confidence::Verified);
    store.beliefs.insert("plan:task:1:abc".to_string(), other);

    let removed = store.prune_line_level_memory_beliefs();

    assert_eq!(removed.len(), line_keys.len());
    for key in line_keys {
        assert!(
            store.get_belief(key).is_none(),
            "{key} should have been pruned"
        );
    }
    assert_eq!(store.beliefs.len(), 3);
    assert!(store.get_belief("MEMORY.md##Rules").is_some());
    assert!(store.get_belief("MEMORY.md##War Stories").is_some());
    assert!(store.get_belief("plan:task:1:abc").is_some());

    // Idempotent: nothing left to prune.
    assert!(store.prune_line_level_memory_beliefs().is_empty());
    assert_eq!(store.beliefs.len(), 3);
}

/// #887: the prune persists — a pruned store reloads without the retired
/// rows, and section keys round-trip. Uses a temp dir, never the live
/// profile store.
#[test]
fn test_prune_persists_through_save_and_load() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("brain/epistemic/beliefs.toml");

    let mut store = EpistemicStore::new();
    let line = stale_belief("memory:0f3a1b2c3d4e5f60", Confidence::Expired);
    store.beliefs.insert(line.key.clone(), line);
    let section = stale_belief("MEMORY.md##Rules", Confidence::Inferred);
    store.beliefs.insert(section.key.clone(), section);
    store.save(&path).expect("save");

    let mut reloaded = EpistemicStore::load(&path);
    assert_eq!(reloaded.beliefs.len(), 2, "pre-state has both namespaces");
    assert_eq!(reloaded.prune_line_level_memory_beliefs().len(), 1);
    reloaded.save(&path).expect("save pruned");

    let after = EpistemicStore::load(&path);
    assert_eq!(after.beliefs.len(), 1);
    assert!(after.get_belief("memory:0f3a1b2c3d4e5f60").is_none());
    assert_eq!(
        after.get_belief("MEMORY.md##Rules").unwrap().confidence,
        Confidence::Inferred
    );
}

/// #887: every confidence level round-trips through the TOML store as a
/// string, including the new `Expired` variant. Old rows (`verified`,
/// `inferred`, `uncertain`, `contradicted`) keep reading as before — no
/// variant is renumbered or renamed.
#[test]
fn test_confidence_roundtrips_through_toml() {
    let mut store = EpistemicStore::new();
    let levels = [
        Confidence::Verified,
        Confidence::Inferred,
        Confidence::Uncertain,
        Confidence::Expired,
        Confidence::Contradicted,
    ];
    for level in levels {
        let key = format!("test:rt:{}", level.label());
        store.add_belief(&key, "value", level, "test");
    }

    let toml_str = toml::to_string_pretty(&store).unwrap();
    // Serialized form is the lowercase label — the shape older rows already
    // carry on disk.
    assert!(toml_str.contains("confidence = \"expired\""));
    assert!(toml_str.contains("confidence = \"contradicted\""));

    let loaded: EpistemicStore = toml::from_str(&toml_str).unwrap();
    for level in levels {
        let key = format!("test:rt:{}", level.label());
        assert_eq!(
            loaded.get_belief(&key).unwrap().confidence,
            level,
            "{key} must survive serialization"
        );
    }
}

/// #887: a store written before the `Expired` variant existed still loads —
/// the four historical labels parse to the same variants.
#[test]
fn test_legacy_confidence_rows_still_load() {
    let legacy = r#"
version = 1

[beliefs."MEMORY.md##Rules"]
key = "MEMORY.md##Rules"
value = "old rule"
confidence = "contradicted"

[beliefs."MEMORY.md##Rules".source]
origin = "MEMORY.md backfill"
recorded_at = "2026-01-01T00:00:00Z"
last_verified = "2026-01-01T00:00:00Z"

[beliefs."MEMORY.md##Other"]
key = "MEMORY.md##Other"
value = "other rule"
confidence = "uncertain"

[beliefs."MEMORY.md##Other".source]
origin = "MEMORY.md backfill"
recorded_at = "2026-01-01T00:00:00Z"
last_verified = "2026-01-01T00:00:00Z"
"#;
    let loaded: EpistemicStore = toml::from_str(legacy).unwrap();
    assert_eq!(
        loaded.get_belief("MEMORY.md##Rules").unwrap().confidence,
        Confidence::Contradicted
    );
    assert_eq!(
        loaded.get_belief("MEMORY.md##Other").unwrap().confidence,
        Confidence::Uncertain
    );
}
