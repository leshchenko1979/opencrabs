//! Background maintenance service (#241).
//!
//! Coordinates periodic database and memory maintenance:
//! - Sub-agent session TTL expiration and plan file pruning
//! - Memory store GC: orphan vector embeddings, unreferenced content chunks, stale symbols/call_edges
//! - Periodic SQLite `VACUUM` on `memory.db` and `opencrabs.db`

use anyhow::{Context, Result};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::db::MaintenanceKnobs;
use crate::services::context::ServiceContext;
use crate::services::session::SessionService;
use crate::utils::scope::PriorityClass;

/// Set while a maintenance sweep is running, so concurrent ticks skip.
static RUNNING_MAINTENANCE: AtomicBool = AtomicBool::new(false);

/// Spawned-once guard for the memory reclaim ticker (#522).
static MEMORY_RECLAIM_SPAWNED: AtomicBool = AtomicBool::new(false);

/// Set when the startup sub-agent catch-up has pruned for this boot, so the
/// first 24 h sweep does not repeat the same prune (#842).
///
/// `subagent_session_ttl_days` had TWO schedulers pruning the same table — a
/// startup catch-up in `cli::ui` and the sweep. Under restart churn only the
/// startup leg ever ran, so the sweep's copy was dead weight; under a
/// long-lived daemon both ran and the same row set was enumerated twice a day.
/// The startup leg still exists (a daemon that never survives 24 h would
/// otherwise never prune at all), but it now records that it ran, and the
/// sweep's first cycle after a boot hands the work back to it.
static SUBAGENT_CATCHUP_DONE: AtomicBool = AtomicBool::new(false);

/// Record that the startup sub-agent catch-up pruned for this boot (#842).
pub(crate) fn note_subagent_catchup_done() {
    SUBAGENT_CATCHUP_DONE.store(true, Ordering::Release);
}

/// Consume the catch-up flag: `true` exactly once after a startup catch-up.
///
/// Swapping rather than reading is what makes the handover one-shot — the
/// first sweep after a boot skips its copy, every later sweep owns the prune
/// again.
pub(crate) fn take_subagent_catchup() -> bool {
    SUBAGENT_CATCHUP_DONE.swap(false, Ordering::AcqRel)
}

/// When the reclaim first became due and was deferred, so the starvation cap
/// has a start point. `None` while nothing is owed (#522).
static RECLAIM_DEBT_SINCE: Mutex<Option<Instant>> = Mutex::new(None);

/// How often the memory reclaim ticker wakes (#522).
pub const MEMORY_RECLAIM_TICK: Duration = Duration::from_secs(300);

/// Post-boot warmup before the first 24 h sweep (#273).
///
/// Startup and the first tool calls must not compete with housekeeping, so the
/// sweep waits 30 min before its first attempt.
pub const SWEEP_WARMUP: Duration = Duration::from_secs(1800);

/// How often the 24 h sweep wakes once it has run (#844).
///
/// This was a bare `86400` at the spawn call site in `cli::ui`, i.e. the
/// sweep's own period lived outside the module that owns the sweep. It belongs
/// beside the other timings so [`MaintenanceTiming`] can enforce the ordering
/// between them.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(86_400);

/// Extra delay the reclaim ticker adds to [`SWEEP_WARMUP`] before its first
/// tick (#840).
///
/// Both loops are spawned from the same bootstrap, so equal warmups make their
/// first wake simultaneous BY CONSTRUCTION — measured on `ops`: 32 µs apart,
/// the ticker took the shared single-flight permit first, and the sweep
/// skipped. Offsetting the ticker past the sweep is the structural fix; it
/// also decides the winner deterministically rather than by a race:
///
/// - the SWEEP goes first, because losing costs it a whole sweep (every prune,
///   both vacuums and the memory GC live inside it), while the ticker's loss
///   costs one 300 s tick that it retries on its own interval;
/// - the ticker still lands inside the sweep's first window often enough to be
///   deferred once, which is harmless — `reclaim_once` re-arms its own debt and
///   the next tick picks it up.
pub const RECLAIM_WARMUP_OFFSET: Duration = Duration::from_secs(150);

