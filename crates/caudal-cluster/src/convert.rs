//! A `hang` catalog (what `caudal-moq` publishes) → the local track list.
//! Only the codecs `caudal-moq` carries (H.264, H.265, AAC, Opus) come
//! back; the avcC/hvcC/AudioSpecificConfig/OpusHead travel as
//! `description` and become the track's `init` unchanged.

use bytes::Bytes;
use caudal_core::{AudioParams, Codec, TrackId, TrackInfo, VideoParams};
use hang::catalog::{AudioCodec, VideoCodec};

/// Video tracks run on the usual 90 kHz clock.
pub(crate) const VIDEO_TIMESCALE: u32 = 90_000;

/// One track to pull: its MoQ track name and what the registry learns.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Pulled {
    pub moq_name: String,
    pub info: TrackInfo,
}

pub(crate) fn tracks(catalog: &hang::Catalog) -> Vec<Pulled> {
    let mut out = Vec::new();
    // Video first, then audio; ids in that order so a republished track
    // list with the same codecs compares equal.
    for (name, v) in &catalog.video.renditions {
        let codec = match v.codec {
            VideoCodec::H264(_) => Codec::H264,
            VideoCodec::H265(_) => Codec::H265,
            _ => {
                tracing::info!(track = %name, codec = %v.codec, "cluster: video codec not pulled");
                continue;
            }
        };
        let Some(init) = v.description.clone().filter(|d| !d.is_empty()) else {
            tracing::warn!(track = %name, "cluster: video track without avcC/hvcC; skipped");
            continue;
        };
        let video = match (v.coded_width, v.coded_height) {
            (Some(width), Some(height)) => Some(VideoParams { width, height, fps: v.framerate }),
            _ => None,
        };
        out.push(Pulled {
            moq_name: name.clone(),
            info: TrackInfo {
                id: TrackId(out.len() as u32),
                codec,
                timescale: VIDEO_TIMESCALE,
                init,
                lang: None,
                video,
                audio: None,
            },
        });
    }
    for (name, a) in &catalog.audio.renditions {
        let codec = match a.codec {
            AudioCodec::AAC(_) => Codec::Aac,
            AudioCodec::Opus => Codec::Opus,
            _ => {
                tracing::info!(track = %name, codec = %a.codec, "cluster: audio codec not pulled");
                continue;
            }
        };
        let init = a.description.clone().unwrap_or_else(Bytes::new);
        if codec == Codec::Aac && init.is_empty() {
            tracing::warn!(track = %name, "cluster: AAC track without AudioSpecificConfig; skipped");
            continue;
        }
        out.push(Pulled {
            moq_name: name.clone(),
            info: TrackInfo {
                id: TrackId(out.len() as u32),
                codec,
                timescale: a.sample_rate.max(1),
                init,
                lang: None,
                video: None,
                audio: Some(AudioParams { sample_rate: a.sample_rate, channels: a.channel_count.min(255) as u8 }),
            },
        });
    }
    out
}
