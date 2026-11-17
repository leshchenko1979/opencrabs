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

/// CPU/IO priority for a spawned run.
///
/// Only ever *lower* priority: a negative nice needs `CAP_SYS_NICE`, so a
/// daemon that cannot acquire it would silently keep the default. Every class
/// here therefore sits at or below the process default.
///
/// The split between what rides on `systemd-run`'s argv and what is applied in
/// a `pre_exec` hook is measured, not stylistic (2026-11-16): `--scope` rejects
/// `-p Nice=19` with `Unknown assignment: Nice=19` (rc 1, payload never runs),
/// and the `io` controller is not delegated to user slices, so an `IOWeight=`
/// property is equally unreachable. `CPUWeight=` *is* accepted on the scope
/// path, so only that one is an argv property; nice and ioprio are set on the
/// child itself, just before it execs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PriorityClass {
    /// The daemon's own priority: nothing is changed.
    #[default]
    Interactive,
    /// Nice 10, best-effort IO class 7, `CPUWeight=50`.
    Background,
    /// Nice 19, idle IO class, `CPUWeight=10`.
    Idle,
}

/// `ioprio_set(2)` for the calling thread, via the raw syscall.
///
/// `libc` 0.2.189 ships NEITHER `ioprio_set` NOR `IOPRIO_WHO_PROCESS`: the crate
/// exposes the per-arch syscall NUMBER (`SYS_ioprio_set`) and no wrapper, which
/// is why calling `libc::ioprio_set(..)` fails to compile (E0425). The call is
/// made directly instead.
///
/// `syscall` is async-signal-safe, so this stays legal inside a `pre_exec`
/// closure, where only async-signal-safe calls are permitted.
#[cfg(target_os = "linux")]
fn ioprio_set_self(ioprio: libc::c_int) -> libc::c_int {
    // `IOPRIO_WHO_PROCESS` from <linux/ioprio.h>: the `who` argument names a
    // process; `which` 0 selects the calling thread.
    const IOPRIO_WHO_PROCESS: libc::c_int = 1;
    // SAFETY: ioprio_set(2) takes (int who, int which, int ioprio) and touches
    // no memory this program owns.
    unsafe {
        libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PROCESS as libc::c_long,
            0 as libc::c_long,
            ioprio as libc::c_long,
        ) as libc::c_int
    }
}

impl PriorityClass {
    /// Nice value to set on the child.
    ///
    /// Never negative — raising priority needs `CAP_SYS_NICE`, and a daemon
    /// without it would silently keep the default.
    pub fn nice(self) -> i32 {
        match self {
            Self::Interactive => 0,
            Self::Background => 10,
            Self::Idle => 19,
        }
    }

    /// `ioprio_set` value: best-effort class 2 with data 4/7, or idle class 3.
    ///
    /// Encoded as `(class << 13) | data`, the layout `ioprio_set(2)` expects.
    pub fn ioprio_value(self) -> libc::c_int {
        match self {
            Self::Interactive => (2 << 13) | 4,
            Self::Background => (2 << 13) | 7,
            Self::Idle => 3 << 13,
        }
    }

    /// `CPUWeight=` for the transient scope, or `None` to leave systemd's own
    /// default (100) alone. `Interactive` returns `None` rather than
    /// `Some(100)` so an unwrapped argv stays byte-identical to the argv it
    /// produced before priority classes existed, and the pinned-argv test keeps
    /// meaning what it says.
    pub fn cpu_weight(self) -> Option<u16> {
        match self {
            Self::Interactive => None,
            Self::Background => Some(50),
            Self::Idle => Some(10),
        }
    }

