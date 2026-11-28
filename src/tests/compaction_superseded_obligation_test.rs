//! #1022 — a superseded summary must stop claiming an OPEN obligation.
//!
//! A delta apply leaves every earlier marker in the prompt verbatim, so each
//! one keeps its `**Obligation status: OPEN**` line, its `CONTINUE THIS TASK:`
//! directive and its §9 continuation message. The SYSTEM continuation appended
//! after every marker tells the model an OPEN obligation is not optional, so
//! the leftovers compete with the real task for hours. Measured on session
//! `185feba1` (25 Nov 2026): markers at seq 719 (restart) and 743 (delta) both
//! stayed OPEN, and the bot re-asked the same Drive-link question three times
//! between 22:19 and 22:46 MSK after the work named in both notes was done.
//!
//! What is pinned here:
//! 1. A superseded marker loses the status line, the directive, §9 and §8, and
//!    keeps its chronology / manifest / recovery playbook (§7's retention is
//!    deliberate; #1070 tracks its imperative shape separately).
//! 2. The NEWEST marker keeps its own OPEN obligation and §9 — the SYSTEM
//!    continuation appended after it refers to exactly that document.
//! 3. The preamble variant (no `## 0.` heading) is stripped too, which is the
//!    shape the Елино markers actually have.
//! 4. A second delta supersedes the first the same way, so only the newest
//!    marker can still be OPEN.
//! 5. The rows on disk are rewritten in the same step, so a reload cannot
//!    resurrect the retired obligation.
//! 6. A fenced block that quotes the markers survives — the strip is not a
//!    blind text replacement.

use crate::brain::agent::context::AgentContext;
use crate::brain::agent::service::context::retire_superseded_obligation;
use crate::brain::provider::Message;
use crate::services::{MessageService, ServiceContext, SessionService};

/// The restart marker's BODY — the apply supplies the banner itself, so a
/// fixture that carried one too would nest two.
const OPEN_BODY: &str = "\
## 0. IMMEDIATE TASK
**Obligation status: OPEN**
CONTINUE THIS TASK: send the Drive link. You were composing it.

## 1. Chronological Analysis
We read the spreadsheet and drafted the reply.

## 2. Files Modified
None.

## 7. Recovery Playbook
1. Re-read the sheet.

## 8. Next Step
Run:
`gh issue create -R leshchenko1979/opencrabs --title \"x\" --assignee leshchenko1979`
— then post the URL.

## 9. Continuation Message
Waiting on the Drive link. Do not start anything else.

## 10. Context Manifest
```context-manifest
active_skills: []
```
";

/// The delta marker's body, for the same reason as [`OPEN_BODY`].
const DELTA_BODY: &str = "\
## 0. IMMEDIATE TASK
**Obligation status: OPEN**
CONTINUE THIS TASK: wait for the three reviewers.

## 1. Chronological Analysis
Spawned three reviewers.

## 9. Continuation Message
Wait for the reviewers to report.
";

/// What a persisted marker row actually looks like: banner, blank line, body.
fn open_marker() -> String {
    format!(
        "[CONTEXT COMPACTION — The conversation was automatically compacted. \
         Below is a structured summary of everything before this point.]\n\n{OPEN_BODY}"
    )
}

fn delta_marker() -> String {
    format!(
        "[CONTEXT COMPACTION — DELTA SEGMENT. This block summarises only the messages \
         since the previous compaction marker.]\n\n{DELTA_BODY}"
    )
}

fn ctx() -> AgentContext {
    AgentContext::new(uuid::Uuid::nil(), 200_000)
}

/// The preamble shape: status and directive before any heading at all, which is
/// what the live Елино documents look like.
const PREAMBLE_MARKER: &str = "\
[CONTEXT COMPACTION — The conversation was automatically compacted.]

**Obligation status: OPEN**
CONTINUE THIS TASK: deliver the Drive link, then stop.

## 1. Chronological Analysis
Drafted the link.

## 9. Continuation Message
Deliver the link.
";

