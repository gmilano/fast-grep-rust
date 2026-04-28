#!/bin/bash
# Precision test: compare fgr vs rg output on linux kernel
# Checks false negatives (fgr misses) and false positives (fgr extra)
# Run from repo root: bash scripts/precision-test.sh

FGR="${FGR:-./target/release/fgr}"
CORPUS="${CORPUS:-/tmp/linux-6.6}"
INDEX="${INDEX:-/tmp/fgr-bench}"

# ── Literals ──────────────────────────────────────────────────────────────────
LITERAL_PATTERNS=(
  "TODO" "FIXME" "HACK" "XXX" "NOTE"
  "printk" "EXPORT_SYMBOL" "container_of" "static inline" "NULL"
  "return -EINVAL" "return -ENOMEM" "return -ENODEV"
  "spin_lock" "mutex_lock" "kfree" "kmalloc" "kzalloc"
  "pr_err" "pr_warn" "pr_info" "dev_err" "dev_warn"
  "BUG_ON" "WARN_ON" "BUG" "WARN"
  "__init" "__exit" "module_param" "MODULE_LICENSE"
  "sizeof(struct" "sizeof(int)"
)

# ── Regex: alternation ────────────────────────────────────────────────────────
ALTERNATION_PATTERNS=(
  "kmalloc|kzalloc|vmalloc"
  "pr_err|pr_warn|pr_info"
  "spin_lock|mutex_lock|rw_lock"
  "BUG_ON|WARN_ON"
  "return -EIO|return -EINVAL|return -ENOMEM"
)

# ── Regex: quantifiers & wildcards ────────────────────────────────────────────
REGEX_PATTERNS=(
  "struct.*_ops"
  "if.*NULL"
  "goto.*err"
  "CONFIG_[A-Z_]+"
  "sizeof\(struct"
  "v[0-9]\+\.[0-9]\+"
  "#define [A-Z_][A-Z0-9_]*"
  "0x[0-9a-fA-F]\+"
  "__[a-z_]\+__"
  "[a-z_]\+_lock\b"
)

# ── Short patterns (stress index) ─────────────────────────────────────────────
SHORT_PATTERNS=(
  "err"
  "ret"
  "buf"
  "len"
  "ptr"
)

# ── Patterns with special chars ───────────────────────────────────────────────
SPECIAL_PATTERNS=(
  "\.c:[0-9]"
  "\->"
  "!="
  "==[[:space:]]*0"
  "/*.*\*/"
)

# ── Case insensitive ──────────────────────────────────────────────────────────
# These use -i flag — run separately
CASE_INSENSITIVE=(
  "todo"
  "fixme"
  "bug"
  "error"
)

total=0; pass=0; fail_fn=0; fail_fp=0

run_test() {
  local label="$1"
  local pat="$2"
  local extra_flags="${3:-}"

  rg_n=$(rg $extra_flags "$pat" "$CORPUS" 2>/dev/null | wc -l | tr -d ' ')
  fgr_n=$("$FGR" $extra_flags "$pat" "$CORPUS" --index "$INDEX" 2>/dev/null | wc -l | tr -d ' ')
  total=$((total + 1))

  if [ "$rg_n" -eq "$fgr_n" ]; then
    status="✅"; pass=$((pass + 1))
  elif [ "$fgr_n" -lt "$rg_n" ]; then
    diff=$((rg_n - fgr_n)); status="❌ FN -$diff"; fail_fn=$((fail_fn + 1))
  else
    diff=$((fgr_n - rg_n)); status="⚠️  FP +$diff"; fail_fp=$((fail_fp + 1))
  fi

  printf "%-35s  rg=%-8s fgr=%-8s  %s\n" "$label" "$rg_n" "$fgr_n" "$status"
}

echo "══ Literals ══════════════════════════════════════════════════════"
for pat in "${LITERAL_PATTERNS[@]}"; do run_test "$pat" "$pat"; done

echo ""
echo "══ Alternation ═══════════════════════════════════════════════════"
for pat in "${ALTERNATION_PATTERNS[@]}"; do run_test "$pat" "$pat"; done

echo ""
echo "══ Regex (wildcards/quantifiers) ═════════════════════════════════"
for pat in "${REGEX_PATTERNS[@]}"; do run_test "$pat" "$pat"; done

echo ""
echo "══ Short patterns ════════════════════════════════════════════════"
for pat in "${SHORT_PATTERNS[@]}"; do run_test "$pat" "$pat"; done

echo ""
echo "══ Special characters ════════════════════════════════════════════"
for pat in "${SPECIAL_PATTERNS[@]}"; do run_test "$pat" "$pat"; done

echo ""
echo "══ Case insensitive (-i) ══════════════════════════════════════════"
for pat in "${CASE_INSENSITIVE[@]}"; do run_test "$pat (-i)" "$pat" "-i"; done

echo ""
echo "══════════════════════════════════════════════════════════════════"
echo "Total: $total | Pass: $pass | False Neg: $fail_fn | False Pos: $fail_fp"
printf "Precision: %.1f%%\n" "$(echo "scale=4; $pass * 100 / $total" | bc)"
