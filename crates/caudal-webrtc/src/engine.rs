//! The peer loops. All peers share one UDP socket; `threads` engines each
//! own a set of peers (their `Rtc`s), so SRTP and packetization run on
//! several cores. One receive task reads the socket and hands each
//! datagram to the engine that owns its session: STUN binding requests
//! carry the server's ICE ufrag in USERNAME (the server is ICE-lite, so
//! every session starts with one), which names the engine; after that the
//! source address does.
//!
//! Inside an engine, incoming datagrams go to the peer whose `Rtc::accepts` them (the last
//! peer seen at that source address is tried first). Each peer's
//! `poll_output` is drained after every input; its next timeout is kept and
//! the loop sleeps until the earliest one. HTTP handlers and WHEP
//! forwarders talk to the loop over channels.
//!
//! The loop never waits on the socket. Sends use `try_send_to`; whatever
//! the kernel refuses waits in an [`Outbox`] flushed when the socket turns
//! writable, so one full send buffer can't stall reads, commands or other
//! peers. After every wake-up the loop drains a burst of incoming
//! datagrams, so STUN consent and RTCP are read even while media is busy.
//! Benchmark 18 Sep 2026 (`docs/research/BENCH-MEDIAMTX.md`): the old loop
//! awaited every `send_to` behind a `biased` select that served media
//! before the socket; at 300 WHEP viewers it delivered 245 Mbps on 47 % of
//! one core while the kernel dropped 12,700 datagrams.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use str0m::media::{KeyframeRequestKind, MediaKind, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Event, IceConnectionState, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

use crate::egress::{self, Egress, Out};
use crate::ingest::Ingest;
use crate::net::Destinations;

/// A peer with no media (WHIP) or no datagrams (WHEP) for this long is gone.
const IDLE: Duration = Duration::from_secs(30);
const PLI_EVERY: Duration = Duration::from_millis(500);
const MEDIA_QUEUE: usize = 4096;
/// Datagrams held while the kernel's send buffer is full. Past this the
/// newest are dropped (RTP recovers through NACK/PLI; blocking would stall
/// every peer).
const OUTBOX_MAX: usize = 65_536;
/// Queued frames handled per loop turn.
const MEDIA_BURST: usize = 2048;
/// Incoming datagrams read per loop turn before other work runs again.
const RECV_BURST: usize = 256;

/// Datagrams the socket would not take yet, in send order.
#[derive(Default)]
struct Outbox {
    queue: VecDeque<(Vec<u8>, SocketAddr)>,
    dropped: u64,
}

impl Outbox {
    fn send(&mut self, socket: &UdpSocket, data: Vec<u8>, to: SocketAddr) {
        // Only bypass the queue when it is empty, so order is kept.
        if self.queue.is_empty() {
            match socket.try_send_to(&data, to) {
                Ok(_) => return,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => {
                    tracing::debug!(error = %e, to = %to, "webrtc: udp send failed");
                    return;
                }
            }
        }
        if self.queue.len() >= OUTBOX_MAX {
            self.dropped += 1;
            if self.dropped.is_power_of_two() {
                tracing::warn!(dropped = self.dropped, "webrtc: udp send queue full; dropping datagrams");
            }
            return;
        }
        self.queue.push_back((data, to));
    }

    /// Sends queued datagrams until the socket refuses one.
    fn flush(&mut self, socket: &UdpSocket) {
        while let Some((data, to)) = self.queue.front() {
            match socket.try_send_to(data, *to) {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) => tracing::debug!(error = %e, to = %to, "webrtc: udp send failed"),
            }
            self.queue.pop_front();
        }
    }
}

/// Which engine owns each session, by the server's ICE ufrag.
pub(crate) type Ufrags = Arc<Mutex<HashMap<String, usize>>>;

/// A datagram and its sender, from the receive task to an engine.
pub(crate) type Inbound = (Vec<u8>, SocketAddr);
/// Datagrams queued per engine before the receive task drops them.
pub(crate) const INBOUND_QUEUE: usize = 8192;

