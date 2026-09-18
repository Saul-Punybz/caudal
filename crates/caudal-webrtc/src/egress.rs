//! WHEP playback: stream frames -> str0m writers.
//!
//! Each WHEP session has a forwarder task that reads its `Subscriber` and
//! hands frames to the engine (which owns the `Rtc`). The forwarder never
//! blocks the engine: if the engine's queue is full it drops frames and
//! resumes at the next keyframe, as it does when the ring reports `Lagged`.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use caudal_core::{Codec, Event, Frame, StartAt, Subscriber, TrackId, TrackInfo, TrackKind};
use str0m::Rtc;
use str0m::format::Codec as RtcCodec;
use str0m::media::{Frequency, MediaKind, MediaTime, Mid, Pt};
use tokio::sync::mpsc;

use crate::codec;

/// From a forwarder to the engine.
pub(crate) enum Out {
    Tracks(Vec<TrackInfo>),
    Frame(Arc<Frame>),
    End,
}

/// From the engine to a forwarder.
pub(crate) enum Ctl {
    /// The viewer lost the picture (PLI/FIR): jump to the newest keyframe.
    SkipToLive,
    /// Nothing was sent yet (ICE/DTLS just came up): start over at the
    /// newest keyframe, even if it is behind the current read position.
    Restart,
}

pub(crate) async fn forward(
    id: u64,
    mut sub: Subscriber,
    tx: mpsc::Sender<(u64, Out)>,
    mut ctl: mpsc::UnboundedReceiver<Ctl>,
) {
    let mut video: Option<TrackId> = None;
    let mut need_key = false;
    loop {
        tokio::select! {
            biased;
            c = ctl.recv() => match c {
                None => return,
                Some(Ctl::SkipToLive) => { sub.skip_to_live(); }
                Some(Ctl::Restart) => {
                    sub = sub.stream().subscribe(StartAt::LiveEdge);
                    need_key = false;
                }
            },
            ev = sub.recv() => match ev {
                Event::TracksChanged => {
                    let tracks = sub.tracks();
                    video = tracks.iter().find(|t| t.kind() == TrackKind::Video).map(|t| t.id);
                    if tx.send((id, Out::Tracks(tracks))).await.is_err() {
                        return;
                    }
                }
                Event::Frame(f) => {
                    let is_video = Some(f.track) == video;
                    if need_key && is_video {
                        if !f.keyframe {
                            continue;
                        }
                        need_key = false;
                    }
                    match tx.try_send((id, Out::Frame(f))) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            need_key = true;
                            sub.skip_to_live();
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => return,
                    }
                }
                // The ring already moved us to a keyframe.
                Event::Lagged { .. } => {}
                Event::End => {
                    let _ = tx.send((id, Out::End)).await;
                    return;
                }
            },
        }
    }
}

struct VideoOut {
    track: TrackId,
    timescale: u32,
    /// SPS + PPS NAL units from the avcC, sent before every IDR.
    param_sets: Vec<Vec<u8>>,
}

pub(crate) struct Egress {
    pub(crate) name: Arc<str>,
    pub(crate) sub: Option<Subscriber>,
    pub(crate) ctl: Option<mpsc::UnboundedSender<Ctl>>,
    pub(crate) task: Option<tokio::task::AbortHandle>,
    video_mid: Option<Mid>,
    audio_mid: Option<Mid>,
    video: Option<VideoOut>,
    audio: Option<(TrackId, u32)>,
    connected: bool,
    started: bool,
    last_ctl: Option<Instant>,
    warned_aac: bool,
}

impl Drop for Egress {
    fn drop(&mut self) {
        if let Some(t) = &self.task {
            t.abort();
        }
    }
}

impl Egress {
    pub(crate) fn new(name: Arc<str>, sub: Subscriber) -> Self {
        Self {
            name,
            sub: Some(sub),
            ctl: None,
            task: None,
            video_mid: None,
            audio_mid: None,
            video: None,
            audio: None,
            connected: false,
            started: false,
            last_ctl: None,
            warned_aac: false,
        }
    }

    pub(crate) fn media_added(&mut self, mid: Mid, kind: MediaKind) {
        match kind {
            MediaKind::Video if self.video_mid.is_none() => self.video_mid = Some(mid),
            MediaKind::Audio if self.audio_mid.is_none() => self.audio_mid = Some(mid),
            _ => {}
        }
    }

