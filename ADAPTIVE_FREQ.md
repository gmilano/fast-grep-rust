# Task: Corpus-adaptive bigram frequency table

## Problem
BIGRAM_FREQ in src/sparse.rs is hardcoded. Sparse n-grams are suboptimal for specific corpora.

## Solution
Build BigramFreq from the actual corpus during indexing.

### src/sparse.rs — add BigramFreq struct
```rust
pub struct BigramFreq {
    pub table: Box<[f32; 65536]>,
}

impl BigramFreq {
    pub fn from_corpus(files: &[PathBuf], sample_size: usize) -> Self;
    pub fn from_bytes(data: &[u8]) -> Self;
    pub fn to_bytes(&self) -> Vec<u8>;
    pub fn flat() -> Self;
    pub fn weight(&self, a: u8, b: u8) -> f32 { 1.0 - self.table[a as usize * 256 + b as usize] }
}
```

from_corpus: sample up to sample_size files (seed 42), read first 16KB each,
count printable ASCII bigrams (32-126), normalize by max count.

### src/persist.rs
- build(): construct BigramFreq::from_corpus(&all_files, 5000), serialize to_bytes(),
  save as base64 in meta.json field "bigram_freq_b64"
- Use that freq for extract_sparse_ngrams on each document
- load(): if bigram_freq_b64 in meta: BigramFreq::from_bytes(), else BigramFreq::flat()
- PersistentIndex stores freq field, search() uses self.freq for extract_covering_ngrams

### src/index.rs
- build_from_directory(): collect all paths first, then BigramFreq::from_corpus(&paths, 3000),
  use that freq for indexing

### Remove global BIGRAM_FREQ hardcoded table

### Cargo.toml: add base64 = "0.21" if not present

## Steps after implementation
1. cargo build --release -q
2. ./target/release/fast-grep index /tmp/linux-6.6 --output /tmp/fgr-bench2
3. ./target/release/fast-grep bench "EXPORT_SYMBOL" /tmp/linux-6.6 (uses fgr-bench2 from tmp_dir)
4. ./target/release/fast-grep bench "printk" /tmp/linux-6.6
5. Show: candidates before vs after adaptive table
6. git add -A && git commit -m "feat: corpus-adaptive bigram frequency table"
7. git push
8. openclaw system event --text "adaptive freq listo" --mode now
