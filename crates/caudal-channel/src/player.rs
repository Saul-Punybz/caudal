//! One channel's task: walks the playlist, opens each file off the
//! runtime, stitches timestamps and paces frames into the publisher.
//!
//! Timeline. The channel keeps, per track, the last timestamp it pushed
//! and the last frame duration it saw. When a file starts, the channel
//! clock `base` is the latest end over all tracks (last dts + duration, in
//! microseconds), and every track's offset is `base` on that track's own
//! clock (never below its last dts + 1). A file's frames are rebased to
//! its first video keyframe (`t0`): `out = dts - t0 + offset`. Using one
//! common `base` keeps audio and video aligned across any number of files,
//! and the "+1" floor keeps each track strictly monotonic even if a file's
//! first frame would land on the previous file's last tick.
//!
//! Pacing. The first pushed frame anchors media time to the wall clock;
//! every frame is pushed [`LEAD`] before it is due. If the channel falls
//! more than [`CATCH_UP`] behind (a slow disk, an idle spell), it
//! re-anchors instead of bursting to catch up.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use caudal_core::media::valid_stream_name;
use caudal_core::{BufferConfig, Frame, PublishError, Publisher, Registry, TrackInfo, TrackKind};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::source::{self, Source};
use crate::{Channel, ChannelState, LEAD, NowPlaying, RETRY, Shared};

/// Frames read per blocking call.
const BATCH: usize = 32;
/// Audio frames held while waiting for a file's first video keyframe.
const MAX_PENDING: usize = 2_000;
const CATCH_UP: Duration = Duration::from_secs(1);

/// What playing one item came to.
#[derive(Default)]
struct Outcome {
    played: bool,
    error: Option<String>,
    /// The stream name cannot be claimed: stop this pass and wait.
    blocked: bool,
}

impl Outcome {
    fn failed(msg: impl Into<String>) -> Self {
        Self { error: Some(msg.into()), ..Self::default() }
    }
}

/// The stream while it is on air: the publisher and the stitched clock.
struct OnAir {
    publisher: Publisher,
    tracks: Vec<TrackInfo>,
    last_dts: Vec<Option<i64>>,
    last_delta: Vec<i64>,
    /// Channel clock (µs) where the next file starts, if nothing was pushed.
    base: i64,
}

impl OnAir {
    fn new(publisher: Publisher, tracks: Vec<TrackInfo>) -> Self {
        let n = tracks.len();
        Self { publisher, tracks, last_dts: vec![None; n], last_delta: vec![0; n], base: 0 }
    }

    /// Latest end over all tracks, in microseconds.
    fn end_micros(&self) -> i64 {
        let mut end = self.base;
        for (i, t) in self.tracks.iter().enumerate() {
            if let Some(last) = self.last_dts[i] {
                end = end.max(t.to_micros(last + self.last_delta[i]));
            }
        }
        end
    }

    /// Per-track offsets for the next file.
    fn begin_file(&mut self) -> Vec<i64> {
        self.base = self.end_micros();
        self.tracks
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let ts = i128::from(t.timescale.max(1));
                let off = ((i128::from(self.base) * ts + 999_999) / 1_000_000) as i64;
                match self.last_dts[i] {
                    Some(last) => off.max(last + 1),
                    None => off,
                }
            })
            .collect()
    }

    /// Rebases `f` onto the channel clock and enforces strict monotonicity.
    fn stamp(&mut self, mut f: Frame, t0: i64, offsets: &[i64]) -> Frame {
        let i = f.track.0 as usize;
        let ts = i128::from(self.tracks[i].timescale.max(1));
        let t0_ts = (i128::from(t0) * ts / 1_000_000) as i64;
        let shift = offsets[i] - t0_ts;
        f.dts += shift;
        f.pts += shift;
        if let Some(last) = self.last_dts[i] {
            if f.dts <= last {
                let bump = last + 1 - f.dts;
                f.dts += bump;
                f.pts += bump;
            }
            self.last_delta[i] = f.dts - last;
        }
        self.last_dts[i] = Some(f.dts);
        f
    }
}

/// Maps media time to wall time.
#[derive(Default)]
struct Clock {
    anchor: Option<(Instant, i64)>,
}

impl Clock {
    /// When to push a frame at channel time `m` (µs).
    fn push_at(&mut self, m: i64) -> Instant {
        let now = Instant::now();
        let (at, base) = *self.anchor.get_or_insert((now, m));
        let ahead = m - base;
        let due = if ahead >= 0 {
            at + Duration::from_micros(ahead as u64)
        } else {
            at.checked_sub(Duration::from_micros(ahead.unsigned_abs())).unwrap_or(at)
        };
        let push = due.checked_sub(LEAD).unwrap_or(due);
        if now > push + CATCH_UP {
            self.anchor = Some((now, m));
            return now;
        }
        push
    }
}

/// xorshift64*: enough to shuffle a playlist, no dependency.
struct Rng(u64);

