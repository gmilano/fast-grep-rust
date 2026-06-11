//! Unicode simple case folding for the case-insensitive (CI) trigram index.
//!
//! The CI index stores trigrams over *case-folded* text so that an `(?i)`
//! search can be answered from the index. For the trigram filter to be
//! **sound** (never drop a real match) it must fold with at least the same
//! equivalence classes the regex engine uses for `(?i)`. The `regex` crate
//! uses Unicode *simple* case folding (1:1 per character), so we fold both the
//! indexed content and the query literals the same way.
//!
//! A plain ASCII lowercase would be unsound: e.g. the Kelvin sign `K` (U+212A)
//! is case-insensitively equal to `k` for the regex, but ASCII-lowercasing
//! leaves it untouched — a line containing `K` would then be dropped by the
//! filter even though `(?i)k` matches it. So non-ASCII text takes the full
//! Unicode fold; pure-ASCII text (the overwhelming common case) takes a fast
//! `to_ascii_lowercase` path, which yields the same canonical representative
//! for ASCII letters as the full fold would.

use std::cell::RefCell;
use std::collections::HashMap;

use regex_syntax::hir::{ClassUnicode, ClassUnicodeRange};

thread_local! {
    /// Memoizes the canonical fold of each non-ASCII char seen on this thread.
    /// Building a `ClassUnicode` per char is comparatively expensive, but
    /// non-ASCII chars are rare and highly repetitive within a corpus.
    static FOLD_MEMO: RefCell<HashMap<char, char>> = RefCell::new(HashMap::new());
}

/// The canonical simple-case-fold representative of `c`: the smallest char in
/// `c`'s simple-case-fold equivalence class. All chars the regex treats as
/// case-insensitively equal map to the same representative, so folding both
/// sides of a comparison with this function preserves every `(?i)` match.
fn canonical_fold_char(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    FOLD_MEMO.with(|memo| {
        if let Some(&f) = memo.borrow().get(&c) {
            return f;
        }
        let f = compute_fold(c);
        memo.borrow_mut().insert(c, f);
        f
    })
}

fn compute_fold(c: char) -> char {
    let mut class = ClassUnicode::new([ClassUnicodeRange::new(c, c)]);
    // `try_case_fold_simple` adds every char simple-case-fold-equivalent to the
    // ones already in the class. The class is then the full equivalence class;
    // its smallest member is a stable canonical representative.
    if class.try_case_fold_simple().is_err() {
        return c; // class grew too large to fold; leave the char as-is
    }
    let min = class.iter().map(|r| r.start()).min().unwrap_or(c);
    // If the class contains an ASCII letter it is necessarily the minimum (ASCII
    // < every non-ASCII char), and it is the *uppercase* form (e.g. 'K' < 'k').
    // Lower it so the representative matches the ASCII fast path, which maps both
    // ASCII case forms — and any non-ASCII member like U+212A — to ASCII 'k'.
    min.to_ascii_lowercase()
}

/// Case-fold `line` into `out` (cleared first). ASCII bytes are lowercased in a
/// fast pass; a line with any non-ASCII byte is decoded as UTF-8 (lossily, so
/// invalid bytes can't abort the build) and folded char by char. The output is
/// the byte sequence the CI index extracts trigrams from.
pub fn fold_into(line: &[u8], out: &mut Vec<u8>) {
    out.clear();
    if line.is_ascii() {
        out.reserve(line.len());
        for &b in line {
            out.push(b.to_ascii_lowercase());
        }
        return;
    }
    let s = String::from_utf8_lossy(line);
    let mut buf = [0u8; 4];
    for ch in s.chars() {
        let f = canonical_fold_char(ch);
        out.extend_from_slice(f.encode_utf8(&mut buf).as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fold(s: &str) -> Vec<u8> {
        let mut out = Vec::new();
        fold_into(s.as_bytes(), &mut out);
        out
    }

    #[test]
    fn ascii_lowercases() {
        assert_eq!(fold("Hello WORLD 123"), b"hello world 123");
    }

    #[test]
    fn empty_and_short() {
        assert_eq!(fold(""), b"");
        assert_eq!(fold("A"), b"a");
    }

    #[test]
    fn kelvin_sign_folds_to_k() {
        // U+212A KELVIN SIGN is (?i)-equal to 'k'; ASCII lowercase would not
        // touch it. The canonical fold must collapse it to ASCII 'k'.
        assert_eq!(fold("\u{212A}"), b"k");
        assert_eq!(fold("\u{212A}elvin"), b"kelvin");
    }

    #[test]
    fn greek_sigma_variants_share_a_representative() {
        // Σ (U+03A3), σ (U+03C3) and final ς (U+03C2) are all (?i)-equal.
        let cap = fold("\u{03A3}");
        let small = fold("\u{03C3}");
        let final_sigma = fold("\u{03C2}");
        assert_eq!(cap, small);
        assert_eq!(small, final_sigma);
    }

    #[test]
    fn latin_accented_folds_consistently() {
        // Uppercase and lowercase é must fold to the same bytes.
        assert_eq!(fold("\u{00C9}"), fold("\u{00E9}"));
    }

    #[test]
    fn fold_is_idempotent_for_ascii_and_unicode() {
        for s in ["function", "K", "\u{212A}", "\u{03A3}\u{03C2}"] {
            let once = fold(s);
            let mut twice = Vec::new();
            fold_into(&once, &mut twice);
            assert_eq!(once, twice, "fold not idempotent for {s:?}");
        }
    }
}
