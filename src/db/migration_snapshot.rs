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

use anyhow::{Context, Result, bail};
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

/// How old an UNUSABLE dated snapshot must be before the reap deletes it (#1034).
///
/// A killed `VACUUM INTO` leaves its destination behind, and nothing ever
/// reaps it: the #845 removal covers only a RETURNED error, and `rotate` used
/// to run only after a SUCCESSFUL vacuum, so a run of killed attempts
/// accumulated without bound — measured 2026-11-26, nine corpses totalling
/// 3.6 GiB against a 3 GiB budget, every one of them `file is not a database
/// (26)` and none of them restorable.
///
/// The age gate is what keeps the reap from deleting a copy that is still being
/// written by a PEER process: `VACUUM INTO` of the 1.25 GiB ops image measured
/// 752.1 s and 1169.1 s (19.5 min), so nothing legitimate is still mid-copy at
/// 30 minutes. A file under this age is left alone and re-examined next time.
pub const STALE_SNAPSHOT_SECS: u64 = 30 * 60;

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

    // #1034: a killed `VACUUM INTO` leaves its destination behind under a
    // perfectly valid snapshot name, and neither the #845 removal (which covers
    // only a RETURNED error) nor `rotate` (which runs on the success side of
    // the same `if`) ever touches it. So a run of killed attempts accumulates
    // without bound: measured 2026-11-26, nine dated corpses totalling 3.6 GiB
    // on a 58 GiB disk at 92 %, each one answering `file is not a database (26)`
    // to `PRAGMA quick_check` and none of them restorable. Reaping here bounds
    // that population at the one point every startup passes through.
    let reaped = reap_interrupted_snapshots(dir);
    if reaped > 0 {
        tracing::warn!("Reaped {reaped} unusable pre-migration snapshot file(s) before copying");
    }

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
    // #1034: retention that runs only AFTER a successful copy cannot bound a
    // run of failures — the corpses are exactly the population that never
    // reaches the success path. So rotate before the copy too (this is what
    // frees space for the copy that is about to need it), and again in the
    // failure arm beside the #845 removal.
    if let Err(e) = rotate(dir) {
        tracing::warn!(
            "Pre-migration rotation before the copy failed ({}): {e}",
            dir.display()
        );
    }
    if let Err(e) = conn.execute("VACUUM INTO ?1", [&dated.to_string_lossy()]) {
        let _ = std::fs::remove_file(&dated);
        if let Err(rot) = rotate(dir) {
            tracing::warn!(
                "Pre-migration rotation after a failed copy failed ({}): {rot}",
                dir.display()
            );
        }
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

/// Delete dated snapshots that no operator could restore from (#1034).
///
/// The population this bounds is the one nothing else reaps. `VACUUM INTO`
/// writes its destination as it goes, so a process killed mid-copy (a `timeout
/// 25` probe, a daemon restart, a deploy kill) leaves a truncated file under a
/// valid snapshot name; the #845 removal at the call site covers only a
/// RETURNED error, and `rotate` runs on the success side of the same `if`. A
/// series of killed attempts therefore accumulates without bound, and every one
/// of them is unusable: measured 2026-11-26, nine dated files totalling 3.6 GiB
/// on a 58 GiB disk that reached 92 %, each answering `file is not a database
/// (26)` to `PRAGMA quick_check`.
///
/// The enumeration is [`collect_dated`] and the predicate is
/// [`snapshot_is_usable`] — the SAME two the restore path already uses, so the
/// reaper and [`newest_snapshot`] cannot disagree about what is worth keeping.
/// Re-deriving either here is how the two would drift.
///
/// A file younger than [`STALE_SNAPSHOT_SECS`] is left alone: a peer process
/// may be writing it right now, and `VACUUM INTO` of a large image legitimately
/// takes minutes.
///
/// Returns how many files were removed.
pub fn reap_interrupted_snapshots(dir: &Path) -> usize {
    let now = std::time::SystemTime::now();
    let mut removed = 0usize;
    let mut kept_recent = 0usize;
    for path in collect_dated(dir) {
        if snapshot_is_usable(&path) {
            continue;
        }
        let age = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|mtime| now.duration_since(mtime).ok());
        match age {
            Some(age) if age.as_secs() >= STALE_SNAPSHOT_SECS => {
                // Read the size BEFORE the removal: a log line that reports
                // the length of a file it has just deleted is a claim about
                // nothing.
                let bytes = file_len(&path);
                match std::fs::remove_file(&path) {
                    Ok(()) => {
                        tracing::warn!(
                            "Reaped an interrupted pre-migration snapshot (#1034): {} \
                             ({bytes} bytes, unusable, {} min old) — a killed VACUUM INTO left \
                             it behind",
                            path.display(),
                            age.as_secs() / 60
                        );
                        removed += 1;
                    }
                    Err(e) => tracing::warn!(
                        "Could not reap the unusable snapshot {}: {e}",
                        path.display()
                    ),
                }
            }
            Some(_) => kept_recent += 1,
            // No mtime: a file whose age cannot be read is left alone rather
            // than deleted on a guess.
            None => kept_recent += 1,
        }
    }
    if kept_recent > 0 {
        tracing::info!(
            "Kept {kept_recent} unusable snapshot file(s) younger than {} min — possibly still \
             being written",
            STALE_SNAPSHOT_SECS / 60
        );
    }
    removed
}

