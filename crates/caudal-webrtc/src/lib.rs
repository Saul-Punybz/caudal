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
//!
//! The server is ICE-lite with host candidates only (no STUN/TURN): it must
//! be reachable on `udp_bind` (or `public_ips` with a 1:1 NAT). Codecs:
//! H.264 (packetization-mode 1) and Opus. Built on str0m (sans-I/O), with
//! its pure-Rust crypto backend.

mod codec;
mod egress;
mod engine;
mod ingest;
mod net;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use caudal_core::{Access, Denied, PublishError, Registry, StartAt};
use str0m::change::SdpOffer;
use str0m::{Candidate, Rtc, RtcConfig};
use tokio::sync::{mpsc, oneshot};

use crate::engine::{Cmd, Peer, Role};

#[derive(Debug, Clone)]
pub struct WebRtcConfig {
    /// UDP address every WebRTC peer talks to.
    pub udp_bind: SocketAddr,
    /// Addresses to advertise as ICE host candidates (public IP behind NAT).
    /// Empty: the local addresses of `udp_bind`.
    pub public_ips: Vec<IpAddr>,
    pub buffer: caudal_core::BufferConfig,
}

#[derive(Clone)]
struct AppState {
    registry: Arc<Registry>,
    /// `None` when the UDP socket could not be bound: every request is 503.
    cmds: Option<mpsc::Sender<Cmd>>,
    candidates: Arc<Vec<SocketAddr>>,
    buffer: caudal_core::BufferConfig,
}

/// Must be called inside a tokio runtime (it binds the UDP socket and spawns
/// the peer loop).
pub fn router(registry: Arc<Registry>, cfg: WebRtcConfig) -> axum::Router {
    let (cmds, candidates) = match start(&cfg) {
        Ok((tx, c)) => (Some(tx), c),
        Err(e) => {
            tracing::error!(error = %e, bind = %cfg.udp_bind, "webrtc: cannot start; WHIP/WHEP disabled");
            (None, Vec::new())
        }
    };
    let state = AppState { registry, cmds, candidates: Arc::new(candidates), buffer: cfg.buffer };
    axum::Router::new()
        .route("/whip/{name}", post(whip_post).options(preflight))
        .route("/whip/{name}/{session}", axum::routing::delete(whip_delete).options(preflight))
        .route("/whep/{name}", post(whep_post).options(preflight))
        .route("/whep/{name}/{session}", axum::routing::delete(whep_delete).options(preflight))
        .layer(axum::middleware::map_response(cors))
        .with_state(state)
}

fn start(cfg: &WebRtcConfig) -> std::io::Result<(mpsc::Sender<Cmd>, Vec<SocketAddr>)> {
    let std_sock = std::net::UdpSocket::bind(cfg.udp_bind)?;
    std_sock.set_nonblocking(true)?;
    let sock = tokio::net::UdpSocket::from_std(std_sock)?;
    let local = sock.local_addr()?;
    let candidates = net::candidate_addrs(local, &cfg.public_ips);
    if candidates.is_empty() {
        return Err(std::io::Error::other("no address to advertise as an ICE candidate; set [webrtc] public_ips"));
    }
    tracing::info!(bind = %local, candidates = ?candidates, "webrtc: listening");
    let dest = net::Destinations::new(local, candidates.clone());
    let (tx, rx) = mpsc::channel(256);
    tokio::spawn(engine::run(sock, dest, rx));
    Ok((tx, candidates))
}

/// Fuzz-only entry points into the SDP offer/answer parsing internals of
/// this module, which are otherwise private. Not part of the public API;
/// used by `fuzz/fuzz_targets/webrtc_sdp.rs`. Must never panic on any
/// input; `engine.rs` (owned by other work) is never touched or called
/// from here.
#[doc(hidden)]
pub mod fuzz {
    use std::sync::Arc;

