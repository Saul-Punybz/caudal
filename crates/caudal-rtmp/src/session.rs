//! The per-connection RTMP session handler: bridges `scuffle_rtmp`'s
//! `SessionHandler` callbacks to a [`caudal_core::Publisher`] per published
//! stream, and to a [`crate::play::Play`] when the client plays one.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use caudal_core::{Access, BufferConfig, PublishError, Registry, StartAt};
use scuffle_rtmp::session::server::{PlayMessage, PlayResponse, ServerSessionError, SessionData, SessionHandler};

use crate::ingest::FlvIngest;
use crate::play::{Next, Play};

/// One RTMP connection. A connection may publish more than one stream (each
/// with its own RTMP stream id), though real encoders publish exactly one.
pub(crate) struct Handler {
    registry: Arc<Registry>,
    app: String,
    buffer: BufferConfig,
    /// The TCP peer's address, passed to every `authorize` call on this
    /// connection.
    peer_ip: IpAddr,
    /// Whether `play` is answered at all (`[rtmp] play`).
    play_enabled: bool,
    streams: HashMap<u32, FlvIngest>,
    /// The stream this connection plays, with its RTMP stream id.
    play: Option<(u32, Play)>,
}

impl Handler {
    pub(crate) fn new(
        registry: Arc<Registry>,
        app: String,
        buffer: BufferConfig,
        peer_ip: IpAddr,
        play_enabled: bool,
    ) -> Self {
        Self { registry, app, buffer, peer_ip, play_enabled, streams: HashMap::new(), play: None }
    }
}

/// `name?token=...` (OBS and ffmpeg put credentials in the stream key) into
/// the name and the token, if any.
fn split_token(stream_name: &str) -> (&str, Option<&str>) {
    let (name, query) = stream_name.split_once('?').unwrap_or((stream_name, ""));
    let token = query.split('&').find_map(|kv| kv.strip_prefix("token=")).filter(|t| !t.is_empty());
    (name, token)
}

const PLAY_NOT_FOUND: &str = "NetStream.Play.StreamNotFound";
const PLAY_FAILED: &str = "NetStream.Play.Failed";

fn reject(code: &'static str, description: &str) -> PlayResponse {
    PlayResponse::Reject { code, description: description.to_owned() }
}

/// `ServerSessionError` (scuffle-rtmp 0.2.3) is a closed enum with no
/// "rejected"/"custom" variant we're allowed to add (see NOTES.md). Returning
/// *any* `Err` from `on_publish` still does what we need: `on_command_publish`
/// propagates it with `?` before sending `NetStream.Publish.Start`, straight
/// out of `ServerSession::run(self)`, which drops `self` — and with it the
/// `TcpStream` it owns — closing the connection immediately, no extra
/// round trip. `InvalidChunkSize(0)` is reused purely as an inert carrier to
/// trigger that unwind; its `Display` text is misleading and must never be
/// treated as the reason. The real reason is the `rtmp publish rejected` info
/// log emitted right before returning it.
fn reject_error() -> ServerSessionError {
    ServerSessionError::InvalidChunkSize(0)
}

impl SessionHandler for Handler {
    async fn on_publish(
        &mut self,
        stream_id: u32,
        app_name: &str,
        stream_name: &str,
    ) -> Result<(), ServerSessionError> {
        // OBS and ffmpeg carry credentials in the stream key:
        // rtmp://host/live/<name>?token=<jwt>, or a stream key alone.
        let (requested, token) = split_token(stream_name);
        if app_name != self.app {
            let stream = self.registry.loggable(requested);
            tracing::info!(app = %app_name, %stream, reason = "wrong_app", "rtmp publish rejected");
            // No Publisher is ever created for this stream_id; returning Err
            // ends the session and closes the TCP connection (see reject_error).
            return Err(reject_error());
        }
        let stream_name = match self
            .registry
            .resolve(caudal_core::Access::Publish, "rtmp", requested, token, Some(self.peer_ip))
            .await
        {
            Ok(name) => name,
            Err(denied) => {
                let stream = self.registry.loggable(requested);
                tracing::info!(app = %app_name, %stream, reason = ?denied, "rtmp publish rejected");
                return Err(reject_error());
            }
        };
        let stream_name = stream_name.as_str();

        match self.registry.publish(stream_name, self.buffer) {
            Ok(publisher) => {
                self.streams.insert(stream_id, FlvIngest::new(publisher));
                Ok(())
            }
            Err(PublishError::Busy(name)) => {
                tracing::info!(app = %app_name, stream = %name, reason = "busy", "rtmp publish rejected");
                Err(reject_error())
            }
            Err(PublishError::InvalidName) => {
                tracing::info!(app = %app_name, stream = %stream_name, reason = "invalid_name", "rtmp publish rejected");
                Err(reject_error())
            }
        }
    }