/// The server's ufrag in a STUN binding request (`USERNAME` =
/// `server:client`, RFC 8445 7.2.2), or `None` for anything else.
pub(crate) fn stun_server_ufrag(d: &[u8]) -> Option<&str> {
    if d.len() < 20 || d[0] & 0xc0 != 0 || d[0..2] != [0x00, 0x01] || d[4..8] != [0x21, 0x12, 0xa4, 0x42] {
        return None;
    }
    let end = (20 + u16::from_be_bytes([d[2], d[3]]) as usize).min(d.len());
    let mut i = 20;
    while i + 4 <= end {
        let kind = u16::from_be_bytes([d[i], d[i + 1]]);
        let len = u16::from_be_bytes([d[i + 2], d[i + 3]]) as usize;
        let value = d.get(i + 4..i + 4 + len)?;
        if kind == 0x0006 {
            let user = std::str::from_utf8(value).ok()?;
            return user.split(':').next().filter(|u| !u.is_empty());
        }
        i += 4 + len.div_ceil(4) * 4;
    }
    None
}

/// Reads the shared socket and hands each datagram to its engine.
pub(crate) async fn receive(socket: Arc<UdpSocket>, engines: Vec<mpsc::Sender<Inbound>>, ufrags: Ufrags) {
    let mut by_source: HashMap<SocketAddr, usize> = HashMap::new();
    let mut buf = vec![0u8; 2048];
    let mut dropped: u64 = 0;
    loop {
        let (n, source) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            // ICMP port unreachable and friends surface here; they concern
            // one remote, never the socket as a whole.
            Err(e) => {
                tracing::debug!(error = %e, "webrtc: udp recv error");
                continue;
            }
        };
        let data = &buf[..n];
        let from_stun = stun_server_ufrag(data).and_then(|u| ufrags.lock().ok()?.get(u).copied());
        let engine = match from_stun {
            Some(e) => {
                if by_source.len() >= 65_536 {
                    by_source.clear();
                }
                by_source.insert(source, e);
                Some(e)
            }
            None => by_source.get(&source).copied(),
        };
        let Some(tx) = engine.and_then(|e| engines.get(e)) else { continue };
        match tx.try_send((data.to_vec(), source)) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                dropped += 1;
                if dropped.is_power_of_two() {
                    tracing::warn!(dropped, "webrtc: engine inbound queue full; dropping datagrams");
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

/// Where each incoming datagram goes.
struct Router {
    dest: Destinations,
    by_source: HashMap<SocketAddr, u64>,
}

pub(crate) enum Role {
    Whip(Box<Ingest>),
    Whep(Box<Egress>),
}

pub(crate) struct Peer {
    pub(crate) session: String,
    pub(crate) name: Arc<str>,
    pub(crate) rtc: Rtc,
    pub(crate) role: Role,
}

pub(crate) enum Cmd {
    Add(Box<Peer>),
    /// Ends session `session` of stream `name`; replies whether it existed.
    Delete {
        whip: bool,
        name: String,
        session: String,
        reply: oneshot::Sender<bool>,
    },
}

struct Slot {
    peer: Peer,
    /// The server's ICE ufrag for this session (key in [`Ufrags`]).
    ufrag: String,
    deadline: Instant,
    last_activity: Instant,
    video_mid: Option<Mid>,
    last_pli: Option<Instant>,
}

pub(crate) async fn run(
    socket: Arc<UdpSocket>,
    dest: Destinations,
    mut cmds: mpsc::Receiver<Cmd>,
    mut inbound: mpsc::Receiver<Inbound>,
    ufrags: Ufrags,
) {
    let (media_tx, mut media_rx) = mpsc::channel::<(u64, Out)>(MEDIA_QUEUE);
    let mut slots: HashMap<u64, Slot> = HashMap::new();
    let mut router = Router { dest, by_source: HashMap::new() };
    let mut outbox = Outbox::default();
    let mut queue_full_seen = 0u64;
    let mut bframes_warned: HashSet<Arc<str>> = HashSet::new();
    let mut next_id: u64 = 0;
    let mut last_sweep = Instant::now();

    loop {
        let now = Instant::now();
        let wake = slots.values().map(|s| s.deadline).min().unwrap_or(now + Duration::from_secs(1));
        let wake = wake.min(now + Duration::from_millis(250)).max(now);

        // Unbiased: under load no branch may starve another.
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                None => break,
                Some(Cmd::Add(peer)) => {
                    let id = next_id;
                    next_id += 1;
                    let mut peer = *peer;
                    if let Role::Whep(e) = &mut peer.role
                        && let Some(sub) = e.sub.take() {
                            let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
                            e.ctl = Some(ctl_tx);
                            let task = tokio::spawn(egress::forward(id, sub, media_tx.clone(), ctl_rx));
                            e.task = Some(task.abort_handle());
                        }
                    tracing::info!(stream = %peer.name, session = %peer.session, kind = kind(&peer), "webrtc: session created");
                    let now = Instant::now();
                    let ufrag = peer.rtc.direct_api().local_ice_credentials().ufrag;
                    slots.insert(id, Slot { peer, ufrag, deadline: now, last_activity: now, video_mid: None, last_pli: None });
                    drive(&socket, &mut outbox, id, &mut slots);
                }
                Some(Cmd::Delete { whip, name, session, reply }) => {
                    let found = slots.iter().find(|(_, s)| {
                        s.peer.session == session && *s.peer.name == *name && matches!(s.peer.role, Role::Whip(_)) == whip
                    }).map(|(id, _)| *id);
                    if let Some(id) = found {
                        remove(id, &mut slots, &ufrags, "deleted");
                    }
                    let _ = reply.send(found.is_some());
                }
            },
            m = media_rx.recv() => {
                // `media_tx` is held here, so the channel never closes.
                let Some((id, out)) = m else { continue };
                on_media(id, out, &socket, &mut outbox, &mut slots, &mut bframes_warned);
                // Take what else is queued in the same turn: the per-turn
                // scans below cost O(peers), so one turn per frame made the
                // loop quadratic (300 viewers: 80 % of a core, queue full).
                for _ in 0..MEDIA_BURST {
                    let Ok((id, out)) = media_rx.try_recv() else { break };
                    on_media(id, out, &socket, &mut outbox, &mut slots, &mut bframes_warned);
                }
            }
            r = socket.writable(), if !outbox.queue.is_empty() => {
                if r.is_ok() {
                    outbox.flush(&socket);
                }
            }
            d = inbound.recv() => match d {
                Some((data, source)) => on_datagram(&data, source, &socket, &mut outbox, &mut router, &mut slots),
                None => break,
            },
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(wake)) => {}
        }

        // Read what else arrived, without waiting.
        for _ in 0..RECV_BURST {
            let Ok((data, source)) = inbound.try_recv() else { break };
            on_datagram(&data, source, &socket, &mut outbox, &mut router, &mut slots);
        }

        // Drive every peer whose timeout passed.
        let now = Instant::now();
        let due: Vec<u64> = slots.iter().filter(|(_, s)| s.deadline <= now).map(|(id, _)| *id).collect();
        for id in due {
            if let Some(slot) = slots.get_mut(&id)
                && let Err(e) = slot.peer.rtc.handle_input(Input::Timeout(now))
            {
                tracing::debug!(session = %slot.peer.session, error = %e, "webrtc: timeout error");
                slot.peer.rtc.disconnect();
            }
            drive(&socket, &mut outbox, id, &mut slots);
        }

        let closed: Vec<u64> = slots.iter().filter(|(_, s)| !s.peer.rtc.is_alive()).map(|(id, _)| *id).collect();
        for id in closed {
            remove(id, &mut slots, &ufrags, "closed");
        }

        if now.duration_since(last_sweep) >= Duration::from_millis(250) {
            last_sweep = now;
            let mut dead = Vec::new();
            for (id, s) in slots.iter_mut() {
                if now.duration_since(s.last_activity) > IDLE {
                    dead.push((*id, "idle"));
                } else if let Role::Whip(ing) = &mut s.peer.role {
                    // A track that never arrived: announce what did.
                    if let Err(e) = ing.maybe_announce(now) {
                        tracing::info!(stream = %s.peer.name, error = %e, "whip: stream closed under us");
                        s.peer.rtc.disconnect();
                    }
                    // Ask for a keyframe at start and after loss.
                    if ing.wants_keyframe()
                        && s.last_pli.is_none_or(|t| now.duration_since(t) >= PLI_EVERY)
                        && let Some(mid) = s.video_mid
                        && let Some(mut w) = s.peer.rtc.writer(mid)
                        && w.request_keyframe(None, KeyframeRequestKind::Pli).is_ok()
                    {
                        s.last_pli = Some(now);
                    }
                }
            }
            for (id, why) in dead {
                remove(id, &mut slots, &ufrags, why);
            }
            router.by_source.retain(|_, v| slots.contains_key(v));
            let full = egress::QUEUE_FULL.load(std::sync::atomic::Ordering::Relaxed);
            if full != queue_full_seen {
                tracing::warn!(
                    total = full,
                    outbox = outbox.queue.len(),
                    peers = slots.len(),
                    "webrtc: media queue full; viewers skipped to the next keyframe"
                );
                queue_full_seen = full;
            }
        }
    }
}

