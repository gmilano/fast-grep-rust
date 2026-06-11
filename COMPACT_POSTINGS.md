# Compact posting format (design note)

Status: in progress on `feat/compact-postings`. Implemented incrementally; this
note is the spec and the rationale.

## Motivation

The line-level index stores one posting per `(trigram, document, line)`. On the
Linux kernel (79,406 files) that is **~1.0 billion postings at a flat 16 bytes
each → a 14.9 GB `ngrams.postings` file**, and because `persist::build` holds the
entire `HashMap<[u8;3], Vec<Posting>>` in memory before serializing, the **build
peaks at ~15 GB of RAM** (measured). The README's "775 MB postings" figure is
stale (pre line-level format); the bitmaps (162 MB) still match.

A posting is `(doc_id: u32, line_no: u32, byte_offset: u32, line_prefix: [u8;4])`.
Three observations drive the redesign:

1. **`line_prefix` is dead weight.** It feeds the "4-byte prefix filter"
   (`378ca8e`), which was **disabled** in `83a1e87` because it stores the first 4
   bytes of the *line*, not the match position, so it drops mid-line and indented
   matches (e.g. `TODO` in `// TODO:`). With it on, recall collapsed
   (TODO 73/5909). It is unsound by construction and currently unused → drop it.
2. **The other three fields are massively redundant under delta + varint.** doc/
   line/offset are sorted and monotonic within a trigram's list.
3. **Position masks (`loc_mask`/`next_mask`, `d0219a2`) were a doc-level filter**
   that avoided reading whole files. The line-level verify already reads only the
   candidate line (~80 B), so re-adding masks (+2 GB, not externalizable) isn't
   worth it at line granularity. Drop them too.

## Goals

- Shrink the on-disk index and the build RAM peak by ~5–6x.
- No perceptible search-time penalty (postings are scanned sequentially; the
  heavy doc-level intersection is on the Roaring bitmaps, untouched).
- Make a **second case-insensitive index** feasible (2 × ~2.7 GB ≈ ~5.4 GB
  instead of ~30 GB).

## The format

Per posting, the kept fields are `(doc_id, line_no, byte_offset)`. Within each
trigram's list, postings are sorted ascending by `(doc_id, line_no)`.

**Per posting (see `src/postenc.rs`):**

1. A **flagged varint** for the same-doc flag + doc/line:
   - `byte0 = [bit7 same-doc flag][bit6 continuation][bits5..0 payload]`,
     subsequent bytes standard LEB128 (7 payload bits each).
   - `flag = 1` (same doc as previous): payload = `line_no − prev_line`.
   - `flag = 0` (new document): payload = `doc_id − prev_doc` (absolute for the
     first posting), then a standard varint with the **absolute** `line_no`.
2. A standard varint for `byte_offset`: `off − prev_off` (same doc) or absolute
   (new doc).

The posting list is a length-delimited byte blob (the lookup table gives the
byte length); decode iterates until the blob is consumed — there is no fixed
stride any more.

### Why the flag bit

~88% of postings stay in the same document as the previous posting (measured).
For those the flag bit absorbs what used to be a wasted `0x00` doc-delta byte,
and the same-doc line delta is usually < 64 → a same-doc posting costs **one
byte** for doc+line in the common case.

## In-memory packed build (the RAM fix)

The win is realised at build time, not just on disk. Instead of
`HashMap<[u8;3], Vec<Posting>>` (16 B tuples), the builder keeps per trigram a
small state + an already-encoded byte buffer:

```
HashMap<[u8;3], TrigramBuilder>
  TrigramBuilder { bytes: Vec<u8>, writer: PostingWriter, count: u32 }   // see postenc::PostingWriter
```

`add_document` appends each `(trigram, line)` posting straight into the trigram's
`bytes` via the delta encoder, deduping `(trigram, doc, line)` against the
writer's last pushed posting (`PostingWriter::last_dl`). Documents and lines
arrive in ascending order, so the deltas are valid without re-sorting.
Serialization writes each `bytes` blob verbatim plus its lookup entry — no
re-encode pass. `count` is kept only for `stats()` / `avg_postings_len`.

The doc-level Roaring bitmaps (Tier 1, ~162 MB) are *not* held during the build;
they are rebuilt at serialize time by decoding each trigram's `bytes` once with a
`PostingReader` and inserting the `doc_id`s — a single cheap pass over the already
packed data, so the build keeps no bitmap state in RAM.

**Measured build peak: 3.4 GB** (down from ~12.5 GB with the Phase 1 flat
`Vec<Posting>`, ~15 GB with the original 16-byte format).

## Unchanged components

- **Lookup** (`hash → offset/len`, binary search): unchanged — blobs are now
  variable-length but offset+len still locate them.
- **Roaring bitmaps (Tier 1)** and the doc-level intersection: untouched.
- **`docids.bin`, `meta.json`**: structurally unchanged. `meta.version` bumps to
  **4**, and `IndexMeta` gains a `case_insensitive: bool` (`#[serde(default)]`,
  default `false`) reserved for the planned case-sensitive / case-insensitive
  index pair — so adding the CI index later won't need another version bump.