impl Rng {
    fn from_time() -> Self {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
        Self(nanos | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = (self.next() % (i as u64 + 1)) as usize;
            v.swap(i, j);
        }
    }
}

/// Why a file's tracks cannot join the channel's fixed track list.
fn compatible(fixed: &[TrackInfo], file: &[TrackInfo]) -> Result<(), String> {
    if fixed.len() != file.len() {
        return Err(format!("tracks differ: {} in the channel, {} in this file", fixed.len(), file.len()));
    }
    for (a, b) in fixed.iter().zip(file) {
        let kind = a.kind().as_str();
        if a.codec != b.codec {
            return Err(format!("{kind} codec differs: {} vs {}", a.codec.as_str(), b.codec.as_str()));
        }
        if a.timescale != b.timescale {
            return Err(format!("{kind} timescale differs: {} vs {}", a.timescale, b.timescale));
        }
        if let (Some(x), Some(y)) = (a.video, b.video) {
            if (x.width, x.height) != (y.width, y.height) {
                return Err(format!("resolution differs: {}x{} vs {}x{}", x.width, x.height, y.width, y.height));
            }
        }
        if let (Some(x), Some(y)) = (a.audio, b.audio) {
            if x != y {
                return Err(format!(
                    "audio differs: {} Hz/{} ch vs {} Hz/{} ch",
                    x.sample_rate, x.channels, y.sample_rate, y.channels
                ));
            }
        }
        if a.init != b.init {
            return Err(format!("{kind} codec configuration differs"));
        }
    }
    Ok(())
}

type Batch = (Box<dyn Source>, Vec<Frame>, Result<bool, String>);

/// Reads up to `n` frames. The result is `Ok(true)` at end of file.
fn read_batch(mut src: Box<dyn Source>, n: usize) -> Batch {
    let mut frames = Vec::with_capacity(n);
    while frames.len() < n {
        match src.next_frame() {
            Ok(Some(f)) => frames.push(f),
            Ok(None) => return (src, frames, Ok(true)),
            Err(e) => return (src, frames, Err(e)),
        }
    }
    (src, frames, Ok(false))
}

struct Player {
    name: String,
    buffer: BufferConfig,
    registry: Arc<Registry>,
    shared: Arc<Shared>,
    skip: watch::Receiver<u64>,
    on_air: Option<OnAir>,
    clock: Clock,
}

pub(crate) async fn run(ch: Channel, buffer: BufferConfig, registry: Arc<Registry>, shared: Arc<Shared>) {
    if !valid_stream_name(&ch.name) {
        tracing::error!(channel = %ch.name, "channel: invalid stream name; not starting");
        let mut s = shared.status.lock();
        s.state = ChannelState::Error;
        s.error = Some("invalid stream name".into());
        return;
    }
    let skip = shared.skip.subscribe();
    let mut p = Player { name: ch.name.clone(), buffer, registry, shared, skip, on_air: None, clock: Clock::default() };
    let mut rng = Rng::from_time();
    let mut ever_played = false;
    loop {
        let cfg_items = ch.items.clone();
        let mut items = tokio::task::spawn_blocking(move || source::expand(&cfg_items)).await.unwrap_or_default();
        if ch.shuffle {
            rng.shuffle(&mut items);
        }
        p.shared.status.lock().items = items.len();
        let (mut played, mut errors) = (false, false);
        for (i, path) in items.iter().enumerate() {
            p.shared.status.lock().index = i;
            let out = p.play(path).await;
            played |= out.played;
            if let Some(err) = out.error {
                errors = true;
                tracing::warn!(channel = %p.name, path = %path.display(), %err, "channel: item skipped");
                let mut s = p.shared.status.lock();
                s.error = Some(format!("{}: {err}", path.display()));
                s.state = ChannelState::Error;
                s.now_playing = None;
            }
            if out.blocked {
                break;
            }
        }
        ever_played |= played;
        if !errors {
            p.shared.status.lock().error = None;
        }
        if !ch.r#loop && ever_played {
            break;
        }
        if !played {
            // Nothing on air this pass: end the stream (a fresh one with
            // freshly fixed tracks starts when something plays again).
            p.on_air = None;
            p.clock = Clock::default();
            {
                let mut s = p.shared.status.lock();
                s.state = ChannelState::Idle;
                s.now_playing = None;
            }
            if items.is_empty() {
                tracing::info!(channel = %p.name, "channel: no items; retrying");
            }
            tokio::time::sleep(RETRY).await;
        }
    }
    p.on_air = None;
    let mut s = p.shared.status.lock();
    s.state = ChannelState::Idle;
    s.now_playing = None;
    tracing::info!(channel = %p.name, "channel: playlist finished");
}

impl Player {
    async fn play(&mut self, path: &Path) -> Outcome {
        let owned: PathBuf = path.to_owned();
        let opened = match tokio::task::spawn_blocking(move || source::open(&owned)).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Outcome::failed(e),
            Err(e) => return Outcome::failed(format!("reader failed: {e}")),
        };

