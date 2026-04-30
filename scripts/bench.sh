#!/usr/bin/env bash
# Reproducible perf bench for fast-grep.
#
# Runs hyperfine over a fixed corpus with patterns of varied match density
# (sparse / medium / dense / no-match), in both indexed and full-scan modes,
# and emits a single JSON file with all measurements + context (git sha,
# corpus path, hostname, fgr version) so two runs can be compared cleanly.
#
# Usage:
#   scripts/bench.sh                                 # writes benches/<sha>.json
#   scripts/bench.sh --output benches/baseline.json
#   FGR_BIN=/path/to/fgr CORPUS=/path/to/corpus scripts/bench.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FGR_BIN="${FGR_BIN:-$ROOT/target/release/fgr}"
CORPUS="${CORPUS:-/Users/gaston.milano/os.glob.ai/os.agents.coding/coda-desktop}"
INDEX_DIR="${INDEX_DIR:-$CORPUS/.fgr}"
OUT="${1:-}"
if [[ "${1:-}" == "--output" ]]; then OUT="${2:-}"; shift 2 || true; fi
if [[ -z "$OUT" ]]; then
  sha=$(git -C "$ROOT" rev-parse --short HEAD)
  OUT="$ROOT/benches/bench-${sha}.json"
fi

if [[ ! -x "$FGR_BIN" ]]; then
  echo "ERROR: fgr binary not found at $FGR_BIN" >&2
  echo "       run \`cargo build --release\` first or set FGR_BIN" >&2
  exit 1
fi
if [[ ! -d "$CORPUS" ]]; then
  echo "ERROR: corpus not found at $CORPUS" >&2
  exit 1
fi

echo "==> Ensuring index exists at $INDEX_DIR"
if [[ ! -f "$INDEX_DIR/meta.json" ]]; then
  "$FGR_BIN" index "$CORPUS" --output "$INDEX_DIR" >/dev/null
fi

# Patterns tuned to coda-desktop content; representative of real use.
# (Probed counts at v0.3.1 baseline: see comments.)
patterns_indexed=(
  "TODO"       # ~92 matches — sparse
  "useState"   # ~369 matches — medium
  "function"   # ~2547 matches — dense
  "import"     # ~6806 matches — very dense
  "zzznopematch" # 0 matches — index says "skip", worst case for false-positive filter
)
# Direct (no-index) is much slower; one representative is enough.
patterns_direct=("useState")

tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

run_hyperfine() {
  local label="$1" cmd="$2" out_json="$3"
  echo "==> [$label] $cmd"
  hyperfine \
    --warmup 5 --runs 30 \
    --export-json "$out_json" \
    --shell=none \
    --command-name "$label" \
    "$cmd" >/dev/null
}

results=()
for p in "${patterns_indexed[@]}"; do
  label="indexed:$p"
  cmd="$FGR_BIN \"$p\" \"$CORPUS\" --index \"$INDEX_DIR\" --quiet"
  json="$tmpdir/$(echo "$label" | tr ':/' '__').json"
  run_hyperfine "$label" "$cmd" "$json"
  results+=("$json")
done
for p in "${patterns_direct[@]}"; do
  label="direct:$p"
  cmd="$FGR_BIN \"$p\" \"$CORPUS\" --quiet"
  json="$tmpdir/$(echo "$label" | tr ':/' '__').json"
  run_hyperfine "$label" "$cmd" "$json"
  results+=("$json")
done

# Stitch all per-pattern JSON outputs into one file with context.
echo "==> Combining results into $OUT"
python3 - "$OUT" "${results[@]}" <<'PY'
import json, sys, os, subprocess, datetime, platform
out_path = sys.argv[1]
inputs = sys.argv[2:]

def sh(cmd):
    return subprocess.check_output(cmd, shell=True, text=True).strip()

context = {
    "timestamp_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
    "git_sha": sh("git rev-parse HEAD"),
    "git_short": sh("git rev-parse --short HEAD"),
    "git_dirty": sh("git status --porcelain") != "",
    "fgr_version": sh(f"{os.environ.get('FGR_BIN', 'target/release/fgr')} --version"),
    "hostname": platform.node(),
    "platform": platform.platform(),
    "corpus": os.environ.get("CORPUS", "(default)"),
}

results = []
for f in inputs:
    with open(f) as fp:
        data = json.load(fp)
    for r in data["results"]:
        results.append({
            "label": r["command"],
            "mean_s": r["mean"],
            "stddev_s": r["stddev"],
            "min_s": r["min"],
            "max_s": r["max"],
            "median_s": r["median"],
            "runs": len(r["times"]),
        })

with open(out_path, "w") as fp:
    json.dump({"context": context, "results": results}, fp, indent=2)

print(f"\nResults ({len(results)} patterns):")
print(f"{'pattern':32} {'mean (ms)':>12} {'stddev':>10} {'min':>10} {'max':>10}")
for r in results:
    print(f"{r['label']:32} {r['mean_s']*1000:>12.2f} {r['stddev_s']*1000:>10.2f} {r['min_s']*1000:>10.2f} {r['max_s']*1000:>10.2f}")
PY
