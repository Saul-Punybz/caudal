//! A stream's audio track to 16 kHz mono PCM, in process and in pure Rust:
//! AAC-LC with `rusty_aac`, Opus with `opus-decoder` (which decodes
//! straight to 16 kHz mono, so it needs no resampler).

use caudal_core::{Codec, TrackInfo};

use crate::mel::SAMPLE_RATE;

pub enum AudioDecoder {
    Aac { dec: Box<rusty_aac::AacDecoder>, resampler: Option<Resampler> },
    Opus { dec: Box<opus_decoder::OpusDecoder>, buf: Vec<f32> },
}

impl AudioDecoder {
    /// A decoder for `track`, or `None` if it is not AAC/Opus or its
    /// configuration does not parse.
    pub fn new(track: &TrackInfo) -> Option<Self> {
        match track.codec {
            Codec::Aac => {
                let dec = rusty_aac::AacDecoder::with_config_bytes(&track.init).ok()?;
                Some(Self::Aac { dec: Box::new(dec), resampler: None })
            }
            Codec::Opus => {
                let dec = opus_decoder::OpusDecoder::new(SAMPLE_RATE as u32, 1).ok()?;
                let n = dec.max_frame_size_per_channel();
                Some(Self::Opus { dec: Box::new(dec), buf: vec![0.0; n] })
            }
            _ => None,
        }
    }

    /// One access unit to 16 kHz mono samples (possibly none yet: the
    /// resampler holds a few samples back).
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<f32>, String> {
        match self {
            Self::Aac { dec, resampler } => {
                let t_aac = std::time::Instant::now();
                let out = match dec.decode(packet, None) {
                    Ok(out) => out,
                    Err(rusty_aac::Error::Again) => return Ok(Vec::new()),
                    Err(e) => return Err(format!("aac: {e}")),
                };
                let mono = downmix(&out.samples, usize::from(out.channels));
                let r = resampler.get_or_insert_with(|| Resampler::new(out.sample_rate, SAMPLE_RATE as u32));
                if r.from != out.sample_rate {
                    *r = Resampler::new(out.sample_rate, SAMPLE_RATE as u32);
                }
                let t_rs = std::time::Instant::now();
                let res = r.process(&mono);
                probe!("audio: aac decode {:?}, resample {:?}", t_rs - t_aac, t_rs.elapsed());
                Ok(res)
            }
            Self::Opus { dec, buf } => {
                let n = dec.decode_float(packet, buf, false).map_err(|e| format!("opus: {e:?}"))?;
                Ok(buf[..n].to_vec())
            }
        }
    }
}

fn downmix(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved.chunks_exact(channels).map(|f| f.iter().sum::<f32>() / channels as f32).collect()
}

/// Streaming band-limited resampler (windowed sinc, Blackman window). Good
/// enough for speech recognition; not meant for listening.
pub struct Resampler {
    from: u32,
    to: u32,
    /// Input samples not yet fully used; `input[0]` is input sample `base`.
    input: Vec<f32>,
    base: u64,
    /// Next output sample index.
    next: u64,
    /// Half the filter length, in input samples.
    half: usize,
    /// The kernel sampled every 1/[`PHASES`] input sample, from 0 to `half`.
    table: Vec<f32>,
}

