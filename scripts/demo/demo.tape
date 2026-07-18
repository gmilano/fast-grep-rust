# VHS tape for fast-grep demo
# Usage: vhs scripts/demo/demo.tape
# Output: docs/assets/demo.gif

Output docs/assets/demo.gif
Set FontSize 14
Set Width 1200
Set Height 700
Set Theme "Dracula"

Type "# fast-grep: context-efficient code search for agents"
Enter
Sleep 1s

Type "# 1. Ripgrep baseline (full scan)"
Enter
Type "rg -n 'fn main' . | head -10"
Enter
Sleep 3s

Type "# 2. Build fast-grep index (one-time)"
Enter
Type "fgr index . --output .fgr"
Enter
Sleep 5s

Type "# 3. Indexed search (same pattern)"
Enter
Type "fgr 'fn main' . --index .fgr | head -10"
Enter
Sleep 2s

Type "# 4. Agent output — path printed once per file"
Enter
Type "fgr --agent 'fn main' . --index .fgr | head -15"
Enter
Sleep 2s

Type "# 5. Token estimates"
Enter
Type "fgr --agent --agent-stats 'fn main' . --index .fgr"
Enter
Sleep 3s

Type "# 6. Context budget protection"
Enter
Type "fgr --agent --max-results 10 --max-files 3 'fn main' . --index .fgr"
Enter
Sleep 2s
