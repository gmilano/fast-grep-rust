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

With a pre-built index:

```yaml
grep: "fgr --agent --index .fgr"
```

## Building the index

```bash
fgr index . --output .fgr
```

Run once per project. Update after large changesets:

```bash
fgr update --index .fgr
```

## Falling back

Remove the `grep:` line from `.aider.conf.yml` to restore aider's default.

## Limits

- Case-insensitive patterns (`-i`) bypass the index
- The index reflects files at build time; stale results possible before `update`
- Token estimates from `--agent-stats` use ~4 bytes/token, not aider's tokenizer
