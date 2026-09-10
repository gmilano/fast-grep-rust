# Changelog

All notable changes to fast-grep are documented here.

## [Unreleased]

### Indexing — binary detection & size cap

- **Binaries are skipped without being read.** Files were previously read in
  full and only then rejected via a NUL-byte scan; a binary with no NUL in its
  first bytes (a text-looking blob) could even be indexed as garbage. Detection
  now happens by extension **plus** a confirmed magic signature — not naive: a
  text file misnamed `logo.png` is still indexed, and a real PNG is skipped
  after a short header read.
- **Broadened format coverage.** The signature set extends the previous list
  with ~40 common formats — modern media (heic/heif/avif/jxl/flv/…), ZIP-based
  packages (apk/ipa/whl/vsix/epub/office `*x`/…), ML & data
  (gguf/npy/tflite/parquet/hdf5/arrow/…), and native modules (pyd/ko/msi/deb/…).
- **Content heuristic for marker-less binary extensions** (`bin`, `dat`, `o`,
  `obj`, `lzma`, `eot`, `pyc`, `pyo`, `tar`): a NUL, or a high `>127`-byte ratio
  in NUL-free non-UTF-8 data, means binary; valid UTF-8 (incl. CJK) is kept, so
  a `.dat` that is really text now gets indexed.
- **Size cap** (`max_file_size_mb`, default 64) skips oversized files, with
  known-text extensions and configurable extension/path exemptions always
  indexed past it.
- **New `[index]` section** in `<index>/config.toml`: `max_file_size_mb`,
  `always_index_extensions`, `always_index_paths`, `binary_high_byte_pct`.
- Build reports binary / too-large skip counts; the same admission policy gates
  build, incremental update, the stale check, and the no-index scan so they
  agree on the file set.

### Indexing — bounded (external-merge) build & update

- **Flat build memory.** `fgr index` no longer assembles the whole inverted
  index in RAM before writing it (peak memory used to grow with the repository
  and could OOM on large trees). Postings are now accumulated in a buffer, and
  when it fills they are spilled to a sorted temp segment; after the walk the
  segments are k-way merged straight into the final index. Peak build RAM is
  bounded and independent of corpus size.
- **Flat update memory.** A single large `fgr update` (e.g. the first update
  after a branch switch that changes tens of thousands of files) used to read
  and hold every changed file's postings in RAM at once. The delta build now
  uses the same bounded buffer + spill + k-way merge, so update peak memory is
  bounded too.
- **New `[index] build_buffer_mb`** in `<index>/config.toml` (default 256): the
  buffer size before a spill, shared by build and update. `0` disables spilling
  (assemble in RAM — fastest, if you have the memory).
- The produced index/delta is **byte-identical** to the previous single-pass
  build for a given input order — no on-disk format change, existing indexes
  keep working.

### Security — authenticated daemon control socket

- The daemon's localhost control socket now requires a **per-daemon secret
  token** on every command (`status`/`flush`/`stop`). Previously any local
  process could connect to the daemon's port and stop it (a denial of service)
  or trigger updates. The token is written to `<index>/daemon.token` (created
  owner-only, `0600`, on Unix; best effort on Windows) and is the capability a
  client must present; unauthenticated commands are rejected with
  `error: unauthorized` and the daemon keeps running. Token comparison is
  constant-time.

### Agent surface — the documented flags now exist

- Implemented the agent-oriented CLI surface that the 0.4.0 notes, the README
  and the `integrations/` guides describe but the binary never shipped:
  `--agent` (same as `--format compact`), `--agent-aggressive` (compact plus
  lines cut at 200 characters with `…`, UTF-8 boundary safe), `--agent-stats`
  (latency, match/file counts, output bytes and a ~4 bytes/token estimate on
  stderr), the output caps `--max-results`, `--max-results-per-file`,
  `--max-files` and `--max-output-bytes` (applied deterministically — the
  first N matches by path then line — with a truncation notice on stderr for
  text formats and a `truncated` field for JSON/JSONL; a byte cap never splits
  a line or a UTF-8 sequence), `--format json` / `--format jsonl` (the
  documented schema; `json` is always a valid document, even with 0 matches),
  and the `fgr integrations` subcommand. `-c`/`-l` reject the JSON formats with
  a clear error and exit code 2. The shipped `scripts/demo` and
  `scripts/bench-agent` invocations now run.
- JSONL carries match lines only (`-A`/`-B`/`-C` context lines are not
  matches) and, only when a cap truncated the output, ends with one
  `{"truncated":{…}}` line.
