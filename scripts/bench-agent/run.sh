#!/usr/bin/env bash
# bench-agent/run.sh — reproducible fast-grep vs ripgrep agent output benchmark
#
# Usage:
#   ./scripts/bench-agent/run.sh [REPO_PATH]
#
# Output:
#   benchmarks/results/<timestamp>.json
#   benchmarks/results/<timestamp>.md
#
# Requirements:
#   - fgr (fast-grep binary) in PATH or at ./target/release/fgr
#   - rg (ripgrep) in PATH (optional, comparison only)
#   - jq in PATH (optional, for JSON formatting)
#
# Note: ripgrep does not use an index. The comparison is meaningful for
# REPEATED searches in large, stable repositories — not for one-off queries.
# For a first search on a new repo, ripgrep will be faster.

set -euo pipefail

REPO="${1:-$(pwd)}"
REPO="$(cd "$REPO" && pwd)"   # absolute path
TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
RESULTS_DIR="$(dirname "$0")/../../benchmarks/results"
mkdir -p "$RESULTS_DIR"

JSON_OUT="$RESULTS_DIR/${TIMESTAMP}.json"
MD_OUT="$RESULTS_DIR/${TIMESTAMP}.md"

# Locate fgr binary
FGR=""
if command -v fgr &>/dev/null; then
    FGR="fgr"
elif [ -x "$(dirname "$0")/../../target/release/fgr" ]; then
    FGR="$(cd "$(dirname "$0")/../.." && pwd)/target/release/fgr"
else
    echo "ERROR: fgr not found in PATH or ./target/release/fgr" >&2
    exit 1
fi

RG=""
if command -v rg &>/dev/null; then
    RG="rg"
fi

echo "=== fast-grep agent benchmark ==="
echo "Repo:    $REPO"
echo "fgr:     $($FGR --version 2>/dev/null || echo 'unknown')"
echo "rg:      $(${RG:-echo} --version 2>/dev/null | head -1 || echo 'not found')"
echo "Output:  $JSON_OUT"
echo ""

# Gather system info
OS="$(uname -s)"
ARCH="$(uname -m)"
CPU="$(sysctl -n machdep.cpu.brand_string 2>/dev/null || grep 'model name' /proc/cpuinfo 2>/dev/null | head -1 | cut -d: -f2 | xargs || echo 'unknown')"
MEM_KB="$(sysctl -n hw.memsize 2>/dev/null | awk '{print int($1/1024)}' || grep MemTotal /proc/meminfo 2>/dev/null | awk '{print $2}' || echo 0)"

# Count files and repo size
FILE_COUNT="$(find "$REPO" -type f 2>/dev/null | wc -l | tr -d ' ')"
REPO_SIZE_KB="$(du -sk "$REPO" 2>/dev/null | awk '{print $1}' || echo 0)"

# Test patterns: representative mix of literal, regex, common, rare
PATTERNS=(
    "TODO"
    "fn main"
    "process_request"
    "static.*inline"
    "EXPORT_SYMBOL"
)

INDEX_DIR="$(mktemp -d)"
trap 'rm -rf "$INDEX_DIR"' EXIT

# Build index
echo "Building index..."
INDEX_BUILD_START=$(date +%s%3N)
"$FGR" index "$REPO" --output "$INDEX_DIR" 2>/dev/null
INDEX_BUILD_END=$(date +%s%3N)
INDEX_BUILD_MS=$((INDEX_BUILD_END - INDEX_BUILD_START))
INDEX_SIZE_KB="$(du -sk "$INDEX_DIR" 2>/dev/null | awk '{print $1}' || echo 0)"
echo "  Done: ${INDEX_BUILD_MS}ms, ${INDEX_SIZE_KB}KB"

