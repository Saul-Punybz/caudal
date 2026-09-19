//! Origins and edges in one process, on loopback: real MoQ (caudal-moq's
//! output, QUIC with a pinned self-signed certificate) and real HTTP
//! (locate, fingerprint) between them. The fixture (H.264 + AAC) is fed
//! into each origin's registry at 2x real time.

#[path = "../../caudal-hls/src/mp4demux.rs"]
mod mp4demux;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use caudal_cluster::{Edge, EdgeConfig, Secret};
use caudal_core::{Access, BufferConfig, Codec, Denied, Event, Gate, GateFuture, Registry, StartAt, Subscriber};
use tokio::time::{Instant, timeout};

const SECRET: &str = "loopback-cluster-secret";
const T: Duration = Duration::from_secs(15);

/// Play needs a valid cluster token, like an origin with `[auth] play`
/// required whose combined gate also accepts cluster tokens.
struct ClusterOnly(Secret);

impl Gate for ClusterOnly {
    fn check<'a>(
        &'a self,
        access: Access,
        _stream: &'a str,
        token: Option<&'a str>,
        _ip: Option<std::net::IpAddr>,
    ) -> GateFuture<'a> {
        Box::pin(async move {
            match (access, token) {
                (Access::Publish, _) => Ok(()),
                (Access::Play, None) => Err(Denied::Missing),
                (Access::Play, Some(t)) => self.0.verify(t).map(|_| ()).ok_or(Denied::Refused("not a node".into())),
            }
        })
    }
}

struct Origin {
    registry: Arc<Registry>,
    http: std::net::SocketAddr,
    stop: Arc<AtomicBool>,
    feeder: Option<tokio::task::JoinHandle<()>>,
}

impl Origin {
    async fn start(node: &str, stream: Option<&str>) -> Self {
        let registry = Registry::new();
        registry.set_gate(Arc::new(ClusterOnly(Secret::new(SECRET).unwrap())));
        let moq = caudal_moq::start(
            registry.clone(),
            caudal_moq::MoqConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                cert: caudal_moq::MoqCert::SelfSigned { hosts: vec!["127.0.0.1".into()] },
                buffer: BufferConfig::default(),
            },
        )
        .expect("moq");
        let app = caudal_cluster::origin_router(registry.clone(), Secret::new(SECRET).unwrap(), node.into())
            .merge(moq.router());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut o = Self { registry, http, stop: Arc::new(AtomicBool::new(false)), feeder: None };
        if let Some(name) = stream {
            o.feed(name);
        }
        o
    }

    fn url(&self) -> url::Url {
        format!("http://{}", self.http).parse().unwrap()
    }

    /// Publishes the fixture as `name`, looped at 2x real time, until
    /// [`Origin::stop_feed`].
    fn feed(&mut self, name: &str) {
        let fx = mp4demux::demux(include_bytes!("../../caudal-hls/tests/fixtures/av.mp4"));
        let publisher = self.registry.publish(name, BufferConfig::default()).unwrap();
        publisher.set_tracks(fx.tracks.clone()).unwrap();
        let stop = self.stop.clone();
        self.feeder = Some(tokio::spawn(async move {
            let start = Instant::now();
            for n in 0.. {
                for f in fx.looped(n) {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    tokio::time::sleep_until(start + Duration::from_micros(fx.micros(&f) as u64 / 2)).await;
                    publisher.push(f).unwrap();
                }
            }
        }));
    }

    /// Ends the stream here (its publisher goes away).
    async fn stop_feed(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(f) = self.feeder.take() {
            f.await.unwrap();
        }
    }
}

fn edge(origins: &[&Origin], secret: &str, idle: Duration) -> (Arc<Registry>, Edge) {
    let registry = Registry::new();
    let edge = Edge::start(
        &registry,
        EdgeConfig {
            node_id: "edge-test".into(),
            secret: Secret::new(secret).unwrap(),
            origins: origins.iter().map(|o| o.url()).collect(),
            idle_timeout: idle,
            source_timeout: Duration::from_secs(10),
            buffer: BufferConfig::default(),
        },
    )
    .unwrap();
    (registry, edge)
}

