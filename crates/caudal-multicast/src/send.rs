//! Runs one [`crate::MulticastTarget`] forever: waits for its stream to be
//! published, muxes it to MPEG-TS and sends paced datagrams to the group
//! until the stream ends, then waits for the next publish (a republish
//! rejoins on its own). Same publish-wait shape as
//! `crates/caudal-srt/src/push.rs` and `crates/caudal-restream/src/push.rs`;
//! a stream fed by `[[failover]]` stays one live stream across source
//! switches, so the group sees no gap beyond the switch itself.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use caudal_core::{Event, Registry, StartAt, Stream};
use caudal_ts::mux::TsMux;
use tokio::net::UdpSocket;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;

use crate::pacer::Pacer;
use crate::packet::{Chunker, RtpState};
use crate::status::OutputStatus;
use crate::{Format, MulticastTarget};

/// Retry interval while the socket cannot be opened.
const OPEN_RETRY: Duration = Duration::from_secs(5);
/// Queue bound: far above what pacing ever holds (at most `MAX_LAG` of
/// output), so reaching it means the send path itself is stuck.
const MAX_QUEUE_BYTES: usize = 16 << 20;

struct Queued {
    send_at: Instant,
    deadline: Instant,
    datagram: Vec<u8>,
}

pub(crate) async fn run(target: MulticastTarget, registry: Arc<Registry>, status: Arc<OutputStatus>) {
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
    if let Some(s) = registry.get(name)
        && !s.is_ended()
    {
        return Some(s);
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

async fn run_while_live(target: &MulticastTarget, stream: &Arc<Stream>, status: &Arc<OutputStatus>) {
    while !stream.is_ended() {
        match crate::socket::open(target) {
            Ok(socket) => {
                status.set_live();
                tracing::info!(stream = %target.stream, group = %target.group, format = target.format.as_str(), "multicast output started");
                send_session(&socket, target, stream, status).await;
                tracing::info!(stream = %target.stream, group = %target.group, "multicast output stopped: stream ended");
                return;
            }
            Err(err) => {
                tracing::warn!(stream = %target.stream, group = %target.group, %err, "multicast socket failed; retrying");
                status.set_error(err.to_string());
                tokio::time::sleep(OPEN_RETRY).await;
            }
        }
    }
}

/// Subscribes to `stream` (an outbound relay, not counted as a viewer),
/// muxes it and sends paced datagrams until it ends. The last queued
/// datagrams still go out, on schedule, after the end.
async fn send_session(socket: &UdpSocket, target: &MulticastTarget, stream: &Arc<Stream>, status: &OutputStatus) {
    let mut sub = stream.subscribe_internal(StartAt::LiveEdge);
    let mut tracks = sub.tracks();
    let mut mux = TsMux::with_media_clock();
    mux.set_tracks(&tracks);
    let mut chunker = Chunker::default();
    let mut pacer = Pacer::new(target.pacing);
    let mut rtp = (target.format == Format::Rtp).then(|| RtpState::new(Instant::now().into_std()));
    let mut queue: VecDeque<Queued> = VecDeque::new();
    let mut queued_bytes = 0usize;
    let mut media_us = 0i64;
    let mut datagrams = Vec::new();

    loop {
        let next = queue.front().map(|q| q.send_at);
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(next.unwrap_or_else(Instant::now)), if next.is_some() => {
                send_due(socket, target, &mut queue, &mut queued_bytes, rtp.as_mut(), status).await;
                continue;
            }
            event = sub.recv() => match event {
                Event::TracksChanged => {
                    tracks = sub.tracks();
                    mux.set_tracks(&tracks);
                }
                Event::Frame(frame) => {
                    if let Some(info) = tracks.iter().find(|t| t.id == frame.track) {
                        media_us = info.to_micros(frame.dts);
                        mux.push_frame(info, &frame);
                    }
                }
                Event::Cue(cue) => mux.push_cue(&cue),
                Event::Lagged { .. } => {}
                Event::End => break,
            },
        }
        let out = mux.take_output();
        if out.is_empty() {
            continue;
        }
        chunker.push(&out, &mut datagrams);
        let now = Instant::now();
        let deadline = Instant::from_std(pacer.deadline(now.into_std(), media_us));
        pacer.record(media_us, out.len());
        enqueue(&mut pacer, deadline, &mut datagrams, &mut queue, &mut queued_bytes, status);
    }

    chunker.flush(&mut datagrams);
    let deadline = queue.back().map_or_else(Instant::now, |q| q.send_at);
    enqueue(&mut pacer, deadline, &mut datagrams, &mut queue, &mut queued_bytes, status);
    while let Some(front) = queue.front() {
        tokio::time::sleep_until(front.send_at).await;
        send_due(socket, target, &mut queue, &mut queued_bytes, rtp.as_mut(), status).await;
    }
}

fn enqueue(
    pacer: &mut Pacer,
    deadline: Instant,
    datagrams: &mut Vec<Vec<u8>>,
    queue: &mut VecDeque<Queued>,
    queued_bytes: &mut usize,
    status: &OutputStatus,
) {
    for datagram in datagrams.drain(..) {
        if *queued_bytes + datagram.len() > MAX_QUEUE_BYTES {
            // Unreachable while sends complete; never grow without bound.
            status.send_failed(&std::io::Error::other("send queue full; datagram dropped"));
            continue;
        }
        let send_at = Instant::from_std(pacer.schedule(deadline.into_std(), datagram.len()));
        *queued_bytes += datagram.len();
        queue.push_back(Queued { send_at, deadline, datagram });
    }
}

/// Sends every queued datagram whose time has come.
async fn send_due(
    socket: &UdpSocket,
    target: &MulticastTarget,
    queue: &mut VecDeque<Queued>,
    queued_bytes: &mut usize,
    mut rtp: Option<&mut RtpState>,
    status: &OutputStatus,
) {
    let now = Instant::now();
    while queue.front().is_some_and(|q| q.send_at <= now) {
        let q = queue.pop_front().expect("checked");
        *queued_bytes -= q.datagram.len();
        let wrapped;
        let payload: &[u8] = match rtp.as_deref_mut() {
            Some(rtp) => {
                wrapped = rtp.wrap(&q.datagram, q.send_at.into_std());
                &wrapped
            }
            None => &q.datagram,
        };
        match socket.send_to(payload, target.group).await {
            Ok(n) => status.sent(n, Instant::now().saturating_duration_since(q.deadline)),
            Err(err) => {
                if status.send_failed(&err) {
                    tracing::warn!(stream = %target.stream, group = %target.group, %err, "multicast send failed");
                } else {
                    tracing::debug!(stream = %target.stream, group = %target.group, %err, "multicast send failed");
                }
            }
        }
    }
}