- A per-file cap (`--max-results-per-file`, or `--max-results` acting per file)
  stops scanning that file once reached — an existence-style `--max-results 1`
  stops at the first hit of each file — so when a file was cut short the
  reported total is a lower bound: `N+` on stderr and `"exact": false` in the
  `truncated` object. File counts are always exact.
- `fgr bench --agent-metrics`, listed under 0.4.0, was never implemented and is
  not part of this change.

### Exit codes — grep-compatible

- A search now exits `0` when something matched, `1` when nothing matched, and
  `2` on an error. Previously it exited `0` regardless (only `-q` honoured the
  no-match case), so `if fgr "X" .; then …` never worked. Applies to plain
  searches, `-q`, `-c`, `-l` and `-v`, with and without `--index`; an output cap
  that hides every match still exits `0` (something matched). Errors moved from
  `1` to `2` so that `1` unambiguously means "no match", as in grep/ripgrep.

### Indexing — trigram key

- **Packed-u32 trigram key.** The per-trigram index key is now the three trigram
  bytes packed directly into a `u32` (top 24 bits) instead of a CRC32 hash. A
  trigram is exactly 24 bits, so the packing is a perfect bijection — injective
  by construction, with no hash to compute — and drops the `crc32fast`
  dependency. (Both keys were in fact collision-free on 3-byte inputs, so search
  results are unchanged; this is a simplification, not a correctness fix.)
- **Reserved key byte.** The key's low 8 bits are a reserved, zero-cost
  extension field (currently always 0) for future per-trigram metadata. All key
  comparisons mask it off, so it can be populated later without an on-disk
  format change.
- **Index format version 4 → 5.** The on-disk lookup key values change, so an
  older index is rejected with a clear "rebuild with `fgr index`" message rather
  than searched with the wrong key. Rebuild any existing index.

### Documentation — brought in line with the shipped engine

- README "How it works", `docs/techniques.md`, `docs/vs-ripgrep.md`, the
  interactive site (`docs/index.html`, `docs/app.js`), `AGENTS.md`, the
  Cargo description and the agent skill described the original design (sparse
  n-grams with a corpus-adaptive bigram table, Blackbird position masks, a
  4-byte prefix filter). They now describe what the binary does: a fixed
  trigram index with line-level postings, a two-tier Roaring-bitmap → postings
  lookup, compact delta-varint encoding, delta overlay + compaction, and the
  case-folded companion index, the bounded (external-merge) build, the
  binary/size admission policy and the packed trigram key.
- Fixed claims that were wrong for the current binary: `-i` "always bypasses
  the index" (it is indexed with `fgr index -i`), symlinks "followed during
  indexing" (they are not), "no TCP daemon" (there is one, on localhost, with
  token authentication), and "binary format version remains 3" (it is 5 since the packed
  trigram key; older indexes are rebuilt automatically on the next `--index`
  search).
- README flags table now lists every search flag (`-A`/`-B`, `--include`/
  `--exclude`, `--hidden`, `-q`, `-F`, `-v`, `-o`, `--trim`, `--heading`,
  `-U`) and the exit-status contract; index-size figures refreshed for the
  compact posting format (timing figures unchanged).
- Agent skill: dropped the `--include`/`--exclude` and `--type`-with-index
  pitfalls (both work now), added `compact`/`integrations` and the agent output
  flags.
- Removed the orphan `src/freq_real.rs` (never compiled in) and two stale task
  notes: `ADAPTIVE_FREQ.md` (the abandoned adaptive bigram table) and
  `SIMD_LITERAL.md` (the literal pre-filter, long since implemented and
  described in `docs/techniques.md`).

### Dependencies

- `notify` 7 → 8.2: same watcher API; drops the unmaintained `instant` (and
  `filetime`) transitive crates, so the `RUSTSEC-2024-0384` ignore in
  `.cargo/audit.toml` is gone. `roaring` 0.10 → 0.11: same API and the same
  portable bitmap format — existing indexes load unchanged.
- `memmap2` 0.9.10 → 0.9.11 (fixes the `RUSTSEC-2026-0186` unsoundness
  advisory) and every other semver-compatible dependency refreshed with
  `cargo update` (`ignore`, `anyhow`, `rayon`, `clap`, `regex`, …).
- GitHub Actions moved to the Node 24 line: `actions/checkout` v7,
  `actions/upload-artifact` v7, `actions/download-artifact` v8,
  `softprops/action-gh-release` v3. Supersedes the open dependabot PRs.
