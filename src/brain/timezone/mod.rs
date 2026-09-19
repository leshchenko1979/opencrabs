//! User timezone resolution and caching (#153).
//!
//! Provides temporal grounding for agents by parsing user-preferred timezones
//! from `USER.md` (e.g. `Timezone: UTC+3 (MSK)` or `Europe/Paris`) and caching
//! the resolved [`chrono_tz::Tz`]. Keyed on `USER.md` mtime and content length
//! to avoid disk/LLM overhead on subsequent turns while invalidating
//! automatically when `USER.md` is modified.
//!
//! Wiring only — module declarations and re-exports, no function bodies
//! (CODE.md §Module Declarations). The public paths `crate::brain::timezone::{…}`
//! are unchanged by the split, so no call site moves.

mod cache;
mod parse;

pub use cache::{
    format_utc_time, resolve_active_tz, TzInfo, UserTimezoneCache, GLOBAL_TZ_CACHE,
};
pub use parse::parse_timezone_heuristic;
