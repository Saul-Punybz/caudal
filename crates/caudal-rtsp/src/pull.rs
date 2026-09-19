//! Pulls one camera over RTSP/TCP-interleaved with `retina` and republishes
//! it into the registry, reconnecting forever.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use caudal_core::{
    AudioParams, BufferConfig, Codec, Frame, PublishError, Publisher, Registry, TrackId, TrackInfo, VideoParams,
};
use futures::StreamExt;
use retina::client::{Credentials, InitialTimestampPolicy, PlayOptions, Session, SessionOptions, SetupOptions};
use retina::codec::{CodecItem, ParametersRef};
use url::Url;

use crate::RtspPull;

const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// The video track is always announced as id 0, audio as id 1, matching the
/// convention `caudal-rtmp` uses.
const VIDEO_TRACK: TrackId = TrackId(0);
const AUDIO_TRACK: TrackId = TrackId(1);

type PullError = Box<dyn std::error::Error + Send + Sync>;

pub(crate) async fn run(pull: RtspPull, registry: Arc<Registry>, buffer: BufferConfig) {
    let mut backoff = MIN_BACKOFF;
    loop {
        match pull_once(&pull, &registry, buffer).await {
            Ok(()) => {
                tracing::info!(stream = %pull.stream, "rtsp pull: camera disconnected");
                backoff = MIN_BACKOFF;
            }
            Err(e) => {
                tracing::warn!(stream = %pull.stream, error = %e, "rtsp pull failed");
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Splits `user:pass@` out of the URL: retina refuses URLs carrying
/// credentials, wanting them as a separate [`Credentials`] instead.
fn split_credentials(raw: &str) -> Result<(Url, Option<Credentials>), PullError> {
    let mut url = Url::parse(raw)?;
    let username =
        percent_decode(url.username()).ok_or_else(|| PullError::from("invalid percent-encoding in username"))?;
    let password = match url.password() {
        Some(p) => Some(percent_decode(p).ok_or_else(|| PullError::from("invalid percent-encoding in password"))?),
        None => None,
    };
    let creds =
        if username.is_empty() { None } else { Some(Credentials { username, password: password.unwrap_or_default() }) };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    Ok((url, creds))
}

/// Minimal percent-decoder (camera credentials are rarely percent-encoded,
/// but `%40` etc. does show up for `@` in passwords).
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16)?;
            let lo = (bytes[i + 2] as char).to_digit(16)?;
            out.push((hi as u8) << 4 | lo as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Finds the first usable video (H.264/H.265) and audio (AAC) stream index.
fn pick_streams(streams: &[retina::client::Stream]) -> (Option<usize>, Option<usize>) {
    let mut video = None;
    let mut audio = None;
    for (i, s) in streams.iter().enumerate() {
        match (s.media(), s.encoding_name()) {
            ("video", "h264" | "h265") if video.is_none() => video = Some(i),
            ("audio", "mpeg4-generic") if audio.is_none() => audio = Some(i),
            _ => {}
        }
    }
    (video, audio)
}

fn video_track(stream: &retina::client::Stream, vp: &retina::codec::VideoParameters, h265: bool) -> TrackInfo {
    let (width, height) = vp.pixel_dimensions();
    let fps = vp.frame_rate().filter(|&(n, _)| n != 0).map(|(n, d)| f64::from(d) / f64::from(n));
    TrackInfo {
        id: VIDEO_TRACK,
        codec: if h265 { Codec::H265 } else { Codec::H264 },
        timescale: stream.clock_rate_hz(),
        init: Bytes::copy_from_slice(vp.extra_data()),
        lang: None,
        video: Some(VideoParams { width, height, fps }),
        audio: None,
    }
}

fn audio_track(stream: &retina::client::Stream, ap: &retina::codec::AudioParameters) -> TrackInfo {
    let sample_rate = stream.clock_rate_hz();
    let channels = stream.channels().map_or(2, |c| c.get().min(255) as u8);
    TrackInfo {
        id: AUDIO_TRACK,
        codec: Codec::Aac,
        timescale: sample_rate,
        init: Bytes::copy_from_slice(ap.extra_data()),
        lang: None,
        video: None,
        audio: Some(AudioParams { sample_rate, channels }),
    }
}

fn announce(
    publisher: &Publisher,
    video: &Option<TrackInfo>,
    audio: &Option<TrackInfo>,
) -> Result<(), caudal_core::PushError> {
    let mut tracks = Vec::new();
    if let Some(t) = video {
        tracks.push(t.clone());
    }
    if let Some(t) = audio {
        tracks.push(t.clone());
    }
    publisher.set_tracks(tracks)
}

async fn pull_once(pull: &RtspPull, registry: &Arc<Registry>, buffer: BufferConfig) -> Result<(), PullError> {
    let (url, creds) = split_credentials(&pull.url)?;
    let redacted = url.as_str().to_owned();

    let publisher = match registry.publish(&pull.stream, buffer) {
        Ok(p) => p,
        Err(PublishError::Busy(_)) => {
            tracing::debug!(stream = %pull.stream, "rtsp pull: stream name busy, will retry");
            return Ok(());
        }
        Err(e) => return Err(Box::new(e)),
    };

    tracing::info!(stream = %pull.stream, url = %redacted, "rtsp pull: connecting");
    let options = SessionOptions::default().creds(creds);
    let mut session = Session::describe(url, options).await?;

    let (video_i, audio_i) = pick_streams(session.streams());
    if video_i.is_none() && audio_i.is_none() {
        return Err("no usable H.264/H.265 video or AAC audio track advertised".into());
    }
    let is_h265 = video_i.is_some_and(|i| session.streams()[i].encoding_name() == "h265");
    if let Some(i) = video_i {
        session.setup(i, SetupOptions::default()).await?;
    }
    if let Some(i) = audio_i {
        session.setup(i, SetupOptions::default()).await?;
    }

    // Many cheap cameras (and our own RTSP server, which doesn't emit
    // `RTP-Info` at all) don't supply an initial rtptime per RFC 2326
    // §12.33; without this, retina's default policy refuses to PLAY a
    // multi-track session at all.
    let play_options = PlayOptions::default().initial_timestamp(InitialTimestampPolicy::Permissive);
    let playing = session.play(play_options).await?;
    let mut demuxed = playing.demuxed()?;

    let mut video_track_info: Option<TrackInfo> = None;
    let mut audio_track_info: Option<TrackInfo> = None;

    loop {
        let item = match demuxed.next().await {
            Some(r) => r?,
            None => return Ok(()),
        };
        match item {
            CodecItem::VideoFrame(vf) => {
                if Some(vf.stream_id()) != video_i {
                    continue;
                }
                if (video_track_info.is_none() || vf.has_new_parameters())
                    && let Some(ParametersRef::Video(vp)) = demuxed.streams()[vf.stream_id()].parameters()
                {
                    let info = video_track(&demuxed.streams()[vf.stream_id()], vp, is_h265);
                    video_track_info = Some(info);
                    announce(&publisher, &video_track_info, &audio_track_info)?;
                }
                if video_track_info.is_none() {
                    continue;
                }
                let ts = vf.timestamp().timestamp();
                publisher.push(Frame {
                    track: VIDEO_TRACK,
                    dts: ts,
                    pts: ts,
                    keyframe: vf.is_random_access_point(),
                    data: Bytes::copy_from_slice(vf.data()),
                })?;
            }
            CodecItem::AudioFrame(af) => {
                if Some(af.stream_id()) != audio_i {
                    continue;
                }
                if audio_track_info.is_none()
                    && let Some(ParametersRef::Audio(ap)) = demuxed.streams()[af.stream_id()].parameters()
                {
                    audio_track_info = Some(audio_track(&demuxed.streams()[af.stream_id()], ap));
                    announce(&publisher, &video_track_info, &audio_track_info)?;
                }
                if audio_track_info.is_none() {
                    continue;
                }
                let ts = af.timestamp().timestamp();
                publisher.push(Frame {
                    track: AUDIO_TRACK,
                    dts: ts,
                    pts: ts,
                    keyframe: true,
                    data: Bytes::copy_from_slice(af.data()),
                })?;
            }
            CodecItem::MessageFrame(_) | CodecItem::Rtcp(_) => {}
            _ => {}
        }
    }
}
