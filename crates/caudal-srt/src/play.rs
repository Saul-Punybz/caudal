//! Serves a stream as MPEG-TS to an SRT viewer: `play/<name>` (optionally
//! `?token=`) or `#!::r=<name>,m=request` (with `t=<token>`), parsed by
//! `crate::connection::parse_play`. See `NOTES.md` for the TS mux design.

use std::net::SocketAddrV4;
use std::sync::Arc;

use caudal_core::{Access, Event, Registry, StartAt};
use rsrt::SrtSocket;

use crate::mux::TsMux;

/// rsrt's live-mode chunk size: 7 TS packets (188 bytes each).
const CHUNK_BYTES: usize = 7 * 188;

/// Serves `name` as MPEG-TS on `socket` until the stream ends, the viewer
/// disconnects, or authorization is refused. Never panics: any failure ends
/// this connection only, never the listener.
pub(crate) async fn handle(socket: SrtSocket, peer: SocketAddrV4, registry: Arc<Registry>, name: &str, token: Option<&str>) {
    if let Err(denied) = registry.authorize(Access::Play, name, token).await {
        tracing::info!(%peer, stream = %name, reason = ?denied, "srt play rejected");
        return;
    }
    let Some(stream) = registry.get(name) else {
        tracing::debug!(%peer, stream = %name, "srt play rejected: stream not found");
        return;
    };

    // A real viewer: counted in the stream's viewer stats, same as any
    // other output's audience.
    let mut sub = stream.subscribe(StartAt::LiveEdge);
    let mut tracks = sub.tracks();
    let mut mux = TsMux::new();
    mux.set_tracks(&tracks);
    tracing::info!(%peer, stream = %name, "srt play started");

    loop {
        match sub.recv().await {
            Event::TracksChanged => {
                tracks = sub.tracks();
                mux.set_tracks(&tracks);
            }
            Event::Frame(frame) => {
                if let Some(info) = tracks.iter().find(|t| t.id == frame.track) {
                    mux.push_frame(info, &frame);
                }
            }
            Event::Lagged { skipped } => {
                // The muxer re-injects SPS/PPS/VPS on the next keyframe
                // regardless of what preceded it, so resuming here needs no
                // extra state: the next video frame this subscriber sees is
                // itself the resync point.
                tracing::debug!(%peer, stream = %name, skipped, "srt play lagged; resuming at next keyframe");
            }
            Event::End => break,
        }
        let out = mux.take_output();
        if out.is_empty() {
            continue;
        }
        if !send_chunks(&socket, &out).await {
            break;
        }
    }
    tracing::debug!(%peer, stream = %name, "srt play ended");
}

/// Sends `data` (whole TS packets) in [`CHUNK_BYTES`]-sized pieces. Returns
/// `false` on a send error, which ends the connection.
async fn send_chunks(socket: &SrtSocket, data: &[u8]) -> bool {
    for chunk in data.chunks(CHUNK_BYTES) {
        if let Err(err) = socket.send(chunk).await {
            tracing::debug!(%err, "srt play send failed");
            return false;
        }
    }
    true
}
