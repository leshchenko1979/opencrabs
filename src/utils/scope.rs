//! Per-run cgroup scoping for detached jobs (#776).
//!
//! Every detached bash job used to land in `opencrabs-ops.service`'s cgroup,
//! sharing its `MemoryMax`/`MemoryHigh`/`TasksMax` with the daemon and with
//! every other lane. The consequence is not cosmetic: the cgroup-local OOM
//! killer has the **daemon itself** in its candidate set, so one runaway lane
//! job can take the agent down with it.
//!
//! This module moves each run into its own transient scope with its own caps,
//! and puts every such scope inside a slice whose cap bounds their sum. The
//! containment is then structural: the kernel's cgroup-local killer never
//! considers a process outside the breaching cgroup, so a scope that exceeds
//! its `MemoryMax` dies alone.
//!
//! Three properties of `systemd-run --user --scope` are load-bearing here, all
//! measured on systemd 255.4:
//!
//! 1. **`--scope` execs in place.** The `Child` handle we hold IS the payload,
//!    not a supervisor, so `kill_on_drop` and the recorded pid keep pointing at
//!    the real process.
//! 2. **`--quiet` is mandatory.** Without it `systemd-run` writes
//!    `Running as unit: run-r<hex>.scope; …` to **stderr**, i.e. into every
//!    run's stderr capture.
//! 3. **`--collect` is unnecessary and harmful.** Scopes self-collect when
//!    empty; the flag only adds the systemd <240 rejection risk.
//!
//! A bad property never runs the payload at all (`Failed to parse`, rc 1), so
//! fail-open is a **pre-flight probe** ([`ensure`]), never a post-spawn error
//! check: a refused wrapper means the run goes *unwrapped*, not *dead*.

/// The systemd-run binary, overridable for tests (and for a fail-open probe
/// that needs a binary guaranteed to fail).
pub const SYSTEMD_RUN_ENV: &str = "OPENCRABS_SYSTEMD_RUN";

/// The slice every scope is placed in. Its `MemoryMax` is the pool that bounds
/// the SUM of all live runs (#776, D3).
pub const DEFAULT_SLICE: &str = "oc-tasks.slice";

/// What a wrapped run's cgroup should look like.
///
/// Sizes are bytes; `None` means `infinity` (no cap at all), which is also what
/// an empty config string parses to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeSpec {
    /// Master switch. `false` returns every spawn to a bare, unwrapped command.
    pub enabled: bool,
    /// Slice to place the scope in — the pool that bounds the sum of runs.
    pub slice: String,
    /// Per-run `MemoryMax`. `None` = `infinity`.
    pub memory_max: Option<u64>,
    /// Per-run `MemorySwapMax` in bytes. `0` forbids swap for the run, which is
    /// the default: a run that swaps is a run that outlived its budget.
    pub swap_max: u64,
    /// Per-run `TasksMax`, so one run cannot exhaust the daemon's task budget.
    pub tasks_max: u64,
}

impl Default for ScopeSpec {
    fn default() -> Self {
        Self {
            enabled: true,
            slice: DEFAULT_SLICE.to_string(),
            memory_max: Some(1 << 30),
            swap_max: 0,
            tasks_max: 16,
        }
    }
}

impl ScopeSpec {
    /// A spec that wraps nothing — the kill switch.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }
}

/// Build the per-run spec from `[agent.background]` (#776).
///
/// Called at SPAWN time against [`crate::config::Config::current()`], never
/// captured at boot: the config watcher swaps the live `Arc`, so a spec read
/// here follows an operator's edit on the next run instead of the next restart.
///
/// An invalid size never reaches argv. systemd rejects a bad `-p` with rc 1 and
/// **does not run the payload** (measurement 10), so a malformed config value
/// passed through would turn every detached job on the host into a dead one —
/// the opposite of the intent. Each bad value is warned about once and replaced
/// by its shipped default, which keeps the mechanism armed.
pub fn spec_from_config(cfg: &crate::config::types::BackgroundConfig) -> ScopeSpec {
    let fallback = ScopeSpec::default();

    let memory_max = match parse_memory_max(&cfg.scope_memory_max) {
        Ok(value) => value,
        Err(e) => {
            tracing::warn!(
                target: "background_task",
                "ignoring agent.background.scope_memory_max: {e}; using the default"
            );
            fallback.memory_max
        }
    };
    let swap_max = match parse_size_allow_zero(&cfg.scope_swap_max) {
        Ok(value) => value.unwrap_or(0),
        Err(e) => {
            tracing::warn!(
                target: "background_task",
                "ignoring agent.background.scope_swap_max: {e}; using the default"
            );
            fallback.swap_max
        }
    };
    let tasks_max = if cfg.scope_tasks_max == 0 {
        tracing::warn!(
            target: "background_task",
            "agent.background.scope_tasks_max is 0, which cannot start a process; using the default"
        );
        fallback.tasks_max
    } else {
        cfg.scope_tasks_max
    };

    ScopeSpec {
        enabled: cfg.scope,
        slice: DEFAULT_SLICE.to_string(),
        memory_max,
        swap_max,
        tasks_max,
    }
}

