//! Pre-migration image snapshot (#1779).
//!
//! A migration is a *write*. When the image already carries damage, the
//! migration turns a readable-but-broken database into a destroyed one: the
//! rpi5 incident (arfonzo, 2026-09-27) lost every cron row because `/evolve`
//! restarted the daemon, `run_migrations` ran `ALTER TABLE` against a torn
//! page 1, and the only copy of the data was the file being overwritten.
//!
//! So: copy the whole image *before* anything touches it, and refuse to
//! migrate when that copy cannot be made. A failed snapshot on a non-empty
//! database means the image is either unreadable or the disk is unusable, and
//! both cases migrating is the worst possible next action.
//!
//! Retention is 7 dated copies for small databases (owner directive 2026-09-28:
//! "rolling 7 days"), and also a byte budget so a multi-gigabyte profile cannot
//! keep seven full images (#717). At least one dated copy is always kept.
//! `-latest` is a hard link to the newest dated file, not a second full copy
//! (#716), so a message, a script or a panicked user can always point at
//! exactly one name.

use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

/// Dated snapshots kept before the oldest is pruned.
pub const RETENTION: usize = 7;

/// Byte budget for the dated copies (#717).
///
/// Seven copies of the ops profile (about 2.44 GiB each) are a third of a
/// 58 GiB disk. 3 GiB admits one such image and not a second. One file larger
/// than the budget is kept: the floor is a single known-good copy.
pub const RETENTION_BYTES: u64 = 3 * 1024 * 1024 * 1024;

/// Prefix shared by every snapshot name, so rotation can recognise its own.
pub const PREFIX: &str = "opencrabs.db.pre-migration-";

/// Stable alias pointing at the newest snapshot. Never pruned by rotation.
pub const LATEST: &str = "opencrabs.db.pre-migration-latest";

/// Where snapshots for the active profile are written.
///
/// `crate::config::opencrabs_home()` (not the `types::io` path) because
/// `config::types` is `pub(crate)` with a re-export; every other caller in the
/// crate goes through the re-export, and so does this.
pub fn snapshot_dir() -> PathBuf {
    crate::config::opencrabs_home().join("backups")
}

/// The file a connection is really backed by, or `None` for an in-memory DB.
///
/// `Connection::path()` cannot be used here: it wraps `sqlite3_db_filename`,
/// which strips the query part of a URI, so the `?mode=memory` URIs the test
/// suite connects with would never be recognised and every unit test would
/// write snapshots into the developer's real home.
///
/// `PRAGMA database_list` is the canonical signal: measured on
/// `sqlite3 "file:mem_abc123?mode=memory&cache=shared" "pragma database_list"`,
/// the filename column comes back EMPTY, same as for a plain `:memory:`. The
/// `is_file()` gate is defence in depth for builds whose SQLite reports the
/// memory-db name there instead, and costs one stat.
fn backing_file(conn: &Connection) -> Result<Option<String>> {
    let file: String = conn
        .query_row("PRAGMA database_list", [], |r| r.get(2))
        .context("read PRAGMA database_list")?;
    if file.is_empty() || file == ":memory:" {
        return Ok(None);
    }
    if !Path::new(&file).is_file() {
        return Ok(None);
    }
    Ok(Some(file))
}

/// True when there is nothing on disk worth keeping.
///
/// Two distinct cases: an in-memory database has no file to copy at all, and a
/// brand-new file has no user tables. Both skip *silently* rather than fail.
fn is_empty_or_transient(conn: &Connection) -> Result<bool> {
    if backing_file(conn)?.is_none() {
        return Ok(true);
    }
    let tables: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table'",
            [],
            |r| r.get(0),
        )
        .context("count sqlite_master rows")?;
    Ok(tables == 0)
}

/// `(YYYYMMDD, HHMMSS, same-second suffix)` from a dated snapshot name.
///
/// The version sits in front of the stamp (`{version}-{date}-{time}`), so a
/// lexical sort of the filename is not chronological once `user_version`
/// changes (#717). Unparseable names sort first and are pruned first.
fn chronological_key(path: &Path) -> (u64, u64, u64) {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return (0, 0, 0);
    };
    let Some(rest) = name.strip_prefix(PREFIX) else {
        return (0, 0, 0);
    };
    let parts: Vec<&str> = rest.split('-').collect();
    if parts.len() < 3 || parts[2].len() != 6 {
        return (0, 0, 0);
    }
    let date = parts[1].parse().unwrap_or(0);
    let time = parts[2].parse().unwrap_or(0);
    let collision = parts.get(3).and_then(|n| n.parse().ok()).unwrap_or(0);
    if date == 0 {
        return (0, 0, 0);
    }
    (date, time, collision)
}

