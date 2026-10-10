//! Repair a schema that the version stamp says is complete but is not (#1401).
//!
//! `rusqlite_migration` applies migrations by list INDEX. When two branches
//! each append "migration N" and the merge re-sorts the list by date, any
//! database that was stamped N by the first branch's build skips the other
//! branch's migration forever: the stamp says done, so `to_latest` never
//! looks at it. That is how `pending_requests.origin` (migration 37) went
//! missing on a database stamped 38, and with it every restart-recovery row
//! for three days: the INSERT failed on every turn and boot found nothing to
//! resume.
//!
//! A heal is idempotent and checks the schema, not the stamp. It runs after
//! `to_latest` on every boot, so a healed database stays healed and a
//! correct one is untouched.
//!
//! # The reconciliation pass and the five post-pass heals
//!
//! [`reconcile_before_migrations`] runs BEFORE `to_latest`; the five heals at
//! the foot of this module (`heal_pending_requests_origin`, `heal_notify_queue`,
//! `heal_project_repo_remote`, `heal_session_seen_skills_loaded_mtime`,
//! `heal_tool_executions_exit_code`) still run
//! AFTER it, from `Database::run_migrations`. They are kept because their
//! migrations are NOT all declared above, and a reader who deletes one as
//! "covered by the table" would remove the only cover its migration has:
//!
//! - `heal_notify_queue` (migration 42, `notify_queue`) — **overlapped.**
//!   [`MIGRATION_EFFECTS`] declares the table and its index, so on a boot that
//!   runs the pass the heal is already a no-op. It stays for the callers that
//!   reach it with the stamp at latest and an undeclared fault, and because the
//!   migration's earlier filename date is what made it skippable in the first
//!   place (#111).
//! - `heal_project_repo_remote` (migration 46, `projects.repo_remote`) —
//!   **overlapped for the column, NOT for the index.** The heal early-outs when
//!   the COLUMN is present, so a database carrying `repo_remote` without
//!   `idx_projects_repo_remote` is one it cannot repair; the declaration is
//!   per object and does repair it, but only on a boot whose stamp disagrees
//!   with the schema. The gap is real and is left as it is here: closing it
//!   means changing a heal that is the sole cover for its own migration on the
//!   boots the pass does not touch.
//! - `heal_session_seen_skills_loaded_mtime` (migration 48) — **overlapped.**
//!   The declaration carries both of that migration's columns, which is what
//!   retired the active-migration guard (#724).
//! - `heal_pending_requests_origin` (migration 37, `pending_requests.origin`) —
//!   **NOT overlapped, and must not be.** The migration is below the declared
//!   window, so nothing above covers it; this heal is its only repair (#1401).
//! - `heal_tool_executions_exit_code` (migration 60, `tool_executions.exit_code`)
//!   — **NOT overlapped, and must not be.** The migration is appended LAST, above
//!   the declared window, and the window's contiguity test forbids declaring it
//!   there. Appending is exactly the shape the re-sort punishes: an upstream
//!   merge that inserts an earlier filename below this one shifts its index, and
//!   a database stamped against the pre-merge list then skips it in silence —
//!   the #1401 class in the direction `to_latest` cannot see. This heal is its
//!   only repair.

