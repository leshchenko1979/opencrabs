//! Wake a lane whose declared wait outlived the thing it waited for (#344).
//!
//! A lane that ends its turn on an EXTERNAL completion — a CI run, a peer lane,
//! an owner decision — leaves no journal row, no open turn and no goal behind.
//! Its only evidence is the await record it declares on its binding
//! (`await_kind` / `await_ref` / `await_at`), and the boot classifier consults
//! that record on every restart. But a boot only happens when the process
//! restarts: a lane parked on a run that then DIES waits forever inside a
//! healthy daemon, and nothing notices.
//!
//! This is what notices. A timer, not a queue: the await record already says
//! who is waiting and since when, so there is no state to keep here and nothing
//! to lose across a restart — the same reasoning that shapes
//! `crate::memory::backfill_sweep`.
//!
//! ## Why config is re-read every tick
//!
//! `Config::load()` parses config.toml on each call, so a key fixed mid-session
//! is picked up on the next tick with no cache to invalidate and no reload
//! plumbing. That is also why the interval is resolved per tick rather than
//! captured once: changing it takes effect within one period instead of at the
//! next restart.
//!
//! ## Single flight
//!
//! A tick that lands while the previous pass is still querying skips rather
//! than stacking. Two passes over the same rows would select the same lane
//! twice, and the second selection would wake a lane the first one is already
//! waking.
//!
//! ## Fail-open
//!
//! Every error path logs and stands down for this tick: a sweep that cannot
//! read the table must not take the daemon with it. The one failure that would
//! be permanent is the single-flight flag itself — a pass that died holding it
//! would skip every tick forever without saying so, which is precisely the
//! silent-stall class this module exists to remove. Hence [`FlightGuard`],
//! which releases the slot on an unwind as well as on the happy path.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use uuid::Uuid;

use super::TelegramState;
use super::resume::{ResumeTargets, short_session_id, spawn_resumes};
use crate::brain::agent::AgentService;
use crate::config::Config;
use crate::config::types::TelegramConfig;
use crate::db::{Pool, SessionBinding, SessionBindingRepository};

/// The channel whose await records this sweep serves. The module lives under
/// `channels::telegram`, and `awaiting_for_channel` is channel-keyed because
/// the record is a property of the binding, not of the session — a session
/// rebound to another surface must not be woken into the wrong one.
const AWAIT_CHANNEL: &str = "telegram";

/// Set while a sweep pass is running, so a tick that lands mid-pass skips
/// rather than running a second selection over the same rows.
static SWEEPING: AtomicBool = AtomicBool::new(false);

/// The prompt that frames a sweep wake.
///
/// It names the two possible states honestly — landed, or still coming — and
/// asks the lane to re-declare in the second case, which is what makes the
/// pass safe to consume the record (see [`sweep_inner`]).
const AWAIT_WAKE_PROMPT: &str = "[System: You declared that you were waiting on \
an external completion — a run, a peer lane, or an owner decision — and that \
wait has now outlived its expected duration. The dependency may have died \
without telling you. Re-check it now: if it landed, continue from where you \
left off; if it is genuinely still coming, say so briefly and declare the wait \
again. Do not mention this reminder.]";

/// The sweep period for a given config, or `None` when the sweep is off.
///
/// Off means an explicit `await_sweep_interval_secs = 0`. Takes the config
/// rather than reading it, so the decision can be tested without a config.toml
/// on disk.
pub(crate) fn interval_for(cfg: &TelegramConfig) -> Option<Duration> {
    if cfg.await_sweep_interval_secs == 0 {
        return None;
    }
    Some(Duration::from_secs(cfg.await_sweep_interval_secs))
}

/// The sweep's period and patience for this install, or `None` when the sweep
/// is off.
///
/// One `Config::load()` for both knobs, so they can never be read from
/// different generations of the file. A config that cannot be read falls back
/// to the defaults rather than stopping the sweep: the sweep is the recovery
/// path, and a recovery path that disables itself because a file was briefly
/// unreadable is the failure it was built to prevent.
fn sweep_settings() -> Option<(Duration, i64)> {
    let cfg = Config::load()
        .map(|c| c.channels.telegram)
        .unwrap_or_default();
    interval_for(&cfg).map(|period| (period, cfg.await_stale_secs))
}

/// Start the periodic await sweep for this process.
///
/// Call once at boot, after the boot classifier has already had its pass, so a
/// lane recovered at startup is not also selected by the first tick.
pub(crate) fn spawn(pool: Pool, agent: Arc<AgentService>, telegram_state: Arc<TelegramState>) {
    tokio::spawn(async move {
        loop {
            // Resolved per iteration rather than once outside the loop: an
            // interval edited in config.toml then takes effect within one
            // period, and turning the sweep off stops it without a restart.
            let Some((period, stale_secs)) = sweep_settings() else {
                tracing::debug!("Await sweep (#344): disabled by config, stopping");
                return;
            };
            tokio::time::sleep(period).await;
            // Cloned per tick rather than captured once: the closure below is
            // consumed by `run_once`, so each pass needs its own handles.
            let tick_agent = agent.clone();
            let tick_state = telegram_state.clone();
            run_once(&pool, stale_secs, move |targets| {
                spawn_resumes(targets, AWAIT_WAKE_PROMPT, tick_agent, tick_state);
            })
            .await;
        }
    });
}

