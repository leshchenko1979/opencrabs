//! Background maintenance service (#241).
//!
//! Coordinates periodic database and memory maintenance:
//! - Sub-agent session TTL expiration and plan file pruning
//! - Memory store GC: orphan vector embeddings, unreferenced content chunks, stale symbols/call_edges
//! - Periodic SQLite `VACUUM` on `memory.db` and `opencrabs.db`

use anyhow::{Context, Result};
use chrono::Utc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::db::MaintenanceKnobs;
use crate::services::context::ServiceContext;
use crate::services::session::SessionService;

/// Set while a maintenance sweep is running, so concurrent ticks skip.
static RUNNING_MAINTENANCE: AtomicBool = AtomicBool::new(false);

/// Spawned-once guard for the memory reclaim ticker (#522).
static MEMORY_RECLAIM_SPAWNED: AtomicBool = AtomicBool::new(false);

/// When the reclaim first became due and was deferred, so the starvation cap
/// has a start point. `None` while nothing is owed (#522).
static RECLAIM_DEBT_SINCE: Mutex<Option<Instant>> = Mutex::new(None);

/// How often the memory reclaim ticker wakes (#522).
pub const MEMORY_RECLAIM_TICK: Duration = Duration::from_secs(300);

/// How long the fleet must be quiet before a reclaim tick may run (#522).
pub const MEMORY_RECLAIM_QUIET_FOR: Duration = Duration::from_secs(60);

/// Hard cap on how long a reclaim may be deferred by continuous activity
/// (#522). Past this the batch runs anyway, so a chat that is never quiet
/// cannot starve the reclaim — the arm that makes the gate safe to ship.
pub const MEMORY_RECLAIM_MAX_DELAY: Duration = Duration::from_secs(1800);

/// How soon the 24 h sweep retries after it lost the single-flight permit
/// (#838).
///
/// The reclaim ticker and the sweep are both spawned in the same bootstrap
/// with the same 1800 s warmup, so their first wake is simultaneous by
/// construction: the tick takes the shared permit first and the sweep skips.
/// Waiting the full interval after a skip then costs a whole day of
/// housekeeping — measured on `ops`, 32 µs apart, no sweep for that
/// incarnation. A contended start retries on this short interval instead.
///
/// MUST stay strictly BELOW [`MEMORY_RECLAIM_TICK`]. A retry equal to the tick
/// is phase-locked to it: the sweep wakes exactly when the ticker does, loses
/// the same race, and retries into the same collision forever — measured on
/// `ops` at 300 s, losing at 05:11:42 and 05:16:42 (425 µs and 960 µs behind).
/// Retrying faster than the ticker guarantees attempts that land BETWEEN
/// ticks, where the permit is free. A test in
/// `src/tests/memory_maintenance_test.rs` pins the inequality.
pub const MAINTENANCE_CONTENDED_RETRY: Duration = Duration::from_secs(60);

/// Delay before the next 24 h sweep attempt.
///
/// A run that never happened (permit held by the reclaim ticker) must not cost
/// the full interval; a run that did happen waits it. Pure, so the choice is
/// pinable without a clock or a process-global flag (#838).
pub fn next_sweep_delay(skipped: bool, interval: Duration) -> Duration {
    if skipped {
        MAINTENANCE_CONTENDED_RETRY
    } else {
        interval
    }
}

/// Summary report of a maintenance run.
#[derive(Debug, Clone, Default)]
pub struct MaintenanceReport {
    /// `true` when the sweep never ran because another maintenance run held
    /// the single-flight permit (#838). The caller must retry on a short
    /// interval rather than the full one, and it must be able to tell this
    /// apart from a real sweep that simply found nothing to prune.
    pub skipped: bool,
    /// Number of expired sub-agent sessions pruned.
    pub subagents_pruned: usize,
    /// Number of expired messages pruned (#278).
    pub messages_pruned: usize,
    /// Number of `feedback_ledger` rows pruned (#836).
    pub feedback_ledger_pruned: usize,
    /// Memory GC report (if memory store is available).
    pub memory_gc: Option<crate::memory::MemoryGcReport>,
    /// Whether the bulk memory GC was skipped because the freelist was already
    /// at or above `memory_gc_freelist_cap` (#522).
    pub memory_gc_skipped: bool,
    /// Whether `opencrabs.db` was vacuumed.
    pub database_vacuumed: bool,
    /// Whether `memory.db` was vacuumed.
    pub memory_vacuumed: bool,
}

/// Service managing periodic database & memory maintenance sweeps.
pub struct MaintenanceService {
    context: ServiceContext,
}

impl MaintenanceService {
    pub fn new(context: ServiceContext) -> Self {
        Self { context }
    }