/// The schema object a migration's effect creates.
///
/// Declared per OBJECT rather than per migration, because a single migration's
/// objects can land in different states: `20260908000000_add_session_seen_skills`
/// creates a table and an index, and a database can carry the table without the
/// index. A per-object declaration lets the reconciliation pass apply exactly
/// the object that is missing instead of replaying a batch that would fail on
/// the object already there.
///
/// The variant carries only the object's NAME — the migration file stays the
/// single source of truth for its definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effect {
    /// A column added by `ALTER TABLE <table> ADD COLUMN <column> ...`.
    Column(&'static str, &'static str),
    /// A table created by `CREATE TABLE`.
    Table(&'static str),
    /// An index created by `CREATE INDEX`.
    Index(&'static str),
}

/// The declared effect of each migration whose replay the boot must reconcile,
/// keyed by a marker that appears in **exactly one** migration's SQL.
///
/// `rusqlite_migration` applies by list INDEX, so `user_version` is a
/// list-relative position and not a migration identity. Inserting a migration
/// with an earlier filename shifts every later index, and a database stamped by
/// the old list then maps its count onto a DIFFERENT migration. This table is
/// what lets the boot ask "is this migration's effect actually present?" rather
/// than trusting the stamp, in both directions.
///
/// Resolution goes through [`migration_index_1based`], so no index is written
/// by hand here (#715): the marker is the identity, the position is derived.
///
/// Two rules bind every row:
///
/// - **Never declare an object a later migration DROPs.** `plans` and
///   `plan_tasks` are dropped by `20260713000001_drop_orphaned_plans_tables.sql`,
///   so they are correctly absent from a database past index 30 — declaring
///   either would make the fill-below pass resurrect a table the list
///   deliberately removes. A module test enforces this.
/// - **Declare only what can be checked.** A migration whose effect is not a
///   table, index or column (a DROP, a data backfill, a rename with no residue)
///   has nothing to verify and is deliberately left out. The declared set must
///   therefore have NO HOLES across the window where the old and new lists
///   disagree: the stamp advances past an undeclared migration without
///   applying it, and `to_latest` never revisits anything below the stamp, so
///   a hole is a migration that would be skipped in silence. A module test
///   asserts that contiguity.
///
/// Coverage is the measured victim set from #724 — the replay-crash set
/// (present above a v0.5.3 stamp, so `to_latest` would replay the `ALTER`),
/// the silent-gap set (absent below a too-high stamp, so the stamp claims them
/// done while the object is missing), the window's hole-closers (checkable
/// migrations between the two, declared so the stamp can advance across them
/// without skipping), and the two migrations the hand-written guards covered.
///
/// #774 extended the window from HEAD 51 to HEAD 59. The #1401 class was alive
/// one band higher than #724 covered: a database stamped 55 by the PRE-re-sort
/// list carries `cron_jobs.run_once` (55 then) while the four migrations the
/// re-sort inserted below it are absent, so replay died on `duplicate column
/// name: run_once` and 55–58 were skipped in silence. Declaring 52–59 lets pass
/// 1 advance the stamp past the present `run_once` and pass 2 fill the gaps
/// beneath it. The span is kept CONTIGUOUS for the same reason as before: an
/// undeclared migration inside it is stepped over and never applied.
pub(crate) const MIGRATION_EFFECTS: &[(&str, Effect)] = &[
    // --- Replay-crash set: the effect is PRESENT above a v0.5.3 stamp, so the
    //     walk must stamp past the migration instead of letting `to_latest`
    //     replay an `ALTER TABLE ... ADD COLUMN` that already ran. ---
    //
    // HEAD 46 — `20260912000001_add_project_repo_remote.sql`. A uv=45 database
    // dies here: `duplicate column name: repo_remote`.
    (
        "ALTER TABLE projects ADD COLUMN repo_remote TEXT;",
        Effect::Column("projects", "repo_remote"),
    ),
    (
        "CREATE INDEX IF NOT EXISTS idx_projects_repo_remote",
        Effect::Index("idx_projects_repo_remote"),
    ),
    // HEAD 51 — `20260915000001_add_cron_trigger_pipeline.sql`. A database
    // hand-stamped 47-50 dies here: `duplicate column name: trigger_cmd`.
    (
        "ALTER TABLE cron_jobs ADD COLUMN trigger_cmd TEXT;",
        Effect::Column("cron_jobs", "trigger_cmd"),
    ),
    (
        "ALTER TABLE cron_jobs ADD COLUMN trigger_on TEXT DEFAULT 'non_empty';",
        Effect::Column("cron_jobs", "trigger_on"),
    ),
    (
        "ALTER TABLE cron_jobs ADD COLUMN set_goal INTEGER NOT NULL DEFAULT 0;",
        Effect::Column("cron_jobs", "set_goal"),
    ),
    (
        "ALTER TABLE cron_jobs ADD COLUMN goal_template TEXT;",
        Effect::Column("cron_jobs", "goal_template"),
    ),
    // --- Silent-gap set: the effect is ABSENT below a too-high stamp, so the
    //     fill-below pass must apply it although `user_version` claims it done.
    //     Without this half the fix would trade a loud failure for a quiet one. ---
    //
    // HEAD 40 — `20260904120000_add_channel_messages_ship_plane.sql`.
    (
        "ALTER TABLE channel_messages ADD COLUMN ship_plane TEXT;",
        Effect::Column("channel_messages", "ship_plane"),
    ),
    // HEAD 41 — `20260905000000_add_pending_tombstones.sql`.
    (
        "CREATE TABLE IF NOT EXISTS pending_tombstones",
        Effect::Table("pending_tombstones"),
    ),
    (
        "CREATE INDEX IF NOT EXISTS idx_pending_tombstones_session",
        Effect::Index("idx_pending_tombstones_session"),
    ),
    // HEAD 42 — `20260906000001_add_notify_queue.sql` (#111). The post-pass
    // `heal_notify_queue` also covers this migration; see the module doc note
    // on overlapping cover.
    (
        "CREATE TABLE IF NOT EXISTS notify_queue",
        Effect::Table("notify_queue"),
    ),
    (
        "CREATE INDEX IF NOT EXISTS idx_notify_queue_session",
        Effect::Index("idx_notify_queue_session"),
    ),
    // HEAD 43 — `20260908000000_add_session_seen_skills.sql` (#138). The
    // measured victim carries the TABLE but not the index: the batch was never
    // fully applied, which is why the declaration is per object.
    (
        "CREATE TABLE IF NOT EXISTS session_seen_skills",
        Effect::Table("session_seen_skills"),
    ),
    (
        "CREATE INDEX IF NOT EXISTS idx_session_seen_skills_session",
        Effect::Index("idx_session_seen_skills_session"),
    ),
    // HEAD 45 — `20260910000000_add_session_seen_skills_epoch.sql` (#150).
    (
        "ALTER TABLE session_seen_skills ADD COLUMN epoch INTEGER NULL;",
        Effect::Column("session_seen_skills", "epoch"),
    ),
    // HEAD 47 — `20260912160000_session_bindings_last_origin.sql` (#180).
    (
        "ALTER TABLE session_bindings ADD COLUMN last_origin TEXT;",
        Effect::Column("session_bindings", "last_origin"),
    ),
    // HEAD 48 — `20260912210000_add_session_seen_skills_active.sql` (#138
    // part 2, #210). This migration adds TWO columns, and the measured victim
    // carries `loaded_mtime` without `active` — the partial case the per-object
    // declaration exists for. The retired active-migration guard used to
    // pre-add `loaded_mtime`; the declaration carries that duty now.
    (
        "ALTER TABLE session_seen_skills ADD COLUMN active INTEGER NOT NULL DEFAULT 0;",
        Effect::Column("session_seen_skills", "active"),
    ),
    (
        "ALTER TABLE session_seen_skills ADD COLUMN loaded_mtime INTEGER;",
        Effect::Column("session_seen_skills", "loaded_mtime"),
    ),
    // HEAD 49 — `20260913000001_session_bindings_turn_open_at.sql` (#200).
    (
        "ALTER TABLE session_bindings ADD COLUMN turn_open_at INTEGER;",
        Effect::Column("session_bindings", "turn_open_at"),
    ),
    // HEAD 50 — `20260914000001_add_whatsapp_newsletter_cursors.sql`. Not a
    // victim: it is here so the declared set has no hole. The re-sort put
    // hazards at 46 and 51 with unapplied migrations between them, and the
    // stamp must cross the whole span — an undeclared migration inside it
    // would be stepped over and never applied by anyone.
    (
        "CREATE TABLE IF NOT EXISTS whatsapp_newsletter_cursors",
        Effect::Table("whatsapp_newsletter_cursors"),
    ),
    // --- The two migrations the hand-written guards covered. ---
    //
    // HEAD 44 — `20260908000001_pending_requests_thread_id.sql` (#1401): the
    // first instance of the class, guarded until #724 by a hand-written guard
    // that this table replaced.
    (
        "ALTER TABLE pending_requests ADD COLUMN channel_thread_id TEXT;",
        Effect::Column("pending_requests", "channel_thread_id"),
    ),
    // --- The 52–59 band: the far side of the same re-sort (#774). ---
    //
    // The window above stopped at HEAD 51, which left the #1401 class alive one
    // band higher. On the PRE-re-sort list `run_once` sat at index 55, so a
    // database stamped 55 by that build carries the one-shot flag while
    // `decision_cache` (55), `decision_stats` (56), `sessions.channel_chat_key`
    // (57) and the audit pair (58) — all inserted BELOW it when the list was
    // re-sorted by filename — are absent. A window ending at 51 cannot see any
    // of them: pass 1 finds no declared migration above the stamp, so the stamp
    // stays at 55, `to_latest` replays `ALTER TABLE cron_jobs ADD COLUMN
    // run_once` and startup dies on `duplicate column name: run_once`, while
    // 55–58 are never applied. Declaring the band lets pass 1 advance the stamp
    // past the present `run_once` and pass 2 fill the four absent migrations
    // below it. Every object here is also a hole-closer: an undeclared
    // migration inside the span would be stepped over in silence.
    //
    // HEAD 52 — `20260918000001_add_goal_criteria.sql` (#299). Two columns;
    // the judge's declared criteria and the consecutive-Uncertain counter that
    // parks a non-converging goal.
    (
        "ALTER TABLE goal_state ADD COLUMN criteria TEXT;",
        Effect::Column("goal_state", "criteria"),
    ),
    (
        "ALTER TABLE goal_state ADD COLUMN consecutive_uncertain INTEGER NOT NULL DEFAULT 0;",
        Effect::Column("goal_state", "consecutive_uncertain"),
    ),
    // HEAD 53 — `20260918000002_add_goal_criterion_evaluations.sql` (#299).
    (
        "ALTER TABLE goal_state ADD COLUMN criterion_evaluations TEXT;",
        Effect::Column("goal_state", "criterion_evaluations"),
    ),
    // HEAD 54 — `20260919000001_session_bindings_await.sql` (#344). The durable
    // await record: what a lane is waiting on when it ends its turn on an
    // EXTERNAL completion. Three columns, so the per-object declaration is what
    // lets a partly-applied migration be completed.
    (
        "ALTER TABLE session_bindings ADD COLUMN await_kind TEXT;",
        Effect::Column("session_bindings", "await_kind"),
    ),
    (
        "ALTER TABLE session_bindings ADD COLUMN await_ref TEXT;",
        Effect::Column("session_bindings", "await_ref"),
    ),
    (
        "ALTER TABLE session_bindings ADD COLUMN await_at INTEGER;",
        Effect::Column("session_bindings", "await_at"),
    ),
    // HEAD 55 — `20260921000001_add_decision_cache.sql` (#1648).
    (
        "CREATE TABLE IF NOT EXISTS decision_cache",
        Effect::Table("decision_cache"),
    ),
    (
        "CREATE INDEX IF NOT EXISTS idx_decision_cache_tier",
        Effect::Index("idx_decision_cache_tier"),
    ),
    // HEAD 56 — `20260921000002_add_decision_stats.sql` (#1648 PR2).
    (
        "CREATE TABLE IF NOT EXISTS decision_stats",
        Effect::Table("decision_stats"),
    ),
    // HEAD 57 — `20260925220000_add_session_channel_chat_key.sql` (#1721). The
    // migration also backfills and dedups before creating its UNIQUE index; only
    // the column and the index are declared, because those are the objects whose
    // presence is checkable. On a database where the column is absent (this
    // victim) the whole migration is applied, so the backfill still runs.
    (
        "ALTER TABLE sessions ADD COLUMN channel_chat_key TEXT;",
        Effect::Column("sessions", "channel_chat_key"),
    ),
    (
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_sessions_channel_chat_key",
        Effect::Index("idx_sessions_channel_chat_key"),
    ),
    // HEAD 58 — `20260926000001_add_audit_turn_retrievals.sql` (#1705). Two
    // tables and their indexes, declared per object.
    (
        "CREATE TABLE IF NOT EXISTS turn_retrievals",
        Effect::Table("turn_retrievals"),
    ),
    (
        "CREATE INDEX IF NOT EXISTS idx_turn_retrievals_session",
        Effect::Index("idx_turn_retrievals_session"),
    ),
    (
        "CREATE TABLE IF NOT EXISTS turn_outcomes",
        Effect::Table("turn_outcomes"),
    ),
    (
        "CREATE INDEX IF NOT EXISTS idx_turn_outcomes_session",
        Effect::Index("idx_turn_outcomes_session"),
    ),
    // HEAD 59 — `20260927000001_add_cron_run_once.sql` (#544). The far-side
    // replay hazard: PRESENT on the victim, which is why the stamp must advance
    // past it rather than let `to_latest` replay the `ADD COLUMN`.
    (
        "ALTER TABLE cron_jobs ADD COLUMN run_once INTEGER NOT NULL DEFAULT 0;",
        Effect::Column("cron_jobs", "run_once"),
    ),
];

/// Does `table` carry a column named `column`?
pub(crate) fn has_column(
    conn: &rusqlite::Connection,
    table: &str,
    column: &str,
) -> rusqlite::Result<bool> {
    let cols: Vec<String> = conn
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |r| r.get::<_, String>(1))?
        .filter_map(Result::ok)
        .collect();
    Ok(cols.iter().any(|c| c == column))
}

