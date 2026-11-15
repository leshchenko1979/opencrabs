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
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::config::PreflightMode;

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
        if let (Ok(src), Ok(dst)) = (std::fs::metadata(dated), std::fs::metadata(latest))
            && src.dev() == dst.dev()
            && src.ino() == dst.ino()
        {
            return Ok(());
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
    //
    // #835: this is also the full-page probe now, so a multi-gigabyte image
    // spends minutes here with nothing else on the log. Bracketing it with a
    // start and a finish line is what makes a slow copy distinguishable from a
    // hang — the incident that produced this module had neither, and the
    // operator had no way to tell which stage was stalled.
    let started = std::time::Instant::now();
    tracing::info!(
        "Pre-migration snapshot: VACUUM INTO {} (user_version {user_version})",
        dated.display()
    );
    // #845: `VACUUM INTO` writes its destination as it goes, so a failure
    // leaves the file behind at whatever size it reached — measured on a
    // damaged image, a 0-byte file carrying a name that sorts NEWEST. Removing
    // it here is safe by construction: the loop above picked a name that did
    // not exist, so this path is ours and nothing else can be holding it.
    // `newest_snapshot` filters such a leftover out as well, because a process
    // killed mid-vacuum never reaches this line.
    if let Err(e) = conn.execute("VACUUM INTO ?1", [&dated.to_string_lossy()]) {
        let _ = std::fs::remove_file(&dated);
        return Err::<Option<PathBuf>, _>(e)
            .with_context(|| format!("VACUUM INTO {}", dated.display()));
    }
    tracing::info!(
        "Pre-migration snapshot written: {} ({} bytes in {:.1}s)",
        dated.display(),
        file_len(&dated),
        started.elapsed().as_secs_f64()
    );

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
    // (#717). Unusable files are skipped rather than reported (#845).
    collect_dated(dir)
        .into_iter()
        .filter(|p| snapshot_is_usable(p.as_path()))
        .next_back()
}

/// Whether a dated snapshot is a file an operator could actually restore from.
///
/// #845: a failed `VACUUM INTO` leaves its destination behind at whatever size
/// it reached — measured on a damaged image, a **0-byte** file under a
/// perfectly valid snapshot name. The name is not evidence of the content, and
/// the name is exactly what [`collect_dated`] sorts on, so the newest *name*
/// can be the one file that restores nothing. Without this filter `doctor`
/// answers the one question an operator asks mid-incident by pointing at an
/// empty file.
///
/// Reuses [`check_header`] rather than inventing a second notion of "looks like
/// a database": it is the same 101-byte read the pre-migration path already
/// trusts, so the two cannot drift. A 0-byte file is excluded explicitly
/// because `check_header` deliberately ACCEPTS one — a fresh install is not a
/// torn image, but a fresh install is also not a snapshot.
fn snapshot_is_usable(path: &Path) -> bool {
    let Some(path_str) = path.to_str() else {
        return false;
    };
    file_len(path) >= HEADER_PROBE_LEN as u64 && check_header(path_str).is_ok()
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
    // #845: the hint makes a promise ("restore from this"), so it must not be
    // satisfied by a file that only exists. A 0-byte leftover from a failed
    // `VACUUM INTO` is exactly the shape that exists and restores nothing, and
    // this message is read mid-incident over SSH by someone who will believe it.
    let hint = if snapshot_is_usable(&latest) {
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
        // `VACUUM INTO` is the full-page probe (#835), so a damaged image now
        // surfaces HERE rather than in the header check. Keep the refusal the
        // operator needs: whether the image was already broken is what decides
        // their next action, not which stage noticed it.
        Err(e) if is_corrupt(&e) => bail!(
            "{}",
            refuse(
                "the pre-migration integrity check failed",
                dir,
                &e.to_string()
            )
        ),
        Err(e) => bail!("{}", refusal_message(dir, &e.to_string())),
    }
}

/// Whether an error chain carries SQLite's own corruption verdict.
///
/// `VACUUM INTO` fails on a damaged image and `rusqlite` reports that as
/// `SqliteFailure(DatabaseCorrupt)`. Distinguishing it from a disk-full or lock
/// failure is what preserves the two refusal messages this module exists for:
/// "the pre-migration integrity check failed" (the image was already broken)
/// versus "the pre-migration snapshot failed" (the copy could not be made).
/// Collapsing them is what made the rpi5 receipt useless for two days.
fn is_corrupt(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ffi::ErrorCode::DatabaseCorrupt,
                    ..
                },
                _,
            ))
        )
    })
}

/// Bytes read to validate the header: the 100-byte SQLite header plus the
/// first byte of page 1, which carries the b-tree page type.
const HEADER_PROBE_LEN: usize = 101;

