//! Task Output Tool (#692)
//!
//! Read one detached run's captured output — live while the run is producing,
//! complete once it is over.
//!
//! There is no separate "live" path and no polling loop: the stream reader
//! writes bytes to the capture file as they arrive, so reading the file *is*
//! reading the run. That is the whole reason the pty leg exists — over a pipe
//! the file stays empty until the child exits, and this tool would silently
//! return nothing for the entire run.

use super::error::Result;
use super::r#trait::{Tool, ToolCapability, ToolExecutionContext, ToolHints, ToolResult};
use crate::brain::agent::service::work_status::{WorkState, WorkStatus};
use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

/// Default tail returned per stream.
const DEFAULT_TAIL_BYTES: u64 = 16 * 1024;
/// Ceiling for one call's `tail_bytes`.
const MAX_TAIL_BYTES: u64 = 256 * 1024;

/// Which captures to return.
#[derive(PartialEq)]
enum Stream {
    Out,
    Err,
    Both,
}

impl Stream {
    fn parse(raw: Option<&str>) -> Option<Self> {
        match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            None | Some("") | Some("both") => Some(Self::Both),
            Some("stdout") | Some("out") => Some(Self::Out),
            Some("stderr") | Some("err") => Some(Self::Err),
            Some(_) => None,
        }
    }

    fn wants_out(&self) -> bool {
        matches!(self, Self::Out | Self::Both)
    }

    fn wants_err(&self) -> bool {
        matches!(self, Self::Err | Self::Both)
    }
}

/// One stream's tail, with what was left behind.
struct Tail {
    text: String,
    /// Total bytes currently in the capture file — not the bytes returned.
    size: u64,
    /// How much of the file was skipped to honour the tail budget.
    skipped: u64,
    /// Progress frames collapsed, if any.
    collapsed: usize,
    /// The first line was partial (the tail window started mid-line).
    partial_first_line: bool,
}

/// Read the last `tail_bytes` of a capture, without holding the whole file.
///
/// Seeking to `size - tail_bytes` rather than reading and slicing is what makes
/// this safe on a run whose capture is at its cap (8 MiB): the cost of a status
/// check does not scale with what the run has already produced.
async fn read_tail(path: &Path, tail_bytes: u64) -> std::io::Result<Tail> {
    let mut f = tokio::fs::File::open(path).await?;
    let size = f.metadata().await?.len();
    let start = size.saturating_sub(tail_bytes);
    let skipped = start;
    if start > 0 {
        f.seek(std::io::SeekFrom::Start(start)).await?;
    }
    let mut raw = Vec::new();
    f.read_to_end(&mut raw).await?;

    // A window that starts mid-line begins with that line's tail, which is not
    // a line the run produced. Dropped, and said so, rather than printed as
    // though it were output.
    let mut partial_first_line = false;
    let mut text = String::from_utf8_lossy(&raw).into_owned();
    if start > 0
        && let Some(pos) = text.find('\n')
    {
        text.drain(..=pos);
        partial_first_line = true;
    }

    let (text, collapsed) = collapse_frames(&text);
    Ok(Tail {
        text,
        size,
        skipped,
        collapsed,
        partial_first_line,
    })
}

/// Collapse carriage-return progress frames, keeping the last one per line.
///
/// A progress bar emits one frame per update, each rewriting the same line with
/// `\r`. Returned raw that is thousands of near-identical lines — the exact
/// context flood the stream was meant to relieve — so only the final state of
/// each line survives, and the count of what was dropped is reported: a
/// collapsed view that did not say it collapsed would read as the run's output.
fn collapse_frames(raw: &str) -> (String, usize) {
    let mut collapsed = 0usize;
    let mut out = String::with_capacity(raw.len());
    for (i, line) in raw.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if !line.contains('\r') {
            out.push_str(line);
            continue;
        }
        let frames: Vec<&str> = line.split('\r').collect();
        let tail = frames.iter().rfind(|f| !f.trim().is_empty()).copied().unwrap_or("");
        collapsed += frames.len().saturating_sub(1);
        out.push_str(tail);
    }
    (out, collapsed)
}

