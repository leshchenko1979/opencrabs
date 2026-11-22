//! #908 — the ship-plane marker is written by the PRODUCTION reply
//! persistence path, not only by `with_ship_plane` in tests.
//!
//! Feature #91 (cross-turn glue) finds its host with
//! `ChannelMessageRepository::last_bot_rich_md`, which filters
//! `ship_plane = 'rich-md'`. Nothing outside the test suite ever wrote that
//! column, so on a live DB the lookup matched nothing and glue silently took
//! the standalone-bubble path on every turn. These tests drive
//! `delivery::record_bot_reply` — the function both final-answer arms now
//! persist through — and assert the column it lands, plus the lookup the
//! feature actually uses.

use crate::channels::telegram::delivery::{ShipPlane, record_bot_reply};
use crate::db::ChannelMessageRepository;
use crate::db::Database;
use crate::db::models::SHIP_PLANE_RICH_MD;
use teloxide::types::{MessageId, ThreadId};

const CHAT: i64 = -1004428873948;

async fn setup() -> (Database, ChannelMessageRepository) {
    let db = Database::connect_in_memory().await.unwrap();
    db.run_migrations().await.unwrap();
    let repo = ChannelMessageRepository::new(db.pool().clone());
    (db, repo)
}

/// A final answer that shipped on the rich-markdown plane must be persisted
/// WITH the marker, and the glue lookup must then find it. Before #908 the
/// production path built the row through `ChannelMessage::new` alone, so
/// `ship_plane` stayed NULL and this lookup returned `None` on every live DB.
#[tokio::test]
async fn rich_md_reply_is_stamped_and_found_by_the_glue_lookup() {
    let (_db, repo) = setup().await;
    let thread = ThreadId(MessageId(249));
    let body = "| a | b |\n|---|---|\n| 1 | 2 |";

    record_bot_reply(
        &repo,
        CHAT,
        "Kanban",
        "OpenCrabs",
        body,
        Some(4242),
        Some(thread),
        ShipPlane::RichMd,
    )
    .await;

    let rows = repo
        .recent(Some("telegram"), &CHAT.to_string(), 10, None, None)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].ship_plane.as_deref(),
        Some(SHIP_PLANE_RICH_MD),
        "the production persistence path must stamp the rich-markdown plane"
    );
    assert_eq!(rows[0].platform_message_id.as_deref(), Some("4242"));
    assert_eq!(rows[0].thread_id.as_deref(), Some("249"));

    // The lookup cross-turn glue (#91) actually calls must resolve this row.
    let target = repo
        .last_bot_rich_md("telegram", &CHAT.to_string(), Some("249"), 900)
        .await
        .unwrap();
    assert_eq!(
        target,
        Some(("4242".to_string(), body.to_string())),
        "cross-turn glue must see the row this path just wrote"
    );
}

/// The non-forum (no-thread) arm of the same lookup — the SQL arm whose
/// parameters changed with #908 — must resolve a marked row just the same.
#[tokio::test]
async fn rich_md_reply_without_a_thread_is_found_by_the_general_arm() {
    let (_db, repo) = setup().await;

    record_bot_reply(
        &repo,
        CHAT,
        "DM",
        "OpenCrabs",
        "## heading\n\nprose",
        Some(5252),
        None,
        ShipPlane::RichMd,
    )
    .await;

    let target = repo
        .last_bot_rich_md("telegram", &CHAT.to_string(), None, 900)
        .await
        .unwrap();
    assert_eq!(
        target,
        Some(("5252".to_string(), "## heading\n\nprose".to_string())),
        "the general arm must serve marked rows outside any thread"
    );
}

/// The classic plane — parse-mode HTML, or the plain-text fallback
/// `send_html_or_plain` degrades to — and the rich-HTML mermaid fallback are
/// not re-sendable from storage. Their rows must stay unmarked, or glue would
/// re-edit a bubble whose stored body no longer matches what shipped.
#[tokio::test]
async fn classic_reply_is_unmarked_and_never_a_glue_target() {
    let (_db, repo) = setup().await;
    let thread = ThreadId(MessageId(249));

    record_bot_reply(
        &repo,
        CHAT,
        "Kanban",
        "OpenCrabs",
        "plain prose answer",
        Some(4343),
        Some(thread),
        ShipPlane::Classic,
    )
    .await;

    let rows = repo
        .recent(Some("telegram"), &CHAT.to_string(), 10, None, None)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].ship_plane, None,
        "a classic/plain reply is not a re-sendable rich-md body"
    );
    let target = repo
        .last_bot_rich_md("telegram", &CHAT.to_string(), Some("249"), 900)
        .await
        .unwrap();
    assert_eq!(target, None, "classic rows must never be glue targets");
}
