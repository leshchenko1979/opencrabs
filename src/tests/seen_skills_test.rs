// Tests for issue #131: skills are loadable via `load_brain_file` slug form,
// and any skill-body consumption (read or slug-load) marks the skill SEEN so
// the post-compaction inventory stamp (#125) lists it.

use crate::brain::tools::Tool;
use crate::brain::tools::ToolExecutionContext;
use crate::brain::tools::load_brain_file::*;
use crate::brain::tools::seen_skills;
use uuid::Uuid;

fn ctx() -> ToolExecutionContext {
    ToolExecutionContext::new(Uuid::new_v4())
}

fn tool() -> LoadBrainFileTool {
    LoadBrainFileTool
}

// ── acceptance 1: slug form loads the body; traversal still refused ────────

#[tokio::test]
async fn slug_form_loads_builtin_skill_body() {
    let c = ctx();
    let result = tool()
        .execute(serde_json::json!({"name": "cost-estimate"}), &c)
        .await
        .unwrap();
    assert!(result.success, "slug form of a built-in skill must succeed");
    let text = result.output;
    assert!(
        text.contains("--- skill: cost-estimate ---"),
        "body must be framed as a skill, got: {}",
        &text[..text.len().min(200)]
    );
    assert!(!text.is_empty(), "skill body must not be empty");
}

#[tokio::test]
async fn slug_form_with_query_returns_matching_sections() {
    let c = ctx();
    let result = tool()
        .execute(
            serde_json::json!({"name": "cost-estimate", "query": "usage"}),
            &c,
        )
        .await
        .unwrap();
    assert!(result.success, "slug+query form must succeed");
}

#[tokio::test]
async fn path_traversal_still_refused_after_slug_form_added() {
    for bad in [
        "../../etc/passwd",
        "sub/file.md",
        "../skills/cost-estimate/SKILL.md",
    ] {
        let result = tool()
            .execute(serde_json::json!({"name": bad}), &ctx())
            .await
            .unwrap();
        assert!(!result.success, "traversal input {bad} must fail");
    }
}

#[tokio::test]
async fn unknown_slug_falls_through_to_brain_file_error() {
    let result = tool()
        .execute(
            serde_json::json!({"name": "no-such-skill-or-brain"}),
            &ctx(),
        )
        .await
        .unwrap();
    // Pre-existing contract (unchanged by #131): a missing brain file is a
    // SOFT success carrying a not-found message — the slug branch must not
    // have resolved it, so the body must be the brain-file not-found text,
    // never skill content. Also: nothing gets marked seen.
    let out = &result.output;
    assert!(
        out.contains("not found"),
        "unknown slug must fall through to the brain-file not-found body, got: {out}"
    );
    assert!(
        !out.contains("--- skill:"),
        "unknown slug must never render as skill content"
    );
}

// ── acceptance 2: slug-load marks the skill SEEN ────────────────────────────

#[tokio::test]
async fn slug_load_marks_skill_seen_for_session() {
    let c = ctx();
    let session = c.session_id;
    assert!(
        !seen_skills::was_seen(session, "cost-estimate"),
        "fresh session must not have the skill seen"
    );
    tool()
        .execute(serde_json::json!({"name": "cost-estimate"}), &c)
        .await
        .unwrap();
    assert!(
        seen_skills::was_seen(session, "cost-estimate"),
        "slug-load must mark the skill seen"
    );
    assert_eq!(
        seen_skills::seen_for_session(session),
        vec!["cost-estimate".to_string()]
    );
}

// ── acceptance 2/3: read_file on a SKILL.md also counts ────────────────────

#[test]
fn read_file_whole_read_marks_skill_seen_via_hook() {
    // The read.rs hook calls skill_slug_from_path before mark_seen; here we
    // verify the registry contract the hook relies on, session-scoped.
    let session = Uuid::new_v4();
    let path = std::path::Path::new("/root/.opencrabs/profiles/ops/skills/grafana/SKILL.md");
    let slug = seen_skills::skill_slug_from_path(path).expect("skill path must yield slug");
    assert_eq!(slug, "grafana");
    seen_skills::mark_seen(session, &slug);
    assert!(seen_skills::was_seen(session, "grafana"));
}

// ── acceptance 3: the STAMP is the real active ∪ seen union (#138 part 2) ──

