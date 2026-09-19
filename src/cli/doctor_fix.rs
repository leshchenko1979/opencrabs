//! `/doctor --fix` repair engine (#1114).
//!
//! Detection + repair of common stuck states. Every action returns what it
//! changed so the CLI output doubles as the audit log (what, where, why).

use crate::config::profile::InstanceOwner;
use anyhow::Context;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// One completed repair, rendered by `cmd_doctor`.
#[derive(Debug, PartialEq)]
pub(crate) struct FixReport {
    pub action: &'static str,
    pub detail: String,
}

/// Who a stuck-run sweep may declare dead (#332, D4).
///
/// The two callers of [`run_all`] sit in genuinely different positions, so the
/// policy is stated at the call site rather than inferred from an age: the
/// startup sweep has just taken the instance lock and therefore owns nothing,
/// while an explicit `--fix` may be racing a live daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearPolicy {
    /// The startup sweep: this process has just acquired the profile's instance
    /// lock, so it cannot own any pre-existing row — every `running` row is an
    /// orphan by definition, however young. Clearing them here, instead of
    /// after an age threshold, is what stops a restart orphan from being read
    /// as in-flight work by the #277 guard.
    OrphanedAtStartup,
    /// An explicit `--fix` on a possibly-live box: another process may be
    /// running a job right now, so a row is cleared only when no live instance
    /// owns the profile at all, or when it is past the age backstop.
    Conservative,
}

impl ClearPolicy {
    /// Does this policy declare every `running` row dead, whatever its age?
    fn clears_everything(self, owner: InstanceOwner) -> bool {
        match self {
            // A process that has just taken the lock owns no pre-existing row.
            Self::OrphanedAtStartup => true,
            // `Self_` counts as live: this process holds the lock, so a row it
            // wrote may still be executing.
            Self::Conservative => matches!(owner, InstanceOwner::None),
        }
    }

    /// Short name for the audit trail.
    fn label(self) -> &'static str {
        match self {
            Self::OrphanedAtStartup => "orphaned-at-startup",
            Self::Conservative => "conservative",
        }
    }
}

/// Why a row was marked interrupted: no live process can own it.
const ORPHANED_REASON: &str =
    "interrupted: cleared by doctor --fix (orphaned: no live process owns this run)";

/// Why a row was marked interrupted: it outlived the age backstop.
const AGED_REASON: &str = "interrupted: cleared by doctor --fix (no completion within max age)";

/// Clear every `running` row: nothing can own one.
const CLEAR_ORPHANS_SQL: &str = "UPDATE cron_job_runs SET status='interrupted', error=?1, \
     completed_at=strftime('%Y-%m-%dT%H:%M:%SZ','now') \
     WHERE status='running'";

/// Clear only the `running` rows that outlived the age backstop.
const CLEAR_AGED_SQL: &str = "UPDATE cron_job_runs SET status='interrupted', error=?1, \
     completed_at=strftime('%Y-%m-%dT%H:%M:%SZ','now') \
     WHERE status='running' AND started_at < ?2";

/// Clear cron run rows left in `running` by a process that is gone.
///
/// A crash between insert and mark-finish leaves `status='running'` forever,
/// and anything reading runs treats those as live work (#1114). Rows only
/// enter `running` via the insert path, which writes RFC3339 `+00:00`
/// timestamps, so a lexicographic comparison against a cutoff in the same
/// format is exact for exactly this population.
///
/// WHICH rows are orphans is decided by `policy` plus the live instance lock
/// (#332, D4) — see `clear_stuck_cron_runs_with_owner`, the core this reads
/// the owner for.
pub async fn clear_stuck_cron_runs(
    pool: &crate::db::Pool,
    policy: ClearPolicy,
    max_age_secs: i64,
) -> anyhow::Result<usize> {
    // The DB being swept and this lock resolve from the SAME profile, so the
    // owner read is about the very rows in question.
    let owner =
        crate::config::profile::instance_owner(&crate::config::profile::current_profile_name());
    clear_stuck_cron_runs_with_owner(pool, policy, owner, max_age_secs).await
}

