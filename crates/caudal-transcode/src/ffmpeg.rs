//! The `Ffmpeg` engine: one ffmpeg process per source producing every
//! rendition (one decode, `split` + `scale`, N libx264 + N AAC encodes).
//!
//! Input: the source's frames on stdin, as MPEG-TS from
//! `caudal_ts::mux::TsMux` (or Matroska from [`crate::mkv`] when the audio is
//! Opus, which TsMux can't carry). Output: ONE MPEG-TS on stdout holding all
//! renditions, PIDs `0x100 + i` in output-stream order (`v0 a0 v1 a1 ..`).
//! The reader splits the packets by PID (PAT/PMT go to everyone) into one
//! `TsDemux` + `Demuxer` per rendition. That keeps a single process and a
//! single decode, with no named pipes to clean up.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use caudal_core::{Codec, Event, Frame, Registry, Subscriber, TrackInfo, TrackKind};
use caudal_ts::demux::{DemuxEvent, Demuxer};
use caudal_ts::mux::TsMux;
use caudal_ts::ts::{EsKind, TsDemux};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::mpsc;

use crate::mkv::MkvWriter;
use crate::out::{AUDIO_OUT, Clock, RenditionOut, VIDEO_OUT};
use crate::{Source, TranscodeConfig};

const MIN_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(10);
/// A session that ran this long resets the backoff.
const HEALTHY_RUN: Duration = Duration::from_secs(20);
/// How long ffmpeg gets to flush its last frames after the source ends.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(3);
/// Chunks of input queued for ffmpeg's stdin; when full the feeder drops
/// frames up to the next keyframe instead of stalling or growing.
const STDIN_QUEUE: usize = 256;
const START_PID: u16 = 0x100;
const PMT_PID: u16 = 0x1000;
const TS_PACKET: usize = 188;

/// ffmpeg's process group, killed whole when dropped.
struct ProcGuard {
    child: Child,
}

impl ProcGuard {
    fn kill_group(&mut self) {
        // `id()` is `None` once the child was reaped, so a recycled pid is
        // never signalled.
        if let Some(pid) = self.child.id() {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &format!("-{pid}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = self.child.start_kill();
    }

    /// Kills the group and reaps the child.
    async fn shutdown(mut self) {
        self.kill_group();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await;
    }
}

impl Drop for ProcGuard {
    fn drop(&mut self) {
        self.kill_group();
    }
}

enum SessionEnd {
    /// The source ended; the renditions end with it.
    Source,
    /// ffmpeg died (or could not start) while the source is live.
    Crashed,
    /// The source's codec configuration changed: start a fresh ffmpeg.
    Restart,
}

pub(crate) async fn run(registry: &Arc<Registry>, mut sub: Subscriber, mut src: Source, cfg: &TranscodeConfig) {
    let expect_audio = audio_input(&src).is_some();
    let outs: Vec<RenditionOut> = src
        .renditions
        .iter()
        .map(|r| RenditionOut::new(registry.clone(), format!("{}+{}", src.name, r.label), cfg.buffer, expect_audio))
        .collect();
    let outs = Arc::new(Mutex::new(outs));
    let mut clock = Clock::default();
    let mut fps = src.video.video.and_then(|v| v.fps).filter(|f| f.is_finite() && *f > 0.0);
    let mut backoff = MIN_BACKOFF;
    loop {
        let started = Instant::now();
        match session(&mut sub, &mut src, &mut clock, &mut fps, &outs, cfg).await {
            SessionEnd::Source => return,
            SessionEnd::Restart => {
                backoff = MIN_BACKOFF;
                continue;
            }
            SessionEnd::Crashed => {}
        }
        if started.elapsed() >= HEALTHY_RUN {
            backoff = MIN_BACKOFF;
        }
        tracing::warn!(stream = %src.name, retry_in = ?backoff, "ffmpeg exited; restarting");
        let sleep = tokio::time::sleep(backoff);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => break,
                ev = sub.recv() => if ev == Event::End { return },
            }
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
        sub.skip_to_live();
    }
}

/// The source audio track ffmpeg gets, if any: AAC over TS, Opus over MKV.
fn audio_input(src: &Source) -> Option<&TrackInfo> {
    src.audio.as_ref().filter(|a| matches!(a.codec, Codec::Aac | Codec::Opus))
}

/// Refreshes the source's video/audio track descriptions. Returns true if
/// the codec configuration changed.
fn refresh_tracks(src: &mut Source, tracks: &[TrackInfo]) -> bool {
    let video = tracks.iter().find(|t| matches!(t.codec, Codec::H264 | Codec::H265)).cloned();
    let audio = tracks.iter().find(|t| t.kind() == TrackKind::Audio).cloned();
    let changed = video.as_ref().is_some_and(|v| v.codec != src.video.codec || v.init != src.video.init)
        || audio.as_ref().map(|a| (a.codec, &a.init)) != src.audio.as_ref().map(|a| (a.codec, &a.init));
    if let Some(v) = video {
        src.video = v;
    }
    src.audio = audio;
    changed
}

enum Feeder {
    Ts(TsMux),
    Mkv(MkvWriter),
}

impl Feeder {
    fn new(src: &Source) -> (Self, Vec<u8>) {
        match audio_input(src) {
            Some(a) if a.codec == Codec::Opus => {
                let (w, header) = MkvWriter::new(&[&src.video, a]);
                (Feeder::Mkv(w), header)
            }
            audio => {
                let mut mux = TsMux::new();
                let tracks: Vec<TrackInfo> = std::iter::once(&src.video).chain(audio).cloned().collect();
                mux.set_tracks(&tracks);
                (Feeder::Ts(mux), Vec::new())
            }
        }
    }

