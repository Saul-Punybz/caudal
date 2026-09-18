//! SCTE-35 over SRT, both directions, with no external tools: a Rust SRT
//! caller publishes MPEG-TS carrying a cue on its own PID (stream type
//! 0x86), the cue lands in the stream at the right media time, and a
//! second caller pulling the stream gets it back on the TS output's cue
//! PID, re-timed onto that output's clock.
//!
//! The TS is muxed by `caudal_ts::TsMux`, so this is a loopback of our own
//! writer and reader over a real SRT transport. A TSDuck (`tsp`) test with
//! an independently written cue is not here: `tsp` is not installed on the
//! machine this was written on.

use std::time::Duration;

use bytes::Bytes;
use caudal_core::{AudioParams, BufferConfig, Codec, Cue, CueKind, Event, Frame, Registry, StartAt, TrackId};
use caudal_core::{TrackInfo, VideoParams};
use caudal_scte35::{Command, build, ticks_to_us};
use caudal_srt::{SrtConfig, serve};
use caudal_ts::demux::{DemuxEvent, Demuxer};
use caudal_ts::mux::TsMux;
use caudal_ts::ts::TsDemux;
use rsrt::{SrtOptions, SrtSocket};

const SPS: &[u8] = &[0x67, 0x42, 0x00, 0x0A, 0x8C, 0x8D, 0x40, 0x50, 0x1E, 0xD0, 0x0F, 0x08, 0x84, 0x6A];
const PPS: &[u8] = &[0x68, 0xCE, 0x3C, 0x80];
/// The source clock starts at 10 s; Caudal rebases it to 0.
const BASE: i64 = 900_000;

fn video() -> TrackInfo {
    // avcC by hand: version, profile, compat, level, 4-byte lengths, 1 SPS, 1 PPS.
    let mut init = vec![1, SPS[1], SPS[2], SPS[3], 0xFF, 0xE1];
    init.extend_from_slice(&(SPS.len() as u16).to_be_bytes());
    init.extend_from_slice(SPS);
    init.push(1);
    init.extend_from_slice(&(PPS.len() as u16).to_be_bytes());
    init.extend_from_slice(PPS);
    TrackInfo {
        id: TrackId(0),
        codec: Codec::H264,
        timescale: 90_000,
        init: Bytes::from(init),
        lang: None,
        video: Some(VideoParams { width: 16, height: 16, fps: None }),
        audio: None,
    }
}

fn audio() -> TrackInfo {
    let asc: u16 = (2 << 11) | (3 << 7) | (2 << 3); // AAC-LC, 48 kHz, stereo
    TrackInfo {
        id: TrackId(1),
        codec: Codec::Aac,
        timescale: 48_000,
        init: Bytes::copy_from_slice(&asc.to_be_bytes()),
        lang: None,
        video: None,
        audio: Some(AudioParams { sample_rate: 48_000, channels: 2 }),
    }
}

/// Frame `n` of a 30 fps source, keyframe every 10, plus the audio
/// frame(s) that start before the next video frame.
fn mux_frame(mux: &mut TsMux, n: i64) {
    let (v, a) = (video(), audio());
    let dts = BASE + n * 3000;
    let vf = Frame {
        track: TrackId(0),
        dts,
        pts: dts,
        keyframe: n % 10 == 0,
        data: Bytes::from_static(&[0, 0, 0, 1, 0x65]),
    };
    mux.push_frame(&v, &vf);
    let ats = (i128::from(dts) * 48_000 / 90_000) as i64;
    let af = Frame { track: TrackId(1), dts: ats, pts: ats, keyframe: true, data: Bytes::from_static(&[0x21; 16]) };
    mux.push_frame(&a, &af);
}

async fn send(socket: &SrtSocket, ts: &[u8]) {
    for chunk in ts.chunks(1316) {
        socket.send(chunk).await.expect("srt send");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cue_over_srt_reaches_the_stream_and_comes_back_out() {
    let port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let registry = Registry::new();
    let cfg = SrtConfig {
        bind: format!("127.0.0.1:{port}").parse().unwrap(),
        latency_ms: 120,
        passphrase: None,
        buffer: BufferConfig::default(),
        pushes: Vec::new(),
    };
    let server = tokio::spawn(serve(cfg, registry.clone()));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut publishes = registry.subscribe_publishes();

    let publisher =
        SrtSocket::connect(format!("127.0.0.1:{port}"), SrtOptions::default().streamid("publish/cue")).await.unwrap();
    let stream = tokio::time::timeout(Duration::from_secs(5), publishes.recv()).await.unwrap().unwrap();
    let mut sub = stream.subscribe_internal(StartAt::Oldest);

    let (v, a) = (video(), audio());
    let mut mux = TsMux::new();
    mux.set_tracks(&[v, a]);
    for n in 0..15 {
        mux_frame(&mut mux, n);
    }
    send(&publisher, &mux.take_output()).await;

    // Tracks announced: now a viewer pulls the stream over SRT.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while stream.tracks().len() < 2 {
        assert!(tokio::time::Instant::now() < deadline, "tracks never announced");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut player =
        SrtSocket::connect(format!("127.0.0.1:{port}"), SrtOptions::default().streamid("play/cue")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The cue: a 20 s break splicing at source PTS BASE + 1.5 s (0.5 s
    // after it is sent), then enough frames to carry it through.
    let kind = CueKind::Out { duration_us: Some(20_000_000) };
    let splice_at = BASE + 45_000;
    let cue =
        Cue { at_us: ticks_to_us(splice_at), section: build(kind, Some(0), 42, Command::SpliceInsert).unwrap(), kind };
    for n in 15..60 {
        mux_frame(&mut mux, n);
        if n == 30 {
            mux.push_cue(&cue);
        }
        send(&publisher, &mux.take_output()).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // 1. In the stream, on the rebased clock.
    let got = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Event::Cue(c) = sub.recv().await {
                return c;
            }
        }
    })
    .await
    .expect("no cue reached the stream");
    assert_eq!(got.kind, kind);
    assert_eq!(got.at_us, ticks_to_us(45_000), "1.5 s after the first DTS");

    // 2. Out of the SRT play output, on its own PID, splicing at that same
    // media time on the output clock.
    let mut demux = TsDemux::new();
    let mut demuxer = Demuxer::new();
    let (mut units, mut events) = (Vec::new(), Vec::new());
    let out_cue = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let Ok(Some(payload)) = player.recv().await else { panic!("player connection ended") };
            demux.feed(&payload);
            demux.drain(&mut units);
            for u in units.drain(..) {
                demuxer.consume(u, &mut events);
            }
            if let Some(c) = events.drain(..).find_map(|e| if let DemuxEvent::Cue(c) = e { Some(c) } else { None }) {
                return c;
            }
        }
    })
    .await
    .expect("no cue in the SRT play output");
    assert_eq!(out_cue.kind, kind);
    let splice = caudal_scte35::parse(&out_cue.section).unwrap();
    assert_eq!(splice.pts_90k, Some(45_000), "re-timed onto the output clock");

    drop(player);
    drop(publisher);
    server.abort();
}
