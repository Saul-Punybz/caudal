//! Registry stream → `hang` broadcast.
//!
//! One task per live stream reads it like an internal packager
//! (`subscribe_internal`, so MoQ viewers are counted separately) and writes
//! each track through a `moq_mux::container::Producer` in the Legacy
//! container: a microsecond timestamp plus the codec payload, which for
//! H.264/H.265 is our AVCC/HVCC access unit as-is. The catalog carries the
//! avcC/hvcC/AudioSpecificConfig/OpusHead as `description`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use caudal_core::{Codec, Event, Frame, Registry, StartAt, Stream, TrackId, TrackInfo, TrackKind};
use moq_mux::catalog::hang::Container;
use moq_net::Timestamp;
use tokio::sync::broadcast::error::RecvError;

type Error = Box<dyn std::error::Error + Send + Sync>;
type MediaProducer = moq_mux::container::Producer<Container>;

/// How long a group other than the newest stays cached for MoQ viewers.
/// hang's default is 30 s, which kept a second copy of every stream's last
/// 30 s in memory whether or not anyone watched over MoQ (about 25 MB for a
/// 6 Mbps stream, measured with dhat: bench/heap.sh). A viewer joins at the
/// newest group and one further behind than this has lost the live edge
/// anyway; 5 s is moq-net's own default (`moq_net::track::DEFAULT_LATENCY_MAX`).
const LATENCY_MAX: Duration = moq_net::track::DEFAULT_LATENCY_MAX;

fn track_info() -> moq_net::track::Info {
    hang::container::track_info().with_latency_max(LATENCY_MAX)
}

/// Longest audio group in an audio-only stream. With video, audio groups
/// follow the video keyframes instead, so a late joiner gets both at once.
const AUDIO_GROUP: Duration = Duration::from_secs(1);

/// Publishes every current and future stream of `registry` into `origin`.
pub(crate) fn spawn_all(registry: Arc<Registry>, origin: moq_net::origin::Producer) {
    // Subscribe before listing so a stream published in between is not
    // missed; `Live` removes the duplicate it may cause.
    let mut publishes = registry.subscribe_publishes();
    let live = Live::default();
    for stream in registry.list() {
        live.spawn(&origin, stream);
    }
    tokio::spawn(async move {
        loop {
            match publishes.recv().await {
                Ok(stream) => live.spawn(&origin, stream),
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "moq: publish events lagged; rescanning streams");
                    for stream in registry.list() {
                        live.spawn(&origin, stream);
                    }
                }
                Err(RecvError::Closed) => break,
            }
        }
    });
}

/// Streams that already have a broadcast task, by pointer identity (a name
/// can be republished as a new stream while the old task drains).
#[derive(Default, Clone)]
struct Live(Arc<parking_lot::Mutex<HashSet<usize>>>);

impl Live {
    fn spawn(&self, origin: &moq_net::origin::Producer, stream: Arc<Stream>) {
        let key = Arc::as_ptr(&stream) as usize;
        if stream.is_ended() || !self.0.lock().insert(key) {
            return;
        }
        let guard = LiveGuard { live: self.clone(), key };
        let origin = origin.clone();
        tokio::spawn(async move {
            let _guard = guard;
            let name = stream.name().to_owned();
            match run(origin, stream).await {
                Ok(()) => tracing::debug!(stream = %name, "moq: broadcast ended"),
                Err(e) => tracing::warn!(stream = %name, error = %e, "moq: broadcast failed"),
            }
        });
    }
}

/// Removes the stream from [`Live`] however its task ends, panics included.
struct LiveGuard {
    live: Live,
    key: usize,
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.live.0.lock().remove(&self.key);
    }
}

async fn run(origin: moq_net::origin::Producer, stream: Arc<Stream>) -> Result<(), Error> {
    let mut sub = stream.subscribe_internal(StartAt::LiveEdge);
    let mut broadcast = origin.create_broadcast(stream.name(), moq_net::broadcast::Route::new().with_announce(true))?;
    let mut catalog = moq_mux::catalog::Producer::new(&mut broadcast)?;
    let mut out = Outputs::default();

    let result = loop {
        match sub.recv().await {
            Event::TracksChanged => {
                if let Err(e) = out.rebuild(stream.name(), &sub.tracks(), &mut broadcast, &mut catalog) {
                    break Err(e);
                }
            }
            Event::Frame(frame) => out.write(stream.name(), &frame),
            Event::Lagged { skipped } => {
                tracing::debug!(stream = %stream.name(), skipped, "moq: lagged; next group starts at a keyframe");
                out.cut_all();
            }
            Event::Cue(_) => {}
            Event::End => break Ok(()),
        }
    };

    // Close cleanly either way, so players see the broadcast end instead of
    // waiting on it.
    out.finish_all();
    if let Err(e) = catalog.finish() {
        tracing::debug!(error = %e, "moq: catalog finish");
    }
    broadcast.finish();
    result
}

