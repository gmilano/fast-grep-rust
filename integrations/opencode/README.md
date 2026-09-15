# fast-grep + OpenCode

## What this does

Configures OpenCode to use fast-grep for repository searches, reducing both
search latency and output token usage.

## Recommended command

```bash
fgr --agent "$PATTERN" "$PATH" --max-results 100
```

## Configuration

In your OpenCode agent configuration:

```yaml
tools:
  search:
    command: "fgr --agent"
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

- Index it once with `fgr index .` (~60s for large repos); searches find that
  `.fgr` from the search path or any parent, so the command above is the same
  indexed or not
- Edits are folded in by the search that first notices them; `fgr daemon start .`
  moves that cost off the search path
- Token estimates in `--agent-stats` are approximate (4 bytes/token heuristic)
