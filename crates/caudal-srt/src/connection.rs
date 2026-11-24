//! One accepted SRT connection: validates the stream id, claims a
//! `Publisher`, and turns MPEG-TS payloads into `caudal_core::Frame`s.
//!
//! The TS → frames → publisher part is `caudal_ts::ingest::TsIngest`, shared
//! with `[[srt.pull]]` and `[[udp.pull]]`.

use std::net::SocketAddrV4;
use std::sync::Arc;

use caudal_core::{BufferConfig, PublishError, Registry};
use caudal_ts::ingest::TsIngest;
use rsrt::SrtSocket;

/// Extracts the publish target and optional token from an SRT stream id:
/// `publish/<name>` or `publish/<name>?token=<t>`, or the SRT access-control
/// form `#!::r=<name>,m=publish` with an optional `t=<token>`. Any other
/// `m=` value, a missing name, or an id matching neither form is rejected
/// (`None`).
fn parse_publish(streamid: &str) -> Option<(&str, Option<&str>)> {
    if let Some(rest) = streamid.strip_prefix("publish/") {
        let (name, query) = rest.split_once('?').unwrap_or((rest, ""));
        let token = query.split('&').find_map(|kv| kv.strip_prefix("token="));
        return (!name.is_empty()).then_some((name, token.filter(|t| !t.is_empty())));
    }
    if let Some(rest) = streamid.strip_prefix("#!::") {
        let mut mode: Option<&str> = None;
        let mut name: Option<&str> = None;
        let mut token: Option<&str> = None;
        for field in rest.split(',') {
            let (key, value) = field.split_once('=')?;
            match key {
                "m" => mode = Some(value),
                "r" => name = Some(value),
                "t" => token = Some(value),
                _ => {}
            }
        }
        if mode == Some("publish") {
            return name.filter(|n| !n.is_empty()).map(|n| (n, token.filter(|t| !t.is_empty())));
        }
        return None;
    }
    None
}

/// Extracts the play target and optional token from an SRT stream id:
/// `play/<name>` or `play/<name>?token=<t>`, or the SRT access-control form
/// `#!::r=<name>,m=request` with an optional `t=<token>`. Mirrors
/// [`parse_publish`], just for `m=request` instead of `m=publish`.
fn parse_play(streamid: &str) -> Option<(&str, Option<&str>)> {
    if let Some(rest) = streamid.strip_prefix("play/") {
        let (name, query) = rest.split_once('?').unwrap_or((rest, ""));
        let token = query.split('&').find_map(|kv| kv.strip_prefix("token="));
        return (!name.is_empty()).then_some((name, token.filter(|t| !t.is_empty())));
    }
    if let Some(rest) = streamid.strip_prefix("#!::") {
        let mut mode: Option<&str> = None;
        let mut name: Option<&str> = None;
        let mut token: Option<&str> = None;
        for field in rest.split(',') {
            let (key, value) = field.split_once('=')?;
            match key {
                "m" => mode = Some(value),
                "r" => name = Some(value),
                "t" => token = Some(value),
                _ => {}
            }
        }
        if mode == Some("request") {
            return name.filter(|n| !n.is_empty()).map(|n| (n, token.filter(|t| !t.is_empty())));
        }
        return None;
    }
    None
}

/// Dispatches one accepted SRT connection by stream-id mode: `publish/...`
/// or `#!::...,m=publish` goes to [`handle_publish`]; `play/...` or
/// `#!::...,m=request` goes to [`crate::play::handle`]. Anything else is
/// rejected. Never panics: a malformed stream or a rejected caller ends
/// this connection only, never the listener.
pub(crate) async fn handle(socket: SrtSocket, peer: SocketAddrV4, registry: Arc<Registry>, buffer: BufferConfig) {
    let streamid = socket.streamid().unwrap_or_default();
    if let Some((name, token)) = parse_publish(&streamid) {
        handle_publish(socket, peer, registry, buffer, name, token).await;
        return;
    }
    if let Some((name, token)) = parse_play(&streamid) {
        crate::play::handle(socket, peer, registry, name, token).await;
        return;
    }
    tracing::debug!(%peer, %streamid, "srt rejected: unrecognized stream id");
}

/// Handles one accepted `publish/...` connection to completion.
async fn handle_publish(
    mut socket: SrtSocket,
    peer: SocketAddrV4,
    registry: Arc<Registry>,
    buffer: BufferConfig,
    name: &str,
    token: Option<&str>,
) {
    let name = match registry
        .resolve(caudal_core::Access::Publish, "srt", name, token, Some(std::net::IpAddr::V4(*peer.ip())))
        .await
    {
        Ok(n) => n,
        Err(denied) => {
            let stream = registry.loggable(name);
            tracing::info!(%peer, %stream, reason = ?denied, "srt publish rejected");
            return;
        }
    };
    let name = name.as_str();

    let publisher = match registry.publish(name, buffer) {
        Ok(p) => p,
        Err(PublishError::Busy(name)) => {
            tracing::warn!(%peer, %name, "srt rejected: stream already publishing");
            return;
        }
        Err(PublishError::InvalidName) => {
            tracing::warn!(%peer, name, "srt rejected: invalid stream name");
            return;
        }
    };
    tracing::info!(%peer, stream = %publisher.stream().name(), "srt publish started");
    let mut ingest = TsIngest::new(publisher);
    loop {
        match socket.recv().await {
            Ok(Some(payload)) => ingest.feed(&payload),
            Ok(None) => {
                tracing::debug!(%peer, "srt stream ended cleanly");
                break;
            }
            Err(err) => {
                tracing::debug!(%peer, %err, "srt connection ended");
                break;
            }
        }
    }
    // Dropping `ingest` here drops its `Publisher`, ending the stream.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_form_accepted() {
        assert_eq!(parse_publish("publish/test"), Some(("test", None)));
        assert_eq!(parse_publish("publish/test?token=abc"), Some(("test", Some("abc"))));
        assert_eq!(parse_publish("#!::r=test,m=publish,t=abc"), Some(("test", Some("abc"))));
        assert_eq!(parse_publish("publish/"), None);
    }

    #[test]
    fn access_control_form_accepted() {
        assert_eq!(parse_publish("#!::r=test,m=publish"), Some(("test", None)));
        assert_eq!(parse_publish("#!::m=publish,r=test"), Some(("test", None)));
        assert_eq!(parse_publish("#!::u=alice,r=test,m=publish"), Some(("test", None)));
    }

    #[test]
    fn play_and_other_modes_rejected() {
        assert_eq!(parse_publish("play/test"), None);
        assert_eq!(parse_publish("#!::r=test,m=request"), None);
        assert_eq!(parse_publish("#!::m=publish"), None);
        assert_eq!(parse_publish(""), None);
        assert_eq!(parse_publish("garbage"), None);
    }

    #[test]
    fn play_form_accepted() {
        assert_eq!(parse_play("play/test"), Some(("test", None)));
        assert_eq!(parse_play("play/test?token=abc"), Some(("test", Some("abc"))));
        assert_eq!(parse_play("#!::r=test,m=request,t=abc"), Some(("test", Some("abc"))));
        assert_eq!(parse_play("play/"), None);
    }

    #[test]
    fn publish_and_other_modes_rejected_for_play() {
        assert_eq!(parse_play("publish/test"), None);
        assert_eq!(parse_play("#!::r=test,m=publish"), None);
        assert_eq!(parse_play("#!::m=request"), None);
        assert_eq!(parse_play(""), None);
        assert_eq!(parse_play("garbage"), None);
    }
}
