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

    assert!(!parsed.scope, "the one field the document names is honoured");
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
    assert!(parse_memory_max("50%").is_err(), "a percentage is not a bound");
    // A zero-byte cap cannot start a process at all.
    assert!(parse_memory_max("0").is_err(), "zero bytes cannot start");
    assert!(parse_memory_max("max").is_err(), "`max` is not a size");
    assert!(parse_memory_max("1 gig").is_err(), "a space is not a unit");
}

#[test]
fn slice_names_are_validated_before_argv() {
    assert!(validate_slice_name("oc-tasks.slice").is_ok());
    assert!(validate_slice_name("bad/name").is_err());
    assert!(validate_slice_name("oc-tasks").is_err(), "must end in .slice");
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
