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
fgr index . --output .fgr
```

3. Add to `.claude/settings.json` in your project:

```json
{
  "tools": {
    "grep": {
      "command": "fgr --agent --index .fgr",
      "description": "Fast indexed code search with agent-optimised output"
    }
  }
}
```

## Recommended command

```bash
fgr --agent "$PATTERN" "$PATH" --index .fgr --max-results 100 --max-files 20
```

The `--max-results` and `--max-files` limits protect against accidental context
exhaustion on broad patterns.

## Without an index

If you haven't built an index, drop `--index .fgr`:

```bash
fgr --agent "$PATTERN" "$PATH" --max-results 100
```

This is faster than ripgrep for repeated searches even without an index due to
SIMD pre-filtering, but will not have the 10–25× speedup of indexed mode.

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

- Case-insensitive search (`-i`) bypasses the index and runs a full scan.
- The index must be rebuilt (or updated with `fgr update`) when files change significantly.
- Token estimates from `--agent-stats` use a 4-bytes/token heuristic, not Claude's tokenizer.
