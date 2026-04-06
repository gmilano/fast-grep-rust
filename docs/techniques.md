# Techniques

Detailed description of the algorithms and optimizations used in fast-grep.

## Sparse N-grams vs Classical Trigrams

Classical code search indexes (e.g., zoekt, Google Code Search) decompose queries into fixed-length trigrams (3-character substrings). This has two problems:

1. **Common trigrams are useless.** Trigrams like `the`, `int`, `for` appear in nearly every file and provide no filtering value.
2. **Wildcards break decomposition.** A pattern like `static.*inline` cannot produce useful trigrams across the `.*` boundary.

Sparse n-grams solve both problems. Instead of fixed-length substrings, we extract variable-length substrings whose boundaries are determined by **bigram rarity**:

- Each adjacent character pair (bigram) is assigned a weight: `weight = 1 - normalized_frequency`
- Rare pairs like `_Z`, `>>`, `kw` have high weight
- An n-gram starts and ends at **local maxima** of the bigram weight function

This produces fewer, longer, more specific n-grams. For example, `EXPORT_SYMBOL` might produce the full string as a single n-gram (since `_S` is rare), while a trigram index would produce `EXP`, `XPO`, `POR`, `ORT`, `RT_`, `T_S`, `_SY`, `SYM`, `YMB`, `MBO`, `BOL` — many of which are common and provide weak filtering.

Implementation: `src/sparse.rs`

## Corpus-Adaptive Bigram Frequency Table

The quality of sparse n-gram decomposition depends on accurate bigram frequencies. Using generic English or code frequencies leads to suboptimal n-gram boundaries for specific corpora.

fast-grep computes bigram frequencies **from the actual corpus** during index build:

1. During the file-scanning phase, count occurrences of all 256x256 byte pairs
2. Normalize to [0, 1] range
3. Use these frequencies for n-gram boundary detection during both indexing and querying

This means the n-gram decomposition adapts to the language and style of the codebase — a C kernel codebase will produce different n-gram boundaries than a JavaScript web app.

The frequency table is stored in `meta.json` alongside the index and loaded at query time to ensure consistent decomposition.

Implementation: `src/freq_real.rs`, `src/sparse.rs`

## Position Masks (Blackbird Algorithm)

Even with good n-gram selection, false positives occur when a document contains all required n-grams but not in the right order or adjacency. The Blackbird algorithm adds two 8-bit bloom filters per (n-gram, document) entry:

### `loc_mask: u8` — Position filter
- For each occurrence of the n-gram in the document at byte position `pos`, set bit `pos % 8`
- When checking two consecutive n-grams T1 and T2 in a query, verify that `(loc_mask(T1) << len(T1)) & loc_mask(T2) != 0`
- This ensures the n-grams can appear at adjacent positions (modulo 8)

### `next_mask: u8` — Successor character filter
- For each occurrence of the n-gram, set bit `next_char % 8` where `next_char` is the byte immediately following the n-gram
- When checking consecutive n-grams T1, T2: verify that `next_mask(T1)` has bit `T2[0] % 8` set
- This ensures the character bridging the two n-grams is consistent

Both filters are bloom filters with 8 bits, so false positive rate per filter is approximately `(1 - (7/8)^k)` where `k` is the number of distinct positions/successors. For most n-grams in most documents, `k` is small (1-3), giving a per-filter false positive rate of ~12-33%.

Combined, the two filters reduce false positives dramatically — especially for common n-grams like those found in `printk` where the unfiltered candidate set is large.

Implementation: `src/index.rs`

## Persistent Index (Lookup Table + mmap Postings)

The index is stored as two binary files designed for fast cold-start loading:

### `ngrams.lookup` — Sorted hash table
```
[hash_u32][offset_u64][len_u32] × N entries
```
- Loaded entirely into memory (~few MB for 81k files)
- Binary search on `hash_u32` to find the posting list location
- Each entry points to an offset and length in the postings file

### `ngrams.postings` — Concatenated posting lists
```
[roaring_bitmap_bytes][position_mask_bytes] per n-gram
```
- Memory-mapped with `mmap` — the OS pages in only the posting lists that are actually accessed
- Roaring bitmaps are deserialized lazily from the mmap'd region
- Position masks are stored inline after each bitmap

### `docids.bin` — Document ID to path mapping
```
[len_u16][path_bytes] per document
```

### `meta.json` — Index metadata
- Version, document count, root directory
- Per-file modification times for staleness detection
- Corpus bigram frequency table

Load time is ~18ms regardless of corpus size — dominated by reading the lookup table into memory. The postings file is never fully read; only the pages containing accessed posting lists are faulted in by the OS.

Implementation: `src/persist.rs`

## Rayon Parallel Verification

After the index reduces candidates to a small set of files, each file must be opened and searched with the full regex pattern. This is the bottleneck (~86% of total query time).

fast-grep parallelizes verification using Rayon's work-stealing thread pool:

1. Collect candidate file paths from bitmap intersection
2. `par_iter()` over candidates with automatic chunk sizing
3. Each thread: mmap the file → run regex/literal matcher → collect matches
4. Results are gathered via `Mutex<Vec<Match>>` (contention is low since match collection is rare relative to scanning)

The work-stealing scheduler automatically balances load across cores — large files that take longer to scan don't block other threads from processing smaller files.

Implementation: `src/searcher.rs`

## SIMD Literal Pre-filter

For patterns that are plain literals or contain long literal substrings, fast-grep bypasses the regex engine entirely:

### Pure literals
- Detected by `is_literal()` — no regex metacharacters
- Searched with `memchr::memmem::Finder` which uses SIMD (SSE2/AVX2 on x86, NEON on ARM) for substring search
- 2-5x faster than the regex engine for literal patterns

### Regex with literal prefix/suffix
- `extract_longest_literal()` pulls the longest literal substring from the regex
- If the literal is >= 3 bytes, it's used as a pre-filter: scan with `memmem` first, then run the regex only on lines containing the literal
- Patterns like `EXPORT_SYMBOL\(.*\)` get the full `EXPORT_SYMBOL(` as a literal pre-filter

### Multi-pattern literal search
- When multiple literal n-grams are extracted, Aho-Corasick automaton is used
- SIMD-accelerated multi-pattern matching via the `aho-corasick` crate
- Particularly effective for patterns decomposed into several literal fragments

Implementation: `src/searcher.rs`

## Incremental Updates

Rebuilding the entire index after a few file changes is wasteful. fast-grep supports incremental updates:

1. **Staleness detection:** On load, compare file modification times in `meta.json` against the filesystem
2. **Changed file identification:** Files with newer mtimes are flagged for re-indexing
3. **Selective re-index:** Only the changed files are re-scanned and their posting list entries updated
4. **Bitmap patching:** Document IDs for changed files are cleared from all posting lists, then new entries are inserted

Performance: updating 10 modified files takes ~707ms vs ~53s for a full rebuild — **75x faster**.

New files are added with new document IDs. Deleted files have their entries cleared but their document IDs are not reused (to avoid index compaction complexity). Over many incremental updates, a periodic full rebuild reclaims unused IDs.

Implementation: `src/persist.rs`, `src/cli.rs`
