//! The retention sweep for OpenCrabs' throwaway files (#345).
//!
//! `logging::cleanup_old_temp_files` is the single startup purge for every
//! directory the product writes temp files into: the profile's `tmp/files`
//! (channel image uploads), the profile's `tmp/` root (loose scratch files left
//! beside those subdirectories), and the tool-output spill dir, where a tool
//! result that exceeds the inline budget is written. Before #345 only the first
//! was swept and the spill dir had no deletion path at all, so it grew without
//! bound — the host's OS sweep does not recurse into it.
//!
//! Every test drives the public entry point rather than a private helper, so
//! what is pinned is the behaviour callers get: the window decides what goes,
//! `0` disables the purge, subdirectories and symlinks are skipped rather than
//! followed, and a target that cannot be scanned warns instead of aborting.

use crate::config::profile::with_home_override;
use crate::logging::cleanup_old_temp_files;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The spill dir is a compile-time absolute path shared by every profile on the
/// host, so its leg cannot be redirected into a tempdir the way the home legs
/// can. Collateral is prevented by the numbers instead: this module's spill
/// files are aged 1000 days and the windows it passes are 900 days, so nothing
/// a real session wrote (minutes old) is ever eligible. A long window is used
/// in every test that may touch that path, and the exact-count assertions are
/// confined to tests that cannot see it.
const SPILL_DIR: &str = crate::brain::agent::service::tool_loop::TOOL_OUTPUT_DIR;

const DAY: u64 = 24 * 60 * 60;

fn days(n: u64) -> Duration {
    Duration::from_secs(n * DAY)
}

/// The spill dir is shared by every test in this module, so the home legs'
/// isolation trick — one tempdir each — does not cover it: each test sweeps the
/// same real directory. This lock serializes them.
///
/// Without it, one test's sweep can remove another test's aged spill file
/// between that test's write and its assertion, which turns the count
/// assertions into coin flips: `aged_files_are_reaped_in_every_swept_dir` would
/// read 2 where it wants 3, and
/// `a_failed_unlink_is_counted_out_but_does_not_abort_the_sweep` would count
/// removals it never made. Both would fail intermittently for no defect at all,
/// which is worse than failing for a real one.
static SPILL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Hold the spill lock for the rest of the calling test.
///
/// A poisoned lock is recovered rather than propagated: the test that panicked
/// has already reported its own failure, and failing every later test on the
/// strength of it would bury the real one.
fn spill_guard() -> std::sync::MutexGuard<'static, ()> {
    SPILL_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Write `name` into `dir` (created if needed), backdating its mtime by `age`.
fn write_aged(dir: &Path, name: &str, age: Duration) -> PathBuf {
    std::fs::create_dir_all(dir).expect("create dir");
    let path = dir.join(name);
    let file = std::fs::File::create(&path).expect("create file");
    let modified = SystemTime::now().checked_sub(age).expect("clock is sane");
    file.set_modified(modified).expect("backdate mtime");
    path
}

/// A spill-dir filename unique to this process, so a parallel test binary and a
/// live session's own output can never be confused for it.
fn spill_name(tag: &str) -> String {
    format!("test-345-{tag}-{}.log", std::process::id())
}

#[test]
fn aged_files_are_reaped_in_every_swept_dir() {
    let _spill = spill_guard();
    let home = tempfile::tempdir().expect("tempdir");
    let tmp = home.path().join("tmp");

    // One aged file in each of the three roots the purge claims to cover, all
    // swept by a single call — the point of the change is that no root is left
    // without an owner.
    let spill = write_aged(Path::new(SPILL_DIR), &spill_name("aged"), days(1000));
    let upload = write_aged(&tmp.join("files"), "tg_photo_345.jpg", days(1000));
    let scratch = write_aged(&tmp, "loose-345.bin", days(1000));

    let removed = with_home_override(home.path().to_path_buf(), || cleanup_old_temp_files(900, 900));

    for path in [&spill, &upload, &scratch] {
        assert!(
            !path.exists(),
            "an aged file must be reaped from every swept dir, survived: {}",
            path.display()
        );
    }
    assert!(
        removed >= 3,
        "the purge reports every file it removed, got {removed}"
    );
}