fn has_table(conn: &rusqlite::Connection, table: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
}

/// Create `notify_queue` when its migration was skipped.
///
/// The #111 migration carries an earlier filename date than
/// `20260908000001_pending_requests_thread_id.sql`, so merging it in name
/// order places it at an index that databases stamped by the newer build have
/// already passed — `to_latest` never looks at it and the durable notify queue
/// silently does not exist. Mirrors
/// `src/migrations/20260906000001_add_notify_queue.sql`, which stays the source
/// of truth. Returns `true` when it changed the schema.
pub(crate) fn heal_notify_queue(conn: &rusqlite::Connection) -> rusqlite::Result<bool> {
    if has_table(conn, "notify_queue")? {
        return Ok(false);
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS notify_queue (
            id           TEXT PRIMARY KEY NOT NULL,
            session_id   TEXT NOT NULL,
            context_text TEXT NOT NULL,
            display_text TEXT NOT NULL,
            origin       TEXT NOT NULL,
            bg_meta      TEXT,
            created_at   INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_notify_queue_session ON notify_queue(session_id);",
    )?;
    tracing::warn!(
        "Healed notify_queue: the #111 table was missing although the schema was stamped past \
         its migration index (#1401). Parked pushes could not survive a restart until now."
    );
    Ok(true)
}

/// 1-based position of the migration whose SQL contains `marker`.
///
/// The index is load-bearing for `to_latest`. A hand-kept integer goes stale
/// when the fork inserts a migration earlier in filename order (#715): the
/// constant said 41 while `pending_requests_thread_id` sits at 44.
fn migration_index_1based(marker: &str) -> i64 {
    let pos = super::database::MIGRATION_SQL
        .iter()
        .position(|sql| sql.contains(marker))
        .expect("migration marker missing from MIGRATION_SQL");
    pos as i64 + 1
}

/// 0-based position of the migration whose SQL contains `marker`.
fn migration_index_0based(marker: &str) -> usize {
    (migration_index_1based(marker) - 1) as usize
}

/// Does an index with this name exist?
fn has_index(conn: &rusqlite::Connection, index: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
        [index],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
}

/// Is the declared effect already present in the schema?
fn effect_present(conn: &rusqlite::Connection, effect: Effect) -> rusqlite::Result<bool> {
    match effect {
        Effect::Column(table, column) => has_column(conn, table, column),
        Effect::Table(table) => has_table(conn, table),
        Effect::Index(index) => has_index(conn, index),
    }
}

/// `MIGRATION_EFFECTS` resolved to `(0-based index, effect)`.
///
/// Resolved once per pass rather than per lookup: [`migration_index_1based`]
/// scans the whole list, and the walk and fill-below passes ask per migration.
fn resolved_effects() -> Vec<(usize, Effect)> {
    MIGRATION_EFFECTS
        .iter()
        .map(|(marker, effect)| (migration_index_0based(marker), *effect))
        .collect()
}

/// The declared effects of the migration at 0-based `index`.
fn effects_at(declared: &[(usize, Effect)], index: usize) -> Vec<Effect> {
    declared
        .iter()
        .filter(|(i, _)| *i == index)
        .map(|(_, effect)| *effect)
        .collect()
}

/// Split a migration's SQL into individual statements, dropping comment lines.
///
/// A declaration names an OBJECT, not its DDL — the migration file stays the
/// single source of truth — so applying one missing object means finding the
/// statement that creates it. Comments are stripped line-wise BEFORE the split,
/// because a comment may contain a semicolon.
fn statements(sql: &str) -> Vec<String> {
    sql.lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
        .split(';')
        .map(str::trim)
        .filter(|stmt| !stmt.is_empty())
        .map(str::to_string)
        .collect()
}

/// The statement in `sql` that creates `effect`, if the migration carries one.
fn statement_for(sql: &str, effect: Effect) -> Option<String> {
    statements(sql).into_iter().find(|stmt| match effect {
        Effect::Column(_, column) => stmt.contains(&format!("ADD COLUMN {column}")),
        Effect::Table(table) => {
            stmt.contains(&format!("TABLE IF NOT EXISTS {table}"))
                || stmt.contains(&format!("TABLE {table}"))
        }
        Effect::Index(index) => {
            stmt.contains(&format!("INDEX IF NOT EXISTS {index}"))
                || stmt.contains(&format!("INDEX {index}"))
        }
    })
}

/// Apply ONE declared object of the migration at 0-based `index`.
///
/// This is the partially-applied case: some of the migration's objects are
/// already present, so replaying the batch whole would fail on the object that
/// is there. The declaration is per object precisely so the missing one can be
/// applied alone.
fn apply_effect(conn: &rusqlite::Connection, index: usize, effect: Effect) -> rusqlite::Result<()> {
    let sql = crate::db::database::MIGRATION_SQL[index];
    match statement_for(sql, effect) {
        Some(stmt) => conn.execute_batch(&stmt),
        None => {
            // A declaration the migration cannot satisfy is a defect in this
            // module, not a schema state: fail loudly rather than leave the
            // silent shortfall this whole pass exists to prevent. The module
            // test asserts every declared effect has a matching statement.
            tracing::error!(
                "Migration reconciliation: no statement in migration {} creates {effect:?} — \
                 MIGRATION_EFFECTS and the migration file disagree (#724).",
                index + 1
            );
            Err(rusqlite::Error::InvalidQuery)
        }
    }
}

/// One step the reconciliation would take, in the order it would take it.
///
/// The pass is computed as a list before it is executed so the #714
/// pre-migration snapshot predicate can ask whether it would write without
/// performing it ([`reconciliation_would_write`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReconAction {
    /// Move `user_version` to this position in the migration list — the count
    /// of migrations the schema is now reconciled to.
    Stamp(i64),
    /// Apply one declared object of the migration at this 0-based index,
    /// because the rest of that migration is already present.
    ApplyEffect { index: usize, effect: Effect },
    /// Apply the migration at this 0-based index whole — none of its declared
    /// objects is present, so it is being applied for the first time.
    ApplyMigration { index: usize },
}