    /// Exercises SDP offer parsing and ICE-lite negotiation exactly as
    /// `whip_post`/`whep_post` do (str0m's offer parse and answer, then our
    /// own `answer_codecs` scan), with no registry lookups or network I/O.
    pub fn negotiate_offer(body: &[u8]) {
        let st = crate::AppState {
            registry: caudal_core::Registry::new(),
            cmds: None,
            candidates: Arc::new(vec!["127.0.0.1:9".parse().unwrap()]),
            buffer: caudal_core::BufferConfig::default(),
        };
        let _ = crate::negotiate(&st, body);
    }

    /// Exercises just the answer-codec line scan on arbitrary SDP-shaped
    /// text (the `str0m`-produced answer is not the only input this ever
    /// sees in practice; WHEP/WHIP offers can also reach it indirectly).
    pub fn answer_codecs(sdp: &str) {
        let _ = crate::answer_codecs(sdp);
    }
}

// ---- HTTP ----

fn plain(code: StatusCode, msg: &str) -> Response {
    (code, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], format!("{msg}\n")).into_response()
}

async fn cors(mut res: Response) -> Response {
    let h = res.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    h.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, HeaderValue::from_static("Location, ETag, Link, Accept-Post"));
    res
}

async fn preflight() -> Response {
    (
        StatusCode::NO_CONTENT,
        [
            (header::ACCESS_CONTROL_ALLOW_METHODS, "POST, DELETE, OPTIONS"),
            (header::ACCESS_CONTROL_ALLOW_HEADERS, "Authorization, Content-Type, If-Match"),
            (header::ACCESS_CONTROL_MAX_AGE, "86400"),
            (header::HeaderName::from_static("accept-post"), "application/sdp"),
        ],
    )
        .into_response()
}

/// `Authorization: Bearer <t>` first, then `?token=<t>`.
fn token(headers: &HeaderMap, uri: &Uri) -> Option<String> {
    if let Some(v) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        if v.len() > 7 && v[..7].eq_ignore_ascii_case("bearer ") {
            let t = v[7..].trim();
            if !t.is_empty() {
                return Some(t.to_owned());
            }
        }
    }
    uri.query()?.split('&').find_map(|kv| kv.strip_prefix("token=")).map(percent_decode).filter(|t| !t.is_empty())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h << 4 | l);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

fn denied(d: Denied) -> Response {
    match d {
        Denied::Missing => {
            let mut r = plain(StatusCode::UNAUTHORIZED, "token required");
            r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            r
        }
        Denied::Refused(why) => {
            tracing::info!(reason = %why, "webrtc: access refused");
            plain(StatusCode::FORBIDDEN, "forbidden")
        }
    }
}

fn is_sdp(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().to_ascii_lowercase().starts_with("application/sdp"))
}

fn build_rtc(candidates: &[SocketAddr]) -> Rtc {
    let mut cfg = RtcConfig::new().set_ice_lite(true).clear_codecs().enable_opus(true);
    // H.264 packetization-mode 1 only, the profiles browsers and ffmpeg offer.
    for (pt, rtx, profile_level_id) in
        [(127u8, 121u8, 0x42001f), (108, 109, 0x42e01f), (123, 119, 0x4d001f), (114, 115, 0x64001f)]
    {
        cfg.codec_config().add_h264(pt.into(), Some(rtx.into()), true, profile_level_id);
    }
    let mut rtc = cfg.build(std::time::Instant::now());
    for c in candidates {
        match Candidate::host(*c, "udp") {
            Ok(c) => {
                rtc.add_local_candidate(c);
            }
            Err(e) => tracing::warn!(addr = %c, error = %e, "webrtc: bad host candidate"),
        }
    }
    rtc
}

/// Parses the offer and answers it. Errors are HTTP responses.
fn negotiate(st: &AppState, body: &[u8]) -> Result<(Rtc, String), (StatusCode, String)> {
    let offer = std::str::from_utf8(body)
        .ok()
        .and_then(|s| SdpOffer::from_sdp_string(s).ok())
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "malformed SDP offer".to_owned()))?;
    let mut rtc = build_rtc(&st.candidates);
    let answer = rtc.sdp_api().accept_offer(offer).map_err(|e| {
        tracing::debug!(error = %e, "webrtc: offer rejected");
        (StatusCode::BAD_REQUEST, format!("SDP offer not accepted: {e}"))
    })?;
    let sdp = answer.to_sdp_string();
    let (h264, opus) = answer_codecs(&sdp);
    if !h264 && !opus {
        return Err((StatusCode::NOT_ACCEPTABLE, "offer has neither H.264 (packetization-mode=1) nor Opus".to_owned()));
    }
    Ok((rtc, sdp))
}