## Migration / stale-index handling

The posting format is incompatible with v3, so a stale index must not be read or
incrementally updated with the new code:

- **`load`** rejects any `meta.version != 4` with an error pointing at `fgr index`.
- **`run_indexed_search`** auto-(re)builds when the index is missing **or** a stale
  version (`persist::is_current`), so a search over an old index transparently
  rebuilds once instead of erroring.
- **`update_incremental`** refuses on a version mismatch. An incremental update
  only rewrites the delta; mixing a new-format delta into an old-format main index
  would silently corrupt it (the main `ngrams.postings` is never rewritten by an
  update). It errors and tells the user to rebuild rather than corrupt.

## Search integration

- `extract_line_postings` / `extract_line_postings_filtered` become a
  `postenc::PostingReader` sequential decode producing the same `(doc, line,
  offset)` tuples. `sorted_intersect_lines` / `merge_sorted_lines` are unaffected
  (they operate on decoded tuples).
- `LineHit` loses its `line_prefix` field; the disabled prefix-filter block in
  `search_persistent_timed` is removed.

## Incremental updates (`delta.postings`)

The delta index is produced by the **same builder**, so it uses the **same
compact format** for free — keeping it flat would require a separate serializer.
At query time the merge decodes both main and delta to absolute tuples and
merge-sorts; each being delta-encoded internally is irrelevant to the merge.

## Deferred / future levers

- **Drop `byte_offset` and derive line offsets by scanning newlines at verify**
  (SIMD `memchr`): would take the index from ~2.7 GB to ~1.2 GB (12x), at the
  cost of a newline scan per candidate file. Evaluate later as a space
  optimisation; for now `byte_offset` stays as a delta-varint.

## Phasing

- **Phase 1 (done):** on-disk compact format + decode, drop prefix/masks, version
  4 + stale-index handling + the `case_insensitive` meta flag. This realises the
  **disk** win (~2.7 GB) and a partial RAM win (the in-memory `Vec<Posting>` drops
  from 16 → 12 bytes/posting, ~15 → ~11 GB build peak). Validated: indexed search
  matches the full scan; full lib/ round-trip is byte-for-byte correct.
- **Phase 2 (done):** the packed in-memory build — `SparseIndex.ngrams` is now
  `HashMap<[u8;3], TrigramBuilder>`, encoding each posting into the trigram's
  `Vec<u8>` during `add_document` instead of holding `Vec<Posting>`. Bitmaps are
  decoded from the packed bytes at serialize time. **Measured** build peak
  12.5 GB → **3.4 GB**; build time 321 s → 249 s (verbatim write, no re-encode);
  on-disk and search results unchanged (EXPORT_SYMBOL 35568, PM_RESUME 39 vs the
  full scan). `update_incremental` keeps its small `Vec<Posting>` delta — no RAM
  concern there.
- **Phase 3 (done):** the dual case-sensitive / case-insensitive index. `fgr
  index -i` builds the case-sensitive store plus a case-folded companion
  (`ngrams.ci.*`) in one filesystem pass; `-i` searches resolve `(?i)` against
  the folded store, and `update` maintains a lockstep CI delta. Folding uses
  Unicode *simple* case folding (matching the regex engine) so the trigram
  filter stays sound — see [`CASE_INSENSITIVE.md`](CASE_INSENSITIVE.md).
  **Measured** on the kernel: CI postings 2.68 GB (total index ~5.75 GB), build
  peak 6.6 GB, build 347 s; indexed `-i` is byte-for-byte correct vs the `(?i)`
  full scan and ~32× faster (1.3 s vs 41 s for EXPORT_SYMBOL).

## Results (Linux kernel, 79,406 docs — measured)

| | original (16 B flat) | Phase 1 (compact disk) | Phase 2 (packed build) |
|---|---|---|---|
| `ngrams.postings` on disk | 14.9 GB | **2.7 GB** | **2.7 GB** |
| build RAM peak | ~15 GB | ~12.5 GB | **3.4 GB** |
| build time | — | 321 s | **249 s** |
| factor (disk / RAM) | 1x | 5.6x / 1.2x | **5.6x / ~4.4x** |
| dual case-insensitive index | infeasible (~30 GB) | feasible (~5.4 GB) | **feasible (~5.4 GB)** |

## Per-field measurements (from `scripts`/probe over the real 1.0 B-posting index)

| field | current | re-encoded | bytes/posting |
|---|---|---|---|
| `doc_id` | 4 B | Δvarint | 1.01 |
| `line_no` | 4 B | Δvarint | 1.12 |
| `doc_id`+`line_no` | 8 B | combined (flag bit) | **1.30** |
| `byte_offset` | 4 B | Δvarint | 1.58 |
| `line_prefix` | 4 B | dropped | 0 |

Combined compact posting ≈ **2.88 bytes** (1.30 + 1.58) vs 16 today.
