//! RTSP server: TCP interleaved OPTIONS/DESCRIBE/SETUP/PLAY/TEARDOWN/
//! GET_PARAMETER against any live [`caudal_core::Stream`]. One task per
//! connection; one connection is one RTSP "session" (this server never
//! spans a session across TCP connections). A session's failure (a bad
//! request, a dropped socket) only ends that connection.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use caudal_core::{Access, Denied, Event, Registry, StartAt, TrackId, TrackInfo};
use rtsp_types::headers::{
    CONTENT_BASE, CONTENT_TYPE, CSEQ, PUBLIC, RtpLowerTransport, RtpTransport, RtpTransportParameters, SESSION,
    Session as SessionHeader, TRANSPORT, Transport, Transports, WWW_AUTHENTICATE,
};
use rtsp_types::{HeaderName, Message, Method, ParseError, Request, Response, StatusCode, Url};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::Mutex;

use crate::rtp::{self, PacketState};

/// A connection reads at most this much unparsed data before it is dropped
/// as malformed (control traffic only; media never flows this way).
const MAX_BUFFERED: usize = 64 * 1024;

type Headers = Vec<(HeaderName, String)>;
type Resp = (StatusCode, Headers, Vec<u8>);

fn simple(status: StatusCode) -> Resp {
    (status, Vec::new(), Vec::new())
}

fn denied(d: Denied) -> Resp {
    match d {
        Denied::Missing => (StatusCode::Unauthorized, vec![(WWW_AUTHENTICATE, "Bearer".to_owned())], Vec::new()),
        Denied::Refused(reason) => {
            tracing::info!(%reason, "rtsp: access refused");
            simple(StatusCode::Forbidden)
        }
    }
}

/// `(stream name, path segments, ?token=)` from a request's URI.
fn parse_uri(req: &Request<Vec<u8>>) -> Option<(String, Vec<String>, Option<String>)> {
    let uri: &Url = req.request_uri()?;
    let token = uri.query_pairs().find(|(k, _)| k.as_ref() == "token").map(|(_, v)| v.into_owned());
    let segments: Vec<String> = uri.path_segments()?.filter(|s| !s.is_empty()).map(str::to_owned).collect();
    let name = segments.first()?.clone();
    Some((name, segments, token))
}

fn random_session_id() -> String {
    format!("{:016X}", rand::random::<u64>())
}

#[derive(Default)]
struct ConnState {
    session_id: Option<String>,
    stream_name: Option<String>,
    stream: Option<Arc<caudal_core::Stream>>,
    tracks: Vec<TrackInfo>,
    /// Track id -> interleaved RTP channel, as SETUP established it.
    setups: HashMap<TrackId, u8>,
    play_handle: Option<tokio::task::JoinHandle<()>>,
}

pub(crate) async fn serve(bind: SocketAddr, registry: Arc<Registry>) -> std::io::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    tracing::info!(%bind, "rtsp server listening");
    loop {
        let (socket, peer) = listener.accept().await?;
        let _ = socket.set_nodelay(true);
        let registry = registry.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(socket, registry).await {
                tracing::debug!(%peer, error = %e, "rtsp connection ended");
            }
        });
    }
}

async fn handle_conn(socket: tokio::net::TcpStream, registry: Arc<Registry>) -> std::io::Result<()> {
    let host = socket.local_addr().map(|a| a.to_string()).unwrap_or_else(|_| "127.0.0.1:554".to_owned());
    let (mut reader, writer) = socket.into_split();
    let writer = Arc::new(Mutex::new(writer));
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let mut state = ConnState::default();

    'conn: loop {
        loop {
            let parsed: Result<(Message<Vec<u8>>, usize), ParseError> = Message::parse(&buf);
            match parsed {
                Ok((msg, consumed)) => {
                    buf.drain(0..consumed);
                    if let Message::Request(req) = msg {
                        let resp = handle_request(&req, &registry, &mut state, &host, &writer).await;
                        let mut out = Vec::new();
                        if resp.write(&mut out).is_err() {
                            break 'conn;
                        }
                        let mut w = writer.lock().await;
                        if w.write_all(&out).await.is_err() {
                            break 'conn;
                        }
                    }
                    // Data/Response messages from the client (stray RTCP,
                    // mostly) are simply discarded.
                }
                Err(ParseError::Incomplete(_)) => break,
                Err(ParseError::Error) => break 'conn,
            }
        }
        if buf.len() > MAX_BUFFERED {
            break;
        }
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }

    if let Some(h) = state.play_handle.take() {
        h.abort();
    }
    Ok(())
}

