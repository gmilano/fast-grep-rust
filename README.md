# fast-grep

**Context-efficient code search for coding agents.**

fast-grep reduces both costs of repository search:

- **Search latency**: indexed searches avoid repeatedly scanning the entire repository.
- **Context usage**: agent-oriented output avoids repeating paths and unnecessary formatting.

```bash
fgr --agent "process_request" .
```

---

## Why coding agents need a different grep

Every repository search costs an agent twice:

1. The agent **waits** for the search tool to finish scanning.
2. The search **output consumes model context** — tokens that could be used for reasoning.

fast-grep is designed to reduce both.

---

## Use-case guide

| Use case | Recommended tool |
|---|---|
| One-off search in a small repository | ripgrep |
| Repeated searches in a large repository | fast-grep |
| Coding-agent tool calls | `fast-grep --agent` |
| Scripts requiring grep-compatible output | `fast-grep --format grep` |
| Structured tool integration | `fast-grep --format jsonl` |

---

## Agent output

Traditional grep output repeats the file path on every line:

```text
src/api/request.rs:143:fn process_request(ctx: &Context) {
src/api/request.rs:189:    process_request(&ctx);
src/api/request.rs:214:    let result = process_request(&ctx);
src/api/handler.rs:67:fn process_request(req: Request) -> Response {
src/api/handler.rs:102:    process_request(incoming)?;
```

fast-grep agent output prints the path once per file:

```text
src/api/request.rs
143: fn process_request(ctx: &Context) {
189:     process_request(&ctx);
214:     let result = process_request(&ctx);

src/api/handler.rs
67: fn process_request(req: Request) -> Response {
102:     process_request(incoming)?;
```

For a 5-match, 2-file result like the above, the difference is roughly:
- grep format: ~190 bytes (~48 tokens)
- compact format: ~160 bytes (~40 tokens)

On searches returning hundreds of matches across dozens of files, the savings compound significantly. Use `--agent-stats` to measure on your actual queries.

---

## Benchmarks — Linux kernel 6.6 (81,690 files)

**Apple M1 Pro, 32 GB RAM — warm cache**

### Search latency (indexed vs full scan)

| Pattern | fast-grep (indexed) | ripgrep (no index) | Speedup |
|---------|--------------------|--------------------|---------|
| `TODO` | **97ms** | 2,463ms | **25x** |
| `printk` | **172ms** | 2,492ms | **14x** |
| `EXPORT_SYMBOL` | **197ms** | 1,553ms | **8x** |
| `container_of` | **344ms** | 2,440ms | **7x** |
| `static.*inline` | **394ms** | 2,369ms | **6x** |

**Note**: ripgrep does not use an index. These comparisons are meaningful for repeated searches
in large, stable repositories. For a first-time search or a small repo, ripgrep is faster.

### vs ugrep (also indexed)

| Pattern | fast-grep | ugrep | Speedup |
|---------|-----------|-------|---------|
| `EXPORT_SYMBOL` | **197ms** | 1,898ms | **9.6x** |
| `TODO` | **97ms** | 599ms | **6.2x** |
| `static.*inline` | **394ms** | 1,595ms | **4.0x** |
| `printk` | **172ms** | 645ms | **3.8x** |
| `container_of` | **344ms** | 656ms | **1.9x** |

### Index cost