fn kind(p: &Peer) -> &'static str {
    match p.role {
        Role::Whip(_) => "whip",
        Role::Whep(_) => "whep",
    }
}

fn remove(id: u64, slots: &mut HashMap<u64, Slot>, ufrags: &Ufrags, why: &str) {
    if let Some(mut s) = slots.remove(&id) {
        if let Ok(mut u) = ufrags.lock() {
            u.remove(&s.ufrag);
        }
        s.peer.rtc.disconnect();
        tracing::info!(stream = %s.peer.name, session = %s.peer.session, kind = kind(&s.peer), reason = why, "webrtc: session closed");
        // Dropping the peer drops its Publisher (ends the stream) or aborts
        // its forwarder.
    }
}

/// Writes one forwarded frame (or track change / end) into its WHEP peer.
fn on_media(
    id: u64,
    out: Out,
    socket: &UdpSocket,
    outbox: &mut Outbox,
    slots: &mut HashMap<u64, Slot>,
    bframes_warned: &mut HashSet<Arc<str>>,
) {
    let Some(slot) = slots.get_mut(&id) else { return };
    if let Role::Whep(e) = &mut slot.peer.role {
        match out {
            Out::Tracks(t) => e.set_tracks(&t),
            Out::Frame(f) => e.write(&mut slot.peer.rtc, &f, bframes_warned),
            Out::End => {
                bframes_warned.remove(&slot.peer.name);
                slot.peer.rtc.disconnect();
            }
        }
    }
    drive(socket, outbox, id, slots);
}

