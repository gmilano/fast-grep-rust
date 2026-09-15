# fast-grep + Claude Code

## What this does

Replaces Claude Code's default grep tool with fast-grep's `--agent` mode.
Results use compact output (path once per file, then line numbers), which
reduces token usage compared to the standard `file:line:text` format.

## Installation

1. Build fast-grep and put `fgr` on your PATH:

```bash
cargo build --release
cp target/release/fgr /usr/local/bin/fgr
```

2. Build the index for your project (one-time, run from your repo root):

```bash
fgr index .
```

Searches pick that `.fgr` up on their own — from the repo root or any
subdirectory, with no flag in the tool definition — and refresh it when the
tree has moved on, so the configuration below is the same whether or not the
project is indexed.

3. Add to `.claude/settings.json` in your project:

```json
{
  "tools": {
    "grep": {
      "command": "fgr --agent",
      "description": "Fast indexed code search with agent-optimised output"
    }
  }
}
```

## Recommended command

```bash
fgr --agent "$PATTERN" "$PATH" --max-results 100 --max-files 20
```

The `--max-results` and `--max-files` limits protect against accidental context
exhaustion on broad patterns.

## Without an index

The same command works in a project that was never indexed: with no `.fgr` to
find, fast-grep scans the tree directly. That is faster than ripgrep for
repeated searches thanks to SIMD pre-filtering, but without the 10–25×
speedup indexed mode gives you.

## Falling back to ripgrep

To disable fast-grep and return to the default, remove the `"grep"` entry from
`.claude/settings.json`, or set it back to `rg`:

```json
{
  "tools": {
    "grep": {
      "command": "rg --no-heading --line-number"
    }
  }
}
```

## Limits

- Case-insensitive search (`-i`) is only indexed when the index was built with `fgr index -i`; otherwise it scans every indexed file.
- Edits are folded in by the search that first notices them; `fgr daemon start .` moves that cost off the search path.
- Token estimates from `--agent-stats` use a 4-bytes/token heuristic, not Claude's tokenizer.
