//! Whisper's log-mel spectrogram, computed the way `openai/whisper`'s
//! `audio.py` does: a centered (reflect-padded) STFT with a periodic Hann
//! window, power spectrum, Slaney-normalised mel filters (librosa's
//! defaults), `log10`, clamped to 8 below the maximum, scaled to about ±1.
//!
//! Single-threaded on purpose: it runs inside the transcription pool, whose
//! thread cap is the resource guard, and costs a few milliseconds per chunk.

use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

pub const SAMPLE_RATE: usize = 16_000;
pub const N_FFT: usize = 400;
pub const HOP: usize = 160;
/// Whisper always sees 30 s windows: 3000 mel frames.
pub const N_SAMPLES: usize = 30 * SAMPLE_RATE;
pub const N_FRAMES: usize = N_SAMPLES / HOP;

const N_BINS: usize = N_FFT / 2 + 1;

/// Slaney mel scale (librosa `htk=False`): linear below 1 kHz, log above.
fn hz_to_mel(f: f64) -> f64 {
    let (f_sp, min_log_hz) = (200.0 / 3.0, 1000.0);
    let min_log_mel = min_log_hz / f_sp;
    let logstep = 6.4f64.ln() / 27.0;
    if f >= min_log_hz { min_log_mel + (f / min_log_hz).ln() / logstep } else { f / f_sp }
}

fn mel_to_hz(m: f64) -> f64 {
    let (f_sp, min_log_hz) = (200.0 / 3.0, 1000.0);
    let min_log_mel = min_log_hz / f_sp;
    let logstep = 6.4f64.ln() / 27.0;
    if m >= min_log_mel { min_log_hz * (logstep * (m - min_log_mel)).exp() } else { f_sp * m }
}

/// `librosa.filters.mel(sr=16000, n_fft=400, n_mels)`: `n_mels` rows of
/// [`N_BINS`] weights each, row-major.
pub fn mel_filters(n_mels: usize) -> Vec<f32> {
    let fft_freqs: Vec<f64> = (0..N_BINS).map(|i| i as f64 * SAMPLE_RATE as f64 / N_FFT as f64).collect();
    let (lo, hi) = (hz_to_mel(0.0), hz_to_mel(SAMPLE_RATE as f64 / 2.0));
    let mel_f: Vec<f64> = (0..n_mels + 2).map(|i| mel_to_hz(lo + (hi - lo) * i as f64 / (n_mels + 1) as f64)).collect();
    let mut w = vec![0f32; n_mels * N_BINS];
    for m in 0..n_mels {
        let (l, c, r) = (mel_f[m], mel_f[m + 1], mel_f[m + 2]);
        let enorm = 2.0 / (r - l);
        for (k, &f) in fft_freqs.iter().enumerate() {
            let lower = (f - l) / (c - l);
            let upper = (r - f) / (r - c);
            w[m * N_BINS + k] = (lower.min(upper).max(0.0) * enorm) as f32;
        }
    }
    w
}

/// Log-mel front end for one model size (80 or 128 mel bins).
pub struct Mel {
    n_mels: usize,
    filters: Vec<f32>,
    window: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
}

