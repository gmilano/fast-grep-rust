# token-cost — how many tokens does fgr's output cost an LLM agent?

When an agent (Claude Code, etc.) runs a grep-style tool, the tool's stdout is fed
back into the model as **tokens** — which cost money and context window. This harness
measures the token cost of fgr's output formats so you can verify that
`fgr --format compact` really is cheaper than the grep-style default.

It runs fgr in each format, captures the exact bytes an agent would receive, and
counts tokens with a Claude-family tokenizer.

## Run it

```sh
cargo build --release                      # from the repo root, so the binary exists
pip install -r requirements.txt

python token_cost.py                       # defaults: search this repo's src/ for "fn "
python token_cost.py --dir /path/to/code --pattern "TODO"
CORPUS=/path/to/code python token_cost.py  # reuse the same $CORPUS as scripts/bench.sh
```

The search directory is resolved as: `--dir` (if given) → `$CORPUS` → this repo's `src/`.

Example (searching this repo's `src/`):

```
format                 bytes    claude  vs grep  tiktoken  vs grep
------------------------------------------------------------------
grep (baseline)        ...       ...       +0%      ...       +0%
heading                ...       ...      -NN%      ...      -NN%
compact                ...       ...      -NN%      ...      -NN%
compact --trim         ...       ...      -NN%      ...      -NN%
files-only (-l)        ...       ...      -NN%      ...      -NN%
count (-c)             ...       ...      -NN%      ...      -NN%
rg --json              ...       ...     +NNN%      ...     +NNN%
```

`vs grep` is the token delta against the flat `path:line:content` baseline. `compact`
= grouped-by-file + paths relative to the search root (lossless). `--trim` additionally
strips leading indentation (lossy). `rg --json` is included as a structured-format
reference — it is dramatically *more* expensive, not less.

## Tokenizers

Counted with whichever of these is available (at least one is required):

| name       | what                                   | notes |
|------------|----------------------------------------|-------|
| `claude`   | `Xenova/claude-tokenizer` (offline)    | closest public proxy for Claude's tokenizer; **primary** |
| `tiktoken` | `o200k_base` (offline, OpenAI GPT-4o)  | cross-check only — undercounts Claude, biased on paths/code |
| `api`      | Anthropic `count_tokens` (ground truth)| **only if `ANTHROPIC_API_KEY` is set** |

Anthropic does not publish Claude 3/4's tokenizer, so the offline `claude` column is an
approximation. Trust a comparison when `claude` and `tiktoken` agree on the ranking; for
official numbers, set an API key:

```sh
ANTHROPIC_API_KEY=sk-ant-... python token_cost.py
```

`count_tokens` is free (it is not inference). Large outputs are chunked and summed, which
adds a few tokens of per-request overhead — negligible for modest scopes, but prefer a
smaller `--dir`/`--pattern` if you want the `api` column to be exact.

## Finding the binary

The harness locates fgr via `$FGR_BIN`, else `target/release/fgr[.exe]` (or `debug/`)
relative to the repo, else `fgr` on `PATH`.

## The levers, and how their payoff varies

`compact` stacks two lossless reductions, and `--trim` adds a third (lossy):

- **group-by-file** — the path is printed once per file instead of on every match line.
  Pays off in proportion to *matches per file*: large on dense in-function patterns, ~nil
  when a pattern hits roughly once per file.
- **relative paths** — drops the repeated search-root prefix. The most consistent lever;
  it shortens *every* path occurrence regardless of density.
- **`--trim`** — strips leading indentation. Zero for top-level matches (column 0), but
  meaningful for deeply-indented in-function matches; bigger as a share of the already-lean
  compact output.

You can see all of this with this tool by varying `--pattern` and `--dir`: compare a
top-level declaration (e.g. `pub fn`) against an indented in-function pattern (e.g. `return`
or `if `), and a few-files scope against a many-files one. The relative deltas move exactly
as described above.
