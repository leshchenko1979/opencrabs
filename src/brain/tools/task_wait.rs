//! Task Wait Tool (#692)
//!
//! Block, up to a bound, on one detached run — returning the moment its output
//! matches a pattern or it reaches a terminal state, instead of making the agent
//! re-read a file and guess what it saw.
//!
//! This is NOT [`await_external`](super::await_external), and the difference is
//! the substrate rather than the verb: that tool records a DURABLE park on the
//! session's binding row so a daemon restart cannot leave a lane comatose, and
//! starts no timer at all. This one waits INSIDE the current turn and is useless
//! across a restart. A lane parked on a run for longer than a turn should declare
//! it with `await_external` and end its turn; `task_wait` is for a wait the lane
//! intends to sit through.

use super::error::Result;
use super::r#trait::{Tool, ToolCapability, ToolExecutionContext, ToolHints, ToolResult};
use crate::brain::agent::service::background_tasks::scan_stream_from;
use crate::brain::agent::service::work_status::WorkStatus;
use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use std::path::PathBuf;

/// Default wait when the caller names none.
const DEFAULT_WAIT_SECS: u64 = 60;

/// Hard ceiling on one wait.
///
/// Deliberately the same figure as the bash tool's own `timeout_secs` ceiling: a
/// wait is a turn spent doing nothing else, so it earns no larger budget than the
/// command it watches.
const MAX_WAIT_SECS: u64 = 600;

/// Poll interval.
const POLL_MS: u64 = 250;

/// How many matched lines the report carries before summarising.
const SHOW_MATCHES: usize = 20;

/// Which of a run's captures to watch.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Stream {
    #[default]
    Both,
    Out,
    Err,
}

impl Stream {
    /// Map a validated string onto the enum.
    ///
    /// Unknown values fall back to `Both` rather than panicking: `accepts` gates
    /// the tool's error path, and a panic inside a tool would trade a bad input
    /// for a lost turn.
    fn parse(v: Option<&str>) -> Self {
        match v {
            None | Some("both") => Self::Both,
            Some("out") | Some("stdout") => Self::Out,
            Some("err") | Some("stderr") => Self::Err,
            Some(_) => Self::Both,
        }
    }

    fn accepts(v: &str) -> bool {
        matches!(v, "both" | "out" | "stdout" | "err" | "stderr")
    }
}

/// Block on a detached run until it matches, finishes, or the wait expires.
#[derive(Default)]
pub struct TaskWaitTool;

impl TaskWaitTool {
    pub fn new() -> Self {
        Self
    }
}

/// A run's capture paths, resolved from whichever record still holds them.
struct RunPaths {
    out: Option<PathBuf>,
    err: Option<PathBuf>,
    label: String,
}

#[async_trait]
impl Tool for TaskWaitTool {
    fn name(&self) -> &str {
        "task_wait"
    }

