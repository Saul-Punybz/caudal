//! SCTE-35 through MPEG-TS: `TsMux` writes a cue on its own PID (stream
//! type 0x86, CUEI registration), `TsDemux` + `Demuxer` read it back onto
//! the same timeline as the frames, across a 33-bit clock wrap too.

use bytes::Bytes;
use caudal_core::{Codec, Cue, CueKind, Frame, TrackId, TrackInfo, VideoParams};
use caudal_scte35::{Command, build, ticks_to_us};
use caudal_ts::demux::{DemuxEvent, Demuxer};
use caudal_ts::mux::TsMux;
use caudal_ts::ts::TsDemux;
use mp4_atom::{Atom, Avcc};

const WRAP: i64 = 1 << 33;

fn video() -> TrackInfo {
    let sps: &[u8] = &[0x67, 0x42, 0x00, 0x0A, 0x8C, 0x8D, 0x40, 0x50, 0x1E, 0xD0, 0x0F, 0x08, 0x84, 0x6A];
    let pps: &[u8] = &[0x68, 0xCE, 0x3C, 0x80];
    let mut init = Vec::new();
    Avcc::new(sps, pps).unwrap().encode_body(&mut init).unwrap();
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

fn frame(dts: i64) -> Frame {
    Frame { track: TrackId(0), dts, pts: dts, keyframe: true, data: Bytes::from_static(&[0, 0, 0, 1, 0x65]) }
}

/// Muxes `frames` with `cue` after the `cue_after`-th frame, demuxes the
/// bytes, and returns the cues seen plus how many frames came out.
fn round_trip(frames: &[i64], cue_after: usize, cue: &Cue) -> (Vec<Cue>, usize) {
    let v = video();
    let mut mux = TsMux::new();
    mux.set_tracks(std::slice::from_ref(&v));
    let mut ts = Vec::new();
    for (i, &dts) in frames.iter().enumerate() {
        mux.push_frame(&v, &frame(dts));
        if i == cue_after {
            mux.push_cue(cue);
        }
        ts.extend(mux.take_output());
    }
    assert_eq!(ts.len() % 188, 0);

    let mut demux = TsDemux::new();
    let mut demuxer = Demuxer::new();
    let (mut units, mut events) = (Vec::new(), Vec::new());
    // Feed packet by packet, the way SRT delivers it.
    for pkt in ts.chunks(188) {
        demux.feed(pkt);
        demux.drain(&mut units);
    }
    demux.flush(&mut units);
    for u in units {
        demuxer.consume(u, &mut events);
    }
    let mut cues = Vec::new();
    let mut n = 0;
    for e in events {
        match e {
            DemuxEvent::Cue(c) => cues.push(c),
            DemuxEvent::VideoFrame(_) => n += 1,
            _ => {}
        }
    }
    (cues, n)
}

#[test]
fn a_cue_comes_back_at_its_media_time() {
    // Source clock starts at 10 s; the demuxer rebases to 0.
    let frames: Vec<i64> = (0..10).map(|n| 900_000 + n * 3000).collect();
    let kind = CueKind::Out { duration_us: Some(30_000_000) };
    let at_us = ticks_to_us(900_000 + 4500);
    let cue = Cue { at_us, section: build(kind, Some(0), 7, Command::TimeSignal).unwrap(), kind };
    let (cues, n) = round_trip(&frames, 3, &cue);
    assert_eq!(n, 10);
    assert_eq!(cues.len(), 1, "exactly one cue");
    assert_eq!(cues[0].kind, kind);
    assert_eq!(cues[0].at_us, ticks_to_us(4500));
    // The section was re-timed onto the mux clock, then parsed back.
    assert_eq!(caudal_scte35::parse(&cues[0].section).unwrap().pts_90k, Some(900_000 + 4500));
}

#[test]
fn a_cue_after_the_33_bit_wrap_lands_after_the_frames_before_it() {
    let frames: Vec<i64> = (-3..3).map(|n| WRAP + n * 3000).collect();
    let at_us = ticks_to_us(WRAP + 1500);
    let cue = Cue { at_us, section: build(CueKind::In, Some(0), 1, Command::SpliceInsert).unwrap(), kind: CueKind::In };
    // Pushed after the frame at WRAP (wire DTS 0): the cue's wire PTS is 1500.
    let (cues, n) = round_trip(&frames, 3, &cue);
    assert_eq!(n, 6);
    assert_eq!(cues.len(), 1);
    // Zero point is the first DTS, WRAP - 9000.
    assert_eq!(cues[0].at_us, ticks_to_us(9000 + 1500));
    assert_eq!(cues[0].kind, CueKind::In);
}

#[test]
fn streams_without_cues_carry_no_scte35_pid() {
    let v = video();
    let mut mux = TsMux::new();
    mux.set_tracks(std::slice::from_ref(&v));
    mux.push_frame(&v, &frame(0));
    let out = mux.take_output();
    for pkt in out.chunks(188) {
        let pid = u16::from_be_bytes([pkt[1], pkt[2]]) & 0x1FFF;
        assert_ne!(pid, 0x103);
    }
    // PMT (second packet) has no CUEI registration.
    assert!(!out[188..376].windows(4).any(|w| w == b"CUEI"));
}

/// ffprobe (FFmpeg's own mpegts demuxer) must see a `scte_35` data stream.
#[test]
fn ffprobe_sees_an_scte35_stream() {
    let have = std::process::Command::new("ffprobe").arg("-version").output().is_ok_and(|o| o.status.success());
    if !have {
        eprintln!("SKIP: ffprobe not installed");
        return;
    }
    let v = video();
    let mut mux = TsMux::new();
    mux.set_tracks(std::slice::from_ref(&v));
    let kind = CueKind::Out { duration_us: Some(30_000_000) };
    let cue = Cue { at_us: 100_000, section: build(kind, Some(0), 7, Command::TimeSignal).unwrap(), kind };
    let mut ts = Vec::new();
    for n in 0..30 {
        mux.push_frame(&v, &frame(n * 3000));
        if n == 2 {
            mux.push_cue(&cue);
        }
        ts.extend(mux.take_output());
    }
    let path = std::env::temp_dir().join(format!("caudal_scte35_{}.ts", std::process::id()));
    std::fs::write(&path, &ts).unwrap();
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_name,codec_type", "-of", "csv=p=0"])
        .arg(&path)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("scte_35,data"), "ffprobe streams: {text}");
}

