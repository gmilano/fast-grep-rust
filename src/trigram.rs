/// Returns true if `pattern` enables case-insensitive matching anywhere
/// via an inline `(?i…)` flag group. The trigram index is built from the
/// raw bytes of source files (case-sensitive), so a `(?i)abc` pattern
/// can match `ABC` in a file even though the trigram `abc` was never
/// recorded — the index would falsely report no candidates. Callers use
/// this as a short-circuit signal to fall back to a full-file scan.
///
/// Walks `(?…)` flag groups looking for `i` not preceded by `-` (the
/// `-i` form disables the flag rather than enabling it). Catches
/// `(?i)`, `(?im)`, `(?mi)`, `(?Ri)`, `(?i:…)`, etc. False positives
/// (treating `(?-i)abc` as case-insensitive) are harmless — we just
/// pay the full-scan cost unnecessarily.
pub fn has_case_insensitive_flag(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'(' && bytes[i + 1] == b'?' {
            // Walk the flag-group prefix until `:` (scoped flags) or `)`
            // (top-level flags), checking each char.
            let mut j = i + 2;
            let mut negate = false;
            while j < bytes.len() {
                match bytes[j] {
                    b'-' => negate = true,
                    b'i' if !negate => return true,
                    b':' | b')' => break,
                    _ => {}
                }
                j += 1;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    false
}

/// Mask selecting the trigram-identity bits of a [`trigram_key`]. The three
/// trigram bytes occupy the top 24 bits; the low 8 bits are a **reserved
/// field**, currently always 0. The reserved byte is a zero-cost extension
/// point (per-trigram flags, a rarity bucket for the planner, …): it is already
/// materialized in every 16-byte lookup entry. Two rules make it safe to fill
/// later without an on-disk format change:
///
///  1. **Every key *comparison* masks with this constant.** The lookup table is
///     sorted and binary-searched by key, but a query only knows the trigram
///     bits — an exact match over the full `u32` would miss an entry whose
///     reserved byte is non-zero. All four comparison sites (main lookup,
///     bitmap lookup, delta lookup, and the compaction merge-join) already mask,
///     so a build that starts writing the reserved byte stays readable by
///     existing binaries (they mask it off and preserve it through compaction).
///  2. **The sort needs no mask** — the identity bits are the most significant,
///     so full-key order is trigram order regardless of the reserved byte.
pub const TRIGRAM_KEY_MASK: u32 = 0xFFFF_FF00;

/// Pack a trigram into its `u32` index key: the three bytes in the top 24 bits,
/// low 8 bits reserved (see [`TRIGRAM_KEY_MASK`]). A trigram is exactly 24 bits,
/// so this is a perfect bijection — two distinct trigrams can never share a key
/// (no hashing, no collisions, injective by construction). Used as the on-disk
/// lookup key for postings and bitmaps, and to key query trigrams at search
/// time. Replaced the former CRC32 hash, which was also collision-free on
/// 3-byte inputs but computed a hash the key never needed and scattered keys
/// across the full 32-bit range.
#[inline]
pub fn trigram_key(tri: &[u8; 3]) -> u32 {
    ((tri[0] as u32) << 24) | ((tri[1] as u32) << 16) | ((tri[2] as u32) << 8)
}

/// Decompose a regex pattern into literal trigrams that must appear in any match.
/// Returns a Vec of Vec<[u8;3]> where the outer vec is OR alternatives,
/// and each inner vec is AND-required trigrams for that alternative.
pub fn decompose_pattern(pattern: &str) -> Vec<Vec<[u8; 3]>> {
    decompose_inner(pattern, false)
}

/// Like [`decompose_pattern`], but each literal run is Unicode case-folded
/// (the same fold the CI index applies to content) before trigrams are taken.
/// Used to resolve `(?i)` queries against the case-insensitive companion index.
pub fn decompose_pattern_folded(pattern: &str) -> Vec<Vec<[u8; 3]>> {
    decompose_inner(pattern, true)
}

fn decompose_inner(pattern: &str, fold: bool) -> Vec<Vec<[u8; 3]>> {
    // Split on top-level '|' (not inside parens/brackets)
    let alternatives = split_alternatives(pattern);
    let mut result = Vec::new();
    let mut folded = Vec::new();
    for alt in &alternatives {
        let literals = extract_literal_runs(alt);
        let mut trigrams = Vec::new();
        for lit in &literals {
            let bytes: &[u8] = if fold {
                crate::casefold::fold_into(lit.as_bytes(), &mut folded);
                &folded
            } else {
                lit.as_bytes()
            };
            if bytes.len() >= 3 {
                for w in bytes.windows(3) {
                    trigrams.push([w[0], w[1], w[2]]);
                }
            }
        }
        trigrams.sort();
        trigrams.dedup();
        result.push(trigrams);
    }
    // Filter out empty alternatives (they match everything)
    if result.iter().any(|v| v.is_empty()) {
        return vec![vec![]];
    }
    result
}

fn split_alternatives(pattern: &str) -> Vec<String> {
    let mut alts = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut bracket = false;
    let mut escape = false;

    for ch in pattern.chars() {
        if escape {
            current.push(ch);
            escape = false;
            continue;
        }
        if ch == '\\' {
            escape = true;
            current.push(ch);
            continue;
        }
        if bracket {
            current.push(ch);
            if ch == ']' {
                bracket = false;
            }
            continue;
        }
        match ch {
            '[' => {
                bracket = true;
                current.push(ch);
            }
            '(' => {
                depth += 1;
                current.push(ch);
            }
            ')' => {
                depth -= 1;
                current.push(ch);
            }
            '|' if depth == 0 => {
                alts.push(std::mem::take(&mut current));
            }
            _ => {
                current.push(ch);
            }
        }
    }
    alts.push(current);
    alts
}

/// Extract contiguous literal runs from a regex pattern.
/// Stops at metacharacters (., *, +, ?, [, (, {, |, ^, $).
fn extract_literal_runs(pattern: &str) -> Vec<String> {
    let mut runs = Vec::new();
    let mut current = String::new();
    let mut chars = pattern.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            // Escaped character — check if it's a literal
            if let Some(&next) = chars.peek() {
                match next {
                    'd' | 'D' | 'w' | 'W' | 's' | 'S' | 'b' | 'B' | 'A' | 'z' | 'Z' | 'p' | 'P' => {
                        // Not a literal — regex escape class
                        if !current.is_empty() {
                            runs.push(std::mem::take(&mut current));
                        }
                        chars.next();
                        // \p{...} and \P{...} — skip the brace-delimited property name
                        if (next == 'p' || next == 'P') && chars.peek() == Some(&'{') {
                            chars.next(); // skip '{'
                            while let Some(c) = chars.next() {
                                if c == '}' {
                                    break;
                                }
                            }
                        }
                    }
                    _ => {
                        // Escaped literal (e.g., \. \* etc)
                        current.push(chars.next().unwrap());
                    }
                }
            }
        } else if ch == '[' {
            // Skip entire character class — contents are not literals
            if !current.is_empty() {
                runs.push(std::mem::take(&mut current));
            }
            // Handle '^' and ']' as first char in class (e.g., [^]b] or []b])
            let mut first = true;
            if chars.peek() == Some(&'^') {
                chars.next();
            }
            while let Some(c) = chars.next() {
                if c == '\\' {
                    chars.next();
                    first = false;
                } else if c == '[' && chars.peek() == Some(&':') {
                    // POSIX class like [:alnum:] inside a bracket expression.
                    // The full pattern is [[:alnum:]] — inner closes at :] and
                    // outer bracket expression continues. Skip to :].
                    chars.next(); // consume ':'
                    while let Some(p) = chars.next() {
                        if p == ':' {
                            if chars.peek() == Some(&']') {
                                chars.next(); // consume closing ']'
                            }
                            break;
                        }
                    }
                    first = false;
                } else if c == ']' && !first {
                    break;
                } else {
                    first = false;
                }
            }
        } else if ch == '{' {
            // Skip repetition quantifier {n}, {n,}, {n,m}
            if !current.is_empty() {
                runs.push(std::mem::take(&mut current));
            }
            while let Some(c) = chars.next() {
                if c == '}' {
                    break;
                }
            }
        } else if ch == '(' {
            // Skip entire group if it contains alternation — extracting literals
            // from inside would produce AND constraints from OR alternatives.
            // For groups without alternation, just skip the group syntax.
            if !current.is_empty() {
                runs.push(std::mem::take(&mut current));
            }
            // Scan ahead to find matching ')' and check for '|'
            let mut depth = 1i32;
            let mut has_alt = false;
            let saved = chars.clone();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        chars.next();
                    }
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    '|' if depth == 1 => has_alt = true,
                    _ => {}
                }
            }
            if !has_alt {
                // No alternation — re-parse group contents for literals
                chars = saved;
                // Skip non-capturing group syntax (?:, (?P<, etc.)
                if chars.peek() == Some(&'?') {
                    let mut lookahead = chars.clone();
                    lookahead.next();
                    if let Some(&after) = lookahead.peek() {
                        // Flag-group prefix chars the rust `regex` crate accepts:
                        // `i` `m` `s` `x` `u` `U` (standard) plus `R` (CRLF
                        // line-terminator mode); plus `:` (end of flag prefix
                        // before the group body), `P` (named-group `(?P<…>`),
                        // `-` (negate flag), `<`/`!`/`=` (lookaround prefixes).
                        // Missing one of these means the parser falls into
                        // the literal extractor and pulls trigrams from the
                        // regex syntax — which won't exist in source files
                        // and produces a false-empty candidate set.
                        if ":PimRsxuU-<!=".contains(after) {
                            chars.next();
                            while let Some(&c) = chars.peek() {
                                if c == ':' || c == ')' {
                                    chars.next();
                                    break;
                                }
                                chars.next();
                            }
                        }
                    }
                }
            }
        } else if is_meta(ch) {
            if !current.is_empty() {
                runs.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        runs.push(current);
    }
    runs
}

