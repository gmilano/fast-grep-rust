# fast-grep + OpenCode

## What this does

Configures OpenCode to use fast-grep for repository searches, reducing both
search latency and output token usage.

## Recommended command

```bash
fgr --agent "$PATTERN" "$PATH" --index .fgr --max-results 100
```

## Configuration

In your OpenCode agent configuration:

```yaml
tools:
  search:
    command: "fgr --agent --index .fgr"
    args:
      - pattern: "$PATTERN"
      - path: "$PATH"
    max_results: 100
```

## Falling back to ripgrep

```yaml
tools:
  search:
    command: "rg --no-heading -n"
```

## Limits

- Index requires `fgr index . --output .fgr` (one-time, ~60s for large repos)
- Incremental update: `fgr update --index .fgr`
- Token estimates in `--agent-stats` are approximate (4 bytes/token heuristic)