async fn handle_request(
    req: &Request<Vec<u8>>,
    registry: &Arc<Registry>,
    state: &mut ConnState,
    host: &str,
    writer: &Arc<Mutex<OwnedWriteHalf>>,
) -> Response<Vec<u8>> {
    let (status, headers, body) = match req.method() {
        Method::Options => {
            (StatusCode::Ok, vec![(PUBLIC, "OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN, GET_PARAMETER".to_owned())], Vec::new())
        }
        Method::Describe => handle_describe(req, registry, state, host).await,
        Method::Setup => handle_setup(req, state),
        Method::Play => handle_play(req, registry, state, writer.clone()).await,
        Method::Teardown => handle_teardown(state),
        Method::GetParameter => simple(StatusCode::Ok),
        _ => simple(StatusCode::MethodNotAllowed),
    };

    let mut builder = Response::builder(req.version(), status);
    if let Some(cseq) = req.header(&CSEQ) {
        builder = builder.header(CSEQ, cseq.as_str().to_owned());
    }
    if let Some(id) = &state.session_id {
        builder = builder.header(SESSION, id.clone());
    }
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    builder.build(body)
}

async fn handle_describe(req: &Request<Vec<u8>>, registry: &Arc<Registry>, state: &mut ConnState, host: &str) -> Resp {
    let Some((name, _segments, token)) = parse_uri(req) else {
        return simple(StatusCode::BadRequest);
    };
    if let Err(d) = registry.authorize(Access::Play, &name, token.as_deref()).await {
        return denied(d);
    }
    let Some(stream) = registry.get(&name) else {
        return simple(StatusCode::NotFound);
    };
    let tracks = stream.tracks();

    state.stream_name = Some(name.clone());
    state.stream = Some(stream);
    state.tracks = tracks.clone();

    let body = crate::sdp::build(&name, host, &tracks, token.as_deref());
    let headers =
        vec![(CONTENT_BASE, format!("rtsp://{host}/{name}/")), (CONTENT_TYPE, "application/sdp".to_owned())];
    (StatusCode::Ok, headers, body)
}

/// `Some(response)` when a `Session` header was sent and doesn't match the
/// session already established on this connection.
fn session_mismatch(req: &Request<Vec<u8>>, state: &ConnState) -> Option<Resp> {
    let Ok(Some(sess)) = req.typed_header::<SessionHeader>() else { return None };
    match &state.session_id {
        Some(id) if id.as_str() != sess.0.as_str() => Some(simple(StatusCode::SessionNotFound)),
        _ => None,
    }
}