#[test]
fn fresh_files_are_kept() {
    let _spill = spill_guard();
    let home = tempfile::tempdir().expect("tempdir");
    let tmp = home.path().join("tmp");

    let spill = write_aged(Path::new(SPILL_DIR), &spill_name("fresh"), days(0));
    let upload = write_aged(&tmp.join("files"), "fresh-upload-345.jpg", days(0));
    let scratch = write_aged(&tmp, "fresh-loose-345.bin", days(0));

    with_home_override(home.path().to_path_buf(), || cleanup_old_temp_files(900, 900));

    // No count assertion here: a sibling test in this binary may legitimately
    // have a stale spill file in flight, and the property under test is which
    // files SURVIVE, not how many went.
    for path in [&spill, &upload, &scratch] {
        assert!(
            path.exists(),
            "a file written now must never be reaped, gone: {}",
            path.display()
        );
    }
}

#[test]
fn the_shipped_default_window_is_seven_days() {
    let _spill = spill_guard();
    // The window is the only load-bearing number in the change: 7 days bounds
    // the spill dir, and it is what an unconfigured install gets. Pin it to the
    // config field so a later default edit cannot drift from this test.
    let window = u64::from(crate::config::AgentConfig::default().tool_output_retention_days());
    assert_eq!(window, 7, "the shipped retention window");

    let home = tempfile::tempdir().expect("tempdir");
    let tmp = home.path().join("tmp");
    let beyond = write_aged(&tmp, "beyond-345.bin", days(window + 1));
    let within = write_aged(&tmp, "within-345.bin", days(window - 1));

    with_home_override(home.path().to_path_buf(), || cleanup_old_temp_files(window, window));

    assert!(!beyond.exists(), "older than the window must go");
    assert!(within.exists(), "younger than the window must stay");
}

#[test]
fn a_zero_window_disables_the_purge() {
    let _spill = spill_guard();
    // `age > max_age` is satisfied by every file that was not written this
    // instant, so a zero window would wipe the directories rather than spare
    // them. The guard is what makes `0` mean "off", and it is asserted here
    // because a config value of 0 must never delete a user's files.
    let home = tempfile::tempdir().expect("tempdir");
    let tmp = home.path().join("tmp");
    let upload = write_aged(&tmp.join("files"), "tg_photo_345.jpg", days(1000));
    let scratch = write_aged(&tmp, "loose-345.bin", days(1000));

    let removed = with_home_override(home.path().to_path_buf(), || cleanup_old_temp_files(0, 0));

    assert_eq!(removed, 0, "a disabled purge removes nothing");
    assert!(upload.exists(), "0 must not age out the uploads");
    assert!(scratch.exists(), "0 must not age out the scratch files");
}

#[test]
fn subdirectories_are_skipped_and_do_not_abort_the_sweep() {
    let _spill = spill_guard();
    let home = tempfile::tempdir().expect("tempdir");
    let tmp = home.path().join("tmp");

    // An aged file in a subdirectory of each swept home dir, plus aged files
    // directly beside them. A subdirectory has its own owner and its own sweep
    // (`<home>/tmp/detached` is aged out by `work_status::cleanup_stale`), so
    // this purge must not reach inside it — and it must keep going: the
    // removable files are placed on both sides of a skipped entry, so a sweep
    // that stopped at the first non-file would leave the second one behind.
    let nested_upload = write_aged(&tmp.join("files").join("aged-sub"), "old.jpg", days(1000));
    let nested_scratch = write_aged(&tmp.join("some-sub"), "old.bin", days(1000));
    let before = write_aged(&tmp, "before-345.bin", days(1000));
    let after = write_aged(&tmp, "after-345.bin", days(1000));

    with_home_override(home.path().to_path_buf(), || cleanup_old_temp_files(900, 900));

    assert!(
        nested_upload.exists(),
        "a subdirectory's files are not this purge's to remove"
    );
    assert!(
        nested_scratch.exists(),
        "a subdirectory's files are not this purge's to remove"
    );
    assert!(
        tmp.join("files").join("aged-sub").is_dir(),
        "the subdirectory itself survives"
    );
    assert!(
        tmp.join("some-sub").is_dir(),
        "the subdirectory itself survives"
    );
    assert!(
        !before.exists(),
        "an aged file beside a skipped entry is still reaped"
    );
    assert!(!after.exists(), "the sweep continues past a skipped entry");
}

#[test]
fn symlinks_are_skipped_and_never_followed() {
    let _spill = spill_guard();
    let home = tempfile::tempdir().expect("tempdir");
    let tmp = home.path().join("tmp");
    std::fs::create_dir_all(&tmp).expect("create tmp");

    // The link lives in a swept dir and points at an aged file OUTSIDE it. If
    // the sweep resolved the link, the target would be removed by a purge that
    // was never asked to look there.
    let target = write_aged(&home.path().join("outside"), "target-345.bin", days(1000));
    let link = tmp.join("link-345.bin");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");

    with_home_override(home.path().to_path_buf(), || cleanup_old_temp_files(900, 900));

    assert!(
        link.symlink_metadata().is_ok(),
        "the link itself is skipped, not removed"
    );
    assert!(
        target.exists(),
        "a symlink must never be followed: its target was deleted"
    );
}

