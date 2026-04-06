# fast-grep

> Regex search with a sparse n-gram inverted index. Beats ripgrep by 4–9x on indexed corpora.

## How it works

Instead of scanning every file, fast-grep builds a **sparse n-gram inverted index** over your codebase. When you search, it narrows the candidate set to a small fraction of files before running the regex engine.

### Filtering pipeline

1. **Sparse n-grams** — variable-length substrings weighted by corpus-adaptive bigram rarity. Rare character pairs produce longer, more specific n-grams with fewer false positives than fixed trigrams.
2. **Roaring Bitmaps** — compressed posting lists; candidate intersection is a bitwise AND, not a linear scan.
3. **Position masks** (Blackbird algorithm) — two 8-bit bloom filters per (n-gram, document) entry encode position and successor character. Eliminates false positives from non-adjacent n-gram matches.
4. **Literal pre-filter** — SIMD-accelerated `memchr`/`memmem` and Aho-Corasick multi-pattern matching skip the regex engine entirely for literal patterns.
5. **Parallel verification** — Rayon work-stealing pool verifies candidates using the `regex` crate (Teddy SIMD, auto-enabled with `target-cpu=native`).

The index is persisted to disk as two binary files (`ngrams.lookup` + `ngrams.postings`) and memory-mapped at query time — load latency is ~18ms regardless of corpus size.

## Benchmark — Linux kernel 6.6 (81,690 files)

**Hardware:** Apple M1 Pro, 32 GB RAM | **Date:** 2026-04-06

### With persistent index

| Pattern | ripgrep | fast-grep | Speedup |
|---------|---------|-----------|---------|
| `EXPORT_SYMBOL` | 2.13s | **269ms** | **7.9x** |
| `TODO` | 2.24s | **254ms** | **8.8x** |
| `printk` | 2.25s | **304ms** | **7.4x** |
| `static.*inline` | 1.91s | **444ms** | **4.3x** |
| `int main` | 2.64s | **339ms** | **7.8x** |

### Time breakdown (typical query)

| Phase | Time | Description |
|-------|------|-------------|
| Lookup | 0.38ms | Hash n-grams → fetch posting lists from mmap'd file |
| Intersection | 3.6ms | Roaring bitmap AND + position mask filtering |
| Verify | ~220ms | Parallel regex match on candidate files |
| **Total** | **~254ms** | vs ripgrep ~2.2s |

> ~98% of time is spent in verification. The index reduces candidates to <1% of files (false positive rate: 0.42%).

### Without index (full scan)

Without an index, fast-grep performs comparably to ripgrep (0.8–1.1x). The walker and regex engine dominate, and there's no filtering advantage.

### Index operations

| Operation | Time |
|-----------|------|
| Full build | ~60s (81,690 files) |
| Index load (mmap) | 18ms |
| Incremental update (10 files changed) | 707ms vs 53s rebuild (**75x faster**) |

## Install

```bash
git clone https://github.com/gmilano/fast-grep-rust
cd fast-grep-rust
cargo build --release
```

The binary is at `./target/release/fgr`. For maximum performance (AVX2/NEON auto-enabled):

```bash
# Already configured in .cargo/config.toml:
# rustflags = ["-C", "target-cpu=native"]
cargo build --release
```

## Usage

```bash
# Build persistent index
fgr index /path/to/codebase --output .fgr

# Search using persistent index
fgr search "EXPORT_SYMBOL" /path/to/codebase --index .fgr

# Search without index (full scan, ripgrep-like)
fgr search "EXPORT_SYMBOL" /path/to/codebase

# Benchmark: compare full scan vs in-memory vs persistent vs ripgrep
fgr bench "static.*inline" /path/to/codebase

# Index stats
fgr stats --index .fgr
```

### Flags

| Flag | Description |
|------|-------------|
| `--index <path>` | Use persistent index from this directory |
| `--files-only` | Print matching file paths only |
| `--count` | Print match count only |
| `--type <ext>` | Filter by file extension (e.g. `c`, `rs`, `ts`) |
| `--no-ignore` | Don't respect `.gitignore` |

## Architecture

```
src/
├── main.rs          # Entry point
├── cli.rs           # clap CLI, bench runner
├── trigram.rs       # Classic trigram extraction + regex decomposition
├── sparse.rs        # Sparse n-gram extraction (build_all + covering modes)
├── index.rs         # SparseIndex with Roaring Bitmaps + position masks
├── persist.rs       # Binary index format, mmap loading, staleness check
├── searcher.rs      # Rayon parallel verify, full-scan baseline, SIMD pre-filter
├── freq_real.rs     # Corpus-adaptive bigram frequency table
└── lib.rs           # Public API
```

### Index format

```
.fgr/
├── ngrams.lookup    # Sorted table: [hash_u32][offset_u64][len_u32] per entry
├── ngrams.postings  # Roaring bitmap posting lists, concatenated
├── docids.bin       # [len_u16][path_bytes] per document
└── meta.json        # version, doc count, root dir, file mtimes
```

The lookup table is loaded into memory (~a few MB). The postings file is mmap'd — only accessed posting lists are paged in by the OS.

## Techniques

See [docs/techniques.md](docs/techniques.md) for detailed descriptions of:
- Sparse n-grams vs classical trigrams
- Corpus-adaptive bigram frequency table
- Position masks (Blackbird algorithm)
- Persistent index with mmap
- SIMD literal pre-filtering
- Incremental index updates

See [docs/vs-ripgrep.md](docs/vs-ripgrep.md) for a detailed comparison with ripgrep.

## Roadmap

- [ ] **SIMD-accelerated bitmap intersection** — AVX2/NEON intrinsics for Roaring bitmap AND operations
- [ ] **Query plan optimizer** — choose n-gram decomposition based on posting list sizes, not just bigram rarity
- [ ] **Multi-pattern search** — batch multiple queries in a single index pass
- [ ] **File-type aware indexing** — separate indexes per language for type-filtered searches
- [ ] **Daemon mode** — long-running process with filesystem watcher for instant incremental updates
- [ ] **Compressed postings** — variable-byte or PFOR-delta encoding for smaller index files
- [ ] **GPU verification** — offload regex matching to GPU for very large candidate sets

## Comparison with related work

| Project | Algorithm | Language | Notes |
|---------|-----------|----------|-------|
| [ripgrep](https://github.com/BurntSushi/ripgrep) | No index, SIMD scan | Rust | Best no-index tool |
| [zoekt](https://github.com/sourcegraph/zoekt) | Trigram index | Go | Powers Sourcegraph |
| [livegrep](https://github.com/livegrep/livegrep) | Suffix array | C++ | Best for short patterns |
| [Cursor](https://cursor.com/blog/fast-regex-search) | Sparse n-gram | (closed) | Inspiration for this project |
| **fast-grep** | Sparse n-gram + Roaring + position masks | Rust | This project |

## License

MIT
