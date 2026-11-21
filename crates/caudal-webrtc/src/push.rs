//! WHIP push out (`[[webrtc.push]]`, MistServer's `whip://` push): while a
//! stream is live, Caudal is the WHIP *client* and sends it to a remote
//! WHIP endpoint (Cloudflare Stream, Dolby/Millicast, LiveKit, another
//! Caudal).
//!
//! One task per push. Each session: subscribe to the stream, wait for its
//! tracks, build a full-ICE `Rtc` with send-only H.264 and/or Opus, `POST`
//! the offer as `application/sdp` (with `Authorization: Bearer` when a
//! token is set), apply the answer (its `a=candidate` lines are the remote
//! candidates; no trickle `PATCH`), then run the peer on its own UDP
//! socket and write frames through the same [`Egress`] WHEP uses. When the
//! stream ends, or the push is removed, the resource named by `Location`
//! gets a `DELETE`. A failed or dropped session reconnects with
//! exponential backoff while the stream is live.
//!
//! Codecs are what WHEP sends: H.264 and Opus. AAC (or other non-Opus)
//! audio is not transcoded here; such a stream is pushed as video only.
//!
//! URL schemes: `whip://host/path` is WHIP over HTTPS, `whip+http://` over
//! plain HTTP (a LAN or a local test). `https://` and `http://` are taken
//! as they are, since the section says what they are for.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use caudal_core::{Codec, Event as StreamEvent, Registry, StartAt, Stream};
use parking_lot::Mutex;
use str0m::change::SdpAnswer;
use str0m::media::{Direction, MediaKind};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

use crate::egress::{Ctl, Egress};
use crate::net::{Destinations, candidate_addrs};

const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
/// For each HTTP request (the offer `POST`, the `DELETE`).
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// From the answer to ICE + DTLS up.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a just-published stream may take to announce its tracks.
const TRACKS_WAIT: Duration = Duration::from_secs(10);

/// Push `stream` to the WHIP endpoint `url` while it is live.
#[derive(Clone, PartialEq, Eq)]
pub struct WhipPush {
    pub stream: String,
    /// `whip://` (HTTPS), `whip+http://`, `https://` or `http://`.
    pub url: String,
    /// Sent as `Authorization: Bearer <token>`.
    pub token: Option<String>,
}

impl std::fmt::Debug for WhipPush {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhipPush")
            .field("stream", &self.stream)
            .field("url", &redact_url(&self.url))
            .field("token", &self.token.as_ref().map(|_| "****"))
            .finish()
    }
}

/// The HTTP(S) URL a push URL posts its offer to. `Err` names what is
/// wrong (never the URL's secrets: the message shows it masked).
pub fn whip_endpoint(url: &str) -> Result<String, String> {
    let shown = redact_url(url);
    let (scheme, rest) = url.split_once("://").ok_or_else(|| format!("{shown:?}: not a URL"))?;
    let http = match scheme.to_ascii_lowercase().as_str() {
        "whip" | "https" => "https",
        "whip+http" | "http" => "http",
        _ => return Err(format!("{shown:?}: expected whip://, whip+http://, https:// or http://")),
    };
    let parsed = reqwest::Url::parse(&format!("{http}://{rest}")).map_err(|e| format!("{shown:?}: {e}"))?;
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err(format!("{shown:?}: no host"));
    }
    Ok(parsed.into())
}

/// `url` for logs and listings: user info, every query value and every
/// path segment of 16 characters or more (a stream key, like Cloudflare's
/// `/<key>/webRTC/publish`) become `****`.
pub fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else { return url.to_owned() };
    let (rest, query) = rest.split_once('?').map_or((rest, None), |(r, q)| (r, Some(q)));
    let (authority, path) = rest.split_once('/').map_or((rest, None), |(a, p)| (a, Some(p)));
    let mut out = format!("{scheme}://");
    match authority.rsplit_once('@') {
        Some((_, host)) => {
            out.push_str("****@");
            out.push_str(host);
        }
        None => out.push_str(authority),
    }
    if let Some(path) = path {
        for seg in path.split('/') {
            out.push('/');
            out.push_str(if seg.len() >= 16 { "****" } else { seg });
        }
    }
    if let Some(q) = query {
        let q: Vec<String> = q
            .split('&')
            .map(|kv| match kv.split_once('=') {
                Some((k, _)) => format!("{k}=****"),
                None => kv.to_owned(),
            })
            .collect();
        out.push('?');
        out.push_str(&q.join("&"));
    }
    out
}

