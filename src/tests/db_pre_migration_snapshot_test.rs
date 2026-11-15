//! Pre-migration image snapshot (#1779).
//!
//! The rpi5 incident: `/evolve` restarted the daemon, `run_migrations` wrote
//! `ALTER TABLE` against an already-damaged image, and the cron rows died
//! because the file being migrated was the only copy of itself. These tests pin
//! the two halves of the fix: the snapshot happens BEFORE any write, and a
//! snapshot that cannot be taken STOPS the migration.

use crate::config::profile::with_home_override_async;
use crate::config::PreflightMode;
use crate::db::migration_snapshot::{
    integrity_preflight, rotate, snapshot_before_migrations, LATEST, PREFIX, RETENTION,
};
use crate::db::Database;
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

/// A file-backed database carrying one table and no migrations applied.
///
/// The canary table is what makes it "non-empty": the snapshot must contain it,
/// and must NOT contain anything a migration would have created.
fn seeded_db(dir: &Path) -> PathBuf {
    let path = dir.join("opencrabs.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute("CREATE TABLE canary(x TEXT)", []).unwrap();
    conn.execute("INSERT INTO canary VALUES ('nicole')", [])
        .unwrap();
    drop(conn);
    path
}

fn tables_in(path: &Path) -> Vec<String> {
    // READ_WRITE rather than READ_ONLY on purpose: the live database under test
    // is in WAL mode by the time these helpers run (the pool's post_create hook
    // applies the pragma), and a WAL database cannot be opened read-only without
    // its -shm sidecar. No CREATE flag, so a missing file is still a hard failure
    // instead of a silently empty result.
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap();
    let rows: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    rows
}

fn dated_snapshots(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(PREFIX) && n != LATEST)
                .unwrap_or(false)
        })
        .collect();
    v.sort();
    v
}

// ---------------------------------------------------------------- outcome

#[tokio::test]
async fn non_empty_db_is_snapshotted_before_migrations_run() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join(".opencrabs");
    std::fs::create_dir_all(&home).unwrap();
    let db_path = seeded_db(&home);

    let db = with_home_override_async(home.clone(), async {
        let db = Database::connect(&db_path).await.unwrap();
        db.run_migrations().await.unwrap();
        db
    })
    .await;
    let _ = db;

    let backups = home.join("backups");
    let snaps = dated_snapshots(&backups);
    assert_eq!(
        snaps.len(),
        1,
        "exactly one pre-migration snapshot expected"
    );

    // The ordering claim, measured rather than asserted from a log line: the
    // snapshot holds the canary and none of the migration's tables, so it was
    // taken before `to_latest` ran.
    let names = tables_in(&snaps[0]);
    assert!(
        names.contains(&"canary".to_string()),
        "snapshot must carry the pre-migration data, got {names:?}"
    );
    assert!(
        !names.contains(&"sessions".to_string()),
        "snapshot was taken AFTER migrations ran, not before: got {names:?}"
    );

    // The alias the refusal message promises must exist and match the newest.
    let latest = backups.join(LATEST);
    assert!(latest.exists(), "-latest alias must always name a file");
    assert_eq!(
        std::fs::read(&latest).unwrap().len(),
        std::fs::read(&snaps[0]).unwrap().len(),
        "-latest must name the same bytes as the newest snapshot"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let alias = std::fs::metadata(&latest).unwrap();
        let dated_meta = std::fs::metadata(&snaps[0]).unwrap();
        assert_eq!(
            (alias.dev(), alias.ino()),
            (dated_meta.dev(), dated_meta.ino()),
            "-latest must be a hard link to the dated snapshot, not a second copy"
        );
    }
}

#[tokio::test]
async fn second_startup_does_not_write_another_snapshot() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join(".opencrabs");
    std::fs::create_dir_all(&home).unwrap();
    let db_path = seeded_db(&home);

    with_home_override_async(home.clone(), async {
        let db = Database::connect(&db_path).await.unwrap();
        db.run_migrations().await.unwrap();
        db.run_migrations().await.unwrap();
    })
    .await;

    let snaps = dated_snapshots(&home.join("backups"));
    assert_eq!(
        snaps.len(),
        1,
        "a database already at the latest migration must not VACUUM INTO again: {snaps:?}"
    );
}

