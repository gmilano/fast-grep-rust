# Case-insensitive index (`fgr index -i`)

The trigram index is built from the raw bytes of source files, so a `(?i)`
query can't be answered from it: `(?i)abc` looks up the trigram `abc` and would
miss a file containing `ABC`. Historically `-i` fell back to a full scan.

`fgr index -i` builds a **second, case-folded** trigram store alongside the
case-sensitive one. An `(?i)` search then resolves entirely against the folded
store and gets the same sub-200ms acceleration as a case-sensitive search.

## Layout

One index directory holds both stores. The case-sensitive files are unchanged;
the companion mirrors them with a `ngrams.ci.` prefix:

```
ngrams.postings  ngrams.lookup  ngrams.bitmaps  ngrams.bitmaps.lookup   # CS
ngrams.ci.postings  ngrams.ci.lookup  ngrams.ci.bitmaps  ngrams.ci.bitmaps.lookup  # CI
docids.bin  meta.json                                                   # shared
delta.*  delta.ci.*                                                     # CS + CI deltas
```

`docids.bin`, the deleted-set, and the delta doc-ids are **shared** — the two
stores index the *same documents and lines*, only the trigrams differ. A CI
posting carries the **original-file** byte offset, so verification reads the
un-folded line and runs the real `(?i)` regex. `meta.case_insensitive` records
whether the companion is present; the loader mmaps `ngrams.ci.*` only then.

## Single filesystem pass

`SparseIndex.ngrams` is `HashMap<[u8;3], TrigramBuilder>`; a CI build adds
`ngrams_ci: Option<HashMap<…>>`. `add_document` reads each file once and, per
line, extracts case-sensitive trigrams into `ngrams` and — when CI is enabled —
case-folds the line and extracts folded trigrams into `ngrams_ci`. No second
read, no second walk. Build peak is ~2× the single-store peak (both packed maps
live in RAM at once); on the kernel that is 3.4 GB → 6.6 GB.

## Folding: sound vs. the regex engine

The trigram index is a *filter*: it must never drop a real match (the final
`(?i)` regex still verifies every candidate line). Soundness therefore requires
folding content **and** query literals with at least the same equivalence
classes the regex uses for `(?i)` — Unicode **simple** case folding (1:1 per
char).

A plain ASCII lowercase is **unsound**: e.g. U+212A KELVIN SIGN `K` is
`(?i)`-equal to `k` for the regex, but ASCII-lowercasing leaves it untouched, so
a line containing `K` would be dropped by the filter even though `(?i)kelvin`
matches it. `src/casefold.rs` instead maps each char to the smallest member of
its simple-case-fold class (computed via `regex-syntax`), lowercased so it
agrees with the ASCII fast path. Pure-ASCII lines (the common case) take a fast
`to_ascii_lowercase` path that yields the same representatives; only lines with
non-ASCII bytes pay the full Unicode fold. The same fold is applied to query
literals in `trigram::decompose_pattern_folded`.

Because folding is per-char and identical on both sides, the folded trigrams of
a matching substring are always a subset of the folded trigrams of the line that
contains it — no false negatives. `tests/case_insensitive.rs` checks this
end-to-end, including the Kelvin-sign case, against a full `(?i)` scan.

## Search routing

`PersistentIndex::search_timed` detects `(?i)` (`trigram::has_case_insensitive_flag`):

- CI companion loaded → decompose with the folded variant and thread a `ci`
  flag through the posting/bitmap lookups so everything resolves against
  `ngrams.ci.*`.
- No companion → fall back to scanning all live docs (the previous behaviour).

The CLI routes `-i` through the indexed path (only `-v` still forces direct
scan). A first `-i` search over a missing index auto-builds the CI companion.

## Incremental updates (lockstep)

When `meta.case_insensitive` is set, `update_incremental` builds the CI delta in
the same pass that builds the CS delta — same changed files, folded trigrams —
and writes `delta.ci.{postings,lookup}` (doc-ids shared with the CS delta). The
search merges main + delta for whichever store it is reading, so an `(?i)` query
sees edits immediately. Keeping the companion eagerly in sync (rather than
invalidating it on the first edit) is what makes the daemon path correct for
`-i`.
