//! User timezone resolution and caching (#153).
//!
//! Provides temporal grounding for agents by parsing user-preferred timezones
//! from `USER.md` (e.g. `Timezone: UTC+3 (MSK)` or `Europe/Paris`) and caching
//! the resolved [`chrono_tz::Tz`]. Keyed on `USER.md` mtime and content hash
//! to avoid disk/LLM overhead on subsequent turns while invalidating automatically
//! when `USER.md` is modified.

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use once_cell::sync::Lazy;
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::time::SystemTime;

/// Resolved timezone info with display label.
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

/// Tier 1: Fast synchronous parser for timezone declarations in text (USER.md).
/// Matches common formats:
/// - `Timezone: UTC+3 (MSK)` / `Timezone: UTC-5 (EST)`
/// - `Timezone: Europe/Moscow`
/// - `Часовой пояс: UTC+3 (MSK)` / `Часовой пояс: Москва (UTC+3)`
/// - `Timezone: America/New_York`
pub fn parse_timezone_heuristic(text: &str) -> Option<TzInfo> {
    for line in text.lines() {
        let Some((key, val)) = normalize_declaration(line) else {
            continue;
        };

        let key_lower = key.to_lowercase();
        if key_lower != "timezone" && key_lower != "часовой пояс" {
            continue;
        }

        if let Some(info) = parse_tz_value(&val) {
            return Some(info);
        }
    }

    // Secondary scan: check if any standalone line contains an explicit IANA or UTC offset pattern
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.contains("Europe/") || trimmed.contains("America/") || trimmed.contains("Asia/")
        {
            for word in trimmed.split_whitespace() {
                let clean_word = word.trim_matches(|c| {
                    c == '(' || c == ')' || c == '[' || c == ']' || c == '`' || c == ',' || c == '.'
                });
                if let Ok(tz) = clean_word.parse::<Tz>() {
                    return Some(TzInfo::new(tz, None));
                }
            }
        }
    }

    None
}

/// Normalise a `USER.md` declaration line into a `(key, value)` pair.
///
/// Handles the plain form (`Key: value`), markdown table rows
/// (`| Key | value |`), list markers (`- Key: value`), and bold/backtick/quote
/// wrappers around either side. Returns `None` when the line carries no
/// separator, so callers skip it without guessing.
///
/// Order matters: the table frame is stripped BEFORE the split. Splitting a row
/// like `| **Timezone** | UTC+3 (MSK) |` on its leading pipe yields an empty key
/// — that ordering is the defect this function exists to avoid.
fn normalize_declaration(line: &str) -> Option<(String, String)> {
    let mut s = line.trim();

    // Strip a markdown table frame: one leading and one trailing pipe.
    if let Some(rest) = s.strip_prefix('|') {
        s = rest.trim();
    }
    if let Some(rest) = s.strip_suffix('|') {
        s = rest.trim();
    }

    // Strip list markers (`- `, `* `, `# `) and blockquote markers.
    s = s.trim_start_matches(['-', '*', '#', '>', ' ']).trim();

    // Split on the FIRST `:` or `|`, whichever comes first.
    let sep = s.find(|c| c == ':' || c == '|')?;
    let key = unwrap_declaration_wrappers(&s[..sep]);
    let value = unwrap_declaration_wrappers(&s[sep + 1..]);

    if key.is_empty() || value.is_empty() {
        return None;
    }

    Some((key, value))
}

/// Strip emphasis/code/quote wrappers and surrounding whitespace from one side
/// of a declaration, so `**Timezone**` and `Timezone` compare equal.
fn unwrap_declaration_wrappers(s: &str) -> String {
    s.trim()
        .trim_matches(|c| {
            c == '*' || c == '_' || c == '`' || c == '"' || c == '\'' || c == '|'
        })
        .trim()
        .to_string()
}

