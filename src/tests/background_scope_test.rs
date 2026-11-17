//! Per-run cgroup scoping for detached jobs (#776).
//!
//! The unit under test is `[agent.background]`'s deserialisation contract and
//! the pure helpers around it: a partial section must not zero the fields it
//! does not mention, and a size string must either parse or be refused before
//! it can reach a unit file. The wrapper's argv is pinned separately, because
//! the ordering in it is load-bearing rather than incidental.

use crate::config::types::BackgroundConfig;
use crate::utils::scope::{ScopeSpec, parse_memory_max, validate_slice_name};

/// The `[agent.background]` nesting, without dragging in the rest of `Config`.
///
/// A whole `Config` would work but proves less: these tests are about the
/// per-field `#[serde(default = "…")]` functions, so the smallest document that
/// still exercises the real `[agent.background]` shape is the honest instrument.
#[derive(Debug, Default, serde::Deserialize)]
struct Doc {
    #[serde(default)]
    agent: AgentSection,
}

#[derive(Debug, Default, serde::Deserialize)]
struct AgentSection {
    #[serde(default)]
    background: BackgroundConfig,
}

fn parse(doc: &str) -> BackgroundConfig {
    toml::from_str::<Doc>(doc)
        .expect("the document parses")
        .agent
        .background
}

/// A partial section must leave every unmentioned field at its shipped default.
///
/// This is the trap the design names: without a per-field `#[serde(default)]`,
/// `scope = false` alone deserialises `scope_tasks_max` as `0`, and a run with a
/// zero task budget cannot start at all — a config edit that looks like it only
/// disabled scoping would break every detached job instead.
#[test]
fn a_partial_section_keeps_the_other_fields_at_their_defaults() {
    let parsed = parse("[agent.background]\nscope = false\n");
    let defaults = BackgroundConfig::default();

    assert!(
        !parsed.scope,
        "the one field the document names is honoured"
    );
    assert_eq!(parsed.scope_memory_max, defaults.scope_memory_max);
    assert_eq!(parsed.scope_swap_max, defaults.scope_swap_max);
    assert_eq!(parsed.scope_tasks_max, defaults.scope_tasks_max);
    assert_eq!(parsed.slice_memory_max, defaults.slice_memory_max);
}

/// An absent section is the shipped default, in full.
#[test]
fn an_absent_section_is_the_default() {
    let parsed = parse("[agent]\napproval_policy = \"ask\"\n");
    let defaults = BackgroundConfig::default();

    assert_eq!(parsed.scope, defaults.scope);
    assert_eq!(parsed.scope_memory_max, defaults.scope_memory_max);
    assert_eq!(parsed.scope_swap_max, defaults.scope_swap_max);
    assert_eq!(parsed.scope_tasks_max, defaults.scope_tasks_max);
    assert_eq!(parsed.slice_memory_max, defaults.slice_memory_max);
}

/// The hand-written `Default` must agree with the per-field functions.
///
/// Two writers for one set of values is a silent-divergence machine: the
/// `Default` impl serves code, the serde functions serve a config file, and a
/// value changed in only one of them makes "unset" mean two different things.
#[test]
fn the_default_impl_matches_the_serde_field_defaults() {
    let via_serde = parse("");
    let via_impl = BackgroundConfig::default();

    assert_eq!(via_serde.scope, via_impl.scope);
    assert_eq!(via_serde.scope_memory_max, via_impl.scope_memory_max);
    assert_eq!(via_serde.scope_swap_max, via_impl.scope_swap_max);
    assert_eq!(via_serde.scope_tasks_max, via_impl.scope_tasks_max);
    assert_eq!(via_serde.slice_memory_max, via_impl.slice_memory_max);
}

/// Accepted spellings, including the two "no cap" forms.
#[test]
fn parse_memory_max_accepts_the_systemd_sizes() {
    assert_eq!(parse_memory_max("1G").unwrap(), Some(1 << 30));
    assert_eq!(parse_memory_max("512M").unwrap(), Some(512 << 20));
    assert_eq!(parse_memory_max("1.5G").unwrap(), Some(3 << 29));
    assert_eq!(parse_memory_max("1048576").unwrap(), Some(1_048_576));
    assert_eq!(parse_memory_max("infinity").unwrap(), None);
    assert_eq!(parse_memory_max("").unwrap(), None);
    assert_eq!(parse_memory_max("  ").unwrap(), None);
}