| Metric | Value |
|--------|-------|
| Full build | ~60s (one-time) |
| Incremental update | <1s for 10–100 changed files |
| Index load (mmap) | 17ms |
| Index size (postings) | 775 MB |
| Index size (bitmaps) | 161 MB |
| RAM at query time | ~22 MB (rest is mmap'd) |

---

## Install

```bash
git clone https://github.com/gmilano/fast-grep-rust
cd fast-grep-rust
cargo build --release
```

Binary: `./target/release/fgr`. SIMD (AVX2/NEON) is auto-enabled via `.cargo/config.toml`.

---

## Usage

Searching is the default — pass PATTERN and PATH as positional args. Other
operations (`index`, `update`, `compact`, `bench`, `stats`, `daemon`) are
subcommands.

```bash
# Index a codebase (one-time, ~60s for the Linux kernel)
fgr index /path/to/codebase --output .fgr

# Agent-optimised search (compact output, relative paths)
fgr --agent "process_request" /path/to/codebase --index .fgr

# Agent-aggressive (compact + trim long lines at 200 chars)
fgr --agent-aggressive "TODO" /path/to/codebase --index .fgr

# Structured JSON output
fgr --format json "process_request" /path/to/codebase --index .fgr

# JSONL streaming output (one match per line)
fgr --format jsonl "process_request" /path/to/codebase --index .fgr

# Full scan (no index, ripgrep-equivalent)
fgr "EXPORT_SYMBOL" /path/to/codebase

# With context lines
fgr --agent "process_request" . --context 3

# With output limits (safe for agent context budgets)
fgr --agent "TODO" . --max-results 50 --max-files 10

# Print agent stats to stderr
fgr --agent-stats --agent "process_request" . --index .fgr

# Rebaseline: fold the accumulated delta back into the primary index
fgr compact --index .fgr

# Incremental index update
fgr update --index .fgr

# Benchmark against ripgrep + output metrics
fgr bench "static.*inline" /path/to/codebase

# Index stats (also shows delta / tombstone divergence + "Compaction due")
fgr stats --index .fgr
```

### Keeping the index fresh: delta, then rebaseline

An `fgr update` doesn't rewrite the primary index — it records changed files in
a small **delta** overlay (and tombstones the stale docs). Searches read the
primary + delta together, so results stay correct, but as the working tree
diverges the delta grows and every query pays a small, growing overhead.

**Compaction** folds the delta and drops the tombstones back into a fresh, dense
primary baseline. It reuses the existing postings (no re-reading or
re-trigramming of source files), re-encodes them in parallel, and overlaps the
encode with the file writes, so it is **far faster than a full rebuild** (~3s vs
~183s on the 79K-file Linux kernel). The swap is atomic and never blocks
in-flight searches (see [REBASELINE.md](REBASELINE.md) for the design).

- **Manual:** `fgr compact --index .fgr` always folds whatever is pending.
- **Automatic:** `fgr update` and the daemon rebaseline on their own once
  divergence crosses a threshold. The thresholds live in an editable
  `<index>/config.toml` (written with commented defaults on first build):

  ```toml
  [compaction]
  auto = true              # set false to only ever compact via `fgr compact`
  delta_docs_abs = 500     # compact once the live delta exceeds this many docs
  delta_docs_ratio = 0.05  # ...or this fraction of the baseline
  tombstone_ratio = 0.10   # ...or once tombstones exceed this fraction of it
  min_main_docs = 500      # never auto-compact a baseline smaller than this
  ```

  Pass `fgr update --no-compact` to skip the automatic rebaseline for one run.
  The auto-compaction cost is paid by the updater (or the daemon, off its event
  loop) — never by a search.

### What gets indexed (binaries & large files)

fast-grep indexes text files and skips binaries. Binaries are detected by
extension **and** a confirmed magic signature, so detection is not naive: a
text file misnamed `logo.png` is still indexed, while a real PNG is skipped
without reading its body. A built-in denylist covers 100+ formats (images,
audio/video, archives, executables, fonts, documents, databases, ML/data).
Extensions with no reliable magic (`bin`, `dat`, `o`, `obj`, `lzma`, `eot`,
`pyc`, `pyo`, `tar`) are decided by content: a NUL byte — or, in NUL-free
non-UTF-8 data, a high ratio of `>127` bytes — means binary, while valid UTF-8
(including CJK / accented text) is always kept.

Files larger than a size cap (default **64 MiB**) are skipped, **unless** their
extension is known-text (`.log`, `.csv`, `.txt`, source files, …) or you exempt
them. All of this is tunable per index in `<index>/config.toml`:

```toml
[index]
max_file_size_mb = 64            # skip files larger than this (0 = no limit)
always_index_extensions = []     # extra text extensions, indexed past the cap
always_index_paths = []          # relative-path globs, indexed past the cap
binary_high_byte_pct = 30        # >127-byte %% for the no-magic content check
```

These settings apply to the index. A no-index `fgr` scan skips the same known
binaries (for parity) but has no size cap — you can always grep a huge file
directly.
The same `[index]` section also bounds the peak memory of a full build:

```toml
[index]
build_buffer_mb = 256    # spill postings past this buffer, then k-way merge (0 = build in RAM)
```

`fgr index` accumulates postings in a buffer of this size; when it fills, a
sorted segment is spilled to disk, and the segments are k-way merged into the
final index at the end. Peak build RAM stays flat regardless of repository size
(on the 79K-file Linux kernel, ~3.5 GB → ~0.4 GB) with no measurable change in
build time. `fgr update` uses the same buffer, so a single large update (say the
first one after a branch switch) is bounded the same way. `0` assembles the
whole index in RAM (fastest, if it fits). The produced index is byte-identical
either way.

### Daemon mode (auto-incremental updates)

Run a background watcher that observes filesystem changes and applies
debounced incremental index updates. Searches automatically flush pending
changes before running, so the index never lags behind your edits.

```bash
# Build index and start the daemon in one step
fgr index /path/to/codebase --output .fgr --daemon

# Or start the daemon against an existing index
fgr daemon start /path/to/codebase --output .fgr

# Status / stop
fgr daemon status /path/to/codebase --output .fgr
fgr daemon stop   /path/to/codebase --output .fgr
```

The daemon debounces FS events by 3 seconds, so a burst of writes triggers a
single update. State is exchanged over a localhost TCP socket recorded in
`<index>/daemon.port`.
### Flags

| Flag | Description |
|------|-------------|
| `--agent` | Compact output: path once, then `line: text`. Lossless. |
| `--agent-aggressive` | Compact + trim lines > 200 chars. |
| `--format <fmt>` | Output format: `grep`, `compact`, `json`, `jsonl`. Highest priority. |
| `--max-results <N>` | Cap total matches emitted. |
| `--max-results-per-file <N>` | Cap matches per file. |
| `--max-files <N>` | Cap number of files in output. |
| `--max-output-bytes <N>` | Cap output size in bytes. |
| `--context <N>` / `-C <N>` | Print N lines of context around each match. |
| `--agent-stats` | Print latency and token estimates to stderr. |
| `--index <path>` | Use persistent index for fast repeated searches. |
| `--type <ext>` | Filter by file extension (`rs`, `ts`, `py`, …). |
| `--no-ignore` | Don't respect `.gitignore`. |
| `--count` / `-c` | Count matching lines per file. |
| `--files-with-matches` / `-l` | Print file paths only. |
| `--ignore-case` / `-i` | Case-insensitive search (disables index). |

### Output format precedence

When multiple format flags are combined, the priority is:

```
--format <fmt>           (highest — explicit always wins)
--agent-aggressive
--agent
FGR_FORMAT env var
default (grep format)    (lowest)
```

### JSON output schema

```json
{
  "query": "process_request",
  "root": "/absolute/path/to/repo",
  "indexed": true,
  "elapsed_ms": 172,
  "total_matches": 87,
  "files": [
    {
      "path": "src/api/request.rs",
      "matches": [
        { "line": 143, "text": "fn process_request(ctx: &Context) {" }
      ]
    }
  ],
  "truncated": null
}
```

JSONL emits one object per match for streaming:

```jsonl
{"path":"src/api/request.rs","line":143,"text":"fn process_request(ctx: &Context) {"}
```

Keys are stable across versions. New optional keys may be added in minor releases.

---

## How it works

Five techniques combine to eliminate >99% of I/O before the regex engine runs:

1. **Sparse n-grams with adaptive frequency table** — Variable-length substrings weighted by corpus-specific bigram rarity. Produces fewer, more selective posting lists than fixed trigrams.

2. **Position masks (Blackbird algorithm)** — Two 8-bit bloom filters per (n-gram, document) encode position and successor character. Drops the false positive rate to 0.42%.

3. **Persistent index with mmap** — Binary posting lists memory-mapped at query time. 17ms load regardless of corpus size; the OS pages in only the lists you touch.

4. **Line-level index with byte offsets** — Index stores line positions, not just file IDs. Verification jumps directly to candidate lines instead of scanning entire files.

5. **SIMD verification** — The `regex` crate uses Teddy SIMD when `target-cpu=native` is set; `memchr` uses AVX2/NEON for literal pre-filters.

---

## Trade-offs

fast-grep is not always faster or better.

- **The first index build has a cost** (~60s for the Linux kernel). Amortised over repeated searches, this pays off quickly.
- **The index consumes disk space** (~936 MB for 81k files). A smaller repo produces a proportionally smaller index.
- **Small repositories and one-off searches** are typically faster with ripgrep (no build cost).
- **Some regex patterns cannot be filtered by the index** — patterns with no extractable n-grams (e.g. `.*`, `\d+`) fall back to a full scan. Use `fgr stats` to check.
- **Case-insensitive search (`-i`) bypasses the index** — the index is case-sensitive; `-i` always falls back to a full scan.
- **Agent compact output reduces token overhead but does not replace semantic retrieval or ranking.** It is a formatting optimisation, not an AI feature.
- **Incremental updates detect changed files but require a full scan of those files.** Partial index merging is on the roadmap.
- **Symlinks**: followed during indexing; `is_stale()` does not detect symlink retargets.
- **Binary files**: skipped based on null-byte detection in the first 512 bytes.
- **Large files**: indexed and searched via mmap; no hard size limit, but very large files increase index size.
- **Socket security**: there is no TCP daemon in 0.4.0; searches are in-process.

---

## Integrations

See `integrations/` for Claude Code, Codex, OpenCode, Aider, and MCP setup guides.

Quick reference:

```bash
fgr integrations          # print guide to stdout
```

---

## Related work

| Project | Approach | Notes |
|---------|----------|-------|
| [ripgrep](https://github.com/BurntSushi/ripgrep) | SIMD scan, no index | Best no-index grep |
| [ugrep](https://github.com/Genivia/ugrep) | Index + scan | Previously fastest indexed grep |
| [zoekt](https://github.com/sourcegraph/zoekt) | Trigram index (Go) | Powers Sourcegraph |
| [Cursor](https://cursor.com/blog/fast-regex-search) | Sparse n-gram (closed) | Inspiration for this project |

---

## Further reading

- [Russ Cox — Regular Expression Matching with a Trigram Index](https://swtch.com/~rsc/regexp/regexp4.html)
- [Cursor — Fast Regex Search](https://cursor.com/blog/fast-regex-search)
- [Sourcegraph/zoekt](https://github.com/sourcegraph/zoekt)
- [ripgrep internals](https://blog.burntsushi.net/ripgrep/)

---

## License

MIT