#[test]
fn superseded_marker_loses_obligation_and_keeps_its_history() {
    let stripped = retire_superseded_obligation(&open_marker()).expect("nothing was stripped");

    let lower = stripped.to_lowercase();
    assert!(
        !lower.contains("obligation status"),
        "the status line survived:\n{stripped}"
    );
    assert!(
        !lower.contains("continue this task"),
        "the directive survived:\n{stripped}"
    );
    assert!(
        !lower.contains("continuation message"),
        "§9 survived:\n{stripped}"
    );
    // #1070: §8 is a command line, not a record — a superseded segment must not
    // keep handing out an invocation.
    assert!(
        !lower.contains("next step"),
        "§8 Next Step survived on a superseded marker:\n{stripped}"
    );
    assert!(
        !stripped.contains("gh issue create"),
        "§8's command line survived:\n{stripped}"
    );
    // The record itself is untouched — this is a retirement, not a deletion.
    // §7 stays deliberately (#1070 keeps it; its imperative SHAPE is a separate
    // prompt-level question, not a strip rule).
    for kept in [
        "## 1. Chronological Analysis",
        "## 2. Files Modified",
        "## 7. Recovery Playbook",
        "Re-read the sheet",
        "## 10. Context Manifest",
        "drafted the reply",
    ] {
        assert!(
            stripped.contains(kept),
            "historical block {kept:?} was lost:\n{stripped}"
        );
    }
}

#[test]
fn preamble_obligation_is_stripped_without_a_section_zero() {
    assert!(
        !PREAMBLE_MARKER.contains("## 0."),
        "fixture must exercise the preamble shape"
    );
    let stripped = retire_superseded_obligation(PREAMBLE_MARKER).expect("nothing was stripped");
    let lower = stripped.to_lowercase();
    assert!(!lower.contains("obligation status"), "{stripped}");
    assert!(!lower.contains("continue this task"), "{stripped}");
    assert!(stripped.contains("Drafted the link"), "{stripped}");
}

#[test]
fn a_section_zero_the_strip_emptied_is_dropped_too() {
    // The status line and the directive WERE §0's content, so a strip that
    // removed them and left the heading would tell a woken model there is an
    // immediate task and then show it none.
    let stripped = retire_superseded_obligation(&open_marker()).expect("nothing was stripped");
    assert!(
        !stripped.contains("IMMEDIATE TASK"),
        "an emptied §0 heading survived:\n{stripped}"
    );
}

#[test]
fn an_emptied_section_zero_loses_its_heading_too() {
    // The status line and the directive WERE §0's content, so a strip that
    // removed them and left the heading would tell a woken model there is an
    // immediate task and then show it none.
    let stripped = retire_superseded_obligation(&open_marker()).expect("nothing was stripped");
    assert!(
        !stripped.contains("IMMEDIATE TASK"),
        "the emptied §0 heading survived:\n{stripped}"
    );
}

#[test]
fn section_eight_dies_even_when_its_body_is_fenced() {
    // §8's payload IS fenced commands, so the cut must take the section whole
    // rather than stop at the first `##` it meets inside the fence.
    let doc = "\
## 1. Chronological Analysis
Drafted the reply.

## 8. Next Step
Run:
```bash
gh issue create -R leshchenko1979/opencrabs --title \"x\"
```

## 10. Context Manifest
```context-manifest
active_skills: []
```
";
    let stripped = retire_superseded_obligation(doc).expect("nothing was stripped");
    assert!(
        !stripped.to_lowercase().contains("next step"),
        "§8 survived:\n{stripped}"
    );
    assert!(
        !stripped.contains("gh issue create"),
        "§8's fenced command survived:\n{stripped}"
    );
    assert!(stripped.contains("Drafted the reply"), "{stripped}");
    assert!(
        stripped.contains("context-manifest"),
        "the manifest was cut with §8:\n{stripped}"
    );
}

#[test]
fn stripping_is_idempotent() {
    let once = retire_superseded_obligation(&open_marker()).expect("first pass stripped nothing");
    assert_eq!(
        retire_superseded_obligation(&once),
        None,
        "a second pass reported a change — the strip is not idempotent"
    );
}