#[tokio::test]
async fn fresh_empty_db_skips_the_snapshot_silently() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join(".opencrabs");
    std::fs::create_dir_all(&home).unwrap();
    let db_path = home.join("opencrabs.db");
    Connection::open(&db_path).unwrap(); // no tables at all

    with_home_override_async(home.clone(), async {
        let db = Database::connect(&db_path).await.unwrap();
        db.run_migrations().await.unwrap();
    })
    .await;

    let backups = home.join("backups");
    let snaps = if backups.exists() {
        dated_snapshots(&backups)
    } else {
        vec![]
    };
    assert!(
        snaps.is_empty(),
        "a brand-new database has nothing to protect: got {snaps:?}"
    );
}

/// The whole test suite calls `run_migrations()` on in-memory databases, and
/// `Connection::path()` strips a URI's query part, so `?mode=memory` is
/// invisible to it. Without the `PRAGMA database_list` check every `cargo test`
/// run would write 70 MB snapshots into the developer's real home.
#[tokio::test]
async fn in_memory_db_never_snapshots() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join(".opencrabs");
    std::fs::create_dir_all(&home).unwrap();

    with_home_override_async(home.clone(), async {
        let db = Database::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
    })
    .await;

    assert!(
        !home.join("backups").exists(),
        "in-memory database must not create a backups dir"
    );
}

// ---------------------------------------------------------------- retention

#[tokio::test]
async fn rotation_keeps_seven_dated_copies_and_the_alias() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = seeded_db(tmp.path());
    let backups = tmp.path().join("backups");
    let conn = Connection::open(&db_path).unwrap();

    // Nine startups. Same-second collisions are expected in a crash-restart
    // loop, so the names must stay distinct rather than fail.
    for _ in 0..9 {
        snapshot_before_migrations(&conn, &backups)
            .unwrap()
            .expect("non-empty db must snapshot");
    }

    let snaps = dated_snapshots(&backups);
    assert_eq!(
        snaps.len(),
        RETENTION,
        "retention is the directed 7-day window, got {}",
        snaps.len()
    );
    assert!(
        backups.join(LATEST).exists(),
        "rotation must never prune the alias"
    );

    // Oldest first, so the survivors are the newest RETENTION.
    let removed = rotate(&backups).unwrap();
    assert_eq!(removed, 0, "already at the cap, nothing to prune");
    assert_eq!(dated_snapshots(&backups).len(), RETENTION);
}

// ------------------------------------------------------------ refusal gate

#[tokio::test]
async fn migrations_are_refused_when_the_snapshot_cannot_be_taken() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join(".opencrabs");
    std::fs::create_dir_all(&home).unwrap();
    let db_path = seeded_db(&home);

    // Sabotage the destination: a regular FILE where the backups directory has
    // to be, so create_dir_all fails and VACUUM INTO never gets a chance.
    std::fs::write(home.join("backups"), b"not a directory").unwrap();

    let (before, err) = with_home_override_async(home.clone(), async {
        let db = Database::connect(&db_path).await.unwrap();
        // Baseline AFTER the pool's first real connection, not before it.
        // connect() is lazy: the first get() runs post_create, whose
        // journal_mode=WAL rewrites the image header (bytes 18-19 go 1,1 -> 2,2)
        // and bumps the change counter. That is SQLite converting the file to
        // WAL, it happens on every single open, and it is not a migration.
        // Measuring from the pre-open bytes would fail on the pragma and prove
        // nothing about the refusal.
        let _warm = db.pool.get().await.unwrap();
        let before = std::fs::read(&db_path).unwrap();
        let err = db.run_migrations().await.unwrap_err();
        (before, err)
    })
    .await;

    let msg = format!("{err:#}");
    assert!(
        msg.contains("Refusing to run database migrations"),
        "message must say it refused, got: {msg}"
    );
    assert!(
        !msg.contains("Failed to run database migrations"),
        "the misleading migration wording must not resurface here: {msg}"
    );
    assert!(
        msg.contains("Restore it by copying a snapshot"),
        "message must name the recovery action: {msg}"
    );
    assert!(
        msg.contains("brain files and config are untouched"),
        "message must scope the blast radius: {msg}"
    );

    // Nothing was written past the pragma. This is the zero-data-loss half of
    // the promise: a refused migration leaves the image byte-identical, so it
    // stays repairable by the rescue tooling instead of being half-migrated.
    assert_eq!(
        std::fs::read(&db_path).unwrap(),
        before,
        "refused migration must not touch the database file"
    );
    let names = tables_in(&db_path);
    assert!(
        !names.contains(&"sessions".to_string()),
        "migrations must NOT have run: got {names:?}"
    );

    // And the row that mattered is still there, still readable, still at
    // user_version 0. The byte comparison above can only ever prove "unchanged
    // since the baseline"; this proves "the data survives the refusal".
    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    let version: i32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        version, 0,
        "a refused migration must not advance user_version"
    );
    let canary: String = conn
        .query_row("SELECT x FROM canary", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        canary, "nicole",
        "the pre-existing row must survive refusal"
    );
}