fn is_dated_snapshot(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.starts_with(PREFIX) && n != LATEST)
        .unwrap_or(false)
}

/// Dated snapshots, oldest first by the stamp embedded in the name (#717).
fn collect_dated(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dated: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| is_dated_snapshot(p))
        .collect();
    dated.sort_by_key(|p| chronological_key(p));
    dated
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Drop oldest dated copies until both the count window and the byte budget
/// hold. Never drops the last copy.
fn prune_dated(mut dated: Vec<PathBuf>, retention: usize, byte_budget: u64) -> usize {
    let mut removed = 0;
    while dated.len() > 1
        && (dated.len() > retention || dated.iter().map(|p| file_len(p)).sum::<u64>() > byte_budget)
    {
        let old = dated.remove(0);
        match std::fs::remove_file(&old) {
            Ok(()) => removed += 1,
            Err(e) => {
                tracing::warn!("Could not prune stale snapshot {}: {e}", old.display());
                break;
            }
        }
    }
    removed
}

/// Point `-latest` at `dated` without a second full image (#716).
///
/// A hard link shares the dated file's bytes. A cross-device filesystem
/// cannot link, and falls back to a copy so the alias still exists.
fn publish_latest(dated: &Path, latest: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(src), Ok(dst)) = (std::fs::metadata(dated), std::fs::metadata(latest)) {
            if src.dev() == dst.dev() && src.ino() == dst.ino() {
                return Ok(());
            }
        }
    }
    if latest.exists() {
        std::fs::remove_file(latest)?;
    }
    match std::fs::hard_link(dated, latest) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(18) => std::fs::copy(dated, latest).map(|_| ()),
        Err(e) => Err(e),
    }
}

/// Copy the whole image into `dir` before any migration touches it.
///
/// Returns the dated snapshot path, or `None` when the database is empty or
/// in-memory and there is nothing to protect. Errors are fatal to the caller:
/// a non-empty database that cannot be snapshotted must NOT be migrated.
pub fn snapshot_before_migrations(conn: &Connection, dir: &Path) -> Result<Option<PathBuf>> {
    if is_empty_or_transient(conn)? {
        return Ok(None);
    }

    std::fs::create_dir_all(dir)
        .with_context(|| format!("create backups dir {}", dir.display()))?;

    let user_version: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .context("read user_version")?;
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");

    // VACUUM INTO refuses to overwrite, so a same-second second attempt needs a
    // distinct name rather than a failure. Two startups inside one second are
    // rare, but a crash-restart loop makes them routine.
    let mut dated = dir.join(format!("{PREFIX}{user_version}-{stamp}"));
    for n in 2.. {
        if !dated.exists() {
            break;
        }
        dated = dir.join(format!("{PREFIX}{user_version}-{stamp}-{n}"));
    }

    // VACUUM INTO is the only copy that is safe against a live writer: it
    // produces a compact, integrity-checked image in one transaction, where a
    // plain file copy of a WAL database can catch a half-written page.
    conn.execute("VACUUM INTO ?1", [&dated.to_string_lossy()])
        .with_context(|| format!("VACUUM INTO {}", dated.display()))?;

    // Stable alias for the newest snapshot. A hard link, not a second copy:
    // the dated file is the retention unit and must stay put (#716).
    let latest = dir.join(LATEST);
    if let Err(e) = publish_latest(&dated, &latest) {
        tracing::warn!(
            "Snapshot {} written, but the -latest alias could not be updated: {e}",
            dated.display()
        );
    }

    rotate(dir)?;
    Ok(Some(dated))
}

/// Prune dated snapshots beyond the newest [`RETENTION`].
///
/// Never touches the `-latest` alias, and never fails the caller over a prune:
/// the snapshot is already safe, and a cleanup problem is not a reason to
/// refuse a migration.
pub fn rotate(dir: &Path) -> Result<usize> {
    let dated = collect_dated(dir);
    if dated.is_empty() {
        return Ok(0);
    }
    Ok(prune_dated(dated, RETENTION, RETENTION_BYTES))
}