struct Output {
    info: TrackInfo,
    name: String,
    producer: MediaProducer,
    /// Timestamp (µs) of the frame that opened the current audio group.
    group_start: u64,
    /// Audio: a video keyframe arrived, open a new group with the next frame.
    cut_pending: bool,
}

#[derive(Default)]
struct Outputs {
    tracks: HashMap<TrackId, Output>,
    /// Bumped on every rebuild that creates tracks, so a reconfigured track
    /// gets a fresh name instead of reusing one a player may still hold.
    generation: u32,
    has_video: bool,
}

/// Catalog entry for a track, or `None` when the codec has no hang mapping.
enum Rendition {
    Video(hang::catalog::VideoConfig),
    Audio(hang::catalog::AudioConfig),
}

fn rendition(t: &TrackInfo) -> Result<Option<Rendition>, Error> {
    let rendition = match t.codec {
        // avcC / hvcC go verbatim into `description` (avc1 / hvc1: parameter
        // sets out of band, AVCC payloads as-is).
        Codec::H264 | Codec::H265 => {
            if t.init.is_empty() || t.init.starts_with(&[0, 0, 1]) || t.init.starts_with(&[0, 0, 0, 1]) {
                return Err("video track without an avcC/hvcC record".into());
            }
            let mut v = match t.codec {
                Codec::H264 => moq_mux::codec::h264::config(&t.init)?,
                _ => moq_mux::codec::h265::config(&t.init)?,
            };
            if let Some(p) = t.video {
                v.coded_width = v.coded_width.or(Some(p.width));
                v.coded_height = v.coded_height.or(Some(p.height));
                v.framerate = v.framerate.or(p.fps);
            }
            v.container = hang::catalog::Container::Legacy;
            Rendition::Video(v)
        }
        Codec::Aac => {
            let mut a = moq_mux::codec::aac::config(&t.init)?;
            a.container = hang::catalog::Container::Legacy;
            Rendition::Audio(a)
        }
        Codec::Opus => {
            let mut a = match moq_mux::codec::opus::config(&t.init) {
                Ok(a) => a,
                // No usable OpusHead: the codec needs none to decode stereo/mono.
                Err(_) => {
                    let p = t.audio.unwrap_or(caudal_core::AudioParams { sample_rate: 48_000, channels: 2 });
                    hang::catalog::AudioConfig::new(hang::catalog::AudioCodec::Opus, p.sample_rate, p.channels.into())
                }
            };
            a.container = hang::catalog::Container::Legacy;
            Rendition::Audio(a)
        }
        _ => return Ok(None),
    };
    Ok(Some(rendition))
}

impl Outputs {
    fn rebuild(
        &mut self,
        stream: &str,
        tracks: &[TrackInfo],
        broadcast: &mut moq_net::broadcast::Producer,
        catalog: &mut moq_mux::catalog::Producer,
    ) -> Result<(), Error> {
        // Drop tracks that disappeared or changed.
        let stale: Vec<TrackId> = self
            .tracks
            .iter()
            .filter(|(id, o)| !tracks.iter().any(|t| t.id == **id && *t == o.info))
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            if let Some(mut o) = self.tracks.remove(&id) {
                let _ = o.producer.finish();
                let _ = broadcast.remove_track(&o.name);
            }
        }