// ------------------------------------------------------------ corrupt image

/// The rpi5 shape: a valid magic and header over a page-1 b-tree that no
/// longer parses.
///
/// Blanking bytes 100..200 leaves the file openable but makes
/// `PRAGMA integrity_check` fail with SQLITE_CORRUPT instead of returning a
/// report row. Verified 2026-09-28 against the sqlite3 CLI, where it was the
/// candidate that reproduced the incident's exact error string (a flipped byte
/// at 150, at 300, or a zeroed change counter all still report `ok`, so they
/// prove nothing). That distinction is the point of this corruption: a check
/// treating only non-`"ok"` *text* as damage waves this through, because there
/// is no text at all.
///
/// Returns the corrupted bytes so a test can prove nothing was written back.
fn corrupt_page_one(path: &Path) -> Vec<u8> {
    let mut bytes = std::fs::read(path).unwrap();
    bytes[100..200].fill(0);
    std::fs::write(path, &bytes).unwrap();
    bytes
}

// ── #835: the preflight is an O(1) header read, not a page walk ─────────────
//
// The full `PRAGMA integrity_check` cost a complete scan on every boot that
// needed a snapshot: ~33 min on the 3.7 GiB ops image at the measured ~2 MB/s,
// and it was a second read of a file `VACUUM INTO` reads anyway. The header
// check must still refuse the shapes that matter, and must NOT refuse a file
// whose damage lies deeper, because the vacuum is the probe for that.

fn preflight_dir(tmp: &Path) -> PathBuf {
    let dir = tmp.join("backups");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn preflight_accepts_a_healthy_image() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = preflight_dir(tmp.path());
    let path = seeded_db(tmp.path());

    integrity_preflight(&path.to_string_lossy(), &dir, PreflightMode::Header)
        .expect("a healthy image must pass the header check");
}

#[test]
fn preflight_refuses_a_zeroed_page_one_header() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = preflight_dir(tmp.path());
    let path = seeded_db(tmp.path());
    corrupt_page_one(&path);

    let err = integrity_preflight(&path.to_string_lossy(), &dir, PreflightMode::Header)
        .expect_err("the rpi5 shape must be refused by the header read");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("the pre-migration integrity check failed"),
        "the refusal must name the stage that found the damage: {msg}"
    );
    assert!(
        msg.contains("b-tree page type"),
        "the cause must say what the header read saw, not just that it failed: {msg}"
    );
    assert!(
        msg.contains(&dir.display().to_string()),
        "the message must name the directory the operator restores from: {msg}"
    );
}

#[test]
fn preflight_refuses_a_file_without_the_sqlite_magic() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = preflight_dir(tmp.path());
    let path = tmp.path().join("not-a-database.db");
    std::fs::write(&path, vec![0x41u8; 4096]).unwrap();

    let err = integrity_preflight(&path.to_string_lossy(), &dir, PreflightMode::Header)
        .expect_err("a file with no SQLite magic is not a database");
    assert!(
        format!("{err:#}").contains("SQLite magic"),
        "the cause must name the missing magic: {err:#}"
    );
}