/// Replaces the old `stamp_union_dedupes_active_and_seen`, which built two
/// local `BTreeSet`s and called `HashSet::union` on them. That test exercised
/// the stdlib, not the product: it passed with the production stamp untouched
/// and with `seen_for_session` still dead code — it could never have caught
/// the gap #138 exists to close.
///
/// This one drives the REAL union helper and asserts the three things that
/// actually matter: the stamp carries both registries, the re-injection set
/// carries ONLY the active one, and the overlap dedupes.
#[test]
fn stamp_union_spans_active_and_seen_while_active_stays_narrow() {
    let sid = Uuid::new_v4();
    // Held active AND consumed → must appear exactly once.
    seen_skills::mark_seen(sid, "opencrabs-dev");
    seen_skills::mark_active(sid, "opencrabs-dev");
    // Consumed then discarded → seen only, and must still reach the stamp.
    seen_skills::mark_seen(sid, "grafana");

    let stamp = seen_skills::stamp_skills_for_session(sid);
    assert_eq!(stamp.len(), 2, "overlap must dedupe: {stamp:?}");
    assert!(stamp.contains("opencrabs-dev"), "active half missing");
    assert!(stamp.contains("grafana"), "seen-only half missing");

    let active = seen_skills::active_for_session(sid);
    assert_eq!(
        active.len(),
        1,
        "re-injection must stay active-only: {active:?}"
    );
    assert!(active.contains("opencrabs-dev"));
    assert!(
        !active.contains("grafana"),
        "a seen-only skill must NOT be re-injected"
    );

    // …and the union renders through the REAL inventory builder.
    let rendered = crate::brain::agent::service::AgentService::format_context_inventory(
        200_000,
        &stamp,
        &std::collections::HashSet::new(),
        None,
    );
    assert!(
        rendered.contains("opencrabs-dev") && rendered.contains("grafana"),
        "the rendered inventory must list both halves of the union, got: {rendered}"
    );
}

/// Discarding a skill clears it from BOTH registries (#138 part 2) — the
/// discard path must not leave a skill that is neither re-injected nor
/// stamped still lingering in either set.
#[test]
fn discard_clears_active_and_seen() {
    let sid = Uuid::new_v4();
    seen_skills::mark_seen(sid, "grafana");
    seen_skills::mark_active(sid, "grafana");
    assert!(seen_skills::active_for_session(sid).contains("grafana"));
    assert!(seen_skills::was_seen(sid, "grafana"));

    seen_skills::unmark_seen(sid, "grafana");
    seen_skills::unmark_active(sid, "grafana");

    assert!(
        seen_skills::active_for_session(sid).is_empty(),
        "discard must drop the active entry"
    );
    assert!(
        !seen_skills::was_seen(sid, "grafana"),
        "discard must drop the seen entry too"
    );
    assert!(
        seen_skills::stamp_skills_for_session(sid).is_empty(),
        "so the stamp must then be empty"
    );
}

/// The two registries are INDEPENDENT: activating a skill must not mark it
/// seen, and marking it seen must not activate it. If they leaked into each
/// other, every read would start re-injecting its own body.
#[test]
fn active_and_seen_are_independent_registries() {
    let sid = Uuid::new_v4();
    seen_skills::mark_active(sid, "cost-estimate");
    assert!(
        !seen_skills::was_seen(sid, "cost-estimate"),
        "activation alone must not mark the skill consumed"
    );
    assert!(
        seen_skills::stamp_skills_for_session(sid).contains("cost-estimate"),
        "but the stamp still sees it via the active half"
    );

    let other = Uuid::new_v4();
    seen_skills::mark_seen(other, "cost-estimate");
    assert!(
        seen_skills::active_for_session(other).is_empty(),
        "consumption alone must not activate the skill"
    );
}

// ── acceptance 5: stamp-build observability is a DEBUG line ────────────────
// Verified by code inspection of continuation_prompt (tracing::debug! with
// the full inventory list).

// ── issue #138 gap 2: filename form registers the skill ────────────────────

#[tokio::test]
async fn filename_form_marks_skill_seen() {
    let c = ctx();
    let session = c.session_id;
    assert!(!seen_skills::was_seen(session, "cost-estimate"));
    let result = tool()
        .execute(serde_json::json!({"name": "cost-estimate.md"}), &c)
        .await
        .unwrap();
    assert!(
        result.success,
        "filename form of a built-in skill must succeed"
    );
    assert!(
        result.output.contains("--- skill: cost-estimate ---"),
        "filename form must render as skill content, got: {}",
        &result.output[..result.output.len().min(200)]
    );
    assert!(
        seen_skills::was_seen(session, "cost-estimate"),
        "filename-form load must mark the skill seen (#138 gap 2)"
    );
}

#[tokio::test]
async fn filename_form_with_query_marks_skill_seen_and_filters() {
    let c = ctx();
    let session = c.session_id;
    let result = tool()
        .execute(
            serde_json::json!({"name": "cost-estimate.md", "query": "usage"}),
            &c,
        )
        .await
        .unwrap();
    assert!(result.success, "filename+query form must succeed");
    assert!(
        seen_skills::was_seen(session, "cost-estimate"),
        "query-filtered filename-form load is consumption too (#138 gap 2)"
    );
}