/// Tool reading one detached run's captured output.
#[derive(Default)]
pub struct TaskOutputTool;

impl TaskOutputTool {
    pub fn new() -> Self {
        Self
    }
}

fn state_label(state: &WorkState) -> &'static str {
    match state {
        WorkState::Pending => "pending",
        WorkState::Running => "running",
        WorkState::AwaitingInput => "awaiting input",
        WorkState::Completed => "completed",
        WorkState::Failed => "failed",
        // Not an error: the owning process died before the run reached a
        // terminal state, so how far it got is genuinely unknown. Reporting it
        // as "failed" would assert the command went wrong; reporting it as
        // "completed" would assert it went right. Neither is known.
        WorkState::Interrupted => {
            "interrupted (the process died mid-run; how far it got is unknown)"
        }
    }
}

/// Render one stream's block. Pure, so tests pin the shape without a live run.
fn render_stream(
    name: &str,
    tail: &Tail,
    path: &Path,
    lines: Option<usize>,
    out: &mut String,
) {
    out.push_str(&format!(
        "\n\n{name} ({} bytes at {}):",
        tail.size,
        path.display()
    ));
    if tail.size == 0 {
        out.push_str("\n  (empty so far)");
        return;
    }
    let mut body = tail.text.clone();
    if let Some(n) = lines {
        // `lines()` rather than `split('\n')`: the latter yields a spurious
        // trailing empty element for text that ends in a newline — which is
        // almost all captured output — so "last 2 lines" would return one, and
        // the count it reported would be wrong by one for the same reason.
        let split: Vec<&str> = body.lines().collect();
        if split.len() > n {
            let kept = split[split.len() - n..].join("\n");
            out.push_str(&format!("\n  (last {n} of {} lines)", split.len()));
            body = kept;
        }
    }
    if tail.skipped > 0 {
        out.push_str(&format!(
            "\n  (showing the last {} of {} bytes{})",
            tail.size - tail.skipped,
            tail.size,
            if tail.partial_first_line {
                "; first line is partial"
            } else {
                ""
            }
        ));
    }
    if tail.collapsed > 0 {
        out.push_str(&format!(
            "\n  ({} progress frames collapsed — last frame kept per line)",
            tail.collapsed
        ));
    }
    for line in body.lines() {
        out.push_str("\n  ");
        out.push_str(line);
    }
    if body.is_empty() {
        out.push_str("\n  (no complete lines yet)");
    }
}

#[async_trait]
impl Tool for TaskOutputTool {
    fn name(&self) -> &str {
        "task_output"
    }

