//! AAC / Opus → planar f32 for one output.
//!
//! AAC-LC decodes in process with `rusty_aac` (already Caudal's AAC
//! decoder for live captions), Opus with `opus-decoder`. Other AAC object
//! types (HE-AAC v1/v2: `rusty_aac` decodes only their core, band-limited)
//! go through ffmpeg: ADTS frames built from the AudioSpecificConfig on
//! stdin, `f32le` at the output rate on stdout.
//!
//! Timestamps count samples from an anchor: the first packet's pts, moved
//! whenever a packet's pts is more than [`RESYNC_US`] away from the count
//! (a gap, a lag skip, a source restart).

use std::fs::File;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use caudal_core::{Codec, Frame, TrackInfo};
use caudal_transcode::pipe::FfmpegProcess;
use open_media_transport::sender::Sender;

use super::{Ctx, OutputStats};
use crate::time::micros_to_omt;

/// A pts this far from the sample count re-anchors the count.
const RESYNC_US: i64 = 100_000;
/// Samples per channel per chunk read back from ffmpeg.
const FFMPEG_CHUNK: usize = 1024;
/// Most channels OMT carries.
const MAX_CHANNELS: usize = 32;

/// Sample-counting timestamps.
#[derive(Default)]
struct SampleClock {
    anchor_us: i64,
    rate: u32,
    samples: u64,
    started: bool,
}

impl SampleClock {
    /// The start time (µs) of `n` samples at `rate` whose packet says
    /// `pts_us`, advancing the count.
    fn stamp(&mut self, pts_us: i64, n: usize, rate: u32) -> i64 {
        let rate = rate.max(1);
        let at = |c: &Self| c.anchor_us + (i128::from(c.samples) * 1_000_000 / i128::from(c.rate.max(1))) as i64;
        if !self.started || rate != self.rate || (pts_us - at(self)).abs() > RESYNC_US {
            *self = SampleClock { anchor_us: pts_us, rate, samples: 0, started: true };
        }
        let t = at(self);
        self.samples += n as u64;
        t
    }
}

/// Interleaved → planar.
fn planar(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    out.clear();
    let n = interleaved.len() / channels.max(1);
    for ch in 0..channels {
        out.extend(interleaved.iter().skip(ch).step_by(channels).take(n));
    }
}

fn send(sender: &Sender, stats: &OutputStats, name: &str, planes: &[f32], channels: usize, rate: u32, ts_us: i64) {
    if planes.is_empty() {
        return;
    }
    match sender.send_audio(planes, channels, rate as i32, micros_to_omt(ts_us), &[]) {
        Ok(_) => OutputStats::add(&stats.audio_sent, 1),
        Err(e) => {
            tracing::debug!(output = %name, error = %e, "OMT sender refused audio");
            OutputStats::add(&stats.errors, 1);
        }
    }
}

/// What ffmpeg needs to frame raw AAC access units as ADTS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Adts {
    /// ADTS `profile` (core object type - 1).
    profile: u8,
    sf_index: u8,
    channel_config: u8,
    /// What ffmpeg outputs (after SBR/PS).
    out_rate: u32,
    out_channels: usize,
}

/// The parts of an AudioSpecificConfig (ISO 14496-3 §1.6.2.1) the output
/// needs, with both explicit SBR signallings. (`rusty_aac`'s own SBR parser
/// takes the hierarchical form's first rate for the extension rate and
/// skips `extensionSamplingFrequencyIndex`; the standard has it the other
/// way round.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Asc {
    /// Object type of the core (2 = AAC-LC), under any SBR/PS.
    pub core_object_type: u8,
    /// Rate the core is coded at.
    pub core_rate: u32,
    /// Rate after SBR (the core rate without it).
    pub out_rate: u32,
    /// `channelConfiguration` (0: a program config element, not handled).
    pub channel_config: u8,
    pub sbr: bool,
    pub ps: bool,
}

