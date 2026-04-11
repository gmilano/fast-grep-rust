use std::collections::HashSet;

pub fn extract_trigrams(text: &str) -> HashSet<[u8; 3]> {
    let bytes = text.as_bytes();
    let mut set = HashSet::new();
    if bytes.len() < 3 {
        return set;
    }
    for w in bytes.windows(3) {
        set.insert([w[0], w[1], w[2]]);
    }
    set
}

/// Decompose a regex pattern into literal trigrams that must appear in any match.
/// Returns a Vec of Vec<[u8;3]> where the outer vec is OR alternatives,
/// and each inner vec is AND-required trigrams for that alternative.
pub fn decompose_pattern(pattern: &str) -> Vec<Vec<[u8; 3]>> {
    // Split on top-level '|' (not inside parens/brackets)
    let alternatives = split_alternatives(pattern);
    let mut result = Vec::new();
    for alt in &alternatives {
        let literals = extract_literal_runs(alt);
        let mut trigrams = Vec::new();
        for lit in &literals {
            let bytes = lit.as_bytes();
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

/// Like decompose_pattern but preserves trigram order (no sort/dedup).
/// Used for adjacency filtering with position masks.
pub fn decompose_pattern_ordered(pattern: &str) -> Vec<Vec<[u8; 3]>> {
    let alternatives = split_alternatives(pattern);
    let mut result = Vec::new();
    for alt in &alternatives {
        let literals = extract_literal_runs(alt);
        let mut trigrams = Vec::new();
        for lit in &literals {
            let bytes = lit.as_bytes();
            if bytes.len() >= 3 {
                for w in bytes.windows(3) {
                    trigrams.push([w[0], w[1], w[2]]);
                }
            }
        }
        result.push(trigrams);
    }
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
                    'd' | 'D' | 'w' | 'W' | 's' | 'S' | 'b' | 'B' | 'A' | 'z' | 'Z'
                    | 'p' | 'P' => {
                        // Not a literal — regex escape class
                        if !current.is_empty() {
                            runs.push(std::mem::take(&mut current));
                        }
                        chars.next();
                        // \p{...} and \P{...} — skip the brace-delimited property name
                        if (next == 'p' || next == 'P') && chars.peek() == Some(&'{') {
                            chars.next(); // skip '{'
                            while let Some(c) = chars.next() {
                                if c == '}' { break; }
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
                if c == '}' { break; }
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
                    '\\' => { chars.next(); }
                    '(' => depth += 1,
                    ')' => { depth -= 1; if depth == 0 { break; } }
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
                        if ":PimsxuU-<!=".contains(after) {
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

    // --- extract_trigrams tests (ported from trigram.test.ts) ---

    #[test]
    fn empty_string_returns_no_trigrams() {
        assert_eq!(extract_trigrams("").len(), 0);
    }

    #[test]
    fn short_string_returns_no_trigrams() {
        assert_eq!(extract_trigrams("ab").len(), 0);
    }

    #[test]
    fn extracts_single_trigram_from_3_char_string() {
        let result = extract_trigrams("abc");
        assert_eq!(result.len(), 1);
        assert!(result.contains(&[b'a', b'b', b'c']));
    }

    #[test]
    fn extracts_all_trigrams_from_a_word() {
        let result = extract_trigrams("hello");
        assert_eq!(result.len(), 3);
        assert!(result.contains(&[b'h', b'e', b'l']));
        assert!(result.contains(&[b'e', b'l', b'l']));
        assert!(result.contains(&[b'l', b'l', b'o']));
    }

    #[test]
    fn deduplicates_repeated_trigrams() {
        let result = extract_trigrams("aaaa");
        assert_eq!(result.len(), 1);
        assert!(result.contains(&[b'a', b'a', b'a']));
    }

    #[test]
    fn handles_unicode_characters() {
        let result = extract_trigrams("café");
        assert!(result.len() > 0);
        // "caf" should be present as bytes
        assert!(result.contains(&[b'c', b'a', b'f']));
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