    /// Prune `feedback_ledger` rows older than `retention_days` (#836).
    ///
    /// `retention_days == 0` disables pruning and returns `Ok(0)`, matching
    /// `SessionService::prune_expired_messages` — so an upgrade never deletes
    /// ledger history by itself.
    ///
    /// `feedback_ledger.created_at` is TEXT ISO8601 (`%Y-%m-%dT%H:%M:%SZ`), so
    /// the cutoff is formatted identically and the DELETE's range walks
    /// `idx_feedback_ledger_created` instead of scanning the table.
    pub(crate) async fn prune_expired_feedback_ledger(
        &self,
        retention_days: u32,
    ) -> Result<usize> {
        if retention_days == 0 {
            return Ok(0);
        }
        let cutoff = (Utc::now() - chrono::Duration::days(i64::from(retention_days)))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let repo = crate::db::repository::feedback_ledger::FeedbackLedgerRepository::new(
            self.context.pool(),
        );
        repo.prune_older_than(cutoff)
            .await
            .context("Failed to prune expired feedback ledger")
    }

    /// Run one complete maintenance cycle across databases, with the default
    /// knobs.
    pub async fn run_maintenance(&self) -> Result<MaintenanceReport> {
        self.run_maintenance_with(MaintenanceKnobs::default()).await
    }

    /// Run one complete maintenance cycle across databases (#522).
    ///
    /// Takes the knobs rather than reading them inside, so the memory freelist
    /// gate and the reclaim budget are always the same numbers.
    pub async fn run_maintenance_with(&self, knobs: MaintenanceKnobs) -> Result<MaintenanceReport> {
        // A permit, not a bare try_enter/leave pair: an unwind inside the body
        // would otherwise leave the flag set for the process lifetime and
        // silently disable every later sweep (#522).
        let Some(_permit) = enter_maintenance() else {
            tracing::debug!("Maintenance sweep: already in flight, skipping");
            // #838: flag the skip. Returning a bare default made "the sweep was
            // starved" indistinguishable from "the sweep ran and found nothing",
            // so the 24 h loop could not tell that a whole day was lost.
            return Ok(MaintenanceReport {
                skipped: true,
                ..Default::default()
            });
        };

        self.run_maintenance_inner(knobs).await
    }

    /// The sweep body. `pub(crate)` so a test can drive it with injected knobs
    /// without contending for the process-global single-flight flag.
    pub(crate) async fn run_maintenance_inner(
        &self,
        knobs: MaintenanceKnobs,
    ) -> Result<MaintenanceReport> {
        let mut report = MaintenanceReport::default();
        let config = crate::config::Config::load().unwrap_or_default();

        // 1. Prune expired sub-agent sessions
        let session_svc = SessionService::new(self.context.clone());
        let ttl_days = config.agent.subagent_session_ttl_days;
        match session_svc.prune_expired_subagent_sessions(ttl_days).await {
            Ok(pruned) => report.subagents_pruned = pruned,
            Err(e) => tracing::warn!("Maintenance: failed to prune subagent sessions: {e:#}"),
        }

        // 1b. Prune expired messages (#278)
        let msg_retention_days = config.agent.message_retention_days;
        match session_svc.prune_expired_messages(msg_retention_days).await {
            Ok(pruned) => report.messages_pruned = pruned,
            Err(e) => tracing::warn!("Maintenance: failed to prune expired messages: {e:#}"),
        }

        // 1c. Prune the feedback ledger (#836)
        //
        // `feedback_ledger` is append-only and had NO pruning path, so a busy
        // profile grew it past 1M rows / 3.75 GiB and every `feedback_analyze`,
        // RSI digest and `self_improve` call aggregated over the whole table —
        // `tool_%` is 90 % of the rows, so no index makes those scans selective.
        let ledger_retention_days = config.agent.feedback_ledger_retention_days;
        match self
            .prune_expired_feedback_ledger(ledger_retention_days)
            .await
        {
            Ok(pruned) => report.feedback_ledger_pruned = pruned,
            Err(e) => tracing::warn!("Maintenance: failed to prune feedback ledger: {e:#}"),
        }

        // 2. Memory Store GC & Vacuum
        //
        // Two separate lock acquisitions on purpose: merging them would hold the
        // Store lock across both steps and lengthen the longest hold on the 24 h
        // path, which is the opposite of the goal (#522).
        if let Ok(store_mutex) = crate::memory::get_store() {
            let gc_res = {
                let store = store_mutex
                    .lock()
                    .map_err(|e| anyhow::anyhow!("Store lock poisoned: {e}"))?;
                let freelist = store.freelist_count();
                if freelist >= knobs.memory_gc_freelist_cap {
                    // gc_orphans is a bulk DELETE: it can add GiB of freelist in
                    // one sweep while the bounded reclaim removes 16 MiB. Once
                    // the freelist is already this large, more holes are not
                    // what the file needs — so skip the GC, keep the reclaim.
                    report.memory_gc_skipped = true;
                    tracing::warn!(
                        freelist = freelist,
                        cap = knobs.memory_gc_freelist_cap,
                        "Memory GC skipped: freelist over cap (#522); reclaim still runs"
                    );
                    None
                } else {
                    Some(
                        store
                            .gc_orphans()
                            .map_err(|e| anyhow::anyhow!("gc_orphans error: {e}"))?,
                    )
                }
            };

            if let Some(gc_report) = gc_res {
                report.memory_gc = Some(gc_report);
            }

            let vac_res = {
                let store = store_mutex
                    .lock()
                    .map_err(|e| anyhow::anyhow!("Store lock poisoned: {e}"))?;
                store
                    .vacuum_memory_with(knobs)
                    .map_err(|e| anyhow::anyhow!("vacuum_memory error: {e}"))
            };

            match vac_res {
                Ok(vacuumed) => report.memory_vacuumed = vacuumed,
                Err(e) => tracing::warn!("Maintenance: memory vacuum failed: {e:#}"),
            }
        }

        // 3. Database Vacuum
        let db = crate::db::Database::new_with_pool(self.context.pool());
        match db.vacuum_database().await {
            Ok(vacuumed) => report.database_vacuumed = vacuumed,
            Err(e) => tracing::warn!("Maintenance: database vacuum failed: {e:#}"),
        }

        tracing::info!(
            subagents_pruned = report.subagents_pruned,
            messages_pruned = report.messages_pruned,
            feedback_ledger_pruned = report.feedback_ledger_pruned,
            database_vacuumed = report.database_vacuumed,
            memory_vacuumed = report.memory_vacuumed,
            memory_gc_skipped = report.memory_gc_skipped,
            "Maintenance sweep completed"
        );

        Ok(report)
    }