const PHASES: usize = 256;

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        let ratio = f64::from(to) / f64::from(from);
        // Low-pass a little under the output Nyquist when downsampling.
        let cutoff = ratio.min(1.0) * 0.92;
        let half = (16.0 / cutoff).ceil() as usize;
        let h = half as f64;
        let table = (0..=half * PHASES + 1)
            .map(|k| {
                let x = k as f64 / PHASES as f64;
                if x >= h {
                    return 0.0;
                }
                let a = std::f64::consts::PI * x * cutoff;
                let sinc = if k == 0 { 1.0 } else { a.sin() / a };
                let w = 0.42
                    + 0.5 * (std::f64::consts::PI * x / h).cos()
                    + 0.08 * (2.0 * std::f64::consts::PI * x / h).cos();
                (cutoff * sinc * w) as f32
            })
            .collect();
        Self { from: from.max(1), to: to.max(1), input: Vec::new(), base: 0, next: 0, half, table }
    }

    /// The kernel at `x` input samples from the centre, interpolated.
    fn kernel(&self, x: f64) -> f32 {
        let p = x.abs() * PHASES as f64;
        let k = p as usize;
        if k + 1 >= self.table.len() {
            return 0.0;
        }
        let frac = (p - k as f64) as f32;
        self.table[k] + (self.table[k + 1] - self.table[k]) * frac
    }

    pub fn process(&mut self, pcm: &[f32]) -> Vec<f32> {
        if self.from == self.to {
            return pcm.to_vec();
        }
        self.input.extend_from_slice(pcm);
        let end = self.base + self.input.len() as u64;
        let mut out = Vec::with_capacity(pcm.len() * self.to as usize / self.from as usize + 1);
        loop {
            // Output sample `next` sits at input position `centre`.
            let centre = self.next as f64 * f64::from(self.from) / f64::from(self.to);
            let last_needed = centre.floor() as u64 + self.half as u64;
            if last_needed >= end {
                break;
            }
            let first = (centre.floor() as i64 - self.half as i64 + 1).max(0) as u64;
            let mut acc = 0.0f32;
            for i in first.max(self.base)..=last_needed {
                acc += self.input[(i - self.base) as usize] * self.kernel(centre - i as f64);
            }
            out.push(acc);
            self.next += 1;
        }
        // Keep what the next output still needs.
        let centre = self.next as f64 * f64::from(self.from) / f64::from(self.to);
        let keep_from = (centre.floor() as i64 - self.half as i64 + 1).max(0) as u64;
        if keep_from > self.base {
            let drop = ((keep_from - self.base) as usize).min(self.input.len());
            self.input.drain(..drop);
            self.base += drop as u64;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, hz: f32, secs: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize)
            .map(|i| (i as f32 * 2.0 * std::f32::consts::PI * hz / rate as f32).sin() * 0.5)
            .collect()
    }

    /// Amplitude of `hz` in `pcm` (a single DFT bin).
    fn level(pcm: &[f32], rate: u32, hz: f32) -> f32 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, s) in pcm.iter().enumerate() {
            let a = 2.0 * std::f64::consts::PI * f64::from(hz) * i as f64 / f64::from(rate);
            re += f64::from(*s) * a.cos();
            im += f64::from(*s) * a.sin();
        }
        (2.0 * (re * re + im * im).sqrt() / pcm.len() as f64) as f32
    }

    #[test]
    fn speech_band_passes_and_aliases_are_removed() {
        for from in [48_000, 44_100, 32_000, 22_050] {
            // Fed in uneven pieces, as decoded frames arrive.
            let input = sine(from, 1000.0, 2.0);
            let mut r = Resampler::new(from, 16_000);
            let mut out = Vec::new();
            for piece in input.chunks(1024 + 17) {
                out.extend(r.process(piece));
            }
            let expect = (input.len() as u64 * 16_000 / u64::from(from)) as usize;
            assert!(out.len() + 64 >= expect && out.len() <= expect, "{from}: {} vs {expect}", out.len());
            let body = &out[1000..out.len() - 1000];
            let l = level(body, 16_000, 1000.0);
            assert!((l - 0.5).abs() < 0.02, "{from}: 1 kHz level {l}");
            // A 12 kHz tone is above the new Nyquist: it must not fold back
            // to 4 kHz.
            if from > 24_000 {
                let hi = sine(from, 12_000.0, 1.0);
                let mut r = Resampler::new(from, 16_000);
                let out = r.process(&hi);
                let alias = level(&out[500..out.len() - 500], 16_000, 4000.0);
                assert!(alias < 0.01, "{from}: alias {alias}");
            }
        }
    }

    #[test]
    fn same_rate_is_a_copy() {
        let mut r = Resampler::new(16_000, 16_000);
        assert_eq!(r.process(&[0.1, 0.2]), vec![0.1, 0.2]);
    }

    #[test]
    fn stereo_downmix_averages() {
        assert_eq!(downmix(&[1.0, 0.0, 0.5, 0.5], 2), vec![0.5, 0.5]);
    }
}