    fn description(&self) -> &str {
        "Wait for a detached shell run to finish, or for its output to match a pattern, \
         up to a bound you set. Give 'pattern' (a regex) to return the moment a COMPLETE \
         output line matches — that is the point: instead of re-reading the capture and \
         guessing, you get woken on the line you named. Omit it to wait for the run to \
         reach a terminal state. 'timeout_secs' defaults to 60 and is capped at 600; 0 is \
         a single non-blocking status check. Returns which of the three happened — MATCHED, \
         FINISHED, or still running — with the matching lines and a tail. Waits inside THIS \
         turn: to park across a restart use await_external."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "run_id": {
                    "type": "string",
                    "description": "Run id, as shown by tasks_list or in the handover message."
                },
                "pattern": {
                    "type": "string",
                    "description": "Rust regex. The wait returns as soon as a complete line of the watched stream matches. Omit to wait only for the run to finish."
                },
                "stream": {
                    "type": "string",
                    "enum": ["both", "out", "err"],
                    "description": "Which capture to match against. Default 'both' (stdout then stderr, the historical merged shape)."
                },
                "timeout_secs": {
                    "type": "number",
                    "description": "How long to wait, in seconds. Default 60, capped at 600. 0 returns immediately with current state."
                }
            },
            "required": ["run_id"]
        })
    }

    fn capabilities(&self) -> Vec<ToolCapability> {
        vec![ToolCapability::ReadFiles]
    }

    fn hints(&self) -> ToolHints {
        ToolHints {
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
        }
    }

    async fn execute(&self, input: Value, context: &ToolExecutionContext) -> Result<ToolResult> {
        let Some(run_id) = input.get("run_id").and_then(Value::as_str) else {
            return Ok(ToolResult::error(
                "run_id is required (the id shown by tasks_list)".to_string(),
            ));
        };

        let timeout_secs = input
            .get("timeout_secs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_WAIT_SECS)
            .min(MAX_WAIT_SECS);

        // A bad regex is reported as a bad regex. Falling back to "no pattern"
        // here would make the tool silently wait for completion instead, and the
        // caller would read that as "the pattern never matched".
        let pattern = match input.get("pattern").and_then(Value::as_str) {
            None => None,
            Some(p) => match Regex::new(p) {
                Ok(re) => Some(re),
                Err(e) => {
                    return Ok(ToolResult::error(format!("Invalid regex in 'pattern': {e}")));
                }
            },
        };

        let stream_raw = input.get("stream").and_then(Value::as_str).unwrap_or("both");
        if !Stream::accepts(stream_raw) {
            return Ok(ToolResult::error(format!(
                "Unknown 'stream' {stream_raw:?}; expected one of both, out, err."
            )));
        }
        let stream = Stream::parse(Some(stream_raw));

        let Some(paths) = resolve(context, run_id) else {
            return Ok(ToolResult::error(format!(
                "No run {run_id} in this session, and no status record for it either. \
                 Detached runs are addressable only by the session that started them; \
                 tasks_list shows what is live."
            )));
        };

        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_secs(timeout_secs);
        let (mut out_off, mut err_off) = (0u64, 0u64);
        let mut last_tail = String::new();
        let mut skipped: u64 = 0;

        loop {
            // The scans run before the terminal check on purpose: a run that
            // exits with the caller's pattern on its last line owes a MATCHED
            // answer, because "the line I asked for appeared" is the stronger
            // fact. FINISHED is reported when no *new* line matched.
            let status = WorkStatus::read(run_id);
            let terminal = status.as_ref().is_some_and(|s| s.state.is_terminal());

            if matches!(stream, Stream::Both | Stream::Out)
                && let Some(p) = &paths.out
            {
                let scan = scan_stream_from(p, out_off, pattern.as_ref()).await;
                out_off = scan.next_offset;
                skipped += scan.skipped_bytes;
                if !scan.tail.is_empty() {
                    last_tail = scan.tail;
                }
                if !scan.matches.is_empty() {
                    return Ok(report(
                        Outcome::Matched(scan.matches),
                        &paths,
                        run_id,
                        status.as_ref(),
                        &last_tail,
                        skipped,
                    ));
                }
            }
            if matches!(stream, Stream::Both | Stream::Err)
                && let Some(p) = &paths.err
            {
                let scan = scan_stream_from(p, err_off, pattern.as_ref()).await;
                err_off = scan.next_offset;
                skipped += scan.skipped_bytes;
                if !scan.tail.is_empty() {
                    last_tail = scan.tail;
                }
                if !scan.matches.is_empty() {
                    return Ok(report(
                        Outcome::Matched(scan.matches),
                        &paths,
                        run_id,
                        status.as_ref(),
                        &last_tail,
                        skipped,
                    ));
                }
            }

            if terminal {
                return Ok(report(
                    Outcome::Finished,
                    &paths,
                    run_id,
                    status.as_ref(),
                    &last_tail,
                    skipped,
                ));
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(report(
                    Outcome::TimedOut,
                    &paths,
                    run_id,
                    status.as_ref(),
                    &last_tail,
                    skipped,
                ));
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(POLL_MS)).await;
        }
    }
}

