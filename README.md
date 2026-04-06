# fast-grep

> Indexed regex search. 6–25x faster than ripgrep, 2–10x faster than ugrep.

Built for agent harnesses and large codebases where grep is the bottleneck. A one-time index build turns every subsequent search into a sub-200ms lookup instead of a multi-second full scan.

## Benchmarks — Linux kernel 6.6 (81,690 files)

**Apple M1 Pro, 32 GB RAM — warm cache**

### vs ripgrep (no index)

| Pattern | fast-grep | ripgrep | Speedup |
|---------|-----------|---------|---------|
| `TODO` | **97ms** | 2,463ms | **25x** |
| `printk` | **172ms** | 2,492ms | **14x** |
| `EXPORT_SYMBOL` | **197ms** | 1,553ms | **8x** |
| `container_of` | **344ms** | 2,440ms | **7x** |
| `static.*inline` | **394ms** | 2,369ms | **6x** |

### vs ugrep (indexed)

| Pattern | fast-grep | ugrep | Speedup |
|---------|-----------|-------|---------|
| `EXPORT_SYMBOL` | **197ms** | 1,898ms | **9.6x** |
| `TODO` | **97ms** | 599ms | **6.2x** |
| `static.*inline` | **394ms** | 1,595ms | **4.0x** |
| `printk` | **172ms** | 645ms | **3.8x** |
| `container_of` | **344ms** | 656ms | **1.9x** |

**Without index:** comparable to ripgrep (~2–2.5s full scan).

### Index cost

| Metric | Value |
|--------|-------|
| Full build | ~60s (one-time) |
| Incremental update | <1s for 10–100 files (75x faster than rebuild) |
| Index load (mmap) | 17ms |
| Index size | 775 MB postings + 161 MB bitmaps |

## How it works

Five techniques combine to eliminate >99% of I/O before the regex engine runs:

1. **Sparse n-grams with adaptive frequency table** — Variable-length substrings weighted by corpus-specific bigram rarity. Produces fewer, more selective posting lists than fixed trigrams.

2. **Position masks (Blackbird algorithm)** — Two 8-bit bloom filters per (n-gram, document) encode position and successor character. Drops the false positive rate to 0.42%.

3. **Persistent index with mmap** — Binary posting lists memory-mapped at query time. 17ms load regardless of corpus size; the OS pages in only the lists you touch.

4. **Line-level index with byte offsets** — Index stores line positions, not just file IDs. Verification jumps directly to candidate lines instead of scanning entire files.

5. **4-byte content prefix filter** — Before running the regex engine, checks a 4-byte content prefix per candidate. Eliminates 95%+ of I/O during verification.

## Install

```bash
git clone https://github.com/gmilano/fast-grep-rust
cd fast-grep-rust
cargo build --release
```

Binary at `./target/release/fgr`. SIMD (AVX2/NEON) auto-enabled via `.cargo/config.toml`.

## Usage

```bash
# Build index (one-time, ~60s for Linux kernel)
fgr index /path/to/codebase --output .fgr

# Search with index
fgr search "EXPORT_SYMBOL" /path/to/codebase --index .fgr

# Search without index (ripgrep-equivalent full scan)
fgr search "EXPORT_SYMBOL" /path/to/codebase

# Benchmark against ripgrep
fgr bench "static.*inline" /path/to/codebase

# Index stats
fgr stats --index .fgr
```

### Flags

| Flag | Description |
|------|-------------|
| `--index <path>` | Use persistent index |
| `--files-only` | Print file paths only |
| `--count` | Print match count |
| `--type <ext>` | Filter by extension (`c`, `rs`, `ts`) |
| `--no-ignore` | Don't respect `.gitignore` |

## Why this matters for agents

LLM coding agents (Cursor, Claude Code, Aider) spend significant time grepping large repos. Every search blocks the agent's next reasoning step. fast-grep turns 2.5s waits into <200ms lookups — a 10x reduction in tool-call latency that compounds across an entire coding session.

## Related work

| Project | Approach | Notes |
|---------|----------|-------|
| [ripgrep](https://github.com/BurntSushi/ripgrep) | SIMD scan, no index | Best no-index grep |
| [ugrep](https://github.com/Genivia/ugrep) | Index + scan | Previously fastest indexed grep |
| [zoekt](https://github.com/sourcegraph/zoekt) | Trigram index (Go) | Powers Sourcegraph |
| [Cursor](https://cursor.com/blog/fast-regex-search) | Sparse n-gram (closed) | Inspiration for this project |

## License

MIT
