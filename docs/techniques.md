# Techniques

Detailed description of the algorithms and optimizations used in fast-grep.

## Trigram Index with Line-Level Postings

Classical code search indexes (zoekt, Google Code Search) decompose queries into
fixed-length trigrams (3-byte substrings) and keep, per trigram, the set of
*files* that contain it. That tells you which files to open, but not where to
look inside them — a common trigram like `int` still forces a scan of every
candidate file.

fast-grep keeps trigrams but changes what a posting is. Instead of a file id, a
posting is a **line**:

```
(doc_id, line_no, byte_offset_of_line)
```

Indexing walks every line of every file and records one posting per distinct
`(trigram, document, line)`. Querying decomposes the pattern into the trigrams
any match must contain (literal runs of each alternation branch, see
`src/trigram.rs`), fetches their posting lists and intersects them on
`(doc_id, line_no)`. What survives is not "files that might match" but
"lines that contain every required trigram" — verification then reads only
those lines.

Patterns with no literal run of at least 3 bytes (`.*`, `\d+`, `ab`) cannot be
decomposed; they scan every indexed file.

Implementation: `src/index.rs`, `src/trigram.rs`

An earlier design (sparse n-grams with corpus-adaptive bigram weights, plus
8-bit position/successor masks, after the Cursor blog post) was replaced by this
one: line-level postings make the extra filters unnecessary and put the
selectivity where it pays — at verification.

## Compact Posting Encoding

Line-level postings are numerous (~1 billion for the Linux kernel). Stored flat
they would cost 16 bytes each; fast-grep delta-encodes them to ~2.9 bytes.

Within a posting list, postings are sorted by `(doc_id, line_no)`. Each posting
is written as:

1. a **flagged varint** — bit 7 says "same document as the previous posting";
   if set the payload is `line_no - prev_line`, otherwise it is
   `doc_id - prev_doc` followed by the absolute `line_no`;
2. a varint `byte_offset` (delta within a document, absolute at a document
   boundary).

About 88% of postings stay within the same document, so the common case is one
byte for doc+line. Postings are encoded on the fly during the build, so the
decoded index is never materialized in RAM.

Implementation: `src/postenc.rs` (design note: `COMPACT_POSTINGS.md`)

## Bounded Build (External Merge Sort)

Even compact-encoded, the whole inverted index of a large tree does not fit
comfortably in RAM while it is being built. `fgr index` therefore never holds
it all:

1. Postings accumulate in the trigram map until the encoded bytes reach the
   `[index] build_buffer_mb` budget (default 256 MiB).
2. The map is then **spilled** to a sorted temp segment —
   `[key u32][len u32][blob]` per trigram, keys ascending — and cleared.
3. After the walk, the segments are **k-way merged** (a min-heap over the head
   key of each segment) straight into the final `ngrams.*` files, one trigram at
   a time.

Because the spill happens at document boundaries and each trigram's postings
are already in document order, the merged output is **byte-identical** to an
in-RAM build (`build_buffer_mb = 0`). On the Linux kernel this takes peak build
RAM from ~3.5 GB to ~0.4 GB with no measurable change in build time. The
incremental update's delta build uses the same buffer, so a very large update
is bounded too.

Which files enter the build is decided before any body is read: a binary
denylist confirmed by magic signature, a content heuristic for marker-less
binary extensions, and a size cap with known-text/config exemptions
(`src/filetype.rs`, `config::admit_file`). The same policy gates the update,
the stale check and the no-index scan.

Implementation: `src/buildsort.rs`, `src/filetype.rs`

## Two-Tier Lookup: Roaring Bitmaps, then Line Postings

Decoding the full line postings of a common trigram is expensive. The index
therefore stores, per trigram, a second structure: a **Roaring bitmap** of the
documents containing it.

A query runs in two tiers:

1. **Document tier.** Load the bitmaps of all required trigrams and AND them,
   smallest first, stopping as soon as the intersection is empty. Tombstoned
   (deleted/replaced) documents are removed; documents in the delta overlay are
   added since they have no bitmap entry.
   - If the surviving set is tiny (≤0.7% of the corpus, with a 500-document
     floor) and the pattern has a single alternative, the files are verified
     directly — no posting decode at all.
2. **Line tier.** Otherwise decode the line postings. When the bitmap is
   selective (<50% of documents) the decoder skips postings of documents outside
   it; when it is not, a full decode is faster. Lists are intersected on
   `(doc_id, line_no)`, smallest first.

Alternation branches are processed independently and their hits unioned, then
deduplicated on `(doc_id, line_no)`.

Implementation: `src/persist.rs` (`search_timed`)

