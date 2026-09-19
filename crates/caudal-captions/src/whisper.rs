//! Whisper inference on `candle` (pure Rust; Metal on macOS, CPU
//! elsewhere). Greedy decoding without timestamps: each call turns one
//! short chunk of speech (a few seconds, cut at a pause) into text; the
//! caller times the text.
//!
//! Model files are what Hugging Face's `openai/whisper-*` repositories hold
//! (`config.json`, `tokenizer.json`, `model.safetensors`); see
//! [`crate::models`] for the pinned list and checksums.

use std::path::Path;
use std::sync::Arc;

use candle_core::{D, DType, Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use candle_nn::ops::softmax;
use candle_transformers::models::whisper::{Config, model::Whisper as Net};

use crate::mel::{HOP, Mel, N_FRAMES};
use crate::tokenizer::Tokenizer;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("model file {path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("model config: {0}")]
    Config(#[from] serde_json::Error),
    #[error(transparent)]
    Tokenizer(#[from] crate::tokenizer::TokenizerError),
    #[error("inference: {0}")]
    Candle(#[from] candle_core::Error),
    #[error("the model does not know language `{0}`")]
    Language(String),
}

/// Where the model runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeviceChoice {
    /// Metal on macOS when a GPU is there, else CPU.
    #[default]
    Auto,
    Cpu,
    Metal,
}

/// What language to transcribe in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Language {
    /// ISO 639-1 code the model knows (`es`, `en`, ...).
    Fixed(String),
    /// Detect per chunk.
    Auto,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    pub text: String,
    pub language: String,
    /// Probability the model gives the chunk of being silence / not speech.
    pub no_speech_prob: f32,
    /// Mean log-probability of the chosen tokens.
    pub avg_logprob: f32,
}

impl Transcript {
    /// Whisper's own rule for "this was not speech" (openai/whisper
    /// `transcribe.py`): high no-speech probability and low confidence, or
    /// nothing at all.
    pub fn is_silence(&self) -> bool {
        self.text.trim().is_empty() || (self.no_speech_prob > 0.6 && self.avg_logprob < -1.0)
    }
}

/// A loaded model. Cloning is cheap (the weights are shared); each clone
/// has its own decoding state, so one clone per worker thread.
#[derive(Clone)]
pub struct Model {
    net: Net,
    tok: Arc<Tokenizer>,
    mel: Arc<Mel>,
    device: Device,
    suppress: Tensor,
    /// Also suppressed on the first sampled token (whisper's `suppress_blank`).
    suppress_first: Tensor,
    /// Encoder input: the full 30 s window (the model's training shape), or
    /// only as many mel frames as the chunk needs (faster, less accurate).
    pub full_window: bool,
}

fn read(dir: &Path, file: &str) -> Result<Vec<u8>, EngineError> {
    let path = dir.join(file);
    std::fs::read(&path).map_err(|source| EngineError::Io { path: path.display().to_string(), source })
}

impl Model {
    /// Loads `config.json`, `tokenizer.json` and `model.safetensors` from
    /// `dir`. The weights are read into memory (no mmap: no `unsafe`).
    pub fn load(dir: &Path, device: DeviceChoice) -> Result<Self, EngineError> {
        let device = match device {
            DeviceChoice::Cpu => Device::Cpu,
            DeviceChoice::Metal => Device::new_metal(0)?,
            DeviceChoice::Auto => metal_or_cpu(),
        };
        let config: Config = serde_json::from_slice(&read(dir, "config.json")?)?;
        let tok = Tokenizer::from_json(&String::from_utf8_lossy(&read(dir, "tokenizer.json")?))?;
        let weights = read(dir, "model.safetensors")?;
        let vb = VarBuilder::from_buffered_safetensors(weights, DType::F32, &device)?;
        let net = Net::load(&vb, config.clone())?;
        drop(vb);

        let vocab = config.vocab_size;
        let mut mask = vec![0f32; vocab];
        let mut off = |id: u32| {
            if let Some(m) = mask.get_mut(id as usize) {
                *m = f32::NEG_INFINITY;
            }
        };
        for &id in &config.suppress_tokens {
            off(id);
        }
        // Only text and end-of-text may be sampled.
        for id in (tok.timestamp_begin as usize)..vocab {
            off(id as u32);
        }
        for id in [tok.sot, tok.transcribe, tok.no_timestamps] {
            off(id);
        }
        for (_, id) in &tok.languages {
            off(*id);
        }
        for s in ["<|translate|>", "<|startoflm|>", "<|startofprev|>", "<|nocaptions|>", "<|nospeech|>"] {
            if let Some(id) = tok.special(s) {
                off(id);
            }
        }
        let mut first = mask.clone();
        first[tok.eot as usize] = f32::NEG_INFINITY;
        // " " alone (GPT-2 id 220) as the first token.
        if let Some(m) = first.get_mut(220) {
            *m = f32::NEG_INFINITY;
        }
        Ok(Self {
            suppress: Tensor::new(mask.as_slice(), &device)?,
            suppress_first: Tensor::new(first.as_slice(), &device)?,
            mel: Arc::new(Mel::new(config.num_mel_bins)),
            net,
            tok: Arc::new(tok),
            device,
            full_window: true,
        })
    }

    pub fn device_name(&self) -> &'static str {
        if self.device.is_metal() { "metal" } else { "cpu" }
    }

    pub fn knows_language(&self, code: &str) -> bool {
        self.tok.language_token(code).is_some()
    }

    /// Transcribes one chunk of 16 kHz mono speech (at most 30 s).
    pub fn transcribe(&mut self, pcm: &[f32], language: &Language) -> Result<Transcript, EngineError> {
        let n_mels = self.mel.n_mels();
        let mut mel = self.mel.spectrogram(pcm);
        let frames = if self.full_window {
            N_FRAMES
        } else {
            // One second of margin; the encoder's stride-2 conv wants an
            // even count.
            (pcm.len() / HOP + 100).min(N_FRAMES).next_multiple_of(2).min(N_FRAMES)
        };
        if frames < N_FRAMES {
            mel = (0..n_mels).flat_map(|m| mel[m * N_FRAMES..m * N_FRAMES + frames].to_vec()).collect();
        }
        let mel = Tensor::from_vec(mel, (1, n_mels, frames), &self.device)?;
        let features = self.net.encoder.forward(&mel, true)?;

        let lang_token = match language {
            Language::Fixed(code) => {
                self.tok.language_token(code).ok_or_else(|| EngineError::Language(code.clone()))?
            }
            Language::Auto => self.detect_language(&features)?,
        };
        let mut tokens = vec![self.tok.sot, lang_token, self.tok.transcribe, self.tok.no_timestamps];
        let prompt = tokens.len();
        let seconds = pcm.len() as f32 / 16_000.0;
        let max_new = ((20.0 + 12.0 * seconds) as usize).min(224);
        let mut no_speech_prob = 0.0;
        let mut logprob_sum = 0.0;
        for i in 0..max_new {
            let input = Tensor::new(tokens.as_slice(), &self.device)?.unsqueeze(0)?;
            let ys = self.net.decoder.forward(&input, &features, i == 0)?;
            if i == 0
                && let Some(ns) = self.tok.no_speech
            {
                let logits = self.net.decoder.final_linear(&ys.i((..1, ..1))?)?.i(0)?.i(0)?;
                no_speech_prob = softmax(&logits, D::Minus1)?.i(ns as usize)?.to_scalar::<f32>()?;
            }
            let seq = ys.dim(1)?;
            let logits = self.net.decoder.final_linear(&ys.i((..1, seq - 1..))?)?.i(0)?.i(0)?;
            let mask = if i == 0 { &self.suppress_first } else { &self.suppress };
            let logits = logits.broadcast_add(mask)?;
            let next = logits.argmax(D::Minus1)?.to_scalar::<u32>()?;
            let probs = softmax(&logits, D::Minus1)?;
            logprob_sum += probs.i(next as usize)?.to_scalar::<f32>()?.max(1e-10).ln();
            if next == self.tok.eot {
                break;
            }
            tokens.push(next);
            if repeating(&tokens[prompt..]) {
                break;
            }
        }
        let generated = &tokens[prompt..];
        let text = self.tok.decode(generated).trim().to_owned();
        Ok(Transcript {
            text,
            language: self.tok.language_code(lang_token).unwrap_or("").to_owned(),
            no_speech_prob,
            avg_logprob: logprob_sum / (generated.len() + 1) as f32,
        })
    }

    /// The language token the model finds most likely after
    /// `<|startoftranscript|>`.
    fn detect_language(&mut self, features: &Tensor) -> Result<u32, EngineError> {
        let input = Tensor::new(&[self.tok.sot], &self.device)?.unsqueeze(0)?;
        let ys = self.net.decoder.forward(&input, features, true)?;
        let logits = self.net.decoder.final_linear(&ys.i((..1, ..1))?)?.i(0)?.i(0)?;
        let ids: Vec<u32> = self.tok.languages.iter().map(|(_, id)| *id).collect();
        let index = Tensor::new(ids.as_slice(), &self.device)?;
        let best = logits.index_select(&index, 0)?.argmax(D::Minus1)?.to_scalar::<u32>()?;
        Ok(ids[best as usize])
    }
}

fn metal_or_cpu() -> Device {
    #[cfg(target_os = "macos")]
    if let Ok(d) = Device::new_metal(0) {
        return d;
    }
    Device::Cpu
}

/// Greedy decoding can loop ("la la la la ..."). True when the tail is the
/// same 1-4 token pattern four times over.
fn repeating(t: &[u32]) -> bool {
    (1..=4).any(|n| {
        t.len() >= 4 * n && {
            let tail = &t[t.len() - n..];
            (1..4).all(|k| &t[t.len() - (k + 1) * n..t.len() - k * n] == tail)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::repeating;

    #[test]
    fn loops_are_caught_but_speech_is_not() {
        assert!(repeating(&[5, 1, 1, 1, 1]));
        assert!(repeating(&[7, 1, 2, 1, 2, 1, 2, 1, 2]));
        assert!(!repeating(&[1, 2, 3, 1, 2, 3, 1, 2, 3]));
        assert!(!repeating(&[1, 2, 3, 4, 5]));
    }
}