struct Entry {
    push: WhipPush,
    task: tokio::task::JoinHandle<()>,
}

/// The running `[[webrtc.push]]` tasks. Cheap to clone.
#[derive(Clone)]
pub struct WhipPushHandle {
    entries: Arc<Mutex<Vec<Entry>>>,
    client: reqwest::Client,
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder().timeout(HTTP_TIMEOUT).build().unwrap_or_default()
}

/// Starts every push (current and future publishes of its stream) and
/// returns at once. Must be called inside a tokio runtime; each push is its
/// own task, so a slow or failing endpoint never affects another.
pub fn start_pushes(registry: Arc<Registry>, pushes: Vec<WhipPush>) -> WhipPushHandle {
    let client = http_client();
    let entries = pushes.into_iter().map(|p| spawn(&registry, &client, p)).collect();
    WhipPushHandle { entries: Arc::new(Mutex::new(entries)), client }
}

fn spawn(registry: &Arc<Registry>, client: &reqwest::Client, push: WhipPush) -> Entry {
    let task = tokio::spawn(run(push.clone(), registry.clone(), client.clone()));
    Entry { push, task }
}

impl WhipPushHandle {
    /// Applies a new list: an unchanged push (same stream, URL and token)
    /// keeps its task and its session; removed ones stop (their remote
    /// resource gets a `DELETE`), new or changed ones start.
    pub fn reload(&self, registry: &Arc<Registry>, pushes: Vec<WhipPush>) {
        let mut entries = self.entries.lock();
        let mut remaining = std::mem::take(&mut *entries);
        let mut next = Vec::with_capacity(pushes.len());
        for p in pushes {
            match remaining.iter().position(|e| e.push == p) {
                Some(pos) => next.push(remaining.remove(pos)),
                None => next.push(spawn(registry, &self.client, p)),
            }
        }
        for gone in remaining {
            gone.task.abort();
        }
        *entries = next;
    }

    /// The pushes running now, in order.
    pub fn targets(&self) -> Vec<WhipPush> {
        self.entries.lock().iter().map(|e| e.push.clone()).collect()
    }
}