fn handle_setup(req: &Request<Vec<u8>>, state: &mut ConnState) -> Resp {
    let Some((name, segments, _token)) = parse_uri(req) else {
        return simple(StatusCode::BadRequest);
    };
    let Some(stream_name) = state.stream_name.clone() else {
        return simple(StatusCode::MethodNotValidInThisState);
    };
    if name != stream_name {
        return simple(StatusCode::NotFound);
    }
    if let Some(mismatch) = session_mismatch(req, state) {
        return mismatch;
    }
    let Some(track_id) =
        segments.get(1).and_then(|s| s.strip_prefix("streamid=")).and_then(|n| n.parse::<u32>().ok())
    else {
        return simple(StatusCode::BadRequest);
    };
    if !state.tracks.iter().any(|t| t.id.0 == track_id) {
        return simple(StatusCode::NotFound);
    }

    let transports = match req.typed_header::<Transports>() {
        Ok(Some(t)) => t,
        _ => return simple(StatusCode::UnsupportedTransport),
    };
    // TCP interleaved is the only transport implemented; anything without an
    // `interleaved=` parameter (plain UDP) gets 461, as the brief allows.
    let Some(interleaved) = transports.iter().find_map(|t| match t {
        Transport::Rtp(RtpTransport { params: RtpTransportParameters { interleaved: Some(i), .. }, .. }) => Some(*i),
        _ => None,
    }) else {
        return simple(StatusCode::UnsupportedTransport);
    };
    let (ch0, ch1) = interleaved;
    let ch1 = ch1.unwrap_or_else(|| ch0.saturating_add(1));

    if state.session_id.is_none() {
        state.session_id = Some(random_session_id());
    }
    state.setups.insert(TrackId(track_id), ch0);

    let _ = RtpLowerTransport::Tcp; // documents the transport this echoes below
    let value = format!("RTP/AVP/TCP;unicast;interleaved={ch0}-{ch1}");
    (StatusCode::Ok, vec![(TRANSPORT, value)], Vec::new())
}

async fn handle_play(
    req: &Request<Vec<u8>>,
    registry: &Arc<Registry>,
    state: &mut ConnState,
    writer: Arc<Mutex<OwnedWriteHalf>>,
) -> Resp {
    if let Some(mismatch) = session_mismatch(req, state) {
        return mismatch;
    }
    if state.setups.is_empty() {
        return simple(StatusCode::MethodNotValidInThisState);
    }
    let Some(stream_name) = state.stream_name.clone() else {
        return simple(StatusCode::MethodNotValidInThisState);
    };
    let token = parse_uri(req).and_then(|(_, _, t)| t);
    if let Err(d) = registry.authorize(Access::Play, &stream_name, token.as_deref()).await {
        return denied(d);
    }
    let Some(stream) = state.stream.clone() else {
        return simple(StatusCode::NotFound);
    };
    if state.play_handle.is_some() {
        return simple(StatusCode::Ok);
    }

    let sub = stream.subscribe(StartAt::LiveEdge);
    let tracks = state.tracks.clone();
    let setups = state.setups.clone();
    state.play_handle = Some(tokio::spawn(play_task(sub, tracks, setups, writer)));
    simple(StatusCode::Ok)
}

async fn play_task(
    mut sub: caudal_core::Subscriber,
    tracks: Vec<TrackInfo>,
    setups: HashMap<TrackId, u8>,
    writer: Arc<Mutex<OwnedWriteHalf>>,
) {
    let mut state: HashMap<TrackId, PacketState> = HashMap::new();
    loop {
        match sub.recv().await {
            Event::Frame(frame) => {
                let Some(&channel) = setups.get(&frame.track) else { continue };
                let Some(info) = tracks.iter().find(|t| t.id == frame.track) else { continue };
                let ps = state.entry(frame.track).or_insert_with(PacketState::new);
                let packets = rtp::packetize(info, &frame, ps, channel);
                let mut w = writer.lock().await;
                for pkt in packets {
                    if w.write_all(&pkt).await.is_err() {
                        return;
                    }
                }
            }
            Event::TracksChanged | Event::Lagged { .. } | Event::Cue(_) => continue,
            Event::End => {
                tracing::debug!("rtsp play_task: source stream ended, closing connection");
                // The source is gone: close our side so the client (a
                // pulling `caudal-rtsp` in particular) notices promptly and
                // reconnects, instead of a connection that just goes quiet.
                let _ = writer.lock().await.shutdown().await;
                return;
            }
        }
    }
}

fn handle_teardown(state: &mut ConnState) -> Resp {
    if let Some(h) = state.play_handle.take() {
        h.abort();
    }
    state.setups.clear();
    simple(StatusCode::Ok)
}