/// How long the fleet must be quiet before a reclaim tick may run (#522).
pub const MEMORY_RECLAIM_QUIET_FOR: Duration = Duration::from_secs(60);

/// Hard cap on how long a reclaim may be deferred by continuous activity
/// (#522). Past this the batch runs anyway, so a chat that is never quiet
/// cannot starve the reclaim — the arm that makes the gate safe to ship.
pub const MEMORY_RECLAIM_MAX_DELAY: Duration = Duration::from_secs(1800);

/// How soon the 24 h sweep retries after it lost the single-flight permit
/// (#838).
///
/// #840 removed the STRUCTURAL collision — the two loops no longer wake
/// together, because the ticker's warmup is [`SWEEP_WARMUP`] +
/// [`RECLAIM_WARMUP_OFFSET`] while the sweep's is [`SWEEP_WARMUP`]. This retry
/// is the safety net for the residual case: a tick that fires while a long
/// sweep (vacuums, memory GC) is still holding the permit.
///
/// Waiting the full interval after a skip would cost a whole day of
/// housekeeping — measured on `ops`, 32 µs apart, no sweep for that
/// incarnation — so a contended start retries on this short interval instead.
///
/// MUST stay strictly BELOW [`MEMORY_RECLAIM_TICK`]. A retry equal to the tick
/// is phase-locked to it: the sweep wakes exactly when the ticker does, loses
/// the same race, and retries into the same collision forever — measured on
/// `ops` at 300 s, losing at 05:11:42 and 05:16:42 (425 µs and 960 µs behind).
/// Retrying faster than the ticker guarantees attempts that land BETWEEN
/// ticks, where the permit is free. A test in
/// `src/tests/memory_maintenance_test.rs` pins the inequality.
pub const MAINTENANCE_CONTENDED_RETRY: Duration = Duration::from_secs(60);

/// How a sweep attempt ended, as far as the 24 h loop cares (#843).
///
/// The loop's only decision is how long to wait next, and these are the three
/// outcomes that change the answer. Without this split a failed run and a
/// completed run were indistinguishable — the error arm fell through to the
/// full interval and the retention knobs silently stopped working for a day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepOutcome {
    /// The sweep ran to completion.
    Ran,
    /// The permit was held, so nothing ran (#838).
    Skipped,
    /// The sweep ran (or tried to) and at least one leg failed (#843).
    Failed,
}

/// How many consecutive failures retry on the short interval before the loop
/// gives up and waits the full one (#843).
///
/// A persistently broken sweep — a locked DB, a full disk, a failing vacuum —
/// must not hot-loop against the very database it is failing on. After this
/// many consecutive short retries the loop returns to the configured interval
/// and tries again there, which still surfaces the failure on every attempt.
pub const MAX_CONSECUTIVE_FAILURE_RETRIES: u32 = 3;

/// Every timing value the maintenance scheduler depends on, in ONE place, with
/// the ordering between them enforced by construction (#844).
///
/// The values were previously five independent constants, and nothing checked
/// how they related. That gap is not hypothetical: #838 was exactly an ordering
/// violation — a contended retry equal to the reclaim tick made the sweep
/// phase-locked to the ticker, so every retry re-collided with the wake that
/// had just starved it and housekeeping was lost for a whole incarnation.
/// #840 was the same class one step earlier: equal warmups made both loops
/// reach the shared permit in the same microsecond.
///
/// So the constructor does not merely store the numbers, it refuses an
/// arrangement that cannot work:
///
/// - `contended_retry < reclaim_tick` — a retry at or above the tick is
///   phase-locked to it and re-collides forever (#838);
/// - `reclaim_tick < sweep_interval` — the ticker must tick several times per
///   sweep period, or "retry sooner" has nothing sooner to retry into;
/// - `reclaim_warmup_offset > 0` — the ticker must NOT share the sweep's first
///   wake instant (#840).
///
/// Panics rather than returns an error: these are compile-time-shaped facts
/// about constants, and a violation is a coding error that must never reach a
/// running daemon.
#[derive(Debug, Clone, Copy)]
pub struct MaintenanceTiming {
    contended_retry: Duration,
    reclaim_tick: Duration,
    sweep_interval: Duration,
    sweep_warmup: Duration,
    reclaim_warmup_offset: Duration,
}