    pub(crate) fn connected(&mut self) {
        self.connected = true;
        self.started = false;
        self.send_ctl(Ctl::Restart, true);
    }

    pub(crate) fn keyframe_requested(&mut self) {
        self.send_ctl(Ctl::SkipToLive, false);
    }

    fn send_ctl(&mut self, c: Ctl, force: bool) {
        let now = Instant::now();
        if !force && self.last_ctl.is_some_and(|t| now.duration_since(t) < Duration::from_millis(300)) {
            return;
        }
        self.last_ctl = Some(now);
        if let Some(ctl) = &self.ctl {
            let _ = ctl.send(c);
        }
    }

    pub(crate) fn set_tracks(&mut self, tracks: &[TrackInfo]) {
        self.video = None;
        self.audio = None;
        for t in tracks {
            match t.codec {
                Codec::H264 if self.video.is_none() => {
                    self.video = Some(VideoOut {
                        track: t.id,
                        timescale: t.timescale.max(1),
                        param_sets: codec::avcc_parameter_sets(&t.init),
                    });
                }
                Codec::Opus if self.audio.is_none() => self.audio = Some((t.id, t.timescale.max(1))),
                Codec::Aac | Codec::Mp3 | Codec::Ac3 | Codec::Eac3 if !self.warned_aac => {
                    self.warned_aac = true;
                    tracing::info!(
                        stream = %self.name,
                        codec = t.codec.as_str(),
                        "whep: audio codec not playable over WebRTC without transcoding; sending video only"
                    );
                }
                _ => {}
            }
        }
    }

    /// Writes one frame. `bframes_warned` is the engine's per-stream memory
    /// of the B-frame warning.
    pub(crate) fn write(&mut self, rtc: &mut Rtc, f: &Frame, bframes_warned: &mut HashSet<Arc<str>>) {
        if let Some(v) = self.video.as_ref().filter(|v| v.track == f.track) {
            if f.pts != f.dts && !bframes_warned.contains(&self.name) {
                bframes_warned.insert(self.name.clone());
                tracing::warn!(
                    stream = %self.name,
                    "whep: source has B-frames (pts != dts); browsers may stutter or fail to decode it"
                );
            }
            let Some(mid) = self.video_mid else { return };
            if !self.connected {
                return;
            }
            if !self.started {
                if !f.keyframe {
                    self.send_ctl(Ctl::SkipToLive, false);
                    return;
                }
                self.started = true;
            }
            let nals = codec::avcc_nals(&f.data);
            let mut annexb = Vec::with_capacity(f.data.len() + 64);
            if f.keyframe && !nals.iter().any(|n| codec::nal_type(n) == codec::NAL_SPS) {
                for ps in &v.param_sets {
                    codec::push_annexb_nal(&mut annexb, ps);
                }
            }
            for nal in nals {
                if codec::nal_type(nal) != codec::NAL_AUD {
                    codec::push_annexb_nal(&mut annexb, nal);
                }
            }
            let ts = rescale(f.pts, v.timescale, 90_000);
            write_media(rtc, mid, RtcCodec::H264, MediaTime::new(ts, Frequency::NINETY_KHZ), annexb);
        } else if let Some((track, timescale)) = self.audio {
            if track != f.track || !self.connected {
                return;
            }
            let Some(mid) = self.audio_mid else { return };
            let ts = rescale(f.pts, timescale, 48_000);
            write_media(rtc, mid, RtcCodec::Opus, MediaTime::new(ts, Frequency::FORTY_EIGHT_KHZ), f.data.to_vec());
        }
    }
}

/// `ts` from `from` Hz to `to` Hz, wrapped to u64 (RTP time wraps anyway).
fn rescale(ts: i64, from: u32, to: u32) -> u64 {
    (i128::from(ts) * i128::from(to) / i128::from(from.max(1))) as u64
}

fn write_media(rtc: &mut Rtc, mid: Mid, codec: RtcCodec, time: MediaTime, data: Vec<u8>) {
    let Some(writer) = rtc.writer(mid) else { return };
    let pt: Option<Pt> = writer
        .payload_params()
        .find(|p| {
            let s = p.spec();
            s.codec == codec && (codec != RtcCodec::H264 || s.format.packetization_mode == Some(1))
        })
        .map(|p| p.pt());
    let Some(pt) = pt else { return };
    if let Err(e) = writer.write(pt, Instant::now(), time, data) {
        tracing::debug!(error = %e, "whep: write failed");
    }
}
