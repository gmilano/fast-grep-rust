# fast-grep + Aider

## What this does

Replaces aider's default grep with fast-grep, reducing search latency and
token overhead when aider searches your repository for context.

## Configuration

In `.aider.conf.yml` (project root) or `~/.aider.conf.yml` (global):

```yaml
# Use fast-grep with agent output for repository searches
grep: "fgr --agent"
```

## Building the index

```bash
fgr index .
```

Run once per project — searches find the `.fgr` from the repo root or any
subdirectory and keep it up to date as you edit, so the `grep:` line above
stays the same.

## Falling back

Remove the `grep:` line from `.aider.conf.yml` to restore aider's default.

## Limits

- Case-insensitive patterns (`-i`) are only indexed when the index was built with `fgr index -i`; otherwise they scan every indexed file
- Edits are folded in by the search that first notices them; `fgr daemon start .` moves that cost off the search path
- Token estimates from `--agent-stats` use ~4 bytes/token, not aider's tokenizer
