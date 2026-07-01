//! Embeddings — the optional semantic layer, gated by the `embeddings` feature
//! AND a configured model. Nano (no feature, or `OCCIPITAL_EMBED_MODEL=""`) →
//! FTS5 keyword recall. Micro+ (`--features embeddings` + a model) → cosine
//! semantic recall. Behind a trait so the recall logic is testable without the
//! ONNX model, and so the fastembed runtime is compiled in only when wanted.

use std::sync::Arc;

/// Produces a dense vector for a text.
pub trait Embedder: Send + Sync {
    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>>;
}

/// Cosine similarity in `[-1, 1]`; `0` for empty or mismatched-length vectors.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// Build the configured embedder, or `None` for FTS5-only recall.
#[cfg(feature = "embeddings")]
pub fn make_embedder(model: &str) -> Option<Arc<dyn Embedder>> {
    if model.is_empty() {
        return None;
    }
    match fast::FastEmbedder::new(model) {
        Ok(e) => {
            tracing::info!("semantic recall enabled (model {model})");
            Some(Arc::new(e))
        }
        Err(e) => {
            tracing::warn!("embedder init failed ({e}); falling back to FTS5 keyword recall");
            None
        }
    }
}

/// Build-without-feature variant: always FTS5 keyword recall.
#[cfg(not(feature = "embeddings"))]
pub fn make_embedder(model: &str) -> Option<Arc<dyn Embedder>> {
    if !model.is_empty() {
        tracing::warn!(
            "OCCIPITAL_EMBED_MODEL is set but this binary was built without \
             `--features embeddings` — using FTS5 keyword recall"
        );
    }
    None
}

#[cfg(feature = "embeddings")]
mod fast {
    use std::sync::Mutex;

    use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

    use super::Embedder;

    /// fastembed (ONNX) embedder. The model is downloaded on first construction
    /// to fastembed's cache dir; serialized behind a `Mutex` (the session isn't
    /// `Sync`). Embedding is CPU-bound and brief.
    pub struct FastEmbedder {
        model: Mutex<TextEmbedding>,
    }

    impl FastEmbedder {
        pub fn new(model_id: &str) -> anyhow::Result<Self> {
            let model = map_model(model_id)?;
            let te = TextEmbedding::try_new(
                InitOptions::new(model).with_show_download_progress(false),
            )?;
            Ok(Self { model: Mutex::new(te) })
        }
    }

    fn map_model(id: &str) -> anyhow::Result<EmbeddingModel> {
        Ok(match id {
            "BAAI/bge-small-en-v1.5" | "bge-small-en-v1.5" | "bge-small" => {
                EmbeddingModel::BGESmallENV15
            }
            other => anyhow::bail!("unsupported embed model: {other}"),
        })
    }

    impl Embedder for FastEmbedder {
        fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
            let m = self.model.lock().unwrap();
            let mut out = m.embed(vec![text.to_string()], None)?;
            out.pop().ok_or_else(|| anyhow::anyhow!("empty embedding"))
        }
    }
}

/// A deterministic bag-of-words embedder for tests — no model, no network.
/// Hashes tokens into a fixed-dim vector, so overlapping text scores higher.
#[cfg(test)]
pub struct BagOfWordsEmbedder {
    dim: usize,
}

#[cfg(test)]
impl BagOfWordsEmbedder {
    pub fn new() -> Self {
        Self { dim: 64 }
    }
}

#[cfg(test)]
impl Default for BagOfWordsEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl Embedder for BagOfWordsEmbedder {
    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let mut v = vec![0f32; self.dim];
        for tok in text.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()) {
            let mut h: u32 = 2_166_136_261;
            for b in tok.to_lowercase().bytes() {
                h ^= b as u32;
                h = h.wrapping_mul(16_777_619);
            }
            v[(h as usize) % self.dim] += 1.0;
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_basics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6, "identical → 1");
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6, "orthogonal → 0");
        assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0, "mismatched length → 0");
        assert_eq!(cosine(&[], &[]), 0.0, "empty → 0");
    }

    #[test]
    fn bag_of_words_scores_overlap_higher() {
        let e = BagOfWordsEmbedder::new();
        let q = e.embed("rust async programming").unwrap();
        let near = e.embed("async programming in rust").unwrap();
        let far = e.embed("baking sourdough bread").unwrap();
        assert!(cosine(&q, &near) > cosine(&q, &far), "topical overlap ranks higher");
    }
}