    fn description(&self) -> &str {
        "Read a detached command's captured output by run id — live while it is \
         still running, complete once it has finished. The run id comes from \
         tasks_list. Use this instead of re-reading a path by hand: it reports \
         the run's state alongside the bytes, collapses carriage-return progress \
         frames into their last state, and never blocks."
    }

    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "run_id": {
                    "type": "string",
                    "description": "The run id from tasks_list."
                },
                "stream": {
                    "type": "string",
                    "enum": ["stdout", "stderr", "both"],
                    "description": "Which capture to read. Default: both."
                },
                "tail_bytes": {
                    "type": "integer",
                    "description": "Bytes to read from the END of each capture \
                                    (default 16384, max 262144)."
                },
                "tail_lines": {
                    "type": "integer",
                    "description": "Optional: return only the last N lines of what was read."
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
        let Some(run_id) = input.get("run_id").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error(
                "task_output requires a run_id".to_string(),
            ));
        };
        let run_id = run_id.trim();
        let Some(stream) = Stream::parse(input.get("stream").and_then(|v| v.as_str())) else {
            return Ok(ToolResult::error(
                "stream must be one of: stdout, stderr, both".to_string(),
            ));
        };
        let tail_bytes = input
            .get("tail_bytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TAIL_BYTES)
            .clamp(1, MAX_TAIL_BYTES);
        let tail_lines = input
            .get("tail_lines")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .filter(|n| *n > 0);

        // The status record is the durable source: it survives a restart, and a
        // detached run's child does too (setsid). The in-memory handle is only
        // consulted for a run whose record has not been written yet.
        let record = WorkStatus::read(run_id);
        let handle = context
            .background_manager
            .as_ref()
            .and_then(|bm| bm.handle(run_id));

        let Some(owner) = record
            .as_ref()
            .map(|r| r.session_id.clone())
            .or_else(|| handle.as_ref().map(|h| h.session_id.to_string()))
        else {
            // Nothing to read and nothing to attribute: refusing is the only
            // honest answer. Both capture paths are derivable from the id, so a
            // tool that "helpfully" printed them would hand back the contents of
            // a stream whose ownership it cannot establish.
            return Ok(ToolResult::error(format!(
                "No run {run_id} in this process, and no status record on disk. \
                 Ids come from tasks_list; a run from a previous boot can only be \
                 read while its status record exists."
            )));
        };
        if owner != context.session_id.to_string() {
            return Ok(ToolResult::error(format!(
                "Run {run_id} belongs to another session; task_output only reads \
                 this session's runs."
            )));
        }

        let mut out = String::new();
        let label = record
            .as_ref()
            .map(|r| r.label.clone())
            .or_else(|| handle.as_ref().map(|h| h.label.clone()))
            .unwrap_or_default();
        let state = record
            .as_ref()
            .map(|r| state_label(&r.state).to_string())
            .unwrap_or_else(|| "running".to_string());
        out.push_str(&format!("Run {run_id} [{label}] — {state}"));
        let pid = record
            .as_ref()
            .and_then(|r| r.pid)
            .or_else(|| handle.as_ref().and_then(|h| h.pid));
        if let Some(p) = pid {
            out.push_str(&format!("\n  pgid: {p} (stop with: kill -- -{p})"));
        }
        if let Some(finish) = record.as_ref().and_then(|r| r.finish.as_ref()) {
            if let Some(code) = finish.code {
                out.push_str(&format!("\n  exit code: {code}"));
            }
        } else if handle.is_some() {
            out.push_str(&format!(
                "\n  elapsed: {}s",
                handle.as_ref().map(|h| h.started.elapsed().as_secs()).unwrap_or(0)
            ));
        }

        for (name, is_err) in [("stdout", false), ("stderr", true)] {
            let wanted = if is_err {
                stream.wants_err()
            } else {
                stream.wants_out()
            };
            if !wanted {
                continue;
            }
            let path = record
                .as_ref()
                .and_then(|r| {
                    if is_err {
                        r.output_err.clone()
                    } else {
                        r.output_out.clone()
                    }
                })
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    handle.as_ref().map(|h| {
                        if is_err {
                            h.output_err.clone()
                        } else {
                            h.output_out.clone()
                        }
                    })
                });
            let Some(path) = path else {
                out.push_str(&format!("\n\n{name}: (no capture path recorded)"));
                continue;
            };
            match read_tail(&path, tail_bytes).await {
                Ok(tail) => render_stream(name, &tail, &path, tail_lines, &mut out),
                // A missing file for a run we can attribute means the run never
                // reached its spawn — reported as such rather than as empty.
                Err(e) => out.push_str(&format!(
                    "\n\n{name}: unreadable at {} ({e})",
                    path.display()
                )),
            }
        }

        Ok(ToolResult::success(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn progress_frames_collapse_to_the_last_one_per_line() {
        // A progress bar rewrites one line per update. Returned raw that is
        // thousands of near-identical lines — the flood the stream was meant to
        // relieve — so only the final frame survives, and the count of what was
        // dropped travels with it: a collapsed view that did not say it
        // collapsed would read as the run's own output.
        let (out, collapsed) = collapse_frames("a\n10%\r55%\r100%\nnext\n");
        assert_eq!(out, "a\n100%\nnext\n");
        assert_eq!(collapsed, 2, "two frames were superseded, not one");
    }

    #[test]
    fn a_line_with_no_frames_is_returned_verbatim() {
        let (out, collapsed) = collapse_frames("plain\nlines\n");
        assert_eq!(out, "plain\nlines\n");
        assert_eq!(collapsed, 0);
    }

    #[test]
    fn a_trailing_empty_frame_does_not_erase_the_line() {
        // `\r` at the end of a line is a redraw that printed nothing; taking it
        // literally would blank a line that did have content.
        let (out, _) = collapse_frames("progress...\r");
        assert_eq!(out, "progress...");
    }

    #[tokio::test]
    async fn a_tail_window_starting_mid_line_drops_that_partial_line() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("out");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"first line is long\nsecond\nthird\n").unwrap();
        drop(f);

        // A 12-byte tail lands inside "second\nthird\n", so the returned text
        // must not begin with the tail of a line the run never emitted alone.
        let tail = read_tail(&path, 12).await.unwrap();
        assert_eq!(tail.size, 32);
        assert!(tail.skipped > 0, "the window must report what it skipped");
        assert!(
            tail.partial_first_line,
            "a mid-line window must say its first line is partial"
        );
        assert!(
            !tail.text.starts_with("nd\n") && !tail.text.contains("rst line"),
            "no fragment of an earlier line may appear, got {:?}",
            tail.text
        );
        assert!(tail.text.contains("third"), "got {:?}", tail.text);
    }

    #[tokio::test]
    async fn an_empty_capture_reports_zero_bytes_rather_than_absent() {
        // The run exists and has said nothing yet — distinguishable from a run
        // that never existed, which is why the file is created at spawn.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("out");
        std::fs::File::create(&path).unwrap();
        let tail = read_tail(&path, 4096).await.unwrap();
        assert_eq!(tail.size, 0);
        assert_eq!(tail.text, "");
        assert_eq!(tail.skipped, 0);
    }

    #[test]
    fn an_empty_capture_renders_explicitly_not_as_a_bare_header() {
        let dir = std::path::Path::new("/tmp");
        let mut out = String::new();
        let tail = Tail {
            text: String::new(),
            size: 0,
            skipped: 0,
            collapsed: 0,
            partial_first_line: false,
        };
        render_stream("stdout", &tail, dir, None, &mut out);
        assert!(
            out.contains("(empty so far)"),
            "an empty capture must say so, got {out:?}"
        );
    }

    #[test]
    fn tail_lines_keeps_the_end_and_says_how_much_it_hid() {
        let mut out = String::new();
        let tail = Tail {
            text: "l1\nl2\nl3\nl4\n".to_string(),
            size: 12,
            skipped: 0,
            collapsed: 0,
            partial_first_line: false,
        };
        render_stream("stdout", &tail, std::path::Path::new("/tmp/x"), Some(2), &mut out);
        assert!(out.contains("last 2 of"), "got {out:?}");
        assert!(out.contains("l3") && out.contains("l4"), "got {out:?}");
        assert!(!out.contains("  l1"), "the hidden lines must be dropped");
    }

    #[test]
    fn the_stream_selector_is_explicit_about_what_it_rejects() {
        assert!(Stream::parse(None).is_some());
        assert!(Stream::parse(Some("both")).is_some());
        assert!(Stream::parse(Some("STDOUT")).is_some());
        assert!(Stream::parse(Some("err")).is_some());
        // An unknown value must be refused, not silently treated as "both": a
        // caller asking for a stream that does not exist should be told.
        assert!(Stream::parse(Some("stdin")).is_none());
    }
}
