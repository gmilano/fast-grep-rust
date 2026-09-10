# AGENTS.md — fast-grep-rust

## What this project is

A regex search engine for codebases, implemented in Rust, aimed at coding agents.
The core idea: build a persistent **trigram inverted index with line-level
postings** over a directory so that a search touches only the lines that can
possibly match, instead of scanning every file. Without an index it falls back
to a ripgrep-style parallel scan.

The project started from the [Cursor blog post on fast regex search](https://cursor.com/blog/fast-regex-search)
(sparse n-grams + position masks). That design was replaced: the shipped engine
uses fixed trigrams, one posting per `(trigram, document, line)`, a Roaring
bitmap per trigram for document-level pre-filtering, and mmap'd persistence.

## Project structure

```
src/
├── main.rs        # Entry point, maps cli::run() to the exit status (0 match / 1 none / 2 error)
├── cli.rs         # clap-based CLI: search (default), index, update, compact, stats, bench, daemon, integrations
├── lib.rs         # Public API re-exports
├── trigram.rs     # Regex → required trigrams (alternation-aware), (?i) detection, packed trigram key
├── casefold.rs    # Unicode case folding for the case-insensitive companion index
├── filetype.rs    # File classification: known-text extensions, magic signatures, binary heuristics
├── config.rs      # <index>/config.toml: compaction thresholds, file admission, build buffer
├── index.rs       # SparseIndex: walk + per-line trigram extraction into compact postings
├── postenc.rs     # Delta-varint posting encoding (doc_id, line_no, byte_offset)
├── buildsort.rs   # Bounded (external-merge) build: spill sorted segments + k-way merge
├── persist.rs     # On-disk format, two-slot layout, load/mmap, delta update, compaction, search
├── searcher.rs    # Matcher (literal / Aho-Corasick / regex), verify, full-scan baseline
├── render.rs      # Output: grep / heading / compact / json / jsonl, context lines, caps, colour
├── daemon.rs      # FS watcher + token-authenticated localhost control socket (feature `daemon`)
└── metal/         # Optional macOS Metal literal pre-filter (FGR_METAL=1)
tests/             # Integration tests; several drive the real `fgr` binary (grep_compat,
                   # agent_surface, exit_code, daemon_auth, searcher_integration)
benches/search.rs  # Criterion benchmarks
integrations/      # Setup guides for Claude Code, Codex, OpenCode, Aider, MCP (printed by `fgr integrations`)
.claude/skills/fast-grep/SKILL.md   # Usage guide for AI coding agents
```

Design notes live next to the code: `COMPACT_POSTINGS.md` (posting encoding),
`CASE_INSENSITIVE.md` (`fgr index -i`), `REBASELINE.md` (delta → primary
compaction), `RELEASING.md`, `SECURITY.md`.

## Key algorithms

### Trigram extraction (src/index.rs, src/trigram.rs)
- Indexing: every 3-byte window of every line becomes a posting
  `(doc_id, line_no, byte_offset_of_line)`, deduplicated per `(trigram, doc, line)`.
- Querying: `decompose_pattern()` splits top-level alternation, pulls the literal
  runs out of each branch and takes their trigrams. The outer vec is OR
  (alternatives), each inner vec is AND (required trigrams). A branch with no
  literal run of ≥3 bytes (`.*`, `\d+`, `ab`) means "every file is a candidate"
  → full scan of the indexed files.
- `has_case_insensitive_flag()` detects `(?i…)`; `decompose_pattern_folded()`
  case-folds the literal runs to query the companion CI index.
- `trigram_key()` is the on-disk key: the three bytes packed into the top 24
  bits of a `u32`, low byte reserved (always 0 today) for future per-trigram
  metadata. Every key comparison masks it with `TRIGRAM_KEY_MASK`, so that byte
  can be populated later without a format bump.

### File admission (src/filetype.rs, src/config.rs)
- `config::admit_file()` decides, before reading a body, whether a file is
  indexed: extension on the binary denylist **and** a confirmed magic signature
  → skip; marker-less binary extensions (`bin`, `dat`, `o`, …) → content
  heuristic (NUL, or high `>127`-byte ratio in non-UTF-8 data); over the size
  cap (`max_file_size_mb`, default 64) and not exempt → skip. A NUL in the first
  512 bytes remains the backstop for everything read.
- The same policy gates the build, the incremental update, the stale walk and
  the no-index scan (no size cap there), so they agree on the file set.

### Posting encoding (src/postenc.rs)
- Postings in a list are sorted by `(doc_id, line_no)` and delta-encoded with a
  flagged varint (bit 7 = "same document as previous"), ~2.9 bytes per posting
  on the Linux kernel vs 16 bytes flat. `PostingWriter` encodes on the fly during
  the build so the whole decoded index is never held in RAM.

### Bounded build (src/buildsort.rs)
- Postings accumulate in a buffer of `[index] build_buffer_mb` MiB (default
  256); when it fills, the map is spilled as a sorted segment
  (`[key u32][len u32][blob]` per trigram) and cleared. After the walk the
  segments are k-way merged (`BinaryHeap` over `(key, segment)`) straight into
  `NgramFileWriter`, so the output is byte-identical to an in-RAM build
  (`0` = never spill). `update_incremental` bounds the delta build the same way.

### Two-tier search (src/persist.rs `search_timed`)
1. For each required trigram, load its **Roaring bitmap** (set of documents
   containing it) and AND them, smallest first, with early exit. Tombstoned
   docs are removed; delta docs (no bitmap entry) are added.
2. If the surviving document set is tiny (≤0.7% of the corpus, 500-doc floor)
   and there is a single alternative, verify those files directly.
3. Otherwise decode the trigrams' **line postings** — filtered by the bitmap
   when it is selective (<50% of docs) — and intersect them on `(doc, line)`.
   The result is a list of candidate `(file, line, byte_offset)` hits.
4. Main and delta stores are merged per trigram; results are deduplicated.

### Verification (src/searcher.rs)
- `Matcher` picks the cheapest engine: `memchr::memmem` for pure literals,
  Aho-Corasick for literal alternations, otherwise `regex` (with the longest
  literal as a pre-filter). The `regex` crate uses Teddy SIMD with
  `target-cpu=native`.
- Candidate hits are grouped per file and verified in parallel with Rayon.
  Density-adaptive: >10 candidate lines per file → read the whole file once
  ("file-level"), otherwise seek to each candidate line via its byte offset
  ("line-level"). Patterns that need line-by-line semantics (`^`, `$`, `\b` at
  edges) always go line-level.

### Rendering (src/render.rs, src/cli.rs)
- Per file, matches (and `-A/-B/-C` context) are rendered into a byte buffer
  through one chokepoint (`emit_line`) that knows the format: grep, heading,
  compact (`--agent`), json fragment, jsonl. `--agent-aggressive` cuts lines at
  200 chars on a UTF-8 boundary.
- Streaming dispatch when order does not matter; otherwise files are collected,
  sorted by path and drained with the caps applied (`--max-results`,
  `--max-results-per-file`, `--max-files`, `--max-output-bytes`) using per-match
  cut points, so a cut never splits a line, a UTF-8 sequence or a context block.
  A per-file cap stops scanning that file; the reported total is then a lower
  bound (`N+` / `"exact": false`).
- Exit status: `run() -> Result<bool>`; `0` matched, `1` no match, `2` error.

### Persistence (src/persist.rs)
- Index root holds `current` (name of the live slot, `slot-a`/`slot-b`),
  `config.toml`, the lock and daemon files. Content lives in the slot:
  - `ngrams.lookup`: sorted `[key_u32][offset_u64][len_u32]` entries, loaded
    into RAM; binary search by packed trigram key (reserved byte masked).
  - `ngrams.postings`: concatenated compact posting lists, mmap'd.
  - `ngrams.bitmaps` + `ngrams.bitmaps.lookup`: serialized Roaring bitmaps, mmap'd.
  - `docids.bin`: `[len_u16][path_bytes]` per document.
  - `meta.json`: version, doc/trigram counts, root, `built_at`, dir/file mtimes.
  - Delta overlay: `delta.postings`, `delta.lookup`, `delta.docids`, `deleted.bin`
    (tombstones).
  - Optional CI companion: the same four `ngrams.ci.*` files + `delta.ci.*`.
- Format version is `INDEX_VERSION` (5). An index with another version is
  rebuilt automatically on the next `--index` search.
- Staleness: `is_stale()` compares stored directory mtimes and a sample of file
  mtimes; `full_stale_check()` walks everything.

### Incremental update & compaction (src/persist.rs)
- `update_incremental()` re-indexes only changed/new files into the **delta**
  and tombstones stale docs; the primary is never rewritten.
- `compact()` folds delta + tombstones into a fresh dense primary in the
  non-live slot and flips `current` atomically (see `REBASELINE.md`).
  `maybe_auto_compact()` applies the `[compaction]` thresholds after an update.

### Daemon (src/daemon.rs, feature `daemon`, default on)
- `notify` watcher with a 3 s debounce → `update_incremental`. Control socket on
  `127.0.0.1:<random port>` recorded in `<index>/daemon.port`; command set is
  closed (`status`, `flush`, `stop`) and every command carries the per-daemon
  token from `<index>/daemon.token` (owner-only on Unix; constant-time compare;
  `error: unauthorized` otherwise). A search with `--index` asks a running
  daemon to `flush` pending changes first.

## Build

```bash
cargo build --release   # .cargo/config.toml sets target-cpu=native for SIMD
cargo test              # CI runs the suite in debug — do the same locally
cargo clippy --all-targets
cargo fmt --check       # CI gates on rustfmt
cargo bench             # requires Linux kernel at /tmp/linux-6.6 or falls back to ./
```

## Development conventions

- All public functions return `anyhow::Result<T>` — no unwrap() in library code
- Index build progress is printed to stderr only when `verbose=true`
- CLI output goes to stdout; stats/progress/summary lines to stderr
- Binary format versioned via `meta.json` `version` (`INDEX_VERSION`) — bump
  when the on-disk layout changes; old indexes are then rebuilt on first use
- Never commit an index: `.fgr/` belongs in `.gitignore`

## Known limitations

1. **Patterns without a literal run of ≥3 bytes** (`.*`, `\d+`, two-letter
   words) cannot be filtered by the index and scan every indexed file.
2. **`-v` / `--invert-match`** always uses the direct scan — the index locates
   matches, not their absence.
3. **`-i` needs the companion index.** Without `fgr index -i`, a
   case-insensitive query scans all indexed files.
4. **No lookaround / backreferences** (Rust `regex` crate).
5. **Symlinks are not followed** (`ignore::WalkBuilder` default), at index time
   or during a scan.
6. **Files over the size cap** (64 MiB by default) are not indexed unless their
   extension is known-text or exempted in `config.toml`; the no-index scan has
   no cap.
7. **Index size**: about 2.9 GB of postings + 170 MB of bitmaps for the Linux
   kernel (79,406 indexed files). The primary is mmap'd, so RAM at query time is
   small, but disk usage is real.

## Related reading

- [Russ Cox — Regular Expression Matching with a Trigram Index](https://swtch.com/~rsc/regexp/regexp4.html)
- [Cursor — Fast Regex Search](https://cursor.com/blog/fast-regex-search)
- [Sourcegraph/zoekt](https://github.com/sourcegraph/zoekt) — production trigram index in Go
- [Nelson Elhage — Regex search with suffix arrays](https://blog.nelhage.com/2015/02/regular-expression-search-with-suffix-arrays/)
- [ripgrep internals](https://blog.burntsushi.net/ripgrep/)
