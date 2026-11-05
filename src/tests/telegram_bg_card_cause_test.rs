//! The receipt card's cause line (#808 cue 9).
//!
//! The card is the one surface a HUMAN reads when a detached run finishes, and
//! before this cue it carried `✅`/`❌` and a duration and nothing else — owner
//! lock P3f keeps the `<summary>` that way on purpose. So the discriminator
//! #786 built (`RunCause`) reached the agent-facing surfaces and stopped
//! there: a ceiling kill, a kernel OOM kill and a command that ran `exit 137`
//! itself all arrived as the same bubble.
//!
//! These tests pin the fix in the one place P3f permits — the BODY's first
//! line — and pin the two properties that make it safe:
//!
//! * the `<summary>` stays byte-identical to P3f (the cause must never leak
//!   into it), and
//! * a legacy `BgTaskMeta` with no cause renders exactly as it did before,
//!   because the notify queue (#111) holds JSON rows written by the previous
//!   binary and `#[serde(default)]` is what lets them round-trip.

use crate::brain::agent::service::work_status::RunCause;
use crate::brain::agent::BgTaskMeta;
use crate::channels::telegram::resume::{build_bg_receipt_card, BubbleWire};

fn meta(cause: Option<RunCause>, exit_code: Option<i32>) -> BgTaskMeta {
    BgTaskMeta {
        success: exit_code == Some(0),
        label: "sleep 4000".to_string(),
        elapsed_secs: 3600.0,
        tail: "still going\n".to_string(),
        exit_code,
        cause,
    }
}

/// Pull the rich leg's HTML out of the card. Panics with the actual wire when
/// the card took the flat path, which is what a broken `<pre>` guard looks
/// like from here.
fn rich_html(meta: &BgTaskMeta) -> String {
    match build_bg_receipt_card(meta).0 {
        BubbleWire::Html(html) => html,
        BubbleWire::Markdown(md) => panic!("expected the rich wire, got markdown: {md}"),
    }
}

/// The body opens with the cause. This is the whole cue: `137` alone cannot
/// tell a ceiling kill from a self-inflicted exit code, and `signal 9` can.
#[test]
fn card_body_opens_with_the_signal_cause() {
    let html = rich_html(&meta(
        Some(RunCause::Signalled { signal: 9 }),
        Some(137),
    ));
    // First line after the summary: the italic cause, then the tail.
    let after_summary = html
        .split("</summary>")
        .nth(1)
        .expect("card must carry a <summary>");
    let first = after_summary
        .trim_start_matches('>')
        .trim_start()
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    assert_eq!(first.trim(), "<i>signal 9 (killed)</i>", "got: {first:?}");
}

/// An ordinary success says `exit 0` — the same text the agent-facing surfaces
/// render, so a human and a lane read one vocabulary.
#[test]
fn card_body_names_an_ordinary_exit() {
    let html = rich_html(&meta(Some(RunCause::Ordinary), Some(0)));
    assert!(html.contains("<i>exit 0</i>"), "got: {html}");
}

/// A ceiling kill names the ceiling, not the code. This is the case #786 was
/// filed for: the remedy is a longer `run_max_lifetime_secs`, never a code
/// change, and the card has to say so.
#[test]
fn card_body_names_the_ceiling() {
    let html = rich_html(&meta(Some(RunCause::Ceiling { limit_secs: 3600 }), Some(137)));
    assert!(html.contains("<i>ceiling 3600s</i>"), "got: {html}");
}

/// P3f lock: the `<summary>` must stay `<sub>{icon} <code>{label}</code> 🕒 {d}</sub>`
/// — icon as the SOLE outcome signal, no exit code, no wording. A cause that
/// leaked into the summary would break the owner's lock silently, so this
/// asserts the exact summary shape.
#[test]
fn summary_stays_byte_identical_to_p3f() {
    let html = rich_html(&meta(Some(RunCause::Signalled { signal: 9 }), Some(137)));
    let summary = html
        .split("<summary>")
        .nth(1)
        .and_then(|s| s.split("</summary>").next())
        .expect("card must carry a <summary>");
    assert_eq!(
        summary,
        "<sub>❌ <code>sleep 4000</code> 🕒 1h 0m</sub>",
        "P3f summary drifted"
    );
}

/// Negative control: a legacy meta (cause absent — every queue row written by
/// the previous binary) renders NO cause line at all. Without this the fix
/// would be indistinguishable from "always print something", and an old row
/// would grow a line claiming a cause nobody recorded.
#[test]
fn legacy_meta_renders_no_cause_line() {
    let html = rich_html(&meta(None, None));
    assert!(!html.contains("<i>"), "legacy meta must not grow a cause line: {html}");
    // The card is otherwise intact — this is not a "renders nothing" pass.
    assert!(html.contains("<pre>still going"), "got: {html}");
    assert!(html.contains("<summary>"), "got: {html}");
}

/// The empty-tail guard (#38) still wins: a whitespace-only tail leaves
/// nothing to put in a `<details>`, so the card is the flat one-liner. The
/// cause is therefore dropped here — recorded in the plan rather than
/// silently lost, because there is no body to open.
#[test]
fn empty_tail_still_returns_the_flat_card() {
    let mut m = meta(Some(RunCause::Signalled { signal: 9 }), Some(137));
    m.tail = "   \n".to_string();
    match build_bg_receipt_card(&m).0 {
        BubbleWire::Markdown(md) => {
            assert!(!md.contains("signal"), "flat card carries no body: {md}");
            assert!(md.contains("sleep 4000"), "got: {md}");
        }
        BubbleWire::Html(html) => panic!("empty tail must not ride the HTML wire: {html}"),
    }
}
