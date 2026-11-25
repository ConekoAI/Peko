//! Audit of links (ADR-125 D9): `link.up`, `link.down`, `link.refused`,
//! as structured log lines in the shape of `tunnel_channel_audit`, and
//! the path each frame took, which the tunnel audit's forwarded and
//! received events carry.

use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};

use tracing::info;

use super::Path;

/// How many received frames' paths are remembered for the audit.
const REMEMBER: usize = 4096;

#[derive(Default)]
struct Paths {
    /// The path frames to each computer take now.
    to: HashMap<String, Path>,
    /// The path each recently received frame came by, by `requestId`.
    received: HashMap<String, Path>,
    order: VecDeque<String>,
}

static PATHS: LazyLock<Mutex<Paths>> = LazyLock::new(Mutex::default);

/// Frames to `runtime` now take `path`.
pub fn set_path(runtime: &str, path: Path) {
    PATHS.lock().unwrap().to.insert(runtime.to_string(), path);
}

/// The path frames to `runtime` take now (`relay` when no router said).
#[must_use]
pub fn path_to(runtime: &str) -> &'static str {
    PATHS
        .lock()
        .unwrap()
        .to
        .get(runtime)
        .copied()
        .unwrap_or(Path::Relay)
        .as_str()
}

/// The frame `request_id` was received by `path`.
pub fn received_by(request_id: &str, path: Path) {
    let mut paths = PATHS.lock().unwrap();
    if paths
        .received
        .insert(request_id.to_string(), path)
        .is_none()
    {
        paths.order.push_back(request_id.to_string());
    }
    while paths.order.len() > REMEMBER {
        if let Some(old) = paths.order.pop_front() {
            paths.received.remove(&old);
        }
    }
}

/// How the frame `request_id` was received (`relay` when not known).
#[must_use]
pub fn received_path(request_id: &str) -> &'static str {
    PATHS
        .lock()
        .unwrap()
        .received
        .get(request_id)
        .copied()
        .unwrap_or(Path::Relay)
        .as_str()
}

/// A channel event or invite was sent to `recipient` by `path`
/// (ADR-125 D9: the path of each copy a forwarded event became).
pub fn routed(ty: &str, request_id: Option<&str>, recipient: &str, path: Path) {
    info!(
        event_kind = "tunnel_channel.routed",
        frame_type = %ty,
        request_id = %request_id.unwrap_or(""),
        recipient_runtime_id = %recipient,
        path = %path.as_str(),
        "channel envelope routed"
    );
}

/// A link came up.
pub fn link_up(runtime: &str, address: &str, dialed: bool) {
    info!(
        event_kind = "link.up",
        runtime_id = %runtime,
        address = %address,
        direction = if dialed { "dialed" } else { "accepted" },
        "direct link up"
    );
}

/// A link went down.
pub fn link_down(runtime: &str, address: &str, reason: &str) {
    info!(
        event_kind = "link.down",
        runtime_id = %runtime,
        address = %address,
        reason = %reason,
        "direct link down"
    );
}

/// A link was refused (its hello, or a frame on it).
pub fn link_refused(runtime: Option<&str>, address: &str, reason: &str) {
    info!(
        event_kind = "link.refused",
        runtime_id = %runtime.unwrap_or(""),
        address = %address,
        reason = %reason,
        "direct link refused"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_remembered_per_computer_and_per_frame() {
        set_path("did:key:zAuditA", Path::Direct);
        assert_eq!(path_to("did:key:zAuditA"), "direct");
        assert_eq!(path_to("did:key:zAuditUnknown"), "relay");
        received_by("audit-req-1", Path::Direct);
        assert_eq!(received_path("audit-req-1"), "direct");
        assert_eq!(received_path("audit-req-unknown"), "relay");
        link_up("did:key:zAuditA", "10.0.0.2:11438", true);
        link_down("did:key:zAuditA", "10.0.0.2:11438", "closed");
        link_refused(None, "10.0.0.3:11438", "wrong_binding");
    }
}
