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

    let removed = with_home_override(home.path().to_path_buf(), || cleanup_old_temp_files(900));

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

    with_home_override(home.path().to_path_buf(), || cleanup_old_temp_files(900));

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
    let window = u64::from(crate::config::AgentConfig::default().tool_output_retention_days);
    assert_eq!(window, 7, "the shipped retention window");

    let home = tempfile::tempdir().expect("tempdir");
    let tmp = home.path().join("tmp");
    let beyond = write_aged(&tmp, "beyond-345.bin", days(window + 1));
    let within = write_aged(&tmp, "within-345.bin", days(window - 1));

    with_home_override(home.path().to_path_buf(), || cleanup_old_temp_files(window));

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

    let removed = with_home_override(home.path().to_path_buf(), || cleanup_old_temp_files(0));

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

    with_home_override(home.path().to_path_buf(), || cleanup_old_temp_files(900));

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

    with_home_override(home.path().to_path_buf(), || cleanup_old_temp_files(900));

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
    with_home_override(home.path().to_path_buf(), || cleanup_old_temp_files(900));

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

    with_home_override(home.path().to_path_buf(), || cleanup_old_temp_files(7));

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
    let removed = with_home_override(home.path().to_path_buf(), || cleanup_old_temp_files(900));
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
