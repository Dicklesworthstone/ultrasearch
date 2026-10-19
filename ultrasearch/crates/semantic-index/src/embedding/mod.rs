//! Deterministic feature-hashing text embedder and similarity utilities.

pub const DEFAULT_EMBEDDING_DIM: usize = 128;

/// Stateless deterministic text embedder using signed random projections
/// (Weinberger feature hashing) over word tokens and character trigrams.
#[derive(Debug, Clone, Copy)]
pub struct TextEmbedder {
    dim: usize,
}

impl Default for TextEmbedder {
    fn default() -> Self {
        Self::new(DEFAULT_EMBEDDING_DIM)
    }
}

impl TextEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim: dim.max(8) }
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    pub fn embed(&self, text: &str) -> Vec<f32> {
        embed_text(text, self.dim)
    }
}

fn fnv1a_64(bytes: &[u8], seed: u64) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64 ^ seed;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

fn accumulate_feature(vec: &mut [f32], feature: &[u8], seed: u64, weight: f32) {
    let hash = fnv1a_64(feature, seed);
    let idx = (hash as usize) % vec.len();
    let sign = if (hash >> 63) == 0 { 1.0 } else { -1.0 };
    vec[idx] += sign * weight;
}

/// Generate an L2-normalized embedding vector of dimension `dim` from `text`.
pub fn embed_text(text: &str, dim: usize) -> Vec<f32> {
    let dim = dim.max(8);
    let mut vec = vec![0.0_f32; dim];

    for raw_token in text.split(|c: char| !c.is_alphanumeric()) {
        if raw_token.len() < 2 {
            continue;
        }
        let token = raw_token.to_ascii_lowercase();
        accumulate_feature(&mut vec, token.as_bytes(), 0x11, 1.0);

        let chars: Vec<char> = token.chars().collect();
        if chars.len() >= 3 {
            let mut trigram_buf = String::with_capacity(12);
            for window in chars.windows(3) {
                trigram_buf.clear();
                for &ch in window {
                    trigram_buf.push(ch);
                }
                accumulate_feature(&mut vec, trigram_buf.as_bytes(), 0x29, 0.35);
            }
        }
    }

    l2_normalize(&mut vec);
    vec
}

/// Normalize a vector in-place to unit L2 length (if non-zero).
pub fn l2_normalize(vec: &mut [f32]) {
    let norm_sq: f32 = vec.iter().map(|v| v * v).sum();
    if norm_sq > f32::EPSILON {
        let inv = 1.0 / norm_sq.sqrt();
        for v in vec.iter_mut() {
            *v *= inv;
        }
    }
}

/// Compute cosine similarity in `[-1.0, 1.0]` between two vectors.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut norm_a = 0.0_f32;
    let mut norm_b = 0.0_f32;
    for (&x, &y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a <= f32::EPSILON || norm_b <= f32::EPSILON {
        return 0.0;
    }
    (dot / (norm_a.sqrt() * norm_b.sqrt())).clamp(-1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedder_ranks_similar_documents_above_unrelated_ones() {
        let embedder = TextEmbedder::default();
        let query = embedder.embed("quarterly financial revenue report");
        let matching = embedder.embed("Q4 financial revenue report and balance sheet");
        let unrelated = embedder.embed("rust kernel driver ntfs journal ioctl");

        let sim_match = cosine_similarity(&query, &matching);
        let sim_unrelated = cosine_similarity(&query, &unrelated);
        assert!(
            sim_match > sim_unrelated + 0.25,
            "expected matching similarity ({sim_match}) >> unrelated ({sim_unrelated})"
        );
    }
}
