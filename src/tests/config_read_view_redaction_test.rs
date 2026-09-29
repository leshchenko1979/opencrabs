//! Regression (#689): `read_config` must never put a credential into the tool
//! result, whatever path the caller used to reach the section that holds it.
//!
//! The bug had two halves and only one of them was visible from the report.
//!
//! *Resolution half.* `resolve_section` kept the FIRST segment of a dotted path
//! and returned it whenever that head named a known section, so a caller asking
//! for one narrow block (`channels.telegram.groups.-100…`) silently received
//! every sibling in `channels` — `token` included. A caller cannot opt out of a
//! widening it never asked for, so this half is the involuntary exposure.
//!
//! *Render half.* `format_toml` is bare `toml::to_string_pretty`, and serde
//! redacts nothing here: the channel `token` fields carry only
//! `#[serde(default)]`, and the `api_key` fields that DO carry
//! `skip_serializing_if` (`types.rs:2848`) merely omit themselves when UNSET —
//! which cannot protect a value that is set. So the widened block was emitted
//! verbatim, and the same is true of a DELIBERATE
//! full-section read (#408's shape). The `None` arm even carried the comment
//! `Full config — skip api_keys for safety`, asserting a guard the code did not
//! implement.
//!
//! Why the fix sits at the exit and not in an arm: `redact_tool_input` is
//! referenced only from channel DISPLAY sites, so the harness redacted what the
//! model SENDS to a tool and nothing a tool RETURNS. This result is the string
//! that enters the model context and the session message store, which is a
//! credential boundary no display flag governs. Scrubbing at `read_config`'s
//! single exit covers every arm — including arms added later, which is exactly
//! how the `#86` render-match fell behind the struct registry.
//!
//! The scrub is the log writer's UNCONDITIONAL one, not the display-gated
//! wrapper, for the reason that wrapper states about itself: a credential that
//! persists is a credential on disk regardless of a DISPLAY flag. Gating it on
//! `agent.redact_sensitive_data` would re-open the hole for any operator who
//! turned display redaction off — which is a setting this very tool reads.

use crate::brain::tools::Tool;
use crate::brain::tools::ToolExecutionContext;
use crate::brain::tools::config_tool::ConfigTool;
use crate::config::profile::with_home_override_async;
use uuid::Uuid;

/// A realistic Telegram bot token: `<9 digits>:<35 opaque chars>`.
///
/// Only the opaque half is ever asserted against, because the digit run is not
/// secret and a test that failed on it would be reporting the wrong thing.
const TG_TOKEN: &str = "123456789:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw";
const TG_SECRET_HALF: &str = "AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw";

/// A tempdir laid out as an `.opencrabs` home whose Telegram channel holds a
/// token — the exact shape this box's own `config.toml` carries inline.
///
/// Scoped with `with_home_override_async` rather than `$HOME`: the override is
/// task-local, so a parallel test run cannot read or repair another test's
/// config out from under it (#912). Nothing here touches the live home.
fn temp_home_with_channel_token() -> (tempfile::TempDir, std::path::PathBuf) {
    let config = format!(
        r#"
[agent]
approval_policy = "auto-always"

[channels.telegram]
enabled = true
token = "{TG_TOKEN}"
"#
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let opencrabs = dir.path().join(".opencrabs");
    std::fs::create_dir_all(&opencrabs).expect("create .opencrabs");
    std::fs::write(opencrabs.join("config.toml"), config).expect("write config");
    std::fs::write(opencrabs.join("keys.toml"), b"").expect("write keys");
    (dir, opencrabs)
}

async fn read(section: Option<&str>) -> String {
    let c = ToolExecutionContext::new(Uuid::new_v4());
    let input = match section {
        Some(s) => serde_json::json!({"operation": "read_config", "section": s}),
        None => serde_json::json!({"operation": "read_config"}),
    };
    let result = ConfigTool
        .execute(input, &c)
        .await
        .expect("read_config must not return an Err");
    assert!(
        result.success,
        "read_config({section:?}) must succeed on a valid config, got error: {:?}",
        result.error
    );
    result.output
}

/// The assertion every test in this file shares: the key name survives (an
/// operator must still be able to see that a field is set) and the value does
/// not. Asserting the marker alone would pass on a redactor that ate the whole
/// line, and asserting absence alone would pass on a read that returned nothing.
fn assert_key_kept_value_gone(output: &str, key: &str) {
    assert!(
        output.contains(key),
        "the key name must survive redaction so the operator can still see the \
         field exists; got:\n{output}"
    );
    assert!(
        output.contains("[REDACTED"),
        "{key}'s value must be replaced by a redaction marker; got:\n{output}"
    );
    assert!(
        !output.contains(TG_SECRET_HALF),
        "the credential value must not appear in the tool result — this is the \
         string that would enter model context and the session store; got:\n{output}"
    );
    assert!(
        !output.contains(TG_TOKEN),
        "the full token literal must not survive; got:\n{output}"
    );
}

#[tokio::test]
async fn reading_the_channels_section_redacts_the_telegram_token() {
    let (_temp, home) = temp_home_with_channel_token();
    with_home_override_async(home, async {
        let out = read(Some("channels")).await;
        assert_key_kept_value_gone(&out, "token");
    })
    .await;
}

#[tokio::test]
async fn reading_the_whole_config_redacts_the_telegram_token() {
    // The `None` arm — the one whose comment promised `skip api_keys for safety`
    // while the code did no such thing. If only the named arms were scrubbed this
    // test is the one that would catch it.
    let (_temp, home) = temp_home_with_channel_token();
    with_home_override_async(home, async {
        let out = read(None).await;
        assert_key_kept_value_gone(&out, "token");
    })
    .await;
}

#[tokio::test]
async fn a_clean_read_is_left_intact_so_the_view_stays_useful() {
    // The scrub must not be a blanket mangler: a section with no credential in it
    // has to come back readable, or operators lose the ability to inspect config
    // and will be tempted to turn the whole thing off.
    let dir = tempfile::tempdir().expect("tempdir");
    let opencrabs = dir.path().join(".opencrabs");
    std::fs::create_dir_all(&opencrabs).expect("create .opencrabs");
    std::fs::write(
        opencrabs.join("config.toml"),
        "[agent]\napproval_policy = \"auto-always\"\n",
    )
    .expect("write config");
    std::fs::write(opencrabs.join("keys.toml"), b"").expect("write keys");

    with_home_override_async(opencrabs, async {
        let out = read(Some("agent")).await;
        assert!(
            out.contains("auto-always"),
            "a credential-free read must survive the scrub verbatim; got:\n{out}"
        );
        assert!(
            !out.contains("[REDACTED"),
            "nothing in this section is secret, so no marker may appear; got:\n{out}"
        );
    })
    .await;
}
