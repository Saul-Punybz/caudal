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
pub(crate) async fn handle(
    socket: SrtSocket,
    peer: SocketAddrV4,
    registry: Arc<Registry>,
    name: &str,
    token: Option<&str>,
) {
    let name = match registry.resolve(Access::Play, "srt", name, token, Some(std::net::IpAddr::V4(*peer.ip()))).await {
        Ok(n) => n,
        Err(denied) => {
            tracing::info!(%peer, stream = %name, reason = ?denied, "srt play rejected");
            return;
        }
    };
    let name = name.as_str();
    // `get_or_demand`: on a cluster edge, the first viewer starts the pull.
    let Some(stream) = registry.get_or_demand(name).await else {
        tracing::debug!(%peer, stream = %name, "srt play rejected: stream not found");
        return;
    };

    // A real viewer: counted in the stream's viewer stats, same as any
    // other output's audience.
    let mut sub = stream.subscribe(StartAt::LiveEdge);
    let mut session = registry.open_session(name, "srt", Some(std::net::IpAddr::V4(*peer.ip())));
    let quality = session.quality();
    let mut stats_tick = tokio::time::interval(STATS_EVERY);
    stats_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut tracks = sub.tracks();
    let mut mux = TsMux::new();
    mux.set_tracks(&tracks);
    tracing::info!(%peer, stream = %name, "srt play started");

    loop {
        let ev = tokio::select! {
            ev = sub.recv() => ev,
            () = session.kicked() => {
                tracing::info!(%peer, stream = %name, "srt play kicked");
                break;
            }
            _ = stats_tick.tick() => {
                report_quality(&socket, &quality);
                continue;
            }
        };
        match ev {
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
            Event::Cue(cue) => mux.push_cue(&cue),
            Event::End => break,
        }
        let out = mux.take_output();
        if out.is_empty() {
            continue;
        }
        if !send_chunks(&socket, &out).await {
            break;
        }
        session.add_bytes(out.len());
    }
    tracing::debug!(%peer, stream = %name, "srt play ended");
}

/// How often a viewer's RTT and loss are read from the socket.
const STATS_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// RTT from SRT's own ACK/ACKACK measurement; "loss" as the share of data
/// packets that had to be retransmitted (or were dropped too late), which
/// is what the viewer's network cost us. Nothing before the first packet.
fn report_quality(socket: &SrtSocket, quality: &caudal_core::Quality) {
    let s = socket.stats();
    if s.pkts_sent == 0 {
        return;
    }
    if s.rtt_us > 0 {
        quality.set_rtt(std::time::Duration::from_micros(u64::from(s.rtt_us)));
    }
    let bad = s.pkts_retransmitted + s.pkts_send_dropped;
    quality.set_loss(bad as f64 / s.pkts_sent as f64);
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
