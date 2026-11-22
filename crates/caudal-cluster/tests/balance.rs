//! Load balancing with three nodes in one process, over real HTTP on
//! loopback: an origin (balancer, locate, the viewer endpoints) and two
//! edges whose reporters send it their load. The load each node reports
//! comes from a test-controlled sampler, so the test drives the scores.

use std::sync::Arc;
use std::time::Duration;

use caudal_cluster::{BalanceConfig, Balancer, GeoPoint, ReporterConfig, Sample, Secret, spawn_reporter};
use caudal_core::{BufferConfig, Registry};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::time::Instant;

const SECRET: &str = "loopback-cluster-secret";
const INTERVAL: Duration = Duration::from_millis(100);
const STALE: Duration = Duration::from_millis(600);

type Load = Arc<Mutex<Sample>>;

fn reporter(id: &str, origin: &url::Url, load: &Load, location: Option<GeoPoint>) -> tokio::task::JoinHandle<()> {
    let load = load.clone();
    spawn_reporter(
        ReporterConfig {
            node_id: id.into(),
            secret: Secret::new(SECRET).unwrap(),
            origins: vec![origin.clone()],
            interval: INTERVAL,
            public_url: Some(format!("http://{id}.example:8080")),
            accepts_viewers: true,
            capacity_bps: 100_000_000,
            location,
            country: None,
        },
        None,
        move || load.lock().clone(),
    )
}

struct Origin {
    base: url::Url,
    http: reqwest::Client,
    _publisher: caudal_core::Publisher,
}

