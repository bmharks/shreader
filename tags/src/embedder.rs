//! Story embeddings: a small language model turns a story's headline (and
//! the start of its text) into 384 numbers that capture what it's about, so
//! stories about the same thing come out close together even when they
//! share few words ("Nvidia CEO resigns" / "Huang steps down as Nvidia's
//! chief"). Tag models (see `model`) learn from these.
//!
//! The model is all-MiniLM-L6-v2 (sentence-transformers, Apache-2.0): about
//! 23 million parameters, run on the CPU with Hugging Face's pure-Rust
//! `candle`, at roughly 20 ms per story. Its three files (91 MB) are
//! downloaded once, from a fixed revision, and checked against known
//! SHA-256 hashes before use.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use sha2::{Digest, Sha256};

/// The model's Hugging Face repository and the exact revision used
const REPOSITORY: &str = "sentence-transformers/all-MiniLM-L6-v2";
const REVISION: &str = "1110a243fdf4706b3f48f1d95db1a4f5529b4d41";

/// The model's files: (name, SHA-256, largest acceptable size)
const FILES: [(&str, &str, usize); 3] = [
    ("config.json", "953f9c0d463486b10a6871cc2fd59f223b2c70184f49815e7efbcab5d8908b41", 16 * 1024),
    ("tokenizer.json", "be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037", 2 * 1024 * 1024),
    ("model.safetensors", "53aa51172d142c89d9012cce15ae4d6cc0ca6895895114379cacb4fab128d9db", 100 * 1024 * 1024),
];

/// Numbers per embedding
pub const DIMENSIONS: usize = 384;

/// Longest input, in tokens (word pieces): a headline plus a few sentences
const MAX_TOKENS: usize = 128;

/// How much of a story's text, after the title, goes into its embedding
const TEXT_CHARS: usize = 300;

/// Where the model lives: in the archive's folder, so it's private too
pub fn model_dir(archive: &Path) -> PathBuf {
    let base = archive.parent().unwrap_or(Path::new("."));
    base.join("models").join(format!("all-MiniLM-L6-v2-{}", &REVISION[..8]))
}

/// Whether all the model's files are present and intact.
pub fn model_present(dir: &Path) -> bool {
    FILES.iter().all(|(name, hash, _)| file_matches(&dir.join(name), hash))
}

fn file_matches(path: &Path, hash: &str) -> bool {
    std::fs::read(path).is_ok_and(|bytes| hex::encode(Sha256::digest(&bytes)) == hash)
}

/// Downloads any of the model's files that are missing or damaged, each
/// checked against its hash before it's kept. `download` fetches a URL
/// (at most the given size).
pub async fn ensure_model<F, Fut>(dir: &Path, download: F) -> Result<(), String>
where
    F: Fn(String, usize) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>, String>>,
{
    std::fs::create_dir_all(dir).map_err(|e| format!("can't create {:?}: {}", dir, e))?;
    for (name, hash, max_bytes) in FILES {
        let path = dir.join(name);
        if file_matches(&path, hash) {
            continue;
        }
        let url = format!("https://huggingface.co/{}/resolve/{}/{}", REPOSITORY, REVISION, name);
        let bytes = download(url, max_bytes).await.map_err(|e| format!("downloading {}: {}", name, e))?;
        let actual = hex::encode(Sha256::digest(&bytes));
        if actual != hash {
            return Err(format!("{} didn't match its expected contents (got {})", name, &actual[..12]));
        }
        // Written aside and renamed, so a half-written file is never used
        let partial = dir.join(format!("{}.partial", name));
        std::fs::write(&partial, &bytes).map_err(|e| format!("saving {}: {}", name, e))?;
        std::fs::rename(&partial, &path).map_err(|e| format!("saving {}: {}", name, e))?;
    }
    Ok(())
}

/// The loaded model.
pub struct Embedder {
    model: BertModel,
    tokenizer: tokenizers::Tokenizer,
    device: Device,
}