    /// Spawn periodic background maintenance task.
    pub fn spawn_periodic(context: ServiceContext, interval: Duration) {
        tokio::spawn(async move {
            let svc = MaintenanceService::new(context);
            // Initial post-boot warmup delay (30 mins) so startup and tool execution are not bottlenecked (#273)
            tokio::time::sleep(Duration::from_secs(1800)).await;

            loop {
                let skipped = match svc.run_maintenance().await {
                    Ok(report) => report.skipped,
                    Err(e) => {
                        tracing::warn!("Periodic maintenance run encountered error: {e:#}");
                        false
                    }
                };
                if skipped {
                    // #838: the reclaim ticker took the shared permit at the same
                    // instant (both warmups are 1800 s from the same bootstrap),
                    // so nothing ran. Retry soon instead of losing a day.
                    tracing::info!(
                        retry_secs = MAINTENANCE_CONTENDED_RETRY.as_secs(),
                        "Maintenance sweep skipped (permit held) — retrying on the short interval"
                    );
                }
                tokio::time::sleep(next_sweep_delay(skipped, interval)).await;
            }
        });
    }

    /// Spawn the memory-only reclaim ticker (#522).
    ///
    /// ONE bounded #321 batch per tick, skipped while the fleet is active. The
    /// 24 h sweep keeps owning the message prune, the main-DB reclaim and the
    /// memory GC; this exists because the holes the memory store accumulates
    /// outrun a 16 MiB/day budget by ~220x, so they must drain while the daemon
    /// stays up.
    ///
    /// No `ServiceContext`: the tick needs only the store. Spawned-once, like
    /// [`crate::memory::freshness::spawn_external_sweep`].
    pub fn spawn_memory_reclaim(interval: Duration) {
        if MEMORY_RECLAIM_SPAWNED.swap(true, Ordering::AcqRel) {
            return;
        }
        tokio::spawn(async move {
            // Seed the clock, so a silent daemon does not have to wait for its
            // first message before the quiet window can ever elapse.
            crate::brain::agent::service::session_routes::note_activity();
            // Same post-boot warmup as the 24 h sweep: startup and the first
            // tool calls must not compete with housekeeping (#273).
            tokio::time::sleep(Duration::from_secs(1800)).await;

            loop {
                reclaim_once().await;
                tokio::time::sleep(interval).await;
            }
        });
    }
}