/// Compute the reconciliation WITHOUT touching the database.
///
/// Two passes, both driven by the schema rather than by the stamp:
///
/// 1. **Locate the stamp.** The stamp must end ABOVE every declared migration
///    at or above it that is already (partly) applied, because `to_latest`
///    re-runs everything from the stamp onward and SQLite rejects a duplicate
///    column or table. The scan does not stop at the first absent migration —
///    the re-sort interleaves applied and unapplied ones, so stopping early
///    only moves the crash along.
/// 2. **Fill below.** Every declared migration under the final stamp that is
///    not fully applied is applied — whole when none of its objects is
///    present, object-by-object when some are. This is what makes the advance
///    in pass 1 safe, and it is the half no guard ever covered: a too-HIGH
///    stamp passes `to_latest` by and leaves the schema short, silently. It is
///    also the generalisation of what the retired active-migration guard did
///    for `loaded_mtime`.
///
/// The safety property of the guards it replaces is preserved: no migration is
/// ever SKIPPED. The stamp may move past a migration whose effect is absent,
/// but only because pass 2 applies it first — after both passes, every
/// migration below the stamp is applied and every declared migration at or
/// above it is wholly absent.
fn reconciliation_actions(
    conn: &rusqlite::Connection,
    user_version: i64,
) -> rusqlite::Result<Vec<ReconAction>> {
    let total = crate::db::database::MIGRATION_SQL.len() as i64;
    let declared = resolved_effects();
    let mut actions = Vec::new();
    // Where the walk begins. `start` keeps the original stamp so the final
    // `Stamp` action can tell whether anything moved at all; pass 1 advances
    // `stamp` from here, and pass 2 fills the declared gaps left under it.
    let start = user_version.clamp(0, total);
    let mut stamp = start;

    // Pass 1 — how far the stamp has to move.
    //
    // `to_latest` re-runs every migration from the stamp onward, so a DECLARED
    // migration at or above the stamp that is already (partly) applied is a
    // replay hazard: SQLite rejects the duplicate column or table and startup
    // dies. The stamp must therefore end up ABOVE the last such migration.
    //
    // The scan must NOT stop at the first ABSENT migration, because the re-sort
    // interleaves applied and unapplied ones — measured on the #724 fixture
    // (ARM A, uv=45): applied at 45 and 50, unapplied at 46, 47 and 48.
    // Stopping at 46 would only move the crash from the first hazard to the
    // next, since `to_latest` would then replay 50.
    //
    // Only DECLARED migrations are visible here. An undeclared one cannot be
    // checked, so it can be neither advanced past nor applied below — which is
    // why the declared set must have no holes across the window where the old
    // and new lists disagree. `declared_effects_cover_the_whole_window` asserts
    // that contiguity, and pass 2 relies on it.
    for index in start as usize..total as usize {
        let effects = effects_at(&declared, index);
        let mut applied = false;
        for effect in effects {
            if effect_present(conn, effect)? {
                applied = true;
                break;
            }
        }
        if applied {
            stamp = index as i64 + 1;
        }
    }

    // Pass 2 — fill below.
    //
    // Every declared migration under the final stamp that is not fully applied
    // is applied here, and this is what makes pass 1's advance SAFE: the stamp
    // may step over a migration whose objects are absent (because a LATER one
    // is present), but only on the strength of applying it now. It is also the
    // half no guard ever covered — a too-HIGH stamp passes `to_latest` by and
    // leaves the schema short, silently.
    //
    // A migration with NONE of its objects present is applied whole; one with
    // SOME present gets only its missing objects, so the batch cannot fail on
    // an object that is already there.
    for index in 0..stamp as usize {
        let effects = effects_at(&declared, index);
        if effects.is_empty() {
            continue;
        }
        let mut missing = Vec::new();
        for effect in effects.iter().copied() {
            if !effect_present(conn, effect)? {
                missing.push(effect);
            }
        }
        if missing.is_empty() {
            continue;
        }
        if missing.len() == effects.len() {
            // Nothing of it is present: run the migration as written.
            actions.push(ReconAction::ApplyMigration { index });
        } else {
            // Partially applied: apply only the objects that are missing, so
            // the batch cannot fail on an object that is already there.
            for effect in missing {
                actions.push(ReconAction::ApplyEffect { index, effect });
            }
        }
    }

    // The stamp moves LAST, after the fills it authorises: an interrupted pass
    // then leaves the old stamp, and the next boot re-derives the whole
    // reconciliation from the schema rather than trusting a stamp whose work
    // did not finish.
    if stamp != start {
        actions.push(ReconAction::Stamp(stamp));
    }

    Ok(actions)
}

