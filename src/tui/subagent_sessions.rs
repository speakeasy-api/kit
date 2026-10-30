//! Native ACP session routing. Association ownership is event-loop local.
//! The callback clones the existing route under its mutex and never holds a
//! guard across send or await.
use super::{SessionUpdate, Update, UpdateSessionNotification};

pub(super) fn translate(notification: UpdateSessionNotification, root: &str) -> Vec<Update> {
    if notification.session_id.to_string() != root
        || matches!(
            notification.update,
            SessionUpdate::SubagentUpdate(_)
                | SessionUpdate::SessionMessage(_)
                | SessionUpdate::SessionMessageChunk(_)
        )
    {
        return vec![Update::SubagentSession {
            root: root.to_owned(),
            notification: Box::new(notification),
        }];
    }
    super::translate_for_session(notification, root)
}