    /// Parse a config value. Case-insensitive and trimmed; `None` for anything
    /// unrecognised — including `"none"`, which the config layer treats as
    /// "leave the process default" without going through a class at all.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "interactive" => Some(Self::Interactive),
            "background" => Some(Self::Background),
            "idle" => Some(Self::Idle),
            _ => None,
        }
    }

    /// Install the priority on a child about to be spawned.
    ///
    /// Failures are reported from the child and swallowed: a host that refuses
    /// `setpriority`/`ioprio_set` must still run the job. `Interactive` installs
    /// nothing at all, so a spec at the default leaves the command untouched.
    pub fn apply_to_command(self, cmd: &mut std::process::Command) {
        if self == Self::Interactive {
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;

            let nice = self.nice();
            #[cfg(target_os = "linux")]
            let ioprio = self.ioprio_value();
            // SAFETY: the closure runs between fork and exec, where only
            // async-signal-safe calls are permitted — these syscalls are, and
            // the warning path uses a raw `write(2)` rather than allocating.
            unsafe {
                cmd.pre_exec(move || {
                    if libc::setpriority(libc::PRIO_PROCESS, 0, nice) != 0 {
                        warn_from_child(b"priority: setpriority failed\n");
                    }
                    // `ioprio_set` is Linux-only; macOS has no block-IO priority.
                    #[cfg(target_os = "linux")]
                    if ioprio_set_self(ioprio) != 0 {
                        warn_from_child(b"priority: ioprio_set failed\n");
                    }
                    Ok(())
                });
            }
        }
        // Neither syscall exists off unix; the class still parses and still
        // reports itself, it just changes nothing.
        #[cfg(not(unix))]
        {
            let _ = cmd;
        }
    }

    /// Install the class on the CALLING thread.
    ///
    /// The sibling of [`Self::apply_to_command`] for a thread we own outright —
    /// a housekeeping ticker, where there is no child to hand the class to and
    /// no fork window to respect, so a refusal is logged through `tracing`
    /// rather than a raw `write(2)`.
    ///
    /// Both syscalls are per-thread on Linux: `setpriority` with `PRIO_PROCESS`
    /// and pid 0 re-nices the calling thread, and `ioprio_set` is thread-scoped
    /// the same way. So this must run INSIDE the thread it is meant to cover,
    /// never on the thread that spawns it — and it deliberately does not raise
    /// priority (a negative nice needs `CAP_SYS_NICE`), because a class that
    /// claims to be `idle` must never quietly become a hog.
    pub fn apply_to_current_thread(self) {
        if self == Self::Interactive {
            return;
        }
        #[cfg(unix)]
        {
            let nice = self.nice();
            // SAFETY: a plain syscall on the calling thread, no pointers.
            if unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) } != 0 {
                tracing::warn!(
                    nice,
                    error = %std::io::Error::last_os_error(),
                    "priority: setpriority on the calling thread failed"
                );
            }
            // `ioprio_set` is Linux-only; macOS has no block-IO priority.
            #[cfg(target_os = "linux")]
            {
                let ioprio = self.ioprio_value();
                // The syscall's SAFETY note lives with the call, in
                // `ioprio_set_self` — no raw syscall is written here.
                if ioprio_set_self(ioprio) != 0 {
                    tracing::warn!(
                        ioprio,
                        error = %std::io::Error::last_os_error(),
                        "priority: ioprio_set on the calling thread failed"
                    );
                }
            }
        }
    }
}

impl std::fmt::Display for PriorityClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Interactive => "interactive",
            Self::Background => "background",
            Self::Idle => "idle",
        })
    }
}

/// Warn from the child between `fork` and `exec`, where allocation, locking and
/// `tracing` are all forbidden. A raw `write(2)` is async-signal-safe, so the
/// message cannot deadlock the fork.
#[cfg(unix)]
fn warn_from_child(msg: &[u8]) {
    // SAFETY: `msg` is a static byte slice and fd 2 is the child's stderr.
    unsafe {
        libc::write(2, msg.as_ptr().cast(), msg.len());
    }
}

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
    /// `MemoryMax` for the slice itself — the pool that bounds the SUM of every
    /// live run. `None` = `infinity`, i.e. no pool bound at all.
    pub pool_memory_max: Option<u64>,
    /// CPU/IO priority for the run. Only `CPUWeight` rides on the argv; nice
    /// and ioprio are applied by a `pre_exec` hook, because `--scope` refuses
    /// both as properties.
    pub priority: PriorityClass,
}

