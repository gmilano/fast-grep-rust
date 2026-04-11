#!/bin/bash
FGR="./target/release/fgr"
CORPUS="/tmp/linux-6.6"
INDEX="/tmp/fgr-bench"

PATTERNS=(
  "TODO" "FIXME" "printk" "EXPORT_SYMBOL" "container_of"
  "static inline" "NULL" "return -EINVAL" "struct.*_ops"
  "spin_lock" "mutex_lock" "kmalloc" "pr_err" "BUG_ON" "WARN_ON"
  "CONFIG_[A-Z_]+" "__init" "__exit" "module_param" "kfree"
  "dev_err" "dev_warn" "if.*NULL" "goto.*err" "sizeof\(struct"
)

total=0; pass=0; fail_fn=0; fail_fp=0

for pat in "${PATTERNS[@]}"; do
  rg_n=$(rg "$pat" "$CORPUS" 2>/dev/null | wc -l | tr -d ' ')
  fgr_n=$("$FGR" "$pat" "$CORPUS" --index "$INDEX" 2>/dev/null | wc -l | tr -d ' ')
  total=$((total + 1))
  
  if [ "$rg_n" -eq "$fgr_n" ]; then
    status="✅"; pass=$((pass + 1))
  elif [ "$fgr_n" -lt "$rg_n" ]; then
    diff=$((rg_n - fgr_n))
    status="❌ FN -$diff"; fail_fn=$((fail_fn + 1))
  else
    diff=$((fgr_n - rg_n))
    status="⚠️  FP +$diff"; fail_fp=$((fail_fp + 1))
  fi
  
  printf "%-28s  rg=%-8s fgr=%-8s  %s\n" "$pat" "$rg_n" "$fgr_n" "$status"
done

echo ""
echo "────────────────────────────────────────────────────"
echo "Total: $total | Pass: $pass | False Neg: $fail_fn | False Pos: $fail_fp"
printf "Precision: %.1f%%\n" "$(echo "scale=4; $pass * 100 / $total" | bc)"