    fn push(&mut self, info: &TrackInfo, frame: &Frame, out: &mut Vec<u8>) {
        match self {
            Feeder::Ts(mux) => {
                mux.push_frame(info, frame);
                out.extend_from_slice(&mux.take_output());
            }
            Feeder::Mkv(w) => w.write(info, frame, out),
        }
    }
}

fn args(src: &Source, fps: f64, mkv: bool) -> Vec<String> {
    let n = src.renditions.len();
    let gop = (fps * 2.0).round().max(1.0) as u32;
    let audio = audio_input(src).is_some();
    let mut a: Vec<String> = [
        "-hide_banner",
        "-nostdin",
        "-nostats",
        "-loglevel",
        "info",
        "-copyts",
        "-fflags",
        "+nobuffer",
        "-probesize",
        "500000",
        "-analyzeduration",
        "1000000",
        "-f",
        if mkv { "matroska" } else { "mpegts" },
        "-i",
        "pipe:0",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let mut filter = if n == 1 { "[0:v:0]".to_owned() } else { format!("[0:v:0]split={n}") };
    if n > 1 {
        for i in 0..n {
            filter.push_str(&format!("[s{i}]"));
        }
        filter.push(';');
    }
    for (i, r) in src.renditions.iter().enumerate() {
        if n > 1 {
            filter.push_str(&format!("[s{i}]"));
        }
        filter.push_str(&format!("scale=-2:{},format=yuv420p[v{i}]", r.height & !1));
        if i + 1 < n {
            filter.push(';');
        }
    }
    a.extend(["-filter_complex".into(), filter]);
    for i in 0..n {
        a.extend(["-map".into(), format!("[v{i}]")]);
        if audio {
            a.extend(["-map".into(), "0:a:0".into()]);
        }
    }
    for s in [
        "-fps_mode",
        "passthrough",
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-tune",
        "zerolatency",
        "-bf",
        "0",
        "-sc_threshold",
        "0",
        "-force_key_frames",
        "expr:if(isnan(prev_forced_t),1,gte(t-prev_forced_t,2))",
    ] {
        a.push(s.into());
    }
    a.extend(["-g".into(), gop.to_string(), "-keyint_min".into(), gop.to_string()]);
    for (i, r) in src.renditions.iter().enumerate() {
        let k = r.video_kbps.max(1);
        a.extend([format!("-b:v:{i}"), format!("{k}k")]);
        a.extend([format!("-maxrate:v:{i}"), format!("{k}k")]);
        a.extend([format!("-bufsize:v:{i}"), format!("{}k", k * 2)]);
    }
    if audio {
        a.extend(["-c:a".into(), "aac".into()]);
        for (i, r) in src.renditions.iter().enumerate() {
            a.extend([format!("-b:a:{i}"), format!("{}k", r.audio_kbps.max(8))]);
        }
    }
    for s in ["-f", "mpegts", "-mpegts_copyts", "1", "-muxdelay", "0", "-muxpreload", "0", "-flush_packets", "1"] {
        a.push(s.into());
    }
    a.extend(["-mpegts_start_pid".into(), format!("0x{START_PID:x}")]);
    // Names the process for operators (and tests): `pgrep -f service_name=<stream>`.
    a.extend(["-metadata".into(), format!("service_name={}", src.name)]);
    a.push("pipe:1".into());
    a
}

async fn session(
    sub: &mut Subscriber,
    src: &mut Source,
    clock: &mut Clock,
    fps: &mut Option<f64>,
    outs: &Arc<Mutex<Vec<RenditionOut>>>,
    cfg: &TranscodeConfig,
) -> SessionEnd {
    refresh_tracks(src, &sub.tracks());
    // Start on a video keyframe; measure the frame rate from the first two
    // video frames when the source doesn't declare one.
    let mut pre: Vec<Arc<Frame>> = Vec::new();
    loop {
        match sub.recv().await {
            Event::End => return SessionEnd::Source,
            Event::TracksChanged => {
                refresh_tracks(src, &sub.tracks());
                pre.clear();
            }
            Event::Lagged { .. } => pre.clear(),
            Event::Frame(f) => {
                let is_video = f.track == src.video.id;
                if src.track(f.track).is_none() || (pre.is_empty() && !(is_video && f.keyframe)) {
                    continue;
                }
                pre.push(f);
                if fps.is_some() {
                    break;
                }
                let v: Vec<i64> = pre.iter().filter(|f| f.track == src.video.id).map(|f| f.dts).collect();
                if v.len() >= 2 {
                    let d = (v[1] - v[0]) as f64;
                    let est = f64::from(src.video.timescale) / d;
                    *fps = Some(if d > 0.0 && (1.0..=120.0).contains(&est) { est } else { 30.0 });
                    break;
                }
                if pre.len() > 64 {
                    *fps = Some(30.0);
                    break;
                }
            }
        }
    }
    let fps_v = fps.unwrap_or(30.0);
    for o in outs.lock().expect("poisoned").iter_mut() {
        o.set_fps(fps_v);
    }

    let (mut feeder, mut first) = Feeder::new(src);
    for f in &pre {
        if let Some(info) = src.track(f.track) {
            let shifted = clock.onto_encoder(info, f);
            feeder.push(info, &shifted, &mut first);
        }
    }
    let mkv = matches!(feeder, Feeder::Mkv(_));
    let mut cmd = Command::new(&cfg.ffmpeg);
    cmd.args(args(src, fps_v, mkv))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(stream = %src.name, ffmpeg = %cfg.ffmpeg.display(), error = %e, "cannot start ffmpeg");
            return SessionEnd::Crashed;
        }
    };
    tracing::debug!(stream = %src.name, pid = ?child.id(), "ffmpeg started");
    let (Some(mut stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return SessionEnd::Crashed;
    };
    let guard = ProcGuard { child };

    let tail: Arc<Mutex<VecDeque<String>>> = Arc::default();
    let stderr_task = {
        let (tail, name) = (tail.clone(), src.name.clone());
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(stream = %name, "ffmpeg: {line}");
                let mut t = tail.lock().expect("poisoned");
                if t.len() == 8 {
                    t.pop_front();
                }
                t.push_back(line);
            }
        })
    };
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(STDIN_QUEUE);
    let writer = tokio::spawn(async move {
        while let Some(b) = rx.recv().await {
            if stdin.write_all(&b).await.is_err() {
                break;
            }
        }
        // Dropping stdin here is ffmpeg's end of input.
    });
    let per = if audio_input(src).is_some() { 2 } else { 1 };
    let mut reader = tokio::spawn(read_outputs(stdout, outs.clone(), *clock, per, src.renditions.len()));
    let _ = tx.try_send(first);

    let mut resync = false;
    let end = loop {
        tokio::select! {
            ev = sub.recv() => match ev {
                Event::Frame(f) => {
                    let Some(info) = src.track(f.track) else { continue };
                    if resync {
                        if !(f.track == src.video.id && f.keyframe) {
                            continue;
                        }
                        resync = false;
                    }
                    let shifted = clock.onto_encoder(info, &f);
                    let mut bytes = Vec::with_capacity(f.data.len() + 256);
                    feeder.push(info, &shifted, &mut bytes);
                    if bytes.is_empty() {
                        continue;
                    }
                    match tx.try_send(bytes) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            tracing::debug!(stream = %src.name, "ffmpeg is behind; skipping to the next keyframe");
                            resync = true;
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => break SessionEnd::Crashed,
                    }
                }
                Event::TracksChanged => {
                    if refresh_tracks(src, &sub.tracks()) {
                        tracing::info!(stream = %src.name, "source codec changed; restarting ffmpeg");
                        break SessionEnd::Restart;
                    }
                }
                Event::Lagged { .. } => resync = true,
                Event::End => {
                    // Let ffmpeg flush what it has, then stop it.
                    drop(tx);
                    let _ = tokio::time::timeout(DRAIN_TIMEOUT, &mut reader).await;
                    reader.abort();
                    guard.shutdown().await;
                    writer.abort();
                    stderr_task.abort();
                    return SessionEnd::Source;
                }
            },
            _ = &mut reader => break SessionEnd::Crashed,
        }
    };
    reader.abort();
    writer.abort();
    guard.shutdown().await;
    let _ = tokio::time::timeout(Duration::from_millis(500), stderr_task).await;
    if matches!(end, SessionEnd::Crashed) {
        let tail: Vec<String> = tail.lock().expect("poisoned").iter().cloned().collect();
        tracing::warn!(stream = %src.name, stderr = ?tail, "ffmpeg stopped while the source is live");
    }
    end
}

