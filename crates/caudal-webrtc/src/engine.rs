//! The peer loop: one task owns the UDP socket and every `Rtc`.
//!
//! Incoming datagrams go to the peer whose `Rtc::accepts` them (the last
//! peer seen at that source address is tried first). Each peer's
//! `poll_output` is drained after every input; its next timeout is kept and
//! the loop sleeps until the earliest one. HTTP handlers and WHEP
//! forwarders talk to the loop over channels.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
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
    deadline: Instant,
    last_activity: Instant,
    video_mid: Option<Mid>,
    last_pli: Option<Instant>,
}

pub(crate) async fn run(socket: UdpSocket, mut dest: Destinations, mut cmds: mpsc::Receiver<Cmd>) {
    let (media_tx, mut media_rx) = mpsc::channel::<(u64, Out)>(MEDIA_QUEUE);
    let mut slots: HashMap<u64, Slot> = HashMap::new();
    let mut by_source: HashMap<SocketAddr, u64> = HashMap::new();
    let mut bframes_warned: HashSet<Arc<str>> = HashSet::new();
    let mut next_id: u64 = 0;
    let mut buf = vec![0u8; 2048];
    let mut last_sweep = Instant::now();

    loop {
        let now = Instant::now();
        let wake = slots.values().map(|s| s.deadline).min().unwrap_or(now + Duration::from_secs(1));
        let wake = wake.min(now + Duration::from_millis(250)).max(now);

        tokio::select! {
            biased;
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
                    slots.insert(id, Slot { peer, deadline: now, last_activity: now, video_mid: None, last_pli: None });
                    drive(&socket, id, &mut slots).await;
                }
                Some(Cmd::Delete { whip, name, session, reply }) => {
                    let found = slots.iter().find(|(_, s)| {
                        s.peer.session == session && *s.peer.name == *name && matches!(s.peer.role, Role::Whip(_)) == whip
                    }).map(|(id, _)| *id);
                    if let Some(id) = found {
                        remove(id, &mut slots, "deleted");
                    }
                    let _ = reply.send(found.is_some());
                }
            },
            m = media_rx.recv() => {
                // `media_tx` is held here, so the channel never closes.
                let Some((id, out)) = m else { continue };
                if let Some(slot) = slots.get_mut(&id) {
                    if let Role::Whep(e) = &mut slot.peer.role {
                        match out {
                            Out::Tracks(t) => e.set_tracks(&t),
                            Out::Frame(f) => e.write(&mut slot.peer.rtc, &f, &mut bframes_warned),
                            Out::End => {
                                bframes_warned.remove(&slot.peer.name);
                                slot.peer.rtc.disconnect();
                            }
                        }
                    }
                    drive(&socket, id, &mut slots).await;
                }
            }
            r = socket.recv_from(&mut buf) => match r {
                Ok((n, source)) => {
                    let Some(destination) = dest.for_source(source) else { continue };
                    let Ok(recv) = Receive::new(Protocol::Udp, source, destination, &buf[..n]) else { continue };
                    let now = Instant::now();
                    let input = Input::Receive(now, recv);
                    let cached = by_source.get(&source).copied().filter(|id| {
                        slots.get(id).is_some_and(|s| s.peer.rtc.accepts(&input))
                    });
                    let id = cached.or_else(|| {
                        slots.iter().find(|(_, s)| s.peer.rtc.accepts(&input)).map(|(id, _)| *id)
                    });
                    let Some(id) = id else { continue };
                    if cached.is_none() {
                        if by_source.len() > 65_536 {
                            by_source.retain(|_, v| slots.contains_key(v));
                        }
                        by_source.insert(source, id);
                    }
                    let Some(slot) = slots.get_mut(&id) else { continue };
                    if matches!(slot.peer.role, Role::Whep(_)) {
                        slot.last_activity = now;
                    }
                    if let Err(e) = slot.peer.rtc.handle_input(input) {
                        tracing::debug!(session = %slot.peer.session, error = %e, "webrtc: bad input");
                        slot.peer.rtc.disconnect();
                    }
                    drive(&socket, id, &mut slots).await;
                }
                Err(e) => {
                    // ICMP port unreachable and friends surface here; they
                    // concern one remote, never the socket as a whole.
                    tracing::debug!(error = %e, "webrtc: udp recv error");
                }
            },
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(wake)) => {}
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
            drive(&socket, id, &mut slots).await;
        }

        let closed: Vec<u64> = slots.iter().filter(|(_, s)| !s.peer.rtc.is_alive()).map(|(id, _)| *id).collect();
        for id in closed {
            remove(id, &mut slots, "closed");
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
                remove(id, &mut slots, why);
            }
            by_source.retain(|_, v| slots.contains_key(v));
        }
    }
}

fn kind(p: &Peer) -> &'static str {
    match p.role {
        Role::Whip(_) => "whip",
        Role::Whep(_) => "whep",
    }
}

fn remove(id: u64, slots: &mut HashMap<u64, Slot>, why: &str) {
    if let Some(mut s) = slots.remove(&id) {
        s.peer.rtc.disconnect();
        tracing::info!(stream = %s.peer.name, session = %s.peer.session, kind = kind(&s.peer), reason = why, "webrtc: session closed");
        // Dropping the peer drops its Publisher (ends the stream) or aborts
        // its forwarder.
    }
}

/// Drains one peer's output: sends datagrams, handles events, records the
/// next timeout.
async fn drive(socket: &UdpSocket, id: u64, slots: &mut HashMap<u64, Slot>) {
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
            Output::Transmit(t) => {
                if let Err(e) = socket.send_to(&t.contents, t.destination).await {
                    tracing::debug!(error = %e, to = %t.destination, "webrtc: udp send failed");
                }
            }
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