/// Like [`parse_memory_max`], but `"0"` is a real value rather than a mistake.
///
/// Only swap accepts it: `MemorySwapMax=0` is the meaningful "no swap for this
/// run", while `MemoryMax=0` is a cap that cannot start a process.
fn parse_size_allow_zero(raw: &str) -> Result<Option<u64>, String> {
    if raw.trim() == "0" {
        return Ok(Some(0));
    }
    parse_memory_max(raw)
}

/// The `systemd-run` binary to invoke.
pub fn systemd_run_bin() -> std::ffi::OsString {
    std::env::var_os(SYSTEMD_RUN_ENV).unwrap_or_else(|| "systemd-run".into())
}

/// Wrap `cmd` so it runs inside its own transient scope, returning `true` when
/// the command was rewritten.
///
/// The payload's program, args, environment and cwd are preserved; `--scope`
/// makes systemd exec the payload in place, so identity survives the wrapper.
/// stdio is NOT taken from the caller's command — it cannot be read back, so
/// the wrapper re-establishes the trio both spawn paths use. The pty is still
/// installed by the caller *after* this call, and that ordering is what keeps
/// the pty on the payload rather than on `systemd-run`.
///
/// Returns `false` (leaving `cmd` untouched) when scoping is disabled or
/// [`ensure`]'s probe has already refused the wrapper.
pub fn wrap(cmd: &mut tokio::process::Command, spec: &ScopeSpec) -> bool {
    if !ensure(spec) {
        return false;
    }

    let std_cmd = cmd.as_std();
    let program = std_cmd.get_program().to_os_string();
    let args: Vec<std::ffi::OsString> = std_cmd
        .get_args()
        .map(std::ffi::OsStr::to_os_string)
        .collect();
    let envs: Vec<(std::ffi::OsString, Option<std::ffi::OsString>)> = std_cmd
        .get_envs()
        .map(|(k, v)| (k.to_os_string(), v.map(std::ffi::OsStr::to_os_string)))
        .collect();
    let cwd = std_cmd.get_current_dir().map(std::path::Path::to_path_buf);

    let mut wrapped = std::process::Command::new(systemd_run_bin());
    wrapped.arg("--user").arg("--scope");
    // Mandatory: without it systemd-run writes its banner to the run's stderr.
    wrapped.arg("--quiet");
    wrapped.arg(format!("--slice={}", spec.slice));
    if let Some(max) = spec.memory_max {
        wrapped.arg("-p").arg(format!("MemoryMax={max}"));
    }
    wrapped
        .arg("-p")
        .arg(format!("MemorySwapMax={}", spec.swap_max));
    wrapped.arg("-p").arg(format!("TasksMax={}", spec.tasks_max));
    wrapped.arg("--").arg(&program).args(&args);
    // The stdio trio is re-established, not inherited: replacing the inner
    // `std::process::Command` drops whatever was configured on it, and the pty
    // path treats "no pty" as "this run keeps the pipes it already had". A
    // wrapper that left stdio unset would turn a pty failure into a run with
    // no capture at all — a silent loss of the run's output, not a degraded
    // stream. Both spawn paths set exactly this trio.
    wrapped
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    for (key, value) in envs {
        match value {
            Some(value) => {
                wrapped.env(key, value);
            }
            None => {
                wrapped.env_remove(key);
            }
        }
    }
    if let Some(dir) = cwd {
        wrapped.current_dir(dir);
    }

    // Only the inner `std::process::Command` is replaced: tokio keeps its own
    // `kill_on_drop` flag beside it, so the caller's choice survives untouched.
    *cmd.as_std_mut() = wrapped;
    true
}