/// Reconcile the version stamp against the schema, BEFORE `to_latest` (#724).
///
/// `rusqlite_migration` applies migrations by list INDEX, so `user_version` is
/// a list-relative position and not a migration identity. Inserting a migration
/// with an earlier filename shifts every later index, and a database stamped by
/// the old list then maps its count onto a DIFFERENT migration. Two directions,
/// two symptoms:
///
/// - the stamp lands **below** a migration whose effect is present, so
///   `to_latest` replays an `ALTER TABLE ... ADD COLUMN` that already ran and
///   SQLite rejects the duplicate column — the whole startup dies, not one
///   migration. Measured twice before this: `pending_requests.channel_thread_id`
///   (#1401) and `session_seen_skills.active` (#209, #212).
/// - the stamp lands **above** a migration whose effect is absent, so
///   `to_latest` never looks at it and the schema stays short — silently.
///   Measured at v0.5.3 → HEAD (#724): seven fork-only migrations sorted below
///   the v0.5.3 tail, leaving `channel_messages.ship_plane`, the
///   `pending_tombstones` table and three indexes unapplied behind a stamp that
///   claimed them done.
///
/// It runs before `to_latest` because the crash it prevents happens inside it —
/// there is no "after" to heal from. The writes are not wrapped in an explicit
/// transaction: `to_latest` opens its own, and a pass interrupted part-way is
/// repaired by the next boot, which is schema-checked and idempotent.
///
/// Returns `true` when it wrote.
pub(crate) fn reconcile_before_migrations(
    conn: &rusqlite::Connection,
    user_version: i64,
) -> rusqlite::Result<bool> {
    let actions = reconciliation_actions(conn, user_version)?;
    if actions.is_empty() {
        return Ok(false);
    }
    for action in &actions {
        match *action {
            ReconAction::Stamp(stamp) => {
                conn.pragma_update(None, "user_version", stamp)?;
                tracing::warn!(
                    "Migration reconciliation: stamped user_version to {stamp} — the migration \
                     at that position had already run under an older list order (#724)."
                );
            }
            ReconAction::ApplyEffect { index, effect } => {
                apply_effect(conn, index, effect)?;
                tracing::warn!(
                    "Migration reconciliation: applied {effect:?} from migration {} — the version \
                     stamp disagreed with the schema (#724).",
                    index + 1
                );
            }
            ReconAction::ApplyMigration { index } => {
                conn.execute_batch(crate::db::database::MIGRATION_SQL[index])?;
                tracing::warn!(
                    "Migration reconciliation: applied migration {} in full — it sits below the \
                     version stamp but its effect was absent (#724).",
                    index + 1
                );
            }
        }
    }
    Ok(true)
}

