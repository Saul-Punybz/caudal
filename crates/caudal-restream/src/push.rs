//! Runs one [`crate::RestreamTarget`] forever: waits for its source stream
//! to be published, connects to the target, pushes frames until the source
//! ends or the connection drops, and reconnects with backoff while the
//! source stays live. One task per target, spawned by [`crate::start`].
//! Mirrors `crates/caudal-srt/src/push.rs`'s publish-wait/backoff shape.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use caudal_core::{Event, Registry, StartAt, Stream, TrackId, TrackInfo};
use tokio::sync::broadcast::error::RecvError;

use crate::client::RtmpClient;
use crate::status::TargetStatus;
use crate::url::parse_target_url;
use crate::{flv, RestreamTarget};

const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) async fn run(target: RestreamTarget, registry: Arc<Registry>, status: Arc<TargetStatus>) {
    loop {
        status.set_waiting();
        let stream = match registry.get(&target.stream) {
            Some(s) if !s.is_ended() => s,
            _ => match wait_for_publish(&registry, &target.stream).await {
                Some(s) => s,
                None => return, // the registry is gone: shutting down.
            },
        };
        run_while_live(&target, &stream, &status).await;
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

/// Connects and pushes while `stream` stays live, reconnecting with
/// exponential backoff between attempts.
async fn run_while_live(target: &RestreamTarget, stream: &Arc<Stream>, status: &Arc<TargetStatus>) {
    let parsed = match parse_target_url(&target.url) {
        Ok(p) => p,
        Err(err) => {
            // A malformed target URL never becomes valid on retry, but we
            // still back off at the cap rather than hot-loop or give up
            // (the config could be fixed and the process not restarted).
            status.set_retrying(err);
            tokio::time::sleep(BACKOFF_MAX).await;
            return;
        }
    };

    let mut backoff = BACKOFF_MIN;
    while !stream.is_ended() {
        status.set_connecting();
        match RtmpClient::connect(&parsed, CONNECT_TIMEOUT).await {
            Ok(mut client) => {
                status.set_live();
                let sent_any = push_frames(&mut client, stream, status).await;
                if stream.is_ended() {
                    return;
                }
                status.set_retrying("connection to the target ended");
                backoff = if sent_any { BACKOFF_MIN } else { (backoff * 2).min(BACKOFF_MAX) };
            }
            Err(err) => {
                status.set_retrying(err);
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
        tokio::time::sleep(backoff).await;
    }
}

/// Converts a track-timescale timestamp to RTMP milliseconds. Wrapping to
/// `u32` matches `RtmpTimestamp`'s own wraparound semantics.
fn to_rtmp_ms(info: &TrackInfo, ts: i64) -> u32 {
    (info.to_micros(ts) / 1000) as i64 as u32
}

/// Subscribes to `stream` (not counted as a viewer: this is an outbound
/// relay, not our own audience) and pushes frames to `client` until the
/// stream ends or the connection fails. Returns whether any bytes were
/// sent, so the caller can tell a real hiccup from an instantly-failing
/// target.
async fn push_frames(client: &mut RtmpClient, stream: &Arc<Stream>, status: &Arc<TargetStatus>) -> bool {
    let mut sub = stream.subscribe_internal(StartAt::LiveEdge);
    let mut tracks = sub.tracks();
    let mut headers_sent: HashSet<TrackId> = HashSet::new();
    let mut warned_unsupported: HashSet<TrackId> = HashSet::new();
    let mut sent_any = false;

    loop {
        tokio::select! {
            event = sub.recv() => {
                match event {
                    Event::TracksChanged => {
                        tracks = sub.tracks();
                        headers_sent.clear();
                    }
                    Event::Frame(frame) => {
                        let Some(info) = tracks.iter().find(|t| t.id == frame.track).cloned() else { continue };
                        if !flv::supported(info.codec) {
                            if warned_unsupported.insert(info.id) {
                                tracing::warn!(
                                    stream = %stream.name(),
                                    codec = info.codec.as_str(),
                                    "restream: codec has no RTMP encoding; skipping track",
                                );
                            }
                            continue;
                        }

                        if headers_sent.insert(info.id) {
                            if let Some(header) = flv::sequence_header(&info) {
                                let ts = to_rtmp_ms(&info, frame.dts);
                                match send(client, &info, header, ts).await {
                                    Ok(n) => { status.add_bytes(n); sent_any = true; }
                                    Err(err) => { status.set_retrying(err); return sent_any; }
                                }
                            }
                        }

                        let cts_ms = ((info.to_micros(frame.pts) - info.to_micros(frame.dts)) / 1000) as i32;
                        if let Some(tag) = flv::frame_tag(&info, &frame, cts_ms) {
                            let ts = to_rtmp_ms(&info, frame.dts);
                            match send(client, &info, tag, ts).await {
                                Ok(n) => { status.add_bytes(n); sent_any = true; }
                                Err(err) => { status.set_retrying(err); return sent_any; }
                            }
                        }
                    }
                    Event::Lagged { .. } => {}
                    Event::End => return sent_any,
                }
            }
            pumped = client.pump() => {
                if let Err(err) = pumped {
                    status.set_retrying(err);
                    return sent_any;
                }
            }
        }
    }
}

async fn send(client: &mut RtmpClient, info: &TrackInfo, tag: bytes::Bytes, ts_ms: u32) -> Result<u64, String> {
    match info.kind() {
        caudal_core::TrackKind::Video => client.publish_video(tag, ts_ms).await,
        caudal_core::TrackKind::Audio => client.publish_audio(tag, ts_ms).await,
        _ => Ok(0),
    }
}
