//! Silence watchdog for child runs (#1056).
//!
//! A child that FINISHES always wakes its parent: `deliver_to_session(.., true)`
//! starts a new turn even hours later, even across a restart. The hole is the
//! child that NEVER finishes — no completion is ever generated, nothing is
//! delivered, and the parent's turn is long gone. The parent then hand-rolls a
//! `wait_agent` / `agent_progress` polling loop: the same bug with extra steps
//! and a burned context.
//!
//! Nothing here terminates anything. The watchdog's only verb is **tell**:
//! `stop_agent` stays a decision for the parent, and the notice carries the
//! state plus the verb that state needs.
//!
//! Two facts shape the whole design:
//!
//! 1. **A child cannot report its own silence.** `report_progress` wakes a
//!    parent only for `Blocker | Question`; a wedged child emits nothing at all.
//!    So the observer has to be a daemon tick delivering on its own behalf.
//! 2. **There is no nudge primitive for a `Running` child.** `get_input_tx`'s
//!    channel is drained at exactly one place — the `AwaitingInput` park — so a
//!    message written while the child is `Running` sits unread. That is why the
//!    ladder has no nudge rung at all.
//!
//! The population comes from MEMORY, not from status files: a spawn writes no
//! `WorkStatus` record, so a file-keyed sweep would report a clean population
//! while a live child hung silently. Files stay the BOOT source — they answer
//! "what died while the process was down", which memory cannot. Memory answers
//! "what is quiet while it is up".

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::brain::tools::subagent::SubAgentManager;
use crate::brain::tools::subagent::manager::SubAgentState;

use super::session_routes::{Delivery, deliver_to_session};

/// How often the watchdog looks. An implementation detail, deliberately NOT a
/// config key: the three policy knobs are `child_silence_secs`,
/// `child_repeat_secs` and `run_notice_secs`, and `child_silence_secs = 0` is
/// the off switch. 60 s keeps the first notice within a minute of the
/// threshold, and the REPEAT cadence — not this — is what sets how often a
/// parent is told.
const TICK: Duration = Duration::from_secs(60);

/// Per-child bookkeeping. Lives here, not on `WorkStatus` or `SubAgent`:
/// neither type carries a "last output change" instant, so this map is the
/// whole of the watchdog's state.
#[derive(Debug, Default, Clone)]
struct WatchState {
    /// Fingerprint of the child's last observed output, and when it last
    /// CHANGED. Seeded from the child's own spawn instant, so a child that
    /// never completes a round at all — wedged inside its FIRST provider call,
    /// the commonest hang there is — is still measured from birth rather than
    /// from the watchdog's first look.
    fingerprint: u64,
    fingerprint_changed_at: DateTime<Utc>,
    /// When the parent was last told about this child, so the repeat cadence is
    /// measured from the last notice rather than from the first.
    last_notice_at: Option<DateTime<Utc>>,
    /// Set once for an orphan (a child whose parent session no longer exists).
    /// It is warned about ONCE and then never noticed again: there is nobody to
    /// tell, and repeating the warning would be noise about a condition the
    /// operator already read.
    orphan_warned: bool,
}

static STATE: Mutex<Option<HashMap<String, WatchState>>> = Mutex::new(None);

fn with_state<T>(f: impl FnOnce(&mut HashMap<String, WatchState>) -> T) -> T {
    // Recover from poisoning rather than `expect`: a panic anywhere that held
    // this lock would otherwise fail every subsequent test in the binary, and
    // the map itself is still structurally valid data. Same posture as
    // `Config::current`.
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(HashMap::new))
}

/// Fingerprint a child's output. `None` (no output yet) hashes as the empty
/// string rather than being skipped: "still nothing" IS a stable observation,
/// and the spawn-instant seed is what gives it an age.
fn fingerprint_of(output: Option<&str>) -> u64 {
    let mut hasher = DefaultHasher::new();
    output.unwrap_or("").hash(&mut hasher);
    hasher.finish()
}

