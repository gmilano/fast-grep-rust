---
name: fast-grep
description: |
  Use fast-grep (`fgr`) for regex and literal text search across this repository.
  When an index exists it is dramatically faster than `grep`/`ripgrep`; without an
  index it is comparable to ripgrep. TRIGGER when: searching for code (function,
  symbol, identifier, string), counting occurrences, or listing files matching a
  pattern in this repo. SKIP for: git-history search (use `git log -G`/`git grep`),
  binary files, or patterns that require lookaround / backreferences (the Rust
  `regex` crate does not support them — fall back to `rg -P` or `grep -P`).
---

# fast-grep (`fgr`) — agent usage guide

`fgr` is a drop-in `grep` replacement with an optional sparse n-gram index.
The CLI flags are intentionally close to `grep`/`rg`, so most habits transfer.
This skill captures the non-obvious behaviour so an agent can use `fgr`
without surprises.

## Quick decision tree

```
Need to search this repo?
├── Pattern uses lookaround / backreferences?
│   └── Yes → use `rg -P` or `grep -P` (fgr's regex engine doesn't support them)
│
├── Does ./.fgr/ exist?
│   ├── Yes → fgr "<pattern>" . --index .fgr
│   │        If results look stale (recent edits missing):
│   │          fgr update . --index .fgr   # incremental, <1s for small changes
│   │
│   └── No → How big is the repo?
│            ├── Small (< ~2000 files) → fgr "<pattern>" .   # no index needed
│            └── Large, or repeated searches expected:
│                   Ask the user before building an index
│                   (build is one-time but can take ~60s on 80k+ files).
│                   Then: fgr index . && fgr "<pattern>" . --index .fgr
```

`fgr` auto-builds an index on first use when `--index .fgr` is passed and
the directory is missing. This is convenient for small repos but **don't
rely on it for unfamiliar large trees** — the implicit ~60s build is
surprising. Confirm with the user first.

## Flag cheat-sheet (grep-compatible subset)

| Want | Flag |
|---|---|
| Case-insensitive | `-i` |
| File names only | `-l` |
| Match counts | `-c` |
| Line numbers | `-n` (default on) |
| Context lines | `-A N` / `-B N` / `-C N` |
| Literal (not regex) | `-F` |
| Invert match | `-v` |
| Only matching part | `-o` |
| Filter by extension | `--type rs` (see pitfalls) |
| Include `.gitignore`d files | `--no-ignore` |
| Use persistent index | `--index .fgr` |

Subcommands: `index`, `update`, `stats`, `daemon`, `bench`.

## Output format

Matches go to **stdout** as `path:line:content` (grep-compatible).
A trailing summary like `Searched in 5ms, 2 matches` is written to **stderr**,
so `fgr ... | wc -l` and other pipes work the same as with `grep`.

## Known pitfalls (verified on v0.3.1)

These are real behavioural quirks an agent must work around. Tracked in
upstream issue [#6](https://github.com/gmilano/fast-grep-rust/issues/6).

### 1. Exit codes are grep-compatible

`fgr` exits `0` when something matched, `1` when nothing matched, and `2` on
an error (bad pattern, unreadable index, invalid flag combination). This holds
for `-q`, `-c`, `-l` and `-v` too, so `if fgr -q "X" .; then …` works as
with `grep`/`rg`. (Earlier releases always exited `0` — if you targeted one,
drop any output-parsing workaround.)

### 2. `--include` / `--exclude` glob filters are no-ops

The flags are accepted but currently do not filter results. **Don't trust them.**
Workarounds:

- For extension filtering without an index: use `--type <ext>` (works correctly).
- For extension filtering with an index, or arbitrary globs: pipe through `awk`/`grep`:
  ```bash
  fgr "PATTERN" . --index .fgr | awk -F: '$1 ~ /\.rs$/'
  ```
- Or shell out to `find` first and feed paths to `fgr` one at a time when precision matters.

### 3. `--type <ext>` is ignored when `--index` is used

`--type rs` works on the no-index path but is silently dropped on the indexed
path. Workaround: post-filter with `awk` as above, or run without `--index`
when extension precision is required and the repo is small.

### 4. No lookaround, no backreferences

The Rust `regex` crate (which `fgr` uses) does not support `(?=...)`,
`(?<=...)`, `(?!...)`, `(?<!...)`, or `\1` backrefs. `fgr` will return a
parse error. Fall back to `rg -P` or `grep -P` for those patterns.

### 5. `.gitignore` is respected by default

Like `ripgrep`, not like `grep`. Pass `--no-ignore` to search ignored files.

### 6. Index path is relative to the indexed root

If you move or rename the repo, the existing `.fgr/` directory is invalidated.
Rebuild after moves.

## Index lifecycle

**Before the first `fgr index` in a new repo:** make sure `.fgr/` is listed
in `.gitignore`. Index files can be hundreds of MB (postings + bitmaps) and
must never be committed. If `.gitignore` is missing the entry, add it before
running `fgr index`.

| Operation | Command | Cost |
|---|---|---|
| One-time build | `fgr index . [--output .fgr]` | ~60s for 80k files |
| Incremental update after edits | `fgr update . --index .fgr` | <1s for 10–100 files |
| Inspect | `fgr stats --index .fgr` | instant |
| Auto-update on FS changes | `fgr daemon start . --output .fgr` | background process |

If a search returns no results but the user expects matches in recently-edited
files, the index may be stale — run `fgr update` before concluding the result
is correct, or suggest the daemon for active sessions.

## When *not* to use `fgr`

- Searching git history → `git log -G`, `git log -S`, `git grep <rev>`.
- Patterns with lookaround / backreferences → `rg -P` / `grep -P`.
- One-off search of a single small file → plain `grep` is simpler.
- Searching binary files (`fgr` skips them; use `grep -a` if needed).

## Worked examples

```bash
# Find all callers of a function
fgr "frobnicate\(" . --index .fgr

# Count TODOs per file
fgr -c "TODO" . --index .fgr

# Files containing a struct definition (Rust only, small repo)
fgr -l "struct Foo" . --type rs

# With context, case-insensitive
fgr -i -C 2 "panic" . --index .fgr

# After a large refactor: refresh the index
fgr update . --index .fgr
```