impl Origin {
    async fn start() -> Self {
        let registry = Registry::new();
        let publisher = registry.publish("cam", BufferConfig::default()).unwrap();
        let secret = Secret::new(SECRET).unwrap();
        let balancer = Balancer::new(BalanceConfig { stale_after: STALE, ..BalanceConfig::default() });
        // The origin reports itself too, but takes no viewers.
        spawn_reporter(
            ReporterConfig {
                node_id: "origin".into(),
                secret: secret.clone(),
                origins: vec![],
                interval: INTERVAL,
                public_url: Some("http://origin.example:8080".into()),
                accepts_viewers: false,
                capacity_bps: 100_000_000,
                location: None,
                country: None,
            },
            Some(balancer.clone()),
            || Sample { streams: vec!["cam".into()], ..Sample::default() },
        );
        let app = caudal_cluster::origin_router(registry, secret.clone(), "origin".into(), Some(balancer.clone()))
            .merge(caudal_cluster::balance::router(balancer, secret, None, vec![]));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.unwrap()
        });
        let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        Self { base: format!("http://{addr}").parse().unwrap(), http, _publisher: publisher }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base.as_str().trim_end_matches('/'), path)
    }

    /// `best.node_id` from the edge-facing locate.
    async fn locate_best(&self) -> Option<String> {
        let token = Secret::new(SECRET).unwrap().mint("edge-test");
        let v: Value = self
            .http
            .get(self.url("/api/v1/cluster/locate/cam"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        v["best"]["node_id"].as_str().map(str::to_owned)
    }

    /// Polls locate until it names `want` (or `None`: no candidate).
    async fn wait_best(&self, want: Option<&str>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let got = self.locate_best().await;
            if got.as_deref() == want {
                return;
            }
            assert!(Instant::now() < deadline, "locate best is {got:?}, expected {want:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn sample(cpu: f64, bw: u64, streams: &[&str]) -> Sample {
    Sample {
        cpu_percent: cpu,
        ram_percent: 20.0,
        bw_out_bps: bw,
        viewers: 0,
        streams: streams.iter().map(|s| (*s).to_owned()).collect(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn load_reports_change_the_answer() {
    let origin = Origin::start().await;
    let a: Load = Arc::new(Mutex::new(sample(10.0, 0, &["cam"])));
    let b: Load = Arc::new(Mutex::new(sample(10.0, 0, &[])));
    let edge_a = reporter("edge-a", &origin.base, &a, None);
    let _edge_b = reporter("edge-b", &origin.base, &b, None);

    // Equal load: edge-a already has the stream (the bonus).
    origin.wait_best(Some("edge-a")).await;

    // edge-a gets busy: edge-b wins.
    *a.lock() = sample(85.0, 0, &["cam"]);
    origin.wait_best(Some("edge-b")).await;

    // edge-b's link fills up (hard limit): back to edge-a.
    *b.lock() = sample(10.0, 100_000_000, &[]);
    origin.wait_best(Some("edge-a")).await;

    // The admin table shows why edge-b is out and the origin too.
    let table: Value =
        origin.http.get(origin.url("/api/v1/balance?stream=cam")).send().await.unwrap().json().await.unwrap();
    let nodes = table["nodes"].as_array().unwrap();
    let reason = |id: &str| nodes.iter().find(|n| n["node_id"] == id).unwrap()["excluded"].clone();
    assert_eq!(reason("edge-a"), Value::Null);
    assert_eq!(reason("edge-b"), "over_bandwidth");
    assert_eq!(reason("origin"), "refuses_viewers");
    assert_eq!(table["weights"]["bw"], 1000);

    // edge-a stops reporting: once stale nobody can take a viewer.
    edge_a.abort();
    origin.wait_best(None).await;
    let r = origin.http.get(origin.url("/cluster/balance/cam")).send().await.unwrap();
    assert_eq!(r.status(), 503);

    // edge-b recovers; the viewer endpoints send viewers there, and the
    // redirect keeps the stream token.
    *b.lock() = sample(10.0, 0, &[]);
    origin.wait_best(Some("edge-b")).await;
    let v: Value = origin.http.get(origin.url("/cluster/balance/cam")).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["node_id"], "edge-b");
    assert_eq!(v["url"], "http://edge-b.example:8080");
    let r = origin.http.get(origin.url("/cluster/redirect/cam?proto=hls&token=abc")).send().await.unwrap();
    assert_eq!(r.status(), 307);
    assert_eq!(r.headers()["location"], "http://edge-b.example:8080/hls/cam/master.m3u8?token=abc");
    let r = origin.http.get(origin.url("/cluster/redirect/cam?proto=rtmp")).send().await.unwrap();
    assert_eq!(r.status(), 400);

    // Two viewers were sent to edge-b (the refused redirect sent none;
    // locate only asks).
    assert_eq!(table_picks(&origin, "edge-b").await, 2);
}

async fn table_picks(origin: &Origin, id: &str) -> u64 {
    let table: Value = origin.http.get(origin.url("/api/v1/balance")).send().await.unwrap().json().await.unwrap();
    table["nodes"].as_array().unwrap().iter().find(|n| n["node_id"] == id).unwrap()["picks"].as_u64().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn viewer_position_picks_the_nearer_node() {
    let origin = Origin::start().await;
    let idle: Load = Arc::new(Mutex::new(sample(10.0, 0, &[])));
    let _sj = reporter("san-juan", &origin.base, &idle, Some(GeoPoint { lat: 18.4655, lon: -66.1057 }));
    let _md = reporter("madrid", &origin.base, &idle, Some(GeoPoint { lat: 40.4168, lon: -3.7038 }));
    // Both known (no viewer position: equal scores, lower id wins).
    origin.wait_best(Some("madrid")).await;

    let near_sj: Value = origin
        .http
        .get(origin.url("/cluster/balance/cam"))
        .header("X-Latitude", "18.2")
        .header("X-Longitude", "-66.5")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(near_sj["node_id"], "san-juan");
    let near_md: Value = origin
        .http
        .get(origin.url("/cluster/balance/cam?lat=41.4&lon=2.2"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(near_md["node_id"], "madrid");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reports_need_the_cluster_token_of_the_same_node() {
    let origin = Origin::start().await;
    let body = serde_json::json!({
        "node_id": "edge-x", "url": "http://x", "accepts_viewers": true, "cpu_percent": 0.0,
        "ram_percent": 0.0, "bw_out_bps": 0, "bw_capacity_bps": 1000, "viewers": 0,
        "streams": [], "location": null, "country": null
    });
    let post = |token: Option<String>| {
        let mut r = origin.http.post(origin.url("/api/v1/cluster/report")).json(&body);
        if let Some(t) = token {
            r = r.bearer_auth(t);
        }
        r.send()
    };
    assert_eq!(post(None).await.unwrap().status(), 401);
    let other = Secret::new("some-other-secret!").unwrap().mint("edge-x");
    assert_eq!(post(Some(other)).await.unwrap().status(), 403);
    let wrong_node = Secret::new(SECRET).unwrap().mint("edge-y");
    assert_eq!(post(Some(wrong_node)).await.unwrap().status(), 400);
    let ok = Secret::new(SECRET).unwrap().mint("edge-x");
    assert_eq!(post(Some(ok)).await.unwrap().status(), 204);
}
