use std::path::PathBuf;

/// Bigram frequency table: 65536 entries indexed by (c1 << 8 | c2).
/// Values are normalized frequencies [0.0, 1.0].
/// Built from the actual corpus during indexing for optimal sparse n-gram selection.
pub struct BigramFreq {
    pub table: Box<[f32; 65536]>,
}

const SAMPLE_BYTES: usize = 16 * 1024; // read first 16KB per file

impl BigramFreq {
    /// Build frequency table by sampling files from the corpus.
    /// Samples up to `sample_size` files (deterministic seed 42),
    /// reads first 16KB each, counts printable ASCII bigrams (32-126),
    /// normalizes by max count.
    pub fn from_corpus(files: &[PathBuf], sample_size: usize) -> Self {
        let mut table = Box::new([0.0f32; 65536]);
        let mut counts = vec![0u64; 65536];

        // Deterministic sampling: pick evenly-spaced indices (seed-42-style)
        let indices: Vec<usize> = if files.len() <= sample_size {
            (0..files.len()).collect()
        } else {
            // Simple deterministic sampling: stride through the list
            let mut rng_state: u64 = 42;
            let mut idx = Vec::with_capacity(sample_size);
            for _ in 0..sample_size {
                rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
                let i = (rng_state >> 33) as usize % files.len();
                idx.push(i);
            }
            idx.sort();
            idx.dedup();
            idx
        };

        for &i in &indices {
            let data = match std::fs::read(&files[i]) {
                Ok(d) => d,
                Err(_) => continue,
            };
            // Skip binary
            if data.iter().take(512).any(|&b| b == 0) {
                continue;
            }
            let end = data.len().min(SAMPLE_BYTES);
            for w in data[..end].windows(2) {
                let (a, b) = (w[0], w[1]);
                // Only count printable ASCII bigrams
                if (32..=126).contains(&a) && (32..=126).contains(&b) {
                    counts[(a as usize) << 8 | b as usize] += 1;
                }
            }
        }

        let max_count = counts.iter().copied().max().unwrap_or(1).max(1);
        for (i, &c) in counts.iter().enumerate() {
            table[i] = c as f32 / max_count as f32;
        }

        BigramFreq { table }
    }

    /// Deserialize from raw bytes (65536 × f32 little-endian).
    pub fn from_bytes(data: &[u8]) -> Self {
        let mut table = Box::new([0.0f32; 65536]);
        if data.len() >= 65536 * 4 {
            for i in 0..65536 {
                let off = i * 4;
                table[i] = f32::from_le_bytes([
                    data[off], data[off + 1], data[off + 2], data[off + 3],
                ]);
            }
        }
        BigramFreq { table }
    }

    /// Serialize to raw bytes (65536 × f32 little-endian).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(65536 * 4);
        for &v in self.table.iter() {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        buf
    }

    /// Uniform (flat) frequency table — all printable ASCII bigrams get equal weight.
    pub fn flat() -> Self {
        let mut table = Box::new([0.0f32; 65536]);
        for c1 in 32u8..=126 {
            for c2 in 32u8..=126 {
                table[(c1 as usize) << 8 | c2 as usize] = 0.001;
            }
        }
        BigramFreq { table }
    }

    #[inline]
    pub fn weight(&self, a: u8, b: u8) -> f32 {
        1.0 - self.table[(a as usize) << 8 | b as usize]
    }
}

impl Default for BigramFreq {
    fn default() -> Self {
        Self::flat()
    }
}

/// Score an n-gram by the minimum bigram weight (rarer = higher score = better for filtering).
fn ngram_score(ngram: &[u8], freq: &BigramFreq) -> f32 {
    if ngram.len() < 2 {
        return 0.0;
    }
    let mut min_weight = f32::MAX;
    for w in ngram.windows(2) {
        let weight = freq.weight(w[0], w[1]);
        if weight < min_weight {
            min_weight = weight;
        }
    }
    min_weight
}

/// Extract ALL n-grams from text for indexing.
/// Uses variable-length n-grams (3-6 bytes), selecting those with rare bigrams.
pub fn extract_sparse_ngrams(text: &[u8], freq: &BigramFreq) -> Vec<Box<[u8]>> {
    if text.len() < 3 {
        return vec![];
    }

    let mut ngrams = Vec::new();
    let mut pos = 0;

    while pos + 3 <= text.len() {
        // Try to find the best n-gram starting at this position
        let max_len = std::cmp::min(6, text.len() - pos);
        let mut best_len = 3;
        let mut best_score = ngram_score(&text[pos..pos + 3], freq);

        for len in 4..=max_len {
            let score = ngram_score(&text[pos..pos + len], freq);
            if score > best_score {
                best_score = score;
                best_len = len;
            }
        }

        // Only include if score is above threshold (rare enough to be useful)
        if best_score > 0.85 {
            ngrams.push(text[pos..pos + best_len].into());
        }

        pos += 1;
    }

    // Deduplicate
    ngrams.sort();
    ngrams.dedup();
    ngrams
}

