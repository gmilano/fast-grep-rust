# fast-grep + Codex (OpenAI)

## What this does

Provides fast-grep as a shell tool available to Codex agents during coding tasks.
The `--agent` flag produces compact output that minimises token consumption.

## Recommended command

```bash
fgr --agent "$PATTERN" "$PATH" --max-results 100 --max-files 20
```

Index the project once with `fgr index .` and the same command becomes an
indexed search: the `.fgr` is found from the search path, and kept current.

## JSONL output for structured tool results

If your Codex integration expects structured output:

```bash
fgr --format jsonl "$PATTERN" "$PATH"
```

Each line is a self-contained JSON object:

```json
{"path":"src/api/request.rs","line":143,"text":"fn process_request(ctx: &Context) {"}
```

## Falling back

Remove fast-grep from the tool definition and restore `grep -rn` or `rg`.

## Limits

- Edits are folded in by the search that first notices them; `fgr daemon start .` moves that cost off the search path
- Case-insensitive search (`-i`) is only indexed when the index was built with `fgr index -i`; otherwise it scans every indexed file