impl MaintenanceTiming {
    /// Build a timing set, asserting the ordering that makes it workable.
    ///
    /// # Panics
    ///
    /// If `contended_retry >= reclaim_tick` (#838), if
    /// `reclaim_tick >= sweep_interval`, or if `reclaim_warmup_offset` is zero
    /// (#840).
    pub fn new(
        contended_retry: Duration,
        reclaim_tick: Duration,
        sweep_interval: Duration,
        sweep_warmup: Duration,
        reclaim_warmup_offset: Duration,
    ) -> Self {
        assert!(
            contended_retry < reclaim_tick,
            "#838: contended retry ({contended_retry:?}) must be strictly shorter than \
             the reclaim tick ({reclaim_tick:?}), or the sweep is phase-locked to the \
             ticker and re-collides with the wake that starved it"
        );
        assert!(
            reclaim_tick < sweep_interval,
            "the reclaim tick ({reclaim_tick:?}) must be shorter than the sweep \
             interval ({sweep_interval:?}), or a contended retry has no free window \
             to land in"
        );
        assert!(
            !reclaim_warmup_offset.is_zero(),
            "#840: a zero warmup offset restores the simultaneous first wake that let \
             the ticker take the shared permit every boot"
        );
        Self {
            contended_retry,
            reclaim_tick,
            sweep_interval,
            sweep_warmup,
            reclaim_warmup_offset,
        }
    }

    /// How soon a sweep retries after losing the permit or failing (#838, #843).
    pub fn contended_retry(&self) -> Duration {
        self.contended_retry
    }

    /// The memory reclaim ticker's period (#522).
    pub fn reclaim_tick(&self) -> Duration {
        self.reclaim_tick
    }

    /// The sweep's period once it has run (#844).
    pub fn sweep_interval(&self) -> Duration {
        self.sweep_interval
    }

    /// Post-boot wait before the sweep's first attempt (#273).
    pub fn sweep_warmup(&self) -> Duration {
        self.sweep_warmup
    }

    /// Post-boot wait before the reclaim ticker's first tick (#840).
    ///
    /// Always strictly longer than [`Self::sweep_warmup`], so the sweep reaches
    /// the permit first by construction rather than by luck.
    pub fn reclaim_warmup(&self) -> Duration {
        self.sweep_warmup + self.reclaim_warmup_offset
    }
}

impl Default for MaintenanceTiming {
    fn default() -> Self {
        Self::new(
            MAINTENANCE_CONTENDED_RETRY,
            MEMORY_RECLAIM_TICK,
            SWEEP_INTERVAL,
            SWEEP_WARMUP,
            RECLAIM_WARMUP_OFFSET,
        )
    }
}