/// Extract a minimal covering set of n-grams for a query pattern.
/// These are the rarest n-grams that cover the pattern's literal portions.
pub fn extract_covering_ngrams(text: &[u8], freq: &BigramFreq) -> Vec<Box<[u8]>> {
    if text.len() < 3 {
        return vec![];
    }

    // Collect all candidate n-grams with their positions and scores
    let mut candidates: Vec<(usize, usize, f32, Box<[u8]>)> = Vec::new();

    for pos in 0..text.len().saturating_sub(2) {
        let max_len = std::cmp::min(6, text.len() - pos);
        for len in 3..=max_len {
            let ngram = &text[pos..pos + len];
            let score = ngram_score(ngram, freq);
            candidates.push((pos, len, score, ngram.into()));
        }
    }

    // Greedy set cover: pick highest-scoring n-gram, mark positions covered, repeat
    let text_len = text.len();
    let mut covered = vec![false; text_len];
    let mut result = Vec::new();

    // Sort by score descending (rarest first)
    candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    for (pos, len, score, ngram) in &candidates {
        if *score < 0.5 {
            break; // Too common to be useful
        }
        // Check if this covers any uncovered positions
        let covers_new = ((*pos)..(*pos + *len)).any(|i| !covered[i]);
        if covers_new {
            for i in (*pos)..(*pos + *len) {
                covered[i] = true;
            }
            result.push(ngram.clone());
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // --- BigramFreq tests ---

    /// Helper: build a freq table from raw text bytes (for tests without files).
    fn freq_from_text(texts: &[&[u8]]) -> BigramFreq {
        let mut table = Box::new([0.0f32; 65536]);
        let mut counts = vec![0u64; 65536];
        for text in texts {
            for w in text.windows(2) {
                let (a, b) = (w[0], w[1]);
                if (32..=126).contains(&a) && (32..=126).contains(&b) {
                    counts[(a as usize) << 8 | b as usize] += 1;
                }
            }
        }
        let max_count = counts.iter().copied().max().unwrap_or(1).max(1);
        for (i, &c) in counts.iter().enumerate() {
            table[i] = c as f32 / max_count as f32;
        }
        BigramFreq { table }
    }

    #[test]
    fn common_bigram_has_lower_weight_than_rare() {
        // Build freq from English-like text where "th" is common and "zq" never appears
        let freq = freq_from_text(&[
            b"the thing that they thought through the thick thicket",
            b"this then there these those three throw thread",
        ]);
        let w_th = freq.weight(b't', b'h');
        let w_zq = freq.weight(b'z', b'q');
        assert!(w_th < w_zq, "th={} should be < zq={}", w_th, w_zq);
    }

    // --- extract_sparse_ngrams tests (ported from sparse-ngram.test.ts) ---

    #[test]
    fn sparse_returns_empty_for_short_text() {
        let freq = BigramFreq::flat();
        assert!(extract_sparse_ngrams(b"", &freq).is_empty());
        assert!(extract_sparse_ngrams(b"ab", &freq).is_empty());
    }

    #[test]
    fn sparse_produces_ngrams_for_short_text() {
        let freq = BigramFreq::flat();
        let ngrams = extract_sparse_ngrams(b"hello", &freq);
        // All n-grams should be substrings of the text
        for ng in &ngrams {
            let text = b"hello";
            assert!(
                text.windows(ng.len()).any(|w| w == ng.as_ref()),
                "{:?} should be a substring of 'hello'",
                ng
            );
        }
    }

    #[test]
    fn sparse_produces_ngrams_from_longer_text() {
        let freq = BigramFreq::flat();
        let ngrams = extract_sparse_ngrams(b"EXPORT_SYMBOL", &freq);
        assert!(!ngrams.is_empty());
    }

    #[test]
    fn sparse_deduplicates_ngrams() {
        let freq = BigramFreq::flat();
        let ngrams = extract_sparse_ngrams(b"aaabbbaaabbb", &freq);
        let unique: HashSet<&[u8]> = ngrams.iter().map(|n| n.as_ref()).collect();
        assert_eq!(ngrams.len(), unique.len());
    }

    // --- extract_covering_ngrams tests (ported from extractCoveringSparseNgrams) ---

    #[test]
    fn covering_returns_empty_for_short_text() {
        let freq = BigramFreq::flat();
        assert!(extract_covering_ngrams(b"", &freq).is_empty());
        assert!(extract_covering_ngrams(b"ab", &freq).is_empty());
    }

    #[test]
    fn covering_ngrams_are_substrings_of_text() {
        let freq = BigramFreq::flat();
        let text = b"functionName";
        let covering = extract_covering_ngrams(text, &freq);
        for ng in &covering {
            assert!(
                text.windows(ng.len()).any(|w| w == ng.as_ref()),
                "{:?} should be a substring of 'functionName'",
                ng
            );
        }
    }

    #[test]
    fn covering_returns_fewer_or_equal_ngrams_than_full() {
        let freq = freq_from_text(&[
            b"the thing that they thought through the thick thicket",
            b"this then there these those three throw thread",
        ]);
        let text = b"this is a longer text for testing coverage";
        let all = extract_sparse_ngrams(text, &freq);
        let covering = extract_covering_ngrams(text, &freq);
        assert!(covering.len() <= all.len());
    }

    #[test]
    fn covering_ngrams_are_substrings_of_input() {
        let freq = BigramFreq::flat();
        let text = b"constructorPattern";
        let covering = extract_covering_ngrams(text, &freq);
        assert!(!covering.is_empty());
        // Every covering n-gram must be a substring of the input text
        for ng in &covering {
            assert!(
                text.windows(ng.len()).any(|w| w == ng.as_ref()),
                "covering ngram {:?} should be a substring of input",
                ng
            );
        }
    }
}
