//! MoQ ingest: an encoder publishes a `hang` broadcast at `publish/<name>`;
//! this module reads its catalog and tracks and turns it into a normal
//! `caudal_core::Publisher`, so every existing output (LL-HLS, WHEP, RTMP
//! restream, recording, MoQ playback...) carries it exactly like an RTMP or
//! WHIP publish. `hang`/`moq-mux` types never leave this module — the rest
//! of Caudal only ever sees `caudal_core::Frame`.
//!
//! Only H.264/H.265 (avcC/hvcC) and AAC/Opus are accepted, matching every
//! other ingest path in Caudal, and only the first video and first audio
//! rendition the catalog offers are used (an ABR ladder published straight
//! into Caudal is not supported; transcode from the single rendition
//! instead, same as any other ingest). Container framing is whatever the
//! catalog declares (Legacy, CMAF, LOC all decode); the codec payloads
//! themselves must be AVCC/HVCC access units, not Annex B, matching
//! `caudal_core::Frame` everywhere else in Caudal. A later catalog snapshot
//! that changes the chosen renditions is not applied: the tracks announced
//! at the first usable snapshot are used for the whole publish.

use std::sync::Arc;
use std::time::Duration;

use caudal_core::{AudioParams, BufferConfig, Codec, Frame, Registry, TrackId, TrackInfo, VideoParams};
use moq_mux::catalog::hang::Container as WireContainer;
use moq_mux::catalog::{CatalogFormat, Consumer as CatalogConsumer, Stream as CatalogStream};

type Error = Box<dyn std::error::Error + Send + Sync>;
type TrackConsumer = moq_mux::container::Consumer<WireContainer>;

const VIDEO: TrackId = TrackId(0);
const AUDIO: TrackId = TrackId(1);

/// How long to wait for a first usable catalog snapshot (naming at least
/// one video or audio rendition Caudal can use) before giving up.
const CATALOG_TIMEOUT: Duration = Duration::from_secs(10);

/// Waits for `name` to be announced on `origin` (the encoder may connect
/// and start announcing at any point after the session handshake), then
/// converts it. Runs until the broadcast or the registry publish ends, or
/// the caller aborts it when the session itself closes.
pub(crate) async fn run(
    registry: Arc<Registry>,
    origin: moq_net::origin::Consumer,
    name: String,
    buffer: BufferConfig,
) {
    let Some(broadcast) = origin.announced_broadcast(name.as_str()).await else {
        tracing::debug!(stream = %name, "moq: ingest closed before the broadcast was announced");
        return;
    };
    if let Err(e) = pump(&registry, broadcast, &name, buffer).await {
        tracing::warn!(stream = %name, error = %e, "moq: ingest failed");
    }
}

/// One video and/or audio rendition picked from a catalog snapshot: track
/// name, `caudal_core` track info, and the wire container it was declared
/// with (so `pump` can subscribe without re-reading the catalog).
struct Chosen {
    video: Option<(String, TrackInfo, hang::catalog::Container)>,
    audio: Option<(String, TrackInfo, hang::catalog::Container)>,
}

async fn first_usable(catalog: &mut CatalogConsumer<()>) -> Result<Chosen, Error> {
    loop {
        let Some(snap) = catalog.next().await? else {
            return Err("broadcast ended before publishing a usable track".into());
        };
        let video = match snap.video.renditions.iter().next() {
            Some((name, cfg)) => match video_track_info(cfg) {
                Ok(info) => Some((name.clone(), info, cfg.container.clone())),
                Err(e) => {
                    tracing::warn!(rendition = %name, error = %e, "moq: video rendition unusable; ignored");
                    None
                }
            },
            None => None,
        };
        if snap.video.renditions.len() > 1 {
            tracing::warn!(count = snap.video.renditions.len(), "moq: ingest only uses the first video rendition");
        }
        let audio = match snap.audio.renditions.iter().next() {
            Some((name, cfg)) => match audio_track_info(cfg) {
                Ok(info) => Some((name.clone(), info, cfg.container.clone())),
                Err(e) => {
                    tracing::warn!(rendition = %name, error = %e, "moq: audio rendition unusable; ignored");
                    None
                }
            },
            None => None,
        };
        if snap.audio.renditions.len() > 1 {
            tracing::warn!(count = snap.audio.renditions.len(), "moq: ingest only uses the first audio rendition");
        }
        if video.is_some() || audio.is_some() {
            return Ok(Chosen { video, audio });
        }
        // Catalogs can start empty and fill in shortly after; keep waiting
        // for a snapshot with something usable, up to the caller's timeout.
    }
}