/// Would [`reconcile_before_migrations`] write on this connection (#714)?
pub(crate) fn reconciliation_would_write(
    conn: &rusqlite::Connection,
    user_version: i64,
) -> rusqlite::Result<bool> {
    Ok(!reconciliation_actions(conn, user_version)?.is_empty())
}

/// True when a heal below would write on this connection (#714).
///
/// The predicates are the same early-outs as the heal functions. A database
/// whose `user_version` already equals the list length can still need a
/// pre-write snapshot when one of these is true.
pub(crate) fn heals_would_write(
    conn: &rusqlite::Connection,
    user_version: i64,
) -> rusqlite::Result<bool> {
    // The reconciliation pass runs before `to_latest` and rewrites the schema
    // whenever the stamp disagrees with it, so it is a writer here too (#724).
    if reconciliation_would_write(conn, user_version)? {
        return Ok(true);
    }
    if has_table(conn, "pending_requests")? && !has_column(conn, "pending_requests", "origin")? {
        return Ok(true);
    }
    if !has_table(conn, "notify_queue")? {
        return Ok(true);
    }
    if has_table(conn, "projects")? && !has_column(conn, "projects", "repo_remote")? {
        return Ok(true);
    }
    if has_table(conn, "session_seen_skills")?
        && !has_column(conn, "session_seen_skills", "loaded_mtime")?
    {
        return Ok(true);
    }
    if has_table(conn, "tool_executions")? && !has_column(conn, "tool_executions", "exit_code")? {
        return Ok(true);
    }
    Ok(false)
}