/// Whether a `-wal`/`-shm` pair is a leftover SQLite should be allowed to
/// recreate from the main file (#1034 ask #4).
///
/// Pure, so every case is one line to test and the caller cannot pass the wrong
/// length by accident. The rule is deliberately narrow, because both over- and
/// under-deleting are destructive:
///
/// - **A `-wal` with bytes in it is NEVER dropped.** Those frames may hold
///   committed transactions that are not in the main file yet, and SQLite
///   rebuilds a missing `-shm` from the WAL by itself. Deleting it would be data
///   loss wearing the shape of a repair.
/// - **A pair another process currently holds open is NEVER dropped.** A daemon
///   that has opened the database and written nothing yet legitimately shows a
///   0-byte `-wal` beside a 32 KiB `-shm` — byte-for-byte the shape of the wedge
///   — and unlinking it would orphan that process's handles and *create* the
///   very failure this exists to clear.
/// - What is left is a 0-byte `-wal` beside a non-empty `-shm` that nobody
///   holds: a pair left behind by a process that died, which SQLite cannot
///   reconcile on its own because the `-shm` index describes a log the path no
///   longer resolves to.
pub fn should_drop_sidecars(wal_len: Option<u64>, shm_len: Option<u64>, held_open: bool) -> bool {
    matches!(wal_len, Some(0)) && shm_len.unwrap_or(0) > 0 && !held_open
}