/// Hands one datagram to the peer that accepts it and drives that peer.
fn on_datagram(
    data: &[u8],
    source: SocketAddr,
    socket: &UdpSocket,
    outbox: &mut Outbox,
    router: &mut Router,
    slots: &mut HashMap<u64, Slot>,
) {
    let Some(destination) = router.dest.for_source(source) else { return };
    let Ok(recv) = Receive::new(Protocol::Udp, source, destination, data) else { return };
    let now = Instant::now();
    let input = Input::Receive(now, recv);
    let cached =
        router.by_source.get(&source).copied().filter(|id| slots.get(id).is_some_and(|s| s.peer.rtc.accepts(&input)));
    let id = cached.or_else(|| slots.iter().find(|(_, s)| s.peer.rtc.accepts(&input)).map(|(id, _)| *id));
    let Some(id) = id else { return };
    if cached.is_none() {
        if router.by_source.len() > 65_536 {
            router.by_source.retain(|_, v| slots.contains_key(v));
        }
        router.by_source.insert(source, id);
    }
    let Some(slot) = slots.get_mut(&id) else { return };
    if matches!(slot.peer.role, Role::Whep(_)) {
        slot.last_activity = now;
    }
    if let Err(e) = slot.peer.rtc.handle_input(input) {
        tracing::debug!(session = %slot.peer.session, error = %e, "webrtc: bad input");
        slot.peer.rtc.disconnect();
    }
    drive(socket, outbox, id, slots);
}