## Persistent Index (Lookup Tables + mmap'd Stores)

The index directory holds a `current` pointer naming the live **slot**
(`slot-a`/`slot-b`), `config.toml`, a lock and the daemon files. Inside the
slot:

### `ngrams.lookup` — sorted key table
```
[key_u32][offset_u64][len_u32] × N entries
```
- Loaded entirely into memory (a few MB for 80k files)
- Binary search on the 32-bit trigram key: the three trigram bytes packed into
  the top 24 bits (`b0<<24 | b1<<16 | b2<<8`) — injective by construction, no
  hash — with the low byte reserved (always 0 today) for future per-trigram
  metadata; readers mask it, so populating it later needs no format bump

### `ngrams.postings` — concatenated compact posting lists
- Memory-mapped; the OS pages in only the lists a query touches

### `ngrams.bitmaps` + `ngrams.bitmaps.lookup` — Roaring bitmaps
- Serialized bitmap per trigram, mmap'd and deserialized lazily; same lookup
  layout as above

### `docids.bin` — document id → path
```
[len_u16][path_bytes] per document
```

### `meta.json` — metadata
- Format version (`INDEX_VERSION`, currently 5 — an index with another version
  is rebuilt automatically on the next `--index` search)
- Document and trigram counts, root directory, build timestamp
- Directory and file mtimes for staleness detection

### Delta overlay and tombstones
- `delta.postings` / `delta.lookup` / `delta.docids`: postings of files
  re-indexed since the baseline; `deleted.bin`: tombstoned document ids

### Case-insensitive companion (optional, `fgr index -i`)
- `ngrams.ci.*` and `delta.ci.*`: the same structures over case-folded lines,
  sharing the document ids and byte offsets (design note: `CASE_INSENSITIVE.md`)

Load time is dominated by reading the lookup tables; the postings and bitmap
files are never read in full.

Implementation: `src/persist.rs`

## Rayon Parallel Verification

After the index reduces the query to candidate lines, each file must be opened
and the full pattern applied. This is the bulk of query time.

fast-grep groups the candidate hits by file and verifies in parallel with
Rayon's work-stealing pool. The strategy adapts to **density** (candidate lines
per file):

- **Line-level** (≤10 lines/file): mmap the file and seek to each candidate
  line via its stored byte offset; only those lines are matched.
- **File-level** (>10 lines/file): read the file once and match it whole —
  cheaper than many seeks when a file is dense with candidates.

Patterns whose semantics depend on line boundaries (`^`, `$`, edge `\b`) always
use the line-level path.

Implementation: `src/searcher.rs`

## SIMD Literal Pre-filter

For patterns that are plain literals or contain long literal substrings,
fast-grep bypasses the regex engine where it can:

### Pure literals
- Detected by `is_literal()` — no regex metacharacters
- Searched with `memchr::memmem::Finder`, which uses SIMD (SSE2/AVX2 on x86,
  NEON on ARM)

### Literal alternations
- `TODO|FIXME|HACK` is matched with an Aho-Corasick automaton (`aho-corasick`
  crate) instead of the regex engine

### Regex with a literal core
- `extract_longest_literal()` pulls the longest literal substring from the
  regex; if it is ≥3 bytes it gates the regex — lines without the literal are
  never handed to the engine
- `EXPORT_SYMBOL\(.*\)` gets `EXPORT_SYMBOL(` as its pre-filter

The `regex` crate itself uses the Teddy SIMD matcher when built with
`target-cpu=native` (set in `.cargo/config.toml`).

Implementation: `src/searcher.rs`

## Incremental Updates and Rebaseline

Rebuilding the entire index after a few edits is wasteful. `fgr update` (and
the daemon) apply a **delta overlay** instead:

1. **Change detection:** compare the directory and file mtimes stored in
   `meta.json` against the filesystem.
2. **Re-index only changed files** into the delta store (`delta.*`), and
   **tombstone** the stale primary documents in `deleted.bin`. The primary is
   never rewritten.
3. **Search reads both:** per trigram, primary and delta postings are merged;
   tombstoned documents are filtered out.

Performance: updating 10 modified files takes ~707ms vs ~53s for a full rebuild
— **75x faster**.

The delta only grows, so **compaction** (`fgr compact`, or automatically once
the `[compaction]` thresholds in `config.toml` are crossed) folds delta and
tombstones into a fresh, dense primary. It re-encodes the existing postings
(no re-reading of source files) into the non-live slot and flips `current`
atomically, so in-flight searches are never disturbed. See `REBASELINE.md`.

Implementation: `src/persist.rs`, `src/cli.rs`, `src/daemon.rs`
