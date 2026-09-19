//! One task per recorded publish: reads the stream, runs the segmenter and
//! writes files.
//!
//! Crash safety: a segment is written to `seg-NNNNNN.m4s.tmp`, synced, and
//! renamed only when complete; the playlist and `meta.json` are rewritten
//! through a temp file + rename after every closed segment. A crash leaves
//! complete segments and a playlist listing only complete segments.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use caudal_core::{Event, Stream, Subscriber};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

use crate::Shared;
use crate::meta::{Meta, SegEntry, TrackMeta, id_for, playlist, rfc3339, segment_name, write_atomic};
use crate::segmenter::{Out, Segmenter};

struct OpenSeg {
    index: u32,
    tmp: PathBuf,
    file: tokio::fs::File,
    bytes: u64,
    discontinuity: bool,
    pdt: SystemTime,
}

pub(crate) struct Recording {
    dir: PathBuf,
    meta: Meta,
    segments: Vec<SegEntry>,
    seconds: f64,
    open: Option<OpenSeg>,
    init_uploaded: bool,
}

impl Recording {
    fn key(&self, file: &str) -> String {
        format!("{}/{}/{}", self.meta.stream, self.meta.id, file)
    }
}

/// `gate`: `None` records unconditionally (a `[record] streams` match, the
/// original behaviour, untouched below); `Some(rx)` is a schedule gate —
/// frames and track changes are only applied to the segmenter while `*rx.
/// borrow()` is true. The task itself lives for as long as the stream does
/// either way, so it is already running (and sees the live tracks) the
/// instant a window opens, including one that opens while the stream was
/// already live.
pub(crate) async fn run(
    shared: Arc<Shared>,
    stream: Arc<Stream>,
    mut sub: Subscriber,
    mut gate: Option<watch::Receiver<bool>>,
) {
    let name = stream.name().to_owned();
    let mut seg = Segmenter::new(shared.cfg.segment_secs.max(1).saturating_mul(1000));
    let mut rec: Option<Recording> = None;
    // The next frame is the oldest in the buffer (start, or after a lag):
    // its wall clock is "now" minus what is buffered after it.
    let mut fresh = true;
    let mut active = gate.as_ref().is_none_or(|g| *g.borrow());
    loop {
        let ev = match gate.as_mut() {
            Some(g) => {
                tokio::select! {
                    biased;
                    changed = g.changed() => {
                        if changed.is_err() {
                            // The scheduler is gone (shouldn't happen: its
                            // tasks run for the process lifetime). Freeze
                            // the gate as it last stood rather than guess.
                            gate = None;
                            continue;
                        }
                        let now_active = *g.borrow_and_update();
                        if now_active == active {
                            continue;
                        }
                        active = now_active;
                        if !active {
                            // Close cleanly, exactly like falling behind:
                            // flush what's open, mark the next start as a
                            // discontinuity (fresh anchor, wait for a
                            // keyframe) for whenever the next window opens.
                            seg.lagged();
                            let outs = seg.take();
                            if let Err(e) = apply(&shared, &name, &seg, &mut rec, outs).await {
                                tracing::error!(stream = %name, error = %e, "recording stopped: write failed");
                                if let Some(r) = rec.take() {
                                    finish(&shared, r, Some(format!("write failed: {e}"))).await;
                                }
                                break;
                            }
                            if let Some(done) = rec.take() {
                                tracing::info!(stream = %name, id = %done.meta.id, "schedule window closed");
                                finish(&shared, done, None).await;
                            }
                        } else {
                            fresh = true;
                            tracing::info!(stream = %name, "schedule window opened; recording starts at the next keyframe");
                        }
                        continue;
                    }
                    ev = sub.recv() => ev,
                }
            }
            None => sub.recv().await,
        };
        let end = ev == Event::End;
        let res = match ev {
            Event::TracksChanged if active => {
                if seg.set_tracks(&sub.tracks()) {
                    let outs = seg.take();
                    let r = apply(&shared, &name, &seg, &mut rec, outs).await;
                    if r.is_ok()
                        && let Some(done) = rec.take()
                    {
                        tracing::info!(stream = %name, id = %done.meta.id, "tracks changed; starting a new recording");
                        finish(&shared, done, None).await;
                    }
                    if seg.init.is_none() {
                        tracing::warn!(stream = %name, "no recordable track (H.264/H.265 + AAC/Opus); waiting");
                    }
                    r
                } else {
                    Ok(())
                }
            }
            // Inactive: still track what the live tracks are (cheap, no
            // segments come out of it) so the segmenter isn't stale about
            // codecs/init once the next window opens.
            Event::TracksChanged => {
                seg.set_tracks(&sub.tracks());
                seg.take();
                Ok(())
            }
            Event::Frame(f) if active => {
                let mut now = SystemTime::now();
                if std::mem::take(&mut fresh) {
                    now -= Duration::from_micros(stream.stats().buffered_micros.max(0) as u64);
                }
                seg.push(&f, now);
                let outs = seg.take();
                apply(&shared, &name, &seg, &mut rec, outs).await
            }
            Event::Frame(_) => Ok(()),
            Event::Lagged { skipped } if active => {
                tracing::warn!(stream = %name, skipped, "recorder fell behind; frames skipped (discontinuity)");
                seg.lagged();
                fresh = true;
                let outs = seg.take();
                apply(&shared, &name, &seg, &mut rec, outs).await
            }
            Event::Lagged { .. } => Ok(()),
            Event::Cue(_) => Ok(()),
            Event::End => {
                seg.end();
                let outs = seg.take();
                apply(&shared, &name, &seg, &mut rec, outs).await
            }
        };
        if let Err(e) = res {
            tracing::error!(stream = %name, error = %e, "recording stopped: write failed");
            if let Some(r) = rec.take() {
                finish(&shared, r, Some(format!("write failed: {e}"))).await;
            }
            break;
        }
        if end {
            if let Some(r) = rec.take() {
                finish(&shared, r, None).await;
            }
            break;
        }
    }
    drop(sub);
    let mut map = shared.recorders.lock();
    if map.get(&name).is_some_and(|s| Arc::ptr_eq(s, &stream)) {
        map.remove(&name);
    }
}

