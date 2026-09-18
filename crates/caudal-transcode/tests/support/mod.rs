//! Test sources: an ffmpeg `testsrc` clip read back through `caudal-ts`, or
//! the Opus MP4 fixture, published into a registry in real time.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use caudal_core::{BufferConfig, Frame, Registry, TrackInfo};
use caudal_ts::demux::{DemuxEvent, Demuxer};
use caudal_ts::ts::TsDemux;

#[path = "../../../caudal-hls/src/mp4demux.rs"]
pub mod mp4demux;

/// Logs to the test output with `RUST_LOG` (e.g. `caudal_transcode=debug`).
pub fn init_logs() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

pub struct Clip {
    pub tracks: Vec<TrackInfo>,
    /// Frames in push order (by decode time across tracks).
    pub frames: Vec<Frame>,
}

impl Clip {
    pub fn micros(&self, f: &Frame) -> i64 {
        self.tracks.iter().find(|t| t.id == f.track).unwrap().to_micros(f.dts)
    }
}

pub fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("caudal-transcode-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A `secs`-second 640x360 30 fps H.264 + 48 kHz AAC clip, via MPEG-TS.
pub fn testsrc_clip(dir: &Path, secs: u32) -> Clip {
    let path = dir.join("src.ts");
    let st = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i"])
        .arg("testsrc=size=640x360:rate=30")
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"])
        .args(["-t", &secs.to_string()])
        .args(["-c:v", "libx264", "-preset", "ultrafast", "-g", "45", "-bf", "0", "-threads", "2"])
        .args(["-c:a", "aac", "-b:a", "64k", "-f", "mpegts"])
        .arg(&path)
        .status()
        .unwrap();
    assert!(st.success());
    let bytes = std::fs::read(&path).unwrap();
    let mut ts = TsDemux::new();
    let mut demux = Demuxer::new();
    ts.feed(&bytes);
    let mut units = Vec::new();
    ts.drain(&mut units);
    let mut events = Vec::new();
    for u in units {
        demux.consume(u, &mut events);
    }
    let mut tracks: Vec<TrackInfo> = Vec::new();
    let mut frames = Vec::new();
    for ev in events {
        match ev {
            DemuxEvent::VideoInit(t) | DemuxEvent::AudioInit(t) => {
                if !tracks.iter().any(|x| x.id == t.id) {
                    tracks.push(t);
                }
            }
            DemuxEvent::VideoFrame(f) | DemuxEvent::AudioFrame(f) => frames.push(f),
        }
    }
    tracks.sort_by_key(|t| t.id);
    let mut clip = Clip { tracks, frames: Vec::new() };
    let mut keyed: Vec<(i64, Frame)> = frames.into_iter().map(|f| (clip_micros(&clip.tracks, &f), f)).collect();
    keyed.sort_by_key(|(m, f)| (*m, f.track));
    clip.frames = keyed.into_iter().map(|(_, f)| f).collect();
    clip
}

fn clip_micros(tracks: &[TrackInfo], f: &Frame) -> i64 {
    tracks.iter().find(|t| t.id == f.track).unwrap().to_micros(f.dts)
}

/// The 256x144 H.264 (with B-frames) + Opus fixture, looped `loops` times.
pub fn opus_clip(loops: i64) -> Clip {
    let bytes =
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../caudal-hls/tests/fixtures/av_opus.mp4")).unwrap();
    let d = mp4demux::demux(&bytes);
    let frames = (0..loops).flat_map(|n| d.looped(n).collect::<Vec<_>>()).collect();
    Clip { tracks: d.tracks.clone(), frames }
}

/// Publishes `clip` as `name`, pacing frames in real time; the stream ends
/// when the task finishes (or is aborted). `offset_us` shifts every
/// timestamp, to check renditions keep the source's clock.
pub fn publish(registry: &Arc<Registry>, name: &str, clip: Arc<Clip>, offset_us: i64) -> tokio::task::JoinHandle<()> {
    let publisher = registry.publish(name, BufferConfig::default()).unwrap();
    publisher.set_tracks(clip.tracks.clone()).unwrap();
    tokio::spawn(async move {
        let start = Instant::now();
        let first = clip.micros(&clip.frames[0]);
        for f in &clip.frames {
            let at = Duration::from_micros((clip.micros(f) - first).max(0) as u64);
            tokio::time::sleep_until((start + at).into()).await;
            let info = clip.tracks.iter().find(|t| t.id == f.track).unwrap();
            let d = offset_us * i64::from(info.timescale) / 1_000_000;
            let _ = publisher.push(Frame { dts: f.dts + d, pts: f.pts + d, ..f.clone() });
        }
        drop(publisher);
    })
}

/// PIDs of ffmpeg processes serving source `name`.
pub fn ffmpeg_pids(name: &str) -> Vec<u32> {
    let out = Command::new("pgrep").args(["-f", &format!("service_name={name} pipe:1")]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.trim().parse().ok()).collect()
}

pub async fn wait_for<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