/// The newest dated snapshot in `dir`, or `None` when nothing has been kept.
///
/// Same enumeration as [`rotate`], so the two cannot disagree about what counts
/// as a snapshot: the `-latest` alias is excluded because it is a pointer and
/// not a retention unit, and a report naming it would send an operator to copy
/// a file that silently outlives whatever it points at. Order is the stamp in
/// the name, not the raw filename (#717).
pub fn newest_snapshot(dir: &Path) -> Option<PathBuf> {
    // Same enumeration as [`rotate`], ordered by the embedded stamp rather
    // than the filename: a higher `user_version` is not a later snapshot
    // (#717).
    collect_dated(dir).pop()
}

/// What an operator can restore from, in one clause, for `doctor` and the
/// startup log (#1779 defect 4).
///
/// A pure function over the path rather than a printed line, so the wording is
/// testable without capturing stdout. Before this, `doctor` was the one command
/// an operator reaches for mid-incident and it knew only whether the image was
/// healthy, never whether a copy of it existed.
pub fn newest_snapshot_note(newest: Option<&Path>) -> String {
    match newest {
        Some(path) => format!(
            "{} (copy it over the database file to restore)",
            path.display()
        ),
        None => "none yet: one is written on the next successful startup".to_string(),
    }
}

/// The refusal an operator acts on, worded for the stage that found the damage.
///
/// Explains what to do next, not only what broke.
///
/// `stage` is spelled out per call site instead of sharing one generic phrase
/// because the two stages fail for different reasons, and the reader is usually
/// panicked and reading over SSH: "the pre-migration integrity check failed"
/// says the image was already broken, "the pre-migration snapshot failed" says
/// the copy could not be made. Collapsing them into "something failed" is what
/// made the original rpi5 receipt useless two days into the incident.
fn refuse(stage: &str, dir: &Path, cause: &str) -> String {
    let latest = dir.join(LATEST);
    let hint = if latest.exists() {
        format!(
            "An earlier snapshot is still available at {}.",
            latest.display()
        )
    } else {
        format!("No snapshot exists in {} yet.", dir.display())
    };
    format!(
        "Refusing to run database migrations: {stage} ({cause}). \
         {hint} Nothing has been written to the database. \
         Restore it by copying a snapshot over the database file, or repair the header, \
         then start again. Your brain files and config are untouched."
    )
}

/// Refusal for a snapshot that could not be made.
pub fn refusal_message(dir: &Path, cause: &str) -> String {
    refuse("the pre-migration snapshot failed", dir, cause)
}

/// Guard used by `run_migrations`. `Ok(())` means "safe to migrate".
///
/// `dir` is resolved by the CALLER, on its own task, and never here:
/// `opencrabs_home()` reads a task-local profile override, and this runs inside
/// an `interact` closure on a blocking thread where that override is not set, so
/// resolving it in here would silently write snapshots into the default
/// profile's home no matter which profile is starting up.
/// Whether this process will write the schema, so a snapshot is worth taking.
///
/// The copy exists to protect a migration (#1779). When `user_version` is
/// already at the list length and no heal would `ALTER` or stamp, there is
/// nothing to protect and `VACUUM INTO` only fills the disk (#714). The heal
/// probe is required: a stamp that says latest while a skipped migration's
/// column is still absent is exactly the database the snapshot is for.
pub fn needs_pre_migration_snapshot(conn: &Connection, migration_count: i64) -> Result<bool> {
    if is_empty_or_transient(conn)? {
        return Ok(false);
    }
    let user_version: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .context("read user_version")?;
    if user_version < migration_count {
        return Ok(true);
    }
    crate::db::migration_heal::heals_would_write(conn, user_version)
        .context("probe schema heals before deciding on a snapshot")
}

