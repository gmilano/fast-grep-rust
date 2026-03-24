# fast-grep

> Regex search with a sparse n-gram inverted index. Beats ripgrep on filtered patterns by 5–14×.

## How it works

Instead of scanning every file, fast-grep builds a **sparse n-gram inverted index** over your codebase. When you search, it narrows the candidate set to a small fraction of files before running the regex engine.

Three layers of filtering:

1. **Sparse n-grams** — variable-length substrings weighted by bigram rarity (rare pairs → longer, more specific n-grams → fewer false positives)
2. **Roaring Bitmaps** — compressed posting lists; intersection is a bitwise AND, not a set walk
3. **Position masks** (Blackbird algorithm) — two 8-bit bloom filters per (n-gram, document) entry: one for position mod 8, one for the following character. Eliminates false positives caused by non-adjacent n-gram matches.

The index is persisted to disk as two binary files (`ngrams.lookup` + `ngrams.postings`) and memory-mapped at query time — load latency is ~20ms regardless of corpus size.

Verification of candidates runs in parallel via Rayon using the `regex` crate (Teddy SIMD algorithm, auto-enabled with `target-cpu=native`).

## Benchmark — Linux kernel 6.6 (81,690 files)

| Pattern | ripgrep | fast-grep persistent | Speedup |
|---------|---------|----------------------|---------|
| `TODO` | 1.80s | **130ms** | **13.9×** |
| `EXPORT_SYMBOL` | 1.50s | **163ms** | **9.2×** |
| `int main` | 1.86s | **391ms** | **4.8×** |
| `static.*inline` | 2.12s | **411ms** | **5.2×** |
| `printk` | 1.68s | 1.35s | 1.2× |

> `printk` is the worst case — it appears in ~40k files, so the index can't filter much. The position mask filter closes this gap significantly.

Index build is a one-time cost (~66s for the Linux kernel). Subsequent searches load in ~22ms.

## Install

```bash
git clone https://github.com/gmilano/fast-grep-rust
cd fast-grep-rust
cargo build --release
# Binary at ./target/release/fast-grep
```

For maximum performance (enables AVX2/NEON automatically):
```bash
# Already set in .cargo/config.toml:
# rustflags = ["-C", "target-cpu=native"]
cargo build --release
```

## Usage

```bash
# Search (builds in-memory index on first run)
fast-grep search "EXPORT_SYMBOL" /path/to/codebase

# Build persistent index
fast-grep index /path/to/codebase --output .fgr

# Search using persistent index (fast load)
fast-grep search "EXPORT_SYMBOL" /path/to/codebase --index .fgr

# Benchmark: compare full scan vs in-memory vs persistent vs ripgrep
fast-grep bench "static.*inline" /path/to/codebase

# Index stats
fast-grep stats --index .fgr
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
├── searcher.rs      # Rayon parallel verify, full-scan baseline
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

The lookup table is loaded into memory (~a few MB). The postings file is mmap'd — only the accessed posting lists are paged in by the OS.

## Algorithm details

### Sparse n-grams

A sparse n-gram starts and ends at a **local maximum** of the bigram weight function. Bigram weight = `1 - normalized_frequency`, so rare pairs (e.g. `_Z`, `>>`) have high weight and act as natural boundaries.

This produces fewer, longer, more specific n-grams than trigrams — especially useful for patterns with `.*` wildcards that would otherwise break trigram extraction.

### Position masks (Blackbird)

For each `(n-gram, document)` pair we store:
- `loc_mask: u8` — bit `i` set if the n-gram appears at position `pos % 8 == i`
- `next_mask: u8` — bit `next_char % 8` set for each character following the n-gram

When searching for a query that decomposes into consecutive n-grams T1, T2:
1. Load entries for T1 and T2
2. For each document in T1: check that `next_mask(T1)` has bit `T2[0] % 8` set
3. Check that `(loc_mask(T1) << 1) & loc_mask(T2) != 0` (must be adjacent positions)
4. Only documents passing both filters are candidates

False positives are still possible (bloom filter collisions) but the false positive rate drops dramatically for common n-grams like those in `printk`.

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