/// Validate the SQLite file header without walking a single page (#835).
///
/// Three facts settle whether this file can be a database at all: the 16-byte
/// magic, a legal page size, and a legal b-tree page type on page 1. A torn or
/// zeroed header fails one of them immediately. A file damaged anywhere later
/// passes, which is deliberate: `VACUUM INTO` is the probe for that, and it is
/// work the startup path has to do anyway.
fn check_header(path: &str) -> Result<()> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("the database file {path} could not be read"))?;
    let len = file
        .metadata()
        .map(|m| m.len())
        .with_context(|| format!("stat {path}"))?;
    // Zero bytes is a FRESH install, not a torn image: `deadpool_sqlite` creates
    // the file before SQLite writes page 1, and `run_migrations` must be allowed
    // to migrate it. The check exists to catch damage; there is none here.
    if len == 0 {
        return Ok(());
    }
    if len < HEADER_PROBE_LEN as u64 {
        bail!("the file is {len} bytes, too short to hold a SQLite header");
    }
    let mut buf = [0u8; HEADER_PROBE_LEN];
    file.read_exact(&mut buf)
        .with_context(|| format!("read the first {} bytes of {path}", HEADER_PROBE_LEN))?;

    if &buf[..16] != b"SQLite format 3\0" {
        bail!("the file does not begin with the SQLite magic");
    }

    // 1 means 65536, which the 16-bit field cannot hold; every other legal
    // value is a power of two from 512 to 32768.
    let page_size = u16::from_be_bytes([buf[16], buf[17]]);
    let page_size_ok =
        page_size == 1 || (page_size.is_power_of_two() && (512..=32768).contains(&page_size));
    if !page_size_ok {
        bail!("the header declares an illegal page size of {page_size} bytes");
    }

    // Page 1 holds the schema table: 0x0D as a leaf, 0x05 once it grows an
    // interior page. 0x02/0x0A are index pages and cannot legally be page 1,
    // but accepting them costs nothing, and refusing a shape SQLite tolerates
    // would be worse than the opposite.
    let page_type = buf[HEADER_PROBE_LEN - 1];
    if !matches!(page_type, 0x02 | 0x05 | 0x0a | 0x0d) {
        bail!("page 1 declares b-tree page type 0x{page_type:02x}, which is not a legal page type");
    }

    Ok(())
}

/// Check the image before any migration can write to it (#1779 defect 2, #835).
///
/// Deliberately NOT routed through the pool. `post_create` applies
/// `PRAGMA journal_mode = WAL`, which is a *write*, so on a torn image the pool
/// cannot produce a connection at all, and a check that needs one is unreachable
/// in the exact incident it exists to catch. The rpi5 receipt is literally
/// "post_create hook failed: database disk image is malformed", emitted from
/// `pool.get()` before a single line of migration code ran. So this opens its
/// own read-write connection to the file instead — and the default mode does
/// not even need that: it reads the raw bytes with `std::fs`.
///
/// The default mode reads 101 bytes and stops. Measured 2026-11-15 on scratch
/// copies: the rpi5 shape (bytes 100..200 zeroed) makes `integrity_check`,
/// `quick_check` AND `VACUUM INTO` fail at *prepare* — all three detect it
/// without reading a page. So a full `integrity_check` buys nothing for that
/// shape and costs a complete page walk on every boot that needs a snapshot
/// (~33 min on the 3.7 GiB ops image at the measured ~2 MB/s). `VACUUM INTO`,
/// which this function precedes, is the real full-page probe; [`guard`] reports
/// the corruption it finds with this same refusal wording.
///
/// `PreflightMode::Full` restores the old scan for forensic use, and `Off`
/// skips the check while leaving the vacuum as the probe.
pub fn integrity_preflight(path: &str, dir: &Path, mode: PreflightMode) -> Result<()> {
    if mode == PreflightMode::Off {
        return Ok(());
    }
    // A path with no file behind it is a FRESH install, not a torn image: at
    // this point in startup a brand-new database legitimately has no bytes yet,
    // and `SQLITE_OPEN_READ_WRITE` refuses to create one, so the check would
    // report "unable to open database file" and refuse every first boot.
    if !std::path::Path::new(path).exists() {
        return Ok(());
    }

    let cause = match check_header(path) {
        Err(e) => Some(format!("{e:#}")),
        Ok(()) if mode == PreflightMode::Full => {
            match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE) {
                Err(e) => Some(format!("the database could not be opened ({e})")),
                Ok(conn) => {
                    // A damaged image reports as an `Err` from the pragma, not
                    // as a row that says so (verified 2026-09-28), so `Err` is a
                    // verdict here and must refuse rather than escape as if the
                    // plumbing broke.
                    match conn.pragma_query_value(None, "integrity_check", |row| {
                        row.get::<_, String>(0)
                    }) {
                        Ok(report) if report == "ok" => None,
                        Ok(report) => Some(format!("integrity_check reported {report:?}")),
                        Err(e) => Some(format!("integrity_check could not run ({e})")),
                    }
                }
            }
        }
        Ok(()) => None,
    };

    match cause {
        None => Ok(()),
        Some(cause) => bail!(
            "{}",
            refuse("the pre-migration integrity check failed", dir, &cause)
        ),
    }
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