/// Owner-injectable core of [`clear_stuck_cron_runs`] — the same split as
/// `instance_owner_in` (`src/config/profile.rs`), so a test states the owner
/// instead of writing the live `~/.opencrabs/locks/instance/<profile>.lock`.
pub(crate) async fn clear_stuck_cron_runs_with_owner(
    pool: &crate::db::Pool,
    policy: ClearPolicy,
    owner: InstanceOwner,
    max_age_secs: i64,
) -> anyhow::Result<usize> {
    let clear_everything = policy.clears_everything(owner);
    let (sql, reason) = if clear_everything {
        (CLEAR_ORPHANS_SQL, ORPHANED_REASON)
    } else {
        (CLEAR_AGED_SQL, AGED_REASON)
    };
    let mut params = vec![reason.to_string()];
    if !clear_everything {
        params.push((chrono::Utc::now() - chrono::Duration::seconds(max_age_secs)).to_rfc3339());
    }
    let n = pool
        .get()
        .await
        .context("Failed to get connection")?
        .interact(move |conn| conn.execute(sql, rusqlite::params_from_iter(params)))
        .await
        // InteractError doesn't implement std::error::Error, so map it manually
        .map_err(|_| anyhow::anyhow!("cron interact failed"))??;
    Ok(n)
}

/// Remove pre-init plan markers older than `max_age` under each root.
///
/// A `.opencrabs_plan_<uuid>.preinit` file means "entered plan intent, never
/// reached init"; a session that died there stays dead, so aged markers are
/// pure residue. Markers sit flat inside their session dir, so each root is
/// scanned one level deep. Fresh markers are never touched.
pub fn clear_stale_preinit_markers(roots: &[PathBuf], max_age: Duration) -> Vec<FixReport> {
    let mut removed = Vec::new();
    let cutoff = std::time::SystemTime::now()
        .checked_sub(max_age)
        .unwrap_or(std::time::UNIX_EPOCH);
    for root in roots {
        let entries = match std::fs::read_dir(root) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !is_preinit_marker(&path) {
                continue;
            }
            let stale = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| t < cutoff)
                .unwrap_or(false);
            if stale && std::fs::remove_file(&path).is_ok() {
                removed.push(FixReport {
                    action: "stale-preinit-marker",
                    detail: path.display().to_string(),
                });
            }
        }
    }
    removed
}

/// True for files named `.opencrabs_plan_<uuid>.preinit`.
fn is_preinit_marker(path: &Path) -> bool {
    path.is_file()
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with(".opencrabs_plan_") && n.ends_with(".preinit"))
            .unwrap_or(false)
}

/// Tighten brain/log files that are group- or world-accessible to 0600.
///
/// Brain files hold private context and logs can hold message content;
/// neither should ever be readable beyond the owner.
#[cfg(unix)]
pub fn fix_brain_log_permissions(home: &Path) -> Vec<FixReport> {
    use std::os::unix::fs::PermissionsExt;
    let mut fixed = Vec::new();
    for rel in ["brain", "logs"] {
        let entries = match std::fs::read_dir(home.join(rel)) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let mode = meta.permissions().mode();
            if mode & 0o077 != 0
                && std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).is_ok()
            {
                fixed.push(FixReport {
                    action: "permissions-tightened",
                    detail: path.display().to_string(),
                });
            }
        }
    }
    fixed
}

/// Age past which a `running` row is dead even on a box with a live owner.
///
/// Only [`ClearPolicy::Conservative`] consults it: at startup, orphanhood
/// settles the question outright. 4h sits above the longest legitimate run
/// measured for #332 (5534s) and below the oldest row that genuinely needed
/// reclaiming (33573s), so live work is never declared interrupted while real
/// residue is still collectable by this path.
pub const STUCK_CRON_MAX_AGE_SECS: i64 = 14400;

/// Pre-init markers older than this are residue: a session that entered
/// plan intent and never reached `init` within a week is not coming back.
pub const PREINIT_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);

/// Run every repair and return the combined report (the audit trail).
///
/// Cron rows need the pool; markers and permissions are pure filesystem.
/// `policy` is the caller's statement about whether it can own a pre-existing
/// `running` row (#332, D4) — the startup sweep cannot, an explicit `--fix`
/// on a live box can. Callers render the reports: `cmd_doctor` prints them,
/// the startup sweep logs them.
#[cfg_attr(not(unix), allow(unused_variables))]
pub async fn run_all(
    pool: &crate::db::Pool,
    marker_roots: &[PathBuf],
    home: &Path,
    policy: ClearPolicy,
) -> anyhow::Result<Vec<FixReport>> {
    let mut reports = Vec::new();
    let stuck = clear_stuck_cron_runs(pool, policy, STUCK_CRON_MAX_AGE_SECS).await?;
    if stuck > 0 {
        reports.push(FixReport {
            action: "stuck-cron-rows-cleared",
            detail: format!("{stuck} row(s) marked interrupted ({})", policy.label()),
        });
    }
    reports.extend(clear_stale_preinit_markers(marker_roots, PREINIT_MAX_AGE));
    #[cfg(unix)]
    reports.extend(fix_brain_log_permissions(home));
    Ok(reports)
}
