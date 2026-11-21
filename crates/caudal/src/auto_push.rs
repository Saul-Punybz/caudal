//! Automatic pushes (`[[push.auto]]`, MistServer's "auto push"): a rule
//! pairs stream patterns with a target URL template; whenever a matching
//! stream goes live, the push starts by itself, and when the stream ends
//! it stops.
//!
//! No new sender: a live match becomes one more target of the existing
//! engine for its scheme (`caudal_restream`, `caudal_srt`'s push,
//! `caudal_multicast`'s UDP/RTP sender, `caudal_webrtc`'s WHIP push), next
//! to the static `[[restream]]`, `[[srt.push]]`, `[[multicast]]` /
//! `[[udp.push]]`, `[[webrtc.push]]` entries. [`AutoPush`] owns those four
//! target lists: on every publish
//! or end, and on a config reload, it recomputes `static ∪ expansions` and
//! hands each engine its list. The engines' `reload` diffs by identity, so
//! a target that did not change keeps its task and connection: an
//! unrelated push is never restarted.
//!
//! Dedupe: a static entry and an expansion that produce the same target
//! (same stream and URL) run once, as the static one; two rules that expand
//! to the same target also run once. For UDP/RTP the key is the destination
//! address (one stream per destination, as for the static entries): config
//! check refuses a rule whose destination a static entry or another rule
//! already uses, and while several streams match one UDP rule, the stream
//! that already holds the destination keeps it (otherwise the first by
//! name) and the others are reported as skipped.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use caudal_core::Registry;
use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;

use crate::config::{AutoPushEntry, AutoPushKind, Config, expand_push_template};

/// The statically configured targets of the three push engines, and the
/// auto rules on top of them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PushSet {
    pub restream: Vec<caudal_restream::RestreamTarget>,
    pub srt: Vec<caudal_srt::SrtPush>,
    pub udp: Vec<caudal_multicast::MulticastTarget>,
    pub whip: Vec<caudal_webrtc::WhipPush>,
    pub rules: Vec<AutoPushEntry>,
}

impl PushSet {
    /// From a validated config.
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            restream: cfg.restream_targets(),
            srt: cfg.srt.push_targets(),
            udp: cfg.multicast_targets().expect("validated"),
            whip: cfg.webrtc.push_targets(),
            rules: cfg.push.auto.clone(),
        }
    }
}