impl Mel {
    pub fn new(n_mels: usize) -> Self {
        let window =
            (0..N_FFT).map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / N_FFT as f32).cos()).collect();
        Self { n_mels, filters: mel_filters(n_mels), window, fft: FftPlanner::new().plan_fft_forward(N_FFT) }
    }

    pub fn n_mels(&self) -> usize {
        self.n_mels
    }

    /// Mel spectrogram of `pcm` (16 kHz mono) padded or trimmed to 30 s:
    /// `n_mels` rows of [`N_FRAMES`] values, row-major, ready for the
    /// encoder. Frames that only see the zero padding are not transformed.
    pub fn spectrogram(&self, pcm: &[f32]) -> Vec<f32> {
        let pcm = &pcm[..pcm.len().min(N_SAMPLES)];
        let at = |i: isize| -> f32 {
            // Reflect padding by N_FFT/2 on both sides of the 30 s window
            // (torch.stft(center=True, pad_mode="reflect")).
            let n = N_SAMPLES as isize;
            let j = if i < 0 {
                -i
            } else if i >= n {
                2 * (n - 1) - i
            } else {
                i
            };
            pcm.get(j as usize).copied().unwrap_or(0.0)
        };
        // Frames whose window lies wholly past the audio see only zeros
        // (and the reflection of zeros): their power is 0.
        let live = (pcm.len() + N_FFT / 2).div_ceil(HOP).min(N_FRAMES);
        let mut out = vec![1e-10f32.log10(); self.n_mels * N_FRAMES];
        let mut buf = vec![Complex32::new(0.0, 0.0); N_FFT];
        let mut power = [0f32; N_BINS];
        for t in 0..live {
            let start = (t * HOP) as isize - (N_FFT / 2) as isize;
            for (k, b) in buf.iter_mut().enumerate() {
                *b = Complex32::new(at(start + k as isize) * self.window[k], 0.0);
            }
            self.fft.process(&mut buf);
            for (p, b) in power.iter_mut().zip(&buf) {
                *p = b.norm_sqr();
            }
            for m in 0..self.n_mels {
                let row = &self.filters[m * N_BINS..(m + 1) * N_BINS];
                let e: f32 = row.iter().zip(&power).map(|(w, p)| w * p).sum();
                out[m * N_FRAMES + t] = e.max(1e-10).log10();
            }
        }
        let max = out.iter().copied().fold(f32::MIN, f32::max);
        for v in &mut out {
            *v = (v.max(max - 8.0) + 4.0) / 4.0;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slaney_scale_round_trips() {
        for f in [0.0, 440.0, 999.0, 1000.0, 4000.0, 8000.0] {
            assert!((mel_to_hz(hz_to_mel(f)) - f).abs() < 1e-6, "{f}");
        }
        assert!((hz_to_mel(1000.0) - 15.0).abs() < 1e-9);
    }

    #[test]
    fn filters_match_librosa_shape_and_known_values() {
        let w = mel_filters(80);
        assert_eq!(w.len(), 80 * N_BINS);
        // Every filter has some weight; the DC bin feeds no filter but the
        // first; librosa's first filter gives bin 1 (40 Hz) 0.0249 (2/74.5 Hz x 0.926).
        for m in 0..80 {
            assert!(w[m * N_BINS..(m + 1) * N_BINS].iter().any(|&v| v > 0.0), "filter {m} empty");
        }
        assert!((w[1] - 0.024_9).abs() < 3e-4, "{}", w[1]);
    }

    #[test]
    fn a_tone_lights_the_right_band_and_padding_is_quiet() {
        let mel = Mel::new(80);
        let pcm: Vec<f32> =
            (0..SAMPLE_RATE).map(|i| (i as f32 * 2.0 * std::f32::consts::PI * 1000.0 / 16_000.0).sin() * 0.5).collect();
        let s = mel.spectrogram(&pcm);
        assert_eq!(s.len(), 80 * N_FRAMES);
        // Frame 50 (0.5 s): the loudest band is the one around 1 kHz.
        let loudest = (0..80).max_by(|&a, &b| s[a * N_FRAMES + 50].total_cmp(&s[b * N_FRAMES + 50])).unwrap();
        let centre = mel_to_hz(hz_to_mel(8000.0) * (loudest + 1) as f64 / 81.0);
        assert!((800.0..1250.0).contains(&centre), "loudest band centre {centre} Hz");
        // Padding (after 1 s) sits at the floor: max - 8, scaled.
        let max = s.iter().copied().fold(f32::MIN, f32::max);
        let floor = (max * 4.0 - 4.0 - 8.0 + 4.0) / 4.0;
        assert!((s[40 * N_FRAMES + 2000] - floor).abs() < 1e-5);
    }
}