#[test]
fn preflight_refuses_a_file_too_short_to_hold_a_header() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = preflight_dir(tmp.path());
    let path = tmp.path().join("stub.db");
    std::fs::write(&path, b"SQLite format 3\0").unwrap();

    let err = integrity_preflight(&path.to_string_lossy(), &dir, PreflightMode::Header)
        .expect_err("a header truncated mid-way cannot be validated");
    assert!(
        format!("{err:#}").contains("too short to hold a SQLite header"),
        "the cause must say the file is truncated: {err:#}"
    );
}

/// A brand-new database has a file but no bytes yet: refusing it would refuse
/// every first boot.
#[test]
fn preflight_accepts_a_zero_length_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = preflight_dir(tmp.path());
    let path = tmp.path().join("empty.db");
    std::fs::File::create(&path).unwrap();

    integrity_preflight(&path.to_string_lossy(), &dir, PreflightMode::Header)
        .expect("a fresh install must not be refused");
}

/// Same for a path with no file behind it at all.
#[test]
fn preflight_accepts_an_absent_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = preflight_dir(tmp.path());
    let path = tmp.path().join("does-not-exist.db");

    integrity_preflight(&path.to_string_lossy(), &dir, PreflightMode::Header)
        .expect("a path with no file is a fresh install, not a torn image");
}

/// Damage past page 1 is `VACUUM INTO`'s job. The header check must stay out of
/// its way, or it becomes the full scan again.
#[test]
fn preflight_accepts_damage_deeper_than_page_one() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = preflight_dir(tmp.path());
    let path = seeded_db(tmp.path());
    // Grow the file past a few pages, then zero one of them.
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE filler(x BLOB)", []).unwrap();
        conn.execute("INSERT INTO filler VALUES (randomblob(16384))", [])
            .unwrap();
        drop(conn);
    }
    let mut bytes = std::fs::read(&path).unwrap();
    assert!(bytes.len() > 3 * 4096, "need a few pages to damage");
    bytes[2 * 4096..3 * 4096].fill(0);
    std::fs::write(&path, &bytes).unwrap();

    integrity_preflight(&path.to_string_lossy(), &dir, PreflightMode::Header).expect(
        "a later-page corruption must reach VACUUM INTO, which is the real page walk",
    );
}

#[test]
fn preflight_off_skips_even_a_torn_header() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = preflight_dir(tmp.path());
    let path = seeded_db(tmp.path());
    corrupt_page_one(&path);

    integrity_preflight(&path.to_string_lossy(), &dir, PreflightMode::Off)
        .expect("`off` means the vacuum is the only probe");
}

/// The two refusal messages exist because they tell the operator different
/// things. When the vacuum finds corruption, the image was already broken — the
/// receipt must say so rather than blaming the copy.
///
/// Mode-independent by construction: `header` and `off` reach the vacuum, and
/// `full` fails its pragma at prepare, so all three must produce this wording.
#[tokio::test]
async fn vacuum_corruption_is_reported_as_the_integrity_refusal() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join(".opencrabs");
    std::fs::create_dir_all(&home).unwrap();
    let db_path = seeded_db(&home);
    {
        let conn = Connection::open(&db_path).unwrap();
        conn.execute("CREATE TABLE filler(x BLOB)", []).unwrap();
        conn.execute("INSERT INTO filler VALUES (randomblob(16384))", [])
            .unwrap();
        drop(conn);
    }
    let mut bytes = std::fs::read(&db_path).unwrap();
    assert!(bytes.len() > 3 * 4096, "need a few pages to damage");
    bytes[2 * 4096..3 * 4096].fill(0);
    std::fs::write(&db_path, &bytes).unwrap();

    let err = with_home_override_async(home.clone(), async {
        let db = Database::connect(&db_path).await.unwrap();
        db.run_migrations().await.unwrap_err()
    })
    .await;

    let msg = format!("{err:#}");
    assert!(
        msg.contains("the pre-migration integrity check failed"),
        "corruption found by the vacuum must be reported as an integrity \
         failure, not as a snapshot that could not be taken: {msg}"
    );
    assert!(
        !msg.contains("the pre-migration snapshot failed"),
        "the snapshot wording points the operator at the disk when the image is \
         what is broken: {msg}"
    );
    assert!(
        !msg.contains("Failed to run database migrations"),
        "the misleading rpi5 wording must not resurface: {msg}"
    );
}

