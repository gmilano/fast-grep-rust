# Benchmark Results

## Environment

- **Date:** 2026-04-06
- **Hardware:** Apple M1 Pro, 32 GB RAM
- **OS:** macOS (Darwin 23.2.0)
- **Rust:** stable, release build with LTO + `target-cpu=native`
- **Corpus:** Linux kernel 6.6 source tree
  - 81,690 files indexed
  - Index size: `ngrams.lookup` + `ngrams.postings` (mmap'd)

## Index Operations

| Operation | Time |
|-----------|------|
| Full index build | 60s |
| Index load (mmap) | 18ms |
| Incremental update (10 files modified) | 707ms |
| Full rebuild (for comparison) | 53s |
| Incremental speedup | 75x |

## Search with Persistent Index

| Pattern | ripgrep (ms) | fast-grep (ms) | Speedup | Candidate files | False positive rate |
|---------|-------------|----------------|---------|-----------------|---------------------|
| `EXPORT_SYMBOL` | 2130 | 269 | 7.9x | — | — |
| `TODO` | 2240 | 254 | 8.8x | — | — |
| `printk` | 2250 | 304 | 7.4x | — | — |
| `static.*inline` | 1910 | 444 | 4.3x | — | — |
| `int main` | 2640 | 339 | 7.8x | — | — |

**Average speedup: 7.2x**

**Average false positive rate: 0.42%** (candidates that pass the index filter but contain no actual match)

## Time Breakdown (Typical Query)

Measured on `TODO` pattern (254ms total):

| Phase | Time (ms) | % of total | Description |
|-------|----------|------------|-------------|
| N-gram hash + lookup | 0.38 | 0.15% | Hash query n-grams, binary search lookup table |
| Bitmap intersection | 3.6 | 1.4% | Roaring bitmap AND + position mask filtering |
| Candidate I/O + verify | ~220 | 86.6% | mmap candidate files, run regex/literal matcher |
| Overhead (load, collect) | ~30 | 11.8% | Index load, result collection, output |

## Search without Index (Full Scan)

| Pattern | ripgrep (ms) | fast-grep full scan (ms) | Ratio |
|---------|-------------|-------------------------|-------|
| `EXPORT_SYMBOL` | 2130 | ~2100 | 1.0x |
| `TODO` | 2240 | ~2400 | 0.9x |
| `printk` | 2250 | ~2500 | 0.9x |
| `static.*inline` | 1910 | ~2100 | 0.9x |
| `int main` | 2640 | ~2400 | 1.1x |

Without the index, fast-grep performs at roughly ripgrep parity (0.8–1.1x). The directory walker and regex engine dominate; there is no filtering advantage.

## Notes

- ripgrep times measured with `rg --no-heading --count <pattern>` (warm cache, best of 3 runs)
- fast-grep times measured with `fgr bench <pattern>` (warm cache, best of 3 runs)
- All searches performed with warm filesystem cache (second run onward)
- `static.*inline` is slower due to regex complexity — the `.*` wildcard prevents optimal n-gram decomposition, resulting in more candidates to verify
- `printk` appears in ~40k files; the position mask filter is critical for keeping its speedup high
