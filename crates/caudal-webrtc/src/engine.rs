//! The peer loops. `threads` engines each own a set of peers (their
//! `Rtc`s) and their own UDP socket, all bound to the same address with
//! `SO_REUSEPORT`, so SRTP, packetization and sends run on several cores.
//! (One socket shared by every engine made them fight over the kernel's
//! per-socket send lock: 8 engines used 320 % CPU for what one did with
//! 40 %.) The kernel may deliver a datagram to any engine's socket; the
//! engine forwards it to the owner: STUN binding requests carry the
//! server's ICE ufrag in USERNAME (the server is ICE-lite, so every session
//! starts with one), which names the engine; after that the source address
//! does.
//!
//! Inside an engine, incoming datagrams go to the peer whose `Rtc::accepts` them (the last
//! peer seen at that source address is tried first). Each peer's
//! `poll_output` is drained after every input; its next timeout is kept and
//! the loop sleeps until the earliest one. HTTP handlers and WHEP
//! forwarders talk to the loop over channels.
//!
//! The loop never waits on the socket. Datagrams are queued in an
//! [`Outbox`] and sent in batches once a peer's output is drained; whatever
//! the kernel refuses waits there until the socket turns writable, so one
//! full send buffer can't stall reads, commands or other peers. After
//! every wake-up the loop drains a burst of incoming datagrams, so STUN
//! consent and RTCP are read even while media is busy.
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
/// Datagrams held while the kernel's send buffer is full; see [`Outbox`].
const OUTBOX_MAX: usize = 65_536;
/// Queued frames handled per loop turn.
const MEDIA_BURST: usize = 2048;
/// Incoming datagrams read per loop turn before other work runs again.
const RECV_BURST: usize = 256;

/// Datagrams queued before a flush in the middle of one peer's output (a
/// keyframe for one viewer can be a few hundred).
const FLUSH_AT: usize = 512;
/// Largest payload handed to the kernel in one segmented send. A UDP
/// datagram carries at most 65,507 bytes over IPv4; Linux also caps a GSO
/// send at 64 segments (`UDP_MAX_SEGMENTS`).
const MAX_SEND_BYTES: usize = 64_000;

/// Outgoing datagrams, in send order, sent in batches.
///
/// A peer's `Output::Transmit`s are queued here while its output is
/// drained, and the queue is flushed right after (or when it reaches
/// [`FLUSH_AT`]).
/// Consecutive datagrams to the same destination with the same size (the
/// last may be shorter) leave in one `sendmsg` with a segment size (UDP GSO,
/// Linux 4.18+) through `quinn-udp`; where the platform has no segmentation
/// (macOS) each datagram is its own `sendmsg`. Profile 19 Sep 2026 (300 WHEP
/// viewers): one `sendto` per ~1.2 KB datagram, about 190,000 a second, was
/// 75 % of the server's CPU. A WHEP frame is many equal-size RTP packets
/// to one viewer, so one send carries a whole frame. Flushing per peer
/// rather than per loop turn gives the same batches (they never span
/// destinations) without holding datagrams back: at 300 viewers a
/// per-turn flush kept the oldest datagram waiting 1 to 5 ms in about a
/// quarter of the turns.
///
/// Whatever the kernel refuses (`WouldBlock`) stays queued until the socket
/// turns writable, so one full send buffer can't stall reads, commands or
/// other peers. Order is kept: nothing is sent while older datagrams wait.
struct Outbox {
    queue: VecDeque<(Vec<u8>, SocketAddr)>,
    state: quinn_udp::UdpSocketState,
    /// Contiguous copy of a segmented send's datagrams.
    scratch: Vec<u8>,
    /// The kernel refused the last send; wait for `writable()`.
    blocked: bool,
    dropped: u64,
}

impl Outbox {
    fn new(socket: &UdpSocket) -> std::io::Result<Self> {
        let state = quinn_udp::UdpSocketState::new(socket.into())?;
        Ok(Self {
            queue: VecDeque::new(),
            state,
            scratch: Vec::with_capacity(MAX_SEND_BYTES),
            blocked: false,
            dropped: 0,
        })
    }

    /// Queues one datagram; sends the queue if it has grown long.
    fn push(&mut self, socket: &UdpSocket, data: Vec<u8>, to: SocketAddr) {
        if self.queue.len() >= OUTBOX_MAX {
            // Past this the newest are dropped (RTP recovers through
            // NACK/PLI; blocking would stall every peer).
            self.dropped += 1;
            if self.dropped.is_power_of_two() {
                tracing::warn!(dropped = self.dropped, "webrtc: udp send queue full; dropping datagrams");
            }
            return;
        }
        self.queue.push_back((data, to));
        if self.queue.len() >= FLUSH_AT {
            self.flush(socket);
        }
    }

