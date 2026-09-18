//! The media model every protocol speaks: tracks and frames.
//!
//! Timestamps stay in each track's native timescale (90 kHz for most video,
//! the sample rate for audio). MistServer rounds everything to milliseconds,
//! which drifts on audio (an AAC frame at 48 kHz is 21.333 ms); keeping the
//! native clock lets muxers write exact timestamps.

use bytes::Bytes;

/// Index of a track inside one stream. Stable for the life of the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TrackId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
    Data,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
    Av1,
    Vp8,
    Vp9,
    Aac,
    Opus,
    Mp3,
    Ac3,
    Eac3,
    Flac,
    Pcm,
    WebVtt,
    Scte35,
    Json,
}

impl TrackKind {
    /// Lowercase name used in the HTTP API.
    pub fn as_str(self) -> &'static str {
        match self {
            TrackKind::Video => "video",
            TrackKind::Audio => "audio",
            TrackKind::Subtitle => "subtitle",
            TrackKind::Data => "data",
        }
    }
}

impl Codec {
    /// Lowercase name used in the HTTP API and logs.
    pub fn as_str(self) -> &'static str {
        use Codec::*;
        match self {
            H264 => "h264",
            H265 => "h265",
            Av1 => "av1",
            Vp8 => "vp8",
            Vp9 => "vp9",
            Aac => "aac",
            Opus => "opus",
            Mp3 => "mp3",
            Ac3 => "ac3",
            Eac3 => "eac3",
            Flac => "flac",
            Pcm => "pcm",
            WebVtt => "webvtt",
            Scte35 => "scte35",
            Json => "json",
        }
    }

    pub fn kind(self) -> TrackKind {
        use Codec::*;
        match self {
            H264 | H265 | Av1 | Vp8 | Vp9 => TrackKind::Video,
            Aac | Opus | Mp3 | Ac3 | Eac3 | Flac | Pcm => TrackKind::Audio,
            WebVtt => TrackKind::Subtitle,
            Scte35 | Json => TrackKind::Data,
        }
    }
}

/// What a consumer needs to know about a track before it reads frames.
#[derive(Debug, Clone, PartialEq)]
pub struct TrackInfo {
    pub id: TrackId,
    pub codec: Codec,
    /// Ticks per second of `Frame::dts` / `Frame::pts` for this track.
    pub timescale: u32,
    /// Codec configuration: avcC / hvcC / av1C / AudioSpecificConfig / OpusHead.
    pub init: Bytes,
    /// ISO 639-2 language code, if known.
    pub lang: Option<String>,
    pub video: Option<VideoParams>,
    pub audio: Option<AudioParams>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoParams {
    pub width: u32,
    pub height: u32,
    /// Frames per second, if the source declares it.
    pub fps: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioParams {
    pub sample_rate: u32,
    pub channels: u8,
}

impl TrackInfo {
    pub fn kind(&self) -> TrackKind {
        self.codec.kind()
    }

    /// Converts a timestamp on this track's clock to microseconds.
    pub fn to_micros(&self, ts: i64) -> i64 {
        (i128::from(ts) * 1_000_000 / i128::from(self.timescale.max(1))) as i64
    }
}

/// One access unit: a whole video frame or one audio frame.
///
/// `data` is reference-counted; fanning a frame out to a thousand viewers
/// copies a pointer, not the payload.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub track: TrackId,
    /// Decode timestamp, in the track's timescale.
    pub dts: i64,
    /// Presentation timestamp, in the track's timescale. Equal to `dts`
    /// unless the codec reorders frames (B-frames).
    pub pts: i64,
    /// True when a decoder can start here. Every audio frame is a keyframe.
    pub keyframe: bool,
    /// Video: length-prefixed NAL units (AVCC style), never Annex B.
    pub data: Bytes,
}

/// An SCTE-35 ad cue (a `splice_info_section`) placed on the stream's
/// media timeline.
///
/// `at_us` is on the same clock as frame timestamps converted with
/// [`TrackInfo::to_micros`], so outputs can place the cue next to the frame
/// it applies to. `section` is the whole section (table_id `0xFC` through
/// CRC_32), carried unchanged so downstream splicers see what the source
/// sent.
#[derive(Debug, Clone, PartialEq)]
pub struct Cue {
    pub at_us: i64,
    pub section: Bytes,
    pub kind: CueKind,
}

/// What a cue asks a downstream splicer to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CueKind {
    /// Leave the network feed (an ad break starts). `duration_us` is the
    /// planned break length, when the section declares one.
    Out { duration_us: Option<i64> },
    /// Return to the network feed (the break ends).
    In,
    /// Any other command (a cancel, a program boundary, a private command).
    Other,
}

impl CueKind {
    /// Lowercase name used in the HTTP API.
    pub fn as_str(self) -> &'static str {
        match self {
            CueKind::Out { .. } => "out",
            CueKind::In => "in",
            CueKind::Other => "other",
        }
    }
}

/// Stream names travel into URLs, file paths and log lines, so they are
/// restricted up front instead of escaped everywhere.
pub fn valid_stream_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'+'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_names() {
        for ok in ["live", "cam_1", "event-2026.main", "live+wildcard"] {
            assert!(valid_stream_name(ok), "{ok}");
        }
        for bad in ["", ".hidden", "../etc", "a/b", "a b", "ñ", &"x".repeat(129)] {
            assert!(!valid_stream_name(bad), "{bad}");
        }
    }

    #[test]
    fn micros_are_exact_on_audio_clock() {
        let t = TrackInfo {
            id: TrackId(0),
            codec: Codec::Aac,
            timescale: 48_000,
            init: Bytes::new(),
            lang: None,
            video: None,
            audio: Some(AudioParams { sample_rate: 48_000, channels: 2 }),
        };
        // 1024 samples per AAC frame; 375 frames is exactly 8 seconds.
        assert_eq!(t.to_micros(1024 * 375), 8_000_000);
    }
}
