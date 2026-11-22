//! Progressive MP4: a live stream as one endless fragmented-MP4 HTTP
//! response at `/mp4/{name}.mp4`, the way MistServer serves
//! `/{name}.mp4`. A plain `<video src>`, VLC, ffmpeg or a download opens it
//! without an HLS or DASH player.
//!
//! The body is `ftyp` + `moov` once, then `moof` + `mdat` fragments (one
//! per GOP; see [`mux`]) until the stream ends, the client goes away, or an
//! operator kicks the session. Tracks: the first H.264/H.265 video and the
//! first AAC/Opus audio, as `caudal_cmaf` writes them. `?video=0` or
//! `?audio=0` leaves a track out. A stream whose video (or audio) is a
//! codec fMP4 here does not carry (MPEG-2 video, MP2, AC-3, ...) gets `415`
//! naming `/ts/{name}.ts`, which carries everything, and the `?video=0` /
//! `?audio=0` that would serve the rest.
//!
//! A slow client never grows memory without bound: at most [`QUEUE`]
//! fragments wait for the socket, and while they wait this task stops
//! reading the stream; the stream's own buffer then moves the subscriber
//! forward to the next keyframe (`Event::Lagged`), which leaves a gap in
//! the timeline (decode times jump forward, never back). A client that
//! reads nothing for [`STALL`] is closed.

pub mod mux;

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Path, RawQuery, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use caudal_cmaf::fmp4::Mp4Track;
use caudal_core::media::valid_stream_name;
use caudal_core::{Access, Cidr, Denied, Event, Registry, Session, StartAt, Subscriber, TrackInfo, TrackKind};
use tokio::sync::mpsc;

use crate::mux::Mux;

/// Fragments (about one GOP each) that may wait for a slow client.
pub const QUEUE: usize = 8;
/// A client that takes nothing for this long is disconnected.
pub const STALL: Duration = Duration::from_secs(15);
/// The protocol name this output's viewers show in `/api/v1/sessions`.
pub const PROTOCOL: &str = "mp4";
/// How long a request for a stream that has not announced its tracks yet
/// waits for them.
const TRACK_WAIT: Duration = Duration::from_secs(5);

struct Shared {
    registry: Arc<Registry>,
    trusted_proxies: Vec<Cidr>,
}

/// Serves `/mp4/{name}.mp4`. Mounted at the root by the server.
///
/// `trusted_proxies` is `[server] trusted_proxies`: reverse proxies allowed
/// to set `X-Forwarded-For` for the address a play request is judged by.
pub fn router(registry: Arc<Registry>, trusted_proxies: Vec<Cidr>) -> Router {
    Router::new().route("/mp4/{file}", get(serve)).with_state(Arc::new(Shared { registry, trusted_proxies }))
}

fn text(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], msg.into()).into_response()
}

/// `?token=` wins over `Authorization: Bearer`, as on every playback route.
fn request_token<'a>(query: &'a str, req: &'a Request) -> Option<&'a str> {
    let from_query = query.split('&').find_map(|kv| kv.strip_prefix("token=")).filter(|t| !t.is_empty());
    from_query.or_else(|| {
        req.headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|t| !t.is_empty())
    })
}

/// JWT alphabet only, as on the other playback routes.
fn token_is_url_safe(t: &str) -> bool {
    t.len() <= 4096 && t.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// The TCP peer, or (only if it is a trusted proxy) the right-most
/// untrusted `X-Forwarded-For` hop. `None` without connect info (tests).
fn client_ip(req: &Request, trusted_proxies: &[Cidr]) -> Option<IpAddr> {
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>()?.0.ip();
    let xff = req.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok());
    Some(caudal_core::net::resolve_forwarded(peer, xff, trusted_proxies))
}

fn denied(d: Denied) -> Response {
    match d {
        Denied::Missing => {
            let mut r = text(StatusCode::UNAUTHORIZED, "token required");
            r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            r
        }
        Denied::Refused(_) => text(StatusCode::FORBIDDEN, "forbidden"),
    }
}

/// Which tracks a request asked for (`?video=0`, `?audio=0`).
#[derive(Debug, Clone, Copy)]
struct Want {
    video: bool,
    audio: bool,
}

impl Want {
    fn from_query(q: &str) -> Self {
        let off = |k: &str| q.split('&').any(|kv| kv == format!("{k}=0"));
        Self { video: !off("video"), audio: !off("audio") }
    }
}

/// The MP4 tracks for `tracks`: video first (track 1), then audio. `Err`
/// is the `415` message.
fn pick_tracks(tracks: &[TrackInfo], want: Want, name: &str) -> Result<Vec<Mp4Track>, String> {
    let mut out = Vec::new();
    for (kind, wanted, what, flag) in
        [(TrackKind::Video, want.video, "video", "video"), (TrackKind::Audio, want.audio, "audio", "audio")]
    {
        if !wanted {
            continue;
        }
        let of_kind: Vec<&TrackInfo> = tracks.iter().filter(|t| t.kind() == kind).collect();
        if of_kind.is_empty() {
            continue;
        }
        match of_kind.iter().find(|t| mux::carriable(t)) {
            Some(t) => out.push(Mp4Track { track_id: out.len() as u32 + 1, info: (*t).clone() }),
            None => {
                let other = if flag == "video" { "audio" } else { "video" };
                return Err(format!(
                    "this stream's {what} is {}, which MP4 here does not carry (H.264, H.265, AAC and Opus only); \
                     use /ts/{name}.ts, or /mp4/{name}.mp4?{flag}=0 for the {other} alone",
                    of_kind[0].codec.as_str()
                ));
            }
        }
    }
    if out.is_empty() {
        return Err("nothing to send: no video or audio track this output carries".into());
    }
    Ok(out)
}

