//! WHIP ingest: depacketized WebRTC media -> `caudal_core::Frame`s.
//!
//! str0m hands us whole access units (H.264 as Annex B, Opus packets).
//! Video becomes AVCC with a keyframe flag, the avcC comes from the first
//! in-band SPS/PPS; audio becomes Opus frames with an OpusHead init.
//! Timestamps come from RTP (unwrapped), each track anchored at its first
//! packet's arrival relative to the session's first packet, so audio and
//! video start near 0 and stay on their media clocks.

use std::time::{Duration, Instant};

use bytes::Bytes;
use caudal_core::{AudioParams, Codec, Frame, Publisher, PushError, TrackId, TrackInfo, VideoParams};
use str0m::format::Codec as RtcCodec;
use str0m::media::MediaData;

use crate::codec::{self, RtpClock};

const VIDEO: TrackId = TrackId(0);
const AUDIO: TrackId = TrackId(1);
/// How long to wait for the second track before announcing only the first.
const TRACK_WAIT: Duration = Duration::from_secs(2);
const MAX_PENDING: usize = 512;

pub(crate) struct Ingest {
    publisher: Publisher,
    /// Which kinds the offer sends us (from `MediaAdded`).
    pub(crate) expect_video: bool,
    pub(crate) expect_audio: bool,
    t0: Option<Instant>,
    video_clock: Option<RtpClock>,
    audio_clock: Option<RtpClock>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    video_info: Option<TrackInfo>,
    audio_info: Option<TrackInfo>,
    announced: Option<Vec<TrackInfo>>,
    pending: Vec<Frame>,
    /// Video is dropped until the next keyframe (start, or after a gap).
    need_keyframe: bool,
}

impl Ingest {
    pub(crate) fn new(publisher: Publisher) -> Self {
        Self {
            publisher,
            expect_video: false,
            expect_audio: false,
            t0: None,
            video_clock: None,
            audio_clock: None,
            sps: None,
            pps: None,
            video_info: None,
            audio_info: None,
            announced: None,
            pending: Vec::new(),
            need_keyframe: true,
        }
    }

    pub(crate) fn name(&self) -> &str {
        self.publisher.stream().name()
    }

    /// True while a keyframe would help (start, after loss): the engine
    /// sends PLIs, rate-limited.
    pub(crate) fn wants_keyframe(&self) -> bool {
        self.expect_video && self.need_keyframe
    }

    pub(crate) fn on_media(&mut self, data: &MediaData, now: Instant) -> Result<(), PushError> {
        let t0 = *self.t0.get_or_insert(now);
        let since = now.saturating_duration_since(t0);
        let rtp = data.time.numer() as u32;
        match data.params.spec().codec {
            RtcCodec::H264 => {
                if !data.contiguous {
                    self.need_keyframe = true;
                }
                let clock =
                    self.video_clock.get_or_insert_with(|| RtpClock::new(rtp, (since.as_secs_f64() * 90_000.0) as i64));
                let dts = clock.next(rtp);
                if let Some(f) = self.video_frame(&data.data, dts) {
                    self.emit(f)?;
                }
            }
            RtcCodec::Opus => {
                let clock =
                    self.audio_clock.get_or_insert_with(|| RtpClock::new(rtp, (since.as_secs_f64() * 48_000.0) as i64));
                let dts = clock.next(rtp);
                if self.audio_info.is_none() {
                    let channels = data.params.spec().channels.unwrap_or(2).clamp(1, 2);
                    self.audio_info = Some(TrackInfo {
                        id: AUDIO,
                        codec: Codec::Opus,
                        timescale: 48_000,
                        init: Bytes::from(codec::opus_head(channels)),
                        lang: None,
                        video: None,
                        audio: Some(AudioParams { sample_rate: 48_000, channels }),
                    });
                }
                if !data.data.is_empty() {
                    let f =
                        Frame { track: AUDIO, dts, pts: dts, keyframe: true, data: Bytes::copy_from_slice(&data.data) };
                    self.emit(f)?;
                }
            }
            _ => {}
        }
        self.maybe_announce(now)
    }

    fn video_frame(&mut self, annexb: &[u8], dts: i64) -> Option<Frame> {
        let mut out = Vec::with_capacity(annexb.len() + 16);
        let mut idr = false;
        let mut params_changed = false;
        for nal in codec::split_annexb(annexb) {
            match codec::nal_type(nal) {
                codec::NAL_AUD => continue,
                codec::NAL_SPS => {
                    if self.sps.as_deref() != Some(nal) {
                        self.sps = Some(nal.to_vec());
                        params_changed = true;
                    }
                }
                codec::NAL_PPS => {
                    if self.pps.as_deref() != Some(nal) {
                        self.pps = Some(nal.to_vec());
                        params_changed = true;
                    }
                }
                codec::NAL_IDR => idr = true,
                _ => {}
            }
            codec::push_avcc_nal(&mut out, nal);
        }
        if params_changed {
            self.rebuild_video_info();
        }
        if out.is_empty() || self.video_info.is_none() {
            self.need_keyframe = true;
            return None;
        }
        if self.need_keyframe {
            if !idr {
                return None;
            }
            self.need_keyframe = false;
        }
        Some(Frame { track: VIDEO, dts, pts: dts, keyframe: idr, data: Bytes::from(out) })
    }

    fn rebuild_video_info(&mut self) {
        let (Some(sps), Some(pps)) = (&self.sps, &self.pps) else { return };
        let Some(avcc) = codec::build_avcc(sps, pps) else {
            tracing::warn!(stream = %self.name(), "whip: SPS/PPS do not make an avcC; waiting for new ones");
            return;
        };
        let video = codec::sps_params(sps).map(|(width, height, fps)| VideoParams { width, height, fps });
        self.video_info = Some(TrackInfo {
            id: VIDEO,
            codec: Codec::H264,
            timescale: 90_000,
            init: Bytes::from(avcc),
            lang: None,
            video,
            audio: None,
        });
    }

    fn emit(&mut self, f: Frame) -> Result<(), PushError> {
        match &self.announced {
            Some(tracks) if tracks.iter().any(|t| t.id == f.track) => self.publisher.push(f),
            _ => {
                if self.pending.len() >= MAX_PENDING {
                    self.pending.remove(0);
                }
                self.pending.push(f);
                Ok(())
            }
        }
    }

    /// Announces the track list once every expected track is known, or
    /// after [`TRACK_WAIT`] with what arrived; re-announces when it changes
    /// (a late track, new SPS/PPS).
    pub(crate) fn maybe_announce(&mut self, now: Instant) -> Result<(), PushError> {
        let tracks: Vec<TrackInfo> = [&self.video_info, &self.audio_info].into_iter().flatten().cloned().collect();
        if tracks.is_empty() || self.announced.as_ref() == Some(&tracks) {
            return Ok(());
        }
        let complete =
            (!self.expect_video || self.video_info.is_some()) && (!self.expect_audio || self.audio_info.is_some());
        let waited = self.t0.is_some_and(|t| now.saturating_duration_since(t) >= TRACK_WAIT);
        if self.announced.is_none() && !complete && !waited {
            return Ok(());
        }
        self.publisher.set_tracks(tracks.clone())?;
        tracing::info!(
            stream = %self.name(),
            tracks = ?tracks.iter().map(|t| t.codec.as_str()).collect::<Vec<_>>(),
            "whip: tracks announced"
        );
        self.announced = Some(tracks);
        for f in std::mem::take(&mut self.pending) {
            self.emit(f)?;
        }
        Ok(())
    }
}