/// Drains one peer's output: sends datagrams, handles events, records the
/// next timeout.
fn drive(socket: &UdpSocket, outbox: &mut Outbox, id: u64, slots: &mut HashMap<u64, Slot>) {
    let Some(slot) = slots.get_mut(&id) else { return };
    loop {
        if !slot.peer.rtc.is_alive() {
            slot.deadline = Instant::now() + IDLE;
            return;
        }
        let out = match slot.peer.rtc.poll_output() {
            Ok(o) => o,
            Err(e) => {
                tracing::debug!(session = %slot.peer.session, error = %e, "webrtc: peer failed");
                slot.peer.rtc.disconnect();
                slot.deadline = Instant::now() + IDLE;
                return;
            }
        };
        match out {
            Output::Timeout(t) => {
                slot.deadline = t;
                return;
            }
            Output::Transmit(t) => outbox.send(socket, t.contents.into(), t.destination),
            Output::Event(ev) => on_event(slot, ev),
        }
    }
}

fn on_event(slot: &mut Slot, ev: Event) {
    let now = Instant::now();
    match ev {
        Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
            tracing::debug!(session = %slot.peer.session, "webrtc: ICE disconnected");
            slot.peer.rtc.disconnect();
        }
        Event::Connected => {
            tracing::debug!(session = %slot.peer.session, "webrtc: connected");
            if let Role::Whep(e) = &mut slot.peer.role {
                e.connected();
            }
        }
        Event::MediaAdded(m) => match &mut slot.peer.role {
            Role::Whip(ing) => match m.kind {
                MediaKind::Video => {
                    ing.expect_video = true;
                    slot.video_mid.get_or_insert(m.mid);
                }
                MediaKind::Audio => ing.expect_audio = true,
            },
            Role::Whep(e) => e.media_added(m.mid, m.kind),
        },
        Event::MediaData(data) => {
            if let Role::Whip(ing) = &mut slot.peer.role {
                slot.last_activity = now;
                if let Err(e) = ing.on_media(&data, now) {
                    tracing::info!(stream = %slot.peer.name, error = %e, "whip: stream closed under us");
                    slot.peer.rtc.disconnect();
                }
            }
        }
        Event::KeyframeRequest(_) => {
            if let Role::Whep(e) = &mut slot.peer.role {
                e.keyframe_requested();
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::stun_server_ufrag;

    /// A STUN binding request with the given attributes (type, value).
    fn stun(attrs: &[(u16, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (kind, value) in attrs {
            body.extend_from_slice(&kind.to_be_bytes());
            body.extend_from_slice(&(value.len() as u16).to_be_bytes());
            body.extend_from_slice(value);
            body.resize(body.len().div_ceil(4) * 4, 0);
        }
        let mut d = vec![0x00, 0x01];
        d.extend_from_slice(&(body.len() as u16).to_be_bytes());
        d.extend_from_slice(&[0x21, 0x12, 0xa4, 0x42]);
        d.extend_from_slice(&[7; 12]);
        d.extend_from_slice(&body);
        d
    }

    #[test]
    fn server_ufrag_from_binding_request() {
        // PRIORITY (0x0024) first, then USERNAME with odd length (padding).
        let d = stun(&[(0x0024, &[0, 0, 0, 1]), (0x0006, b"srvU:cli")]);
        assert_eq!(stun_server_ufrag(&d), Some("srvU"));
    }

    #[test]
    fn not_a_binding_request() {
        let mut d = stun(&[(0x0006, b"srvU:cli")]);
        d[1] = 0x01;
        d[0] = 0x01; // binding success response
        assert_eq!(stun_server_ufrag(&d), None);
        assert_eq!(stun_server_ufrag(&[0x80, 0x60, 0, 1]), None); // RTP
        assert_eq!(stun_server_ufrag(&stun(&[(0x0024, &[0, 0, 0, 1])])), None);
        // Truncated attribute: no panic.
        let mut t = stun(&[(0x0006, b"srvU:cli")]);
        t.truncate(26);
        assert_eq!(stun_server_ufrag(&t), None);
    }
}