#[test]
fn fenced_sample_quoting_the_markers_survives() {
    let doc = "\
**Obligation status: OPEN**
CONTINUE THIS TASK: do the thing.

## 1. Chronological Analysis
The template says:

```
**Obligation status: OPEN**
CONTINUE THIS TASK: example only
```
";
    let stripped = retire_superseded_obligation(doc).expect("nothing was stripped");
    assert!(
        stripped.contains("example only"),
        "the strip cut into a fenced sample:\n{stripped}"
    );
    assert!(
        !stripped.starts_with("**Obligation status"),
        "the live status line survived:\n{stripped}"
    );
}

#[test]
fn a_second_delta_supersedes_the_first_so_only_the_newest_is_open() {
    let mut c = ctx();
    c.add_message(Message::user("first task"));
    // The restart marker, as the apply would have written it.
    c.compact_with_verbatim_tail(OPEN_BODY.to_string(), 0);
    c.add_message(Message::user("second task"));
    // First delta: the restart above it is now superseded.
    c.compact_with_delta_summary(DELTA_BODY.to_string());
    crate::brain::agent::service::context::retire_superseded_marker_obligations(&mut c);
    c.add_message(Message::user("third task"));
    c.compact_with_delta_summary(DELTA_BODY.to_string());
    crate::brain::agent::service::context::retire_superseded_marker_obligations(&mut c);

    let open_count = c
        .messages
        .iter()
        .filter(|m| {
            m.content_text()
                .to_lowercase()
                .contains("obligation status: open")
        })
        .count();
    assert_eq!(
        open_count, 1,
        "only the newest marker may still be OPEN, found {open_count}"
    );
    // And the one that is still OPEN is the newest MARKER, not merely the last
    // message: the tail the delta kept sits below it.
    let last_open = c
        .messages
        .iter()
        .rposition(|m| {
            m.content_text()
                .to_lowercase()
                .contains("obligation status: open")
        })
        .expect("the newest marker lost its own obligation");
    let newest_marker = c
        .messages
        .iter()
        .rposition(|m| {
            crate::brain::agent::compaction_boundary::classify_user_marker(&m.content_text())
                .is_some()
        })
        .expect("no marker survived");
    assert_eq!(
        last_open, newest_marker,
        "the surviving OPEN obligation is not on the newest marker"
    );
}

async fn service() -> (MessageService, SessionService) {
    use crate::db::Database;

    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let context = ServiceContext::new(db.pool().clone());
    (
        MessageService::new(context.clone()),
        SessionService::new(context),
    )
}

#[tokio::test]
async fn the_retired_obligation_is_written_through_to_the_rows() {
    let (messages, sessions) = service().await;
    let session = sessions
        .create_session(Some("superseded obligation".to_string()))
        .await
        .unwrap();

    messages
        .create_message(session.id, "user".to_string(), open_marker())
        .await
        .unwrap();
    messages
        .create_message(session.id, "user".to_string(), delta_marker())
        .await
        .unwrap();

    let rewritten =
        crate::brain::agent::service::compaction_persist::retire_superseded_marker_rows(
            session.id, &messages,
        )
        .await
        .expect("the write-through failed");
    assert_eq!(
        rewritten, 1,
        "exactly the superseded row should be rewritten"
    );

    // A reload must not resurrect it: this is the whole reason the strip is
    // persisted rather than kept in memory.
    let rows = messages
        .list_messages_for_session(session.id)
        .await
        .unwrap();
    let restart = rows
        .iter()
        .find(|r| r.content.contains("## 2. Files Modified"))
        .expect("the restart row disappeared");
    assert!(
        !restart.content.to_lowercase().contains("obligation status"),
        "a reload would still see the retired obligation:\n{}",
        restart.content
    );
    let newest = rows
        .iter()
        .find(|r| r.content.contains("wait for the three reviewers"))
        .expect("the newest marker disappeared");
    assert!(
        newest
            .content
            .to_lowercase()
            .contains("obligation status: open"),
        "the newest marker lost its own obligation:\n{}",
        newest.content
    );
}

/// `Message` has no `content_text` helper in the crate; this keeps the fixture
/// readable without adding one to production code.
trait ContentText {
    fn content_text(&self) -> String;
}

impl ContentText for Message {
    fn content_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                crate::brain::provider::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}
