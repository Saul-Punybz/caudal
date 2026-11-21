//! A minimal RTMP/RTMPS client: handshake, `connect` / `createStream`, then
//! either `publish` and outgoing `VIDEODATA`/`AUDIODATA` messages
//! (`caudal-restream`'s pushes) or `play` and incoming ones
//! ([`crate::pull`]).
//!
//! The RTMP/AMF0 state machine (handshake, chunk (de)serialization,
//! `ClientSession`) is `rml_rtmp` 0.8.0 (MIT). It is sans-I/O: this module
//! is the tokio transport around it (plain TCP for `rtmp://`, `rustls` with
//! the `ring` provider for `rtmps://`, matching `crates/caudal-tls`). See
//! `REUSE.md` for why `rml_rtmp` was chosen over `rtmp-rs`.
//!
//! `rml_rtmp`'s `ClientSession` auto-generates `releaseStream`/`FCPublish`-
//! free `connect` → `createStream` → `publish` (or `play` plus a
//! `SetBufferLength` user control) traffic, sends its window acknowledgement
//! size, acknowledges the peer's window, applies `SetChunkSize` and answers
//! ping requests on its own; we only drive it with bytes in both directions
//! and react to its events.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use rml_rtmp::chunk_io::Packet;
use rml_rtmp::handshake::{Handshake, HandshakeProcessResult, PeerType};
pub use rml_rtmp::sessions::StreamMetadata;
use rml_rtmp::sessions::{
    ClientSession, ClientSessionConfig, ClientSessionEvent, ClientSessionResult, PublishRequestType,
};
use rml_rtmp::time::RtmpTimestamp;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::url::ParsedUrl;

const READ_CHUNK: usize = 64 * 1024;

/// A duplex byte stream: plain TCP or TLS-over-TCP, used identically once
/// connected.
trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// One connection to a remote RTMP server, publishing or playing.
pub struct RtmpClient {
    io: Box<dyn Io>,
    session: ClientSession,
    read_buf: [u8; READ_CHUNK],
}

fn io_err(e: std::io::Error) -> String {
    e.to_string()
}

async fn dial(parsed: &ParsedUrl, connect_timeout: Duration) -> Result<Box<dyn Io>, String> {
    let addr = format!("{}:{}", parsed.host, parsed.port);
    let tcp = tokio::time::timeout(connect_timeout, TcpStream::connect(&addr))
        .await
        .map_err(|_| "connect timed out".to_owned())?
        .map_err(io_err)?;
    let _ = tcp.set_nodelay(true);

    if !parsed.tls {
        return Ok(Box::new(tcp));
    }

    // Ring is already the provider installed elsewhere in the binary
    // (crates/caudal-tls); installing it again here is a no-op if it lost
    // the race, which is exactly what we want for a crate that may run
    // standalone in tests.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let tls_config = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(tls_config));
    let server_name =
        rustls::pki_types::ServerName::try_from(parsed.host.trim_start_matches('[').trim_end_matches(']').to_owned())
            .map_err(|e| format!("invalid TLS host: {e}"))?;

    let tls = tokio::time::timeout(connect_timeout, connector.connect(server_name, tcp))
        .await
        .map_err(|_| "TLS handshake timed out".to_owned())?
        .map_err(io_err)?;
    Ok(Box::new(tls))
}

/// What a playing connection received, in order.
#[derive(Debug)]
pub enum PlayEvent {
    /// `NetStream.Play.Start`: the server accepted the `play`.
    Started,
    /// One `VIDEODATA` message body and its RTMP timestamp (ms).
    Video { timestamp: u32, data: Bytes },
    /// One `AUDIODATA` message body and its RTMP timestamp (ms).
    Audio { timestamp: u32, data: Bytes },
    /// `onMetaData`, parsed.
    Metadata(StreamMetadata),
    /// The server says the stream is over (`NetStream.Play.Stop`,
    /// `.Complete`, `.UnpublishNotify`): the `onStatus` code.
    Ended(String),
    /// The server refused the `play` (`NetStream.Play.StreamNotFound`,
    /// `.Failed`, a rejected `connect`): the reason.
    Refused(String),
}

impl PlayEvent {
    fn from_session(ev: ClientSessionEvent) -> Option<Self> {
        Some(match ev {
            ClientSessionEvent::PlaybackRequestAccepted => PlayEvent::Started,
            ClientSessionEvent::VideoDataReceived { timestamp, data } => {
                PlayEvent::Video { timestamp: timestamp.value, data }
            }
            ClientSessionEvent::AudioDataReceived { timestamp, data } => {
                PlayEvent::Audio { timestamp: timestamp.value, data }
            }
            ClientSessionEvent::StreamMetadataReceived { metadata } => PlayEvent::Metadata(metadata),
            ClientSessionEvent::ConnectionRequestRejected { description } => {
                PlayEvent::Refused(format!("connection rejected: {description}"))
            }
            ClientSessionEvent::UnhandleableOnStatusCode { code } => match code.as_str() {
                "NetStream.Play.Stop" | "NetStream.Play.Complete" | "NetStream.Play.UnpublishNotify" => {
                    PlayEvent::Ended(code)
                }
                "NetStream.Play.StreamNotFound" | "NetStream.Play.Failed" | "NetStream.Play.BadName" => {
                    PlayEvent::Refused(code)
                }
                // Reset, PublishNotify, Buffer.Empty/Full and the like.
                _ => return None,
            },
            _ => return None,
        })
    }
}

