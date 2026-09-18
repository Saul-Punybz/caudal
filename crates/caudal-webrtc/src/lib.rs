//! WebRTC in and out: WHIP (RFC 9725) to publish, WHEP to play. One UDP
//! socket for all peers. Entry point fixed by the orchestrator.
//!
//! Routes (absolute; merged at the root by the server):
//! - `POST /whip/{name}` (SDP offer in, 201 + SDP answer + `Location` out)
//! - `DELETE /whip/{name}/{session}`
//! - `POST /whep/{name}`, `DELETE /whep/{name}/{session}`
//!
//! Tokens: `Authorization: Bearer <token>` (as the WHIP/WHEP specs say) or
//! `?token=`, checked with `Registry::authorize`.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use caudal_core::Registry;

#[derive(Debug, Clone)]
pub struct WebRtcConfig {
    /// UDP address every WebRTC peer talks to.
    pub udp_bind: SocketAddr,
    /// Addresses to advertise as ICE host candidates (public IP behind NAT).
    /// Empty: the local addresses of `udp_bind`.
    pub public_ips: Vec<IpAddr>,
    pub buffer: caudal_core::BufferConfig,
}

/// Must be called inside a tokio runtime (it binds the UDP socket and spawns
/// the peer loop).
pub fn router(registry: Arc<Registry>, cfg: WebRtcConfig) -> axum::Router {
    let _ = (registry, cfg);
    axum::Router::new()
}