    async fn on_unpublish(&mut self, stream_id: u32) -> Result<(), ServerSessionError> {
        // Dropping the FlvIngest (and its Publisher) ends the stream immediately.
        self.streams.remove(&stream_id);
        // `deleteStream` on the played stream: the viewer is done.
        if self.play.as_ref().is_some_and(|(id, _)| *id == stream_id) {
            self.play = None;
        }
        Ok(())
    }

    async fn on_play(
        &mut self,
        stream_id: u32,
        app_name: &str,
        stream_name: &str,
    ) -> Result<PlayResponse, ServerSessionError> {
        let (name, token) = split_token(stream_name);
        if !self.play_enabled {
            let stream = self.registry.loggable(name);
            tracing::info!(%stream, reason = "play_disabled", "rtmp play rejected");
            return Ok(reject(PLAY_FAILED, "play is disabled on this server"));
        }
        if app_name != self.app {
            let stream = self.registry.loggable(name);
            tracing::info!(app = %app_name, %stream, reason = "wrong_app", "rtmp play rejected");
            return Ok(reject(PLAY_NOT_FOUND, "no such application"));
        }
        // The rewrite hook (if any) picks the real name; the gate judges it.
        let name = match self.registry.resolve(Access::Play, "rtmp", name, token, Some(self.peer_ip)).await {
            Ok(resolved) => resolved,
            Err(denied) => {
                let stream = self.registry.loggable(name);
                tracing::info!(%stream, reason = ?denied, "rtmp play rejected");
                return Ok(reject(PLAY_FAILED, "access denied"));
            }
        };
        let name = name.as_str();
        // `get_or_demand`: on a cluster edge, the first viewer starts the pull.
        let Some(stream) = self.registry.get_or_demand(name).await.filter(|s| !s.is_ended()) else {
            tracing::info!(stream = %name, reason = "not_found", "rtmp play rejected");
            return Ok(reject(PLAY_NOT_FOUND, "stream not found"));
        };
        // A real viewer: counted in the stream's viewers, listed in
        // /api/v1/sessions, kickable.
        let sub = stream.subscribe(StartAt::LiveEdge);
        let session = self.registry.open_session(name, "rtmp", Some(self.peer_ip));
        tracing::info!(peer = %self.peer_ip, stream = %name, "rtmp play started");
        self.play = Some((stream_id, Play::new(sub, session, name.to_owned())));
        Ok(PlayResponse::Start)
    }

    async fn next_play_message(&mut self) -> PlayMessage {
        let Some((_, play)) = self.play.as_mut() else {
            return std::future::pending().await;
        };
        match play.next().await {
            Next::Send(PlayMessage::End) => {
                tracing::info!(peer = %self.peer_ip, stream = %play.stream(), "rtmp play ended: stream ended");
                self.play = None;
                PlayMessage::End
            }
            Next::Send(msg) => msg,
            Next::Kicked => {
                tracing::info!(peer = %self.peer_ip, stream = %play.stream(), "rtmp play kicked");
                self.play = None;
                PlayMessage::Close
            }
        }
    }

    async fn on_data(&mut self, stream_id: u32, data: SessionData) -> Result<(), ServerSessionError> {
        let Some(ingest) = self.streams.get(&stream_id) else {
            // Data for a stream we rejected or never announced: drop it.
            return Ok(());
        };
        match data {
            SessionData::Video { timestamp, data } => ingest.video(timestamp, data),
            SessionData::Audio { timestamp, data } => ingest.audio(timestamp, data),
            SessionData::Amf0 { timestamp, data } => ingest.amf0(timestamp, data),
        }
        Ok(())
    }
}