/// Which side of the stream a connection asks for once connected.
enum Request {
    Publish,
    Play,
}

impl RtmpClient {
    /// Connects, performs the handshake, and waits for the server to accept
    /// both the connection and the publish request. On success the caller
    /// may start calling `publish_video`/`publish_audio`.
    pub async fn connect(parsed: &ParsedUrl, connect_timeout: Duration) -> Result<Self, String> {
        let (mut client, pending) = Self::open(parsed, connect_timeout).await?;
        client.wait_for_publish_accepted(&parsed.stream_key, connect_timeout, pending).await?;
        Ok(client)
    }

    /// Connects, performs the handshake, waits for the server to accept the
    /// connection and sends `createStream` + `play` for the URL's stream
    /// key. Returns what arrived so far; from then on, [`Self::read_some`]
    /// and [`Self::process_play`] yield the rest (`Started`, then media).
    pub async fn connect_play(parsed: &ParsedUrl, connect_timeout: Duration) -> Result<(Self, Vec<PlayEvent>), String> {
        let (mut client, pending) = Self::open(parsed, connect_timeout).await?;
        let events = client.wait_for_connected(Request::Play, &parsed.stream_key, connect_timeout, pending).await?;
        Ok((client, events.into_iter().filter_map(PlayEvent::from_session).collect()))
    }

    /// TCP (+ TLS), the handshake, and the `connect` command sent.
    async fn open(parsed: &ParsedUrl, connect_timeout: Duration) -> Result<(Self, Vec<ClientSessionEvent>), String> {
        let mut io = dial(parsed, connect_timeout).await?;

        let mut handshake = Handshake::new(PeerType::Client);
        let c0c1 = handshake.generate_outbound_p0_and_p1().map_err(|e| e.to_string())?;
        io.write_all(&c0c1).await.map_err(io_err)?;
        io.flush().await.map_err(io_err)?;

        let leftover;
        let mut hbuf = [0u8; 4096];
        loop {
            let n = tokio::time::timeout(connect_timeout, io.read(&mut hbuf))
                .await
                .map_err(|_| "handshake timed out".to_owned())?
                .map_err(io_err)?;
            if n == 0 {
                return Err("connection closed during handshake".to_owned());
            }
            match handshake.process_bytes(&hbuf[..n]).map_err(|e| e.to_string())? {
                HandshakeProcessResult::InProgress { response_bytes } => {
                    if !response_bytes.is_empty() {
                        io.write_all(&response_bytes).await.map_err(io_err)?;
                        io.flush().await.map_err(io_err)?;
                    }
                }
                HandshakeProcessResult::Completed { response_bytes, remaining_bytes } => {
                    if !response_bytes.is_empty() {
                        io.write_all(&response_bytes).await.map_err(io_err)?;
                        io.flush().await.map_err(io_err)?;
                    }
                    leftover = remaining_bytes;
                    break;
                }
            }
        }

        let mut cfg = ClientSessionConfig::new();
        cfg.tc_url = Some(parsed.tc_url.clone());
        let (session, _initial) = ClientSession::new(cfg).map_err(|e| e.to_string())?;

        let mut client = RtmpClient { io, session, read_buf: [0u8; READ_CHUNK] };

        let connect_packet = client.session.request_connection(parsed.app.clone()).map_err(|e| e.to_string())?;
        let mut pending = client.emit(vec![connect_packet]).await?;

        if !leftover.is_empty() {
            let results = client.session.handle_input(&leftover).map_err(|e| e.to_string())?;
            pending.extend(client.emit(results).await?);
        }
        Ok((client, pending))
    }

    /// Writes every `OutboundResponse` in `results` to the peer, in order,
    /// and returns the raised events. RTMP chunk header compression means
    /// these must go out in the order the session produced them.
    async fn emit(&mut self, results: Vec<ClientSessionResult>) -> Result<Vec<ClientSessionEvent>, String> {
        let mut events = Vec::new();
        for r in results {
            match r {
                ClientSessionResult::OutboundResponse(Packet { bytes, .. }) => {
                    self.io.write_all(&bytes).await.map_err(io_err)?;
                    self.io.flush().await.map_err(io_err)?;
                }
                ClientSessionResult::RaisedEvent(e) => events.push(e),
                ClientSessionResult::UnhandleableMessageReceived(_) => {}
            }
        }
        Ok(events)
    }

    /// Waits until the server has accepted both the connection and the
    /// publish request (see [`Self::wait_for_connected`]).
    async fn wait_for_publish_accepted(
        &mut self,
        stream_key: &str,
        overall_timeout: Duration,
        pending: Vec<ClientSessionEvent>,
    ) -> Result<(), String> {
        self.wait_for_connected(Request::Publish, stream_key, overall_timeout, pending).await.map(drop)
    }