/// One sweep pass: wake every lane whose declared wait has outlived
/// `stale_secs`, and return how many were woken.
///
/// `wake` receives the whole batch rather than being called per lane, so the
/// pass has exactly ONE wake boundary — and so a test can drive the real
/// control flow (single flight, staleness filter, record consumption) against
/// a temp DB with a recorder in place of a live bot. A test that instead
/// re-implemented the selection would prove nothing about this function.
pub(crate) async fn run_once<W>(pool: &Pool, stale_secs: i64, wake: W) -> usize
where
    W: FnOnce(ResumeTargets),
{
    if !try_enter() {
        tracing::debug!("Await sweep (#344): already in flight, skipping");
        return 0;
    }
    let _flight = FlightGuard;

    let stale = sweep_inner(pool, stale_secs).await;
    let woken = stale.len();
    // Logged only when there was work. A five-minute timer that reports
    // "0 lanes awaiting" forever is noise that trains people to stop reading
    // the log, which is the same blindness this module fixes.
    if woken > 0 {
        wake(stale);
        tracing::info!(
            target: "telegram",
            "Await sweep (#344): woke {woken} lane(s) whose declared wait outlived {stale_secs}s"
        );
    }
    woken
}

/// The awaiting rows this pass wakes.
///
/// Pure over the rows it is handed and takes the clock as a parameter rather
/// than calling `Utc::now()`, so the staleness boundary is testable without a
/// timer or a config file.
///
/// A wait exactly `stale_secs` old is still inside its patience — the
/// comparison is strictly older-than, so the knob means what it says. A record
/// stamped in the FUTURE (a clock step backwards) is likewise not stale, which
/// is the safe direction: a wait is woken late rather than pre-empted.
///
/// A row with no `await_at` is not awaiting at all. The SQL predicate already
/// excludes those, but restating it here keeps this function correct on its own
/// terms rather than only in the presence of its caller's query.
pub(crate) fn stale_bindings(
    bindings: Vec<SessionBinding>,
    now: i64,
    stale_secs: i64,
) -> Vec<SessionBinding> {
    bindings
        .into_iter()
        .filter(|b| matches!(b.await_at, Some(at) if now - at > stale_secs))
        .collect()
}

/// Claim the single-flight slot. `false` means another pass holds it and this
/// tick must skip rather than select the same rows alongside it.
///
/// Split out from [`run_once`] so the skip can be tested without a database:
/// the alternative is a test that drives a real pass, which proves the guard
/// only incidentally.
pub(crate) fn try_enter() -> bool {
    SWEEPING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// Release the single-flight slot.
pub(crate) fn leave() {
    SWEEPING.store(false, Ordering::Release);
}

/// Releases the single-flight slot when the pass ends — including when it ends
/// by unwinding. `leave()` on the happy path alone would leave a panicking pass
/// holding the flag, and the sweep would then skip every tick forever without
/// saying so.
struct FlightGuard;

impl Drop for FlightGuard {
    fn drop(&mut self) {
        leave();
    }
}

/// Select the stale awaits and consume their records.
async fn sweep_inner(pool: &Pool, stale_secs: i64) -> ResumeTargets {
    let repo = SessionBindingRepository::new(pool.clone());
    let bindings = match repo.awaiting_for_channel(AWAIT_CHANNEL).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                "Await sweep (#344): awaiting query failed ({e}) — standing down this tick"
            );
            return Vec::new();
        }
    };

    let stale = stale_bindings(bindings, chrono::Utc::now().timestamp(), stale_secs);
    let mut targets = ResumeTargets::with_capacity(stale.len());
    for b in stale {
        // Consume the record BEFORE the wake is spawned, so a lane is woken
        // once per DECLARATION rather than once per tick — a record left behind
        // would be re-selected on the next tick, forever. The window this opens
        // is one pass wide and self-healing: the wake prompt tells the lane to
        // declare the wait again if the dependency is genuinely still coming,
        // and a lane that has truly been abandoned is no worse off than before
        // this module existed.
        if let Err(e) = repo.clear_await(&b.session_id).await {
            tracing::warn!(
                "Await sweep (#344): could not consume the await record for {} ({e}) — leaving it for the next tick",
                b.session_id
            );
            continue;
        }
        let Ok(sid) = Uuid::parse_str(&b.session_id) else {
            tracing::warn!(
                "Await sweep (#344): binding holds a non-uuid session id ({}) — cannot route a wake to it",
                b.session_id
            );
            continue;
        };
        let Ok(chat_id) = b.chat_id.parse::<i64>() else {
            tracing::warn!(
                "Await sweep (#344): session {} has a non-numeric chat id ({}) — cannot route a wake to it",
                short_session_id(sid),
                b.chat_id
            );
            continue;
        };
        tracing::info!(
            target: "telegram",
            "Await sweep (#344): session {} has awaited {} (ref={}) for over {stale_secs}s — waking it",
            short_session_id(sid),
            b.await_kind.as_deref().unwrap_or("external"),
            b.await_ref.as_deref().unwrap_or("-")
        );
        targets.push((sid, chat_id, b.thread_id.map(i64::from)));
    }
    targets
}
