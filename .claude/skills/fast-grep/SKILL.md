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

`fgr` is a drop-in `grep` replacement with an optional trigram index
(line-level postings, so verification reads only candidate lines).
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
| Case-insensitive | `-i` (indexed only if the index was built with `fgr index -i`; otherwise scans every indexed file) |
| File names only | `-l` |
| Match counts | `-c` |
| Line numbers | `-n` (default on) |
| Context lines | `-A N` / `-B N` / `-C N` |
| Literal (not regex) | `-F` |
| Invert match | `-v` (always a direct scan, never the index) |
| Only matching part | `-o` |
| Filter by extension | `--type rs` (repeatable) |
| Glob filters | `--include '*.rs'` / `--exclude 'vendor/*'` (repeatable) |
| Include `.gitignore`d files | `--no-ignore` |
| Hidden files / dirs | `--hidden` |
| Use persistent index | `--index .fgr` |

Subcommands: `index`, `update`, `compact`, `stats`, `daemon`, `bench`,
`integrations`.

## Output format

Matches go to **stdout** as `path:line:content` (grep-compatible) when piped;
on a TTY they are grouped under a file heading. A trailing summary like
`Searched in 5ms, 2 matches` is written to **stderr**, so `fgr ... | wc -l`
and other pipes work the same as with `grep`.

For agent tool calls prefer the compact formats — same content, fewer tokens:

```bash
fgr --agent "PATTERN" . --index .fgr                 # path once, then `line: text`
fgr --format jsonl "PATTERN" . --index .fgr          # one {"path","line","text"} per match
fgr --agent "PATTERN" . --max-results 50 --max-files 10   # cap the output (stderr notice; JSON gets `truncated`)
fgr --agent-stats --agent "PATTERN" .                 # latency / bytes / token estimate on stderr
```

Caps are deterministic (first N matches by path, then line). A per-file cap
stops reading a file at its Nth match, so a truncated total may be a lower
bound (`N+` on stderr, `"exact": false` in JSON).

## Known pitfalls (verified on v0.4.0)

These are real behavioural quirks an agent must work around. Tracked in
upstream issue [#6](https://github.com/gmilano/fast-grep-rust/issues/6).

### 1. Exit codes are grep-compatible

`fgr` exits `0` when something matched, `1` when nothing matched, and `2` on
an error (bad pattern, unreadable index, invalid flag combination). This holds
for `-q`, `-c`, `-l` and `-v` too, so `if fgr -q "X" .; then …` works as
with `grep`/`rg`. (Earlier releases always exited `0` — if you targeted one,
drop any output-parsing workaround.)

### 2. Short or literal-less patterns scan everything

The index needs a literal run of at least 3 bytes in every alternation branch.
`ab`, `\d+`, `.*foo|x` and similar fall back to scanning every indexed file —
correct, just not fast. Prefer a longer literal when you have one.

### 3. `-i` is only fast with a companion index

The index is case-sensitive. `-i` uses the case-folded companion if the index
was built with `fgr index -i`; otherwise it scans every indexed file. If an
agent session will do many `-i` searches on a large repo, build with `-i`.

### 4. No lookaround, no backreferences

The Rust `regex` crate (which `fgr` uses) does not support `(?=...)`,
`(?<=...)`, `(?!...)`, `(?<!...)`, or `\1` backrefs. `fgr` will return a
parse error. Fall back to `rg -P` or `grep -P` for those patterns.

### 5. `.gitignore` is respected by default

Like `ripgrep`, not like `grep`. Pass `--no-ignore` to search ignored files
and `--hidden` to include dotfiles/dot-directories.

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
| Fold accumulated updates back into the baseline | `fgr compact --index .fgr` | seconds; also runs automatically past a threshold |
| Inspect (docs, delta, tombstones, stale?) | `fgr stats --index .fgr` | instant |
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
