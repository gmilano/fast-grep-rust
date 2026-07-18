#!/usr/bin/env bash
# demo.sh — reproducible fast-grep demo session
#
# Designed to be recorded with asciinema or VHS.
# Run with: bash scripts/demo/demo.sh [REPO_PATH]
# Or: make demo
#
# What it shows:
#   1. Searching a repo with ripgrep (baseline)
#   2. Building a fast-grep index
#   3. Searching with fast-grep (indexed, same pattern)
#   4. Comparing grep vs --agent output side-by-side
#   5. Token estimates with --agent-stats
#   6. Using --max-results for context control

set -euo pipefail

REPO="${1:-$(pwd)}"
REPO="$(cd "$REPO" && pwd)"
PATTERN="${2:-fn main}"

FGR=""
if command -v fgr &>/dev/null; then
    FGR="fgr"
elif [ -x "$(dirname "$0")/../../target/release/fgr" ]; then
    FGR="$(cd "$(dirname "$0")/../.." && pwd)/target/release/fgr"
else
    echo "ERROR: fgr not found. Run: cargo build --release" >&2
    exit 1
fi

INDEX_DIR="$(mktemp -d)"
trap 'rm -rf "$INDEX_DIR"' EXIT

slowprint() {
    echo ""
    printf '\033[1;36m$ %s\033[0m\n' "$*"
    sleep 0.5
}

section() {
    echo ""
    echo "════════════════════════════════════════════════════════════"
    printf '\033[1;33m  %s\033[0m\n' "$*"
    echo "════════════════════════════════════════════════════════════"
    sleep 0.3
}

section "1. Baseline — ripgrep (no index)"
slowprint "rg -n '${PATTERN}' ${REPO} | head -15"
if command -v rg &>/dev/null; then
    rg -n "${PATTERN}" "${REPO}" 2>/dev/null | head -15 || true
    echo ""
    t_start=$(date +%s%3N)
    rg -n "${PATTERN}" "${REPO}" >/dev/null 2>&1 || true
    t_end=$(date +%s%3N)
    echo "ripgrep time: $((t_end - t_start))ms (no index)"
else
    echo "(ripgrep not installed — install with: brew install ripgrep)"
fi

section "2. Build fast-grep index (one-time cost)"
slowprint "fgr index ${REPO} --output ${INDEX_DIR}"
t_start=$(date +%s%3N)
"$FGR" index "${REPO}" --output "${INDEX_DIR}" 2>/dev/null
t_end=$(date +%s%3N)
echo "Index built in $((t_end - t_start))ms"
IDX_SIZE="$(du -sk "$INDEX_DIR" | awk '{print $1}')KB"
echo "Index size: $IDX_SIZE"

section "3. fast-grep indexed search"
slowprint "fgr '${PATTERN}' ${REPO} --index ${INDEX_DIR} | head -15"
"$FGR" "${PATTERN}" "${REPO}" --index "${INDEX_DIR}" 2>/dev/null | head -15 || true
echo ""
t_start=$(date +%s%3N)
"$FGR" "${PATTERN}" "${REPO}" --index "${INDEX_DIR}" >/dev/null 2>&1
t_end=$(date +%s%3N)
echo "fast-grep indexed time: $((t_end - t_start))ms"

section "4a. Traditional grep output"
slowprint "fgr --format grep '${PATTERN}' ${REPO} --index ${INDEX_DIR} | head -10"
"$FGR" --format grep "${PATTERN}" "${REPO}" --index "${INDEX_DIR}" 2>/dev/null | head -10 || true
GREP_BYTES=$("$FGR" --format grep "${PATTERN}" "${REPO}" --index "${INDEX_DIR}" 2>/dev/null | wc -c | tr -d ' ')
echo ""
echo "grep format: ${GREP_BYTES} bytes (~$(( (GREP_BYTES + 3) / 4 )) tokens est.)"

section "4b. fast-grep --agent output (path printed once per file)"
slowprint "fgr --agent '${PATTERN}' ${REPO} --index ${INDEX_DIR} | head -15"
"$FGR" --agent "${PATTERN}" "${REPO}" --index "${INDEX_DIR}" 2>/dev/null | head -15 || true
AGENT_BYTES=$("$FGR" --agent "${PATTERN}" "${REPO}" --index "${INDEX_DIR}" 2>/dev/null | wc -c | tr -d ' ')
echo ""
echo "compact format: ${AGENT_BYTES} bytes (~$(( (AGENT_BYTES + 3) / 4 )) tokens est.)"
if [ "$GREP_BYTES" -gt 0 ]; then
    REDUCTION=$(( 100 * (GREP_BYTES - AGENT_BYTES) / GREP_BYTES ))
    echo "Token reduction: ${REDUCTION}%"
fi

section "5. Agent stats (latency + token estimates to stderr)"
slowprint "fgr --agent --agent-stats '${PATTERN}' ${REPO} --index ${INDEX_DIR}"
"$FGR" --agent --agent-stats "${PATTERN}" "${REPO}" --index "${INDEX_DIR}" >/dev/null

section "6. Output limits (protect context budget)"
slowprint "fgr --agent --max-results 5 --max-files 2 '${PATTERN}' ${REPO} --index ${INDEX_DIR}"
"$FGR" --agent --max-results 5 --max-files 2 "${PATTERN}" "${REPO}" --index "${INDEX_DIR}" 2>&1 || true

section "Done"
echo ""
echo "To record this demo:"
echo "  asciinema rec docs/assets/demo.cast -- bash scripts/demo/demo.sh"
echo "  # or use VHS: vhs scripts/demo/demo.tape"
echo ""
echo "To try fast-grep in your project:"
echo "  fgr index . --output .fgr"
echo "  fgr --agent 'your_pattern' . --index .fgr"
