# fast-grep + Codex (OpenAI)

## What this does

Provides fast-grep as a shell tool available to Codex agents during coding tasks.
The `--agent` flag produces compact output that minimises token consumption.

## Recommended command

```bash
fgr --agent "$PATTERN" "$PATH" --max-results 100 --max-files 20
```

With a pre-built index:

```bash
fgr --agent "$PATTERN" "$PATH" --index .fgr --max-results 100
```

## JSONL output for structured tool results

If your Codex integration expects structured output:

```bash
fgr --format jsonl "$PATTERN" "$PATH" --index .fgr
```

Each line is a self-contained JSON object:

```json
{"path":"src/api/request.rs","line":143,"text":"fn process_request(ctx: &Context) {"}
```

## Falling back

Remove fast-grep from the tool definition and restore `grep -rn` or `rg`.

## Limits

- Index must be rebuilt after large file changes: `fgr update --index .fgr`
- Case-insensitive search (`-i`) is only indexed when the index was built with `fgr index -i`; otherwise it scans every indexed file