#[tokio::test]
async fn filename_form_of_brain_file_still_reads_flat_file() {
    // A real brain file must NOT be intercepted by the skill branch — only
    // names that resolve through the skill registry take the skill path.
    let result = tool()
        .execute(
            serde_json::json!({"name": "nonexistent-brain-file.md"}),
            &ctx(),
        )
        .await
        .unwrap();
    // Missing brain file is a SOFT success carrying not-found text.
    assert!(result.output.contains("not found") || result.output.contains("exists but is empty"));
}

#[tokio::test]
async fn filename_form_traversal_still_refused() {
    let fresh = uuid::Uuid::new_v4();
    for bad in ["../skills/cost-estimate/SKILL.md", "sub/cost-estimate.md"] {
        let result = tool()
            .execute(serde_json::json!({"name": bad}), &ctx())
            .await
            .unwrap();
        assert!(!result.success, "traversal input {bad} must fail");
        assert!(!seen_skills::was_seen(fresh, "cost-estimate"));
    }
}

// ── issue #138 gap 1: DB persistence + hydrate ─────────────────────────────

mod persistence {
    use crate::brain::tools::seen_skills;
    use crate::db::Database;
    use crate::db::repository::SessionSkillsRepository;
    use uuid::Uuid;

    async fn repo() -> (Database, SessionSkillsRepository) {
        let db = Database::connect_in_memory().await.expect("in-memory db");
        db.run_migrations().await.expect("migrations");
        let r = SessionSkillsRepository::new(db.pool().clone());
        (db, r)
    }

    #[tokio::test]
    async fn record_upserts_and_all_reads_back() {
        let (_db, r) = repo().await;
        let sid = Uuid::new_v4();
        r.record(sid, "opencrabs-dev", 0).await.expect("record");
        r.record(sid, "opencrabs-dev", 0)
            .await
            .expect("re-record (upsert)");
        r.record(sid, "grafana", 1).await.expect("record 2");
        let rows = r.all().await.expect("all");
        assert_eq!(rows.len(), 2, "upsert must not duplicate rows");
        assert!(
            rows.contains(&(sid, "opencrabs-dev".to_string(), Some(0), false)),
            "epoch roundtrip; record() never touches the active flag"
        );
        assert!(
            rows.contains(&(sid, "grafana".to_string(), Some(1), false)),
            "epoch roundtrip; record() never touches the active flag"
        );
    }

    #[tokio::test]
    async fn prune_drops_rows_for_missing_sessions_only() {
        let (_db, r) = repo().await;
        let live = Uuid::new_v4();
        let dead = Uuid::new_v4();
        r.record(live, "grafana", 0).await.expect("live row");
        r.record(dead, "grafana", 0).await.expect("dead row");
        // The live session must exist in `sessions` for the prune-keep leg.
        let pool = _db.pool().clone();
        pool.get()
            .await
            .expect("conn")
            .interact(move |conn| {
                conn.execute(
                    "INSERT INTO sessions (id, title, model, created_at, updated_at) \
                     VALUES (?1, 't', 'm', unixepoch(), unixepoch())",
                    rusqlite::params![live.to_string()],
                )
            })
            .await
            .expect("interact")
            .expect("insert session");
        let pruned = r.prune_missing_sessions().await.expect("prune");
        assert_eq!(pruned, 1, "exactly the dead session's row goes");
        let rows = r.all().await.expect("all after prune");
        assert_eq!(
            rows,
            vec![(live, "grafana".to_string(), Some(0), false)],
            "the surviving row keeps its epoch and its inactive flag"
        );
    }

    #[tokio::test]
    async fn hydrate_loads_rows_into_registry() {
        let (_db, r) = repo().await;
        let sid = Uuid::new_v4();
        r.record(sid, "repo-audit", 0).await.expect("record");
        assert!(!seen_skills::was_seen(sid, "repo-audit"));
        // hydrate_from_db reads the GLOBAL pool (process-wide OnceLock, not
        // settable in tests) — so we test the hydrate DATA path via the repo
        // + registry contract it feeds, not the global-pool plumbing.
        let rows = r.all().await.expect("all");
        for (s, slug, _epoch, _active) in rows {
            seen_skills::mark_seen(s, &slug);
        }
        assert!(
            seen_skills::was_seen(sid, "repo-audit"),
            "applying hydrate rows must mark skills seen"
        );
    }

    // ── #138 part 2: the ACTIVE registry survives a restart ────────────────