#[test]
fn a_sweep_target_that_cannot_be_scanned_is_not_fatal() {
    let _spill = spill_guard();
    // `<home>/tmp` as a regular FILE: the root leg gets NotADirectory and the
    // `files` leg gets NotFound. A startup purge must warn and carry on — it
    // must not fail the boot, and it must not delete what it could not read.
    let home = tempfile::tempdir().expect("tempdir");
    let tmp = home.path().join("tmp");
    std::fs::write(&tmp, "not a directory").expect("write");

    // No count assertion: the spill leg is a real directory here and may
    // legitimately contribute.
    with_home_override(home.path().to_path_buf(), || cleanup_old_temp_files(900, 900));

    assert_eq!(
        std::fs::read_to_string(&tmp).expect("still readable"),
        "not a directory",
        "a target that could not be scanned must be left alone"
    );
}

#[test]
fn missing_directories_are_not_an_error() {
    let _spill = spill_guard();
    // A fresh profile has no `tmp` tree at all. Nothing to sweep is the normal
    // first-boot case, not a failure.
    let home = tempfile::tempdir().expect("tempdir");

    with_home_override(home.path().to_path_buf(), || cleanup_old_temp_files(7, 7));

    assert!(
        !home.path().join("tmp").join("files").exists(),
        "the purge must not create what it sweeps"
    );
}

#[test]
fn a_failed_unlink_is_counted_out_but_does_not_abort_the_sweep() {
    let _spill = spill_guard();
    // One entry cannot be removed — its parent is read-only — while a control
    // file in the next directory can. The sweep must report only what it
    // actually removed and must still reach the control: one failure may
    // neither inflate the count nor end the pass.
    //
    // Whether a read-only parent actually blocks the unlink depends on the uid
    // the tests run as (root ignores the mode), so the assertions compare the
    // reported count against the files that are genuinely gone rather than
    // assuming a fixed number. That makes the test say the same thing on both.
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().expect("tempdir");
    let tmp = home.path().join("tmp");
    let uploads = tmp.join("files");

    let blocked = write_aged(&uploads, "blocked-345.bin", days(1000));
    let control = write_aged(&tmp, "control-345.bin", days(1000));

    std::fs::set_permissions(&uploads, std::fs::Permissions::from_mode(0o555)).expect("chmod");
    let removed = with_home_override(home.path().to_path_buf(), || cleanup_old_temp_files(900, 900));
    // Restored before asserting: a read-only directory left behind would break
    // the tempdir's own recursive cleanup.
    std::fs::set_permissions(&uploads, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    assert!(
        !control.exists(),
        "a failure in one directory must not abort the sweep of the next"
    );
    let gone = [&blocked, &control]
        .iter()
        .filter(|path| !path.exists())
        .count();
    assert_eq!(removed, gone, "the count reports only what it removed");
}

/// #848: the `tmp/runs` leg has exactly one owner, and this is it.
///
/// `work_status::cleanup_stale` used to sweep captured streams as well, and
/// since both it and this purge run on every boot, every start paid the
/// `tmp/runs` scan twice. The duplicate was removed from `cleanup_stale`; this
/// test pins that the duty did not fall through the gap — an aged capture must
/// still be reaped by the purge that kept the leg.
#[test]
fn the_runs_leg_is_swept_by_its_single_owner() {
    let _spill = spill_guard();
    // `work_status::runs_dir()` consults the thread-local test override BEFORE
    // the home, and the test runner reuses threads. A stale override leaked by
    // another test in this binary would point the sweep at an unrelated
    // directory, and this test would then pass or fail on scheduling order.
    crate::brain::agent::service::work_status::test_override::clear();

    let home = tempfile::tempdir().expect("tempdir");
    let runs = home.path().join("tmp").join("runs");
    let capture = write_aged(&runs, "owner-848.out", days(1000));

    let removed = with_home_override(home.path().to_path_buf(), || cleanup_old_temp_files(900, 900));

    assert!(
        !capture.exists(),
        "an aged capture must be swept by the purge that owns the leg: {}",
        capture.display()
    );
    assert!(
        removed >= 1,
        "the purge reports the capture it removed, got {removed}"
    );
}

// ---------------------------------------------------------------------------
// #872 — the purge is a leg of the maintenance sweep, not a step of every start
// ---------------------------------------------------------------------------

/// The purge must run from the maintenance sweep, and report what it removed
/// (#872).
///
/// It used to run in `main.rs` before `cli::run()`, so every CLI invocation —
/// `opencrabs cron list`, a TUI start, a daemon boot — walked the whole temp
/// tree before the surface that had been asked for could answer. Measured on
/// the ops profile: 17.92 s for that leg against 0.37 s on identical code, the
/// entire difference being a stalled disk while nothing was listening yet.
///
/// Driving the sweep rather than the helper is the point: moving the call is
/// only safe if the new caller actually purges. A leg that stopped running
/// altogether would also stop running at boot, and every test of
/// `cleanup_old_temp_files` would still pass.
///
/// The `allow` is deliberate, not a workaround: `spill_guard` is a `std::Mutex`
/// held across this sweep's await on purpose, because what it serializes is a
/// SHARED FILESYSTEM DIRECTORY that another test could delete a fixture out of
/// mid-run. The lint's advice — an async-aware mutex — would let a concurrent
/// test interleave instead of blocking, which is the exact defect the lock
/// exists to prevent. Blocking a runtime worker is harmless here: the sweep is
/// the only thing this task runs.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn maintenance_sweep_purges_and_counts_aged_temp_files() {
    let _spill = spill_guard();
    let home = tempfile::tempdir().expect("tempdir");

    crate::config::profile::with_home_override_async(home.path().to_path_buf(), async {
        // 30 days: past the 7-day default the sweep resolves from an absent
        // config, and inside a directory only the purge walks.
        let files = crate::config::opencrabs_home().join("tmp").join("files");
        let stale = write_aged(&files, "tg_upload_872.jpg", days(30));

        let db = crate::db::Database::connect_in_memory().await.expect("db");
        db.run_migrations().await.expect("migrations");
        let svc = crate::services::maintenance::MaintenanceService::new(
            crate::services::context::ServiceContext::new(db.pool().clone()),
        );

        let report = svc
            .run_maintenance_inner(crate::db::MaintenanceKnobs::default())
            .await
            .expect("sweep");

        assert!(
            !stale.exists(),
            "the sweep must purge an aged temp file: {}",
            stale.display()
        );
        assert!(
            report.temp_files_purged >= 1,
            "the sweep must report the files it purged, got {}",
            report.temp_files_purged
        );
    })
    .await;
}

