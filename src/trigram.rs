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
                    'd' | 'D' | 'w' | 'W' | 's' | 'S' | 'b' | 'B' | 'A' | 'z' | 'Z' => {
                        // Not a literal
                        if !current.is_empty() {
                            runs.push(std::mem::take(&mut current));
                        }
                        chars.next();
                    }
                    _ => {
                        // Escaped literal (e.g., \. \* etc)
                        current.push(chars.next().unwrap());
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

    #[test]
    fn test_extract_trigrams() {
        let tris = extract_trigrams("hello");
        assert!(tris.contains(&[b'h', b'e', b'l']));
        assert!(tris.contains(&[b'e', b'l', b'l']));
        assert!(tris.contains(&[b'l', b'l', b'o']));
        assert_eq!(tris.len(), 3);
    }

    #[test]
    fn test_decompose_simple() {
        let result = decompose_pattern("hello");
        assert_eq!(result.len(), 1);
        assert!(!result[0].is_empty());
    }

    #[test]
    fn test_decompose_alternation() {
        let result = decompose_pattern("TODO|FIXME");
        assert_eq!(result.len(), 2);
    }
}
