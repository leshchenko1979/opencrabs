//! Cron admission-cap tests for catch-up herd protection (#511).

use crate::config::CronConfig;
use std::sync::Arc;
use tokio::sync::Semaphore;

#[test]
fn cron_config_defaults_to_two_concurrent_turns() {
    assert_eq!(CronConfig::default().max_concurrent_turns, 2);
}

#[test]
fn cron_config_accepts_explicit_admission_cap() {
    let config: CronConfig = toml::from_str("max_concurrent_turns = 7")
        .expect("cron admission cap should parse");
    assert_eq!(config.max_concurrent_turns, 7);
}

#[tokio::test]
async fn zero_configured_capacity_is_clamped_to_one() {
    let configured = 0_u32;
    let admission = Arc::new(Semaphore::new(configured.max(1) as usize));
    let permit = admission
        .clone()
        .acquire_owned()
        .await
        .expect("clamped admission must provide one permit");
    assert_eq!(admission.available_permits(), 0);
    drop(permit);
    assert_eq!(admission.available_permits(), 1);
}

#[tokio::test]
async fn admission_serializes_the_second_turn_until_release() {
    let admission = Arc::new(Semaphore::new(1));
    let first = admission
        .clone()
        .acquire_owned()
        .await
        .expect("first turn should be admitted");
    let second = admission.clone();
    let waiter = tokio::spawn(async move {
        let _permit = second
            .acquire_owned()
            .await
            .expect("second turn should eventually be admitted");
        true
    });

    tokio::task::yield_now().await;
    assert!(!waiter.is_finished(), "second turn bypassed the admission cap");

    drop(first);
    assert!(waiter.await.expect("waiter task should complete"));
}