/// Parse a timezone value string into TzInfo.
fn parse_tz_value(val: &str) -> Option<TzInfo> {
    // Check for "UTC+3 (MSK)" or "UTC-5 (EST)"
    if let Some((utc_part, rest)) = val.split_once('(') {
        let label = rest.trim_end_matches(')').trim().to_string();
        let tz_candidate = utc_part.trim();
        if let Some(tz) = parse_utc_offset_or_iana(tz_candidate) {
            return Some(TzInfo::new(
                tz,
                if label.is_empty() { None } else { Some(label) },
            ));
        }
        // Maybe format is "Москва (UTC+3)"
        let inner = rest.trim_end_matches(')').trim();
        if let Some(tz) = parse_utc_offset_or_iana(inner) {
            let outer_label = utc_part.trim().to_string();
            return Some(TzInfo::new(
                tz,
                if outer_label.is_empty() {
                    None
                } else {
                    Some(outer_label)
                },
            ));
        }
    }

    // Direct IANA parse, e.g. "Europe/Paris"
    if let Ok(tz) = val.parse::<Tz>() {
        return Some(TzInfo::new(tz, None));
    }

    // Direct UTC offset parse, e.g. "UTC+3" or "+03:00"
    if let Some(tz) = parse_utc_offset_or_iana(val) {
        return Some(TzInfo::new(tz, None));
    }

    None
}

/// Parse UTC offset (e.g. "UTC+3", "UTC-5", "UTC+03:00", "+03") or direct IANA string.
fn parse_utc_offset_or_iana(s: &str) -> Option<Tz> {
    let s = s.trim();
    if let Ok(tz) = s.parse::<Tz>() {
        return Some(tz);
    }

    let upper = s.to_uppercase();
    let offset_str = upper
        .strip_prefix("UTC")
        .or_else(|| upper.strip_prefix("GMT"))
        .unwrap_or(&upper)
        .trim();

    // Map common fixed offsets to Etc/GMT offsets.
    // Note: In POSIX/IANA Etc/GMT zones, the sign is inverted (Etc/GMT-3 is UTC+3).
    let sign_and_num = offset_str
        .strip_prefix('+')
        .map(|num| (1, num))
        .or_else(|| offset_str.strip_prefix('-').map(|num| (-1, num)))?;

    let hours_str = sign_and_num.1.split(':').next()?.trim();
    let hours: i32 = hours_str.parse().ok()?;

    if !(0..=14).contains(&hours) {
        return None;
    }

    let total_offset = sign_and_num.0 * hours;
    etc_gmt_for_offset(total_offset)
}

/// Convert standard UTC hour offset (e.g. +3 for MSK) to chrono_tz::Tz.
fn etc_gmt_for_offset(offset: i32) -> Option<Tz> {
    // POSIX Etc/GMT signs are inverted: UTC+3 is Etc/GMT-3, UTC-5 is Etc/GMT+5.
    match offset {
        0 => Some(Tz::UTC),
        1 => Some(Tz::Etc__GMTMinus1),
        2 => Some(Tz::Etc__GMTMinus2),
        3 => Some(Tz::Etc__GMTMinus3),
        4 => Some(Tz::Etc__GMTMinus4),
        5 => Some(Tz::Etc__GMTMinus5),
        6 => Some(Tz::Etc__GMTMinus6),
        7 => Some(Tz::Etc__GMTMinus7),
        8 => Some(Tz::Etc__GMTMinus8),
        9 => Some(Tz::Etc__GMTMinus9),
        10 => Some(Tz::Etc__GMTMinus10),
        11 => Some(Tz::Etc__GMTMinus11),
        12 => Some(Tz::Etc__GMTMinus12),
        -1 => Some(Tz::Etc__GMTPlus1),
        -2 => Some(Tz::Etc__GMTPlus2),
        -3 => Some(Tz::Etc__GMTPlus3),
        -4 => Some(Tz::Etc__GMTPlus4),
        -5 => Some(Tz::Etc__GMTPlus5),
        -6 => Some(Tz::Etc__GMTPlus6),
        -7 => Some(Tz::Etc__GMTPlus7),
        -8 => Some(Tz::Etc__GMTPlus8),
        -9 => Some(Tz::Etc__GMTPlus9),
        -10 => Some(Tz::Etc__GMTPlus10),
        -11 => Some(Tz::Etc__GMTPlus11),
        -12 => Some(Tz::Etc__GMTPlus12),
        _ => None,
    }
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