impl Default for ScopeSpec {
    fn default() -> Self {
        Self {
            enabled: true,
            slice: DEFAULT_SLICE.to_string(),
            memory_max: Some(1 << 30),
            swap_max: 0,
            tasks_max: 16,
            pool_memory_max: Some(2 << 30),
            priority: PriorityClass::Interactive,
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
    let pool_memory_max = match parse_memory_max(&cfg.slice_memory_max) {
        Ok(value) => value,
        Err(e) => {
            tracing::warn!(
                target: "background_task",
                "ignoring agent.background.slice_memory_max: {e}; using the default"
            );
            fallback.pool_memory_max
        }
    };

    ScopeSpec {
        enabled: cfg.scope,
        slice: DEFAULT_SLICE.to_string(),
        memory_max,
        swap_max,
        tasks_max,
        pool_memory_max,
        // The class comes from `[agent.background].priority`, resolved
        // through the accessor so the `"none"` opt-out and the typo fallback
        // live in one place instead of being re-parsed here.
        priority: cfg.priority_class(),
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

/// The `systemctl` binary, overridable for tests.
pub const SYSTEMCTL_ENV: &str = "OPENCRABS_SYSTEMCTL";

/// The `systemctl` binary to invoke when reconciling the pool slice.
pub fn systemctl_bin() -> std::ffi::OsString {
    std::env::var_os(SYSTEMCTL_ENV).unwrap_or_else(|| "systemctl".into())
}

/// Bring the pool slice's `MemoryMax` in line with the spec, ONCE per desired
/// value, and report whether the pool is (now) bounded as asked.
///
/// The ORDERING is the whole point: `systemd-run --slice=<name>` creates a
/// missing slice on demand, and a slice created that way lands at
/// `MemoryMax=infinity` (measurement 15). A pool reconciled after the first
/// spawn therefore reconciles a slice that already exists *and is already
/// unbounded* — and on a fresh host that is exactly the state the first run
/// leaves behind. [`ensure`] reconciles before any wrapped spawn for this
/// reason — though not before its probe, which [`settle_pool_with`] consults
/// first so a host that cannot scope is never asked for a `systemctl` call.
///
/// The write is a read-before-write, not a courtesy: `systemctl set-property`
/// rewrites the drop-in even when the value is unchanged (measurement 13), so
/// writing unconditionally would touch the unit file on every daemon start and
/// every config reload. An unchanged bound issues no command at all.
///
/// A failure here is logged and NOT fatal to scoping: the per-run cap still
/// contains a single run, so a missing pool degrades the sum bound rather than
/// the mechanism. `false` says the pool was not established.
///
/// Takes an explicit binary so a test can drive a fake `systemctl` without
/// mutating process-global environment.
pub fn reconcile_pool_with(spec: &ScopeSpec, bin: &std::ffi::OsStr) -> bool {
    if !spec.enabled {
        return false;
    }
    if let Err(e) = validate_slice_name(&spec.slice) {
        tracing::warn!(
            target: "background_task",
            "not reconciling the pool: {e}"
        );
        return false;
    }

    let desired = pool_arg(spec.pool_memory_max);
    match read_pool_memory_max(&spec.slice, bin) {
        Ok(current) if same_bound(&current, spec.pool_memory_max) => {
            tracing::debug!(
                target: "background_task",
                "pool {} already at MemoryMax={current}; no write",
                spec.slice
            );
        }
        Ok(current) => {
            if !write_pool_memory_max(&spec.slice, &desired, bin) {
                return false;
            }
            tracing::info!(
                target: "background_task",
                "pool {} MemoryMax {current} -> {desired}",
                spec.slice
            );
        }
        Err(e) => {
            // A read we cannot trust is not a licence to skip the write: the
            // pool bound is the only thing bounding the SUM of runs, and an
            // unwritten bound is unbounded rather than merely unverified.
            tracing::warn!(
                target: "background_task",
                "cannot read the pool bound ({e}); writing MemoryMax={desired} anyway"
            );
            if !write_pool_memory_max(&spec.slice, &desired, bin) {
                return false;
            }
        }
    }

    advise_on_bounds(spec);
    true
}

/// The `MemoryMax` value systemd expects: bytes, or the literal `infinity`.
fn pool_arg(value: Option<u64>) -> String {
    match value {
        Some(bytes) => bytes.to_string(),
        None => "infinity".to_string(),
    }
}

/// Whether what systemd reports already satisfies the desired bound.
///
/// Compared NUMERICALLY, never as text: systemd echoes bytes for a byte-set
/// property but `infinity` for an unset one, and a host whose drop-in was
/// written by hand may carry `1G`. A text comparison would rewrite all three.
/// An unrecognised spelling is "not equal" — rewriting is the safe direction.
fn same_bound(current: &str, desired: Option<u64>) -> bool {
    match parse_memory_max(current) {
        Ok(Some(bytes)) => Some(bytes) == desired,
        Ok(None) => desired.is_none(),
        Err(_) => false,
    }
}

/// Read the pool slice's current `MemoryMax` as systemd spells it.
///
/// `systemctl show` answers for a slice that does not exist yet, with
/// `MemoryMax=infinity` and rc 0 — "absent" and "unbounded" are the same
/// answer here, which is why a desired bound must be WRITTEN rather than
/// assumed present.
fn read_pool_memory_max(slice: &str, bin: &std::ffi::OsStr) -> Result<String, String> {
    let out = std::process::Command::new(bin)
        .arg("--user")
        .arg("show")
        .arg(slice)
        .arg("-p")
        .arg("MemoryMax")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| format!("{} not runnable: {e}", bin.to_string_lossy()))?;
    if !out.status.success() {
        return Err(format!(
            "`systemctl show {slice} -p MemoryMax` exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("MemoryMax="))
        .map(|value| value.trim().to_string())
        .ok_or_else(|| format!("`systemctl show {slice} -p MemoryMax` printed no MemoryMax line"))
}

/// Write the pool bound. `false` on any failure, with the reason logged.
fn write_pool_memory_max(slice: &str, value: &str, bin: &std::ffi::OsStr) -> bool {
    let attempt = std::process::Command::new(bin)
        .arg("--user")
        .arg("set-property")
        .arg(slice)
        .arg(format!("MemoryMax={value}"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output();
    match attempt {
        Ok(out) if out.status.success() => true,
        Ok(out) => {
            tracing::warn!(
                target: "background_task",
                "could not set {} MemoryMax={value}: `systemctl set-property` exited {}: {}; \
                 the sum of live runs stays unbounded",
                slice,
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
            false
        }
        Err(e) => {
            tracing::warn!(
                target: "background_task",
                "could not set {} MemoryMax={value}: {} not runnable: {e}",
                slice,
                bin.to_string_lossy()
            );
            false
        }
    }
}

/// The two configurations that are legal but cannot be honoured. Advisory only:
/// both are the operator's call, and refusing to start would be worse than
/// running with a bound that is merely tighter than intended.
fn advise_on_bounds(spec: &ScopeSpec) {
    if let (Some(pool), Some(run)) = (spec.pool_memory_max, spec.memory_max)
        && pool < run
    {
        tracing::warn!(
            target: "background_task",
            "slice_memory_max ({pool} B) is below scope_memory_max ({run} B): the pool is the \
             binding limit, so a run is killed by its pool before it can reach its own cap"
        );
    }
    let Some(pool) = spec.pool_memory_max else {
        return;
    };
    if let (Some(daemon), Some(total)) = (daemon_memory_max(), mem_total())
        && pool.saturating_add(daemon) > total
    {
        tracing::warn!(
            target: "background_task",
            "slice_memory_max ({pool} B) plus the daemon's own cap ({daemon} B) exceeds MemTotal \
             ({total} B): if both reach their caps the kernel's global OOM killer chooses, and the \
             daemon is a candidate"
        );
    }
}

/// The effective `MemoryMax` of the cgroup this process runs in, read from the
/// cgroup filesystem rather than from a unit name — the daemon's unit name is
/// not this module's business, and its own cgroup IS its cap.
fn daemon_memory_max() -> Option<u64> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?
        .trim();
    let raw = std::fs::read_to_string(format!("/sys/fs/cgroup{path}/memory.max")).ok()?;
    let raw = raw.trim();
    if raw == "max" {
        return None;
    }
    raw.parse().ok()
}

/// `MemTotal` in bytes, from `/proc/meminfo`.
fn mem_total() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: u64 = meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kb.saturating_mul(1024))
}

/// The exact argv `systemd-run` is invoked with, as a pure function.
///
/// Separated from [`wrap`] because the ORDER is load-bearing and cannot be
/// observed from a live spawn without a running systemd: `--quiet` must precede
/// the payload (else the banner lands in the run's stderr capture), the `-p`
/// properties must precede `--`, and `--` must precede the program so a payload
/// arg that looks like a flag is never parsed by systemd-run. Pinning the argv
/// as data makes a reordering a test failure instead of a behaviour change.
pub fn wrapped_argv(
    spec: &ScopeSpec,
    program: &std::ffi::OsStr,
    args: &[std::ffi::OsString],
) -> Vec<std::ffi::OsString> {
    let mut argv: Vec<std::ffi::OsString> = Vec::with_capacity(12 + args.len());
    argv.push("--user".into());
    argv.push("--scope".into());
    // Mandatory: without it systemd-run writes its banner to the run's stderr.
    argv.push("--quiet".into());
    argv.push(format!("--slice={}", spec.slice).into());
    if let Some(max) = spec.memory_max {
        argv.push("-p".into());
        argv.push(format!("MemoryMax={max}").into());
    }
    argv.push("-p".into());
    argv.push(format!("MemorySwapMax={}", spec.swap_max).into());
    argv.push("-p".into());
    argv.push(format!("TasksMax={}", spec.tasks_max).into());
    if let Some(weight) = spec.priority.cpu_weight() {
        argv.push("-p".into());
        argv.push(format!("CPUWeight={weight}").into());
    }
    argv.push("--".into());
    argv.push(program.to_os_string());
    argv.extend(args.iter().cloned());
    argv
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
    wrapped.args(wrapped_argv(spec, &program, &args));
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
    // BEFORE any wrapped spawn: a slice created on demand by `--slice=` is born
    // unbounded (measurement 15), so a pool reconciled after the first run would
    // be reconciling a bound the first run already escaped. It is NOT before the
    // probe — `settle_pool` consults the same probe first, and a host that
    // cannot scope gets no `systemctl` call at all.
    settle_pool(spec);
    ensure_with(spec, &systemd_run_bin())
}

/// [`ensure`] against an explicit binary, so a test can prove the fail-open
/// refusal without mutating process-global environment.
///
/// The memo is keyed by the binary rather than held in a single `OnceLock`:
/// one process may legitimately be asked about more than one `systemd-run`
/// (the real one, and a test's stand-in), and a single flag would let the first
/// answer speak for the second.
pub fn ensure_with(spec: &ScopeSpec, bin: &std::ffi::OsStr) -> bool {
    spec.enabled && scoping_available(bin)
}

/// Whether this host can scope at all — the memoised `systemd-run` probe.
///
/// Split out because [`settle_pool`] needs the same answer. A host that cannot
/// scope must never be asked to reconcile a pool it will never use: every
/// `systemctl` call there is a dead-bus timeout, and a failed reconcile is
/// deliberately NOT memoised (a transient hiccup has to be retried), so without
/// this gate each spawn pays two timeouts. On a CI runner the test suite spawns
/// thousands of times and the leg runs into its 45-minute wall.
fn scoping_available(bin: &std::ffi::OsStr) -> bool {
    let mut probes = PROBE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(known) = probes.get(bin) {
        return *known;
    }
    let verdict = probe_bin(bin);
    probes.insert(bin.to_os_string(), verdict);
    verdict
}

/// Re-reconcile the pool NOW — the config watcher's entry point.
///
/// [`settle_pool`] already runs on the spawn path, so a bound edited while the
/// daemon is idle would otherwise wait for the next detached job to happen to
/// start. This is what makes `slice_memory_max` a LIVE knob rather than a
/// next-spawn knob. It is cheap when nothing changed: an unchanged bound is
/// memoised, and the read-before-write means an unchanged bound issues no
/// `systemctl` call even on the miss.
///
/// Call it from the blocking pool — `systemctl` is a subprocess, and a config
/// reload must not stall a tokio worker on it.
pub fn refresh_pool(spec: &ScopeSpec) {
    settle_pool(spec);
}

/// The pool bound already reconciled, keyed by slice and desired value, so a
/// spawn costs one uncontended lock and no subprocess.
///
/// Keyed rather than "once": `slice_memory_max` is a live knob, and a bound that
/// only takes effect after a restart is the failure this module exists to
/// remove. A FAILED reconcile is deliberately not recorded, so a transient
/// systemd hiccup is retried on the next spawn instead of being remembered as
/// settled.
static POOL: std::sync::Mutex<Option<(String, Option<u64>)>> = std::sync::Mutex::new(None);

fn settle_pool(spec: &ScopeSpec) {
    settle_pool_with(spec, &systemd_run_bin(), &systemctl_bin());
}

/// [`settle_pool`] against explicit binaries, so a test can prove that a host
/// which cannot scope issues no `systemctl` call at all — without mutating
/// process-global environment in a parallel suite.
pub(crate) fn settle_pool_with(
    spec: &ScopeSpec,
    run_bin: &std::ffi::OsStr,
    ctl_bin: &std::ffi::OsStr,
) {
    // No probe answer, no `systemctl`. Reconciling a pool on a host that cannot
    // scope is two dead-bus timeouts per spawn — and because a failed reconcile
    // is not memoised, the cost lands on EVERY spawn rather than the first.
    if !scoping_available(run_bin) {
        return;
    }
    let key = (spec.slice.clone(), spec.pool_memory_max);
    {
        let guard = match POOL.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.as_ref() == Some(&key) {
            return;
        }
    }
    // The lock is NOT held across the subprocess calls: a reconcile that blocks
    // would serialise every concurrent spawn behind it. Two racing spawns can
    // both reconcile, which is harmless — the second reads the bound the first
    // wrote and issues no write.
    if reconcile_pool_with(spec, ctl_bin) {
        let mut guard = match POOL.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *guard = Some(key);
    }
}

#[cfg(unix)]
fn probe_bin(bin: &std::ffi::OsStr) -> bool {
    // A real scope, not `--version`: measurement 10 shows a bad property or
    // slice name never runs the payload, so the only trustworthy probe is one
    // that actually spawns and exits 0.
    let attempt = std::process::Command::new(bin)
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

/// Memoised probe results, keyed by the `systemd-run` binary. A refused wrapper
/// stays refused for the life of the process.
static PROBE: std::sync::Mutex<
    std::collections::BTreeMap<std::ffi::OsString, bool>,
> = std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Non-Unix hosts have no `systemd-run --scope`, so the module degrades to
/// "never wrap" — exactly the pre-#776 behaviour, and no probe to pay for.
#[cfg(not(unix))]
fn probe_bin(_bin: &std::ffi::OsStr) -> bool {
    tracing::debug!(
        target: "background_task",
        "cgroup scoping is unsupported on this platform; detached runs stay in the daemon's cgroup"
    );
    false
}
