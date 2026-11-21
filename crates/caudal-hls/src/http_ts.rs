//! HTTP-TS: a live stream as one endless MPEG-TS response at
//! `/ts/{name}.ts`, the way MistServer serves `/{name}.ts`. VLC, ffmpeg,
//! IRDs, set-top boxes and playout software open it as a plain URL, and
//! another server pulls it (`[[http.pull]]` in Caudal).
//!
//! Each request is one viewer: it subscribes at the newest keyframe, gets
//! PAT/PMT first, then every frame in order, until the stream ends, the
//! client goes away, or an operator kicks the session. The muxer runs on
//! the media clock (see `caudal_ts::mux::TsMux::with_media_clock`), so the
//! GOP handed over at once on join still carries PSI and PCR at media-time
//! cadence.
//!
//! A slow client never grows memory without bound: at most [`QUEUE`] muxed
//! chunks wait for the socket, and while they wait this task stops reading
//! the stream; the stream's own buffer then moves the subscriber forward to
//! the next keyframe (`Event::Lagged`). A client that reads nothing at all
//! for [`STALL`] is closed.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, RawQuery, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use bytes::Bytes;
use caudal_core::{Access, Event, Registry, Session, StartAt, Subscriber};
use caudal_ts::mux::TsMux;
use tokio::sync::mpsc;

use crate::{client_ip, error, request_token, respond, token_is_url_safe};

/// Muxed chunks (one per frame, roughly) that may wait for a slow client.
pub const QUEUE: usize = 32;
/// A client that takes nothing for this long is disconnected.
pub const STALL: Duration = Duration::from_secs(15);
/// The protocol name this output's viewers show in `/api/v1/sessions`.
pub const PROTOCOL: &str = "http-ts";

struct HttpTs {
    registry: Arc<Registry>,
    trusted_proxies: Vec<caudal_core::Cidr>,
}

/// `GET /ts/{name}.ts`: the stream `name` as continuous MPEG-TS.
///
/// Same rules as every other playback route: a play token from `?token=`
/// or `Authorization: Bearer`, judged by `Registry::authorize` against the
/// client address (`trusted_proxies` as in [`crate::router`]); on a cluster
/// edge the first viewer starts the pull (`Registry::get_or_demand`).
pub fn router(registry: Arc<Registry>, trusted_proxies: Vec<caudal_core::Cidr>) -> Router {
    Router::new().route("/ts/{file}", get(serve)).with_state(Arc::new(HttpTs { registry, trusted_proxies }))
}

async fn serve(
    State(st): State<Arc<HttpTs>>,
    Path(file): Path<String>,
    RawQuery(query): RawQuery,
    req: Request,
) -> Response {
    let Some(name) = file.strip_suffix(".ts").filter(|n| !n.is_empty()) else {
        return error(StatusCode::NOT_FOUND);
    };
    let query = query.as_deref().unwrap_or("");
    let token = request_token(query, &req);
    if token.is_some_and(|t| !token_is_url_safe(t)) {
        return error(StatusCode::FORBIDDEN);
    }
    let ip = client_ip(&req, &st.trusted_proxies);
    match st.registry.authorize(Access::Play, name, token, ip).await {
        Ok(()) => {}
        Err(caudal_core::Denied::Missing) => {
            let mut r = error(StatusCode::UNAUTHORIZED);
            r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            return r;
        }
        Err(caudal_core::Denied::Refused(_)) => return error(StatusCode::FORBIDDEN),
    }
    let Some(stream) = st.registry.get_or_demand(name).await.filter(|s| !s.is_ended()) else {
        return error(StatusCode::NOT_FOUND);
    };

    // A real viewer: counted in the stream's viewer stats and listed as a
    // session, the same as an SRT or HLS viewer.
    let sub = stream.subscribe(StartAt::LiveEdge);
    let session = st.registry.open_session(name, PROTOCOL, ip);
    let (tx, rx) = mpsc::channel::<Result<Bytes, Infallible>>(QUEUE);
    tracing::info!(stream = %name, ?ip, "http-ts play started");
    tokio::spawn(pump(name.to_owned(), sub, session, tx));

    let body = Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(rx));
    let mut r = respond(StatusCode::OK, "video/mp2t", "no-cache, no-store", body);
    // Some proxies buffer a response they believe they can; this one never
    // ends on its own schedule.
    r.headers_mut().insert("x-accel-buffering", HeaderValue::from_static("no"));
    r
}

/// Muxes `sub` into `tx` until the stream ends, the client goes away or
/// stalls, or the session is kicked. Dropping `tx` ends the response body.
async fn pump(name: String, mut sub: Subscriber, mut session: Session, tx: mpsc::Sender<Result<Bytes, Infallible>>) {
    let mut tracks = sub.tracks();
    let mut mux = TsMux::with_media_clock();
    mux.set_tracks(&tracks);
    let why = loop {
        let ev = tokio::select! {
            ev = sub.recv() => ev,
            () = session.kicked() => break "kicked",
            () = tx.closed() => break "client gone",
        };
        let end = match ev {
            Event::TracksChanged => {
                tracks = sub.tracks();
                mux.set_tracks(&tracks);
                false
            }
            Event::Frame(frame) => {
                if let Some(info) = tracks.iter().find(|t| t.id == frame.track) {
                    mux.push_frame(info, &frame);
                }
                false
            }
            Event::Cue(cue) => {
                mux.push_cue(&cue);
                false
            }
            // The subscriber already moved to the next keyframe, and the
            // muxer re-sends parameter sets (and PSI) on every keyframe, so
            // the client resyncs on what comes next.
            Event::Lagged { skipped } => {
                tracing::debug!(stream = %name, skipped, "http-ts viewer lagged; resuming at next keyframe");
                false
            }
            Event::End => true,
        };
        let out = mux.take_output();
        if !out.is_empty() {
            let n = out.len();
            match tokio::time::timeout(STALL, tx.send(Ok(Bytes::from(out)))).await {
                Ok(Ok(())) => session.add_bytes(n),
                Ok(Err(_)) => break "client gone",
                Err(_) => break "client stalled",
            }
        }
        if end {
            break "stream ended";
        }
    };
    tracing::info!(stream = %name, reason = why, "http-ts play ended");
}