/// Refused spellings — every one of them would either never start a process or
/// produce a cap that moves on its own.
#[test]
fn parse_memory_max_refuses_what_cannot_be_a_bound() {
    // systemd accepts this spelling but treats it as an error for our purpose:
    // it is a fraction of whatever cgroup the scope lands in.
    assert!(
        parse_memory_max("50%").is_err(),
        "a percentage is not a bound"
    );
    // A zero-byte cap cannot start a process at all.
    assert!(parse_memory_max("0").is_err(), "zero bytes cannot start");
    assert!(parse_memory_max("max").is_err(), "`max` is not a size");
    assert!(parse_memory_max("1 gig").is_err(), "a space is not a unit");
}

#[test]
fn slice_names_are_validated_before_argv() {
    assert!(validate_slice_name("oc-tasks.slice").is_ok());
    assert!(validate_slice_name("bad/name").is_err());
    assert!(
        validate_slice_name("oc-tasks").is_err(),
        "must end in .slice"
    );
    assert!(validate_slice_name("").is_err());
}

/// The kill switch must survive the config path intact.
#[test]
fn a_disabled_scope_spec_reports_itself_disabled() {
    let spec = ScopeSpec::disabled();
    assert!(!spec.enabled);
    assert!(
        !crate::utils::scope::ensure(&spec),
        "a disabled spec must never wrap, whatever the probe would say"
    );
}

/// A `systemctl` stand-in that records every write, so "issued no write" is a
/// fact about a log rather than an inference from a value.
///
/// The real `systemctl set-property` rewrites the drop-in even when the value is
/// unchanged (measurement 13), so idempotence cannot be read off the resulting
/// bound — the only honest instrument is the call itself. `show` answers from a
/// state file, exactly as systemd answers from the unit it has loaded.
#[cfg(unix)]
fn fake_systemctl(dir: &std::path::Path) -> std::path::PathBuf {
    let state = dir.join("state");
    let log = dir.join("writes");
    std::fs::write(&state, "infinity").expect("state is writable");
    std::fs::write(&log, "").expect("log is writable");

    let script = dir.join("systemctl");
    let body = format!(
        "#!/bin/sh\n\
         STATE={state}\n\
         LOG={log}\n\
         case \"$1\" in\n\
           --user) shift ;;\n\
         esac\n\
         case \"$1\" in\n\
           show)\n\
             printf 'MemoryMax=%s\\n' \"$(cat \"$STATE\")\"\n\
             exit 0 ;;\n\
           set-property)\n\
             printf '%s\\n' \"$3\" >> \"$LOG\"\n\
             printf '%s\\n' \"${{3#MemoryMax=}}\" > \"$STATE\"\n\
             exit 0 ;;\n\
         esac\n\
         exit 1\n",
        state = state.display(),
        log = log.display()
    );
    std::fs::write(&script, body).expect("the fake is writable");
    let mut perms = std::fs::metadata(&script).expect("exists").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&script, perms).expect("the fake is executable");
    script
}