/// Whether this process can wrap at all — the fail-open gate.
///
/// The probe runs ONCE per process and is memoised: it costs a `systemd-run`
/// invocation (~29 ms, measurement 16) on a path that is now most bash calls.
/// A refused wrapper is remembered as refused, because the failure mode of
/// re-probing per spawn is a 29 ms tax on every detached job to learn the same
/// answer.
pub fn ensure(spec: &ScopeSpec) -> bool {
    if !spec.enabled {
        return false;
    }
    *PROBE.get_or_init(probe)
}

#[cfg(unix)]
fn probe() -> bool {
    let bin = systemd_run_bin();
    // A real scope, not `--version`: measurement 10 shows a bad property or
    // slice name never runs the payload, so the only trustworthy probe is one
    // that actually spawns and exits 0.
    let attempt = std::process::Command::new(&bin)
        .arg("--user")
        .arg("--scope")
        .arg("--quiet")
        .arg("-p")
        .arg("MemoryMax=64M")
        .arg("--")
        .arg("/bin/true")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output();

    match attempt {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            tracing::warn!(
                target: "background_task",
                "cgroup scoping unavailable ({} exited {}: {}); detached runs stay in the daemon's cgroup",
                bin.to_string_lossy(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
            false
        }
        Err(e) => {
            tracing::warn!(
                target: "background_task",
                "cgroup scoping unavailable ({} not runnable: {e}); detached runs stay in the daemon's cgroup",
                bin.to_string_lossy()
            );
            false
        }
    }
}

/// Parse a systemd size string into bytes. `None` means `infinity`.
///
/// Accepted: `1G`, `512M`, `1.5G`, `1048576`, `infinity`, and the empty string
/// (which means "unset", i.e. infinity). Rejected: `max`, `0`, `1 gig`, and any
/// percentage.
///
/// Percentages are refused even though systemd accepts them: a percentage is a
/// fraction of whatever cgroup the scope happens to land in, so the cap would
/// move with the pool. A budget that silently rescales is not a budget, and the
/// whole point of this module is a bound that does not drift.
pub fn parse_memory_max(raw: &str) -> Result<Option<u64>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("infinity") {
        return Ok(None);
    }
    if trimmed.contains('%') {
        return Err(format!(
            "{raw:?} is a percentage, not a fixed size; a cap that rescales with the pool is not a bound"
        ));
    }

    let last = trimmed
        .chars()
        .next_back()
        .expect("non-empty after the guard above");
    let (digits, multiplier) = match last.to_ascii_uppercase() {
        'K' => (&trimmed[..trimmed.len() - 1], 1u64 << 10),
        'M' => (&trimmed[..trimmed.len() - 1], 1u64 << 20),
        'G' => (&trimmed[..trimmed.len() - 1], 1u64 << 30),
        'T' => (&trimmed[..trimmed.len() - 1], 1u64 << 40),
        _ => (trimmed, 1u64),
    };

    let value: f64 = digits
        .trim()
        .parse()
        .map_err(|_| format!("{raw:?} is not a size"))?;
    if !value.is_finite() || value < 0.0 {
        return Err(format!("{raw:?} is not a size"));
    }
    let bytes = (value * multiplier as f64) as u64;
    if bytes == 0 {
        return Err(format!(
            "{raw:?} would cap the run at zero bytes, which cannot start"
        ));
    }
    Ok(Some(bytes))
}

/// Validate a slice name before it reaches argv.
///
/// systemd rejects a bad unit name with rc 1 and never runs the payload
/// (measurement 10), so an invalid name here means a dead run rather than an
/// unwrapped one. Refusing early keeps that failure in the config layer.
pub fn validate_slice_name(raw: &str) -> Result<(), String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("slice name is empty".to_string());
    }
    if !name.ends_with(".slice") {
        return Err(format!("{raw:?} does not end in .slice"));
    }
    if name.len() > 200 {
        return Err(format!("{raw:?} is longer than systemd allows"));
    }
    if name.starts_with('.') || name.starts_with('-') {
        return Err(format!("{raw:?} starts with a reserved character"));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '\\')))
    {
        return Err(format!(
            "{raw:?} contains {bad:?}, which systemd does not accept in a unit name"
        ));
    }
    Ok(())
}

/// Memoised result of [`probe`]. A refused wrapper stays refused for the life
/// of the process.
static PROBE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Non-Unix hosts have no `systemd-run --scope`, so the module degrades to
/// "never wrap" — exactly the pre-#776 behaviour, and no probe to pay for.
#[cfg(not(unix))]
fn probe() -> bool {
    tracing::debug!(
        target: "background_task",
        "cgroup scoping is unsupported on this platform; detached runs stay in the daemon's cgroup"
    );
    false
}