    /// Sends queued datagrams until the queue is empty or the socket
    /// refuses one. Does nothing while blocked (see [`Outbox::writable`]).
    fn flush(&mut self, socket: &UdpSocket) {
        while !self.blocked && !self.queue.is_empty() {
            let (n, segment) = self.next_run();
            let (first, to) = &self.queue[0];
            let to = *to;
            let contents: &[u8] = if n == 1 {
                first
            } else {
                self.scratch.clear();
                for (d, _) in self.queue.range(..n) {
                    self.scratch.extend_from_slice(d);
                }
                &self.scratch
            };
            let transmit = quinn_udp::Transmit {
                destination: to,
                ecn: None,
                contents,
                segment_size: (n > 1).then_some(segment),
                src_ip: None,
            };
            // `try_io` clears tokio's write readiness on `WouldBlock`, so
            // `writable()` really waits.
            let state = &self.state;
            match socket.try_io(tokio::io::Interest::WRITABLE, || state.try_send(socket.into(), &transmit)) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    self.blocked = true;
                    return;
                }
                // The kernel or driver refused segmentation: quinn-udp has
                // turned it off; send the same datagrams again one by one.
                Err(_) if n > 1 && self.state.max_gso_segments() < n => continue,
                Err(e) => tracing::debug!(error = %e, to = %to, "webrtc: udp send failed"),
            }
            self.queue.drain(..n);
        }
    }

    /// The socket may take more: resume sending.
    fn writable(&mut self, socket: &UdpSocket) {
        self.blocked = false;
        self.flush(socket);
    }

    /// How many datagrams at the front of the queue can leave in one
    /// segmented send, and their segment size: same destination, same
    /// size, except that the last may be shorter.
    fn next_run(&self) -> (usize, usize) {
        let (first, to) = &self.queue[0];
        let segment = first.len();
        let max = self.state.max_gso_segments();
        let mut n = 1;
        let mut total = segment;
        while n < max {
            let Some((d, t)) = self.queue.get(n) else { break };
            if t != to || d.is_empty() || d.len() > segment || total + d.len() > MAX_SEND_BYTES {
                break;
            }
            total += d.len();
            n += 1;
            if d.len() < segment {
                break;
            }
        }
        (n, segment)
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

/// Which engine owns each datagram, shared by every engine.
#[derive(Clone)]
pub(crate) struct Routes {
    pub(crate) ufrags: Ufrags,
    sources: Arc<Mutex<HashMap<SocketAddr, usize>>>,
    engines: Arc<Vec<mpsc::Sender<Inbound>>>,
}

impl Routes {
    pub(crate) fn new(ufrags: Ufrags, engines: Vec<mpsc::Sender<Inbound>>) -> Self {
        Self { ufrags, sources: Arc::default(), engines: Arc::new(engines) }
    }

    /// The engine that owns a datagram from `source`, learning the source
    /// from a STUN request that names a session's ufrag.
    fn owner(&self, data: &[u8], source: SocketAddr) -> Option<usize> {
        let from_stun = stun_server_ufrag(data).and_then(|u| self.ufrags.lock().ok()?.get(u).copied());
        let mut sources = self.sources.lock().ok()?;
        match from_stun {
            Some(e) => {
                if sources.len() >= 65_536 {
                    sources.clear();
                }
                sources.insert(source, e);
                Some(e)
            }
            None => sources.get(&source).copied(),
        }
    }

    /// Hands a datagram to another engine; dropped if its queue is full.
    fn forward(&self, engine: usize, data: &[u8], source: SocketAddr) {
        let Some(tx) = self.engines.get(engine) else { return };
        if let Err(mpsc::error::TrySendError::Full(_)) = tx.try_send((data.to_vec(), source)) {
            tracing::debug!(engine, "webrtc: engine inbound queue full; dropping a datagram");
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
    me: usize,
    socket: UdpSocket,
    dest: Destinations,
    mut cmds: mpsc::Receiver<Cmd>,
    mut inbound: mpsc::Receiver<Inbound>,
    routes: Routes,
) {
    let ufrags = routes.ufrags.clone();
    let mut buf = vec![0u8; 2048];
    let (media_tx, mut media_rx) = mpsc::channel::<(u64, Out)>(MEDIA_QUEUE);
    let mut slots: HashMap<u64, Slot> = HashMap::new();
    let mut router = Router { dest, by_source: HashMap::new() };
    let mut outbox = match Outbox::new(&socket) {
        Ok(o) => o,
        Err(e) => {
            tracing::error!(engine = me, error = %e, "webrtc: cannot set up the udp socket; engine stopped");
            return;
        }
    };
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
            r = socket.writable(), if outbox.blocked => {
                if r.is_ok() {
                    outbox.writable(&socket);
                }
            }
            r = socket.recv_from(&mut buf) => match r {
                Ok((n, source)) => match routes.owner(&buf[..n], source) {
                    Some(e) if e != me => routes.forward(e, &buf[..n], source),
                    _ => on_datagram(&buf[..n], source, &socket, &mut outbox, &mut router, &mut slots),
                },
                // ICMP port unreachable and friends surface here; they
                // concern one remote, never the socket as a whole.
                Err(e) => tracing::debug!(error = %e, "webrtc: udp recv error"),
            },
            d = inbound.recv() => match d {
                Some((data, source)) => on_datagram(&data, source, &socket, &mut outbox, &mut router, &mut slots),
                None => break,
            },
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(wake)) => {}
        }

        // Read what else arrived, without waiting.
        for _ in 0..RECV_BURST {
            match socket.try_recv_from(&mut buf) {
                Ok((n, source)) => match routes.owner(&buf[..n], source) {
                    Some(e) if e != me => routes.forward(e, &buf[..n], source),
                    _ => on_datagram(&buf[..n], source, &socket, &mut outbox, &mut router, &mut slots),
                },
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => tracing::debug!(error = %e, "webrtc: udp recv error"),
            }
        }
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

/// Drains one peer's output: sends its datagrams (batched, see [`Outbox`]),
/// handles events, records the next timeout.
fn drive(socket: &UdpSocket, outbox: &mut Outbox, id: u64, slots: &mut HashMap<u64, Slot>) {
    poll(socket, outbox, id, slots);
    outbox.flush(socket);
}

fn poll(socket: &UdpSocket, outbox: &mut Outbox, id: u64, slots: &mut HashMap<u64, Slot>) {
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
            Output::Transmit(t) => outbox.push(socket, t.contents.into(), t.destination),
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
    use super::{Outbox, stun_server_ufrag};
    use tokio::net::UdpSocket;

    /// Every datagram arrives whole and in order per destination, whether
    /// the platform segments (Linux GSO) or not: runs of equal sizes, a
    /// shorter last one, a longer one that must start a new send, two
    /// destinations interleaved, and more than one flush's worth.
    #[tokio::test]
    async fn outbox_batches_keep_datagrams_and_order() {
        let tx = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        for s in [&a, &b] {
            // Room for everything: nothing reads until all is sent.
            let _ = socket2::SockRef::from(s).set_recv_buffer_size(4 << 20);
        }
        let (to_a, to_b) = (a.local_addr().unwrap(), b.local_addr().unwrap());
        let mut outbox = Outbox::new(&tx).unwrap();
        let mut sizes = vec![1200, 1200, 1200, 700, 1200, 1300, 1300, 90];
        sizes.extend(std::iter::repeat_n(1100, 150));
        sizes.push(40);
        let mut sent_a = Vec::new();
        let mut sent_b = Vec::new();
        for round in 0..4u8 {
            for (i, len) in sizes.iter().enumerate() {
                let d: Vec<u8> = (0..*len).map(|j| (j as u8) ^ (i as u8) ^ round).collect();
                if i % 40 < 30 {
                    sent_a.push(d.clone());
                    outbox.push(&tx, d, to_a);
                } else {
                    sent_b.push(d.clone());
                    outbox.push(&tx, d, to_b);
                }
            }
        }
        while !outbox.queue.is_empty() {
            if outbox.blocked {
                tx.writable().await.unwrap();
                outbox.writable(&tx);
            } else {
                outbox.flush(&tx);
            }
        }
        for (sock, sent) in [(&a, sent_a), (&b, sent_b)] {
            let mut buf = vec![0u8; 65_536];
            for (k, want) in sent.iter().enumerate() {
                let got = tokio::time::timeout(std::time::Duration::from_secs(5), sock.recv(&mut buf))
                    .await
                    .expect("datagram arrives")
                    .unwrap();
                assert_eq!(&buf[..got], &want[..], "datagram {k}");
            }
        }
    }

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
