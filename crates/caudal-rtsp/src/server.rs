//! RTSP server: OPTIONS/DESCRIBE/SETUP/PLAY/TEARDOWN/GET_PARAMETER against
//! any live [`caudal_core::Stream`]. One task per connection; one
//! connection is one RTSP "session" (this server never spans a session
//! across TCP connections).  A session's failure (a bad request, a dropped
//! socket) only ends that connection.
//!
//! Two transports for media, chosen per track by SETUP's `Transport`
//! header:
//! - **TCP interleaved** (RFC 2326 §10.12): RTP/RTCP framed as `$` +
//!   channel + 16-bit length inside the same TCP connection as the RTSP
//!   control traffic.
//! - **UDP unicast**: the client offers `client_port=a-b`; the server
//!   allocates a free RTP/RTCP port pair from [`udp::UdpPortPool`] and
//!   replies with `server_port=x-y`. Multicast is out of scope and always
//!   answers 461 Unsupported Transport.
//!
//! RTSPS (`bind`'s TLS twin, wired up in [`crate::serve`]) accepts the
//! same control protocol over a `tokio_rustls` stream instead of a plain
//! `TcpStream` (see [`handle_conn`]'s generic bound); a TLS connection only
//! ever offers TCP interleaved media, since sending RTP/RTCP in the clear
//! over UDP next to an encrypted control channel would defeat the point.
//! SRTP is out of scope.
//!
//! Session lifetime: a session ends on TEARDOWN, on the TCP connection
//! closing, or after being idle for `RtspConfig::session_timeout` (any RTSP
//! request, or an incoming RTCP receiver report on a UDP track's RTCP
//! socket, resets the idle clock). `SETUP`'s response carries
//! `Session: <id>;timeout=<secs>` per RFC 2326 §12.37 (60s by default, see
//! [`crate::RtspConfig`]).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use caudal_core::{Access, Denied, Event, Registry, StartAt, TrackId, TrackInfo};
use rtsp_types::headers::{
    CONTENT_BASE, CONTENT_TYPE, CSEQ, PUBLIC, RtpLowerTransport, RtpTransport, RtpTransportParameters, SESSION,
    Session as SessionHeader, TRANSPORT, Transport, Transports,
};
use rtsp_types::{HeaderName, Message, Method, ParseError, Request, Response, StatusCode, Url};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{Mutex, mpsc};

use crate::rtp::{self, PacketState};
use crate::{rtcp, udp};

/// A connection reads at most this much unparsed data before it is dropped
/// as malformed (control traffic only; media never flows this way).
const MAX_BUFFERED: usize = 64 * 1024;

/// How often a UDP track's RTCP Sender Report is sent.
const RTCP_SR_INTERVAL: Duration = Duration::from_secs(5);

/// How often the idle-timeout clock is checked.
const TIMEOUT_POLL_INTERVAL: Duration = Duration::from_secs(1);

type Headers = Vec<(HeaderName, String)>;
type Resp = (StatusCode, Headers, Vec<u8>);
type DynWriter = Box<dyn AsyncWrite + Unpin + Send>;

fn simple(status: StatusCode) -> Resp {
    (status, Vec::new(), Vec::new())
}