        match &self.on_air {
            Some(air) => {
                if let Err(why) = compatible(&air.tracks, &opened.tracks) {
                    return Outcome::failed(format!("{why} (normalization is not done yet)"));
                }
            }
            None => {
                let publisher = match self.registry.publish(&self.name, self.buffer) {
                    Ok(p) => p,
                    Err(PublishError::Busy(_)) => {
                        return Outcome { blocked: true, ..Outcome::failed("stream name already published") };
                    }
                    Err(PublishError::InvalidName) => {
                        return Outcome { blocked: true, ..Outcome::failed("invalid stream name") };
                    }
                };
                if let Err(e) = publisher.set_tracks(opened.tracks.clone()) {
                    return Outcome::failed(format!("set tracks: {e}"));
                }
                tracing::info!(channel = %self.name, tracks = opened.tracks.len(), "channel: on air");
                self.on_air = Some(OnAir::new(publisher, opened.tracks.clone()));
            }
        }
        let Some(air) = self.on_air.as_mut() else { return Outcome::failed("not on air") };

        {
            let mut s = self.shared.status.lock();
            s.state = ChannelState::Playing;
            s.now_playing = Some(NowPlaying {
                path: path.display().to_string(),
                position_secs: 0.0,
                duration_secs: opened.duration_secs,
            });
        }
        tracing::info!(channel = %self.name, path = %path.display(), "channel: playing");

        let tracks = air.tracks.clone();
        let has_video = tracks.iter().any(|t| t.kind() == TrackKind::Video);
        let offsets = air.begin_file();
        // Skips requested before this item started do not apply to it.
        self.skip.borrow_and_update();

        let mut src = Some(opened.source);
        let mut queue: VecDeque<Frame> = VecDeque::new();
        let mut pending: VecDeque<Frame> = VecDeque::new();
        let mut t0: Option<i64> = None;
        let mut emit: Vec<Frame> = Vec::new();
        let mut pushed = 0u64;
        let mut error = None;
        let mut skipped = false;

        'file: loop {
            let Some(f) = queue.pop_front() else {
                let Some(s) = src.take() else { break };
                let read = tokio::task::spawn_blocking(move || read_batch(s, BATCH));
                let res = tokio::select! {
                    r = read => r,
                    _ = self.skip.changed() => {
                        tracing::info!(channel = %self.name, "channel: skipped");
                        skipped = true;
                        break 'file;
                    }
                };
                match res {
                    Ok((s, frames, end)) => {
                        queue.extend(frames);
                        match end {
                            Ok(false) => src = Some(s),
                            Ok(true) => {}
                            Err(e) => error = Some(e),
                        }
                    }
                    Err(e) => error = Some(format!("reader failed: {e}")),
                }
                if queue.is_empty() && src.is_none() {
                    break;
                }
                continue;
            };

            let Some(info) = tracks.get(f.track.0 as usize) else { continue };
            let is_video = info.kind() == TrackKind::Video;
            let m = info.to_micros(f.dts);
            match t0 {
                Some(t) => {
                    if is_video || m >= t {
                        emit.push(f);
                    }
                }
                None if !has_video => {
                    t0 = Some(m);
                    emit.push(f);
                }
                None if is_video && f.keyframe => {
                    t0 = Some(m);
                    emit.push(f);
                    emit.extend(pending.drain(..).filter(|a| tracks[a.track.0 as usize].to_micros(a.dts) >= m));
                }
                None if is_video => {} // before the first keyframe
                None => {
                    pending.push_back(f);
                    if pending.len() > MAX_PENDING {
                        pending.pop_front();
                    }
                }
            }
            let Some(t) = t0 else { continue };

            for f in emit.drain(..) {
                let file_m = tracks[f.track.0 as usize].to_micros(f.dts);
                let out = air.stamp(f, t, &offsets);
                let at = self.clock.push_at(tracks[out.track.0 as usize].to_micros(out.dts));
                tokio::select! {
                    _ = tokio::time::sleep_until(at) => {}
                    _ = self.skip.changed() => {
                        tracing::info!(channel = %self.name, "channel: skipped");
                        skipped = true;
                        break 'file;
                    }
                }
                if let Err(e) = air.publisher.push(out) {
                    tracing::debug!(channel = %self.name, %e, "channel: frame dropped");
                    continue;
                }
                pushed += 1;
                if let Some(np) = self.shared.status.lock().now_playing.as_mut() {
                    np.position_secs = (file_m - t).max(0) as f64 / 1e6;
                }
            }
        }

        if pushed == 0 && error.is_none() && !skipped {
            error = Some(if has_video && t0.is_none() { "no video keyframe" } else { "no frames" }.into());
        }
        Outcome { played: pushed > 0, error, blocked: false }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shuffle_is_a_permutation() {
        let mut rng = Rng(42);
        let mut v: Vec<u32> = (0..50).collect();
        rng.shuffle(&mut v);
        let mut sorted = v.clone();
        sorted.sort();
        assert_eq!(sorted, (0..50).collect::<Vec<_>>());
        assert_ne!(v, sorted);
    }
}
