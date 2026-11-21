//! The per-connection RTMP session handler: bridges `scuffle_rtmp`'s
//! `SessionHandler` callbacks to a [`caudal_core::Publisher`] per published
//! stream.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use caudal_core::{BufferConfig, PublishError, Registry};
use scuffle_rtmp::session::server::{ServerSessionError, SessionData, SessionHandler};

use crate::ingest::FlvIngest;

/// One RTMP connection. A connection may publish more than one stream (each
/// with its own RTMP stream id), though real encoders publish exactly one.
pub(crate) struct Handler {
    registry: Arc<Registry>,
    app: String,
    buffer: BufferConfig,
    /// The TCP peer's address, passed to every `authorize` call on this
    /// connection.
    peer_ip: IpAddr,
    streams: HashMap<u32, FlvIngest>,
}

impl Handler {
    pub(crate) fn new(registry: Arc<Registry>, app: String, buffer: BufferConfig, peer_ip: IpAddr) -> Self {
        Self { registry, app, buffer, peer_ip, streams: HashMap::new() }
    }
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
        if app_name != self.app {
            tracing::info!(app = %app_name, stream = %stream_name, reason = "wrong_app", "rtmp publish rejected");
            // No Publisher is ever created for this stream_id; returning Err
            // ends the session and closes the TCP connection (see reject_error).
            return Err(reject_error());
        }

        // OBS and ffmpeg carry credentials in the stream key:
        // rtmp://host/live/<name>?token=<jwt>
        let (stream_name, query) = stream_name.split_once('?').unwrap_or((stream_name, ""));
        let token = query.split('&').find_map(|kv| kv.strip_prefix("token=")).filter(|t| !t.is_empty());
        let requested = stream_name;
        let stream_name = match self
            .registry
            .resolve(caudal_core::Access::Publish, "rtmp", requested, token, Some(self.peer_ip))
            .await
        {
            Ok(name) => name,
            Err(denied) => {
                tracing::info!(app = %app_name, stream = %requested, reason = ?denied, "rtmp publish rejected");
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
        Ok(())
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