/// Why a child is not selectable right now.
#[derive(Debug, PartialEq, Eq)]
enum Skip {
    /// It ended its turn. Terminal states are the completion path's business.
    Terminal,
    /// Parked at a round boundary having already pushed attention (#859). The
    /// parent is already at fault for not answering; re-waking the child would
    /// be noise on top of a message already sent.
    AwaitingInput,
    /// Somebody is blocked in `wait_agent` on it, so it is going to answer.
    Awaited,
}

fn skip_reason(state: &SubAgentState, waiters: Option<usize>) -> Option<Skip> {
    match state {
        SubAgentState::Running => {
            if waiters.unwrap_or(0) > 0 {
                Some(Skip::Awaited)
            } else {
                None
            }
        }
        SubAgentState::AwaitingInput => Some(Skip::AwaitingInput),
        // `Interrupted` belongs with the terminal states here (#1080): nothing
        // is executing that child, so there is no silence to chase. It is NOT
        // terminal for `resume_agent`, which is a different question and is
        // answered by `next_verb` below.
        SubAgentState::Completed
        | SubAgentState::Failed(_)
        | SubAgentState::Cancelled
        | SubAgentState::Interrupted => Some(Skip::Terminal),
    }
}

/// The next move for a child, given its state. This table is what the notice
/// carries and what the four tool descriptions repeat, because a parent that
/// gets it wrong sends itself at a verb that refuses — and a refusal reads as
/// "the child is gone".
fn next_verb(state: &SubAgentState) -> &'static str {
    match state {
        SubAgentState::Running | SubAgentState::AwaitingInput => "send_input",
        // #1080: an interrupted child is CONTINUED, not re-spawned. Its
        // session and transcript survived the restart, so `resume_agent` picks
        // up where it stopped — the same verb a completed child takes.
        SubAgentState::Completed
        | SubAgentState::Failed(_)
        | SubAgentState::Cancelled
        | SubAgentState::Interrupted => "resume_agent",
    }
}

