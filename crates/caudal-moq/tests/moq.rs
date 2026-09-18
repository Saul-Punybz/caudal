//! End to end: a registry stream (H.264 + AAC from the caudal-hls fixture)
//! goes out as a MoQ broadcast, and a moq-native client pinning the
//! self-signed certificate by fingerprint reads it back over WebTransport.

#[path = "../../caudal-hls/src/mp4demux.rs"]
mod mp4demux;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use caudal_core::{BufferConfig, Codec, Registry};
use caudal_moq::{MoqCert, MoqConfig};
use tower::ServiceExt;

const T: Duration = Duration::from_secs(10);

async fn fingerprint(svc: &caudal_moq::MoqService) -> serde_json::Value {
    let req = Request::get("/moq/fingerprint").header("host", "127.0.0.1:8080").body(Body::empty()).unwrap();
    let res = svc.router().oneshot(req).await.unwrap();
    let body = axum::body::to_bytes(res.into_body(), 1 << 16).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

/// NAL unit types of a 4-byte length-prefixed access unit.
fn nal_types(mut au: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    while au.len() >= 5 {
        let len = u32::from_be_bytes(au[..4].try_into().unwrap()) as usize;
        types.push(au[4] & 0x1f);
        au = &au[(4 + len).min(au.len())..];
    }
    types
}

async fn next_group(track: &mut moq_net::track::Subscriber) -> moq_net::group::Consumer {
    tokio::time::timeout(T, track.recv_group()).await.expect("group timed out").expect("group").expect("track ended")
}

async fn read(group: &mut moq_net::group::Consumer) -> Option<hang::container::Frame> {
    let f = tokio::time::timeout(T, group.read_frame()).await.expect("frame timed out").expect("frame")?;
    Some(hang::container::Frame::decode(f.payload).expect("legacy frame"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_plays_over_moq_and_ends() {
    let fx = mp4demux::demux(include_bytes!("../../caudal-hls/tests/fixtures/av.mp4"));
    let video = fx.tracks.iter().find(|t| t.codec == Codec::H264).expect("h264 track").clone();
    assert!(fx.tracks.iter().any(|t| t.codec == Codec::Aac));

    let registry = Registry::new();
    let svc = caudal_moq::start(
        registry.clone(),
        MoqConfig { bind: "127.0.0.1:0".parse().unwrap(), cert: MoqCert::SelfSigned { hosts: vec!["localhost".into()] } },
    )
    .expect("start");
    let info = fingerprint(&svc).await;
    let url: url::Url = info["url"].as_str().unwrap().parse().unwrap();
    assert_eq!(url.host_str(), Some("127.0.0.1"));
    let fp = info["fingerprint"].as_str().unwrap().to_owned();

    // Publisher: the fixture, looped at 2x real time until told to stop.
    let publisher = registry.publish("cam", BufferConfig::default()).unwrap();
    let stream = publisher.stream().clone();
    publisher.set_tracks(fx.tracks.clone()).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = {
        let stop = stop.clone();
        tokio::spawn(async move {
            let start = tokio::time::Instant::now();
            for n in 0.. {
                for f in fx.looped(n) {
                    if stop.load(Ordering::Relaxed) {
                        drop(publisher); // ends the stream
                        return;
                    }
                    let at = start + Duration::from_micros(fx.micros(&f) as u64 / 2);
                    tokio::time::sleep_until(at).await;
                    publisher.push(f).unwrap();
                }
            }
        })
    };

    // Client: WebTransport, certificate pinned by fingerprint only.
    let mut client_cfg = moq_native::ClientConfig::default();
    client_cfg.tls.fingerprint = vec![fp];
    let client = client_cfg.init().expect("client");
    let sub_origin = moq_net::Origin::random().produce();
    let session = tokio::time::timeout(T, client.with_subscriber(sub_origin.clone()).connect(url))
        .await
        .expect("connect timed out")
        .expect("connect");
    let bc = tokio::time::timeout(T, sub_origin.consume().announced_broadcast("cam"))
        .await
        .expect("announce timed out")
        .expect("announced");

    // Catalog: H.264 with the avcC as description and the fixture's size; AAC.
    let mut cat_track = bc.track(hang::Catalog::DEFAULT_NAME).unwrap().subscribe(None).await.expect("catalog sub");
    let mut g = next_group(&mut cat_track).await;
    let raw = tokio::time::timeout(T, g.read_frame()).await.unwrap().unwrap().expect("catalog frame");
    let catalog = hang::Catalog::from_slice(&raw.payload).expect("catalog json");
    let (vname, vcfg) = catalog.video.renditions.iter().next().expect("video rendition");
    assert!(vcfg.codec.to_string().starts_with("avc1."), "{}", vcfg.codec);
    assert_eq!(vcfg.coded_width, Some(256));
    assert_eq!(vcfg.coded_height, Some(144));
    assert_eq!(vcfg.description.as_deref(), Some(&video.init[..]));
    assert_eq!(vcfg.container, hang::catalog::Container::Legacy);
    let (_, acfg) = catalog.audio.renditions.iter().next().expect("audio rendition");
    assert_eq!(acfg.codec.to_string(), "mp4a.40.2");
    assert_eq!(acfg.sample_rate, 48_000);

    // Video: groups open on an IDR; timestamps move forward.
    let mut vtrack = bc.track(vname).unwrap().subscribe(None).await.expect("video sub");
    let mut group_starts = Vec::new();
    for _ in 0..3 {
        let mut g = next_group(&mut vtrack).await;
        let first = read(&mut g).await.expect("first frame");
        assert!(nal_types(&first.payload).contains(&5), "group does not open on an IDR: {:?}", nal_types(&first.payload));
        let mut n = 1;
        while let Some(f) = read(&mut g).await {
            // B-frames reorder presentation times, but never before the IDR.
            assert!(f.timestamp >= first.timestamp);
            assert!(!nal_types(&f.payload).contains(&5));
            n += 1;
        }
        assert!(n > 1, "group with a single frame");
        group_starts.push(first.timestamp);
    }
    assert!(group_starts.windows(2).all(|w| w[0] < w[1]), "{group_starts:?}");

    // The subscribed session counts as one MoQ viewer.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while stream.stats().viewers < 1 {
        assert!(tokio::time::Instant::now() < deadline, "moq viewer never counted");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // End of the source ends the broadcast for the player.
    stop.store(true, Ordering::Relaxed);
    feeder.await.unwrap();
    tokio::time::timeout(T, bc.closed()).await.expect("broadcast did not end");
    drop(session);
}