/// Add `pending_requests.origin` when migration 37 was skipped.
///
/// Mirrors `src/migrations/20260828000001_pending_requests_origin.sql`, which
/// stays the source of truth. Returns `true` when it changed the schema.
pub(crate) fn heal_pending_requests_origin(conn: &rusqlite::Connection) -> rusqlite::Result<bool> {
    if !has_table(conn, "pending_requests")? || has_column(conn, "pending_requests", "origin")? {
        return Ok(false);
    }
    conn.execute_batch(
        "ALTER TABLE pending_requests ADD COLUMN origin TEXT NOT NULL DEFAULT 'user';",
    )?;
    tracing::warn!(
        "Healed pending_requests: the origin column of migration 37 was missing although the \
         schema was stamped past it (#1401). Restart recovery could not record turns until now."
    );
    Ok(true)
}

/// Add `projects.repo_remote` and its index when migration 46 was skipped on upstream builds.
///
/// The number was written as 48 when this heal landed (cc961a081, 2026-09-13) and
/// has been wrong since: `20260912000001_add_project_repo_remote.sql` sat at
/// 1-based 46 both then and at HEAD. Corrected here because the module doc above
/// names the same migration by its real position, and two numbers for one
/// migration is how the next reader ends up at the wrong file.
///
/// Mirrors `src/migrations/20260912000001_add_project_repo_remote.sql`.
pub(crate) fn heal_project_repo_remote(conn: &rusqlite::Connection) -> rusqlite::Result<bool> {
    if !has_table(conn, "projects")? || has_column(conn, "projects", "repo_remote")? {
        return Ok(false);
    }
    conn.execute_batch(
        "ALTER TABLE projects ADD COLUMN repo_remote TEXT; \
         CREATE INDEX IF NOT EXISTS idx_projects_repo_remote ON projects(repo_remote);",
    )?;
    tracing::warn!(
        "Healed projects: repo_remote was missing although the schema was stamped past it (#1401, #209)."
    );
    Ok(true)
}

/// Add `session_seen_skills.loaded_mtime` when migration was skipped or partially applied (#210).
pub(crate) fn heal_session_seen_skills_loaded_mtime(
    conn: &rusqlite::Connection,
) -> rusqlite::Result<bool> {
    if !has_table(conn, "session_seen_skills")?
        || has_column(conn, "session_seen_skills", "loaded_mtime")?
    {
        return Ok(false);
    }
    conn.execute_batch("ALTER TABLE session_seen_skills ADD COLUMN loaded_mtime INTEGER;")?;
    tracing::warn!("Healed session_seen_skills: added missing loaded_mtime column (#210).");
    Ok(true)
}

