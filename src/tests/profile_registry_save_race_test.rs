//! Regression tests for #976 — `ProfileRegistry::save` racing on a fixed
//! `profiles.toml.tmp`.
//!
//! `save()` used to write to a constant temp path and then rename it over the
//! registry. Under concurrency that is not a temp file at all: every saver
//! wrote the same path, the first `rename` consumed it, and every other rename
//! failed `ENOENT`. It surfaced as an unrelated CI test failing on a branch
//! that touched no profile code, because nextest runs each test in its own
//! process and `profile_test.rs`'s `fs_lock()` is process-local.
//!
//! These tests pin the mechanism, not the symptom: `save()` must never touch a
//! predictable temp path, and concurrent savers must all succeed.

use crate::config::profile::ProfileRegistry;

/// Run `f` with `$HOME` pointed at a throwaway dir.
///
/// `ProfileRegistry::path()` goes through `base_opencrabs_dir()`, which reads
/// `dirs::home_dir()` — i.e. `$HOME` directly. The task-local profile override
/// does NOT affect it, so a process-global mutation under `HOME_ENV_LOCK` is
/// the only way to isolate this (#1096 documents the same constraint for
/// `expand_tilde`).
fn with_temp_home<T>(f: impl FnOnce() -> T) -> T {
    let temp_home = tempfile::TempDir::new().expect("temp home");
    let _lock = crate::tests::HOME_ENV_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let prev_home = std::env::var_os("HOME");
    let prev_userprofile = std::env::var_os("USERPROFILE");
    // SAFETY: HOME_ENV_LOCK serializes HOME mutation for the guard's lifetime.
    unsafe {
        std::env::set_var("HOME", temp_home.path());
        std::env::set_var("USERPROFILE", temp_home.path());
    }

    let out = f();

    unsafe {
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prev_userprofile {
            Some(v) => std::env::set_var("USERPROFILE", v),
            None => std::env::remove_var("USERPROFILE"),
        }
    }
    out
}

/// A stale file sitting at the OLD fixed temp path must survive a `save()`.
///
/// This is the deterministic half of the regression. Before the fix `save()`
/// wrote to `profiles.toml.tmp` and renamed it away, so any file at that path
/// was consumed by an unrelated save. After the fix the temp name carries the
/// pid and a counter, so a predictable path is never touched.
#[test]
fn save_never_consumes_a_file_at_the_fixed_legacy_temp_path() {
    with_temp_home(|| {
        let base = crate::config::profile::base_opencrabs_dir();
        std::fs::create_dir_all(&base).expect("base dir");
        let legacy_tmp = base.join("profiles.toml.tmp");
        std::fs::write(&legacy_tmp, "SENTINEL — not ours to consume").expect("write sentinel");

        ProfileRegistry::default()
            .save()
            .expect("save must succeed");

        assert!(
            legacy_tmp.exists(),
            "save() consumed a file at the predictable temp path {} — the temp \
             name is not unique per call, so concurrent savers race on it (#976)",
            legacy_tmp.display()
        );
        assert_eq!(
            std::fs::read_to_string(&legacy_tmp).expect("sentinel readable"),
            "SENTINEL — not ours to consume",
            "save() overwrote a file at the predictable temp path (#976)",
        );

        // The real registry must still have been written.
        assert!(
            base.join("profiles.toml").exists(),
            "save() must still write the registry itself",
        );
    });
}

/// Concurrent savers must all succeed.
///
/// The real-world shape of the bug: several `save()` calls in flight lose the
/// rename race. Eight threads is enough to collide reliably with a fixed temp
/// name (one rename wins, seven hit ENOENT) and is harmless with unique names.
#[test]
fn concurrent_saves_all_succeed() {
    with_temp_home(|| {
        let threads: Vec<_> = (0..8)
            .map(|i| {
                std::thread::spawn(move || {
                    let mut reg = ProfileRegistry::default();
                    reg.profiles.insert(
                        format!("racer{i}"),
                        crate::config::profile::ProfileEntry {
                            name: format!("racer{i}"),
                            description: None,
                            created_at: chrono::Utc::now().to_rfc3339(),
                            last_used: None,
                        },
                    );
                    reg.save()
                })
            })
            .collect();

        let results: Vec<_> = threads
            .into_iter()
            .map(|t| t.join().expect("thread must not panic"))
            .collect();

        let failures: Vec<String> = results
            .iter()
            .filter_map(|r| r.as_ref().err().map(|e| format!("{e:#}")))
            .collect();
        assert!(
            failures.is_empty(),
            "{} of 8 concurrent saves failed — they race on the temp file (#976): {failures:#?}",
            failures.len(),
        );

        // The registry must still parse after the storm.
        let loaded = ProfileRegistry::load().expect("registry must parse after concurrent saves");
        assert!(
            !loaded.profiles.is_empty(),
            "at least one racer must have landed",
        );
    });
}
