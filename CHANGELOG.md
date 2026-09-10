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
- `fgr bench --agent-metrics`: compares grep-format vs compact-format bytes and token estimates.
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
- Binary format version remains 3; existing indexes are compatible.
- `-c/--count` and `-l/--files-with-matches` reject incompatible formats (`json`, `jsonl`)
  with a clear error and non-zero exit code.

### Removed

- Stale `[[example]]` entry in `Cargo.toml` pointing to `/tmp/re_test.rs`.

---

## [0.3.x] — previous releases

See git log for earlier changes. CHANGELOG started at 0.4.0.