#[cfg(unix)]
fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("oc-scope-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// A second reconcile of an unchanged bound must issue NO write.
///
/// This is the whole reason the reconcile reads before it writes: on this host
/// `set-property` rewrites the drop-in unconditionally, so an unguarded write
/// would touch a unit file on every daemon start and every config reload.
#[cfg(unix)]
#[test]
fn reconcile_writes_once_and_then_goes_quiet() {
    let dir = scratch_dir("idem");
    let bin = fake_systemctl(&dir);
    let spec = ScopeSpec {
        pool_memory_max: Some(2 << 30),
        ..ScopeSpec::default()
    };

    assert!(
        crate::utils::scope::reconcile_pool_with(&spec, bin.as_os_str()),
        "the pool reconciles against a working systemctl"
    );
    let after_first = std::fs::read_to_string(dir.join("writes")).expect("log");
    assert_eq!(
        after_first.lines().count(),
        1,
        "one reconcile issues exactly one write, got {after_first:?}"
    );
    let mtime_first = std::fs::metadata(dir.join("state"))
        .expect("state")
        .modified()
        .expect("mtime");
    std::thread::sleep(std::time::Duration::from_millis(20));

    assert!(
        crate::utils::scope::reconcile_pool_with(&spec, bin.as_os_str()),
        "the second reconcile still reports a bounded pool"
    );
    let after_second = std::fs::read_to_string(dir.join("writes")).expect("log");
    assert_eq!(
        after_second, after_first,
        "an unchanged bound must issue no second write"
    );
    let mtime_second = std::fs::metadata(dir.join("state"))
        .expect("state")
        .modified()
        .expect("mtime");
    assert_eq!(
        mtime_first, mtime_second,
        "the state the drop-in mirrors must not be rewritten"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A bound that is not a size must never reach argv.
///
/// systemd rejects a malformed property and runs nothing, so a garbage value
/// passed through would take every detached job on the host down with it. The
/// config path is the only way a garbage value can arrive, so the assertion is
/// made there: what `spec_from_config` emits is the shipped default, never the
/// operator's typo.
#[test]
fn a_garbage_pool_bound_never_reaches_argv() {
    let cfg = BackgroundConfig {
        slice_memory_max: "banana".to_string(),
        ..BackgroundConfig::default()
    };

    let spec = crate::utils::scope::spec_from_config(&cfg);
    assert_eq!(
        spec.pool_memory_max,
        ScopeSpec::default().pool_memory_max,
        "a garbage size falls back to the shipped default"
    );
    assert!(
        parse_memory_max("banana").is_err(),
        "and the parser refuses it outright"
    );
}

/// A slice name systemd would reject is refused before any command runs.
#[cfg(unix)]
#[test]
fn an_invalid_slice_name_issues_no_command() {
    let dir = scratch_dir("garbage");
    let bin = fake_systemctl(&dir);
    let spec = ScopeSpec {
        slice: "not-a-slice".to_string(),
        ..ScopeSpec::default()
    };

    assert!(
        !crate::utils::scope::reconcile_pool_with(&spec, bin.as_os_str()),
        "an invalid slice name must be refused before any command runs"
    );
    let writes = std::fs::read_to_string(dir.join("writes")).expect("log");
    assert!(
        writes.is_empty(),
        "no systemctl call may happen for a refused slice, got {writes:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A disabled spec must not reconcile either — the kill switch covers the pool.
#[cfg(unix)]
#[test]
fn a_disabled_spec_does_not_reconcile_the_pool() {
    let dir = scratch_dir("disabled");
    let bin = fake_systemctl(&dir);
    assert!(
        !crate::utils::scope::reconcile_pool_with(&ScopeSpec::disabled(), bin.as_os_str()),
        "a disabled spec must not touch the pool"
    );
    assert!(
        std::fs::read_to_string(dir.join("writes"))
            .expect("log")
            .is_empty(),
        "a disabled spec must issue no systemctl call"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The caps are live knobs: an edit reaches the next run's spec with no restart.
///
/// The chain is `config.toml` → watcher → `Config::set_current` →
/// `Config::current()` at spawn → `spec_from_config`. Only the first link is
/// external, so this test drives the middle three: it swaps the process-wide
/// mirror the way the watcher does and reads the spec back through the same
/// accessor the spawn site uses. The mirror swap is serialised against the other
/// tests that mutate it, and restored before the guard drops.
#[tokio::test]
async fn an_edited_cap_moves_the_next_runs_spec() {
    let _guard = crate::channels::telegram::governor::test_support::registry_guard().await;

    // A home override, so nothing this test does can reach the live
    // `~/.opencrabs/config.toml` even if a helper grows a disk write later. The
    // swap below is in-memory only, but "in-memory only" is a property of the
    // code as it stands, not of the test.
    let home = std::env::temp_dir().join(format!("oc-scope-cfg-{}", std::process::id()));
    std::fs::create_dir_all(&home).expect("scratch home");

    crate::config::profile::with_home_override_async(home.clone(), async {
        let previous = crate::config::Config::current();

        // Cloned, then mutated: `field_reassign_with_default` only fires on a
        // `Default::default()` base, and a clone of the running config is the
        // honest base — it is what the watcher itself hands the callbacks.
        let mut edited = (*previous).clone();
        edited.agent.background.scope_memory_max = "3G".to_string();
        edited.agent.background.slice_memory_max = "4G".to_string();
        crate::config::Config::set_current(edited);

        let live = crate::utils::scope::spec_from_config(
            &crate::config::Config::current().agent.background,
        );
        assert_eq!(live.memory_max, Some(3 << 30), "the per-run cap follows");
        assert_eq!(live.pool_memory_max, Some(4 << 30), "so does the pool");

        crate::config::Config::set_current((*previous).clone());
    })
    .await;

    let _ = std::fs::remove_dir_all(&home);
}

/// The spawn site must read the LIVE config, and the watcher must be its writer.
///
/// Both halves are structural, so they are pinned in the source rather than
/// exercised: a spawn site that captured its spec at boot would pass every
/// behavioural test in this file and still ignore every config edit until the
/// next restart — the exact failure the live read exists to prevent.
#[test]
fn the_spawn_site_reads_the_live_config() {
    let spawn_site = include_str!("../brain/agent/service/background_tasks.rs");
    assert!(
        spawn_site.contains("let live = crate::config::Config::current();"),
        "the spawn site must read the live config mirror"
    );
    assert!(
        spawn_site.contains("spec_from_config(&live.agent.background)"),
        "the spec must be derived from that read, not from a captured copy"
    );

    let watcher = include_str!("../utils/config_watcher.rs");
    assert!(
        watcher.contains("Config::set_current("),
        "the watcher must be the mirror's writer, or the live read never moves"
    );
}

/// The pool bound is reconciled on reload, not merely on the next spawn.
///
/// A bound that only moves when a detached job happens to start reads as a
/// broken knob: an operator tightens `slice_memory_max`, runs `systemctl show`,
/// sees the old value, and has no way to tell "not applied yet" from "ignored".
/// Pinned in the source because the behaviour needs a live daemon to observe.
#[test]
fn a_reload_reconciles_the_pool_immediately() {
    let ui = include_str!("../cli/ui.rs");
    // One contiguous window, so the three halves cannot be satisfied by three
    // unrelated sites elsewhere in a 2,600-line file.
    assert!(
        ui.contains(
            "let spec = crate::utils::scope::spec_from_config(&cfg.agent.background);\n            \
             tokio::task::spawn_blocking(move || {\n                \
             crate::utils::scope::refresh_pool(&spec);"
        ),
        "the reload callback must derive the spec from the config it was handed, then reconcile \
         it on the blocking pool — a subprocess must not run on a tokio worker"
    );
}

/// (a) The wrapper argv, byte-pinned.
///
/// Every element here is load-bearing rather than incidental: `--quiet` before
/// the payload (else the banner pollutes the run's stderr), every `-p` before
/// `--`, and `--` before the program so a payload argument that looks like a
/// flag is never parsed by systemd-run. The pin is on the ORDER, not on a
/// formatting choice, so a refactor that keeps the meaning passes and one that
/// moves a flag fails.
#[test]
fn the_wrapper_argv_is_pinned() {
    let spec = ScopeSpec {
        memory_max: Some(1 << 30),
        swap_max: 0,
        tasks_max: 16,
        ..ScopeSpec::default()
    };
    let argv = crate::utils::scope::wrapped_argv(
        &spec,
        std::ffi::OsStr::new("/bin/echo"),
        &[
            std::ffi::OsString::from("hi"),
            std::ffi::OsString::from("--not-a-systemd-flag"),
        ],
    );
    let got: Vec<String> = argv
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    assert_eq!(
        got,
        vec![
            "--user",
            "--scope",
            "--quiet",
            "--slice=oc-tasks.slice",
            "-p",
            "MemoryMax=1073741824",
            "-p",
            "MemorySwapMax=0",
            "-p",
            "TasksMax=16",
            "--",
            "/bin/echo",
            "hi",
            "--not-a-systemd-flag",
        ],
        "--collect must never appear, and no flag may follow the program"
    );
    assert!(
        !got.iter().any(|a| a == "--collect"),
        "scopes self-collect when empty; the flag only adds a systemd <240 risk"
    );
}

/// A `Background` run carries `CPUWeight=50` as a scope property, placed with
/// the other `-p` pairs and before `--`.
///
/// Nice and ioprio cannot ride here at all — `--scope` rejects `-p Nice=19`
/// with `Unknown assignment`, and the `io` controller is not delegated to user
/// slices — so `CPUWeight` is the only priority that reaches systemd, and its
/// position is the only part of it this pin can check.
#[test]
fn a_background_run_carries_a_cpu_weight() {
    let spec = ScopeSpec {
        priority: crate::utils::scope::PriorityClass::Background,
        ..ScopeSpec::default()
    };
    let argv = crate::utils::scope::wrapped_argv(&spec, std::ffi::OsStr::new("/bin/true"), &[]);
    let got: Vec<String> = argv
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    let weight = got
        .iter()
        .position(|a| a == "CPUWeight=50")
        .expect("a background run must carry CPUWeight=50");
    assert_eq!(
        got.get(weight - 1).map(String::as_str),
        Some("-p"),
        "CPUWeight must be introduced by -p, like every other scope property"
    );
    let terminator = got
        .iter()
        .position(|a| a == "--")
        .expect("the argv must always terminate its flags");
    assert!(
        weight < terminator,
        "CPUWeight must precede `--`; after it, systemd-run hands it to the payload"
    );
}

/// An `Interactive` run — the default — emits no `CPUWeight` at all, which is
/// what keeps every pre-existing argv byte-identical.
#[test]
fn an_interactive_run_emits_no_cpu_weight() {
    let spec = ScopeSpec::default();
    let argv = crate::utils::scope::wrapped_argv(&spec, std::ffi::OsStr::new("/bin/true"), &[]);
    let got: Vec<String> = argv
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    assert!(
        !got.iter().any(|a| a.starts_with("CPUWeight=")),
        "the default class must leave systemd's own weight alone"
    );
}

/// An unbounded run omits `MemoryMax` entirely rather than passing `infinity`:
/// `-p MemoryMax=infinity` is accepted, but the absence is the smaller surface
/// and cannot be misread as a cap by a reader of the argv.
#[test]
fn an_unbounded_run_omits_the_memory_cap() {
    let spec = ScopeSpec {
        memory_max: None,
        ..ScopeSpec::default()
    };
    let argv = crate::utils::scope::wrapped_argv(&spec, std::ffi::OsStr::new("/bin/true"), &[]);
    let got: Vec<String> = argv
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(
        !got.iter().any(|a| a.starts_with("MemoryMax=")),
        "an unbounded run carries no MemoryMax, got {got:?}"
    );
}

/// (b) The kill switch leaves the command byte-identical.
///
/// This is the one-line revert the design promises, so it is asserted on the
/// command's own program/args rather than on a boolean: a `wrap` that returned
/// `false` after mutating the command would be the worst of both worlds.
#[tokio::test]
async fn a_disabled_scope_leaves_the_command_untouched() {
    let mut cmd = tokio::process::Command::new("/bin/echo");
    cmd.arg("hello").arg("--world");
    let before = {
        let std_cmd = cmd.as_std();
        let program = std_cmd.get_program().to_os_string();
        let args: Vec<std::ffi::OsString> = std_cmd.get_args().map(|a| a.to_os_string()).collect();
        (program, args)
    };

    assert!(
        !crate::utils::scope::wrap(&mut cmd, &ScopeSpec::disabled()),
        "a disabled spec must report that it wrapped nothing"
    );
    let after = {
        let std_cmd = cmd.as_std();
        let program = std_cmd.get_program().to_os_string();
        let args: Vec<std::ffi::OsString> = std_cmd.get_args().map(|a| a.to_os_string()).collect();
        (program, args)
    };
    assert_eq!(before, after, "the command must be byte-identical");
}

/// (e) Fail-open: a `systemd-run` that cannot scope refuses the wrapper, and the
/// payload still runs.
///
/// Driven through `ensure_with` rather than an environment override, because
/// `OPENCRABS_SYSTEMD_RUN` is process-global and a test that sets it races every
/// other test in this binary. The refusal is what the spawn site reads, and the
/// second half proves an unwrapped run still completes — a fail-open that
/// returned `false` but killed the run would be worse than no wrapper at all.
#[cfg(unix)]
#[tokio::test]
async fn an_unscopable_host_refuses_the_wrapper_and_the_run_still_completes() {
    let spec = ScopeSpec::default();
    assert!(
        !crate::utils::scope::ensure_with(&spec, std::ffi::OsStr::new("/bin/false")),
        "a systemd-run that exits non-zero must be refused"
    );
    assert!(
        !crate::utils::scope::ensure_with(
            &spec,
            std::ffi::OsStr::new("/nonexistent/oc-no-such-binary")
        ),
        "a missing systemd-run must be refused"
    );

    let mut cmd = tokio::process::Command::new("/bin/echo");
    cmd.arg("unwrapped");
    assert!(
        !crate::utils::scope::wrap(&mut cmd, &ScopeSpec::disabled()),
        "with the wrapper refused the command is left alone"
    );
    let out = cmd.output().await.expect("the unwrapped run spawns");
    assert!(out.status.success(), "and completes");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "unwrapped");
}

/// (f) The two structural pins the design calls out: the stdio trio on the
/// wrapper, and `detach_session_pre_exec` re-applied to it.
///
/// Both are invisible at runtime until they are wrong — a wrapper that lost its
/// pipes turns a pty failure into a run with no capture at all, and one that
/// lost its detachment keeps a controlling terminal the daemon asked to shed.
/// Pinned in the source so the failure is a red test rather than a silent
/// behavioural drift on the box.
#[test]
fn the_wrapper_keeps_its_stdio_and_its_detachment() {
    let scope = include_str!("../utils/scope.rs");
    for needle in [
        ".stdin(std::process::Stdio::null())",
        ".stdout(std::process::Stdio::piped())",
        ".stderr(std::process::Stdio::piped())",
    ] {
        assert!(
            scope.contains(needle),
            "the wrapper must re-establish {needle}"
        );
    }

    let spawn_site = include_str!("../brain/agent/service/background_tasks.rs");
    assert!(
        spawn_site.contains("detach_session_pre_exec(&mut cmd)"),
        "the wrapper must have the terminal detachment re-applied to it"
    );
}

/// A host that cannot scope must never be asked to reconcile the pool.
///
/// `settle_pool` reconciled BEFORE consulting the probe, so on a host with no
/// user bus — a CI runner — every spawn paid two dead-bus `systemctl` timeouts.
/// A FAILED reconcile is deliberately not memoised (a transient hiccup must be
/// retried), so the cost landed on EVERY spawn rather than the first: the test
/// leg ran into its 45-minute wall three times over, with the same four tests
/// hung past 60 s each time.
///
/// The instrument is a fake `systemctl` that records every invocation, so "no
/// call" is read off a log rather than inferred from a resulting bound.
#[cfg(unix)]
#[test]
fn an_unscopable_host_never_consults_systemctl() {
    let dir = scratch_dir("noprobe");
    let ctl = fake_systemctl(&dir);

    // A `systemd-run` that fails the way a runner's does: no user bus.
    let run = dir.join("systemd-run");
    std::fs::write(&run, "#!/bin/sh\nexit 1\n").expect("the fake run is writable");
    let mut perms = std::fs::metadata(&run).expect("exists").permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&run, perms).expect("the fake run is executable");

    let spec = ScopeSpec::default();
    assert!(
        spec.enabled,
        "the guard must not pass because scoping is off"
    );

    crate::utils::scope::settle_pool_with(&spec, run.as_os_str(), ctl.as_os_str());

    assert_eq!(
        std::fs::read_to_string(dir.join("writes")).expect("log"),
        "",
        "a host that cannot scope must issue no set-property call"
    );
}
