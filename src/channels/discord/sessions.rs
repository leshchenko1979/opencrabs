//! Session to channel-id map so approvals and resumed background results
//! route back to the channel the session was last handled in.

use uuid::Uuid;

use super::DiscordState;

impl DiscordState {
    /// Record which channel_id corresponds to a given session.
    pub async fn register_session_channel(&self, session_id: Uuid, channel_id: u64) {
        self.session_channels
            .lock()
            .await
            .insert(session_id, channel_id);
    }

    /// Look up the channel_id for a session.
    pub async fn session_channel(&self, session_id: Uuid) -> Option<u64> {
        self.session_channels.lock().await.get(&session_id).copied()
    }
}

/// Reverse lookup (#148): the session currently bound to a Discord channel
/// id, for `oc://discord/<id>` resolution. Last writer wins — same semantics
/// as the forward map.
pub async fn session_owner_by_channel(&self, channel_id: u64) -> Option<Uuid> {
    self.session_channels
        .lock()
        .await
        .iter()
        .find(|(_, ch)| **ch == channel_id)
        .map(|(s, _)| *s)
}
