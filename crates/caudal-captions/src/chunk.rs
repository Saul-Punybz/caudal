//! Cuts a live 16 kHz mono signal into chunks for the recogniser: at a
//! pause once a chunk has at least `min` of audio, or at the quietest spot
//! of its last second when it reaches `max`. Chunks that never rise above
//! the silence threshold are dropped before they cost any inference (and
//! before Whisper can "hear" words in them).

use crate::mel::SAMPLE_RATE;

/// 20 ms analysis frames.
const FRAME: usize = SAMPLE_RATE / 50;

#[derive(Debug, Clone, Copy)]
pub struct ChunkConfig {
    pub min_ms: u32,
    pub max_ms: u32,
    /// A pause this long ends a chunk (once it is at least `min_ms`).
    pub pause_ms: u32,
    /// RMS below this is silence (full scale = 1.0).
    pub silence_rms: f32,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        Self { min_ms: 1500, max_ms: 5000, pause_ms: 300, silence_rms: 0.01 }
    }
}

/// A span of audio ready for the recogniser. `start` counts samples since
/// the chunker began (the caller maps it to media time).
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub start: u64,
    pub pcm: Vec<f32>,
}

impl Chunk {
    pub fn duration_us(&self) -> i64 {
        self.pcm.len() as i64 * 1_000_000 / SAMPLE_RATE as i64
    }
}

pub struct Chunker {
    cfg: ChunkConfig,
    buf: Vec<f32>,
    /// Sample index of `buf[0]`.
    start: u64,
}

fn rms(s: &[f32]) -> f32 {
    if s.is_empty() {
        return 0.0;
    }
    (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt()
}

fn samples(ms: u32) -> usize {
    ms as usize * SAMPLE_RATE / 1000
}

impl Chunker {
    pub fn new(cfg: ChunkConfig) -> Self {
        Self { cfg, buf: Vec::new(), start: 0 }
    }

    /// Samples taken in so far.
    pub fn position(&self) -> u64 {
        self.start + self.buf.len() as u64
    }

    /// Adds audio; returns the chunks it completes, oldest first.
    pub fn push(&mut self, pcm: &[f32]) -> Vec<Chunk> {
        self.buf.extend_from_slice(pcm);
        let mut out = Vec::new();
        while let Some(cut) = self.find_cut() {
            let pcm: Vec<f32> = self.buf.drain(..cut).collect();
            let start = self.start;
            self.start += cut as u64;
            if pcm.chunks(FRAME).any(|f| rms(f) >= self.cfg.silence_rms) {
                out.push(Chunk { start, pcm });
            }
        }
        // Leading silence never needs to wait for a chunk to fill.
        let lead = self.buf.chunks(FRAME).take_while(|f| f.len() == FRAME && rms(f) < self.cfg.silence_rms).count();
        let keep = samples(self.cfg.pause_ms);
        if lead * FRAME > keep {
            let drop = lead * FRAME - keep;
            self.buf.drain(..drop);
            self.start += drop as u64;
        }
        out
    }

    /// Whatever is buffered, as one last chunk (end of stream).
    pub fn flush(&mut self) -> Option<Chunk> {
        let pcm = std::mem::take(&mut self.buf);
        let start = self.start;
        self.start += pcm.len() as u64;
        pcm.chunks(FRAME).any(|f| rms(f) >= self.cfg.silence_rms).then_some(Chunk { start, pcm })
    }

    /// Forgets buffered audio (a gap in the input) and restarts counting at
    /// `position`.
    pub fn reset(&mut self, position: u64) {
        self.buf.clear();
        self.start = position;
    }

    fn find_cut(&self) -> Option<usize> {
        let (min, max, pause) = (samples(self.cfg.min_ms), samples(self.cfg.max_ms), samples(self.cfg.pause_ms));
        let frames: Vec<f32> = self.buf.as_chunks::<FRAME>().0.iter().map(|f| rms(f)).collect();
        let quiet = |f: &f32| *f < self.cfg.silence_rms;
        // A pause after `min`: cut in its middle.
        let need = pause.div_ceil(FRAME);
        let mut run = 0;
        for (i, f) in frames.iter().enumerate() {
            run = if quiet(f) { run + 1 } else { 0 };
            let end = (i + 1) * FRAME;
            if run >= need && end - run * FRAME / 2 >= min {
                return Some(end - run * FRAME / 2);
            }
            if end >= max {
                break;
            }
        }
        if self.buf.len() < max {
            return None;
        }
        // Full: cut in the middle of the quietest 100 ms in the last second
        // before `max` (a single quiet 20 ms frame is often a stop
        // consonant inside a word).
        const SPAN: usize = 5;
        let last = max / FRAME;
        let from = last.saturating_sub(SAMPLE_RATE / FRAME).max(min / FRAME);
        let energy = |a: usize| frames[a..a + SPAN].iter().map(|f| f * f).sum::<f32>();
        let best = (from..last.saturating_sub(SPAN))
            .min_by(|&a, &b| energy(a).total_cmp(&energy(b)))
            .map_or(last, |a| a + SPAN / 2 + 1);
        Some((best * FRAME).min(max))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(ms: u32) -> Vec<f32> {
        (0..samples(ms)).map(|i| (i as f32 * 0.2).sin() * 0.3).collect()
    }

    fn quiet(ms: u32) -> Vec<f32> {
        vec![0.0; samples(ms)]
    }

    #[test]
    fn cuts_at_a_pause_after_the_minimum() {
        let mut c = Chunker::new(ChunkConfig::default());
        let mut a = tone(2000);
        a.extend(quiet(400));
        a.extend(tone(500));
        let out = c.push(&a);
        assert_eq!(out.len(), 1);
        // Cut as soon as the pause reaches 300 ms, in its middle: 2 s + 150 ms.
        assert_eq!(out[0].start, 0);
        assert_eq!(out[0].pcm.len(), samples(2150));
        assert_eq!(c.position(), samples(2900) as u64);
    }

    #[test]
    fn a_pause_before_the_minimum_does_not_cut() {
        let mut c = Chunker::new(ChunkConfig::default());
        let mut a = tone(800);
        a.extend(quiet(400));
        a.extend(tone(800));
        assert!(c.push(&a).is_empty());
    }

    #[test]
    fn nonstop_speech_is_cut_at_the_maximum() {
        let mut c = Chunker::new(ChunkConfig::default());
        let out = c.push(&tone(11_000));
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|ch| ch.pcm.len() <= samples(5000) && ch.pcm.len() >= samples(4000)));
        assert_eq!(out[1].start, out[0].pcm.len() as u64);
    }

    #[test]
    fn silence_costs_nothing_and_keeps_the_clock() {
        let mut c = Chunker::new(ChunkConfig::default());
        assert!(c.push(&quiet(10_000)).is_empty());
        // Only the last pause_ms of silence stays buffered.
        assert!(c.buf.len() <= samples(300) + FRAME);
        let mut a = tone(2000);
        a.extend(quiet(400));
        let out = c.push(&a);
        assert_eq!(out.len(), 1);
        // The chunk starts shortly before the tone began at 10 s.
        let start_ms = out[0].start * 1000 / SAMPLE_RATE as u64;
        assert!((9_600..=10_000).contains(&start_ms), "{start_ms}");
        assert!(c.flush().is_none());
    }
}