# Helper: run a command N times and return median/min/max in ms
bench_cmd() {
    local N=5
    local times=()
    for _ in $(seq 1 $N); do
        local t_start=$(date +%s%3N)
        eval "$@" >/dev/null 2>&1 || true
        local t_end=$(date +%s%3N)
        times+=($((t_end - t_start)))
    done
    # sort and compute median
    IFS=$'\n' sorted=($(sort -n <<<"${times[*]}")); unset IFS
    local n=${#sorted[@]}
    local min="${sorted[0]}"
    local max="${sorted[$((n-1))]}"
    local median="${sorted[$((n/2))]}"
    echo "${median} ${min} ${max}"
}

# Count matches for a pattern
count_matches() {
    local cmd="$1"
    eval "$cmd" 2>/dev/null | wc -l | tr -d ' ' || echo 0
}

# Estimate output tokens (4 bytes per token)
estimate_tokens() {
    local bytes="$1"
    echo $(( (bytes + 3) / 4 ))
}

# Start JSON
{
cat <<JSON
{
  "version": "1",
  "timestamp": "$TIMESTAMP",
  "fast_grep_version": "$($FGR --version 2>/dev/null | head -1 || echo 'unknown')",
  "ripgrep_version": "$(${RG:-echo} --version 2>/dev/null | head -1 || echo 'not available')",
  "system": {
    "os": "$OS",
    "arch": "$ARCH",
    "cpu": "$CPU",
    "mem_kb": $MEM_KB
  },
  "repository": {
    "path": "$REPO",
    "files": $FILE_COUNT,
    "size_kb": $REPO_SIZE_KB
  },
  "index": {
    "build_ms": $INDEX_BUILD_MS,
    "size_kb": $INDEX_SIZE_KB
  },
  "note": "ripgrep has no index; latency comparison is meaningful for repeated searches on large repos only",
  "patterns": [
JSON
} > "$JSON_OUT"

# MD header
{
cat <<MD
# fast-grep agent benchmark — ${TIMESTAMP}

> **Note**: ripgrep does not use an index. The latency comparison is meaningful
> for **repeated searches** in large, stable repositories. For a first search or
> a small repository, ripgrep will be faster.

## Environment

| Key | Value |
|-----|-------|
| OS | $OS $ARCH |
| CPU | $CPU |
| Memory | $((MEM_KB / 1024 / 1024)) GB |
| fast-grep | $($FGR --version 2>/dev/null | head -1 || echo unknown) |
| ripgrep | $(${RG:-echo} --version 2>/dev/null | head -1 || echo 'not available') |
| Repository | $REPO ($FILE_COUNT files, $((REPO_SIZE_KB / 1024)) MB) |
| Index build | ${INDEX_BUILD_MS}ms (${INDEX_SIZE_KB} KB) |

## Results (5 repetitions, median reported)

| Pattern | fgr indexed | fgr no-index | rg no-index | grep-bytes | compact-bytes | token-reduction |
|---------|------------|-------------|------------|------------|----------------|----------------|
MD
} > "$MD_OUT"

FIRST_PATTERN=true
for PATTERN in "${PATTERNS[@]}"; do
    echo "Testing: '$PATTERN'"

    # Measurements
    read -r fgr_idx_med fgr_idx_min fgr_idx_max <<< "$(bench_cmd "\"$FGR\" \"$PATTERN\" \"$REPO\" --index \"$INDEX_DIR\" 2>/dev/null")"
    read -r fgr_scan_med fgr_scan_min fgr_scan_max <<< "$(bench_cmd "\"$FGR\" \"$PATTERN\" \"$REPO\" 2>/dev/null")"
    rg_med=""
    rg_min=""
    rg_max=""
    if [ -n "$RG" ]; then
        read -r rg_med rg_min rg_max <<< "$(bench_cmd "\"$RG\" -n \"$PATTERN\" \"$REPO\"")"
    fi

    # Output sizes
    grep_bytes="$("$FGR" "$PATTERN" "$REPO" --index "$INDEX_DIR" 2>/dev/null | wc -c | tr -d ' ' || echo 0)"
    compact_bytes="$("$FGR" --agent "$PATTERN" "$REPO" --index "$INDEX_DIR" 2>/dev/null | wc -c | tr -d ' ' || echo 0)"
    match_count="$("$FGR" "$PATTERN" "$REPO" --index "$INDEX_DIR" 2>/dev/null | wc -l | tr -d ' ' || echo 0)"

    grep_tokens=$(estimate_tokens "$grep_bytes")
    compact_tokens=$(estimate_tokens "$compact_bytes")
    if [ "$grep_tokens" -gt 0 ]; then
        reduction=$(( 100 * (grep_tokens - compact_tokens) / grep_tokens ))
    else
        reduction=0
    fi

    rg_field="${rg_med:-n/a}"

    # Append to JSON
    if [ "$FIRST_PATTERN" = "true" ]; then
        FIRST_PATTERN=false
    else
        echo "," >> "$JSON_OUT"
    fi
    cat >> "$JSON_OUT" <<JSON2
    {
      "pattern": "$PATTERN",
      "matches": $match_count,
      "fgr_indexed_ms": {"median": $fgr_idx_med, "min": $fgr_idx_min, "max": $fgr_idx_max},
      "fgr_scan_ms": {"median": $fgr_scan_med, "min": $fgr_scan_min, "max": $fgr_scan_max},
      "rg_scan_ms": {"median": ${rg_med:-0}, "min": ${rg_min:-0}, "max": ${rg_max:-0}},
      "output": {
        "grep_bytes": $grep_bytes,
        "compact_bytes": $compact_bytes,
        "grep_tokens_approx": $grep_tokens,
        "compact_tokens_approx": $compact_tokens,
        "token_reduction_pct": $reduction
      }
    }
JSON2

    # Append to MD
    echo "| \`$PATTERN\` | ${fgr_idx_med}ms | ${fgr_scan_med}ms | ${rg_field}ms | ${grep_bytes}B (~${grep_tokens}t) | ${compact_bytes}B (~${compact_tokens}t) | ${reduction}% |" >> "$MD_OUT"
done

# Close JSON
cat >> "$JSON_OUT" <<JSON
  ]
}
JSON

# Close MD
cat >> "$MD_OUT" <<MD

## Commands used

\`\`\`bash
# Build index
fgr index "$REPO" --output <INDEX_DIR>

# Indexed search (fgr)
fgr "<PATTERN>" "$REPO" --index <INDEX_DIR>

# Full scan (fgr, no index)
fgr "<PATTERN>" "$REPO"

# Full scan (rg, no index)
rg -n "<PATTERN>" "$REPO"

# Agent compact output
fgr --agent "<PATTERN>" "$REPO" --index <INDEX_DIR>
\`\`\`

> Token estimates use ~4 bytes/token heuristic. Not based on a tokenizer.
> Repetitions: 5 per pattern. Index built once, shared across patterns.
MD

echo ""
echo "Results written to:"
echo "  $JSON_OUT"
echo "  $MD_OUT"