    /// Drains `pending` events (from the connect packet's response and any
    /// leftover handshake bytes), then keeps reading until the server has
    /// accepted the connection, sending `createStream` + `publish`/`play`
    /// (via `rml_rtmp`'s own `_result`-triggered follow-up) once it has.
    /// A publish also waits for `NetStream.Publish.Start`; a play returns
    /// right after its request is sent, with the events seen after it (a
    /// server may send media before `NetStream.Play.Start`, and the caller
    /// wants those too).
    async fn wait_for_connected(
        &mut self,
        request: Request,
        stream_key: &str,
        overall_timeout: Duration,
        mut pending: Vec<ClientSessionEvent>,
    ) -> Result<Vec<ClientSessionEvent>, String> {
        let deadline = tokio::time::Instant::now() + overall_timeout;
        let mut requested = false;
        let what = match request {
            Request::Publish => "publishing",
            Request::Play => "the connection",
        };
        loop {
            let mut after = Vec::new();
            for ev in pending.drain(..) {
                match ev {
                    ClientSessionEvent::ConnectionRequestAccepted if !requested => {
                        requested = true;
                        let pkt = match request {
                            Request::Publish => {
                                self.session.request_publishing(stream_key.to_owned(), PublishRequestType::Live)
                            }
                            Request::Play => self.session.request_playback(stream_key.to_owned()),
                        }
                        .map_err(|e| e.to_string())?;
                        self.emit(vec![pkt]).await?;
                    }
                    ClientSessionEvent::ConnectionRequestRejected { description } => {
                        return Err(format!("connection rejected: {description}"));
                    }
                    ClientSessionEvent::PublishRequestAccepted => return Ok(Vec::new()),
                    other if requested => after.push(other),
                    _ => {}
                }
            }
            if requested && matches!(request, Request::Play) {
                return Ok(after);
            }

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(format!("timed out waiting for the server to accept {what}"));
            }
            let mut buf = [0u8; 4096];
            let n = tokio::time::timeout(remaining, self.io.read(&mut buf))
                .await
                .map_err(|_| format!("timed out waiting for the server to accept {what}"))?
                .map_err(io_err)?;
            if n == 0 {
                return Err(format!("connection closed before the server accepted {what}"));
            }
            let results = self.session.handle_input(&buf[..n]).map_err(|e| e.to_string())?;
            pending = self.emit(results).await?;
        }
    }

    /// Waits for bytes from the peer (pings, acks, status changes) and
    /// returns how many arrived; `Err` when the connection is gone. Only
    /// reads, so it is cancel-safe inside `tokio::select!`. The caller then
    /// passes the count to [`Self::process`] outside the `select!`, because
    /// that step writes replies, and a write cancelled halfway would
    /// corrupt the RTMP chunk stream.
    pub async fn read_some(&mut self) -> Result<usize, String> {
        let n = self.io.read(&mut self.read_buf).await.map_err(io_err)?;
        if n == 0 {
            return Err("connection closed".to_owned());
        }
        Ok(n)
    }

    /// Feeds the `n` bytes that [`Self::read_some`] read to the session and
    /// writes any replies.
    pub async fn process(&mut self, n: usize) -> Result<(), String> {
        let results = self.session.handle_input(&self.read_buf[..n]).map_err(|e| e.to_string())?;
        self.emit(results).await?;
        Ok(())
    }

    /// [`Self::process`] for a playing connection: also returns what the
    /// bytes carried (media, metadata, status changes), in order.
    pub async fn process_play(&mut self, n: usize) -> Result<Vec<PlayEvent>, String> {
        let results = self.session.handle_input(&self.read_buf[..n]).map_err(|e| e.to_string())?;
        let events = self.emit(results).await?;
        Ok(events.into_iter().filter_map(PlayEvent::from_session).collect())
    }

    /// Sends one `VIDEODATA` message. Returns the number of bytes written
    /// to the socket (chunk framing included), for the byte counter.
    pub async fn publish_video(&mut self, data: Bytes, timestamp_ms: u32) -> Result<u64, String> {
        let r = self
            .session
            .publish_video_data(data, RtmpTimestamp::new(timestamp_ms), false)
            .map_err(|e| e.to_string())?;
        self.write_one(r).await
    }

    /// Sends one `AUDIODATA` message.
    pub async fn publish_audio(&mut self, data: Bytes, timestamp_ms: u32) -> Result<u64, String> {
        let r = self
            .session
            .publish_audio_data(data, RtmpTimestamp::new(timestamp_ms), false)
            .map_err(|e| e.to_string())?;
        self.write_one(r).await
    }

    async fn write_one(&mut self, r: ClientSessionResult) -> Result<u64, String> {
        let n = match &r {
            ClientSessionResult::OutboundResponse(p) => p.bytes.len() as u64,
            _ => 0,
        };
        self.emit(vec![r]).await?;
        Ok(n)
    }
}
