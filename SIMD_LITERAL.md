# Task: SIMD literal search optimization

## Problem
Without index, fast-grep is 2x slower than ripgrep.
Ripgrep detects literal substrings in patterns and uses memchr/Aho-Corasick SIMD before the regex engine.

## Solution
Implement literal pre-filter in the Matcher (src/searcher.rs):

### Pattern analysis
Extract the longest literal substring from the regex pattern:
- `"useState"` → literal `"useState"` (whole thing)
- `"async function"` → literal `"async function"`
- `"import.*from"` → longest literal: `"import"` or `"from"` (pick longer)
- `"prisma\."` → literal `"prisma"`
- `"TODO|FIXME"` → literals `["TODO", "FIXME"]` (alternation)
- `"static.*inline"` → `"static"` and `"inline"` (both required in some order)

### Matcher enum - add new variants
```rust
enum Matcher {
    // Existing: pure regex
    Regex(BytesRegex),
    // New: literal fast path — check literal first, then verify with regex
    LiteralThenRegex { literal: Box<[u8]>, regex: BytesRegex },
    // New: Aho-Corasick for alternations — find any literal, then verify
    AhoCorasickThenRegex { ac: AhoCorasick, regex: BytesRegex },
    // New: pure literal (no regex chars) — just memchr/memmem
    PureLiteral(Box<[u8]>),
}
```

### Add to Cargo.toml:
- `aho-corasick = "1"` — already used by regex crate, expose directly

### Matcher::new() logic:
1. Try to extract literal from pattern using simple heuristic:
   - If pattern has no regex metacharacters (`. * + ? [ ] { } ( ) ^ $ | \ `): PureLiteral
   - If pattern matches `^[literal1][.*|.+][literal2]$` style: LiteralThenRegex with longer literal
   - If pattern is `lit1|lit2|lit3` (pure alternation of literals): AhoCorasickThenRegex
   - Otherwise: Regex (fallback, current behavior)

2. Use `memmem::Finder` from memchr crate (already imported) for PureLiteral and LiteralThenRegex

### search_buffer() for each variant:
- **PureLiteral**: `memmem::find(buf, &literal)` → if found, extract all matching lines. No regex needed.
- **LiteralThenRegex**: first `memmem::find(buf, &literal)` → if not found, skip file entirely. If found, run full regex on buffer.
- **AhoCorasickThenRegex**: `ac.find(buf)` → if no match, skip. If found, run full regex.

### is_match() for each variant:
- **PureLiteral**: `memmem::find(buf, &literal).is_some()`
- **LiteralThenRegex**: `memmem::find(buf, &literal).is_some() && regex.is_match(buf)`
- **AhoCorasickThenRegex**: `ac.find(buf).is_some() && regex.is_match(buf)`

## Key insight
The massive speedup comes from the file-skip optimization:
- For `prisma\.` on ~/Projects: most of 21,922 files don't contain "prisma"
- `memmem::find` uses SIMD (AVX2 on x86, NEON on ARM) — scans ~32 bytes/cycle
- Skipping files with no literal match avoids regex overhead entirely
- This is exactly what ripgrep does internally

## Benchmark target
After: fast-grep --no-index should match or beat ripgrep for literal-heavy patterns.

## Steps
1. Implement in src/searcher.rs
2. cargo build --release -q
3. Run comparison: for patterns "useState" "async function" "TODO" "prisma\." "import.*from"
   show: ripgrep vs fast-grep --no-index (before) vs fast-grep --no-index (after)
4. git add -A && git commit -m "perf: SIMD literal pre-filter, 2x faster no-index search"
5. git push
6. openclaw system event --text "simd literal listo" --mode now
