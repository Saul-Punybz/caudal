//! MPEG-2 video + MPEG audio through Caudal's TS path, checked by ffmpeg:
//! ffmpeg writes the kind of TS an ATSC chain carries (VLC or a hardware
//! encoder: MPEG-2 720p59.94 + MP2), Caudal demuxes it into tracks and
//! frames, muxes it back out, and ffprobe/ffmpeg must read the result as
//! the same two streams and decode it without an error.

use std::process::Command;

use caudal_core::{Codec, Frame, TrackInfo};
use caudal_ts::demux::{DemuxEvent, Demuxer};
use caudal_ts::mux::TsMux;
use caudal_ts::ts::TsDemux;

fn have(bin: &str) -> bool {
    Command::new("which").arg(bin).output().is_ok_and(|o| o.status.success())
}

/// A few seconds of MPEG-2 video + audio as MPEG-TS, from ffmpeg.
fn source(audio_codec: &str, sample_rate: &str) -> Vec<u8> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=60000/1001:duration=3"])
        .args(["-f", "lavfi", "-i", &format!("sine=frequency=440:sample_rate={sample_rate}:duration=3")])
        .args(["-c:v", "mpeg2video", "-b:v", "8M", "-g", "15", "-bf", "2", "-c:a", audio_codec, "-b:a", "128k"])
        .args(["-f", "mpegts", "pipe:1"])
        .output()
        .expect("run ffmpeg");
    assert!(out.status.success(), "ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

struct Demuxed {
    video: Option<TrackInfo>,
    audio: Option<TrackInfo>,
    frames: Vec<(bool, Frame)>, // (is_video, frame)
}

fn demux(ts: &[u8]) -> Demuxed {
    let mut ts_demux = TsDemux::new();
    let mut demuxer = Demuxer::new();
    let (mut units, mut events) = (Vec::new(), Vec::new());
    // Feed in SRT-sized chunks (7 TS packets), like a live connection.
    for chunk in ts.chunks(1316) {
        ts_demux.feed(chunk);
        ts_demux.drain(&mut units);
    }
    ts_demux.flush(&mut units);
    for unit in units {
        demuxer.consume(unit, &mut events);
    }
    let mut d = Demuxed { video: None, audio: None, frames: Vec::new() };
    for ev in events {
        match ev {
            DemuxEvent::VideoInit(t) => d.video = Some(t),
            DemuxEvent::AudioInit(t) => d.audio = Some(t),
            DemuxEvent::VideoFrame(f) => d.frames.push((true, f)),
            DemuxEvent::AudioFrame(f) => d.frames.push((false, f)),
            DemuxEvent::Cue(_) => {}
        }
    }
    d
}

fn ffprobe_streams(path: &std::path::Path) -> String {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_name,width,height,r_frame_rate,sample_rate,channels"])
        .args(["-of", "compact=p=0"])
        .arg(path)
        .output()
        .expect("run ffprobe");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn mpeg2_video_and_mp2_pass_through_demux_and_mux() {
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("SKIP: ffmpeg/ffprobe not installed");
        return;
    }
    let d = demux(&source("mp2", "48000"));

    let video = d.video.clone().expect("an MPEG-2 video track");
    assert_eq!(video.codec, Codec::Mpeg2Video);
    let vp = video.video.unwrap();
    assert_eq!((vp.width, vp.height), (1280, 720));
    assert!((vp.fps.unwrap() - 59.94).abs() < 0.01, "{:?}", vp.fps);

    let audio = d.audio.clone().expect("an MP2 audio track");
    assert_eq!(audio.codec, Codec::Mp2);
    assert_eq!(audio.timescale, 48_000);
    assert_eq!(audio.audio.map(|a| (a.sample_rate, a.channels)), Some((48_000, 1)));

    let video_frames: Vec<&Frame> = d.frames.iter().filter(|(v, _)| *v).map(|(_, f)| f).collect();
    let audio_frames: Vec<&Frame> = d.frames.iter().filter(|(v, _)| !*v).map(|(_, f)| f).collect();
    // 3 s at 59.94 fps; ffmpeg may drop or hold back a frame at the end.
    assert!((170..=181).contains(&video_frames.len()), "video frames: {}", video_frames.len());
    // One I picture every 15 frames (-g 15), the first frame among them.
    assert!(video_frames[0].keyframe, "starts on an I picture");
    let keys = video_frames.iter().filter(|f| f.keyframe).count();
    assert!((11..=13).contains(&keys), "keyframes: {keys}");
    // B-frames: some pictures are presented before they are decoded.
    assert!(video_frames.iter().any(|f| f.pts != f.dts), "B-frames keep their own PTS");
    // 1152 samples per Layer II frame, back to back.
    assert!(audio_frames.len() > 100, "audio frames: {}", audio_frames.len());
    for pair in audio_frames.windows(2) {
        assert_eq!(pair[1].pts - pair[0].pts, 1152, "audio frames are contiguous");
    }

    // Back out through the muxer, and ffmpeg is the judge.
    let mut mux = TsMux::new();
    mux.set_tracks(&[video.clone(), audio.clone()]);
    let mut out = Vec::new();
    for (is_video, f) in &d.frames {
        mux.push_frame(if *is_video { &video } else { &audio }, f);
        out.extend_from_slice(&mux.take_output());
    }
    let path = std::env::temp_dir().join(format!("caudal_ts_mpeg2_{}.ts", std::process::id()));
    std::fs::write(&path, &out).unwrap();
    let streams = ffprobe_streams(&path);
    let decode = Command::new("ffmpeg")
        .args(["-v", "error", "-xerror", "-i"])
        .arg(&path)
        .args(["-f", "null", "-"])
        .output()
        .expect("run ffmpeg decode");
    let frames_back = Command::new("ffprobe")
        .args(["-v", "error", "-count_frames", "-select_streams", "v:0", "-show_entries", "stream=nb_read_frames"])
        .args(["-of", "csv=p=0"])
        .arg(&path)
        .output()
        .expect("run ffprobe count");
    let _ = std::fs::remove_file(&path);

    assert!(streams.contains("codec_name=mpeg2video|width=1280|height=720"), "{streams}");
    assert!(streams.contains("r_frame_rate=60000/1001"), "{streams}");
    assert!(streams.contains("codec_name=mp2|sample_rate=48000|channels=1"), "{streams}");
    assert!(decode.status.success(), "decode errors: {}", String::from_utf8_lossy(&decode.stderr));
    // ffprobe prints "180," (csv with a trailing separator).
    let n: usize = String::from_utf8_lossy(&frames_back.stdout)
        .split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    assert_eq!(n, video_frames.len(), "every picture comes back out");
}

#[test]
fn mp3_at_a_low_sample_rate_is_mpeg2_audio() {
    if !have("ffmpeg") {
        eprintln!("SKIP: ffmpeg not installed");
        return;
    }
    let d = demux(&source("libmp3lame", "24000"));
    let audio = d.audio.expect("an MP3 audio track");
    assert_eq!(audio.codec, Codec::Mp3);
    assert_eq!(audio.timescale, 24_000);
    let audio_frames: Vec<&Frame> = d.frames.iter().filter(|(v, _)| !*v).map(|(_, f)| f).collect();
    assert!(audio_frames.len() > 50, "audio frames: {}", audio_frames.len());
    // MPEG-2 Layer III: 576 samples per frame.
    let steps: Vec<i64> = audio_frames.windows(2).map(|p| p[1].pts - p[0].pts).collect();
    assert!(steps.iter().all(|&d| d == 576), "steps: {steps:?}");
}
