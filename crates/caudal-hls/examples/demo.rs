//! Real check for the LL-HLS output without RTMP: demuxes a local MP4,
//! publishes it in real time (looping) into a `Registry` as stream `demo`,
//! and serves the router on 127.0.0.1:18080.
//!
//! ```text
//! cargo run -p caudal-hls --example demo [file.mp4]
//! ffprobe http://127.0.0.1:18080/hls/demo/index.m3u8
//! open http://127.0.0.1:18080/play/demo
//! ```

#[path = "../src/mp4demux.rs"]
mod mp4demux;

use std::time::Duration;

use caudal_core::{BufferConfig, Registry};
use caudal_hls::{HlsConfig, router};

#[tokio::main]
async fn main() {
    let path =
        std::env::args().nth(1).unwrap_or_else(|| concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/av.mp4").into());
    let file = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let fx = mp4demux::demux(&file);
    eprintln!("{path}: {} tracks, {} frames per loop", fx.tracks.len(), fx.frames.len());

    let registry = Registry::new();
    let app = router(registry.clone(), HlsConfig { part_ms: 200, segment_ms: 2000, cue_tags: true });

    let publisher = registry.publish("demo", BufferConfig::default()).unwrap();
    publisher.set_tracks(fx.tracks.clone()).unwrap();
    tokio::spawn(async move {
        let t0 = tokio::time::Instant::now();
        for n in 0..i64::MAX {
            for f in fx.looped(n) {
                let at = t0 + Duration::from_micros(fx.micros(&f).max(0) as u64);
                tokio::time::sleep_until(at).await;
                publisher.push(f).unwrap();
            }
        }
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:18080").await.unwrap();
    eprintln!("serving http://127.0.0.1:18080/hls/demo/index.m3u8 and http://127.0.0.1:18080/play/demo");
    axum::serve(listener, app).await.unwrap();
}