/// One pass over the roster.
///
/// `now` is injectable so the ladder's timing can be tested without sleeping.
/// Every error inside is logged and swallowed: one unreadable child must never
/// stop the pass for the others.
pub(crate) fn pass(manager: &SubAgentManager, now: DateTime<Utc>, silence: u64, repeat: u64) {
    if silence == 0 {
        return;
    }

    let roster = manager.resolved_all();
    let mut seen: Vec<String> = Vec::with_capacity(roster.len());

    // Iterate by REFERENCE: the roster is borrowed again below to name this
    // child's live siblings, and moving it here would end that borrow.
    for agent in &roster {
        seen.push(agent.id.clone());

        if let Some(skip) = skip_reason(&agent.state, manager.waiter_count(&agent.id)) {
            // A terminal or awaited child leaves the map entirely, so a later
            // respawn of the same id starts clean.
            if matches!(skip, Skip::Terminal) {
                with_state(|s| s.remove(&agent.id));
            }
            continue;
        }

        let spawned_at = DateTime::parse_from_rfc3339(&agent.spawned_at)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or(now);

        let fp = fingerprint_of(manager.get_output(&agent.id).as_deref());

        let (quiet_since, last_notice) = with_state(|s| {
            let entry = s.entry(agent.id.clone()).or_insert_with(|| WatchState {
                fingerprint: fp,
                // Seed from the child's OWN birth, not from this observation.
                fingerprint_changed_at: spawned_at,
                last_notice_at: None,
                orphan_warned: false,
            });
            if entry.fingerprint != fp {
                // It produced something: the child is working again, and the
                // ladder restarts from here rather than resuming a stale count.
                entry.fingerprint = fp;
                entry.fingerprint_changed_at = now;
            }
            (entry.fingerprint_changed_at, entry.last_notice_at)
        });

        let quiet_for = (now - quiet_since).num_seconds().max(0) as u64;
        if quiet_for < silence {
            continue;
        }

        // Repeat gate: `repeat == 0` means one notice, never a second.
        if let Some(last) = last_notice {
            if repeat == 0 {
                continue;
            }
            if ((now - last).num_seconds().max(0) as u64) < repeat {
                continue;
            }
        }

        let quiet = quiet_label(quiet_for);
        let msg = silence_message(agent, &quiet, &roster, now);

        match deliver_to_session(agent.parent_session_id, msg, true) {
            Delivery::Delivered => {
                with_state(|s| {
                    if let Some(e) = s.get_mut(&agent.id) {
                        e.last_notice_at = Some(now);
                    }
                });
                tracing::info!(
                    "child watchdog: told session {} about quiet child {} ({})",
                    agent.parent_session_id,
                    agent.id,
                    quiet
                );
            }
            Delivery::Parked => {
                // Durable: the parking route persisted a notify_queue row, so
                // this leaves when the channel claims the session. Record the
                // notice anyway — otherwise the next tick re-parks a duplicate.
                with_state(|s| {
                    if let Some(e) = s.get_mut(&agent.id) {
                        e.last_notice_at = Some(now);
                    }
                });
                tracing::info!(
                    "child watchdog: notice for {} parked for session {}",
                    agent.id,
                    agent.parent_session_id
                );
            }
            Delivery::NoRoute => {
                // Nobody to tell. The child keeps running (nothing here kills),
                // and repeating the attempt every tick would be noise about a
                // condition that cannot change.
                let first = with_state(|s| {
                    let e = s.entry(agent.id.clone()).or_default();
                    let first = !e.orphan_warned;
                    e.orphan_warned = true;
                    first
                });
                if first {
                    tracing::warn!(
                        "child watchdog: child {} ({}) has no route to its parent session {} \
                         after {quiet} of silence; it keeps running and will not be \
                         noticed again",
                        agent.id,
                        agent.label,
                        agent.parent_session_id
                    );
                }
            }
            other => {
                tracing::debug!(
                    "child watchdog: notice for {} not delivered ({other:?})",
                    agent.id
                );
            }
        }
    }

    // Prune anything that left the roster, so the map cannot grow without
    // bound across a long-lived daemon.
    with_state(|s| s.retain(|id, _| seen.iter().any(|s| s == id)));
}

