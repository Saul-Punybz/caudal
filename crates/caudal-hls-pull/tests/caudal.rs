//! One Caudal pulling another's LL-HLS (fMP4 / CMAF segments): frames
//! published into one registry, served by the real `caudal-hls` router over
//! a real local HTTP listener, pulled with `[[hls.pull]]` into a second
//! registry, and compared frame by frame.

#[path = "../../caudal-hls/src/mp4demux.rs"]
mod mp4demux;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use caudal_core::{BufferConfig, Codec, Event, Frame, Registry, StartAt, TrackId};
use caudal_hls::HlsConfig;
use caudal_hls_pull::{HlsPull, start_pulls};

/// Serves `registry` with Caudal's HLS router, as `[http]` would.
async fn origin(registry: Arc<Registry>) -> SocketAddr {
    let cfg = HlsConfig { reconnect_grace: Duration::ZERO, ..HlsConfig::default() };
    let app = caudal_hls::router(registry, cfg, Vec::new());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
    });
    addr
}

async fn wait_for<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// `(dts, pts - dts, keyframe, payload length)` per frame of one track,
/// with decode times relative to the track's first frame.
fn timeline(frames: &[Frame], track: TrackId) -> Vec<(i64, i64, bool, usize)> {
    let mine: Vec<&Frame> = frames.iter().filter(|f| f.track == track).collect();
    let t0 = mine.first().map_or(0, |f| f.dts);
    mine.iter().map(|f| (f.dts - t0, f.pts - f.dts, f.keyframe, f.data.len())).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn caudal_pulls_caudal_ll_hls() {
    let fx = mp4demux::demux(include_bytes!("../../caudal-hls/tests/fixtures/av.mp4"));
    let source = Registry::new();
    let addr = origin(source.clone()).await;

    // Three loops of the 4 s fixture (H.264 with B-frames + AAC): 12 s,
    // six 2 s segments. The publisher then leaves, so the playlist gets
    // EXT-X-ENDLIST and the pull plays it from its first segment.
    let publisher = source.publish("cam", BufferConfig::default()).unwrap();
    publisher.set_tracks(fx.tracks.clone()).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let mut sent = Vec::new();
    for n in 0..3 {
        for f in fx.looped(n) {
            sent.push(f.clone());
            publisher.push(f).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    drop(publisher);
    let client = reqwest::Client::new();
    let playlist = format!("http://{addr}/hls/cam/index.m3u8");
    let mut text = String::new();
    for _ in 0..100 {
        text = client.get(&playlist).send().await.unwrap().text().await.unwrap();
        if text.contains("#EXT-X-ENDLIST") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(text.contains("#EXT-X-ENDLIST") && text.contains("#EXT-X-MAP:"), "fMP4 playlist, ended:\n{text}");

    let edge = Registry::new();
    let _pulls = start_pulls(
        edge.clone(),
        BufferConfig::default(),
        vec![HlsPull { stream: "relay".into(), url: format!("http://{addr}/hls/cam/master.m3u8") }],
    );
    let stream = wait_for(Duration::from_secs(20), || edge.get("relay")).await.expect("published on the edge");
    let mut viewer = stream.subscribe(StartAt::Oldest);
    let mut got = Vec::new();
    let done = tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            match viewer.recv().await {
                Event::Frame(f) => got.push(Frame::clone(&f)),
                Event::End => break,
                _ => {}
            }
        }
    })
    .await;
    assert!(done.is_ok(), "the pulled stream ended after EXT-X-ENDLIST");

    let tracks = viewer.tracks();
    let codecs: Vec<Codec> = tracks.iter().map(|t| t.codec).collect();
    assert_eq!(codecs, [Codec::H264, Codec::Aac], "H.264 + AAC announced from the init segment");
    for (pulled, orig) in tracks.iter().zip(&fx.tracks) {
        assert_eq!(pulled.timescale, orig.timescale, "{:?} timescale", orig.codec);
        assert_eq!(pulled.init, orig.init, "{:?} codec record", orig.codec);
    }
    for (pulled, orig) in tracks.iter().zip(&fx.tracks) {
        let want = timeline(&sent, orig.id);
        let have = timeline(&got, pulled.id);
        assert_eq!(have.len(), want.len(), "{:?}: every frame came through", orig.codec);
        assert!(have == want, "{:?}: same timestamps, offsets, keyframes and sizes", orig.codec);
    }
    let payloads_match =
        sent.iter().filter(|f| f.track == TrackId(0)).zip(got.iter().filter(|f| f.track == tracks[0].id));
    assert!(payloads_match.into_iter().all(|(a, b)| a.data == b.data), "video payloads byte for byte");
}
