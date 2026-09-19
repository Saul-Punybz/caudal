//! End-to-end: MoQ ingest, against the real binary.
//!
//!   in-process `hang` publisher -> caudal (publish/<name>) -> LL-HLS -> ffmpeg
//!
//! No publisher CLI ships for the pinned moq-net draft (`moq-clock`/
//! `moq-cli`-style tools target the moq-lite predecessor), so this acts as
//! one: an in-process encoder built from the same `moq-net`/`hang`/
//! `moq-mux`/`moq-native` crates caudal-moq itself uses, the fallback batch
//! 11's brief calls out explicitly. It demuxes the same fixture
//! `caudal-hls`'s tests use and republishes it as a `hang` broadcast over a
//! real QUIC/WebTransport connection to the running server.
//!
//! Runs only with `CAUDAL_E2E=1` and ffmpeg on PATH, so unit test runs stay
//! fast.

mod support;

#[path = "../../caudal-hls/src/mp4demux.rs"]
mod mp4demux;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use caudal_core::Codec;
use hang::catalog::Container as CatalogContainer;
use moq_mux::catalog::hang::Container as WireContainer;
use support::{Server, have};

fn enabled() -> bool {
    if std::env::var("CAUDAL_E2E").is_err() {
        eprintln!("SKIP: set CAUDAL_E2E=1 to run end-to-end tests");
        return false;
    }
    if !have("ffmpeg") {
        eprintln!("SKIP: ffmpeg not on PATH");
        return false;
    }
    true
}

struct KillOnDrop(Option<std::process::Child>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Publishes the `av.mp4` fixture (H.264 + AAC) as a `hang` broadcast to
/// `url` over WebTransport, pinning `fingerprint`, looped at 2x real time
/// until `stop` is set. Returns once the session is connected and
/// announcing; the feeder runs in its own task.
async fn publish_moq(url: url::Url, fingerprint: String, stop: Arc<AtomicBool>) -> moq_net::Session {
    let fx = mp4demux::demux(include_bytes!("../../caudal-hls/tests/fixtures/av.mp4"));
    let video = fx.tracks.iter().find(|t| t.codec == Codec::H264).expect("h264 track in fixture").clone();
    let audio = fx.tracks.iter().find(|t| t.codec == Codec::Aac).expect("aac track in fixture").clone();

    let origin = moq_net::Origin::random().produce();
    let mut broadcast =
        origin.create_broadcast("", moq_net::broadcast::Route::new().with_announce(true)).expect("create broadcast");
    let mut catalog = moq_mux::catalog::Producer::new(&mut broadcast).expect("catalog producer");

    let vtrack = broadcast.create_track("video0", hang::container::track_info()).expect("video track");
    let mut vprod = moq_mux::container::Producer::new(vtrack, WireContainer::Legacy);
    let atrack = broadcast.create_track("audio0", hang::container::track_info()).expect("audio track");
    let mut aprod = moq_mux::container::Producer::new(atrack, WireContainer::Legacy);

    {
        let mut vcfg = moq_mux::codec::h264::config(&video.init).expect("avcC");
        vcfg.container = CatalogContainer::Legacy;
        let mut acfg = moq_mux::codec::aac::config(&audio.init).expect("AudioSpecificConfig");
        acfg.container = CatalogContainer::Legacy;
        let mut c = catalog.lock();
        c.video.insert("video0", vcfg).expect("insert video rendition");
        c.audio.insert("audio0", acfg).expect("insert audio rendition");
        c.commit().expect("commit catalog");
    }

    let (video_id, audio_id) = (video.id, audio.id);
    tokio::spawn(async move {
        let start = tokio::time::Instant::now();
        for n in 0.. {
            for f in fx.looped(n) {
                if stop.load(Ordering::Relaxed) {
                    let _ = vprod.finish();
                    let _ = aprod.finish();
                    let _ = catalog.finish();
                    broadcast.finish();
                    return;
                }
                let at = start + Duration::from_micros(fx.micros(&f) as u64 / 2);
                tokio::time::sleep_until(at).await;
                let Ok(timestamp) = moq_net::Timestamp::from_micros(fx.micros(&f).max(0) as u64) else { continue };
                let frame = moq_mux::container::Frame {
                    timestamp,
                    payload: f.data.clone(),
                    keyframe: f.keyframe,
                    duration: None,
                };
                if f.track == video_id {
                    let _ = vprod.write(frame);
                } else if f.track == audio_id {
                    let _ = aprod.write(frame);
                }
            }
        }
    });

    let mut client_cfg = moq_native::ClientConfig::default();
    client_cfg.tls.fingerprint = vec![fingerprint];
    let client = client_cfg.init().expect("moq client");
    tokio::time::timeout(Duration::from_secs(10), client.with_publisher(origin.consume()).connect(url))
        .await
        .expect("connect timed out")
        .expect("connect")
}

/// A stream published over MoQ ingest plays back over LL-HLS, exactly like
/// any other ingest protocol.
#[test]
fn moq_publish_plays_over_hls() {
    if !enabled() {
        return;
    }
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let s = Server::start();

    let (_, fp_json) = s.get("/moq/fingerprint").expect("fingerprint request");
    let fp: serde_json::Value = serde_json::from_str(&fp_json).expect("fingerprint json");
    let base: url::Url = fp["url"].as_str().expect("url").parse().expect("parse moq url");
    let fingerprint = fp["fingerprint"].as_str().expect("fingerprint").to_owned();
    let publish_url = base.join("publish/moqcam").expect("publish url");

    let stop = Arc::new(AtomicBool::new(false));
    let session = rt.block_on(publish_moq(publish_url, fingerprint, stop.clone()));

    // Caudal converted the broadcast into a normal publish: it shows up in
    // the streams API with the tracks a hang broadcast carries.
    let body = s.wait_until("/api/v1/streams/moqcam", Duration::from_secs(15), |b| {
        b.contains("\"h264\"") && b.contains("\"aac\"")
    });
    assert!(body.contains("\"name\":\"moqcam\""), "{body}");

    // And it plays over LL-HLS, exactly like any other ingest protocol.
    s.wait_until("/hls/moqcam/index.m3u8", Duration::from_secs(20), |b| b.matches("#EXTINF").count() >= 2);

    let reader = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-nostats", "-loglevel", "error", "-i"])
        .arg(s.url("/hls/moqcam/index.m3u8"))
        .args(["-t", "5", "-f", "null", "-"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn ffmpeg reader");
    let mut reader = KillOnDrop(Some(reader));
    let t0 = Instant::now();
    let status = loop {
        if let Some(status) = reader.0.as_mut().unwrap().try_wait().expect("ffmpeg wait") {
            break status;
        }
        assert!(t0.elapsed() < Duration::from_secs(30), "ffmpeg reader never exited");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(status.success(), "ffmpeg could not play the MoQ-ingested stream over LL-HLS: {status:?}");

    stop.store(true, Ordering::Relaxed);
    // `Session`'s drop (quinn/web-transport-quinn) needs a runtime context
    // on the dropping thread; this test thread is outside `rt` once
    // `block_on` returns, so enter it explicitly rather than let the drop
    // panic looking for a reactor.
    {
        let _enter = rt.enter();
        drop(session);
    }
    rt.shutdown_timeout(Duration::from_secs(5));
}