/// One push, forever: wait for the stream, push it until it ends, repeat.
async fn run(push: WhipPush, registry: Arc<Registry>, client: reqwest::Client) {
    let shown = redact_url(&push.url);
    loop {
        let stream = match registry.get(&push.stream) {
            Some(s) if !s.is_ended() => s,
            _ => match wait_for_publish(&registry, &push.stream).await {
                Some(s) => s,
                None => return,
            },
        };
        let mut backoff = BACKOFF_MIN;
        while !stream.is_ended() {
            let wait = match session(&push, &shown, &client, &stream).await {
                Ok(Outcome::Ended) => break,
                Ok(Outcome::Dropped { connected }) => {
                    tracing::info!(stream = %push.stream, url = %shown, "whip push session ended; reconnecting");
                    if connected {
                        backoff = BACKOFF_MIN;
                    }
                    backoff
                }
                Err(e) => {
                    tracing::warn!(stream = %push.stream, url = %shown, error = %e, retry_in = ?backoff, "whip push failed");
                    backoff
                }
            };
            tokio::time::sleep(wait).await;
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }
}

/// `name` once it is (re)published; `None` when the registry is gone.
async fn wait_for_publish(registry: &Arc<Registry>, name: &str) -> Option<Arc<Stream>> {
    let mut publishes = registry.subscribe_publishes();
    // A publish may have landed before the subscription.
    if let Some(s) = registry.get(name)
        && !s.is_ended()
    {
        return Some(s);
    }
    loop {
        match publishes.recv().await {
            Ok(s) if s.name() == name => return Some(s),
            Ok(_) | Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => return None,
        }
    }
}

enum Outcome {
    /// The stream ended: wait for the next publish.
    Ended,
    /// The peer went away; `connected`: it had been up (reset the backoff).
    Dropped { connected: bool },
}

/// The remote WHIP resource (`Location`); `DELETE`d when the session ends,
/// or from `Drop` when the task is aborted (push removed by a reload).
struct Resource {
    client: reqwest::Client,
    url: Option<reqwest::Url>,
    token: Option<String>,
}

impl Resource {
    async fn delete(mut self) {
        if let Some(url) = self.url.take() {
            delete(&self.client, url, self.token.take()).await;
        }
    }
}

impl Drop for Resource {
    fn drop(&mut self) {
        if let Some(url) = self.url.take()
            && let Ok(rt) = tokio::runtime::Handle::try_current()
        {
            let (client, token) = (self.client.clone(), self.token.take());
            rt.spawn(async move { delete(&client, url, token).await });
        }
    }
}

async fn delete(client: &reqwest::Client, url: reqwest::Url, token: Option<String>) {
    let mut req = client.delete(url);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    match req.send().await {
        Ok(r) => tracing::debug!(status = %r.status(), "whip push: resource deleted"),
        Err(e) => tracing::debug!(error = %e.without_url(), "whip push: DELETE failed"),
    }
}

/// The local address the kernel would use to reach `remote` (a connected,
/// never-used UDP socket: no packet is sent).
fn route_to(remote: SocketAddr) -> Option<IpAddr> {
    let any: IpAddr = if remote.is_ipv4() { Ipv4Addr::UNSPECIFIED.into() } else { Ipv6Addr::UNSPECIFIED.into() };
    let s = std::net::UdpSocket::bind(SocketAddr::new(any, 0)).ok()?;
    s.connect(remote).ok()?;
    s.local_addr().ok().map(|a| a.ip()).filter(|ip| !ip.is_unspecified())
}

async fn resolve(url: &reqwest::Url) -> Result<SocketAddr, String> {
    let port = url.port_or_known_default().unwrap_or(443);
    match url.host() {
        Some(url::Host::Ipv4(ip)) => Ok(SocketAddr::new(ip.into(), port)),
        Some(url::Host::Ipv6(ip)) => Ok(SocketAddr::new(ip.into(), port)),
        Some(url::Host::Domain(d)) => tokio::net::lookup_host((d, port))
            .await
            .map_err(|e| format!("resolving {d}: {e}"))?
            .next()
            .ok_or_else(|| format!("{d} has no address")),
        None => Err("no host".into()),
    }
}

/// One WHIP session: offer, answer, media until the stream ends or the
/// peer goes away.
async fn session(
    push: &WhipPush,
    shown: &str,
    client: &reqwest::Client,
    stream: &Arc<Stream>,
) -> Result<Outcome, String> {
    let endpoint = reqwest::Url::parse(&whip_endpoint(&push.url)?).map_err(|e| e.to_string())?;

    // Not counted as a viewer: an outbound relay.
    let mut sub = stream.subscribe_internal(StartAt::LiveEdge);
    let deadline = tokio::time::Instant::now() + TRACKS_WAIT;
    while sub.tracks().is_empty() {
        match tokio::time::timeout_at(deadline, sub.recv()).await {
            Err(_) => return Err("the stream announced no tracks".into()),
            Ok(StreamEvent::End) => return Ok(Outcome::Ended),
            Ok(_) => {}
        }
    }
    let tracks = sub.tracks();
    let has_video = tracks.iter().any(|t| t.codec == Codec::H264);
    let has_audio = tracks.iter().any(|t| t.codec == Codec::Opus);
    if !has_video && !has_audio {
        return Err("the stream has neither H.264 video nor Opus audio; nothing WebRTC can carry".into());
    }

    // Our side of ICE: one UDP socket, its host candidate the address that
    // routes to the endpoint (a WHIP server nearly always has its media on
    // the same host or network). Behind NAT the server learns our public
    // address from our connectivity checks (peer-reflexive).
    let remote = resolve(&endpoint).await?;
    let any: IpAddr = if remote.is_ipv4() { Ipv4Addr::UNSPECIFIED.into() } else { Ipv6Addr::UNSPECIFIED.into() };
    let socket = UdpSocket::bind(SocketAddr::new(any, 0)).await.map_err(|e| format!("udp bind: {e}"))?;
    let local = socket.local_addr().map_err(|e| e.to_string())?;
    let candidates = match route_to(remote) {
        Some(ip) => vec![SocketAddr::new(ip, local.port())],
        None => candidate_addrs(local, &[]),
    };
    if candidates.is_empty() {
        return Err("no local address to offer as an ICE candidate".into());
    }
    let mut dests = Destinations::new(local, candidates.clone());

    let mut rtc: Rtc = crate::rtc_config(false).build(Instant::now());
    for c in &candidates {
        if let Ok(c) = Candidate::host(*c, "udp") {
            rtc.add_local_candidate(c);
        }
    }
    let mut api = rtc.sdp_api();
    let video_mid = has_video.then(|| api.add_media(MediaKind::Video, Direction::SendOnly, None, None, None));
    let audio_mid = has_audio.then(|| api.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None));
    let (offer, pending) = api.apply().ok_or("no media to offer")?;

    let mut req =
        client.post(endpoint).header(reqwest::header::CONTENT_TYPE, "application/sdp").body(offer.to_sdp_string());
    if let Some(t) = &push.token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.map_err(|e| format!("POST: {}", e.without_url()))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("the endpoint answered {status}"));
    }
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|l| resp.url().join(l).ok());
    if location.is_none() {
        tracing::warn!(stream = %push.stream, url = %shown, "whip push: answer has no Location; cannot DELETE at the end");
    }
    let resource = Resource { client: client.clone(), url: location, token: push.token.clone() };
    let answer = match resp.text().await {
        Ok(body) => body,
        Err(e) => {
            resource.delete().await;
            return Err(format!("reading the answer: {}", e.without_url()));
        }
    };
    let accepted = SdpAnswer::from_sdp_string(&answer)
        .map_err(|e| format!("malformed SDP answer: {e}"))
        .and_then(|a| rtc.sdp_api().accept_answer(pending, a).map_err(|e| format!("SDP answer not accepted: {e}")));
    if let Err(e) = accepted {
        resource.delete().await;
        return Err(e);
    }

    let name: Arc<str> = push.stream.as_str().into();
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel();
    let mut egress = Egress::for_push(name, ctl_tx);
    if let Some(mid) = video_mid {
        egress.media_added(mid, MediaKind::Video);
    }
    if let Some(mid) = audio_mid {
        egress.media_added(mid, MediaKind::Audio);
    }
    egress.set_tracks(&tracks);

    let started = Instant::now();
    let mut connected = false;
    let mut warned = HashSet::new();
    let mut buf = vec![0u8; 2048];
    let outcome = loop {
        let wake = match drain(&mut rtc, &socket, &mut egress, &mut connected, push, shown).await {
            Some(t) => t,
            None => break Ok(Outcome::Dropped { connected }),
        };
        if !connected && started.elapsed() > CONNECT_TIMEOUT {
            break Err(format!("ICE/DTLS did not come up within {} s", CONNECT_TIMEOUT.as_secs()));
        }
        let wake = wake.min(Instant::now() + Duration::from_millis(500));
        tokio::select! {
            r = socket.recv_from(&mut buf) => match r {
                Ok((n, source)) => {
                    if let Some(dest) = dests.for_source(source)
                        && let Ok(recv) = Receive::new(Protocol::Udp, source, dest, &buf[..n])
                        && let Err(e) = rtc.handle_input(Input::Receive(Instant::now(), recv))
                    {
                        tracing::debug!(error = %e, "whip push: bad input");
                        rtc.disconnect();
                    }
                }
                Err(e) => tracing::debug!(error = %e, "whip push: udp recv error"),
            },
            ev = sub.recv() => match ev {
                StreamEvent::TracksChanged => egress.set_tracks(&sub.tracks()),
                StreamEvent::Frame(f) => egress.write(&mut rtc, &f, &mut warned),
                StreamEvent::End => break Ok(Outcome::Ended),
                StreamEvent::Lagged { .. } | StreamEvent::Cue(_) => {}
            },
            c = ctl_rx.recv() => match c {
                Some(Ctl::SkipToLive) => { sub.skip_to_live(); }
                Some(Ctl::Restart) => {
                    sub = stream.subscribe_internal(StartAt::LiveEdge);
                    egress.set_tracks(&sub.tracks());
                }
                None => {}
            },
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(wake)) => {
                if let Err(e) = rtc.handle_input(Input::Timeout(Instant::now())) {
                    tracing::debug!(error = %e, "whip push: timeout error");
                    rtc.disconnect();
                }
            }
        }
    };
    resource.delete().await;
    rtc.disconnect();
    outcome
}

