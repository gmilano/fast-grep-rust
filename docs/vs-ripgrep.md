# fast-grep vs ripgrep

A detailed comparison of the two tools: when each one wins and why.

## Summary

| Dimension | ripgrep | fast-grep |
|-----------|---------|-----------|
| **Architecture** | No index, SIMD brute-force scan | Sparse n-gram inverted index |
| **First search** | Instant (no build step) | Requires index build (~60s for Linux kernel) |
| **Repeated searches** | Same speed every time | 4–9x faster with index |
| **Regex support** | Full PCRE2 / Rust regex | Full Rust regex (same engine for verification) |
| **Memory usage** | Low (streaming) | Higher (index in memory + mmap) |
| **Incremental cost** | None | ~700ms for 10 changed files |

## Why fast-grep wins with an index

### 1. It reads fewer files

ripgrep must open and scan every file in the tree. For the Linux kernel (81,690 files), this means reading ~800 MB of source code regardless of the pattern.

fast-grep's index reduces candidates to a tiny fraction:
- `EXPORT_SYMBOL` → ~0.5% of files are candidates
- `TODO` → ~3% of files
- Even `printk` (appears in ~40k files) benefits from position mask filtering

Reading 500 files instead of 81,690 is the primary source of speedup.

### 2. Lookup is nearly free

The index lookup phase (hash n-grams → binary search lookup table → load posting lists) takes **0.38ms**. Bitmap intersection takes **3.6ms**. Together, the filtering phase is <4ms — negligible compared to the ~220ms verification phase.

The postings file is mmap'd, so only the pages containing the accessed posting lists are read from disk. For a typical query touching 3-5 n-grams, this is a few KB of I/O.

### 3. Verification uses the same engine

Once candidates are identified, fast-grep verifies them with the same `regex` crate that ripgrep uses. The SIMD-accelerated Teddy algorithm, DFA compilation, and literal optimizations are identical. The only difference is that fast-grep runs this engine on 100-1000 files instead of 81,690.

## Why fast-grep can't beat ripgrep without an index

Without the index, fast-grep's full-scan mode performs at 0.8–1.1x of ripgrep. Here's why closing this gap is difficult:

### 1. Walker overhead is the same

Both tools must enumerate the file tree, respect `.gitignore`, skip binary files, and open each file. This I/O-bound work dominates the search time and can't be avoided without an index.

fast-grep uses the `ignore` crate (same as ripgrep) for gitignore-aware walking, so the walker performance is essentially identical.

### 2. ripgrep is extremely optimized

ripgrep has years of micro-optimization in its scanning pipeline:
- Teddy SIMD multi-pattern matcher for literal acceleration
- Lazy DFA with aggressive caching
- Memory-mapped I/O with optimal buffer sizes
- Careful `madvise` hints for sequential access
- Platform-specific `read` syscall tuning

Beating this without an index would require a fundamentally different approach (e.g., kernel bypass, io_uring), which adds complexity for diminishing returns.

### 3. Regex compilation is amortized

ripgrep compiles the regex once and reuses it across all files. For a single-pattern search, the compilation cost is negligible. fast-grep does the same — there's no advantage to be gained here.

## When to use each tool

### Use ripgrep when:

- **One-off searches** — no index to build, instant results
- **Exploring unfamiliar codebases** — you don't know the codebase well enough to justify building an index
- **Small projects** — under ~10k files, the index overhead isn't worth it (ripgrep finishes in <500ms anyway)
- **Interactive shell use** — piping, `xargs`, quick greps in a terminal
- **Replacement/refactoring** — ripgrep's integration with `sed`/editors is mature

### Use fast-grep when:

- **Repeated searches on the same codebase** — the index build is a one-time cost, every subsequent search is 4–9x faster
- **Large codebases** (>50k files) — the larger the codebase, the more the index helps
- **CI/CD pipelines** — build the index once per commit, run multiple pattern checks quickly
- **Code review tools** — where the index can be pre-built and shared across queries
- **Complex regex patterns** — patterns with `.*` wildcards that defeat ripgrep's literal optimizations still benefit from sparse n-gram filtering

### The crossover point

For the Linux kernel (81,690 files), the break-even point is approximately:

- **Index build cost:** 60 seconds
- **Per-query savings:** ~2 seconds (ripgrep ~2.2s → fast-grep ~0.3s)
- **Break-even:** ~32 searches

After ~32 searches, the cumulative time saved exceeds the index build cost. With incremental updates (707ms for 10 changed files), the index stays fresh cheaply.

## What about zoekt and livegrep?

| Tool | Advantage over fast-grep | Disadvantage |
|------|-------------------------|--------------|
| **zoekt** (Sourcegraph) | Battle-tested at scale, trigram index is simpler | Go, higher false positive rate from fixed trigrams, no position masks |
| **livegrep** | Suffix array enables arbitrary substring search without n-gram decomposition | C++, higher memory usage, slower index build |

fast-grep occupies a middle ground: better filtering than zoekt's trigrams (via sparse n-grams + position masks), lower memory than livegrep's suffix arrays, and Rust's safety/performance guarantees.