/// One rule as `GET /api/v1/auto-pushes` shows it.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RuleJson {
    pub streams: Vec<String>,
    /// The template, secrets masked like every push listing.
    pub url: String,
    /// `rtmp`, `srt`, `udp` or `whip`: the engine the pushes run on.
    pub kind: &'static str,
    /// The pushes this rule runs now, one per live matching stream.
    pub active: Vec<ActiveJson>,
    /// Matching live streams this rule does not push, and why.
    pub skipped: Vec<SkippedJson>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ActiveJson {
    pub stream: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SkippedJson {
    pub stream: String,
    pub reason: String,
}

/// What one recompute decided: the engines' full target lists, and the
/// per-rule view for the API.
#[derive(Debug, Default, PartialEq)]
struct Plan {
    restream: Vec<caudal_restream::RestreamTarget>,
    srt: Vec<caudal_srt::SrtPush>,
    udp: Vec<caudal_multicast::MulticastTarget>,
    whip: Vec<caudal_webrtc::WhipPush>,
    rules: Vec<RuleJson>,
}

/// `set`'s static targets plus every rule expanded for each name in `live`
/// (sorted). `held` maps a UDP destination to the stream an auto push
/// already sends there, so a running one is never displaced.
fn plan(
    set: &PushSet,
    live: &[String],
    held: &BTreeMap<std::net::SocketAddr, String>,
) -> (Plan, BTreeMap<std::net::SocketAddr, String>) {
    let mut p = Plan {
        restream: set.restream.clone(),
        srt: set.srt.clone(),
        udp: set.udp.clone(),
        whip: set.whip.clone(),
        rules: vec![],
    };
    let mut now_held = BTreeMap::new();
    for rule in &set.rules {
        let kind = rule.kind().expect("validated");
        let mut json = RuleJson {
            streams: rule.streams.clone(),
            url: crate::config_edit::redact(&rule.url),
            kind: kind.as_str(),
            active: vec![],
            skipped: vec![],
        };
        let matching: Vec<&String> = live.iter().filter(|n| rule.matches(n)).collect();
        match kind {
            AutoPushKind::Restream => {
                for name in matching {
                    let url = expand_push_template(&rule.url, name);
                    let t = caudal_restream::RestreamTarget { stream: name.clone(), url };
                    json.active.push(ActiveJson { stream: name.clone(), url: crate::config_edit::redact(&t.url) });
                    if !p.restream.contains(&t) {
                        p.restream.push(t);
                    }
                }
            }
            AutoPushKind::Srt => {
                for name in matching {
                    let url = expand_push_template(&rule.url, name);
                    let t = caudal_srt::SrtPush { stream: name.clone(), url };
                    json.active.push(ActiveJson { stream: name.clone(), url: crate::config_edit::redact(&t.url) });
                    if !p.srt.contains(&t) {
                        p.srt.push(t);
                    }
                }
            }
            AutoPushKind::Whip => {
                for name in matching {
                    let url = expand_push_template(&rule.url, name);
                    let t = caudal_webrtc::WhipPush { stream: name.clone(), url, token: rule.token.clone() };
                    json.active.push(ActiveJson { stream: name.clone(), url: crate::config_edit::redact(&t.url) });
                    if !p.whip.iter().any(|w| w.stream == t.stream && w.url == t.url) {
                        p.whip.push(t);
                    }
                }
            }
            AutoPushKind::Udp => {
                let Some(first) = matching.first() else {
                    p.rules.push(json);
                    continue;
                };
                // Config check guarantees a fixed destination per rule, not
                // shared with a static entry or another rule.
                let group = rule.udp_target(first).expect("validated").group;
                let owner = held
                    .get(&group)
                    .filter(|s| matching.iter().any(|m| m == s))
                    .cloned()
                    .unwrap_or_else(|| (*first).clone());
                for name in matching {
                    if *name == owner {
                        let t = rule.udp_target(name).expect("validated");
                        json.active.push(ActiveJson { stream: name.clone(), url: rule.url.clone() });
                        now_held.insert(group, name.clone());
                        p.udp.push(t);
                    } else {
                        json.skipped.push(SkippedJson {
                            stream: name.clone(),
                            reason: format!("destination {group} is taken by stream {owner:?}"),
                        });
                    }
                }
            }
        }
        p.rules.push(json);
    }
    (p, now_held)
}

struct State {
    set: PushSet,
    /// What each engine was last given, so an event that changes nothing
    /// for an engine doesn't call its `reload`.
    applied: Plan,
    held: BTreeMap<std::net::SocketAddr, String>,
    /// Skips already logged, so a warning is printed once per stream.
    warned: HashSet<(usize, String)>,
}

/// The auto-push rules and the four engines they feed. Cheap to clone.
#[derive(Clone)]
pub struct AutoPush {
    inner: Arc<Inner>,
}

struct Inner {
    registry: Arc<Registry>,
    restream: caudal_restream::RestreamHandle,
    srt: caudal_srt::PushHandle,
    udp: caudal_multicast::MulticastHandle,
    whip: caudal_webrtc::WhipPushHandle,
    state: Mutex<State>,
}

impl AutoPush {
    /// Takes over the target lists of the four engines (started with
    /// `set`'s static targets) and starts watching publishes and ends.
    /// Must be called inside a tokio runtime.
    pub fn start(
        registry: Arc<Registry>,
        set: PushSet,
        restream: caudal_restream::RestreamHandle,
        srt: caudal_srt::PushHandle,
        udp: caudal_multicast::MulticastHandle,
        whip: caudal_webrtc::WhipPushHandle,
    ) -> Self {
        let applied = Plan {
            restream: set.restream.clone(),
            srt: set.srt.clone(),
            udp: set.udp.clone(),
            whip: set.whip.clone(),
            rules: vec![],
        };
        let state = State { set, applied, held: BTreeMap::new(), warned: HashSet::new() };
        let inner = Arc::new(Inner { registry: registry.clone(), restream, srt, udp, whip, state: Mutex::new(state) });
        // Subscribe before the first recompute, so a stream published in
        // between is seen (a recompute is idempotent).
        let mut publishes = registry.subscribe_publishes();
        let mut ends = registry.subscribe_ends();
        inner.recompute();
        // Weak: dropping every `AutoPush` ends the watcher.
        let weak = Arc::downgrade(&inner);
        tokio::spawn(async move {
            loop {
                let closed = tokio::select! {
                    r = publishes.recv() => matches!(r, Err(RecvError::Closed)),
                    r = ends.recv() => matches!(r, Err(RecvError::Closed)),
                };
                let Some(inner) = weak.upgrade() else { break };
                if closed {
                    break;
                }
                // Lagged or not, the registry is the truth: recompute.
                inner.recompute();
            }
        });
        Self { inner }
    }

    /// Applies a new configuration (static targets and rules); a target
    /// that is in both the old and the new result keeps running.
    pub fn configure(&self, set: PushSet) {
        self.inner.state.lock().set = set;
        self.inner.recompute();
    }

    /// Every rule with what it pushes right now.
    pub fn rules(&self) -> Vec<RuleJson> {
        self.inner.state.lock().applied.rules.clone()
    }
}

impl Inner {
    fn recompute(&self) {
        // Held for the whole apply, and taken before reading the registry:
        // a publish event and a config reload never interleave, and the
        // last one to apply always saw the newest stream list.
        let mut st = self.state.lock();
        let live: Vec<String> =
            self.registry.list().into_iter().filter(|s| !s.is_ended()).map(|s| s.name().to_owned()).collect();
        let (next, held) = plan(&st.set, &live, &st.held);
        let mut warned = HashSet::new();
        for (i, rule) in next.rules.iter().enumerate() {
            for s in &rule.skipped {
                let key = (i, s.stream.clone());
                if !st.warned.contains(&key) {
                    tracing::warn!(stream = %s.stream, rule = %rule.url, reason = %s.reason, "auto push skipped");
                }
                warned.insert(key);
            }
        }
        st.warned = warned;
        let active = |p: &Plan| -> HashSet<(String, String)> {
            p.rules.iter().flat_map(|r| r.active.iter().map(|a| (a.stream.clone(), a.url.clone()))).collect()
        };
        let (before, after) = (active(&st.applied), active(&next));
        for (stream, target) in after.difference(&before) {
            tracing::info!(%stream, %target, "auto push started");
        }
        for (stream, target) in before.difference(&after) {
            tracing::info!(%stream, %target, "auto push stopped");
        }
        if next.restream != st.applied.restream {
            self.restream.reload(&self.registry, next.restream.clone());
        }
        if next.srt != st.applied.srt {
            self.srt.reload(&self.registry, next.srt.clone());
        }
        if next.udp != st.applied.udp {
            self.udp.reload(&self.registry, next.udp.clone());
        }
        if next.whip != st.applied.whip {
            self.whip.reload(&self.registry, next.whip.clone());
        }
        st.held = held;
        st.applied = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(streams: &[&str], url: &str) -> AutoPushEntry {
        AutoPushEntry {
            streams: streams.iter().map(|s| s.to_string()).collect(),
            url: url.into(),
            ttl: None,
            interface: None,
            pacing: None,
            token: None,
        }
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn expands_per_matching_stream_and_dedupes_with_static() {
        let set = PushSet {
            srt: vec![caudal_srt::SrtPush { stream: "cam1".into(), url: "srt://h:9000?streamid=cam1".into() }],
            rules: vec![
                rule(&["cam*"], "srt://h:9000?streamid=$stream"),
                rule(&["main"], "rtmp://yt.example/live/${stream}-key"),
                // Same target as the first rule for cam2: runs once.
                rule(&["cam2"], "srt://h:9000?streamid=cam2"),
            ],
            ..Default::default()
        };
        let (p, _) = plan(&set, &names(&["cam1", "cam2", "cam2+720p", "main", "other"]), &BTreeMap::new());
        let srt: Vec<_> = p.srt.iter().map(|t| (t.stream.as_str(), t.url.as_str())).collect();
        assert_eq!(srt, [("cam1", "srt://h:9000?streamid=cam1"), ("cam2", "srt://h:9000?streamid=cam2")]);
        assert_eq!(p.restream.len(), 1);
        assert_eq!(p.restream[0].url, "rtmp://yt.example/live/main-key");
        assert_eq!(p.rules[0].active.len(), 2, "cam2+720p never matches");
        assert_eq!(p.rules[1].active[0].url, "rtmp://yt.example/live/****", "keys are masked");
        assert!(p.udp.is_empty());
    }

    #[test]
    fn whip_rules_expand_with_their_token() {
        let set = PushSet {
            whip: vec![caudal_webrtc::WhipPush {
                stream: "cam1".into(),
                url: "whip://h/whip/cam1".into(),
                token: None,
            }],
            rules: vec![AutoPushEntry { token: Some("tk".into()), ..rule(&["cam*"], "whip://h/whip/$stream") }],
            ..Default::default()
        };
        let (p, _) = plan(&set, &names(&["cam1", "cam2", "other"]), &BTreeMap::new());
        let got: Vec<_> = p.whip.iter().map(|t| (t.stream.as_str(), t.url.as_str(), t.token.as_deref())).collect();
        // cam1 is the static entry already (runs once, as the static one).
        assert_eq!(got, [("cam1", "whip://h/whip/cam1", None), ("cam2", "whip://h/whip/cam2", Some("tk"))]);
        assert_eq!(p.rules[0].kind, "whip");
        assert_eq!(p.rules[0].active.len(), 2);
    }

    #[test]
    fn udp_destination_stays_with_its_stream() {
        let set = PushSet { rules: vec![rule(&["*"], "udp://127.0.0.1:5000")], ..Default::default() };
        let (p, held) = plan(&set, &names(&["a", "b"]), &BTreeMap::new());
        assert_eq!(p.udp.len(), 1);
        assert_eq!(p.udp[0].stream, "a");
        assert_eq!(p.rules[0].skipped[0].stream, "b");
        // `a0` sorts first but `a` holds the destination.
        let (p, _) = plan(&set, &names(&["a", "a0", "b"]), &held);
        assert_eq!(p.udp[0].stream, "a");
        // Once `a` ends, the next one takes it.
        let (p, _) = plan(&set, &names(&["a0", "b"]), &held);
        assert_eq!(p.udp[0].stream, "a0");
    }

    #[tokio::test]
    async fn configure_keeps_unrelated_targets() {
        let registry = Registry::new();
        let set = PushSet {
            restream: vec![caudal_restream::RestreamTarget { stream: "s".into(), url: "rtmp://x/a/k".into() }],
            ..Default::default()
        };
        let restream =
            caudal_restream::start(registry.clone(), caudal_restream::RestreamConfig { targets: set.restream.clone() });
        let srt = caudal_srt::start_pushes(registry.clone(), vec![]);
        let udp = caudal_multicast::start(registry.clone(), vec![]);
        let whip = caudal_webrtc::start_pushes(registry.clone(), vec![]);
        let auto = AutoPush::start(registry.clone(), set.clone(), restream, srt, udp, whip);
        let _p = registry.publish("cam", caudal_core::BufferConfig::default()).unwrap();
        let mut with_rule = set.clone();
        with_rule.rules.push(rule(&["cam"], "rtmp://y/live/$stream"));
        auto.configure(with_rule);
        let rules = auto.rules();
        assert_eq!(rules[0].active, [ActiveJson { stream: "cam".into(), url: "rtmp://y/live/****".into() }]);
        let applied = auto.inner.state.lock().applied.restream.clone();
        assert_eq!(applied.len(), 2);
        assert_eq!(applied[0].stream, "s", "the static target stays first and untouched");
        auto.configure(set);
        assert_eq!(auto.inner.state.lock().applied.restream.len(), 1);
    }

    fn aac() -> caudal_core::TrackInfo {
        let asc: u16 = (2u16 << 11) | (3u16 << 7) | (2u16 << 3);
        caudal_core::TrackInfo {
            id: caudal_core::TrackId(0),
            codec: caudal_core::Codec::Aac,
            timescale: 48_000,
            init: bytes::Bytes::copy_from_slice(&asc.to_be_bytes()),
            lang: None,
            video: None,
            audio: Some(caudal_core::AudioParams { sample_rate: 48_000, channels: 2 }),
        }
    }

    /// Publishes `name` with one second of AAC frames; returns the publisher.
    fn publish(registry: &Arc<Registry>, name: &str) -> caudal_core::Publisher {
        let p = registry.publish(name, caudal_core::BufferConfig::default()).unwrap();
        p.set_tracks(vec![aac()]).unwrap();
        p
    }

    fn feed(p: &caudal_core::Publisher, from: i64, n: i64) {
        for i in from..from + n {
            let dts = i * 1024;
            p.push(caudal_core::Frame {
                track: caudal_core::TrackId(0),
                dts,
                pts: dts,
                keyframe: true,
                data: bytes::Bytes::from(vec![0x21; 200]),
            })
            .unwrap();
        }
    }

    /// Datagrams that arrive on `sock` within `wait`.
    async fn received(sock: &tokio::net::UdpSocket, wait: std::time::Duration) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0u8; 2048];
        let deadline = tokio::time::Instant::now() + wait;
        while let Ok(Ok(n)) = tokio::time::timeout_at(deadline, sock.recv(&mut buf)).await {
            out.push(buf[..n].to_vec());
        }
        out
    }

    async fn until(what: &str, mut f: impl FnMut() -> bool) {
        let t0 = std::time::Instant::now();
        while !f() {
            assert!(t0.elapsed() < std::time::Duration::from_secs(5), "timed out waiting for {what}");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// A UDP rule really delivers MPEG-TS to a local socket for a matching
    /// stream, never for a non-matching one, and stops when the stream ends.
    #[tokio::test]
    async fn udp_rule_delivers_ts_while_matching_stream_is_live() {
        use std::time::Duration;
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = sock.local_addr().unwrap().port();
        let registry = Registry::new();
        let set = PushSet {
            rules: vec![AutoPushEntry { pacing: Some(false), ..rule(&["cam*"], &format!("udp://127.0.0.1:{port}")) }],
            ..Default::default()
        };
        let restream = caudal_restream::start(registry.clone(), caudal_restream::RestreamConfig { targets: vec![] });
        let srt = caudal_srt::start_pushes(registry.clone(), vec![]);
        let udp = caudal_multicast::start(registry.clone(), vec![]);
        let whip = caudal_webrtc::start_pushes(registry.clone(), vec![]);
        let auto = AutoPush::start(registry.clone(), set, restream, srt, udp, whip);

        // Not matching: nothing is sent.
        let other = publish(&registry, "studio");
        feed(&other, 0, 100);
        assert!(received(&sock, Duration::from_millis(500)).await.is_empty(), "no push for a non-matching stream");
        assert!(auto.rules()[0].active.is_empty());

        // Matching: TS arrives, seven 188-byte packets per datagram.
        let cam = publish(&registry, "cam1");
        until("the auto push to start", || !auto.rules()[0].active.is_empty()).await;
        let mut got = Vec::new();
        for round in 0..20 {
            feed(&cam, round * 50, 50);
            got.extend(received(&sock, Duration::from_millis(100)).await);
            if !got.is_empty() {
                break;
            }
        }
        assert!(!got.is_empty(), "the auto push delivered nothing");
        for d in &got {
            assert_eq!(d.len() % 188, 0, "whole TS packets");
            assert!(d.chunks(188).all(|p| p[0] == 0x47), "TS sync bytes");
        }

        // Ending the stream stops the push: the target goes away.
        drop(cam);
        until("the auto push to stop", || auto.rules()[0].active.is_empty()).await;
        assert!(auto.inner.state.lock().applied.udp.is_empty());
        received(&sock, Duration::from_millis(200)).await; // drain
        feed(&other, 100, 100);
        assert!(received(&sock, Duration::from_millis(300)).await.is_empty(), "nothing after the end");
    }
}