pub(crate) fn parse_asc(data: &[u8]) -> Option<Asc> {
    let mut r = rusty_aac::BitReader::new(data);
    let aot = |r: &mut rusty_aac::BitReader| -> Option<u8> {
        let ot = r.read_bits(5).ok()? as u8;
        if ot == 31 { Some(32 + r.read_bits(6).ok()? as u8) } else { Some(ot) }
    };
    let rate = |r: &mut rusty_aac::BitReader| -> Option<u32> {
        let idx = r.read_bits(4).ok()? as u8;
        let hz = if idx == 0x0F { r.read_bits(24).ok()? } else { rusty_aac::sample_rate_for_index(idx) };
        (hz > 0).then_some(hz)
    };
    let mut ot = aot(&mut r)?;
    let core_rate = rate(&mut r)?;
    let channel_config = r.read_bits(4).ok()? as u8;
    let (mut sbr, mut ps, mut ext_rate) = (false, false, None);
    if ot == 5 || ot == 29 {
        (sbr, ps) = (true, ot == 29);
        ext_rate = Some(rate(&mut r)?);
        ot = aot(&mut r)?;
    } else if ot == 2 && channel_config != 0 {
        // GASpecificConfig, then maybe the backward-compatible extension.
        let _frame_length = r.read_bool().ok()?;
        if r.read_bool().ok()? {
            r.read_bits(14).ok()?; // coreCoderDelay
        }
        let _extension = r.read_bool().ok()?;
        if r.bits_left() >= 16 && r.read_bits(11).ok()? == 0x2B7 && aot(&mut r)? == 5 && r.read_bool().ok()? {
            sbr = true;
            ext_rate = Some(rate(&mut r)?);
            if r.bits_left() >= 12 && r.read_bits(11).ok()? == 0x548 {
                ps = r.read_bool().ok()?;
            }
        }
    }
    let out_rate = match ext_rate {
        Some(e) if e != core_rate => e,
        _ if sbr => core_rate.checked_mul(2)?,
        _ => core_rate,
    };
    Some(Asc { core_object_type: ot, core_rate, out_rate, channel_config, sbr, ps })
}

/// ADTS framing for an AudioSpecificConfig, if ADTS can carry it.
fn adts_for(asc: &[u8]) -> Option<Adts> {
    let a = parse_asc(asc)?;
    if !(1..=4).contains(&a.core_object_type) || !(1..=7).contains(&a.channel_config) {
        return None;
    }
    let out_channels = match a.channel_config {
        1 if a.ps => 2,
        7 => 8,
        c => usize::from(c),
    };
    Some(Adts {
        profile: a.core_object_type - 1,
        sf_index: rusty_aac::sf_index_for_rate(a.core_rate)?,
        channel_config: a.channel_config,
        out_rate: a.out_rate,
        out_channels,
    })
}

impl Adts {
    /// The 7-byte header of a frame carrying `payload` bytes.
    fn header(&self, payload: usize) -> Option<[u8; 7]> {
        let len = payload + 7;
        if len > 0x1FFF {
            return None;
        }
        let (p, s, c) = (self.profile, self.sf_index, self.channel_config);
        Some([
            0xFF,
            0xF1, // MPEG-4, layer 0, no CRC
            (p << 6) | (s << 2) | (c >> 2),
            ((c & 3) << 6) | (len >> 11) as u8,
            (len >> 3) as u8,
            (((len & 7) as u8) << 5) | 0x1F, // buffer fullness 0x7FF (VBR)
            0xFC,
        ])
    }
}