        let mut video = hang::catalog::Video::default();
        let mut audio = hang::catalog::Audio::default();
        let mut created = false;
        for t in tracks {
            let r = match rendition(t) {
                Ok(Some(r)) => r,
                Ok(None) => {
                    tracing::info!(stream, codec = t.codec.as_str(), "moq: codec not carried; track skipped");
                    continue;
                }
                Err(e) => {
                    tracing::warn!(stream, codec = t.codec.as_str(), error = %e, "moq: bad codec config; track skipped");
                    continue;
                }
            };
            if !self.tracks.contains_key(&t.id) {
                let kind = t.kind().as_str();
                let name = match self.generation {
                    0 => format!("{kind}{}", t.id.0),
                    g => format!("{kind}{}.{g}", t.id.0),
                };
                let track = broadcast.create_track(name.as_str(), track_info())?;
                let producer = MediaProducer::new(track, Container::Legacy);
                self.tracks
                    .insert(t.id, Output { info: t.clone(), name, producer, group_start: 0, cut_pending: false });
                created = true;
            }
            let name = &self.tracks[&t.id].name;
            match r {
                Rendition::Video(v) => video.insert(name, v)?,
                Rendition::Audio(a) => audio.insert(name, a)?,
            }
        }
        if created {
            self.generation += 1;
        }
        self.has_video = self.tracks.values().any(|o| o.info.kind() == TrackKind::Video);

        let mut c = catalog.lock();
        c.video = video;
        c.audio = audio;
        c.commit()?;
        Ok(())
    }

    fn write(&mut self, stream: &str, frame: &Frame) {
        let Some(kind) = self.tracks.get(&frame.track).map(|o| o.info.kind()) else {
            return;
        };
        if kind == TrackKind::Video && frame.keyframe {
            // Audio groups follow video GOPs.
            for o in self.tracks.values_mut() {
                if o.info.kind() == TrackKind::Audio {
                    o.cut_pending = true;
                }
            }
        }
        let has_video = self.has_video;
        let Some(o) = self.tracks.get_mut(&frame.track) else {
            return;
        };
        let micros = o.info.to_micros(frame.pts).max(0) as u64;
        let Ok(timestamp) = Timestamp::from_micros(micros) else {
            return;
        };
        let keyframe = match kind {
            TrackKind::Video => {
                if !frame.keyframe && o.producer.needs_keyframe() {
                    // Joined mid-GOP (or after a cut): wait for the next keyframe.
                    return;
                }
                let reorder = frame.pts - frame.dts;
                if reorder > 0
                    && let Ok(d) = Timestamp::from_micros(o.info.to_micros(reorder) as u64)
                {
                    o.producer.reorder(d);
                }
                frame.keyframe
            }
            TrackKind::Audio => {
                let long = !has_video && micros.saturating_sub(o.group_start) >= AUDIO_GROUP.as_micros() as u64;
                let new_group = o.producer.needs_keyframe() || o.cut_pending || long;
                if new_group {
                    o.group_start = micros;
                    o.cut_pending = false;
                }
                new_group
            }
            _ => return,
        };
        let f = moq_mux::container::Frame { timestamp, payload: frame.data.clone(), keyframe, duration: None };
        if let Err(e) = o.producer.write(f) {
            tracing::debug!(stream, track = %o.name, error = %e, "moq: frame dropped");
        }
    }

    fn cut_all(&mut self) {
        for o in self.tracks.values_mut() {
            let _ = o.producer.cut(None);
        }
    }

    fn finish_all(&mut self) {
        for o in self.tracks.values_mut() {
            let _ = o.producer.finish();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn track(codec: Codec, init: &'static [u8]) -> TrackInfo {
        TrackInfo {
            id: TrackId(0),
            codec,
            timescale: 48_000,
            init: Bytes::from_static(init),
            lang: None,
            video: None,
            audio: Some(caudal_core::AudioParams { sample_rate: 48_000, channels: 2 }),
        }
    }

    #[test]
    fn aac_rendition_keeps_asc() {
        // AAC-LC, 48 kHz, stereo.
        let Some(Rendition::Audio(a)) = rendition(&track(Codec::Aac, &[0x11, 0x90])).unwrap() else { panic!() };
        assert_eq!(a.codec.to_string(), "mp4a.40.2");
        assert_eq!(a.sample_rate, 48_000);
        assert_eq!(a.channel_count, 2);
        assert_eq!(a.description.as_deref(), Some(&[0x11, 0x90][..]));
        assert_eq!(a.container, hang::catalog::Container::Legacy);
    }

    #[test]
    fn opus_rendition() {
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00";
        let Some(Rendition::Audio(a)) = rendition(&track(Codec::Opus, head)).unwrap() else { panic!() };
        assert_eq!(a.codec.to_string(), "opus");
        assert_eq!(a.channel_count, 2);
    }

    #[test]
    fn unsupported_and_annexb() {
        assert!(rendition(&track(Codec::Mp3, b"")).unwrap().is_none());
        assert!(rendition(&track(Codec::H264, &[0, 0, 0, 1, 0x67])).is_err());
    }
}
