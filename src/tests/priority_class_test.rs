//! Priority classes for daemon-side work.
//!
//! The unit under test is `PriorityClass`: the mapping from a class to the three
//! values that actually reach the kernel, the one scope property systemd will
//! accept, and — the part that cannot be checked by reading the code — that a
//! child spawned through [`PriorityClass::apply_to_command`] really comes up at
//! the class it asked for. `pre_exec` is the only hook that can carry `nice` and
//! `ioprio` into a detached run, and a `pre_exec` that never fires looks exactly
//! like one that works until something reads the child back.

use crate::utils::scope::PriorityClass;

/// The mapping is the contract, so pin every arm.
///
/// `nice` is never negative: raising priority needs `CAP_SYS_NICE`, and a class
/// that claims to be `idle` must not quietly become a hog on a host that happens
/// to run the daemon as root. The ioprio values encode class and data in one
/// word — `(class << 13) | data` — with `2` meaning best-effort and `3` meaning
/// idle, which is why they look like magic numbers and are asserted as such.
#[test]
fn class_to_values_map_is_exact() {
    assert_eq!(PriorityClass::Interactive.nice(), 0);
    assert_eq!(PriorityClass::Background.nice(), 10);
    assert_eq!(PriorityClass::Idle.nice(), 19);

    assert_eq!(PriorityClass::Interactive.ioprio_value(), (2 << 13) | 4);
    assert_eq!(PriorityClass::Background.ioprio_value(), (2 << 13) | 7);
    assert_eq!(PriorityClass::Idle.ioprio_value(), 3 << 13);

    assert_eq!(PriorityClass::Interactive.cpu_weight(), None);
    assert_eq!(PriorityClass::Background.cpu_weight(), Some(50));
    assert_eq!(PriorityClass::Idle.cpu_weight(), Some(10));

    assert_eq!(PriorityClass::Interactive.to_string(), "interactive");
    assert_eq!(PriorityClass::Background.to_string(), "background");
    assert_eq!(PriorityClass::Idle.to_string(), "idle");
}

/// The default is the one that changes nothing, which is what makes the feature
/// safe to ship before anyone sets a key.
#[test]
fn the_default_class_is_interactive() {
    assert_eq!(PriorityClass::default(), PriorityClass::Interactive);
    assert_eq!(
        PriorityClass::Interactive.cpu_weight(),
        None,
        "the default must not add a scope property to any existing run"
    );
}

/// Parsing is forgiving about shape and strict about meaning: unknown values
/// (including `"none"`, which is an opt-out rather than a class) return `None`
/// so the caller can fall back and warn, instead of silently picking a class.
#[test]
fn parsing_is_case_insensitive_and_refuses_unknown() {
    assert_eq!(PriorityClass::parse("idle"), Some(PriorityClass::Idle));
    assert_eq!(PriorityClass::parse(" IDLE "), Some(PriorityClass::Idle));
    assert_eq!(
        PriorityClass::parse("Background"),
        Some(PriorityClass::Background)
    );
    assert_eq!(
        PriorityClass::parse("interactive"),
        Some(PriorityClass::Interactive)
    );
    assert_eq!(PriorityClass::parse("none"), None);
    assert_eq!(PriorityClass::parse(""), None);
    assert_eq!(PriorityClass::parse("low"), None);
}

/// The idle class reaches systemd as a `CPUWeight`, in the same `-p` shape as
/// every other scope property and before the `--` terminator.
///
/// This is the only priority systemd will take: `--scope` rejects `-p Nice=19`
/// with `Unknown assignment`, and the `io` controller is not delegated to user
/// slices, so `nice`/`ioprio` have to travel in the child instead.
#[test]
fn idle_class_emits_cpuweight_on_the_scope_path() {
    let spec = crate::utils::scope::ScopeSpec {
        priority: PriorityClass::Idle,
        ..crate::utils::scope::ScopeSpec::default()
    };
    let argv = crate::utils::scope::wrapped_argv(&spec, std::ffi::OsStr::new("/bin/true"), &[]);
    let got: Vec<String> = argv
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    let weight = got
        .iter()
        .position(|a| a == "CPUWeight=10")
        .expect("an idle run must carry CPUWeight=10");
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
        "CPUWeight must precede `--`, or systemd-run hands it to the payload"
    );
}

/// The behavioural leg: spawn a real child through the hook and read the class
/// back from the kernel.
///
/// `ps -o nice= -p $$` prints the SHELL's own nice value, which is inherited from
/// the fork and must therefore equal the class. A `pre_exec` that never fires, a
/// syscall that silently fails, or a class that resolves to `interactive` all
/// fail here and nowhere else — the pure mapping tests above pass either way.
#[cfg(target_os = "linux")]
#[test]
fn a_spawned_child_really_carries_the_class() {
    fn nice_of(class: PriorityClass) -> String {
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg("ps -o nice= -p $$");
        class.apply_to_command(&mut cmd);
        let out = cmd.output().expect("spawning /bin/sh must work");
        assert!(
            out.status.success(),
            "the probe shell failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    assert_eq!(
        nice_of(PriorityClass::Idle),
        "19",
        "an idle run must actually come up at nice 19"
    );
    assert_eq!(
        nice_of(PriorityClass::Background),
        "10",
        "a background run must actually come up at nice 10"
    );
    // The control: without it, a broken hook that happened to leave every child
    // at 19 would satisfy the two assertions above.
    assert_eq!(
        nice_of(PriorityClass::Interactive),
        "0",
        "the default class must leave the child's nice untouched"
    );
}

/// Block-IO priority, read back the same way.
///
/// `ionice` is not guaranteed to be installed, so the leg is skipped where it is
/// missing rather than asserted — but where it IS present, a class that never
/// reached `ioprio_set` must fail.
#[cfg(target_os = "linux")]
#[test]
fn a_spawned_child_really_carries_the_io_class() {
    if std::process::Command::new("ionice")
        .arg("-p")
        .arg(std::process::id().to_string())
        .output()
        .is_err()
    {
        return;
    }

    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.arg("-c").arg("ionice -p $$");
    PriorityClass::Idle.apply_to_command(&mut cmd);
    let out = cmd.output().expect("spawning /bin/sh must work");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "ionice could not read the child back: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("idle"),
        "an idle run must report the idle io class, got {stdout:?}"
    );
}

/// The class applied to a thread we own is the maintenance ticker's path, and it
/// must be idempotent-safe to call on the default class: `interactive` installs
/// nothing, so a host that refuses the syscall is never even asked.
#[test]
fn applying_the_default_class_to_a_thread_is_a_no_op() {
    PriorityClass::Interactive.apply_to_current_thread();
}