#[tokio::test]
async fn corrupt_image_is_refused_before_any_migration_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join(".opencrabs");
    std::fs::create_dir_all(&home).unwrap();
    let db_path = seeded_db(&home);
    let corrupted = corrupt_page_one(&db_path);

    let err = with_home_override_async(home.clone(), async {
        let db = Database::connect(&db_path).await.unwrap();
        db.run_migrations().await.unwrap_err()
    })
    .await;

    let msg = format!("{err:#}");
    assert!(
        msg.contains("Refusing to run database migrations"),
        "a corrupt image must refuse, not migrate: {msg}"
    );
    assert!(
        msg.contains("the pre-migration integrity check failed"),
        "the message must name the stage that found the damage, not blur it \
         into a snapshot failure: {msg}"
    );
    assert!(
        msg.contains(&home.join("backups").display().to_string()),
        "the message must name the directory the operator restores from: {msg}"
    );
    assert!(
        msg.contains("brain files and config are untouched"),
        "the message must bound the blast radius: {msg}"
    );
    assert!(
        !msg.contains("Failed to run database migrations"),
        "the misleading wording that hid the rpi5 cause must not resurface: {msg}"
    );
    assert!(
        !msg.contains("post_create hook failed"),
        "the check must not depend on the pool: on a torn image post_create \
        cannot produce a connection at all: {msg}"
    );

    // Refusing must also mean not writing. A startup that reports corruption
    // and then rewrites the header is how the rpi5 rows were lost a second
    // time, so this is the half of the promise the operator cannot recover
    // without.
    assert_eq!(
        std::fs::read(&db_path).unwrap(),
        corrupted,
        "a refused startup must not write a single byte to the image"
    );
}

/// #459/#677: `run_migrations()` no longer runs a post-migration integrity
/// check, so nothing sets the flag on a healthy image and it must stay clear.
/// (The scan that used to set it is gone; see
/// `run_migrations_does_not_scan_on_every_open`.)
#[tokio::test]
async fn healthy_image_leaves_the_integrity_flag_clear() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join(".opencrabs");
    std::fs::create_dir_all(&home).unwrap();
    let db_path = seeded_db(&home);

    with_home_override_async(home.clone(), async {
        let db = Database::connect(&db_path).await.unwrap();
        db.run_migrations().await.unwrap();
    })
    .await;

    assert!(
        tables_in(&db_path).contains(&"sessions".to_string()),
        "migrations must have run on a healthy image"
    );
    assert!(
        !crate::db::db_integrity_failed(),
        "a healthy image must not trip the integrity flag"
    );
}

/// #459/#677: `run_migrations()` must not scan the image on every open.
///
/// Upstream's unconditional post-migration `PRAGMA integrity_check` was
/// re-imported by the 2026-09-29 sync (`d1e4b98ea`) over #459's removal, so
/// every database open paid a full page scan -- measured 86 s on the 328 MB
/// default image and >120 s on the 2.7 GB ops image (2026-10-04), which is what
/// made `cron list` and every other cold CLI verb appear to hang.
///
/// This pins the removal by source, so the next sync fails loudly here instead
/// of silently restoring the scan.
///
/// The preflight ordering is still pinned: upstream's #1779 check must run
/// BEFORE the migration write, where it can refuse a torn image.
#[test]
fn run_migrations_does_not_scan_on_every_open() {
    const SRC: &str = include_str!("../db/database.rs");

    let preflight = SRC
        .find("integrity_preflight(")
        .expect("the pre-migration integrity check must exist in run_migrations");
    let write = SRC
        .find("migrations.to_latest(conn)")
        .expect("the migration write must exist");
    assert!(
        preflight < write,
        "the pre-migration integrity check must run BEFORE the write, found \
         preflight at {preflight} and the write at {write}"
    );

    // The scan must not come back after the write: it reads every page, so its
    // cost is O(database size) on EVERY open, and no foreground caller can act
    // on the verdict (#459, #677).
    assert!(
        !SRC.contains("Run integrity check on startup"),
        "run_migrations() must not run an unconditional integrity_check after \
         the migrations -- it scans every page on EVERY open (#459, #677)"
    );
}