async fn apply(
    shared: &Arc<Shared>,
    stream: &str,
    seg: &Segmenter,
    rec: &mut Option<Recording>,
    outs: Vec<Out>,
) -> std::io::Result<()> {
    for out in outs {
        match out {
            Out::Open { discontinuity, pdt } => {
                if rec.is_none() {
                    *rec = Some(create(shared, stream, seg, pdt).await?);
                }
                let r = rec.as_mut().unwrap();
                let index = r.segments.len() as u32 + 1;
                let tmp = r.dir.join(format!("{}.tmp", segment_name(index)));
                let file = tokio::fs::File::create(&tmp).await?;
                let discontinuity = discontinuity && !r.segments.is_empty();
                r.open = Some(OpenSeg { index, tmp, file, bytes: 0, discontinuity, pdt });
            }
            Out::Fragment(b) => {
                let Some(o) = rec.as_mut().and_then(|r| r.open.as_mut()) else { continue };
                o.file.write_all(&b).await?;
                o.bytes += b.len() as u64;
            }
            Out::Close { duration } => {
                let Some(r) = rec.as_mut() else { continue };
                let Some(mut o) = r.open.take() else { continue };
                o.file.flush().await?;
                o.file.sync_data().await?;
                drop(o.file);
                let name = segment_name(o.index);
                let path = r.dir.join(&name);
                tokio::fs::rename(&o.tmp, &path).await?;
                let first = r.segments.is_empty();
                r.segments.push(SegEntry {
                    index: o.index,
                    duration,
                    discontinuity: o.discontinuity,
                    pdt: (first || o.discontinuity).then_some(o.pdt),
                });
                r.seconds += duration;
                r.meta.segments = r.segments.len() as u32;
                r.meta.duration_ms = (r.seconds * 1000.0).round() as u64;
                r.meta.bytes += o.bytes;
                write_playlist(shared, r, false).await?;
                write_meta(r).await?;
                if let Some(up) = &shared.uploader {
                    up.enqueue(path, r.key(&name));
                    if !r.init_uploaded {
                        up.enqueue(r.dir.join("init.mp4"), r.key("init.mp4"));
                        r.init_uploaded = true;
                    }
                    up.enqueue(r.dir.join("index.m3u8"), r.key("index.m3u8"));
                    up.enqueue(r.dir.join("meta.json"), r.key("meta.json"));
                }
            }
        }
    }
    Ok(())
}