fn video_track_info(cfg: &hang::catalog::VideoConfig) -> Result<TrackInfo, Error> {
    let codec = match cfg.codec.kind() {
        hang::catalog::VideoCodecKind::H264 => Codec::H264,
        hang::catalog::VideoCodecKind::H265 => Codec::H265,
        other => return Err(format!("unsupported video codec {other:?} (only H.264/H.265)").into()),
    };
    let init = cfg.description.clone().ok_or("video track has no avcC/hvcC description")?;
    if init.starts_with(&[0, 0, 1]) || init.starts_with(&[0, 0, 0, 1]) {
        return Err("video description looks like Annex B, not an avcC/hvcC record".into());
    }
    let video = match (cfg.coded_width, cfg.coded_height) {
        (Some(width), Some(height)) => Some(VideoParams { width, height, fps: cfg.framerate }),
        _ => None,
    };
    Ok(TrackInfo { id: VIDEO, codec, timescale: 1_000_000, init, lang: None, video, audio: None })
}

fn audio_track_info(cfg: &hang::catalog::AudioConfig) -> Result<TrackInfo, Error> {
    let codec = match cfg.codec.kind() {
        hang::catalog::AudioCodecKind::AAC => Codec::Aac,
        hang::catalog::AudioCodecKind::Opus => Codec::Opus,
        other => return Err(format!("unsupported audio codec {other:?} (only AAC/Opus)").into()),
    };
    let init = match (&cfg.description, codec) {
        (Some(d), _) => d.clone(),
        // Opus decodes without an OpusHead when channels/sample rate are
        // already known from the catalog; AAC's AudioSpecificConfig is load-
        // bearing (object type, sample rate index), so it's required.
        (None, Codec::Opus) => bytes::Bytes::new(),
        (None, _) => return Err("audio track has no AudioSpecificConfig description".into()),
    };
    let audio = Some(AudioParams { sample_rate: cfg.sample_rate, channels: cfg.channel_count.min(255) as u8 });
    Ok(TrackInfo { id: AUDIO, codec, timescale: 1_000_000, init, lang: None, video: None, audio })
}

async fn subscribe(
    bc: &moq_net::broadcast::Consumer,
    name: &str,
    container: &hang::catalog::Container,
) -> Result<TrackConsumer, Error> {
    let wire = WireContainer::try_from(container).map_err(|e| format!("track {name}: {e}"))?;
    let sub = bc.track(name)?.subscribe(None).await?;
    Ok(TrackConsumer::new(sub, wire))
}

enum Msg {
    Frame(TrackId, moq_mux::container::Frame),
    Error(Error),
}

async fn read_track(mut sub: TrackConsumer, track: TrackId, tx: tokio::sync::mpsc::Sender<Msg>) {
    loop {
        match sub.read().await {
            Ok(Some(f)) => {
                if tx.send(Msg::Frame(track, f)).await.is_err() {
                    return;
                }
            }
            Ok(None) => return,
            Err(e) => {
                let _ = tx.send(Msg::Error(Box::new(e))).await;
                return;
            }
        }
    }
}

async fn pump(
    registry: &Arc<Registry>,
    broadcast: moq_net::broadcast::Consumer,
    name: &str,
    buffer: BufferConfig,
) -> Result<(), Error> {
    let mut catalog = CatalogConsumer::<()>::new(&broadcast, CatalogFormat::Hang).await?;
    let chosen = tokio::time::timeout(CATALOG_TIMEOUT, first_usable(&mut catalog))
        .await
        .map_err(|_| "no usable track in the catalog within 10s")??;

    let publisher = registry.publish(name, buffer).map_err(|e| e.to_string())?;
    let mut tracks = Vec::new();
    if let Some((_, info, _)) = &chosen.video {
        tracks.push(info.clone());
    }
    if let Some((_, info, _)) = &chosen.audio {
        tracks.push(info.clone());
    }
    publisher.set_tracks(tracks)?;
    tracing::info!(
        stream = name,
        video = chosen.video.is_some(),
        audio = chosen.audio.is_some(),
        "moq: ingest started"
    );

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    if let Some((track_name, _, container)) = &chosen.video {
        let sub = subscribe(&broadcast, track_name, container).await?;
        tokio::spawn(read_track(sub, VIDEO, tx.clone()));
    }
    if let Some((track_name, _, container)) = &chosen.audio {
        let sub = subscribe(&broadcast, track_name, container).await?;
        tokio::spawn(read_track(sub, AUDIO, tx.clone()));
    }
    drop(tx);

    while let Some(msg) = rx.recv().await {
        match msg {
            Msg::Frame(track, f) => {
                let micros = f.timestamp.as_micros() as i64;
                let frame = Frame { track, dts: micros, pts: micros, keyframe: f.keyframe, data: f.payload };
                if let Err(e) = publisher.push(frame) {
                    return Err(e.into());
                }
            }
            Msg::Error(e) => return Err(e),
        }
    }
    tracing::info!(stream = name, "moq: ingest ended");
    Ok(())
}