async fn serve(
    State(st): State<Arc<Shared>>,
    Path(file): Path<String>,
    RawQuery(q): RawQuery,
    req: Request,
) -> Response {
    let not_found = || text(StatusCode::NOT_FOUND, "no such stream");
    let Some(name) = file.strip_suffix(".mp4").filter(|n| valid_stream_name(n)) else { return not_found() };
    let q = q.unwrap_or_default();
    let token = request_token(&q, &req);
    if token.is_some_and(|t| !token_is_url_safe(t)) {
        return text(StatusCode::FORBIDDEN, "bad token");
    }
    let ip = client_ip(&req, &st.trusted_proxies);
    // A name nobody publishes is a 404 whatever the gate says, and only an
    // authorized request may start a cluster pull.
    let known = st.registry.get(name).is_some();
    if let Err(d) = st.registry.authorize(Access::Play, name, token, ip).await {
        return if known { denied(d) } else { not_found() };
    }
    let Some(stream) = st.registry.get_or_demand(name).await.filter(|s| !s.is_ended()) else { return not_found() };

    let want = Want::from_query(&q);
    let mut sub = stream.subscribe(StartAt::LiveEdge);
    let Some(tracks) = wait_tracks(&mut sub).await else {
        return text(StatusCode::NOT_FOUND, "the stream has no tracks yet");
    };
    let picked = match pick_tracks(&tracks, want, name) {
        Ok(p) => p,
        Err(msg) => return text(StatusCode::UNSUPPORTED_MEDIA_TYPE, msg),
    };
    let Some(mux) = Mux::new(picked) else {
        return text(StatusCode::UNSUPPORTED_MEDIA_TYPE, "this stream's codec configuration cannot be written as MP4");
    };

    let session = st.registry.open_session(name, PROTOCOL, ip);
    let (tx, rx) = mpsc::channel::<Bytes>(QUEUE);
    tracing::info!(stream = %name, ?ip, session = session.id(), "mp4 play started");
    tokio::spawn(pump(name.to_owned(), sub, session, mux, want, tx));

    let body = Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move {
        let b = rx.recv().await?;
        Some((Ok::<_, Infallible>(b), rx))
    }));
    let mut resp = Response::new(body);
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, no-store"));
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    // Some proxies buffer a response they believe they can; this one never
    // ends on its own schedule.
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    resp
}

/// The stream's tracks, waiting up to [`TRACK_WAIT`] for a stream that has
/// not announced any yet.
async fn wait_tracks(sub: &mut Subscriber) -> Option<Vec<TrackInfo>> {
    let deadline = tokio::time::Instant::now() + TRACK_WAIT;
    loop {
        let tracks = sub.tracks();
        if !tracks.is_empty() {
            return Some(tracks);
        }
        match tokio::time::timeout_at(deadline, sub.recv()).await {
            Ok(Event::End) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

/// Sends `b`; `Err` is why the response ends.
async fn send(tx: &mpsc::Sender<Bytes>, session: &Session, b: Bytes) -> Result<(), &'static str> {
    let n = b.len();
    match tokio::time::timeout(STALL, tx.send(b)).await {
        Ok(Ok(())) => {
            session.add_bytes(n);
            Ok(())
        }
        Ok(Err(_)) => Err("client gone"),
        Err(_) => Err("client stalled"),
    }
}

/// Muxes `sub` into `tx` until the stream ends, the client goes away or
/// stalls, or the session is kicked. Dropping `tx` ends the response body.
async fn pump(
    name: String,
    mut sub: Subscriber,
    mut session: Session,
    mut mux: Mux,
    want: Want,
    tx: mpsc::Sender<Bytes>,
) {
    let why = 'run: {
        if let Err(why) = send(&tx, &session, mux.init().clone()).await {
            break 'run why;
        }
        loop {
            let ev = tokio::select! {
                ev = sub.recv() => ev,
                () = session.kicked() => break 'run "kicked",
                () = tx.closed() => break 'run "client gone",
            };
            let mut end = None;
            match ev {
                Event::Frame(f) => mux.push(&f),
                // An MP4 has one init segment: keep going while it would be
                // the same, end the response otherwise (the player's
                // reconnect gets the new tracks or a clear answer).
                Event::TracksChanged => {
                    let same = pick_tracks(&sub.tracks(), want, &name).is_ok_and(|p| {
                        p.len() == mux.tracks().len()
                            && p.iter().zip(mux.tracks()).all(|(a, b)| {
                                a.info.id == b.info.id && a.info.codec == b.info.codec && a.info.init == b.info.init
                            })
                    });
                    if !same {
                        mux.flush();
                        end = Some("tracks changed");
                    }
                }
                Event::Lagged { skipped } => {
                    tracing::debug!(stream = %name, skipped, "mp4 viewer lagged; resuming at next keyframe");
                    mux.lagged();
                }
                Event::Cue(_) => {}
                Event::End => {
                    mux.flush();
                    end = Some("stream ended");
                }
            }
            for frag in mux.take_output() {
                if let Err(why) = send(&tx, &session, frag).await {
                    break 'run why;
                }
            }
            if let Some(why) = end {
                break 'run why;
            }
        }
    };
    tracing::info!(stream = %name, reason = why, "mp4 play ended");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn want_from_query() {
        let w = Want::from_query("token=x&audio=0");
        assert!(w.video && !w.audio);
        let w = Want::from_query("video=0");
        assert!(!w.video && w.audio);
        let w = Want::from_query("video=00");
        assert!(w.video && w.audio);
    }
}