fn is_meta(ch: char) -> bool {
    matches!(
        ch,
        '.' | '*' | '+' | '?' | '[' | ']' | '(' | ')' | '{' | '}' | '|' | '^' | '$'
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // --- trigram_key ---

    #[test]
    fn trigram_key_packs_into_top_24_bits() {
        assert_eq!(trigram_key(&[0x41, 0x42, 0x43]), 0x4142_4300);
        assert_eq!(trigram_key(&[0x00, 0x00, 0x00]), 0);
        assert_eq!(trigram_key(&[0xFF, 0xFF, 0xFF]), 0xFFFF_FF00);
        // the low 8 bits (reserved field) are always 0
        for t in [[0x00, 0x00, 0x00], [0x41, 0x42, 0x43], [0xFF, 0xFF, 0xFF]] {
            assert_eq!(trigram_key(&t) & !TRIGRAM_KEY_MASK, 0);
        }
    }

    #[test]
    fn trigram_key_is_injective() {
        // Exhaustive over a dense byte sample: distinct trigrams -> distinct keys.
        let bytes: [u8; 8] = [0x00, 0x01, 0x7F, 0x80, 0xC3, 0xA9, 0xFE, 0xFF];
        let mut seen = HashSet::new();
        for &a in &bytes {
            for &b in &bytes {
                for &c in &bytes {
                    assert!(
                        seen.insert(trigram_key(&[a, b, c])),
                        "collision at {a:#x} {b:#x} {c:#x}"
                    );
                }
            }
        }
        assert_eq!(seen.len(), bytes.len().pow(3));
    }

    // --- decompose_pattern tests (ported from decomposeRegex in trigram.test.ts) ---

    #[test]
    fn extracts_required_trigrams_from_plain_literal() {
        let result = decompose_pattern("hello");
        assert_eq!(result.len(), 1);
        assert!(result[0].contains(&[b'h', b'e', b'l']));
        assert!(result[0].contains(&[b'e', b'l', b'l']));
        assert!(result[0].contains(&[b'l', b'l', b'o']));
    }

    #[test]
    fn returns_empty_trigrams_for_short_patterns() {
        let result = decompose_pattern("ab");
        // Short pattern → single alternative with no trigrams → falls back to vec![vec![]]
        assert!(result.iter().all(|v| v.is_empty()));
    }

    #[test]
    fn handles_alternation_producing_separate_branches() {
        let result = decompose_pattern("hello|world");
        assert_eq!(result.len(), 2);
        assert!(result[0].contains(&[b'h', b'e', b'l']));
        assert!(result[1].contains(&[b'w', b'o', b'r']));
    }

    #[test]
    fn extracts_trigrams_from_literal_parts_with_wildcards() {
        let result = decompose_pattern("function.*async");
        assert_eq!(result.len(), 1);
        assert!(result[0].contains(&[b'f', b'u', b'n']));
        assert!(result[0].contains(&[b'a', b's', b'y']));
    }

    #[test]
    fn handles_escaped_metacharacters_as_literals() {
        let result = decompose_pattern("a\\.b\\.c");
        assert_eq!(result.len(), 1);
        assert!(result[0].contains(&[b'a', b'.', b'b']));
    }

    #[test]
    fn handles_character_classes_by_breaking_literal_run() {
        let result = decompose_pattern("foo[abc]bar");
        assert_eq!(result.len(), 1);
        assert!(result[0].contains(&[b'f', b'o', b'o']));
        assert!(result[0].contains(&[b'b', b'a', b'r']));
    }

    #[test]
    fn handles_shorthand_classes_like_d_w() {
        let result = decompose_pattern("hello\\dworld");
        assert_eq!(result.len(), 1);
        assert!(result[0].contains(&[b'h', b'e', b'l']));
        assert!(result[0].contains(&[b'w', b'o', b'r']));
    }

    #[test]
    fn returns_empty_for_pure_wildcard_patterns() {
        let result = decompose_pattern(".*");
        assert!(result.iter().all(|v| v.is_empty()));
    }

    #[test]
    fn handles_nested_groups_in_alternation() {
        // (foo|bar)baz — parens are not top-level alternation, treated as single alternative
        let result = decompose_pattern("(foo|bar)baz");
        // Should not panic; result depends on implementation details
        assert!(result.len() >= 1);
    }

    #[test]
    fn char_class_not_treated_as_literal() {
        // [A-Z]olland should only produce trigrams from "olland", not from "A-Z"
        let result = decompose_pattern("[A-Z]olland");
        assert_eq!(result.len(), 1);
        assert!(result[0].contains(&[b'o', b'l', b'l']));
        assert!(result[0].contains(&[b'l', b'l', b'a']));
        assert!(!result[0].contains(&[b'A', b'-', b'Z']));
    }
}