/// Drop a `-wal`/`-shm` pair that SQLite cannot use, before a pool opens on it.
///
/// #1034 ask #4, measured on the family profile at 2026-11-26T15:06:42Z. The
/// database was never damaged — `integrity_check` and `quick_check` both
/// answered `ok`, `user_version` 67, 3,497 messages intact — and yet EVERY
/// operation failed with `file is not a database (26)` for ten minutes, one
/// failure per cron tick. The cause was the connection, not the file: `lsof` on
/// the daemon showed three DELETED handles (`-wal` holding 4,499,072 bytes,
/// `-shm` holding 32,768) while the on-disk pair was a **0-byte `-wal` beside a
/// 32,768-byte `-shm`**. A connection whose WAL index points at an inode the
/// path no longer resolves to fails every subsequent read the same way, which is
/// why it never self-healed across ten minutes of retries and why only a restart
/// cleared it.
///
/// The repair is to delete BOTH files and let SQLite recreate the pair from the
/// main file: with a 0-byte `-wal` there are no frames outside the main file, so
/// nothing is lost. Deleting only the empty `-wal` would leave a stale `-shm`
/// describing a log that no longer exists — the same wedge again.
///
/// Costs two `stat`s per pool open in the normal case, and a `/proc` scan only
/// in the one shape that could need a repair.
pub fn reconcile_wal_sidecars(db_path: &Path) -> std::io::Result<usize> {
    let wal = sidecar_path(db_path, "-wal");
    let shm = sidecar_path(db_path, "-shm");
    let wal_len = std::fs::metadata(&wal).ok().map(|m| m.len());
    let shm_len = std::fs::metadata(&shm).ok().map(|m| m.len());

    // Cheap rejection BEFORE the `/proc` scan: a WAL with data in it, or no
    // WAL at all, is the ordinary case on every single pool open, and walking
    // every process's fd table to answer a question that cannot apply would be
    // a startup cost paid for nothing.
    if !matches!(wal_len, Some(0)) || shm_len.unwrap_or(0) == 0 {
        return Ok(0);
    }
    let held = held_by_another_process(&wal) || held_by_another_process(&shm);
    if !should_drop_sidecars(wal_len, shm_len, held) {
        return Ok(0);
    }

    let mut removed = 0usize;
    for path in [&wal, &shm] {
        match std::fs::remove_file(path) {
            Ok(()) => removed += 1,
            // Already gone between the stat and the unlink is the outcome we
            // wanted, not a failure.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(std::io::Error::other(format!(
                    "could not remove the inconsistent sidecar {}: {e}",
                    path.display()
                )));
            }
        }
    }
    tracing::warn!(
        "Dropped an inconsistent WAL sidecar pair for {} (0-byte -wal, {} -shm bytes) — SQLite \
         will recreate it from the main file (#1034)",
        db_path.display(),
        shm_len.unwrap_or(0)
    );
    Ok(removed)
}

/// Whether some process other than this one holds `path` open.
///
/// `/proc/<pid>/fd` is read as SYMLINKS and compared against `path`'s canonical
/// form: that is the cheap way to answer "is this file live" from outside the
/// owning process, and it is what makes the answer about the FILE rather than a
/// name (in the measured incident the wedged daemon held three *deleted*
/// handles, so no process held the on-disk inodes and a delete was provably
/// safe).
///
/// THIS process counts too, deliberately. A daemon that opens two pools on one
/// file — or any code path that connects twice — has a live 0-byte `-wal` beside
/// a 32 KiB `-shm` for the first pool while the second `connect` runs, and that
/// is indistinguishable from the wedge unless the caller's own handles are
/// counted. Skipping `self` here would have this function CREATE the failure it
/// exists to clear. Any failure to enumerate — a non-Linux host, a restricted
/// `/proc` — answers `true`, the safe direction: leaving a stale pair costs one
/// more restart, while unlinking a live one costs a running process its WAL.
#[cfg(target_os = "linux")]
fn held_by_another_process(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    let Ok(canonical) = std::fs::canonicalize(path) else {
        return true;
    };
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return true;
    };
    for proc_entry in procs.flatten() {
        let Some(pid) = proc_entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let fds = proc_entry.path().join("fd");
        let Ok(entries) = std::fs::read_dir(&fds) else {
            continue;
        };
        for fd in entries.flatten() {
            if std::fs::read_link(fd.path()).ok().as_deref() == Some(canonical.as_path()) {
                tracing::debug!("{} is held open by pid {pid}", path.display());
                return true;
            }
        }
    }
    false
}

#[cfg(not(target_os = "linux"))]
fn held_by_another_process(_path: &Path) -> bool {
    // No cheap portable answer, so assume a live owner and leave the files be.
    true
}

/// `path` with a SQLite sidecar suffix appended, as SQLite itself spells it.
fn sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut s = db_path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
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
        .rfind(|p| snapshot_is_usable(p.as_path()))
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
                    match conn
                        .pragma_query_value(None, "integrity_check", |row| row.get::<_, String>(0))
                    {
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
