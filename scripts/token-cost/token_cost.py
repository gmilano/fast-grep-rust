#!/usr/bin/env python3
"""Measure the token cost of fgr's output formats, as an LLM agent would ingest them.

Verifies that `fgr --format compact` (optionally `--trim`) actually reduces the
token count vs the grep-style default, using Claude-family tokenizers.

Tokenizers (auto-detected, best-effort — at least one required):
  claude    Xenova/claude-tokenizer  offline; closest public proxy for Claude's tokenizer
  tiktoken  o200k_base               offline; OpenAI GPT-4o — cross-check only (undercounts Claude)
  api       Anthropic count_tokens   ground truth; only if ANTHROPIC_API_KEY is set

Usage:
  pip install -r requirements.txt
  python token_cost.py                              # default: this repo's src/, pattern "fn "
  python token_cost.py --dir PATH --pattern PAT     # any codebase / pattern
  ANTHROPIC_API_KEY=sk-... python token_cost.py     # adds the ground-truth column

The fgr binary is found via $FGR_BIN, else target/{release,debug}/fgr[.exe] relative
to the repo, else `fgr` on PATH. Build it first: `cargo build --release`.
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
import urllib.request
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]


def find_fgr():
    env = os.environ.get("FGR_BIN")
    if env and Path(env).exists():
        return env
    name = "fgr.exe" if os.name == "nt" else "fgr"
    for cand in (REPO_ROOT / "target" / "release" / name, REPO_ROOT / "target" / "debug" / name):
        if cand.exists():
            return str(cand)
    found = shutil.which("fgr")
    if found:
        return found
    sys.exit("fgr binary not found. Run `cargo build --release` or set $FGR_BIN.")


FGR = find_fgr()

# (label, fgr args). The first entry is the baseline everything is compared against.
FORMATS = [
    ("grep (baseline)", ["--format", "grep"]),
    ("heading", ["--format", "heading"]),
    ("compact", ["--format", "compact"]),
    ("compact --trim", ["--format", "compact", "--trim"]),
    ("files-only (-l)", ["-l"]),
    ("count (-c)", ["-c"]),
]


def run(args):
    p = subprocess.run([FGR] + args, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    return p.stdout.decode("utf-8", errors="replace")


def rg_json(pattern, directory):
    rg = shutil.which("rg")
    if not rg:
        return None
    p = subprocess.run([rg, "--json", "-n", pattern, directory], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    return p.stdout.decode("utf-8", errors="replace")


def make_count_tokens(key, model="claude-opus-4-8"):
    """Anthropic /v1/messages/count_tokens via stdlib. Large inputs are chunked and
    summed (a few tokens of per-request overhead — negligible for modest scopes)."""

    def count(s):
        chunk_chars = 200_000
        total = 0
        for i in range(0, max(len(s), 1), chunk_chars):
            chunk = s[i : i + chunk_chars] or " "
            body = json.dumps({"model": model, "messages": [{"role": "user", "content": chunk}]}).encode()
            req = urllib.request.Request(
                "https://api.anthropic.com/v1/messages/count_tokens",
                data=body,
                headers={
                    "x-api-key": key,
                    "anthropic-version": "2023-06-01",
                    "content-type": "application/json",
                },
            )
            with urllib.request.urlopen(req, timeout=60) as r:
                total += json.loads(r.read())["input_tokens"]
        return total

    return count


def load_tokenizers():
    toks = {}
    try:
        from tokenizers import Tokenizer

        tk = Tokenizer.from_pretrained("Xenova/claude-tokenizer")
        toks["claude"] = lambda s: len(tk.encode(s).ids)
    except Exception as e:  # noqa: BLE001
        print(f"  [claude tokenizer unavailable: {e}]", file=sys.stderr)
    try:
        import tiktoken

        enc = tiktoken.get_encoding("o200k_base")
        toks["tiktoken"] = lambda s: len(enc.encode(s, disallowed_special=()))
    except Exception as e:  # noqa: BLE001
        print(f"  [tiktoken unavailable: {e}]", file=sys.stderr)
    key = os.environ.get("ANTHROPIC_API_KEY")
    if key:
        toks["api"] = make_count_tokens(key)
    return toks


def main():
    ap = argparse.ArgumentParser(description="Token cost of fgr's output formats.")
    ap.add_argument("--dir", default=None, help="directory to search (default: $CORPUS, else repo src/)")
    ap.add_argument("--pattern", default="fn ", help='search pattern (default: "fn ")')
    ap.add_argument("--no-rg-json", action="store_true", help="skip the rg --json reference row")
    args = ap.parse_args()

    # --dir wins, then $CORPUS (shared with scripts/bench.sh), then the repo's own src/.
    directory = args.dir or os.environ.get("CORPUS") or str(REPO_ROOT / "src")

    toks = load_tokenizers()
    if not toks:
        sys.exit("No tokenizers available — run `pip install -r requirements.txt`.")
    tnames = list(toks)

    rows = []
    for name, flags in FORMATS:
        out = run(flags + [args.pattern, directory])
        rows.append((name, out))
    if not args.no_rg_json:
        rj = rg_json(args.pattern, directory)
        if rj is not None:
            rows.append(("rg --json", rj))

    measured = []
    for name, out in rows:
        row = {"fmt": name, "bytes": len(out.encode("utf-8"))}
        for tn in tnames:
            row[tn] = toks[tn](out)
        measured.append(row)

    base = measured[0]
    print(f"\nfgr token cost   dir={directory}   pattern={args.pattern!r}")
    print(f"fgr = {FGR}")
    print(f"tokenizers = {', '.join(tnames)}\n")
    hdr = f"{'format':<18}{'bytes':>10}"
    for tn in tnames:
        hdr += f"{tn:>10}{'vs grep':>9}"
    print(hdr)
    print("-" * len(hdr))
    for r in measured:
        line = f"{r['fmt']:<18}{r['bytes']:>10}"
        for tn in tnames:
            t = r[tn]
            d = f"{round(100 * (t - base[tn]) / base[tn]):+d}%" if base[tn] else "-"
            line += f"{t:>10}{d:>9}"
        print(line)


if __name__ == "__main__":
    main()
