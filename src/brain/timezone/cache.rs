//! Resolved timezone type, dual-time formatting, and the mtime-keyed cache.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use once_cell::sync::Lazy;
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::time::SystemTime;

use super::parse::parse_timezone_heuristic;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TzInfo {
    pub tz: Tz,
    pub label: Option<String>,
}

impl TzInfo {
    pub fn new(tz: Tz, label: Option<String>) -> Self {
        Self { tz, label }
    }

    /// Format a UTC timestamp into dual UTC + local representation:
    /// `YYYY-MM-DD HH:MM:SS UTC (user: HH:MM:SS TZ)`
    pub fn format_dual_time(&self, dt: &DateTime<Utc>) -> String {
        let utc_str = dt.format("%Y-%m-%d %H:%M:%S UTC");
        let local_dt = dt.with_timezone(&self.tz);
        let tz_label = self.label.as_deref().unwrap_or_else(|| self.tz.name());
        let local_str = local_dt.format("%H:%M:%S");
        format!("{utc_str} (user: {local_str} {tz_label})")
    }
}

/// Format UTC-only time marker string.
pub fn format_utc_time(dt: &DateTime<Utc>) -> String {
    format!("{} UTC", dt.format("%Y-%m-%d %H:%M:%S"))
}
/// In-memory cache holding resolved user timezone, keyed by file mtime and size.
#[derive(Debug, Default)]
pub struct UserTimezoneCache {
    inner: Mutex<Option<CachedEntry>>,
}

#[derive(Debug, Clone)]
struct CachedEntry {
    mtime: Option<SystemTime>,
    file_len: u64,
    resolved: Option<TzInfo>,
}

impl UserTimezoneCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }

    /// Resolve timezone from `USER.md` in the given brain directory.
    /// If cached and `USER.md` mtime/size unchanged, returns cached without disk read.
    pub fn resolve_from_brain_dir(&self, brain_dir: &Path) -> Option<TzInfo> {
        let user_md_path = brain_dir.join("USER.md");
        self.resolve_from_file(&user_md_path)
    }

    /// Resolve timezone from a specific `USER.md` file path.
    pub fn resolve_from_file(&self, path: &Path) -> Option<TzInfo> {
        let meta = fs::metadata(path).ok();
        let current_mtime = meta.as_ref().and_then(|m| m.modified().ok());
        let current_len = meta.as_ref().map(|m| m.len()).unwrap_or(0);

        {
            let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(ref entry) = *guard
                && entry.mtime == current_mtime
                && entry.file_len == current_len
            {
                return entry.resolved.clone();
            }
        }

        // Needs re-read or initial read
        let resolved = if let Ok(content) = fs::read_to_string(path) {
            parse_timezone_heuristic(&content)
        } else {
            None
        };

        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(CachedEntry {
            mtime: current_mtime,
            file_len: current_len,
            resolved: resolved.clone(),
        });

        resolved
    }

    /// Update or seed the cache explicitly (e.g. after Tier-2 LLM extraction).
    pub fn set_explicit(&self, path: &Path, resolved: Option<TzInfo>) {
        let meta = fs::metadata(path).ok();
        let current_mtime = meta.as_ref().and_then(|m| m.modified().ok());
        let current_len = meta.as_ref().map(|m| m.len()).unwrap_or(0);

        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(CachedEntry {
            mtime: current_mtime,
            file_len: current_len,
            resolved,
        });
    }

    /// Invalidate cache manually (e.g. on test teardown or explicit notification).
    pub fn invalidate(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        *guard = None;
    }
}

/// Process-wide singleton cache for user timezone.
pub static GLOBAL_TZ_CACHE: Lazy<UserTimezoneCache> = Lazy::new(UserTimezoneCache::new);

/// Resolve the ACTIVE profile's timezone from its brain directory (#349).
///
/// Single entry point for channel surfaces that need the user's timezone. It
/// owns the profile-directory join so callers cannot leak a borrow of a
/// temporary into the cache lookup, and returns `None` when no profile is
/// active.
pub fn resolve_active_tz() -> Option<TzInfo> {
    crate::config::profile::active_profile()
        .map(|name| {
            crate::config::profile::base_opencrabs_dir()
                .join("profiles")
                .join(name)
        })
        .and_then(|dir| GLOBAL_TZ_CACHE.resolve_from_brain_dir(&dir))
}