/// Why a wait returned.
#[derive(Clone, Debug)]
enum Outcome {
    /// Lines matched, in the order they appeared.
    Matched(Vec<String>),
    /// The run recorded a terminal state.
    Finished,
    /// The bound expired with the run still live.
    TimedOut,
}

use Outcome::Finished;
use Outcome::Matched;
use Outcome::TimedOut;

/// Render the three outcomes in one shape, so a caller can branch on the header.
///
/// `tail` is the last bytes the scan saw, which is what a caller decides from
/// when nothing matched — without it a `STILL RUNNING` answer reports nothing.
fn report(
    outcome: Outcome,
    paths: &RunPaths,
    run_id: &str,
    status: Option<&WorkStatus>,
    tail: &str,
    skipped: u64,
) -> ToolResult {
    let state_word = status
        .map(|s| format!("{:?}", s.state).to_lowercase())
        .unwrap_or_else(|| "unknown".to_string());
    let code = status
        .and_then(|s| s.finish.as_ref())
        .and_then(|f| f.code)
        .map(|c| c.to_string())
        .unwrap_or_else(|| "none recorded".to_string());

    let mut out = match &outcome {
        Matched(lines) => format!(
            "MATCHED run {run_id} ({label}) — {n} line(s) matched, showing {shown}.\n",
            label = paths.label,
            n = lines.len(),
            shown = lines.len().min(SHOW_MATCHES)
        ),
        Finished => format!(
            "FINISHED run {run_id} ({}) — state {state_word}, recorded exit code {code}.\n",
            paths.label
        ),
        TimedOut => format!(
            "STILL RUNNING run {run_id} ({}) after the wait expired — state {state_word}. \
             It is live and addressable: wait again, or read its capture.\n",
            paths.label
        ),
    };

    if let Matched(lines) = &outcome {
        out.push_str("Matched lines:\n");
        for line in lines.iter().take(SHOW_MATCHES) {
            out.push_str(&format!("  {line}\n"));
        }
        if lines.len() > SHOW_MATCHES {
            out.push_str(&format!("  ... {} more\n", lines.len() - SHOW_MATCHES));
        }
    } else if !tail.trim().is_empty() {
        // No match to show, so show where the run got to.
        out.push_str(&format!("Tail:\n{tail}\n"));
    }

    if skipped > 0 {
        out.push_str(&format!(
            "NOTE: {skipped} byte(s) of output were passed over unscanned between polls \
             (the run outpaced the scan cap), so a match inside them was not seen.\n"
        ));
    }

    out.push_str(&format_paths(paths));
    ToolResult::success(out)
}

fn format_paths(paths: &RunPaths) -> String {
    let mut s = String::new();
    if let Some(p) = &paths.out {
        s.push_str(&format!("stdout capture: {}\n", p.display()));
    }
    if let Some(p) = &paths.err {
        s.push_str(&format!("stderr capture: {}\n", p.display()));
    }
    s
}

/// Resolve a run's capture paths from the live registry, else its status file.
///
/// Both, in that order, because a run that finished *during* the wait has left
/// the registry but keeps its record on disk — and that is the common case for a
/// caller waiting on a short command, not a corner.
fn resolve(context: &ToolExecutionContext, run_id: &str) -> Option<RunPaths> {
    if let Some(mgr) = context.background_manager.as_ref()
        && let Some(h) = mgr
            .handles_for(context.session_id)
            .into_iter()
            .find(|h| h.id == run_id)
    {
        return Some(RunPaths {
            out: Some(h.output_out.clone()),
            err: Some(h.output_err.clone()),
            label: h.label.clone(),
        });
    }
    let status = WorkStatus::read(run_id)?;
    Some(RunPaths {
        out: status.output_out.filter(|p| !p.is_empty()).map(PathBuf::from),
        err: status.output_err.filter(|p| !p.is_empty()).map(PathBuf::from),
        label: status.label,
    })
}