fn denied(d: Denied) -> Resp {
    match d {
        Denied::Missing => {
            (StatusCode::Unauthorized, vec![(rtsp_types::headers::WWW_AUTHENTICATE, "Bearer".to_owned())], Vec::new())
        }
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

/// What PLAY sends media over, for one track. Cheap to clone: UDP sockets
/// are behind an `Arc`, shared between [`ConnState::setups`] (which owns
/// the RTCP receive listener) and the play task's own copy (which only
/// sends).
#[derive(Clone)]
enum PlayTransport {
    Tcp {
        channel: u8,
    },
    Udp {
        rtp_socket: Arc<UdpSocket>,
        rtcp_socket: Arc<UdpSocket>,
        client_rtp_addr: SocketAddr,
        client_rtcp_addr: SocketAddr,
    },
}

/// One SETUP track's transport plus (for UDP) the listener task that turns
/// incoming RTCP into a keep-alive signal. Dropping this (TEARDOWN, session
/// end, or overwritten by a fresh SETUP) aborts that listener so its
/// `Arc<UdpSocket>` clone releases and, once the play task's clone is gone
/// too, the OS port frees.
struct SetupTransport {
    play: PlayTransport,
    rtcp_listener: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for SetupTransport {
    fn drop(&mut self) {
        if let Some(h) = self.rtcp_listener.take() {
            h.abort();
        }
    }
}

struct ConnState {
    session_id: Option<String>,
    stream_name: Option<String>,
    stream: Option<Arc<caudal_core::Stream>>,
    tracks: Vec<TrackInfo>,
    setups: HashMap<TrackId, SetupTransport>,
    play_handle: Option<tokio::task::JoinHandle<()>>,
    /// The RTSP server's own address on this connection; UDP SETUP binds
    /// its port pair here, matching whichever interface the client reached.
    local_ip: IpAddr,
    /// The client's address; UDP SETUP's `client_port` is a port only, RFC
    /// 2326 assumes the same host as the control connection.
    peer_ip: IpAddr,
    /// True for an RTSPS connection: only TCP interleaved is offered.
    is_tls: bool,
    /// Pinged by RTCP-receive listeners to reset the idle-session clock.
    activity_tx: mpsc::UnboundedSender<()>,
    /// The value advertised (and enforced) as `Session: ...;timeout=`.
    session_timeout: Duration,
}

impl ConnState {
    fn new(
        local_ip: IpAddr,
        peer_ip: IpAddr,
        is_tls: bool,
        activity_tx: mpsc::UnboundedSender<()>,
        session_timeout: Duration,
    ) -> Self {
        Self {
            session_id: None,
            stream_name: None,
            stream: None,
            tracks: Vec::new(),
            setups: HashMap::new(),
            play_handle: None,
            local_ip,
            peer_ip,
            is_tls,
            activity_tx,
            session_timeout,
        }
    }
}

/// Runs the plain-TCP accept loop, and the RTSPS accept loop alongside it
/// when `tls` is set. Both share one [`udp::UdpPortPool`] since they serve
/// the same stream registry.
pub(crate) async fn serve(
    bind: SocketAddr,
    tls: Option<crate::RtspTlsConfig>,
    udp_port_range: (u16, u16),
    session_timeout: Duration,
    registry: Arc<Registry>,
) -> std::io::Result<()> {
    let udp_pool = Arc::new(udp::UdpPortPool::new(udp_port_range));
    let plain = accept_plain(bind, registry.clone(), udp_pool.clone(), session_timeout);

    match tls {
        Some(t) => {
            let acceptor = caudal_tls::raw_tls_acceptor(t.cert, t.key)
                .await
                .map_err(|e| std::io::Error::new(e.kind(), format!("rtsp tls: {e}")))?;
            let secure = accept_tls(t.bind, acceptor, registry, udp_pool, session_timeout);
            tokio::try_join!(plain, secure)?;
            Ok(())
        }
        None => plain.await,
    }
}

/// Everything about one accepted connection that isn't the socket, the
/// registry, or the UDP port pool. Bundled into one value so `handle_conn`
/// takes a reasonable number of arguments.
struct ConnMeta {
    peer_ip: IpAddr,
    local_ip: IpAddr,
    host: String,
    is_tls: bool,
    session_timeout: Duration,
}

async fn accept_plain(
    bind: SocketAddr,
    registry: Arc<Registry>,
    udp_pool: Arc<udp::UdpPortPool>,
    session_timeout: Duration,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    tracing::info!(%bind, "rtsp server listening");
    loop {
        // One accept error (EMFILE, a reset before accept) must not stop the
        // listener for good. Same policy as axum::serve.
        let (socket, peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!(error = %e, "rtsp accept failed; retrying");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        let _ = socket.set_nodelay(true);
        let local = socket.local_addr().unwrap_or(bind);
        let registry = registry.clone();
        let udp_pool = udp_pool.clone();
        let meta = ConnMeta {
            peer_ip: peer.ip(),
            local_ip: local.ip(),
            host: local.to_string(),
            is_tls: false,
            session_timeout,
        };
        tokio::spawn(async move {
            if let Err(e) = handle_conn(socket, meta, registry, udp_pool).await {
                tracing::debug!(%peer, error = %e, "rtsp connection ended");
            }
        });
    }
}

async fn accept_tls(
    bind: SocketAddr,
    acceptor: tokio_rustls::TlsAcceptor,
    registry: Arc<Registry>,
    udp_pool: Arc<udp::UdpPortPool>,
    session_timeout: Duration,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    tracing::info!(%bind, "rtsps server listening");
    loop {
        // One accept error (EMFILE, a reset before accept) must not stop the
        // listener for good. Same policy as axum::serve.
        let (socket, peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!(error = %e, "rtsps accept failed; retrying");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        let _ = socket.set_nodelay(true);
        let local = socket.local_addr().unwrap_or(bind);
        let acceptor = acceptor.clone();
        let registry = registry.clone();
        let udp_pool = udp_pool.clone();
        let meta = ConnMeta {
            peer_ip: peer.ip(),
            local_ip: local.ip(),
            host: local.to_string(),
            is_tls: true,
            session_timeout,
        };
        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(socket).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(%peer, error = %e, "rtsps handshake failed");
                    return;
                }
            };
            if let Err(e) = handle_conn(tls_stream, meta, registry, udp_pool).await {
                tracing::debug!(%peer, error = %e, "rtsps connection ended");
            }
        });
    }
}

async fn handle_conn<S>(
    socket: S,
    meta: ConnMeta,
    registry: Arc<Registry>,
    udp_pool: Arc<udp::UdpPortPool>,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let ConnMeta { peer_ip, local_ip, host, is_tls, session_timeout: timeout } = meta;
    let (mut reader, writer) = tokio::io::split(socket);
    let writer: Arc<Mutex<DynWriter>> = Arc::new(Mutex::new(Box::new(writer)));
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let (activity_tx, mut activity_rx) = mpsc::unbounded_channel::<()>();
    let mut state = ConnState::new(local_ip, peer_ip, is_tls, activity_tx, timeout);
    let mut last_activity = tokio::time::Instant::now();
    let mut timeout_ticker = tokio::time::interval(TIMEOUT_POLL_INTERVAL);
    timeout_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    'conn: loop {
        loop {
            let parsed: Result<(Message<Vec<u8>>, usize), ParseError> = Message::parse(&buf);
            match parsed {
                Ok((msg, consumed)) => {
                    buf.drain(0..consumed);
                    if let Message::Request(req) = msg {
                        last_activity = tokio::time::Instant::now();
                        let resp = handle_request(&req, &registry, &mut state, &host, &writer, &udp_pool).await;
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

        tokio::select! {
            biased;
            _ = activity_rx.recv() => {
                last_activity = tokio::time::Instant::now();
            }
            _ = timeout_ticker.tick() => {
                if last_activity.elapsed() > timeout {
                    tracing::debug!("rtsp: session idle timeout");
                    break 'conn;
                }
            }
            result = reader.read(&mut chunk) => {
                let n = result?;
                if n == 0 {
                    break 'conn;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
        }
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
    writer: &Arc<Mutex<DynWriter>>,
    udp_pool: &Arc<udp::UdpPortPool>,
) -> Response<Vec<u8>> {
    let (status, headers, body) = match req.method() {
        Method::Options => (
            StatusCode::Ok,
            vec![(PUBLIC, "OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN, GET_PARAMETER".to_owned())],
            Vec::new(),
        ),
        Method::Describe => handle_describe(req, registry, state, host).await,
        Method::Setup => handle_setup(req, state, udp_pool).await,
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
        // The timeout parameter only needs to be stated once; RFC 2326
        // §12.37 puts it in the response that creates the session.
        let value = if matches!(req.method(), Method::Setup) {
            format!("{id};timeout={}", state.session_timeout.as_secs())
        } else {
            id.clone()
        };
        builder = builder.header(SESSION, value);
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
    if let Err(d) = registry.authorize(Access::Play, &name, token.as_deref(), Some(state.peer_ip)).await {
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
    let headers = vec![(CONTENT_BASE, format!("rtsp://{host}/{name}/")), (CONTENT_TYPE, "application/sdp".to_owned())];
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

/// One transport SETUP picked from the client's offered alternatives, in
/// the order the client listed them.
enum Chosen {
    Tcp { ch0: u8, ch1: u8 },
    Udp { client_rtp_port: u16, client_rtcp_port: u16 },
}

/// Picks the first offered transport this server can serve: TCP
/// interleaved always, UDP unicast only when the connection isn't TLS
/// (RTSPS keeps media inside the encrypted channel). Multicast, and a TLS
/// connection offering only UDP, fall through to `None` (461).
fn choose_transport(transports: &Transports, is_tls: bool) -> Option<Chosen> {
    transports.iter().find_map(|t| {
        let Transport::Rtp(RtpTransport {
            params: RtpTransportParameters { multicast, interleaved, client_port, .. },
            lower_transport,
            ..
        }) = t
        else {
            return None;
        };
        if *multicast {
            return None;
        }
        if let Some((ch0, ch1)) = interleaved {
            return Some(Chosen::Tcp { ch0: *ch0, ch1: ch1.unwrap_or_else(|| ch0.saturating_add(1)) });
        }
        if is_tls {
            return None;
        }
        if matches!(lower_transport, Some(RtpLowerTransport::Tcp)) {
            return None;
        }
        let (cp0, cp1) = (*client_port)?;
        Some(Chosen::Udp { client_rtp_port: cp0, client_rtcp_port: cp1.unwrap_or_else(|| cp0.saturating_add(1)) })
    })
}

/// Spawns the task that turns any datagram arriving on a UDP track's RTCP
/// socket into a keep-alive ping. It never inspects the datagram: whether
/// it is a real Receiver Report or noise, its arrival alone means the
/// client is still there. Exits (freeing its `Arc<UdpSocket>` clone) once
/// the socket is closed, e.g. by `SetupTransport::drop`.
fn spawn_rtcp_listener(socket: Arc<UdpSocket>, activity_tx: mpsc::UnboundedSender<()>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = [0u8; 1500];
        loop {
            match socket.recv_from(&mut buf).await {
                Ok(_) => {
                    let _ = activity_tx.send(());
                }
                Err(_) => return,
            }
        }
    })
}

async fn handle_setup(req: &Request<Vec<u8>>, state: &mut ConnState, udp_pool: &Arc<udp::UdpPortPool>) -> Resp {
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
    let Some(track_id) = segments.get(1).and_then(|s| s.strip_prefix("streamid=")).and_then(|n| n.parse::<u32>().ok())
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
    let Some(chosen) = choose_transport(&transports, state.is_tls) else {
        return simple(StatusCode::UnsupportedTransport);
    };

    let (setup, value) = match chosen {
        Chosen::Tcp { ch0, ch1 } => {
            let setup = SetupTransport { play: PlayTransport::Tcp { channel: ch0 }, rtcp_listener: None };
            (setup, format!("RTP/AVP/TCP;unicast;interleaved={ch0}-{ch1}"))
        }
        Chosen::Udp { client_rtp_port, client_rtcp_port } => {
            let (rtp_sock, rtcp_sock, server_rtp_port) = match udp_pool.allocate(state.local_ip).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "rtsp: udp setup failed");
                    return simple(StatusCode::InternalServerError);
                }
            };
            let rtp_sock = Arc::new(rtp_sock);
            let rtcp_sock = Arc::new(rtcp_sock);
            let client_rtp_addr = SocketAddr::new(state.peer_ip, client_rtp_port);
            let client_rtcp_addr = SocketAddr::new(state.peer_ip, client_rtcp_port);
            let listener = spawn_rtcp_listener(rtcp_sock.clone(), state.activity_tx.clone());
            let setup = SetupTransport {
                play: PlayTransport::Udp {
                    rtp_socket: rtp_sock,
                    rtcp_socket: rtcp_sock,
                    client_rtp_addr,
                    client_rtcp_addr,
                },
                rtcp_listener: Some(listener),
            };
            let server_rtcp_port = server_rtp_port + 1;
            (
                setup,
                format!(
                    "RTP/AVP;unicast;client_port={client_rtp_port}-{client_rtcp_port};server_port={server_rtp_port}-{server_rtcp_port}"
                ),
            )
        }
    };

    if state.session_id.is_none() {
        state.session_id = Some(random_session_id());
    }
    state.setups.insert(TrackId(track_id), setup);

    (StatusCode::Ok, vec![(TRANSPORT, value)], Vec::new())
}

async fn handle_play(
    req: &Request<Vec<u8>>,
    registry: &Arc<Registry>,
    state: &mut ConnState,
    writer: Arc<Mutex<DynWriter>>,
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
    if let Err(d) = registry.authorize(Access::Play, &stream_name, token.as_deref(), Some(state.peer_ip)).await {
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
    let setups: HashMap<TrackId, PlayTransport> = state.setups.iter().map(|(k, v)| (*k, v.play.clone())).collect();
    state.play_handle = Some(tokio::spawn(play_task(sub, tracks, setups, writer)));
    simple(StatusCode::Ok)
}

async fn play_task(
    mut sub: caudal_core::Subscriber,
    tracks: Vec<TrackInfo>,
    setups: HashMap<TrackId, PlayTransport>,
    writer: Arc<Mutex<DynWriter>>,
) {
    let mut state: HashMap<TrackId, PacketState> = HashMap::new();
    let mut sr_ticker = tokio::time::interval(RTCP_SR_INTERVAL);
    sr_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            ev = sub.recv() => {
                match ev {
                    Event::Frame(frame) => {
                        let Some(transport) = setups.get(&frame.track) else { continue };
                        let Some(info) = tracks.iter().find(|t| t.id == frame.track) else { continue };
                        let ps = state.entry(frame.track).or_insert_with(PacketState::new);
                        let packets = rtp::packetize(info, &frame, ps);
                        match transport {
                            PlayTransport::Tcp { channel } => {
                                let mut w = writer.lock().await;
                                for pkt in &packets {
                                    let framed = rtp::interleave(*channel, pkt);
                                    if w.write_all(&framed).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            PlayTransport::Udp { rtp_socket, client_rtp_addr, .. } => {
                                for pkt in &packets {
                                    let _ = rtp_socket.send_to(pkt, *client_rtp_addr).await;
                                }
                            }
                        }
                    }
                    Event::TracksChanged | Event::Lagged { .. } | Event::Cue(_) => continue,
                    Event::End => {
                        tracing::debug!("rtsp play_task: source stream ended, closing connection");
                        // The source is gone: close our side so the client
                        // (a pulling `caudal-rtsp` in particular) notices
                        // promptly and reconnects, instead of a connection
                        // that just goes quiet.
                        let _ = writer.lock().await.shutdown().await;
                        return;
                    }
                }
            }
            _ = sr_ticker.tick() => {
                for (track_id, transport) in &setups {
                    let PlayTransport::Udp { rtcp_socket, client_rtcp_addr, .. } = transport else { continue };
                    let Some(ps) = state.get(track_id) else { continue };
                    let cname = format!("caudal-{}", track_id.0);
                    let pkt = rtcp::sender_report(ps.ssrc(), rtcp::ntp_now(), ps.last_ts(), ps.packet_count(), ps.octet_count(), &cname);
                    let _ = rtcp_socket.send_to(&pkt, *client_rtcp_addr).await;
                }
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