/// ffmpeg decoding one run of AAC.
struct FfmpegAudio {
    stdin: Option<File>,
    proc: Option<FfmpegProcess>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl FfmpegAudio {
    fn start(ctx: &Ctx, adts: Adts, anchor_us: Arc<AtomicI64>) -> std::io::Result<Self> {
        let (rate, ch) = (adts.out_rate, adts.out_channels);
        let args: Vec<String> = [
            "-hide_banner",
            "-loglevel",
            "error",
            "-probesize",
            "32",
            "-analyzeduration",
            "0",
            "-f",
            "aac",
            "-i",
            "pipe:0",
            "-map",
            "0:a:0",
            "-ac",
            &ch.to_string(),
            "-ar",
            &rate.to_string(),
            "-f",
            "f32le",
            "pipe:1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let (proc, stdin, mut stdout) = ctx.spawn_ffmpeg(&args)?;
        let (sender, stats, name) = (ctx.sender.clone(), ctx.stats.clone(), ctx.name.clone());
        let reader = std::thread::Builder::new().name(format!("omt-out-aac:{}", ctx.name)).spawn(move || {
            let mut bytes = vec![0u8; FFMPEG_CHUNK * ch * 4];
            let (mut interleaved, mut planes) = (Vec::new(), Vec::new());
            let mut samples: u64 = 0;
            while stdout.read_exact(&mut bytes).is_ok() {
                interleaved.clear();
                interleaved.extend(bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
                planar(&interleaved, ch, &mut planes);
                let t = anchor_us.load(Ordering::Relaxed)
                    + (i128::from(samples) * 1_000_000 / i128::from(rate.max(1))) as i64;
                samples += FFMPEG_CHUNK as u64;
                send(&sender, &stats, &name, &planes, ch, rate, t);
            }
        })?;
        Ok(FfmpegAudio { stdin: Some(stdin), proc: Some(proc), reader: Some(reader) })
    }
}

impl Drop for FfmpegAudio {
    fn drop(&mut self) {
        drop(self.stdin.take());
        drop(self.proc.take());
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }
}

enum Engine {
    Aac(Box<rusty_aac::AacDecoder>),
    Opus { dec: Box<opus_decoder::OpusDecoder>, channels: usize, buf: Vec<f32> },
    Ffmpeg {
        adts: Adts,
        run: Option<FfmpegAudio>,
        anchor_us: Arc<AtomicI64>,
        /// Where the next packet should start if nothing was lost.
        next_in_us: Option<i64>,
        /// One access unit (1024 core samples), in µs.
        frame_us: i64,
    },
    Unsupported,
}

/// The audio half of one output.
pub(crate) struct AudioOut {
    track: TrackInfo,
    engine: Engine,
    clock: SampleClock,
    /// Decoding (the sender has connections).
    active: bool,
    interleaved: Vec<f32>,
    planes: Vec<f32>,
}

/// Channels of an Opus track with channel mapping family 0 (1 or 2), from
/// its OpusHead (or the track's parameters when there is none).
fn opus_channels(track: &TrackInfo) -> Option<usize> {
    let init = &track.init;
    if init.len() >= 19 && init.starts_with(b"OpusHead") {
        let (channels, family) = (usize::from(init[9]), init[18]);
        return (family == 0 && (1..=2).contains(&channels)).then_some(channels);
    }
    track.audio.map(|a| usize::from(a.channels)).filter(|c| (1..=2).contains(c))
}

fn make_engine(track: &TrackInfo, ctx: &Ctx) -> Engine {
    let name = &ctx.name;
    match track.codec {
        Codec::Aac => {
            let dec = rusty_aac::AacDecoder::with_config_bytes(&track.init);
            let lc_only = parse_asc(&track.init).is_some_and(|a| a.core_object_type == 2 && !a.sbr);
            let ffmpeg = ctx.options.ffmpeg.is_some();
            match (dec, lc_only) {
                (Ok(d), true) if !ctx.options.decode_with_ffmpeg || !ffmpeg => Engine::Aac(Box::new(d)),
                (dec, _) if ffmpeg => match adts_for(&track.init) {
                    Some(adts) => Engine::Ffmpeg {
                        adts,
                        run: None,
                        anchor_us: Arc::default(),
                        next_in_us: None,
                        frame_us: aac_frame_us(&track.init),
                    },
                    None => {
                        tracing::warn!(output = %name, "AAC configuration ADTS cannot carry; no audio");
                        match dec {
                            Ok(d) => Engine::Aac(Box::new(d)),
                            Err(_) => Engine::Unsupported,
                        }
                    }
                },
                (Ok(d), _) => {
                    tracing::warn!(output = %name,
                        "AAC other than AAC-LC decodes band-limited without ffmpeg ([omt] ffmpeg)");
                    Engine::Aac(Box::new(d))
                }
                (Err(e), _) => {
                    tracing::warn!(output = %name, error = %e, "unusable AAC configuration; no audio");
                    Engine::Unsupported
                }
            }
        }
        Codec::Opus => match opus_channels(track).map(|c| (c, opus_decoder::OpusDecoder::new(48_000, c))) {
            Some((channels, Ok(dec))) => {
                let n = dec.max_frame_size_per_channel() * channels;
                Engine::Opus { dec: Box::new(dec), channels, buf: vec![0.0; n] }
            }
            _ => {
                tracing::warn!(output = %name, "Opus with more than two channels is not supported; no audio");
                Engine::Unsupported
            }
        },
        c => {
            tracing::warn!(output = %name, codec = c.as_str(), "OMT output sends AAC or Opus audio only; no audio");
            Engine::Unsupported
        }
    }
}

impl AudioOut {
    pub fn new(track: TrackInfo, ctx: &Ctx) -> Self {
        AudioOut {
            engine: make_engine(&track, ctx),
            track,
            clock: SampleClock::default(),
            active: false,
            interleaved: Vec::new(),
            planes: Vec::new(),
        }
    }

    pub fn track(&self) -> &TrackInfo {
        &self.track
    }

    pub fn push(&mut self, f: &Frame, ctx: &Ctx) {
        if matches!(self.engine, Engine::Unsupported) {
            return;
        }
        if ctx.sender.connections() == 0 {
            if self.active {
                // Nobody listens: stop decoding, start clean next time.
                self.active = false;
                self.clock = SampleClock::default();
                self.engine = make_engine(&self.track, ctx);
            }
            return;
        }
        self.active = true;
        let pts_us = self.track.to_micros(f.pts);
        let stats = &ctx.stats;
        match &mut self.engine {
            Engine::Aac(dec) => match dec.decode(&f.data, None) {
                Ok(out) => {
                    let ch = usize::from(out.channels);
                    if !(1..=MAX_CHANNELS).contains(&ch) {
                        return;
                    }
                    planar(&out.samples, ch, &mut self.planes);
                    let n = out.samples.len() / ch;
                    let t = self.clock.stamp(pts_us, n, out.sample_rate);
                    send(&ctx.sender, stats, &ctx.name, &self.planes, ch, out.sample_rate, t);
                }
                Err(rusty_aac::Error::Again) => {}
                Err(e) => {
                    tracing::debug!(output = %ctx.name, error = %e, "AAC decode error");
                    OutputStats::add(&stats.errors, 1);
                }
            },
            Engine::Opus { dec, channels, buf } => match dec.decode_float(&f.data, buf, false) {
                Ok(n) => {
                    self.interleaved.clear();
                    self.interleaved.extend_from_slice(&buf[..n * *channels]);
                    planar(&self.interleaved, *channels, &mut self.planes);
                    let t = self.clock.stamp(pts_us, n, 48_000);
                    send(&ctx.sender, stats, &ctx.name, &self.planes, *channels, 48_000, t);
                }
                Err(e) => {
                    tracing::debug!(output = %ctx.name, error = ?e, "Opus decode error");
                    OutputStats::add(&stats.errors, 1);
                }
            },
            Engine::Ffmpeg { adts, run, anchor_us, next_in_us, frame_us } => {
                // A jump in the input restarts ffmpeg on a new anchor: its
                // output is a gap-free sample stream.
                let jumped = next_in_us.is_none_or(|n| (pts_us - n).abs() > RESYNC_US);
                if jumped || run.is_none() {
                    *run = None;
                    anchor_us.store(pts_us, Ordering::Relaxed);
                    match FfmpegAudio::start(ctx, *adts, anchor_us.clone()) {
                        Ok(r) => *run = Some(r),
                        Err(e) => {
                            tracing::warn!(output = %ctx.name, error = %e, "cannot start ffmpeg for AAC");
                            OutputStats::add(&stats.errors, 1);
                            *next_in_us = None;
                            return;
                        }
                    }
                }
                *next_in_us = Some(pts_us + *frame_us);
                let Some(header) = adts.header(f.data.len()) else { return };
                let Some(r) = run.as_mut() else { return };
                let ok = r.stdin.as_mut().is_some_and(|s| s.write_all(&header).and_then(|()| s.write_all(&f.data)).is_ok());
                if !ok {
                    tracing::warn!(output = %ctx.name, "ffmpeg (AAC) stopped; restarting");
                    OutputStats::add(&stats.errors, 1);
                    *run = None;
                    *next_in_us = None;
                }
            }
            Engine::Unsupported => {}
        }
    }
}

/// One AAC access unit (1024 samples at the core rate), in µs.
fn aac_frame_us(asc: &[u8]) -> i64 {
    let rate = parse_asc(asc).map_or(48_000, |a| a.core_rate);
    1024 * 1_000_000 / i64::from(rate.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_counts_samples_and_resyncs_on_jumps() {
        let mut c = SampleClock::default();
        assert_eq!(c.stamp(1_000_000, 1024, 48_000), 1_000_000);
        // Jittered pts: still the count.
        assert_eq!(c.stamp(1_021_000, 1024, 48_000), 1_021_333);
        assert_eq!(c.stamp(1_043_000, 1024, 48_000), 1_042_666);
        // A 1 s gap re-anchors.
        assert_eq!(c.stamp(2_100_000, 1024, 48_000), 2_100_000);
        // So does a rate change.
        assert_eq!(c.stamp(2_121_333, 1024, 44_100), 2_121_333);
    }

    #[test]
    fn planar_splits_channels() {
        let mut out = Vec::new();
        planar(&[1.0, 10.0, 2.0, 20.0, 3.0, 30.0], 2, &mut out);
        assert_eq!(out, [1.0, 2.0, 3.0, 10.0, 20.0, 30.0]);
    }

    #[test]
    fn adts_header_matches_rusty_aac() {
        for (rate, ch) in [(48_000, 2u16), (44_100, 1), (22_050, 6)] {
            let asc = rusty_aac::audio_specific_config_bytes(rate, ch);
            let a = adts_for(&asc).unwrap();
            assert_eq!((a.out_rate, a.out_channels), (rate, usize::from(ch)));
            let ours = a.header(300).unwrap();
            let theirs = rusty_aac::write_adts_header(&rusty_aac::AdtsHeader {
                object_type: 2,
                sample_rate: rate,
                channels: ch,
                frame_length: 307,
                header_len: 7,
            });
            assert_eq!(ours.as_slice(), theirs.as_slice());
            let parsed = rusty_aac::parse_adts(&ours).unwrap();
            assert_eq!((parsed.sample_rate, parsed.channels, parsed.frame_length), (rate, ch, 307));
        }
    }

    #[test]
    fn he_aac_frames_as_its_core() {
        // Explicit hierarchical HE-AAC: AOT 5, 24 kHz core, stereo, 48 kHz
        // extension, core AOT 2: 00101 0110 0010 0011 00010 + padding.
        let asc = [0x2B, 0x11, 0x88, 0];
        let parsed = parse_asc(&asc).unwrap();
        assert_eq!((parsed.core_rate, parsed.out_rate, parsed.sbr, parsed.ps), (24_000, 48_000, true, false));
        let a = adts_for(&asc).unwrap();
        assert_eq!((a.profile, a.sf_index, a.channel_config), (1, 6, 2));
        assert_eq!((a.out_rate, a.out_channels), (48_000, 2));
        assert!(a.header(0x2000).is_none(), "longer than ADTS can frame");
        assert_eq!(aac_frame_us(&asc), 1024 * 1_000_000 / 24_000);
    }

    #[test]
    fn backward_compatible_sbr_and_ps_are_seen() {
        // AAC-LC 22.05 kHz mono, GASpecificConfig 000, then sync 0x2B7,
        // AOT 5, sbr 1, 44.1 kHz, then sync 0x548, ps 1.
        let parts = ["00010", "0111", "0001", "000", "01010110111", "00101", "1", "0100", "10101001000", "1"];
        let bits: String = parts.concat();
        let mut asc = vec![0u8; bits.len().div_ceil(8)];
        for (i, b) in bits.bytes().enumerate() {
            if b == b'1' {
                asc[i / 8] |= 0x80 >> (i % 8);
            }
        }
        let a = parse_asc(&asc).unwrap();
        assert_eq!((a.core_object_type, a.core_rate, a.out_rate), (2, 22_050, 44_100));
        assert!(a.sbr && a.ps);
        let adts = adts_for(&asc).unwrap();
        assert_eq!((adts.out_rate, adts.out_channels), (44_100, 2));
        // Plain LC: nothing more.
        let lc = parse_asc(&rusty_aac::audio_specific_config_bytes(48_000, 2)).unwrap();
        assert!(!lc.sbr && lc.out_rate == 48_000 && lc.core_object_type == 2);
        // Truncated or garbage configurations are refused, never a panic.
        for n in 0..asc.len() {
            let _ = parse_asc(&asc[..n]);
        }
        assert!(parse_asc(&[]).is_none());
        assert!(parse_asc(&[0xFF, 0xFF, 0xFF]).is_none() || adts_for(&[0xFF, 0xFF, 0xFF]).is_none());
    }

    #[test]
    fn opus_head_channels() {
        let mut head = b"OpusHead".to_vec();
        head.extend_from_slice(&[1, 2, 0x38, 1, 0x80, 0xBB, 0, 0, 0, 0, 0]);
        let t = |init: Vec<u8>| TrackInfo {
            id: caudal_core::TrackId(1),
            codec: Codec::Opus,
            timescale: 48_000,
            init: init.into(),
            lang: None,
            video: None,
            audio: None,
        };
        assert_eq!(opus_channels(&t(head.clone())), Some(2));
        let mut surround = head.clone();
        surround[9] = 6;
        surround[18] = 1;
        assert_eq!(opus_channels(&t(surround)), None);
        assert_eq!(opus_channels(&t(Vec::new())), None);
    }
}
