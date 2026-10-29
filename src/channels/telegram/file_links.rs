//! Deep links for files delivered into a DM (#802).
//!
//! A bot cannot address a DM message by link. The Bot API and the client count
//! messages in two different id spaces, so the id a bot receives names nothing
//! in the space `t.me/<bot>/<id>` resolves in: measured 2026-10-29 in the DM
//! `oc_l1979_bot`, `notes.txt` came back as Bot API id `60451` while the same
//! bubble is client id `975269`, and `messages.getHistory` over an id window
//! 60000..=60452 in that chat returns zero messages. `1eb7a355c` shipped the
//! message-link form anyway and every DM link it built was dead; it was
//! withdrawn in `72f21d1c4`.
//!
//! A deep link sidesteps that entirely: `t.me/<bot>?start=<payload>` is
//! addressed by the bot's own username, so no message id is involved. The
//! payload names a token, the token names a path, and `/start file_<token>`
//! re-sends the file — the reader taps the marker in the body and gets the
//! document the marker promised.
//!
//! The map is a small JSON file beside the session store, deliberately not a
//! table: its entries are per-delivery and disposable, and losing the file
//! costs one re-send request, never a delivery. An unwritable map is reported
//! as `None`, so the caller keeps the plain marker instead of splicing a link
//! that leads nowhere.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The token map, beside the session store.
const MAP_FILE: &str = "telegram_file_links.json";

/// Hex chars kept from the token uuid: 48 bits, with collisions checked rather
/// than assumed away.
const TOKEN_LEN: usize = 12;

/// `/start` payload prefix. A payload carrying it is a file request, never a
/// cowork group join (`/start cowork_<id>`).
pub(crate) const PAYLOAD_PREFIX: &str = "file_";

/// Where the token map lives.
fn map_path() -> PathBuf {
    crate::config::opencrabs_home().join(MAP_FILE)
}

/// Read the token map.
///
/// A missing or corrupt file is an empty map: nothing a delivery depends on
/// lives here, and refusing every link because one byte went bad would trade a
/// working marker for a parse error.
fn read_map() -> HashMap<String, String> {
    let raw = match std::fs::read_to_string(map_path()) {
        Ok(raw) => raw,
        Err(_) => return HashMap::new(),
    };
    serde_json::from_str(&raw).unwrap_or_else(|e| {
        tracing::warn!("Telegram: unreadable file-link map, starting empty: {e}");
        HashMap::new()
    })
}

fn write_map(map: &HashMap<String, String>) -> bool {
    let path = map_path();
    let body = match serde_json::to_string(map) {
        Ok(body) => body,
        Err(e) => {
            tracing::warn!("Telegram: cannot serialise the file-link map: {e}");
            return false;
        }
    };
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!("Telegram: cannot create {parent:?} for the file-link map: {e}");
        return false;
    }
    match std::fs::write(&path, body) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("Telegram: cannot write the file-link map: {e}");
            false
        }
    }
}

/// Register `path` and return its token.
///
/// Re-registering a path reuses the token it already has, so a file delivered
/// twice does not grow the map by an entry per delivery. `None` means the map
/// could not be written; the caller then keeps the plain marker.
pub(crate) fn register(path: &Path) -> Option<String> {
    let key = path.to_string_lossy().to_string();
    let mut map = read_map();
    if let Some((token, _)) = map.iter().find(|(_, stored)| **stored == key) {
        return Some(token.clone());
    }
    let mut token = String::new();
    for _ in 0..4 {
        let candidate: String = uuid::Uuid::new_v4()
            .simple()
            .to_string()
            .chars()
            .take(TOKEN_LEN)
            .collect();
        if !map.contains_key(&candidate) {
            token = candidate;
            break;
        }
    }
    if token.is_empty() {
        return None;
    }
    map.insert(token.clone(), key);
    write_map(&map).then_some(token)
}

/// The path a token names, when the file is still on disk.
///
/// A removed file leaves the token pointing at nothing; saying so here keeps
/// the caller from handing a reader a path it cannot read.
pub(crate) fn resolve(token: &str) -> Option<PathBuf> {
    let map = read_map();
    let stored = map.get(token)?;
    let path = PathBuf::from(stored);
    path.exists().then_some(path)
}

/// The `t.me` deep link that asks the bot to re-send `token`'s file.
pub(crate) fn deep_link(bot_username: &str, token: &str) -> String {
    format!("https://t.me/{bot_username}?start={PAYLOAD_PREFIX}{token}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `f` against a throwaway OpenCrabs home.
    ///
    /// The token map lives under `opencrabs_home()`, so without this a test would
    /// write `telegram_file_links.json` into the LIVE profile home — the same
    /// class of leak that once had a test rewriting the user's real config
    /// (#1399). The override is a task-local, so nothing outside the closure
    /// sees the temp path.
    fn in_temp_home(f: impl FnOnce()) {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join(".opencrabs");
        std::fs::create_dir_all(&home).expect("create .opencrabs");
        crate::config::profile::with_home_override(home, f);
    }

    /// A token is the map's own key, and only a path already on disk resolves.
    #[test]
    fn a_registered_path_resolves_and_a_missing_one_does_not() {
        in_temp_home(|| {
            let dir = tempfile::tempdir().expect("tempdir");
            let file = dir.path().join("notes.txt");
            std::fs::write(&file, b"hi").expect("write fixture");

            let token = register(&file).expect("token");
            assert_eq!(token.len(), TOKEN_LEN);
            assert_eq!(resolve(&token).as_deref(), Some(file.as_path()));

            // Same path, same token: a second delivery reuses the entry rather
            // than growing the map by one row per send.
            assert_eq!(register(&file).as_deref(), Some(token.as_str()));

            std::fs::remove_file(&file).expect("remove fixture");
            assert!(
                resolve(&token).is_none(),
                "a token whose file is gone must not resolve"
            );
        });
    }

    /// An unknown token resolves to nothing: the map is the only authority.
    #[test]
    fn an_unknown_token_resolves_to_nothing() {
        in_temp_home(|| {
            assert!(resolve("0123456789ab").is_none());
        });
    }

    /// The payload the link carries is the prefix the `/start` branch reads.
    #[test]
    fn the_deep_link_carries_the_payload_prefix() {
        let link = deep_link("oc_l1979_bot", "0123456789ab");
        assert_eq!(link, "https://t.me/oc_l1979_bot?start=file_0123456789ab");
        assert_eq!(
            link.split("?start=")
                .nth(1)
                .and_then(|p| p.strip_prefix(PAYLOAD_PREFIX)),
            Some("0123456789ab")
        );
    }
}