pub fn guard(conn: &Connection, dir: &Path) -> Result<()> {
    match snapshot_before_migrations(conn, dir) {
        Ok(Some(p)) => {
            tracing::info!("Pre-migration snapshot: {}", p.display());
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(e) => bail!("{}", refusal_message(dir, &e.to_string())),
    }
}

/// Check the image before any migration can write to it (#1779 defect 2).
///
/// Deliberately NOT routed through the pool. `post_create` applies
/// `PRAGMA journal_mode = WAL`, which is a *write*, so on a torn image the pool
/// cannot produce a connection at all, and a check that needs one is unreachable
/// in the exact incident it exists to catch. The rpi5 receipt is literally
/// "post_create hook failed: database disk image is malformed", emitted from
/// `pool.get()` before a single line of migration code ran. So this opens its
/// own read-write connection to the file instead.
///
/// `integrity_check` rather than `quick_check`: quick_check skips the page-ref
/// and freelist walks, and a torn page 1 breaks precisely those. The cost is the
/// same one the post-migration check already paid on every startup.
///
/// A damaged image reports as an `Err` from the pragma, not as a row that says
/// so. Verified 2026-09-28 against a real file: blanking bytes 100..200 (the
/// page-1 b-tree header) leaves the magic intact and makes `integrity_check`
/// fail with SQLITE_CORRUPT instead of returning text, so `Err` is a verdict
/// here and must refuse, never escape as if the plumbing broke.
pub fn integrity_preflight(path: &str, dir: &Path) -> Result<()> {
    // A path with no file behind it is a FRESH install, not a torn image.
    // `deadpool_sqlite` opens lazily, so at this point in startup a brand-new
    // database legitimately has no bytes yet — and `SQLITE_OPEN_READ_WRITE`
    // refuses to create one, so the check would report "unable to open
    // database file" and refuse every first boot. The check exists to catch a
    // damaged image; there is nothing here to damage.
    if !std::path::Path::new(path).exists() {
        return Ok(());
    }
    let cause = match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE) {
        Err(e) => format!("the database could not be opened ({e})"),
        Ok(conn) => {
            match conn.pragma_query_value(None, "integrity_check", |row| row.get::<_, String>(0)) {
                Ok(report) if report == "ok" => return Ok(()),
                Ok(report) => format!("integrity_check reported {report:?}"),
                Err(e) => format!("integrity_check could not run ({e})"),
            }
        }
    };
    bail!(
        "{}",
        refuse("the pre-migration integrity check failed", dir, &cause)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The alias must never match the dated-name filter, or rotation would
    /// delete the one file the refusal message promises.
    #[test]
    fn latest_alias_is_not_a_dated_name() {
        assert!(LATEST.starts_with(PREFIX));
        assert!(
            LATEST
                .trim_start_matches(PREFIX)
                .chars()
                .all(|c| !c.is_ascii_digit()),
            "alias must carry no digits so it is distinguishable from dated copies"
        );
    }

    #[test]
    fn retention_is_the_directed_window() {
        assert_eq!(RETENTION, 7, "owner directive 2026-09-28: rolling 7 days");
    }

    #[test]
    fn retention_bytes_admit_one_ops_sized_image_and_not_two() {
        // Measured 2026-09-30 on the ops profile (issue #717).
        let ops_image = 2_440_159_232u64;
        assert!(
            RETENTION_BYTES >= ops_image,
            "one known-good copy of the ops image must fit"
        );
        assert!(
            RETENTION_BYTES < ops_image * 2,
            "a second full copy must exceed the budget"
        );
    }

    #[test]
    fn byte_budget_keeps_the_newest_by_time_not_by_version() {
        let tmp = tempfile::tempdir().unwrap();
        for name in [
            format!("{PREFIX}55-20260930-010101"),
            format!("{PREFIX}60-20260930-020202"),
            format!("{PREFIX}59-20260930-030303"),
        ] {
            let file = std::fs::File::create(tmp.path().join(&name)).unwrap();
            file.set_len(100).unwrap();
        }
        std::fs::write(tmp.path().join(LATEST), b"alias").unwrap();

        let dated = collect_dated(tmp.path());
        assert!(
            dated
                .last()
                .unwrap()
                .ends_with("opencrabs.db.pre-migration-59-20260930-030303"),
            "03:03 is newer than the version-60 file from 02:02: {dated:?}"
        );

        // 100+100 = 200 > 150, so the two older files go. Count window is 7
        // and would have kept all three.
        let removed = prune_dated(dated, RETENTION, 150);
        assert_eq!(removed, 2);
        let left = collect_dated(tmp.path());
        assert_eq!(left.len(), 1);
        assert!(
            left[0].ends_with("opencrabs.db.pre-migration-59-20260930-030303"),
            "kept {left:?}"
        );
        assert!(
            tmp.path().join(LATEST).exists(),
            "alias is not a dated copy"
        );
    }
}
