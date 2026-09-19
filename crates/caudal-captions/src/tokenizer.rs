//! The part of Whisper's tokenizer a transcriber needs: token ids to text
//! and the ids of the special tokens. Read from the model's
//! `tokenizer.json` (Hugging Face format).
//!
//! Decoding only, so no BPE merges: Whisper's vocabulary is GPT-2
//! byte-level BPE, where every token is a string of "printable" stand-ins
//! for bytes; mapping them back gives UTF-8. This keeps the `tokenizers`
//! crate (and its C regex engine, Oniguruma) out of the binary.

use std::collections::HashMap;

use serde::Deserialize;

#[derive(Deserialize)]
struct TokenizerJson {
    model: Model,
    #[serde(default)]
    added_tokens: Vec<AddedToken>,
}

#[derive(Deserialize)]
struct Model {
    vocab: HashMap<String, u32>,
}

#[derive(Deserialize)]
struct AddedToken {
    id: u32,
    content: String,
}

pub struct Tokenizer {
    /// Token id → its text, already mapped back to bytes. `None` for special
    /// tokens (never shown).
    pieces: Vec<Option<Vec<u8>>>,
    specials: HashMap<String, u32>,
    /// Language code → token id (`<|es|>` → 50262), in the model's order.
    pub languages: Vec<(String, u32)>,
    pub sot: u32,
    pub eot: u32,
    pub transcribe: u32,
    pub no_timestamps: u32,
    /// `<|nospeech|>` (v3) or `<|nocaptions|>` (v1/v2).
    pub no_speech: Option<u32>,
    /// First timestamp token (`<|0.00|>`); everything from here up is one.
    pub timestamp_begin: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum TokenizerError {
    #[error("tokenizer.json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("tokenizer.json has no {0} token")]
    Missing(&'static str),
}

/// GPT-2's `bytes_to_unicode`: the printable character each byte is
/// written as inside the vocabulary.
fn byte_decoder() -> HashMap<char, u8> {
    let mut printable: Vec<u32> = (u32::from(b'!')..=u32::from(b'~')).collect();
    printable.extend(0xA1..=0xAC);
    printable.extend(0xAE..=0xFF);
    let mut map = HashMap::with_capacity(256);
    let mut n = 0;
    for b in 0..=255u32 {
        let c = if printable.contains(&b) {
            b
        } else {
            n += 1;
            255 + n
        };
        map.insert(char::from_u32(c).expect("valid char"), b as u8);
    }
    map
}

impl Tokenizer {
    pub fn from_json(json: &str) -> Result<Self, TokenizerError> {
        let t: TokenizerJson = serde_json::from_str(json)?;
        let decoder = byte_decoder();
        let size = t.model.vocab.values().chain(t.added_tokens.iter().map(|a| &a.id)).max().map_or(0, |m| m + 1);
        let mut pieces = vec![None; size as usize];
        for (tok, id) in &t.model.vocab {
            // A character outside the byte alphabet cannot be decoded; such
            // a token is dropped rather than shown as mojibake.
            let bytes: Option<Vec<u8>> = tok.chars().map(|c| decoder.get(&c).copied()).collect();
            pieces[*id as usize] = bytes;
        }
        let mut specials = HashMap::new();
        for a in &t.added_tokens {
            pieces[a.id as usize] = None;
            specials.insert(a.content.clone(), a.id);
        }
        let get = |s: &str, what: &'static str| specials.get(s).copied().ok_or(TokenizerError::Missing(what));
        let sot = get("<|startoftranscript|>", "start-of-transcript")?;
        let translate = get("<|translate|>", "translate")?;
        // Language tokens sit between <|startoftranscript|> and <|translate|>.
        let mut languages: Vec<(String, u32)> = specials
            .iter()
            .filter(|(_, id)| **id > sot && **id < translate)
            .filter_map(|(s, id)| Some((s.strip_prefix("<|")?.strip_suffix("|>")?.to_owned(), *id)))
            .collect();
        languages.sort_by_key(|(_, id)| *id);
        let no_timestamps = get("<|notimestamps|>", "no-timestamps")?;
        Ok(Self {
            pieces,
            languages,
            sot,
            eot: get("<|endoftext|>", "end-of-text")?,
            transcribe: get("<|transcribe|>", "transcribe")?,
            no_timestamps,
            no_speech: specials.get("<|nospeech|>").or_else(|| specials.get("<|nocaptions|>")).copied(),
            timestamp_begin: specials.get("<|0.00|>").copied().unwrap_or(no_timestamps + 1),
            specials,
        })
    }

    pub fn special(&self, s: &str) -> Option<u32> {
        self.specials.get(s).copied()
    }

    pub fn language_token(&self, code: &str) -> Option<u32> {
        self.languages.iter().find(|(c, _)| c == code).map(|(_, id)| *id)
    }

    pub fn language_code(&self, token: u32) -> Option<&str> {
        self.languages.iter().find(|(_, id)| *id == token).map(|(c, _)| c.as_str())
    }

    pub fn is_special(&self, id: u32) -> bool {
        self.pieces.get(id as usize).is_none_or(Option::is_none)
    }

    /// Text of `ids`, special tokens left out.
    pub fn decode(&self, ids: &[u32]) -> String {
        let bytes: Vec<u8> = ids
            .iter()
            .filter_map(|id| self.pieces.get(*id as usize).and_then(Option::as_ref))
            .flatten()
            .copied()
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny tokenizer.json with Whisper's layout.
    fn sample() -> Tokenizer {
        let json = r#"{
            "model": {"vocab": {"Hola": 0, "Ġmundo": 1, "Ã±": 2, "!": 3}},
            "added_tokens": [
                {"id": 10, "content": "<|endoftext|>"},
                {"id": 11, "content": "<|startoftranscript|>"},
                {"id": 12, "content": "<|en|>"},
                {"id": 13, "content": "<|es|>"},
                {"id": 14, "content": "<|translate|>"},
                {"id": 15, "content": "<|transcribe|>"},
                {"id": 16, "content": "<|nocaptions|>"},
                {"id": 17, "content": "<|notimestamps|>"},
                {"id": 18, "content": "<|0.00|>"}
            ]
        }"#;
        Tokenizer::from_json(json).unwrap()
    }

    #[test]
    fn decodes_byte_level_pieces_and_skips_specials() {
        let t = sample();
        // "Ġ" is the space byte; "Ã±" is the two UTF-8 bytes of "ñ".
        assert_eq!(t.decode(&[11, 13, 15, 17, 0, 1, 2, 3, 10]), "Hola mundoñ!");
        assert!(t.is_special(10) && !t.is_special(1));
    }

    #[test]
    fn finds_special_and_language_tokens() {
        let t = sample();
        assert_eq!((t.sot, t.eot, t.transcribe, t.no_timestamps, t.timestamp_begin), (11, 10, 15, 17, 18));
        assert_eq!(t.no_speech, Some(16));
        assert_eq!(t.languages, vec![("en".to_owned(), 12), ("es".to_owned(), 13)]);
        assert_eq!(t.language_token("es"), Some(13));
        assert_eq!(t.language_code(12), Some("en"));
    }

    #[test]
    fn byte_alphabet_covers_every_byte_once() {
        let d = byte_decoder();
        assert_eq!(d.len(), 256);
        let mut seen: Vec<u8> = d.values().copied().collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 256);
        assert_eq!(d[&'Ġ'], b' ');
    }
}