    /// Round-trip: a skill activated before a restart is STILL active after
    /// the restart rehydrates from the DB — and still listed in the stamp.
    ///
    /// This is the acceptance criterion #138 was opened for. "Restart" is
    /// modelled by the real hydrate path minus the global pool: read rows
    /// from the test DB -> pure fold -> apply to the process-wide registries.
    /// The pool plumbing itself is untestable here because `global_pool()` is
    /// a process-wide OnceLock no test can set — which is precisely why the
    /// fold was split out as a pure function.
    #[tokio::test]
    async fn activated_skill_survives_a_restart() {
        let (_db, r) = repo().await;
        let sid = Uuid::new_v4();
        r.record(sid, "opencrabs-dev", 0).await.expect("record");
        r.set_active(sid, "opencrabs-dev", true)
            .await
            .expect("activate");

        let seeds = seen_skills::hydrate_from_rows(r.all().await.expect("all"));
        seen_skills::apply_seeds(seeds);

        assert!(
            seen_skills::active_for_session(sid).contains("opencrabs-dev"),
            "the active flag must survive the restart"
        );
        assert!(
            seen_skills::stamp_skills_for_session(sid).contains("opencrabs-dev"),
            "and the stamp must list it"
        );
    }

    /// Negative: a SEEN-only row (`active = 0`) must NOT come back active.
    /// If it did, every skill a session ever read would be re-injected on top
    /// of the read already sitting in its own conversation history.
    #[tokio::test]
    async fn seen_only_row_does_not_rehydrate_as_active() {
        let (_db, r) = repo().await;
        let sid = Uuid::new_v4();
        r.record(sid, "grafana", 0)
            .await
            .expect("record (seen only)");

        let seeds = seen_skills::hydrate_from_rows(r.all().await.expect("all"));
        seen_skills::apply_seeds(seeds);

        assert!(
            seen_skills::active_for_session(sid).is_empty(),
            "a seen-only row must not hydrate as active"
        );
        assert!(
            seen_skills::stamp_skills_for_session(sid).contains("grafana"),
            "but it MUST still reach the union stamp"
        );
    }

    /// A legacy row — written before the active column existed — carries NULL
    /// epoch and `active = 0`: seen-only, exactly the pre-feature semantics.
    #[tokio::test]
    async fn legacy_row_hydrates_seen_at_epoch_zero_and_inactive() {
        let (_db, r) = repo().await;
        let sid = Uuid::new_v4();
        // Bypass the repo and write the row exactly as the old schema would
        // have left it: no epoch, and the active column taking its default.
        let pool = _db.pool().clone();
        pool.get()
            .await
            .expect("conn")
            .interact(move |conn| {
                conn.execute(
                    "INSERT INTO session_seen_skills (session_id, slug) VALUES (?1, ?2)",
                    rusqlite::params![sid.to_string(), "legacy-skill"],
                )
            })
            .await
            .expect("interact")
            .expect("insert legacy row");

        let rows = r.all().await.expect("all");
        assert_eq!(
            rows,
            vec![(sid, "legacy-skill".to_string(), None, false)],
            "a legacy row reads back with NULL epoch and an inactive flag"
        );

        let seeds = seen_skills::hydrate_from_rows(rows);
        assert_eq!(
            seeds.seen.get(&(sid, "legacy-skill".to_string())),
            Some(&0),
            "NULL epoch must fold to 0 (always current — back-compat sessions pass the gate)"
        );
        assert!(
            !seeds.active.contains_key(&sid),
            "a legacy row is seen-only and must not seed the active registry"
        );
    }

    /// Activation UPSERTS, so it works even when the active path runs before
    /// the seen path ever wrote a row for that (session, slug).
    #[tokio::test]
    async fn set_active_upserts_when_no_seen_row_exists() {
        let (_db, r) = repo().await;
        let sid = Uuid::new_v4();
        r.set_active(sid, "cost-estimate", true)
            .await
            .expect("activate without a prior record()");
        assert_eq!(
            r.all().await.expect("all"),
            vec![(sid, "cost-estimate".to_string(), None, true)],
            "activation alone must create the row, flagged active"
        );
    }

    /// Deactivation is deliberately a plain UPDATE, NOT an upsert: the
    /// discard path deletes the row outright, and an upserting deactivate
    /// would resurrect it as a phantom seen-only row.
    #[tokio::test]
    async fn deactivate_does_not_resurrect_a_deleted_row() {
        let (_db, r) = repo().await;
        let sid = Uuid::new_v4();
        r.set_active(sid, "grafana", true).await.expect("activate");
        r.delete_skill(sid, "grafana").await.expect("delete row");
        r.set_active(sid, "grafana", false)
            .await
            .expect("deactivate a row that no longer exists");
        assert!(
            r.all().await.expect("all").is_empty(),
            "deactivating a deleted row must not bring it back"
        );
    }
}