/// Per-rendition demux state.
struct OutDemux {
    ts: TsDemux,
    demux: Demuxer,
    /// The raw 90 kHz timestamp the `Demuxer` rebased to zero (its first
    /// unit's), needed to undo that rebase.
    zero: Option<i64>,
}

/// Reads ffmpeg's single MPEG-TS, splits it by PID and publishes each
/// rendition's frames on the source clock. Returns at end of output.
async fn read_outputs(
    mut stdout: ChildStdout,
    outs: Arc<Mutex<Vec<RenditionOut>>>,
    clock: Clock,
    per: usize,
    n: usize,
) {
    let mut demux: Vec<OutDemux> =
        (0..n).map(|_| OutDemux { ts: TsDemux::new(), demux: Demuxer::new(), zero: None }).collect();
    let mut buf = vec![0u8; 64 * 1024];
    let mut carry: Vec<u8> = Vec::new();
    let mut units = Vec::new();
    let mut events = Vec::new();
    let video_off = clock.back_offset(90_000);
    loop {
        let n_read = match stdout.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(k) => k,
        };
        carry.extend_from_slice(&buf[..n_read]);
        let mut pos = 0;
        while carry.len() - pos >= TS_PACKET {
            if carry[pos] != 0x47 {
                pos += 1; // resync on the sync byte
                continue;
            }
            let pkt = &carry[pos..pos + TS_PACKET];
            let pid = (u16::from(pkt[1] & 0x1F) << 8) | u16::from(pkt[2]);
            if pid == 0 || pid == PMT_PID {
                for d in demux.iter_mut() {
                    d.ts.feed(pkt);
                }
            } else if pid >= START_PID {
                let idx = usize::from(pid - START_PID) / per;
                if let Some(d) = demux.get_mut(idx) {
                    d.ts.feed(pkt);
                }
            }
            pos += TS_PACKET;
        }
        carry.drain(..pos);

        let mut outs = outs.lock().expect("poisoned");
        for (d, out) in demux.iter_mut().zip(outs.iter_mut()) {
            d.ts.drain(&mut units);
            for unit in units.drain(..) {
                if d.zero.is_none() {
                    let raw = match unit.kind {
                        EsKind::Aac => unit.pts.or(unit.dts),
                        _ => unit.dts.or(unit.pts),
                    };
                    d.zero = Some(raw.unwrap_or(0) as i64);
                }
                d.demux.consume(unit, &mut events);
            }
            let zero = d.zero.unwrap_or(0);
            for ev in events.drain(..) {
                match ev {
                    DemuxEvent::VideoInit(info) | DemuxEvent::AudioInit(info) => out.set_track(info),
                    DemuxEvent::VideoFrame(mut f) => {
                        f.track = VIDEO_OUT;
                        f.dts += zero + video_off;
                        f.pts += zero + video_off;
                        out.push(f);
                    }
                    DemuxEvent::AudioFrame(mut f) => {
                        let rate = out.audio_rate().unwrap_or(48_000);
                        let off =
                            (i128::from(zero) * i128::from(rate) / 90_000) as i64 + clock.back_offset(rate);
                        f.track = AUDIO_OUT;
                        f.dts += off;
                        f.pts += off;
                        out.push(f);
                    }
                }
            }
        }
    }
}