/// `main.rs` must not call the purge again (#872).
///
/// This is the whole fix, and it is exactly what a later refactor reverts by
/// accident: re-adding the call compiles, passes every behavioural test in this
/// file, and silently puts a full temp-tree walk back in front of `cli::run()`.
/// Only a source pin can see the difference, because the defect is where the
/// call lives rather than what it does.
///
/// The second assertion is the control. Without it, deleting the purge from
/// both files would satisfy the pin.
#[test]
fn main_does_not_purge_temp_files_on_startup() {
    const MAIN: &str = include_str!("../main.rs");
    const SWEEP: &str = include_str!("../services/maintenance.rs");

    assert!(
        !MAIN.contains("cleanup_old_temp_files"),
        "the temp purge belongs to the maintenance sweep (#872), not to every CLI start"
    );
    assert!(
        SWEEP.contains("cleanup_old_temp_files"),
        "control: the sweep is the purge's owner, so it must still call it"
    );
}

/// A run's empty capture files must be unlinked after its finish record, never
/// before (#872).
///
/// `File::create` makes both stream files when a run starts, so a command that
/// writes nothing leaves two zero-byte files behind per run — 17,632 of the
/// 36,461 entries the ops sweep walked. The order is the property: the finish
/// record is how a reader finds those files, so unlinking first leaves the
/// record pointing at nothing for the whole write window. That window is
/// between two awaits, which no behavioural test can observe, hence the pin.
#[test]
fn empty_capture_cleanup_runs_after_the_finish_record() {
    const BG: &str = include_str!("../brain/agent/service/background_tasks.rs");

    assert!(
        BG.contains("fn drop_empty_capture"),
        "control: the cleanup helper must still exist"
    );
    // `rfind`, not `find`: the completion path is the LAST writer of the
    // record, and the first match is an earlier call on a different path.
    let finish = BG
        .rfind("write_finish(")
        .expect("control: the completion path still writes a finish record");
    let cleanup = BG
        .find("drop_empty_capture(&out_path)")
        .expect("the empty-capture cleanup must be on the completion path");

    assert!(
        cleanup > finish,
        "the capture cleanup must follow the finish record, not precede it"
    );
}