/// Delay before the next 24 h sweep attempt (#838, #843).
///
/// A run that never happened (permit held by the reclaim ticker) must not cost
/// the full interval; neither must a run that failed. `consecutive_failures`
/// is the count ALREADY retried, so a broken sweep escalates back to the
/// configured interval instead of spinning.
///
/// Pure, so every branch is pinable without a clock or a process-global flag.
pub fn next_sweep_delay(
    outcome: SweepOutcome,
    consecutive_failures: u32,
    timing: &MaintenanceTiming,
) -> Duration {
    match outcome {
        SweepOutcome::Ran => timing.sweep_interval(),
        SweepOutcome::Skipped => timing.contended_retry(),
        SweepOutcome::Failed if consecutive_failures < MAX_CONSECUTIVE_FAILURE_RETRIES => {
            timing.contended_retry()
        }
        SweepOutcome::Failed => timing.sweep_interval(),
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
    /// How many legs of the sweep failed (#843).
    ///
    /// The legs are best-effort — one failing prune must not cost the others —
    /// but a sweep whose legs ALL failed is not the same event as a sweep that
    /// found nothing, and before this counter existed the loop could not tell
    /// them apart and slept a full day on a broken sweep.
    pub failed_legs: usize,
    /// Number of expired sub-agent sessions pruned.
    pub subagents_pruned: usize,
    /// Number of expired messages pruned (#278).
    pub messages_pruned: usize,
    /// Number of `feedback_ledger` rows pruned (#836).
    pub feedback_ledger_pruned: usize,
    /// Number of `usage_ledger` rows pruned (#1076).
    pub usage_ledger_pruned: usize,
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

impl MaintenanceReport {
    /// How this attempt ended, for the loop's retry decision (#843).
    ///
    /// A skip outranks a failure: a skipped run did no work at all, so nothing
    /// failed — `failed_legs` is necessarily zero on that path, and reading it
    /// first would be dead code.
    pub fn outcome(&self) -> SweepOutcome {
        if self.skipped {
            SweepOutcome::Skipped
        } else if self.failed_legs > 0 {
            SweepOutcome::Failed
        } else {
            SweepOutcome::Ran
        }
    }
}

/// Service managing periodic database & memory maintenance sweeps.
pub struct MaintenanceService {
    context: ServiceContext,
}

/// Run one retention leg and report what it removed, plus whether it failed.
///
/// The three prunes share one shape — a count on success, a warning on failure,
/// and a sweep that carries on either way — so they share this helper instead of
/// three copies of the same `match` (#841). A prune is best-effort by design: a
/// failing leg must not cost the others, and the failure is logged rather than
/// propagated because the sweep's caller has no repair to attempt.
///
/// The `bool` is what makes a failure VISIBLE to the loop (#843): a sweep whose
/// legs all failed must not look like a sweep that found nothing, or it sleeps
/// the full interval and the retention knobs stay broken for a day.
///
/// `what` names the leg in the warning, so the log still says WHICH prune failed
/// even though the code no longer repeats the message three times.
async fn prune_leg<F>(what: &str, leg: F) -> (usize, bool)
where
    F: std::future::Future<Output = Result<usize>>,
{
    match leg.await {
        Ok(pruned) => (pruned, true),
        Err(e) => {
            tracing::warn!("Maintenance: failed to prune {what}: {e:#}");
            (0, false)
        }
    }
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
    /// the cutoff comes from [`crate::services::prune::iso_day_cutoff`] — the
    /// one place that states which representation a column needs — and the
    /// DELETE's range walks `idx_feedback_ledger_created` instead of scanning
    /// the table.
    pub(crate) async fn prune_expired_feedback_ledger(&self, retention_days: u32) -> Result<usize> {
        if retention_days == 0 {
            return Ok(0);
        }
        let cutoff = crate::services::prune::iso_day_cutoff(retention_days);
        let repo = crate::db::repository::feedback_ledger::FeedbackLedgerRepository::new(
            self.context.pool(),
        );
        repo.prune_older_than(cutoff)
            .await
            .context("Failed to prune expired feedback ledger")
    }

    /// Prune `usage_ledger` rows older than `retention_days` (#1076).
    ///
    /// `retention_days == 0` disables pruning and returns `Ok(0)` — the
    /// historical behaviour, because this ledger is the cost source of truth.
    /// Retention here is opt-in and defaults OFF.
    ///
    /// `usage_ledger.created_at` is **INTEGER unix seconds**, so the cutoff is
    /// [`crate::services::prune::day_cutoff`] (an `i64`) — the ISO8601 text
    /// helper belongs to `feedback_ledger` and would match nothing here.
    pub(crate) async fn prune_expired_usage_ledger(&self, retention_days: u32) -> Result<usize> {
        if retention_days == 0 {
            return Ok(0);
        }
        let cutoff = crate::services::prune::day_cutoff(retention_days);
        let repo =
            crate::db::repository::usage_ledger::UsageLedgerRepository::new(self.context.pool());
        repo.prune_older_than(cutoff)
            .await
            .context("Failed to prune expired usage ledger")
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

        // 1. Prune expired sub-agent sessions (#842)
        //
        // The startup catch-up in `cli::ui` already pruned this table for this
        // boot. Taking the flag hands the prune back to the sweep from the NEXT
        // cycle on, instead of enumerating the same rows twice in one day.
        let session_svc = SessionService::new(self.context.clone());
        if take_subagent_catchup() {
            tracing::debug!(
                "Maintenance: subagent catch-up already ran this boot; sweep owns the prune next cycle"
            );
        } else {
            let ttl_days = config.agent.subagent_session_ttl_days();
            let (pruned, ok) = prune_leg(
                "subagent sessions",
                session_svc.prune_expired_subagent_sessions(ttl_days),
            )
            .await;
            report.subagents_pruned = pruned;
            report.failed_legs += usize::from(!ok);
        }

        // 1b. Prune expired messages (#278)
        let (pruned, ok) = prune_leg(
            "expired messages",
            session_svc.prune_expired_messages(config.agent.message_retention_days()),
        )
        .await;
        report.messages_pruned = pruned;
        report.failed_legs += usize::from(!ok);

        // 1c. Prune the feedback ledger (#836)
        //
        // `feedback_ledger` is append-only and had NO pruning path, so a busy
        // profile grew it past 1M rows / 3.75 GiB and every `feedback_analyze`,
        // RSI digest and `self_improve` call aggregated over the whole table —
        // `tool_%` is 90 % of the rows, so no index makes those scans selective.
        let (pruned, ok) = prune_leg(
            "feedback ledger",
            self.prune_expired_feedback_ledger(config.agent.feedback_ledger_retention_days()),
        )
        .await;
        report.feedback_ledger_pruned = pruned;
        report.failed_legs += usize::from(!ok);

        // 1d. Prune the usage ledger (#1076)
        //
        // The ledger is the cost source of truth, so its retention is opt-in
        // and defaults OFF (`0` = keep forever). When it is set, the DELETE is
        // a range walk over `idx_usage_ledger_created`, which
        // `heal_usage_ledger_indexes` supplies.
        let (pruned, ok) = prune_leg(
            "usage ledger",
            self.prune_expired_usage_ledger(config.agent.usage_ledger_retention_days()),
        )
        .await;
        report.usage_ledger_pruned = pruned;
        report.failed_legs += usize::from(!ok);

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
                Err(e) => {
                    tracing::warn!("Maintenance: memory vacuum failed: {e:#}");
                    report.failed_legs += 1;
                }
            }
        }

        // 3. Database Vacuum
        let db = crate::db::Database::new_with_pool(self.context.pool());
        match db.vacuum_database().await {
            Ok(vacuumed) => report.database_vacuumed = vacuumed,
            Err(e) => {
                tracing::warn!("Maintenance: database vacuum failed: {e:#}");
                report.failed_legs += 1;
            }
        }

        tracing::info!(
            subagents_pruned = report.subagents_pruned,
            messages_pruned = report.messages_pruned,
            feedback_ledger_pruned = report.feedback_ledger_pruned,
            usage_ledger_pruned = report.usage_ledger_pruned,
            database_vacuumed = report.database_vacuumed,
            memory_vacuumed = report.memory_vacuumed,
            memory_gc_skipped = report.memory_gc_skipped,
            failed_legs = report.failed_legs,
            "Maintenance sweep completed"
        );

        Ok(report)
    }

    /// Spawn periodic background maintenance task.
    pub fn spawn_periodic(context: ServiceContext, timing: MaintenanceTiming) {
        let class = crate::config::Config::current()
            .agent
            .maintenance
            .priority_class();
        spawn_housekeeping("maintenance-sweep", class, move || async move {
            // One line, at the start of the loop's own thread: a class that
            // silently resolved to `interactive` is invisible in every other
            // way, and this is the only place that can say what the sweep is
            // actually running at.
            tracing::info!(
                "maintenance priority: nice={} ioprio={}",
                class.nice(),
                class
            );
            let svc = MaintenanceService::new(context);
            // Initial post-boot warmup delay (30 mins) so startup and tool execution are not bottlenecked (#273)
            tokio::time::sleep(timing.sweep_warmup()).await;

            // Consecutive failed runs ALREADY retried on the short interval
            // (#843). Only a run that gets through resets it: a skip means no
            // work happened, so it must not buy a broken sweep more retries.
            let mut consecutive_failures: u32 = 0;

            loop {
                let outcome = match svc.run_maintenance().await {
                    Ok(report) => report.outcome(),
                    Err(e) => {
                        // #843: a run that errored out did NOT succeed, so it
                        // must not fall through to the full interval — that
                        // silently disabled every retention knob for a day.
                        tracing::warn!("Periodic maintenance run encountered error: {e:#}");
                        SweepOutcome::Failed
                    }
                };

                match outcome {
                    SweepOutcome::Ran => consecutive_failures = 0,
                    SweepOutcome::Skipped => {
                        // #838/#840: a permit held by the reclaim ticker means
                        // the sweep never ran, so nothing inside it happened.
                        // The warmups are offset now (#840) so this should be
                        // rare, but a tick that lands inside a long sweep still
                        // contends. Retry soon instead of losing a day.
                        tracing::info!(
                            retry_secs = timing.contended_retry().as_secs(),
                            "Maintenance sweep skipped (permit held) — retrying on the short interval"
                        );
                    }
                    SweepOutcome::Failed => {
                        consecutive_failures = consecutive_failures.saturating_add(1);
                        // Bounded (#843): after MAX_CONSECUTIVE_FAILURE_RETRIES
                        // the loop goes back to the configured interval, so a
                        // persistently broken sweep cannot hammer the database
                        // it is failing on.
                        tracing::warn!(
                            consecutive_failures = consecutive_failures,
                            retry_secs =
                                next_sweep_delay(outcome, consecutive_failures, &timing).as_secs(),
                            "Maintenance sweep failed — retrying"
                        );
                    }
                }

                tokio::time::sleep(next_sweep_delay(outcome, consecutive_failures, &timing)).await;
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
    pub fn spawn_memory_reclaim(timing: MaintenanceTiming) {
        if MEMORY_RECLAIM_SPAWNED.swap(true, Ordering::AcqRel) {
            return;
        }
        let class = crate::config::Config::current()
            .agent
            .maintenance
            .priority_class();
        spawn_housekeeping("maintenance-reclaim", class, move || async move {
            // Seed the clock, so a silent daemon does not have to wait for its
            // first message before the quiet window can ever elapse.
            crate::brain::agent::service::session_routes::note_activity();
            // Same post-boot warmup as the 24 h sweep, OFFSET past it (#840):
            // startup and the first tool calls must not compete with
            // housekeeping (#273), and the ticker must not collide with the
            // sweep's first wake — see `RECLAIM_WARMUP_OFFSET`.
            tokio::time::sleep(timing.reclaim_warmup()).await;

            loop {
                reclaim_once().await;
                tokio::time::sleep(timing.reclaim_tick()).await;
            }
        });
    }
}

/// Run an infinite housekeeping loop on an OS thread of its own, at `class`.
///
/// Two reasons this is a thread and not a `tokio::spawn`:
///
/// 1. **The class has to belong to the thread that does the work.** `nice` and
///    `ioprio` are per-thread on Linux, and a spawned task inherits whichever
///    worker thread it happens to land on. A task that lowered its own priority
///    would drag that shared worker down for whatever ran next on it, and a task
///    that never lowered it would silently run the sweep at the daemon's own
///    class — the exact failure the class exists to prevent.
/// 2. **The blocking legs must not occupy a shared worker.** The 50k-file prune,
///    the `VACUUM` and the WAL truncate each hold a thread for the length of a
///    syscall. On the shared runtime that is the same starvation the class is
///    meant to relieve, only moved to the worker pool.
///
/// A current-thread runtime is enough for both bodies: each is one `await`-
/// plus-`sleep` loop with nothing to schedule in parallel. It also keeps the
/// future off the `Send` bound, which is what lets the sweep keep taking its
/// `ServiceContext` by value.
///
/// The class is read from the LIVE config by the caller, never captured at boot:
/// the watcher swaps `Config::current()`'s `Arc`, so an operator's edit applies
/// to the next daemon start without a code change.
fn spawn_housekeeping<F, Fut>(name: &'static str, class: PriorityClass, body: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()>,
{
    let spawned = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            // Before `block_on`, so it covers every poll of the loop, and on
            // THIS thread, because both syscalls are per-thread.
            class.apply_to_current_thread();
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => {
                    // Housekeeping that cannot get a runtime is not a reason to
                    // take the daemon down; say so and let the thread end.
                    tracing::error!("{name}: could not build a housekeeping runtime: {e:#}");
                    return;
                }
            };
            runtime.block_on(body());
        });
    if let Err(e) = spawned {
        tracing::error!("{name}: could not spawn the housekeeping thread: {e:#}");
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