impl Embedder {
    /// Loads the model from `dir` (see `ensure_model`). The files' hashes are
    /// checked first: they're about to be parsed.
    pub fn load(dir: &Path) -> Result<Self> {
        anyhow::ensure!(model_present(dir), "the model's files are missing or damaged");
        let device = Device::Cpu;
        let config: Config = serde_json::from_str(&std::fs::read_to_string(dir.join("config.json"))?)
            .context("reading the model's configuration")?;
        let mut tokenizer = tokenizers::Tokenizer::from_file(dir.join("tokenizer.json")).map_err(anyhow::Error::msg)?;
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams { max_length: MAX_TOKENS, ..Default::default() }))
            .map_err(anyhow::Error::msg)?;
        tokenizer.with_padding(Some(tokenizers::PaddingParams::default()));
        // SAFETY: the file is the model's own (its hash was just checked), and
        // isn't changed while mapped
        let weights = unsafe { VarBuilder::from_mmaped_safetensors(&[dir.join("model.safetensors")], DType::F32, &device)? };
        let model = BertModel::load(weights, &config)?;
        Ok(Self { model, tokenizer, device })
    }

    /// One embedding (unit length) per text. CPU-heavy: off the UI thread.
    pub fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let encodings = self.tokenizer.encode_batch(texts.to_vec(), true).map_err(anyhow::Error::msg)?;
        let length = encodings.iter().map(|e| e.get_ids().len()).max().unwrap_or(0);
        let mut ids = Vec::with_capacity(texts.len() * length);
        let mut mask = Vec::with_capacity(texts.len() * length);
        for encoding in &encodings {
            ids.extend_from_slice(encoding.get_ids());
            mask.extend_from_slice(encoding.get_attention_mask());
        }
        let ids = Tensor::from_vec(ids, (texts.len(), length), &self.device)?;
        let mask = Tensor::from_vec(mask, (texts.len(), length), &self.device)?;
        let types = ids.zeros_like()?;
        let output = self.model.forward(&ids, &types, Some(&mask))?;
        // The average of the words' vectors (padding left out), scaled to length 1
        let mask = mask.to_dtype(DType::F32)?.unsqueeze(2)?;
        let mean = output.broadcast_mul(&mask)?.sum(1)?.broadcast_div(&mask.sum(1)?)?;
        let norm = mean.sqr()?.sum_keepdim(1)?.sqrt()?;
        Ok(mean.broadcast_div(&norm)?.to_vec2::<f32>()?)
    }
}

/// The text a story is embedded from: its title and the start of its text
pub fn story_text(title: &str, text: &str) -> String {
    let text: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(TEXT_CHARS).collect();
    if text.is_empty() { title.to_string() } else { format!("{}. {}", title, text) }
}

/// An embedding as stored: 16-bit floats, little-endian (768 bytes)
pub(crate) fn to_bytes(embedding: &[f32]) -> Vec<u8> {
    embedding.iter().flat_map(|&x| half::f16::from_f32(x).to_le_bytes()).collect()
}

pub fn from_bytes(bytes: &[u8]) -> Vec<f32> {
    bytes.chunks_exact(2).map(|b| half::f16::from_le_bytes([b[0], b[1]]).to_f32()).collect()
}

/// How alike two embeddings are: 1 = same meaning, around 0 = unrelated
pub fn similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

static EMBEDDER: OnceLock<Mutex<Option<Embedder>>> = OnceLock::new();

/// The loaded model, shared by everything in this program (loading takes a
/// moment, so it's kept once loaded, until `release`). Locked while in use.
pub fn shared(dir: &Path) -> Result<std::sync::MutexGuard<'static, Option<Embedder>>> {
    let mut guard = EMBEDDER.get_or_init(|| Mutex::new(None)).lock().unwrap_or_else(|p| p.into_inner());
    if guard.is_none() {
        *guard = Some(Embedder::load(dir)?);
    }
    Ok(guard)
}

/// Unloads the model (about 90 MB), if it's loaded.
pub fn release() {
    if let Some(embedder) = EMBEDDER.get() {
        *embedder.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_embeddings_keep_their_values() {
        let v: Vec<f32> = (0..DIMENSIONS).map(|i| ((i as f32) / 100.0).sin() / 20.0).collect();
        let back = from_bytes(&to_bytes(&v));
        assert_eq!(back.len(), DIMENSIONS);
        assert!(v.iter().zip(&back).all(|(a, b)| (a - b).abs() < 1e-3));
        assert_eq!(to_bytes(&v).len(), DIMENSIONS * 2);
    }

    #[test]
    fn story_text_is_title_and_start() {
        assert_eq!(story_text("Title", ""), "Title");
        assert_eq!(story_text("Title", "  Some\n text  here "), "Title. Some text here");
        assert_eq!(story_text("T", &"word ".repeat(200)).chars().count(), "T. ".len() + TEXT_CHARS);
    }

    /// Needs the model: `SHREADER_MODEL_DIR=/path/to/model cargo test model_ -- --ignored`
    #[test]
    #[ignore]
    fn model_tells_same_story_from_different() {
        let dir = PathBuf::from(std::env::var("SHREADER_MODEL_DIR").expect("set SHREADER_MODEL_DIR"));
        let embedder = Embedder::load(&dir).unwrap();
        let texts: Vec<String> = [
            "Nvidia CEO Jensen Huang resigns amid Chinese spy scandal",
            "Huang steps down as Nvidia's chief after affair allegations",
            "Spirit Airlines data sold to Google in bankruptcy sale",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let v = embedder.embed(&texts).unwrap();
        assert!(similarity(&v[0], &v[1]) > 0.6, "same story: {}", similarity(&v[0], &v[1]));
        assert!(similarity(&v[0], &v[2]) < 0.3, "different: {}", similarity(&v[0], &v[2]));
    }
}