/// Sends what the peer wants sent and handles its events. Returns its next
/// timeout, or `None` once it is closed.
async fn drain(
    rtc: &mut Rtc,
    socket: &UdpSocket,
    egress: &mut Egress,
    connected: &mut bool,
    push: &WhipPush,
    shown: &str,
) -> Option<Instant> {
    loop {
        if !rtc.is_alive() {
            return None;
        }
        let out = match rtc.poll_output() {
            Ok(o) => o,
            Err(e) => {
                tracing::debug!(error = %e, "whip push: peer failed");
                return None;
            }
        };
        match out {
            Output::Timeout(t) => return Some(t),
            Output::Transmit(t) => {
                if let Err(e) = socket.send_to(&t.contents, t.destination).await {
                    tracing::debug!(error = %e, to = %t.destination, "whip push: udp send failed");
                }
            }
            Output::Event(Event::Connected) => {
                tracing::info!(stream = %push.stream, url = %shown, "whip push connected");
                *connected = true;
                egress.connected();
            }
            Output::Event(Event::IceConnectionStateChange(IceConnectionState::Disconnected)) => {
                tracing::debug!(stream = %push.stream, "whip push: ICE disconnected");
                rtc.disconnect();
            }
            Output::Event(Event::KeyframeRequest(_)) => egress.keyframe_requested(),
            Output::Event(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemes_map_to_http() {
        assert_eq!(whip_endpoint("whip://live.example/whip/cam").unwrap(), "https://live.example/whip/cam");
        assert_eq!(whip_endpoint("whip+http://127.0.0.1:8080/whip/a").unwrap(), "http://127.0.0.1:8080/whip/a");
        assert_eq!(whip_endpoint("https://h.example/w").unwrap(), "https://h.example/w");
        assert_eq!(whip_endpoint("WHIP://h.example:8443/x?a=b").unwrap(), "https://h.example:8443/x?a=b");
        assert!(whip_endpoint("rtmp://h/a/k").is_err());
        assert!(whip_endpoint("whip://").is_err());
        assert!(whip_endpoint("h.example/x").is_err());
    }

    #[test]
    fn urls_and_tokens_are_masked() {
        let cf = "whip://customer-x.cloudflarestream.com/0123456789abcdef0123456789abcdef/webRTC/publish";
        assert_eq!(redact_url(cf), "whip://customer-x.cloudflarestream.com/****/webRTC/publish");
        assert_eq!(redact_url("whip://u:pw@h/whip/cam?token=abc&x"), "whip://****@h/whip/cam?token=****&x");
        assert_eq!(redact_url("whip+http://127.0.0.1:8080/whip/a"), "whip+http://127.0.0.1:8080/whip/a");
        let p = WhipPush { stream: "a".into(), url: "whip://h/w".into(), token: Some("hunter2".into()) };
        assert!(!format!("{p:?}").contains("hunter2"));
        let e = whip_endpoint("ftp://h/0123456789abcdef0123456789").unwrap_err();
        assert!(!e.contains("0123456789abcdef"), "{e}");
    }
}
