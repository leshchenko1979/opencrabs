//! Multi-profile log routing and cron scheduler trace tests (#184).
//!
//! Verifies that `ResilientFileWriter` dynamically routes log events based on
//! task-local profile home overrides, and that cron execution and delivery
//! maintain profile log isolation.

use std::io::Write;
use std::sync::Arc;
use tracing_subscriber::fmt::writer::MakeWriter;

use crate::config::profile::{
    current_profile_name, profile_home_override, with_home_override, with_home_override_async,
    with_profile_home_async,
};
use crate::logging::ResilientFileWriter;

#[test]
fn profile_home_override_returns_none_when_unset() {
    assert_eq!(profile_home_override(), None);
}

#[test]
fn profile_home_override_returns_some_inside_scope() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().to_path_buf();
    let captured = with_home_override(target.clone(), profile_home_override);
    assert_eq!(captured, Some(target));
    assert_eq!(profile_home_override(), None);
}

#[tokio::test]
async fn profile_home_override_returns_some_inside_async_scope() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().to_path_buf();
    let captured =
        with_home_override_async(target.clone(), async { profile_home_override() }).await;
    assert_eq!(captured, Some(target));
    assert_eq!(profile_home_override(), None);
}

#[tokio::test]
async fn resilient_file_writer_routes_by_task_local_profile_scope() {
    let tmp_default = tempfile::tempdir().expect("tempdir default");
    let tmp_profile = tempfile::tempdir().expect("tempdir profile");

    let writer =
        ResilientFileWriter::new(tmp_default.path().to_path_buf(), "opencrabs".to_string());

    // 1. Write without override — routes to default_log_dir
    {
        let mut w = writer.make_writer();
        w.write_all(b"default daemon line\n")
            .expect("write default");
        w.flush().expect("flush default");
    }

    // 2. Write with task-local override — routes to tmp_profile/logs
    with_home_override_async(tmp_profile.path().to_path_buf(), async {
        let mut w = writer.make_writer();
        w.write_all(b"secondary profile cron line\n")
            .expect("write profile");
        w.flush().expect("flush profile");
    })
    .await;

    // Check default dir contains "default daemon line"
    let default_has_content = std::fs::read_dir(tmp_default.path())
        .expect("read default dir")
        .filter_map(Result::ok)
        .any(|e| {
            std::fs::read_to_string(e.path())
                .map(|c| c.contains("default daemon line"))
                .unwrap_or(false)
        });
    assert!(
        default_has_content,
        "default directory must receive events without override"
    );

    // Default dir must NOT contain secondary profile line
    let default_has_profile_line = std::fs::read_dir(tmp_default.path())
        .expect("read default dir")
        .filter_map(Result::ok)
        .any(|e| {
            std::fs::read_to_string(e.path())
                .map(|c| c.contains("secondary profile cron line"))
                .unwrap_or(false)
        });
    assert!(
        !default_has_profile_line,
        "default directory must NOT receive secondary profile events"
    );

    // Profile logs dir (tmp_profile/logs) must contain "secondary profile cron line"
    let profile_logs_dir = tmp_profile.path().join("logs");
    assert!(
        profile_logs_dir.exists(),
        "profile logs directory must be created"
    );
    let profile_has_content = std::fs::read_dir(&profile_logs_dir)
        .expect("read profile logs dir")
        .filter_map(Result::ok)
        .any(|e| {
            std::fs::read_to_string(e.path())
                .map(|c| c.contains("secondary profile cron line"))
                .unwrap_or(false)
        });
    assert!(
        profile_has_content,
        "profile logs directory must receive scoped events"
    );
}

#[tokio::test]
async fn resilient_file_writer_multi_dir_concurrent_writes() {
    let tmp_default = tempfile::tempdir().expect("tempdir default");
    let tmp_a = tempfile::tempdir().expect("tempdir A");
    let tmp_b = tempfile::tempdir().expect("tempdir B");

    let writer = Arc::new(ResilientFileWriter::new(
        tmp_default.path().to_path_buf(),
        "test".to_string(),
    ));

    let mut handles = Vec::new();

    // Spawn tasks writing to default, A, and B concurrently
    for i in 0..15 {
        let w = Arc::clone(&writer);
        let path_a = tmp_a.path().to_path_buf();
        let path_b = tmp_b.path().to_path_buf();

        handles.push(tokio::spawn(async move {
            if i % 3 == 0 {
                let mut guard = w.make_writer();
                guard
                    .write_all(format!("default write {i}\n").as_bytes())
                    .unwrap();
            } else if i % 3 == 1 {
                with_home_override_async(path_a, async {
                    let mut guard = w.make_writer();
                    guard
                        .write_all(format!("scope A write {i}\n").as_bytes())
                        .unwrap();
                })
                .await;
            } else {
                with_home_override_async(path_b, async {
                    let mut guard = w.make_writer();
                    guard
                        .write_all(format!("scope B write {i}\n").as_bytes())
                        .unwrap();
                })
                .await;
            }
        }));
    }

    for h in handles {
        h.await.expect("task completed");
    }

    assert!(tmp_a.path().join("logs").exists());
    assert!(tmp_b.path().join("logs").exists());
}

#[tokio::test]
async fn with_profile_home_async_sets_current_profile_name_and_home() {
    assert_eq!(current_profile_name(), "default");

    with_profile_home_async(Some("worker-lane"), async {
        assert_eq!(current_profile_name(), "worker-lane");
        let home = profile_home_override().expect("profile home override set");
        assert!(home.ends_with("profiles/worker-lane"));
    })
    .await;

    assert_eq!(current_profile_name(), "default");
    assert_eq!(profile_home_override(), None);
}