/// Whether the answer has an active m-line with H.264 and/or Opus.
fn answer_codecs(sdp: &str) -> (bool, bool) {
    let (mut h264, mut opus, mut active) = (false, false, false);
    for line in sdp.lines() {
        if let Some(m) = line.strip_prefix("m=") {
            active = m.split_whitespace().nth(1).is_some_and(|port| port != "0");
        } else if let Some(map) = line.strip_prefix("a=rtpmap:").filter(|_| active) {
            let enc = map.split_whitespace().nth(1).unwrap_or("").to_ascii_lowercase();
            h264 |= enc.starts_with("h264/");
            opus |= enc.starts_with("opus/");
        }
    }
    (h264, opus)
}

/// Whether a WHEP answer with these codecs can carry any of the stream's
/// tracks. Unknown tracks (the publisher has not announced them yet) pass.
fn playable(tracks: &[caudal_core::TrackInfo], h264: bool, opus: bool) -> bool {
    tracks.is_empty()
        || tracks
            .iter()
            .any(|t| (h264 && t.codec == caudal_core::Codec::H264) || (opus && t.codec == caudal_core::Codec::Opus))
}

fn session_id() -> String {
    let mut b = [0u8; 16];
    if getrandom::fill(&mut b).is_err() {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        b = t.to_le_bytes();
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn created(location: String, sdp: String) -> Response {
    let mut r = (StatusCode::CREATED, sdp).into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/sdp"));
    if let Ok(v) = HeaderValue::from_str(&location) {
        h.insert(header::LOCATION, v);
    }
    r
}

async fn whip_post(
    State(st): State<AppState>,
    Path(name): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(cmds) = st.cmds.clone() else { return plain(StatusCode::SERVICE_UNAVAILABLE, "WebRTC is not running") };
    if !is_sdp(&headers) {
        return plain(StatusCode::UNSUPPORTED_MEDIA_TYPE, "expected Content-Type: application/sdp");
    }
    let tok = token(&headers, &uri);
    if let Err(d) = st.registry.authorize(Access::Publish, &name, tok.as_deref()).await {
        return denied(d);
    }
    let (rtc, sdp) = match negotiate(&st, &body) {
        Ok(v) => v,
        Err((code, msg)) => return plain(code, &msg),
    };
    let publisher = match st.registry.publish(&name, st.buffer) {
        Ok(p) => p,
        Err(PublishError::Busy(_)) => return plain(StatusCode::CONFLICT, "stream already has a publisher"),
        Err(PublishError::InvalidName) => return plain(StatusCode::BAD_REQUEST, "invalid stream name"),
    };
    let session = session_id();
    let peer = Peer {
        session: session.clone(),
        name: name.as_str().into(),
        rtc,
        role: Role::Whip(Box::new(ingest::Ingest::new(publisher))),
    };
    if cmds.send(Cmd::Add(Box::new(peer))).await.is_err() {
        return plain(StatusCode::SERVICE_UNAVAILABLE, "WebRTC is not running");
    }
    created(format!("/whip/{name}/{session}"), sdp)
}

async fn whep_post(
    State(st): State<AppState>,
    Path(name): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(cmds) = st.cmds.clone() else { return plain(StatusCode::SERVICE_UNAVAILABLE, "WebRTC is not running") };
    if !is_sdp(&headers) {
        return plain(StatusCode::UNSUPPORTED_MEDIA_TYPE, "expected Content-Type: application/sdp");
    }
    let tok = token(&headers, &uri);
    if let Err(d) = st.registry.authorize(Access::Play, &name, tok.as_deref()).await {
        return denied(d);
    }
    let Some(sub) = st.registry.subscribe(&name, StartAt::LiveEdge) else {
        return plain(StatusCode::NOT_FOUND, "no such stream");
    };
    let (rtc, sdp) = match negotiate(&st, &body) {
        Ok(v) => v,
        Err((code, msg)) => return plain(code, &msg),
    };
    // A session that can carry none of the stream's tracks would connect
    // and stay black (e.g. a browser without H.264 for WebRTC, like
    // Playwright's Firefox on Linux, playing an H.264 + AAC stream).
    let (h264, opus) = answer_codecs(&sdp);
    if !playable(&sub.tracks(), h264, opus) {
        return plain(
            StatusCode::NOT_ACCEPTABLE,
            "nothing in this stream can be sent to this browser: its video is H.264 and the offer has no H.264 (packetization-mode=1); its audio is not Opus",
        );
    }
    let session = session_id();
    let name: Arc<str> = name.as_str().into();
    let peer = Peer {
        session: session.clone(),
        name: name.clone(),
        rtc,
        role: Role::Whep(Box::new(egress::Egress::new(name.clone(), sub))),
    };
    if cmds.send(Cmd::Add(Box::new(peer))).await.is_err() {
        return plain(StatusCode::SERVICE_UNAVAILABLE, "WebRTC is not running");
    }
    created(format!("/whep/{name}/{session}"), sdp)
}

async fn delete_session(st: AppState, whip: bool, name: String, session: String) -> Response {
    let Some(cmds) = st.cmds else { return plain(StatusCode::SERVICE_UNAVAILABLE, "WebRTC is not running") };
    let (reply, rx) = oneshot::channel();
    if cmds.send(Cmd::Delete { whip, name, session, reply }).await.is_err() {
        return plain(StatusCode::SERVICE_UNAVAILABLE, "WebRTC is not running");
    }
    match rx.await {
        // A body: ffmpeg's WHIP muxer reports an empty DELETE response as an error.
        Ok(true) => plain(StatusCode::OK, "session ended"),
        _ => plain(StatusCode::NOT_FOUND, "no such session"),
    }
}

async fn whip_delete(State(st): State<AppState>, Path((name, session)): Path<(String, String)>) -> Response {
    delete_session(st, true, name, session).await
}

async fn whep_delete(State(st): State<AppState>, Path((name, session)): Path<(String, String)>) -> Response {
    delete_session(st, false, name, session).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_from_header_or_query() {
        let mut h = HeaderMap::new();
        let uri: Uri = "/whip/a?x=1&token=q%2Bt".parse().unwrap();
        assert_eq!(token(&h, &uri).as_deref(), Some("q+t"));
        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer abc"));
        assert_eq!(token(&h, &uri).as_deref(), Some("abc"));
        assert_eq!(token(&HeaderMap::new(), &"/whip/a".parse().unwrap()), None);
        assert_eq!(percent_decode("%zz%4"), "%zz%4");
    }

    #[test]
    fn playable_needs_a_shared_codec() {
        use caudal_core::{Codec, TrackId, TrackInfo};
        let t = |codec| TrackInfo {
            id: TrackId(0),
            codec,
            timescale: 90_000,
            init: Default::default(),
            lang: None,
            video: None,
            audio: None,
        };
        let h264_aac = [t(Codec::H264), t(Codec::Aac)];
        assert!(playable(&h264_aac, true, true));
        assert!(!playable(&h264_aac, false, true), "Opus-only offer, H.264 + AAC stream: nothing to send");
        assert!(playable(&[t(Codec::H264), t(Codec::Opus)], false, true), "audio alone still plays");
        assert!(playable(&[], false, true), "tracks not announced yet");
    }

    #[test]
    fn codecs_in_answer() {
        let sdp = "v=0\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=rtpmap:111 opus/48000/2\r\nm=video 0 UDP/TLS/RTP/SAVPF 96\r\na=rtpmap:96 H264/90000\r\n";
        assert_eq!(answer_codecs(sdp), (false, true));
    }
}