/// The next frame, skipping everything else.
async fn next_frame(sub: &mut Subscriber) -> Arc<caudal_core::Frame> {
    loop {
        match timeout(T, sub.recv()).await.expect("no frame in time") {
            Event::Frame(f) => return f,
            Event::End => panic!("stream ended"),
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_viewer_pulls_second_shares_idle_tears_down() {
    let origin = Origin::start("origin-a", Some("cam")).await;
    let (registry, edge) = edge(&[&origin], SECRET, Duration::from_secs(2));
    assert!(registry.get("cam").is_none(), "nothing pulled before a viewer");

    // First viewer: the demand pulls the stream and publishes it locally.
    let t0 = Instant::now();
    let stream = timeout(T, registry.get_or_demand("cam")).await.unwrap().expect("pulled");
    let mut v1 = stream.subscribe(StartAt::LiveEdge);
    let tracks = stream.tracks();
    let codecs: Vec<Codec> = tracks.iter().map(|t| t.codec).collect();
    assert_eq!(codecs, vec![Codec::H264, Codec::Aac]);
    let src = origin.registry.get("cam").unwrap().tracks();
    assert_eq!(tracks[0].init, src.iter().find(|t| t.codec == Codec::H264).unwrap().init, "avcC travels unchanged");
    let first = next_frame(&mut v1).await;
    assert!(first.keyframe, "a viewer starts on a keyframe");
    println!("first viewer: first frame {:?} after the demand", t0.elapsed());

    // Decode timestamps keep increasing per track. The fixture has
    // B-frames: until the edge has seen the first reordered frame it cannot
    // know the reorder depth, so the first GOP may carry a couple of frames
    // with DTS above PTS (see timing.rs); none after that.
    let mut last = std::collections::HashMap::new();
    let mut above = Vec::new();
    for i in 0..200 {
        let f = next_frame(&mut v1).await;
        let prev = last.insert(f.track, f.dts);
        assert!(prev.is_none_or(|p| f.dts > p), "dts went back on {:?}", f.track);
        if f.pts < f.dts {
            above.push(i);
        }
    }
    assert!(above.len() <= 3 && above.iter().all(|&i| i < 40), "DTS above PTS at frames {above:?}");

    // Second viewer shares the same pull: same local stream, one MoQ
    // session upstream.
    let again = timeout(T, registry.get_or_demand("cam")).await.unwrap().expect("shared");
    assert!(Arc::ptr_eq(&stream, &again));
    let mut v2 = again.subscribe(StartAt::LiveEdge);
    next_frame(&mut v2).await;
    let pulls = edge.pulls();
    assert_eq!(pulls.len(), 1);
    assert_eq!(pulls[0].origin, origin.url().to_string());
    assert!(pulls[0].setup.is_some());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let upstream = origin.registry.get("cam").unwrap().stats().viewers;
        if upstream == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "origin sees {upstream} MoQ viewers, expected the one edge");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let mut metrics = String::new();
    edge.render_metrics(&mut metrics);
    assert!(metrics.contains("caudal_cluster_pulls{stream=\"cam\",origin=\""), "{metrics}");

    // Idle: both viewers leave; the pull stops after idle_timeout.
    drop(v1);
    drop(v2);
    let left = Instant::now();
    let deadline = left + Duration::from_secs(8);
    while registry.get("cam").is_some() || !edge.pulls().is_empty() {
        assert!(Instant::now() < deadline, "pull still running {:?} after the last viewer", left.elapsed());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!("idle teardown {:?} after the last viewer (idle_timeout 2 s)", left.elapsed());
    assert!(stream.is_ended());

    // A later viewer starts a new pull.
    let fresh = timeout(T, registry.get_or_demand("cam")).await.unwrap().expect("pulled again");
    assert!(!Arc::ptr_eq(&fresh, &stream));
}

/// Wall time between a video frame entering the origin's ring and the same
/// frame (matched by presentation time) entering the edge's: the latency
/// the edge hop adds before any output packages it. Printed, and bounded
/// loosely so a regression to "a GOP late" fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edge_hop_latency() {
    let origin = Origin::start("origin-a", Some("cam")).await;
    let (registry, _edge) = edge(&[&origin], SECRET, Duration::from_secs(5));
    let src = origin.registry.get("cam").unwrap();
    let stream = timeout(T, registry.get_or_demand("cam")).await.unwrap().expect("pulled");
    let mut at_origin = src.subscribe_internal(StartAt::LiveEdge);
    let mut at_edge = stream.subscribe(StartAt::LiveEdge);
    let (src_tracks, edge_tracks) = (src.tracks(), stream.tracks());
    let src_video = src_tracks.iter().find(|t| t.codec == Codec::H264).unwrap().clone();
    let edge_video = edge_tracks.iter().find(|t| t.codec == Codec::H264).unwrap().clone();

    let origin_task = tokio::spawn(async move {
        let mut seen = std::collections::HashMap::new();
        while seen.len() < 400 {
            let f = next_frame(&mut at_origin).await;
            if f.track == src_video.id {
                seen.insert(src_video.to_micros(f.pts) / 1000, Instant::now());
            }
        }
        seen
    });
    let mut edge_seen = Vec::new();
    while edge_seen.len() < 300 {
        let f = next_frame(&mut at_edge).await;
        if f.track == edge_video.id {
            edge_seen.push((edge_video.to_micros(f.pts) / 1000, Instant::now()));
        }
    }
    let origin_seen = origin_task.await.unwrap();
    // Skip the first frames: both subscriptions start with the GOP already
    // buffered, delivered at once, which says nothing about the hop.
    let mut hops: Vec<Duration> = edge_seen
        .iter()
        .skip(60)
        .filter_map(|(ms, t)| origin_seen.get(ms).and_then(|o| t.checked_duration_since(*o)))
        .collect();
    assert!(hops.len() > 100, "matched only {} frames", hops.len());
    hops.sort();
    let pct = |p: usize| hops[(hops.len() - 1) * p / 100];
    println!(
        "edge hop (origin ring -> edge ring, MoQ over loopback, debug build): n={} median={:?} p95={:?} max={:?}",
        hops.len(),
        pct(50),
        pct(95),
        pct(100)
    );
    assert!(pct(50) < Duration::from_millis(250), "median hop {:?}", pct(50));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_stream_is_a_quick_miss() {
    let origin = Origin::start("origin-a", None).await;
    let (registry, edge) = edge(&[&origin], SECRET, Duration::from_secs(2));
    let t0 = Instant::now();
    assert!(registry.get_or_demand("nope").await.is_none());
    assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
    assert!(edge.pulls().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_secret_is_refused() {
    let origin = Origin::start("origin-a", Some("cam")).await;
    let (registry, _edge) = edge(&[&origin], "some-other-cluster-secret", Duration::from_secs(2));
    assert!(timeout(T, registry.get_or_demand("cam")).await.unwrap().is_none());
    assert_eq!(origin.registry.get("cam").unwrap().stats().viewers, 0);

    // The locate endpoint itself: no token, wrong token, right token.
    let url = format!("http://{}/api/v1/cluster/locate/cam", origin.http);
    let http = reqwest::Client::new();
    assert_eq!(http.get(&url).send().await.unwrap().status(), 401);
    let bad = Secret::new("some-other-cluster-secret").unwrap().mint("x");
    assert_eq!(http.get(&url).bearer_auth(bad).send().await.unwrap().status(), 403);
    let good = Secret::new(SECRET).unwrap().mint("x");
    let res = http.get(&url).bearer_auth(&good).send().await.unwrap();
    assert_eq!(res.status(), 200);
    let v: serde_json::Value = res.json().await.unwrap();
    assert_eq!(v["node_id"], "origin-a");
    let missing = format!("http://{}/api/v1/cluster/locate/other", origin.http);
    assert_eq!(http.get(&missing).bearer_auth(&good).send().await.unwrap().status(), 404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failover_to_the_second_origin_keeps_the_viewer() {
    let mut a = Origin::start("origin-a", Some("cam")).await;
    let b = Origin::start("origin-b", Some("cam")).await;
    let (registry, edge) = edge(&[&a, &b], SECRET, Duration::from_secs(5));

    let stream = timeout(T, registry.get_or_demand("cam")).await.unwrap().expect("pulled");
    let mut viewer = stream.subscribe(StartAt::LiveEdge);
    for _ in 0..50 {
        next_frame(&mut viewer).await;
    }
    assert_eq!(edge.pulls()[0].origin, a.url().to_string());

    // Origin A loses the stream. The edge's viewer must keep receiving
    // frames (no End) from origin B.
    a.stop_feed().await;
    let cut = Instant::now();
    let mut last_dts = std::collections::HashMap::new();
    let mut gap = Duration::ZERO;
    let mut prev = Instant::now();
    let mut from_b = 0;
    while from_b < 100 {
        let f = next_frame(&mut viewer).await;
        gap = gap.max(prev.elapsed());
        prev = Instant::now();
        if let Some(p) = last_dts.insert(f.track, f.dts) {
            assert!(f.dts > p, "timeline went back across the failover");
        }
        if edge.pulls().first().is_some_and(|p| p.origin == b.url().to_string()) {
            from_b += 1;
        }
    }
    let p = &edge.pulls()[0];
    assert_eq!(p.failovers, 1);
    assert!(!stream.is_ended());
    println!(
        "failover: longest gap between frames {gap:?}; origin loss to first frame from B {:?} (test ran {:?})",
        p.setup.unwrap(),
        cut.elapsed()
    );
    assert!(gap < Duration::from_secs(5), "{gap:?}");
}