#[test]
fn the_pmt_declares_the_cue_pid_with_cuei() {
    let v = video();
    let mut mux = TsMux::new();
    mux.set_tracks(std::slice::from_ref(&v));
    mux.push_frame(&v, &frame(0));
    let _ = mux.take_output();
    let cue = Cue { at_us: 0, section: build(CueKind::In, None, 1, Command::TimeSignal).unwrap(), kind: CueKind::In };
    mux.push_cue(&cue);
    let out = mux.take_output();
    let pmt = out.chunks(188).find(|p| u16::from_be_bytes([p[1], p[2]]) & 0x1FFF == 0x1000).expect("PMT re-sent");
    assert!(pmt.windows(4).any(|w| w == b"CUEI"), "registration descriptor");
    // es_info entry: stream_type 0x86, PID 0x103.
    assert!(pmt.windows(3).any(|w| w == [0x86, 0xE1, 0x03]), "0x86 on PID 0x103");
    // The section packet: PUSI, pointer_field 0, table_id 0xFC.
    let sec = out.chunks(188).find(|p| u16::from_be_bytes([p[1], p[2]]) & 0x1FFF == 0x103).expect("cue packet");
    assert_ne!(sec[1] & 0x40, 0);
    assert_eq!(&sec[4..6], &[0x00, 0xFC]);
    assert_eq!(&sec[5..5 + cue.section.len()], &cue.section[..]);
}
