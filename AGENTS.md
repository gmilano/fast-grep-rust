# AGENTS.md — fast-grep-rust

## What this project is

A high-performance regex search engine implemented in Rust. The core idea: build a sparse n-gram inverted index over a codebase so that most searches only need to scan a small fraction of files, rather than every file.

This is a research/learning project exploring the algorithms described in the [Cursor blog post on fast regex search](https://cursor.com/blog/fast-regex-search), with a full Rust implementation including Roaring Bitmaps, mmap persistence, and position mask filtering.

## Project structure

```
src/
├── main.rs        # Entry point, wires CLI to library
├── cli.rs         # clap-based CLI: search, index, bench, stats commands
├── lib.rs         # Public API re-exports
├── trigram.rs     # Classic trigram extraction and regex decomposition
├── sparse.rs      # Sparse n-gram algorithm (build_all + covering modes)
├── index.rs       # SparseIndex: HashMap<ngram, Vec<(doc_id, loc_mask, next_mask)>>
├── persist.rs     # Binary format: lookup table + mmap'd postings + meta.json
└── searcher.rs    # Rayon parallel verify + full-scan baseline
benches/
└── search.rs      # Criterion benchmarks
```

## Key algorithms

### Sparse n-gram extraction (src/sparse.rs)
- `extract_sparse_ngrams()` — build_all mode for indexing: extracts every possible sparse n-gram
- `extract_covering_ngrams()` — covering mode for querying: minimum set to cover the pattern
- Bigram weights come from `BIGRAM_FREQ` — a static array of pre-computed frequencies from real code corpora

### Index structure (src/index.rs)
- Each n-gram maps to `Vec<(u32, u8, u8)>` — (doc_id, loc_mask, next_mask)
- `loc_mask`: bit `i` set if n-gram appears at `position % 8 == i`
- `next_mask`: bloom filter of following characters (`char % 8`)
- Search: for consecutive n-grams T1→T2, check `(loc_mask_T1 << 1) & loc_mask_T2 != 0` and `next_mask_T1 & (1 << T2[0]%8) != 0`

### Persistence (src/persist.rs)
- `ngrams.lookup`: sorted array of `[hash_u32, offset_u64, len_u32]` — binary search by hash
- `ngrams.postings`: concatenated Roaring Bitmap serializations
- Load: lookup goes into a `Vec<LookupEntry>` in RAM; postings are `mmap`'d via `memmap2`
- Staleness: compare file mtimes stored in `meta.json` against current filesystem

### Parallel verify (src/searcher.rs)
- `search_persistent()` and `Searcher::search()` both use `rayon::par_iter()` over candidates
- Each thread reads its own files and runs `regex::Regex::is_match()` per line
- The `regex` crate uses the Teddy SIMD algorithm automatically when `target-cpu=native` is set

## Build

```bash
cargo build --release   # .cargo/config.toml sets target-cpu=native for SIMD
cargo test
cargo bench             # requires Linux kernel at /tmp/linux-6.6 or falls back to ./
```

## Development conventions

- All public functions return `anyhow::Result<T>` — no unwrap() in library code
- Index build progress is printed to stderr only when `verbose=true`
- CLI output goes to stdout; stats/progress to stderr
- Binary format versioned via `meta.json` `version` field — bump when format changes

## Known limitations

1. **Memory**: in-memory index for the Linux kernel (~81k files) requires ~600MB RAM with Roaring Bitmaps. The persistent index requires only ~22MB RAM at query time (everything else is mmap'd).
2. **Build time**: indexing 81k files takes ~66s single-threaded. Parallelizing the build with Rayon is a planned improvement.
3. **Regex decomposition**: complex patterns with many alternations or lookahead/behind are decomposed conservatively — the search falls back to full scan for patterns that yield no extractable n-grams.
4. **Incremental updates**: `is_stale()` detects changed files but full rebuild is still required. Incremental index merging is a planned improvement.

## Planned improvements

- [ ] Parallel index build with Rayon
- [ ] Incremental index update (merge changed files only)
- [ ] Roaring Bitmap posting lists (currently using Vec<(u32, u8, u8)> — migration planned)
- [ ] Real bigram frequency table built from the target corpus at index time
- [ ] Watch mode: rebuild index on file changes (inotify/kqueue)
- [ ] Language-aware tokenization for better n-gram extraction

## Related reading

- [Russ Cox — Regular Expression Matching with a Trigram Index](https://swtch.com/~rsc/regexp/regexp4.html)
- [Cursor — Fast Regex Search](https://cursor.com/blog/fast-regex-search)
- [Sourcegraph/zoekt](https://github.com/sourcegraph/zoekt) — production trigram index in Go
- [Nelson Elhage — Regex search with suffix arrays](https://blog.nelhage.com/2015/02/regular-expression-search-with-suffix-arrays/)
- [ripgrep internals](https://blog.burntsushi.net/ripgrep/)
