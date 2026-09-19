//! The Whisper models Caudal knows how to fetch: Hugging Face repository,
//! pinned revision, and the size and SHA-256 of every file, so a download
//! is verified and a model directory can be checked before it is loaded.
//!
//! Weights: OpenAI's Whisper, released under the MIT License
//! (github.com/openai/whisper); the Hugging Face `openai/whisper-*`
//! repositories that host the converted files are marked Apache-2.0. Both
//! are permissive. Never bundled in the binary: the operator downloads them
//! (`caudal captions fetch-model <name>`).

use std::path::{Path, PathBuf};

pub struct ModelFile {
    pub name: &'static str,
    pub size: u64,
    pub sha256: &'static str,
}

pub struct KnownModel {
    pub name: &'static str,
    pub repo: &'static str,
    pub revision: &'static str,
    pub files: [ModelFile; 3],
}

impl KnownModel {
    pub fn url(&self, file: &str) -> String {
        format!("https://huggingface.co/{}/resolve/{}/{file}", self.repo, self.revision)
    }

    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }
}

const TOKENIZER: ModelFile = ModelFile {
    name: "tokenizer.json",
    size: 2_480_466,
    sha256: "27fc476bfe7f17299480be2273fc0608e4d5a99aba2ab5dec5374b4482d1a566",
};

/// Checked 19 Sep 2026 against the Hugging Face API (LFS oids are the
/// SHA-256 of the safetensors files) and by downloading each file.
pub const KNOWN: [KnownModel; 3] = [
    KnownModel {
        name: "tiny",
        repo: "openai/whisper-tiny",
        revision: "169d4a4341b33bc18d8881c4b69c2e104e1cc0af",
        files: [
            ModelFile {
                name: "config.json",
                size: 1_983,
                sha256: "ffdccec4f3211f4c63310f2b7098f309fe70f3952cedc5e4d11e43f5b2379b98",
            },
            TOKENIZER,
            ModelFile {
                name: "model.safetensors",
                size: 151_061_672,
                sha256: "7ebd0e69e78190ffe1438491fa05cc1f5c1aa3a4c4db3bc1723adbb551ea2395",
            },
        ],
    },
    KnownModel {
        name: "base",
        repo: "openai/whisper-base",
        revision: "e37978b90ca9030d5170a5c07aadb050351a65bb",
        files: [
            ModelFile {
                name: "config.json",
                size: 1_983,
                sha256: "a153c53883a6799b6f056b4a8d1a515c9926d03994682ba88a7616618d7da0c1",
            },
            TOKENIZER,
            ModelFile {
                name: "model.safetensors",
                size: 290_403_936,
                sha256: "07cadb9f25677c8d50df603e66a98fbd842cce45047139baeb16e6219a1e807b",
            },
        ],
    },
    KnownModel {
        name: "small",
        repo: "openai/whisper-small",
        revision: "973afd24965f72e36ca33b3055d56a652f456b4d",
        files: [
            ModelFile {
                name: "config.json",
                size: 1_967,
                sha256: "e6a2b489da1b5aed65a8eb8d1e7466fa867ad5643a8bc138ba708bd56b2875c4",
            },
            TOKENIZER,
            ModelFile {
                name: "model.safetensors",
                size: 966_995_080,
                sha256: "1d7734884874f1a1513ed9aa760a4f8e97aaa02fd6d93a3a85d27b2ae9ca596b",
            },
        ],
    },
];

pub fn known(name: &str) -> Option<&'static KnownModel> {
    KNOWN.iter().find(|m| m.name == name)
}

/// Where model `name` lives under `model_dir`: `<model_dir>/whisper-<name>`.
pub fn model_path(model_dir: &Path, name: &str) -> PathBuf {
    model_dir.join(format!("whisper-{name}"))
}

/// Cheap pre-load check: every file is there, and for a known model has
/// the pinned size (the full checksum is verified when it is fetched).
pub fn check(model_dir: &Path, name: &str) -> Result<PathBuf, String> {
    let dir = model_path(model_dir, name);
    let hint = format!("run `caudal captions fetch-model {name} --dir {}`", model_dir.display());
    match known(name) {
        Some(m) => {
            for f in &m.files {
                let p = dir.join(f.name);
                match std::fs::metadata(&p) {
                    Ok(md) if md.len() == f.size => {}
                    Ok(md) => {
                        return Err(format!("{} is {} bytes, expected {}; {hint}", p.display(), md.len(), f.size));
                    }
                    Err(_) => return Err(format!("{} is missing; {hint}", p.display())),
                }
            }
        }
        None => {
            for f in ["config.json", "tokenizer.json", "model.safetensors"] {
                if !dir.join(f).is_file() {
                    return Err(format!(
                        "{} is missing (model `{name}` is not one Caudal can fetch)",
                        dir.join(f).display()
                    ));
                }
            }
        }
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_is_well_formed() {
        for m in &KNOWN {
            assert_eq!(m.revision.len(), 40);
            for f in &m.files {
                assert_eq!(f.sha256.len(), 64);
                assert!(f.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
            }
            assert!(m.url("config.json").starts_with("https://huggingface.co/openai/whisper-"));
        }
        // Operator guidance: all three fit the ~1.5 GB budget together.
        assert!(KNOWN.iter().map(KnownModel::total_size).sum::<u64>() < 1_500_000_000);
    }

    #[test]
    fn check_names_the_missing_file_and_the_fix() {
        let dir = std::env::temp_dir().join(format!("caudal-models-{}", std::process::id()));
        let err = check(&dir, "base").unwrap_err();
        assert!(err.contains("config.json is missing") && err.contains("fetch-model base"), "{err}");
    }
}