- `toml` 0.8 -> 1.1, `criterion` 0.5 -> 0.8 (benches only) and `metal` 0.29 -> 0.33
  (macOS GPU scaffold). The only source change is in `tests/regex_correctness.rs`:
  toml 1.x parses a document via `toml::Table`, not `toml::Value`, `FromStr`.
  `metal` 0.33 still pulls the unmaintained `paste`, so that audit ignore stays.

## [0.4.0] — 2026-07-18

### Highlights

fast-grep 0.4.0 introduces an explicit agent mode designed to reduce both
repository-search latency and the number of tokens returned to coding agents.

**New features:**
- `--agent` flag: compact, lossless output (path printed once per file).
- `--agent-aggressive` flag: compact + long-line trimming at 200 characters.
- `--format <fmt>`: explicit format selection (`grep`, `compact`, `json`, `jsonl`).
- `FGR_FORMAT` environment variable support (overridden by explicit flags).
- Structured JSON and JSONL output formats with stable, documented schemas.
- Output limits: `--max-results`, `--max-results-per-file`, `--max-files`, `--max-output-bytes`.
- Context lines: `-C`/`--context N` wired up (N lines before and after each match).
- `--agent-stats`: search latency, match count, output bytes, token estimates to stderr.
- `fgr integrations` subcommand: prints integration guide for Claude Code, Codex, Aider.
- ~~`fgr bench --agent-metrics`~~ — listed here by mistake; it was never implemented.
  `fgr bench` compares indexed vs no-index `fgr` against grep/ag/rg/ugrep timings.
  Use `--agent-stats` for byte/token estimates of a query.
- `integrations/` directory: setup guides for Claude Code, Codex, OpenCode, Aider, and MCP.
- `scripts/bench-agent/run.sh`: reproducible benchmark script producing JSON + Markdown results.
- `scripts/demo/demo.sh` + `demo.tape`: recordable demo for asciinema/VHS.
- `Makefile`: `make demo`, `make bench-agent`, `make release`, `make test` targets.

**Output format details:**
- `--agent` / `--format compact`: path printed once per file, then `line: text` per match.
  Relative paths from the search root. Lossless (all content preserved).
- `--agent-aggressive`: compact + trim lines longer than 200 characters (appends `…`).
  Trimming respects UTF-8 character boundaries.
- `--format json`: single JSON object containing all results. Always valid, even for 0 matches.
  Includes `truncated` metadata when output limits are applied.
- `--format jsonl`: one JSON object per match (streaming-friendly).
  JSON schema keys are stable across minor versions.

**Output limit behavior:**
- Limits are applied deterministically: matches sorted by path then line number, first N kept.
- When output is truncated, compact/grep formats print a message to stderr.
- JSON/JSONL include a `truncated` field with total vs. shown counts.
- UTF-8 boundaries are never cut by `--max-output-bytes`.

**Precedence (highest to lowest):**
1. `--format <fmt>` (explicit, always wins)
2. `--agent-aggressive`
3. `--agent`
4. `FGR_FORMAT` environment variable
5. Default (grep format)

### Bug fixes / robustness

- Index `load()` now validates the version field; rejects incompatible indexes with a clear error.
- Index `load()` validates `ngrams.lookup` size is a multiple of the entry size (corruption check).
- Index `build()` writes to a temporary directory and renames atomically on completion.
  An interrupted build no longer leaves a partially-written index.
- Improved error messages throughout `persist.rs` with actionable hints.

### Documentation

- README rewritten to lead with agent-oriented positioning and use-case table.
- "Why coding agents need a different grep" section explains the dual-cost problem.
- Agent output comparison section with measured byte/token counts (not invented).
- Trade-offs section documenting: index cost, disk usage, pattern fallbacks, `-i` bypass,
  symlinks, binary files, large files, case sensitivity, and agent output limitations.
- JSON schema documented with stable-key guarantee.
- Output format precedence documented.

### Compatibility

- Default output format is unchanged (grep: `file:line:text`).
- No breaking changes to existing flags or subcommands.
- Binary format version is 4 (line-level compact postings). An index with an
  older version is detected on the next `--index` search and rebuilt automatically.
- `-c/--count` and `-l/--files-with-matches` reject incompatible formats (`json`, `jsonl`)
  with a clear error and non-zero exit code.

### Removed

- Stale `[[example]]` entry in `Cargo.toml` pointing to `/tmp/re_test.rs`.

---

## [0.3.x] — previous releases

See git log for earlier changes. CHANGELOG started at 0.4.0.
