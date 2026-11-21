//! Defines types for handling session events.

use bytes::Bytes;

use super::error::ServerSessionError;
use crate::command_messages::UnknownCommand;
use crate::messages::UnknownMessage;

/// Data received from a session.
#[derive(Debug, Clone)]
pub enum SessionData {
    /// Video data.
    Video {
        /// Timestamp of the data.
        timestamp: u32,
        /// Data.
        data: Bytes,
    },
    /// Audio data.
    Audio {
        /// Timestamp of the data.
        timestamp: u32,
        /// Data.
        data: Bytes,
    },
    /// Metadata.
    Amf0 {
        /// Timestamp of the data.
        timestamp: u32,
        /// Data.
        data: Bytes,
    },
}

/// Caudal patch: the handler's answer to a `play` command.
#[derive(Debug, Clone)]
pub enum PlayResponse {
    /// Start playing: the session sends `StreamBegin`, `NetStream.Play.Reset`
    /// and `NetStream.Play.Start`, then every message
    /// [`SessionHandler::next_play_message`] returns.
    Start,
    /// Refuse: the session sends an `onStatus` at level `error` with this
    /// code (`NetStream.Play.StreamNotFound`, `NetStream.Play.Failed`) and
    /// description, then closes the connection.
    Reject {
        /// The `onStatus` code.
        code: &'static str,
        /// The `onStatus` description.
        description: String,
    },
}

/// Caudal patch: one message the server sends a client that is playing.
#[derive(Debug, Clone)]
pub enum PlayMessage {
    /// A `VIDEODATA` message (an FLV video tag body).
    Video {
        /// Timestamp (ms).
        timestamp: u32,
        /// Tag body.
        data: Bytes,
    },
    /// An `AUDIODATA` message (an FLV audio tag body).
    Audio {
        /// Timestamp (ms).
        timestamp: u32,
        /// Tag body.
        data: Bytes,
    },
    /// An AMF0 data message (`onMetaData`).
    Amf0 {
        /// Timestamp (ms).
        timestamp: u32,
        /// AMF0 payload.
        data: Bytes,
    },
    /// The stream ended: the session sends `StreamEOF` and
    /// `NetStream.Play.Stop`, then closes the connection.
    End,
    /// Close the connection now, sending nothing more (a kicked viewer).
    Close,
}

/// Handler for session events.
pub trait SessionHandler {
    /// Called when a stream is published.
    fn on_publish(
        &mut self,
        stream_id: u32,
        app_name: &str,
        stream_name: &str,
    ) -> impl std::future::Future<Output = Result<(), ServerSessionError>> + Send;

    /// Called when a stream is unpublished.
    fn on_unpublish(&mut self, stream_id: u32) -> impl std::future::Future<Output = Result<(), ServerSessionError>> + Send;

    /// Called when an unknown/undefined message is received.
    fn on_unknown_message(
        &mut self,
        stream_id: u32,
        message: UnknownMessage,
    ) -> impl std::future::Future<Output = Result<(), ServerSessionError>> + Send {
        async move {
            tracing::warn!(stream_id = %stream_id, message = ?message, "unknown message");
            Ok(())
        }
    }

    /// Called when an unknown/undefined command is received.
    fn on_unknown_command(
        &mut self,
        stream_id: u32,
        command: UnknownCommand,
    ) -> impl std::future::Future<Output = Result<(), ServerSessionError>> + Send {
        async move {
            tracing::debug!(stream_id = %stream_id, command = ?command, "unknown command");
            Ok(())
        }
    }

    /// Caudal patch: called when the client sends `play` on `stream_id`.
    /// `stream_name` is the first argument of the command (the stream key,
    /// query string included). The default refuses, as upstream does.
    fn on_play(
        &mut self,
        stream_id: u32,
        app_name: &str,
        stream_name: &str,
    ) -> impl std::future::Future<Output = Result<PlayResponse, ServerSessionError>> + Send {
        let _ = (stream_id, app_name, stream_name);
        async move { Err(ServerSessionError::PlayNotSupported) }
    }

    /// Caudal patch: the next message for the stream being played. Polled
    /// only after [`SessionHandler::on_play`] returned
    /// [`PlayResponse::Start`], next to the socket read, so it must be
    /// cancel-safe. The session writes and flushes each message before
    /// asking for the next one: a client that reads slowly slows this down
    /// rather than growing a buffer.
    fn next_play_message(&mut self) -> impl std::future::Future<Output = PlayMessage> + Send {
        std::future::pending()
    }

    /// Called when data is received.
    fn on_data(
        &mut self,
        stream_id: u32,
        data: SessionData,
    ) -> impl std::future::Future<Output = Result<(), ServerSessionError>> + Send;
}
