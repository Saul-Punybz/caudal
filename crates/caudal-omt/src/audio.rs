//! OMT audio (planar 32-bit float, FPA1 decoded by
//! `open_media_transport::media::decode_audio`) into the [`crate::Feed`]'s
//! [`AudioFrame`].
//!
//! OMT allows up to 32 channels; the feed's AAC encoder (ffmpeg's `aac`)
//! takes at most 8 with its default layouts, so channels past
//! [`MAX_CHANNELS`] are left out (not mixed down). Silent channels arrive
//! from the decoder as zeros and stay in.

use bytes::Bytes;
use open_media_transport::media::AudioFrame as OmtAudio;

use crate::feed::{AudioFrame, SampleLayout};

/// Channels kept from an OMT audio frame; the rest are dropped.
pub const MAX_CHANNELS: usize = 8;

/// The first `min(channels, MAX_CHANNELS)` planes of `a` as little-endian
/// planar f32 with presentation time `pts` (in `a.sample_rate` ticks), or
/// `None` for a frame without samples, channels or a positive rate, or whose
/// sample buffer is shorter than its header says.
pub fn to_feed(a: &OmtAudio, pts: i64) -> Option<AudioFrame> {
    let rate = u32::try_from(a.sample_rate).ok().filter(|&r| r > 0)?;
    let n = a.samples_per_channel;
    let channels = a.channels.min(MAX_CHANNELS);
    if n == 0 || channels == 0 || a.samples.len() < channels * n {
        return None;
    }
    let mut data = Vec::with_capacity(channels * n * 4);
    for s in &a.samples[..channels * n] {
        data.extend_from_slice(&s.to_le_bytes());
    }
    Some(AudioFrame {
        sample_rate: rate,
        channels: channels as u8,
        layout: SampleLayout::Planar,
        pts,
        data: Bytes::from(data),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn omt(channels: usize, n: usize, rate: i32) -> OmtAudio {
        OmtAudio {
            timestamp: 0,
            sample_rate: rate,
            channels,
            samples_per_channel: n,
            samples: (0..channels * n).map(|i| i as f32).collect(),
            active_channels: u32::MAX,
            metadata: Vec::new(),
        }
    }

    #[test]
    fn planar_samples_become_le_bytes() {
        let f = to_feed(&omt(2, 3, 48_000), 42).unwrap();
        assert_eq!((f.sample_rate, f.channels, f.layout, f.pts), (48_000, 2, SampleLayout::Planar, 42));
        assert_eq!(f.samples(), Some(3));
        let back: Vec<f32> = f.data.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
        assert_eq!(back, [0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
    }

    #[test]
    fn channels_past_eight_are_dropped() {
        let f = to_feed(&omt(16, 4, 48_000), 0).unwrap();
        assert_eq!(f.channels, 8);
        assert_eq!(f.samples(), Some(4));
        // Planes 0..8 only: the last sample kept is channel 7's last.
        let last = f32::from_le_bytes(f.data[f.data.len() - 4..].try_into().unwrap());
        assert_eq!(last, 31.0);
    }

    #[test]
    fn malformed_frames_are_refused() {
        assert!(to_feed(&omt(2, 0, 48_000), 0).is_none());
        assert!(to_feed(&omt(0, 4, 48_000), 0).is_none());
        assert!(to_feed(&omt(2, 4, 0), 0).is_none());
        assert!(to_feed(&omt(2, 4, -1), 0).is_none());
        let mut short = omt(2, 4, 48_000);
        short.samples.truncate(7);
        assert!(to_feed(&short, 0).is_none());
    }
}