/// Human-scale quiet duration, in the same units the progress renderer uses.
fn quiet_label(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// The message a parent receives when one of its children goes quiet.
///
/// A THIRD message class beside #859's `blocker` and `question` — and it speaks
/// in the same voice, because all three are the daemon telling a parent that a
/// child needs it. What it must NOT do is describe a child that has ended its
/// turn: this one is still `Running`, so `send_input` is the verb, not
/// `resume_agent`.
fn silence_message(
    agent: &crate::brain::tools::subagent::manager::ResolvedAgent,
    quiet_label: &str,
    roster: &[crate::brain::tools::subagent::manager::ResolvedAgent],
    now: DateTime<Utc>,
) -> crate::brain::agent::service::types::QueuedUserMessage {
    let verb = next_verb(&agent.state);
    let state = state_word(&agent.state);
    let alive = alive_for(&agent.spawned_at, now);
    let doing = crate::brain::tools::subagent::progress::snapshot_line(&agent.id)
        .unwrap_or_else(|| "no progress recorded yet".to_string());

    let mut body = format!(
        "Sub-agent {id} [{label}] has produced no new output for {quiet_label} — it is \
         {state} after {alive} alive, not finished. Nothing was stopped.\n\
         \n\
         What it is doing: {doing}\n",
        id = agent.id,
        label = agent.label,
    );

    // The brief, when there is one — a parent woken out of a long turn has no
    // memory of what this child was asked, and the cheapest wrong move is to
    // spawn a replacement for work that is still running.
    if !agent.task.trim().is_empty() {
        body.push_str(&format!("Asked to: {}\n", task_preview(&agent.task)));
    }

    // The roster, for the same reason one step further out. A parent woken out
    // of a STOPPED turn does not know what else it has running, so "child X is
    // quiet" reads as "X is the only work I have and it is stuck" — and the
    // cheapest response is to spawn a replacement, which runs the work twice.
    // Naming the siblings makes that duplicate visible before it is created.
    let mut live: Vec<String> = roster
        .iter()
        .filter(|a| a.id != agent.id && a.parent_session_id == agent.parent_session_id)
        .filter(|a| {
            !matches!(
                a.state,
                SubAgentState::Completed
                    | SubAgentState::Failed(_)
                    | SubAgentState::Cancelled
                    | SubAgentState::Interrupted
            )
        })
        .map(|a| format!("{} [{}] {}", a.id, a.label, state_word(&a.state)))
        .collect();
    live.sort();
    if live.is_empty() {
        body.push_str("No other children of this session are live.\n");
    } else {
        body.push_str(&format!(
            "Your other live children — do NOT spawn replacements for these: {}\n",
            live.join("; ")
        ));
    }

    let steer = if verb == "send_input" {
        "it is still mid-run, so you can steer it"
    } else {
        "its turn has ended, so you continue it rather than steering it"
    };
    body.push_str(&format!(
        "\nWhat to do: `{verb}` reaches it — {steer}.\n\
         Do not poll it: this notice repeats while it stays quiet, and a completion \
         wakes you on its own.\n"
    ));

    crate::brain::agent::service::types::QueuedUserMessage::system(
        format!("[System: child watchdog] {body}"),
        format!("🔍 {} [{}] quiet for {quiet_label}", agent.id, agent.label),
    )
}

/// One word for a child's state, matching the progress renderer's vocabulary.
fn state_word(state: &SubAgentState) -> &'static str {
    state.label()
}

/// How long the child has been alive, from its own spawn instant.
///
/// `now` is the pass's own clock rather than `Utc::now()`: a notice that mixes
/// two instants can state an elapsed time that contradicts its own quiet
/// duration, and the injected clock is what makes the text testable.
fn alive_for(spawned_at: &str, now: DateTime<Utc>) -> String {
    let Ok(start) = DateTime::parse_from_rfc3339(spawned_at) else {
        return "?".to_string();
    };
    let secs = (now - start.with_timezone(&Utc)).num_seconds().max(0) as u64;
    quiet_label(secs)
}

/// Bounded one-line preview of the child's brief (#960's rule, restated here so
/// the notice does not re-ingest a prompt the parent already wrote).
fn task_preview(task: &str) -> String {
    const LIMIT: usize = 200;
    let flat = task.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= LIMIT {
        return flat;
    }
    let cut: String = flat.chars().take(LIMIT).collect();
    format!("{cut}…")
}

