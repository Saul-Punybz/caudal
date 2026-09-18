//! Pushes each configured [`crate::SrtPush`] out to a remote SRT listener,
//! caller-side: while `stream` is live, connects to `url` and sends it as
//! MPEG-TS, reconnecting with backoff on failure; when the stream is not
//! live, waits for it to be (re)published. One task per push, spawned by
//! `crate::serve`. See `NOTES.md`.

use std::sync::Arc;
use std::time::Duration;

use caudal_core::{Event, Registry, StartAt, Stream};
use rsrt::{SrtOptions, SrtSocket};
use tokio::sync::broadcast::error::RecvError;

use crate::SrtPush;
use crate::mux::TsMux;

/// rsrt's live-mode chunk size: 7 TS packets (188 bytes each).
const CHUNK_BYTES: usize = 7 * 188;
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Runs one push forever: waits for `push.stream` to be published, sends it
/// to `push.url` until the stream ends, then waits for the next publish.
/// Ends only when its task is dropped (the listener shutting down).
pub(crate) async fn run(push: SrtPush, registry: Arc<Registry>) {
    loop {
        let stream = match registry.get(&push.stream) {
            Some(s) if !s.is_ended() => s,
            _ => match wait_for_publish(&registry, &push.stream).await {
                Some(s) => s,
                None => continue,
            },
        };
        send_until_ended(&push, &stream).await;
    }
}

/// Waits for `name` to (re)appear in the registry via the publish
/// broadcast, so this never busy-polls.
async fn wait_for_publish(registry: &Arc<Registry>, name: &str) -> Option<Arc<Stream>> {
    let mut publishes = registry.subscribe_publishes();
    // A publish may have landed between the caller's check and this
    // subscription; check once more before waiting on the broadcast.
    if let Some(s) = registry.get(name) {
        if !s.is_ended() {
            return Some(s);
        }
    }
    loop {
        match publishes.recv().await {
            Ok(stream) if stream.name() == name => return Some(stream),
            Ok(_) => continue,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return None,
        }
    }
}

/// Connects to `push.url` and sends `stream` as MPEG-TS until it ends,
/// reconnecting on failure with exponential backoff (capped at
/// [`BACKOFF_MAX`], reset once a session actually sends data).
async fn send_until_ended(push: &SrtPush, stream: &Arc<Stream>) {
    let mut backoff = BACKOFF_MIN;
    while !stream.is_ended() {
        match connect(&push.url).await {
            Ok(socket) => {
                tracing::info!(url = %push.url, stream = %push.stream, "srt push connected");
                let sent_any = run_push_session(socket, stream).await;
                if stream.is_ended() {
                    return;
                }
                tracing::info!(url = %push.url, stream = %push.stream, "srt push connection ended; reconnecting");
                backoff = if sent_any { BACKOFF_MIN } else { (backoff * 2).min(BACKOFF_MAX) };
            }
            Err(err) => {
                tracing::warn!(url = %push.url, stream = %push.stream, %err, "srt push connect failed");
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
        tokio::time::sleep(backoff).await;
    }
}

/// Sends `stream` over an established caller `socket` until it ends or the
/// connection fails. Returns whether any bytes were sent, so the caller can
/// tell a real network hiccup from an instantly-failing destination.
async fn run_push_session(socket: SrtSocket, stream: &Arc<Stream>) -> bool {
    // Not counted as a viewer: this is an outbound relay, not our own
    // audience.
    let mut sub = stream.subscribe_internal(StartAt::LiveEdge);
    let mut tracks = sub.tracks();
    let mut mux = TsMux::new();
    mux.set_tracks(&tracks);
    let mut sent_any = false;
    loop {
        match sub.recv().await {
            Event::TracksChanged => {
                tracks = sub.tracks();
                mux.set_tracks(&tracks);
            }
            Event::Frame(frame) => {
                if let Some(info) = tracks.iter().find(|t| t.id == frame.track) {
                    mux.push_frame(info, &frame);
                }
            }
            Event::Lagged { .. } => {}
            Event::End => return sent_any,
        }
        let out = mux.take_output();
        if out.is_empty() {
            continue;
        }
        for chunk in out.chunks(CHUNK_BYTES) {
            if socket.send(chunk).await.is_err() {
                return sent_any;
            }
            sent_any = true;
        }
    }
}

/// Parses `srt://host:port[?streamid=&passphrase=&latency=]` and connects
/// as a caller.
async fn connect(url: &str) -> Result<SrtSocket, String> {
    let (addr, opts) = parse_url(url).ok_or_else(|| format!("invalid SRT URL: {url}"))?;
    SrtSocket::connect(addr, opts).await.map_err(|err| err.to_string())
}

fn parse_url(url: &str) -> Option<(String, SrtOptions)> {
    let rest = url.strip_prefix("srt://")?;
    let (authority, query) = rest.split_once('?').unwrap_or((rest, ""));
    if authority.is_empty() {
        return None;
    }
    let mut opts = SrtOptions::default();
    for kv in query.split('&').filter(|s| !s.is_empty()) {
        let (key, value) = kv.split_once('=').unwrap_or((kv, ""));
        let value = urldecode(value);
        match key {
            "streamid" => opts = opts.streamid(value),
            "passphrase" => opts = opts.passphrase(value),
            "latency" => {
                if let Ok(ms) = value.parse::<u64>() {
                    opts = opts.latency(Duration::from_millis(ms));
                }
            }
            _ => {}
        }
    }
    Some((authority.to_owned(), opts))
}

/// Minimal `%XX` decoding for query values (a `streamid` commonly carries
/// `/` and other reserved characters URL-encoded).
fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_host_port_and_query() {
        let (addr, _opts) = parse_url("srt://example.com:9000?streamid=play%2Ftest&latency=250").unwrap();
        assert_eq!(addr, "example.com:9000");
    }

    #[test]
    fn rejects_non_srt_scheme() {
        assert!(parse_url("http://example.com:9000").is_none());
        assert!(parse_url("srt://").is_none());
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(urldecode("play%2Ftest"), "play/test");
        assert_eq!(urldecode("plain"), "plain");
        assert_eq!(urldecode("trailing%2"), "trailing%2");
    }
}