pub(crate) fn try_enter() -> bool {
    RUNNING_MAINTENANCE
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

pub(crate) fn leave() {
    RUNNING_MAINTENANCE.store(false, Ordering::Release);
}

/// RAII single-flight permit for the maintenance sweep (#522).
///
/// The flag is process-global, so a bare `try_enter()` / `leave()` pair leaks
/// it for the process lifetime if the sweep body unwinds — silently disabling
/// the 24 h sweep AND the memory reclaim ticker together, with nothing to
/// notice. Dropping the permit releases it on every exit path.
pub(crate) struct MaintenancePermit;

impl Drop for MaintenancePermit {
    fn drop(&mut self) {
        leave();
    }
}

/// Take the maintenance single-flight permit, or `None` when a sweep is
/// already running.
pub(crate) fn enter_maintenance() -> Option<MaintenancePermit> {
    try_enter().then_some(MaintenancePermit)
}

/// Should a reclaim run now? Pure, so the window math is pinable without a
/// clock and without touching a process-global flag (#522).
///
/// `idle_for` is time since the last fleet activity — a turn in flight OR any
/// inbound message; `debt_for` is time since the reclaim first became due and
/// was deferred.
pub(crate) fn reclaim_is_due(mid_turn: bool, idle_for: Duration, debt_for: Duration) -> bool {
    crate::brain::agent::service::quiet_delivery::is_due(
        mid_turn,
        idle_for,
        debt_for,
        MEMORY_RECLAIM_QUIET_FOR,
        MEMORY_RECLAIM_MAX_DELAY,
    )
}

/// Record the first deferred tick, so the starvation cap has a start point.
fn arm_debt(now: Instant) {
    if let Ok(mut guard) = RECLAIM_DEBT_SINCE.lock()
        && guard.is_none()
    {
        *guard = Some(now);
    }
}

/// Nothing owed any more: drop the starvation start point.
fn clear_debt() {
    if let Ok(mut guard) = RECLAIM_DEBT_SINCE.lock() {
        *guard = None;
    }
}

/// One reclaim tick: decide, then run at most ONE bounded `#321` batch.
///
/// Panic-free by construction — every fallible call is matched and logged. A
/// panic here would kill the spawned loop silently and take the growth-stop
/// with it, and there is no supervisor to notice.
async fn reclaim_once() {
    let now = Instant::now();

    // The SHARED single-flight with the 24 h sweep: the two must never overlap,
    // and the permit releases on every exit path.
    let Some(_permit) = enter_maintenance() else {
        tracing::debug!("Memory reclaim: 24 h sweep in flight, skipping tick");
        return;
    };

    // The ticker's own writer of the activity clock. Its other writer is the
    // channel (telegram handler), so `idle_for` below measures quiet across
    // BOTH signals: a turn in flight, or any inbound message.
    let mid_turn = crate::brain::agent::service::session_routes::any_turn_in_flight();
    if mid_turn {
        crate::brain::agent::service::session_routes::note_activity();
    }
    let idle_for = crate::brain::agent::service::session_routes::last_activity()
        .map_or(Duration::ZERO, |t| now.duration_since(t));
    let debt_for = RECLAIM_DEBT_SINCE
        .lock()
        .ok()
        .and_then(|g| *g)
        .map_or(Duration::ZERO, |t| now.duration_since(t));
    let due = reclaim_is_due(mid_turn, idle_for, debt_for);

    // ONE decision line per tick: the gate's whole safety argument is these
    // four numbers, and a clock reset by inbound traffic is otherwise
    // unobservable. ~288 lines/day against a daemon log that has run 54 137
    // lines in an hour, so the instrument is far cheaper than the blindness.
    tracing::info!(
        mid_turn = mid_turn,
        idle_for_ms = idle_for.as_millis() as u64,
        debt_for_ms = debt_for.as_millis() as u64,
        due = due,
        "Memory reclaim tick"
    );

    if !due {
        arm_debt(now);
        return;
    }

    let store = match crate::memory::get_store() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("Memory reclaim: store unavailable: {e}");
            return;
        }
    };

    // `spawn_blocking` is mandatory: a `std::sync::Mutex` guard cannot be held
    // across `.await`.
    let res = tokio::task::spawn_blocking(move || {
        let Ok(guard) = store.try_lock() else {
            // A walk or a search holds it: skip this tick, never pile on.
            return Err("store busy".to_string());
        };
        if guard.freelist_count() < MaintenanceKnobs::default().min_freelist_pages {
            return Ok(false); // nothing owed; cheap no-op tick
        }
        guard.vacuum_memory()
    })
    .await;

    match res {
        Ok(Ok(_)) => clear_debt(),
        Ok(Err(e)) => tracing::warn!("Memory reclaim skipped: {e}"),
        Err(e) => tracing::warn!("Memory reclaim join failed: {e}"),
    }
}