/// Spawn the watchdog loop for `manager` (#1056).
///
/// Called once, from the single place a `SubAgentManager` is built, so the tick
/// watches exactly the children its own process holds. A multi-profile daemon
/// builds one manager per profile, and `deliver_to_session` routes by session
/// id process-wide, so the notice still lands in the right session.
///
/// The loop is SEQUENTIAL: each pass is awaited before the next sleep, so it
/// cannot overlap itself and no flight guard is needed. The `await` on the
/// `JoinHandle` also turns a panic inside `pass` into a logged `JoinError`
/// instead of a silently dead loop.
pub(crate) fn spawn(manager: std::sync::Arc<SubAgentManager>) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        // No runtime (a test, or a sync entry point): start nothing.
        return;
    };

    handle.spawn(async move {
        loop {
            tokio::time::sleep(TICK).await;

            // Read per tick, never snapshot at spawn: an edit then takes effect
            // within one period instead of at the next restart, and a spawned
            // task reads the same process-global config every other consumer
            // does.
            let cfg = crate::config::Config::current();
            let silence = cfg.agent.child_silence_secs;
            let repeat = cfg.agent.child_repeat_secs;

            let mgr = manager.clone();
            let now = Utc::now();
            if let Err(e) = tokio::spawn(async move {
                pass(&mgr, now, silence, repeat);
            })
            .await
            {
                tracing::error!("child watchdog pass panicked: {e}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    // Imported HERE, not at module scope: the watchdog body builds no agents
    // and mints no uuids, so a module-level import would be dead code in a
    // `-D warnings` build.
    use crate::brain::tools::subagent::SubAgent;
    use uuid::Uuid;

    /// Serializes the tests that touch the process-global `STATE`.
    ///
    /// `pass()` prunes every id absent from the roster it is handed, so a test
    /// that runs it against an EMPTY roster would clear a parallel test's
    /// fixture between that test's insert and its assertion. Only the four
    /// state-touching tests take this; the pure helpers need no lock.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_tests() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn fingerprint_is_stable_for_none_and_moves_with_output() {
        assert_eq!(fingerprint_of(None), fingerprint_of(None));
        assert_eq!(fingerprint_of(Some("")), fingerprint_of(None));
        assert_ne!(fingerprint_of(Some("a")), fingerprint_of(None));
    }

    #[test]
    fn skip_reason_covers_the_four_cases() {
        // Running with no waiter: selectable.
        assert_eq!(skip_reason(&SubAgentState::Running, Some(0)), None);
        assert_eq!(skip_reason(&SubAgentState::Running, None), None);
        // Running with a waiter: someone is already going to be answered.
        assert_eq!(
            skip_reason(&SubAgentState::Running, Some(1)),
            Some(Skip::Awaited)
        );
        // Parked: it already pushed attention (#859).
        assert_eq!(
            skip_reason(&SubAgentState::AwaitingInput, Some(0)),
            Some(Skip::AwaitingInput)
        );
        // Terminal: the completion path owns it.
        assert_eq!(
            skip_reason(&SubAgentState::Completed, None),
            Some(Skip::Terminal)
        );
        // #1080: an interrupted child has no silence to chase — nothing is
        // executing it. Waking a parent about it would be the same noise the
        // Completed arm avoids.
        assert_eq!(
            skip_reason(&SubAgentState::Interrupted, None),
            Some(Skip::Terminal)
        );
    }

    #[test]
    fn next_verb_matches_the_tool_gates() {
        assert_eq!(next_verb(&SubAgentState::Running), "send_input");
        assert_eq!(next_verb(&SubAgentState::AwaitingInput), "send_input");
        assert_eq!(next_verb(&SubAgentState::Completed), "resume_agent");
        assert_eq!(
            next_verb(&SubAgentState::Failed("boom".into())),
            "resume_agent"
        );
        assert_eq!(next_verb(&SubAgentState::Cancelled), "resume_agent");
        // #1080: the verb for an interrupted child is the one that CONTINUES
        // it. The notice carrying `send_input` here would send the parent at a
        // tool that refuses, and the refusal reads as "the child is gone" —
        // the exact conclusion that made parents re-spawn intact work.
        assert_eq!(next_verb(&SubAgentState::Interrupted), "resume_agent");
    }

    #[test]
    fn quiet_label_is_human_scale() {
        assert_eq!(quiet_label(45), "45s");
        assert_eq!(quiet_label(600), "10m00s");
        assert_eq!(quiet_label(3900), "1h05m");
    }

    #[test]
    fn task_preview_is_bounded_and_flattened() {
        let long = "a".repeat(500);
        let p = task_preview(&long);
        assert_eq!(p.chars().count(), 201); // 200 + the ellipsis
        assert_eq!(task_preview("one\ntwo"), "one two");
    }

    #[test]
    fn silence_disabled_does_nothing() {
        let _guard = lock_tests();
        // silence == 0 is the off switch: the pass returns before it reads the
        // roster, so state left by a previous run is untouched.
        with_state(|s| {
            s.insert(
                "kept".to_string(),
                WatchState {
                    fingerprint: 0,
                    fingerprint_changed_at: Utc::now(),
                    last_notice_at: None,
                    orphan_warned: false,
                },
            );
        });
        let m = SubAgentManager::new();
        pass(&m, Utc::now(), 0, 600);
        // Assert on THIS id, not on the map's length: the map is process-global
        // and `cargo test` runs these in parallel, so a length assertion would
        // race a sibling test's insert and fail intermittently.
        let kept = with_state(|s| s.contains_key("kept"));
        assert!(kept, "a disabled watchdog must not prune");
        with_state(|s| s.remove("kept"));
    }

    /// The selection rule, end to end: a child born moments ago is NOT
    /// noticed, and no delivery is attempted for it. This is the half of the
    /// ladder that must never fire early — a watchdog that reports a child two
    /// seconds into its first provider call is worse than no watchdog, because
    /// it teaches the parent to ignore the notice.
    #[test]
    fn a_fresh_child_is_not_noticed() {
        let _guard = lock_tests();
        let m = SubAgentManager::new();
        let parent = Uuid::new_v4();
        m.insert(SubAgent::new(
            "fresh",
            "just started",
            Uuid::new_v4(),
            parent,
        ));

        pass(&m, Utc::now(), 600, 600);

        let noticed = with_state(|s| {
            s.get("fresh")
                .map(|e| e.last_notice_at.is_some())
                .unwrap_or(false)
        });
        assert!(
            !noticed,
            "a child inside its first silence window is not reported"
        );
        with_state(|s| s.clear());
    }

    /// The clock is seeded from the child's OWN birth, not from the watchdog's
    /// first look at it. A child that was already old when this process first
    /// saw it — a rehydrated entry, or a tick that starts late — must be
    /// measured from `spawned_at`, or every restart buys a hung child another
    /// full silence window.
    #[test]
    fn the_clock_seeds_from_the_childs_own_birth() {
        let _guard = lock_tests();
        let m = SubAgentManager::new();
        let parent = Uuid::new_v4();
        let mut old = SubAgent::new("old", "wedged long ago", Uuid::new_v4(), parent);
        old.spawned_at = Utc::now() - chrono::Duration::seconds(3600);
        m.insert(old);

        let now = Utc::now();
        // `silence` is set ABOVE the child's age on purpose: this test asserts
        // the SEED, and a threshold it does not cross keeps the pass from
        // attempting a delivery it has no route for.
        pass(&m, now, 7200, 600);

        let quiet_at = with_state(|s| s.get("old").map(|e| e.fingerprint_changed_at));
        let quiet_at = quiet_at.expect("the child is tracked");
        let age = (now - quiet_at).num_seconds();
        assert!(
            age >= 3600,
            "the seed is the child's birth, not this observation: age {age}s"
        );
        with_state(|s| s.clear());
    }

    #[test]
    fn state_prunes_when_the_child_leaves_the_roster() {
        let _guard = lock_tests();
        // An empty roster must clear the map rather than leaving stale ids.
        with_state(|s| {
            s.insert(
                "gone".to_string(),
                WatchState {
                    fingerprint: 0,
                    fingerprint_changed_at: Utc::now(),
                    last_notice_at: None,
                    orphan_warned: false,
                },
            );
        });
        let m = SubAgentManager::new();
        pass(&m, Utc::now(), 600, 600);
        // Assert on THIS id rather than the map's length: the map is
        // process-global and `cargo test` runs these in parallel, so a length
        // assertion would race a sibling test's insert.
        let pruned = !with_state(|s| s.contains_key("gone"));
        assert!(pruned, "a child that left the roster must be pruned");
    }
}