/// Add `tool_executions.exit_code` when migration 60 was skipped (#763).
///
/// Mirrors `src/migrations/20261002000001_add_tool_executions_exit_code.sql`,
/// which stays the source of truth. The migration sits near the list's tail,
/// so an upstream merge that inserts an earlier filename below it shifts its
/// index, and a database stamped against the pre-merge list then never runs it
/// — the #1401 class, in the direction `to_latest` cannot see. This heal is the
/// column's only repair.
pub(crate) fn heal_tool_executions_exit_code(
    conn: &rusqlite::Connection,
) -> rusqlite::Result<bool> {
    if !has_table(conn, "tool_executions")? || has_column(conn, "tool_executions", "exit_code")? {
        return Ok(false);
    }
    conn.execute_batch("ALTER TABLE tool_executions ADD COLUMN exit_code INTEGER;")?;
    tracing::warn!(
        "Healed tool_executions: the exit_code column of migration 60 was missing although the \
         schema was stamped past it (#763)."
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_effects_resolve_to_the_measured_indices() {
        // The indices below are the #724 MEASUREMENT — a 59-migration fork
        // list. They stay literal on purpose: they are the arithmetic this
        // design was built against, and deriving them from `MIGRATION_SQL` is
        // what makes a re-sort surface HERE instead of as a wrong stamp at
        // boot. A failure means the list moved under the design, which is the
        // event the design exists to survive.
        let index_of = migration_index_1based;
        assert_eq!(
            index_of("ALTER TABLE pending_requests ADD COLUMN channel_thread_id TEXT;"),
            44
        );
        assert_eq!(
            index_of("ALTER TABLE projects ADD COLUMN repo_remote TEXT;"),
            46
        );
        assert_eq!(
            index_of(
                "ALTER TABLE session_seen_skills ADD COLUMN active INTEGER NOT NULL DEFAULT 0;"
            ),
            48
        );
        assert_eq!(
            index_of("ALTER TABLE cron_jobs ADD COLUMN trigger_cmd TEXT;"),
            51
        );
        // The two shifts a v0.5.3 stamp is measured against: `repo_remote` sat
        // at 42 then, `cron_trigger_pipeline` at 45. If either fires, the shift
        // table in the issue's measured evidence is stale.
        assert_ne!(
            index_of("ALTER TABLE projects ADD COLUMN repo_remote TEXT;"),
            42
        );
        assert_ne!(
            index_of("ALTER TABLE cron_jobs ADD COLUMN trigger_cmd TEXT;"),
            45
        );
    }

    /// The declaration table is only trustworthy if every row resolves, no row
    /// is ambiguous, and no declared object is dropped later in the list.
    ///
    /// A marker that matches an EARLIER migration would silently point the
    /// reconciliation at the wrong index, which is the very failure the table
    /// exists to prevent — so uniqueness is asserted, not assumed.
    #[test]
    fn declared_effects_resolve_and_are_never_dropped_later() {
        // Resolve every marker, asserting it matches exactly one migration.
        let mut declared: Vec<(usize, &str, Effect)> = Vec::new();
        for (marker, effect) in MIGRATION_EFFECTS {
            let hits: Vec<usize> = crate::db::database::MIGRATION_SQL
                .iter()
                .enumerate()
                .filter(|(_, sql)| sql.contains(marker))
                .map(|(i, _)| i)
                .collect();
            assert_eq!(
                hits.len(),
                1,
                "declared marker {marker:?} must appear in exactly one migration, found at \
                 1-based indices {:?}",
                hits.iter().map(|i| i + 1).collect::<Vec<_>>()
            );
            declared.push((hits[0], *marker, *effect));
        }

        // No declared object name is listed twice (a copy-paste guard: two rows
        // for the same object would make the second a silent no-op).
        let mut tables: Vec<&str> = Vec::new();
        let mut indexes: Vec<&str> = Vec::new();
        for (_, _, effect) in &declared {
            match *effect {
                Effect::Table(t) => tables.push(t),
                Effect::Index(i) => indexes.push(i),
                Effect::Column(_, _) => {}
            }
        }
        for (kind, names) in [("table", &mut tables), ("index", &mut indexes)] {
            let before = names.len();
            names.sort_unstable();
            names.dedup();
            assert_eq!(
                names.len(),
                before,
                "a {kind} is declared more than once in MIGRATION_EFFECTS"
            );
        }

        // No declared object is dropped by a LATER migration: the fill-below
        // pass would resurrect it. `plans` / `plan_tasks` are dropped at index
        // 30, which is exactly why nothing below it may be declared.
        for (idx, marker, effect) in &declared {
            let later = crate::db::database::MIGRATION_SQL[*idx + 1..].join("\n");
            match *effect {
                Effect::Table(t) => assert!(
                    !later.contains(&format!("DROP TABLE IF EXISTS {t}"))
                        && !later.contains(&format!("DROP TABLE {t}")),
                    "declared table {t:?} (marker {marker:?}) is dropped by a later migration"
                ),
                Effect::Index(i) => assert!(
                    !later.contains(&format!("DROP INDEX IF EXISTS {i}"))
                        && !later.contains(&format!("DROP INDEX {i}")),
                    "declared index {i:?} (marker {marker:?}) is dropped by a later migration"
                ),
                Effect::Column(_, c) => assert!(
                    !later.contains(&format!("DROP COLUMN {c}")),
                    "declared column {c:?} (marker {marker:?}) is dropped by a later migration"
                ),
            }
        }

        // Every declared effect must have a statement in its OWN migration that
        // creates it: `apply_effect` applies a missing object by extracting that
        // statement, and a declaration the migration cannot satisfy is a defect
        // in this module rather than a schema state — so it is asserted here
        // instead of discovered as a failed boot.
        for (idx, marker, effect) in &declared {
            assert!(
                statement_for(crate::db::database::MIGRATION_SQL[*idx], *effect).is_some(),
                "no statement in migration {} creates {effect:?} (marker {marker:?})",
                idx + 1
            );
        }
    }

    /// The declared set must have NO HOLES across the window it spans.
    ///
    /// Pass 1 advances the stamp past every DECLARED migration that is already
    /// applied; pass 2 then fills every declared migration left below the final
    /// stamp. A declared migration is therefore either applied below the stamp
    /// or wholly absent above it. An UNDECLARED migration inside that span is
    /// neither: the stamp steps over it and `to_latest` never revisits anything
    /// below the stamp, so it is skipped in silence — the exact failure class
    /// #724 exists to remove, reintroduced by the fix itself.
    ///
    /// Asserted statically: the declared indices form ONE contiguous run, and
    /// that run BRIDGES the replay hazards. A span that stopped short of the
    /// second hazard could not legally cross from the first to it, and the
    /// crash would merely move along instead of going away.
    #[test]
    fn declared_effects_cover_the_whole_window() {
        let mut indices: Vec<usize> = resolved_effects().iter().map(|(i, _)| *i).collect();
        indices.sort_unstable();
        indices.dedup();
        assert!(!indices.is_empty(), "the declared set is empty");

        for pair in indices.windows(2) {
            assert_eq!(
                pair[1],
                pair[0] + 1,
                "hole in the declared set: 0-based index {} (migration {}) is undeclared, so \
                 the stamp would step over it and `to_latest` would never apply it",
                pair[0] + 1,
                pair[0] + 2
            );
        }

        // The span has to contain every hazard and every hole-closer the walk
        // must cross, or the stamp cannot reach the far side of the re-sort.
        // The last entry is the far-side replay hazard #774 added: it is
        // PRESENT on the victim, so a span that stopped short of it would leave
        // the stamp below it and let `to_latest` replay the `ADD COLUMN`.
        let last = *indices.last().expect("non-empty, asserted above");
        let span = indices[0]..=last;
        for marker in [
            "ALTER TABLE projects ADD COLUMN repo_remote TEXT;",
            "ALTER TABLE cron_jobs ADD COLUMN trigger_cmd TEXT;",
            "CREATE TABLE IF NOT EXISTS whatsapp_newsletter_cursors",
            "ALTER TABLE cron_jobs ADD COLUMN run_once INTEGER NOT NULL DEFAULT 0;",
        ] {
            let idx = migration_index_0based(marker);
            assert!(
                span.contains(&idx),
                "0-based index {idx} of {marker:?} is outside the declared span {span:?}: the \
                 walk cannot cross the hazard it is declared for"
            );
        }
    }
}