async fn create(
    shared: &Arc<Shared>,
    stream: &str,
    seg: &Segmenter,
    started: SystemTime,
) -> std::io::Result<Recording> {
    let parent = shared.cfg.dir.join(stream);
    tokio::fs::create_dir_all(&parent).await?;
    let base = id_for(started);
    let mut n = 1;
    let (id, dir) = loop {
        let id = if n == 1 { base.clone() } else { format!("{base}-{n}") };
        let dir = parent.join(&id);
        match tokio::fs::create_dir(&dir).await {
            Ok(()) => break (id, dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 999 => n += 1,
            Err(e) => return Err(e),
        }
    };
    shared.active.lock().insert((stream.to_owned(), id.clone()));
    let init = seg.init.clone().unwrap_or_default();
    let r = Recording {
        dir: dir.clone(),
        meta: Meta {
            stream: stream.to_owned(),
            id: id.clone(),
            started_at: rfc3339(started),
            ended_at: None,
            duration_ms: 0,
            bytes: init.len() as u64,
            segments: 0,
            tracks: seg.tracks.iter().map(TrackMeta::from_track).collect(),
            error: None,
        },
        segments: Vec::new(),
        seconds: 0.0,
        open: None,
        init_uploaded: false,
    };
    let res = async {
        write_atomic(&dir.join("init.mp4"), &init).await?;
        write_playlist(shared, &r, false).await?;
        write_meta(&r).await
    }
    .await;
    if let Err(e) = res {
        finish(shared, r, Some(format!("write failed: {e}"))).await;
        return Err(e);
    }
    tracing::info!(stream, %id, "recording started");
    Ok(r)
}

async fn write_playlist(shared: &Shared, r: &Recording, ended: bool) -> std::io::Result<()> {
    let text = playlist(&r.segments, u64::from(shared.cfg.segment_secs.max(1)), ended);
    write_atomic(&r.dir.join("index.m3u8"), text.as_bytes()).await
}

async fn write_meta(r: &Recording) -> std::io::Result<()> {
    let json = serde_json::to_vec_pretty(&r.meta).map_err(std::io::Error::other)?;
    write_atomic(&r.dir.join("meta.json"), &json).await
}

/// Closes a recording: VOD playlist with ENDLIST, `ended_at`, and the error
/// if it stopped early. Best effort: on a full disk these writes may fail
/// too, which is logged; the segments already on disk stay playable.
async fn finish(shared: &Arc<Shared>, mut r: Recording, error: Option<String>) {
    if let Some(o) = r.open.take() {
        drop(o.file);
        let _ = tokio::fs::remove_file(&o.tmp).await;
    }
    r.meta.ended_at = Some(rfc3339(SystemTime::now()));
    r.meta.error = error;
    if let Err(e) = write_playlist(shared, &r, true).await {
        tracing::error!(stream = %r.meta.stream, id = %r.meta.id, error = %e, "could not finish playlist");
    }
    if let Err(e) = write_meta(&r).await {
        tracing::error!(stream = %r.meta.stream, id = %r.meta.id, error = %e, "could not write meta.json");
    }
    shared.active.lock().remove(&(r.meta.stream.clone(), r.meta.id.clone()));
    if let Some(up) = &shared.uploader {
        up.enqueue(r.dir.join("index.m3u8"), r.key("index.m3u8"));
        up.enqueue(r.dir.join("meta.json"), r.key("meta.json"));
    }
    tracing::info!(stream = %r.meta.stream, id = %r.meta.id, segments = r.meta.segments, "recording ended");
}
