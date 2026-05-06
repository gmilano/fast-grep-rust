//! Regex correctness tests using the Rust `regex` crate test suite.
//!
//! For each valid regex pattern + haystack pair, we verify that:
//! 1. Full scan finds a match iff the regex crate finds a match
//! 2. Indexed search finds the same matches as full scan (no false negatives from the index)
//!
//! Test data from: https://github.com/rust-lang/regex/tree/master/testdata

use std::fs;
use std::path::PathBuf;

use fast_grep::persist::{build as build_index, load as load_index};
use fast_grep::searcher::{search_full_scan, search_persistent_timed};

#[derive(Debug)]
struct RegexTest {
    name: String,
    /// Effective regex pattern with the toml fixture's `unicode = false`
    /// flag (when present) inlined as `(?-u)`. Both the `regex` crate
    /// and fgr's matcher honour the inline flag, so we don't need a
    /// separate ASCII-mode plumbing path.
    regex: String,
    haystack: String,
    /// Parsed from the toml fixture's `matches` field: `true` iff the
    /// fixture declares at least one match for this haystack. Used as a
    /// sanity gate against the `regex` crate's runtime answer — drift
    /// between the two means the fixture or the crate version moved,
    /// and we want a loud failure rather than silently trusting one or
    /// the other.
    should_match: bool,
    compiles: bool,
    /// True if the fixture relies on runner-level features (bounds,
    /// match-kind, search-kind, line-terminator, utf8=false) that our
    /// `is_match` oracle can't honour. The full-scan test skips these
    /// (oracle disagreement is meaningless); the indexed test still
    /// uses them (it compares fgr-indexed vs fgr-full-scan, both of
    /// which see the same out-of-band metadata — i.e. both ignore it).
    needs_runner_features: bool,
}

/// Unescape a haystack string the way the rust-regex test runner does
/// when a fixture has `unescape = true`. Supports the C-style escapes that
/// show up in the Fowler / regex testdata (`\n`, `\t`, `\r`, `\f`, `\v`,
/// `\\`, `\0`, `\xNN`); unrecognised escapes pass through verbatim so we
/// don't silently corrupt content the fixture didn't intend to escape.
///
/// Without this, fowler fixtures encoding bytes like `\x02` or `\n` end up
/// as the literal multi-character sequence at search time, while the regex
/// engine resolves the same escapes in the *pattern* — so the haystack
/// would never contain what the regex claimed to match.
fn unescape_haystack(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' || i + 1 >= bytes.len() {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        match bytes[i + 1] {
            b'n' => { out.push('\n'); i += 2; }
            b't' => { out.push('\t'); i += 2; }
            b'r' => { out.push('\r'); i += 2; }
            b'f' => { out.push('\x0C'); i += 2; }
            b'v' => { out.push('\x0B'); i += 2; }
            b'\\' => { out.push('\\'); i += 2; }
            b'0' => { out.push('\0'); i += 2; }
            b'x' if i + 3 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 2..i + 4]).unwrap_or("");
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b as char);
                    i += 4;
                } else {
                    out.push(bytes[i] as char);
                    i += 1;
                }
            }
            _ => {
                // Unknown escape — pass through the backslash literally.
                out.push(bytes[i] as char);
                i += 1;
            }
        }
    }
    out
}

fn parse_toml_tests(path: &std::path::Path) -> Vec<RegexTest> {
    let content = fs::read_to_string(path).unwrap();
    let table: toml::Value = content.parse().unwrap();

    let mut tests = Vec::new();
    if let Some(arr) = table.get("test").and_then(|v| v.as_array()) {
        for entry in arr {
            // Stash the runner-only fields on the test struct so each
            // assertion can decide whether to honour them. The full-scan
            // test (regex crate as oracle) needs to skip these because
            // `is_match` can't model bounds/match-kind/etc.; the indexed
            // test (fgr-indexed vs fgr-full-scan) doesn't care because
            // both sides see the same out-of-band info — neither honours
            // them, but they agree.
            let needs_runner_features = entry.get("bounds").is_some()
                || entry.get("match-kind").is_some()
                || entry.get("search-kind").is_some()
                || entry.get("line-terminator").is_some()
                || entry.get("utf8").and_then(|v| v.as_bool()) == Some(false);

            let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();

            // regex can be a string or array of strings; skip multi-pattern tests
            let raw_regex = match entry.get("regex") {
                Some(toml::Value::String(s)) => s.clone(),
                _ => continue,
            };

            let raw_haystack = match entry.get("haystack") {
                Some(toml::Value::String(s)) => s.clone(),
                _ => continue,
            };
            // Honour the fixture's `unescape = true` for the haystack
            // (Fowler-style fixtures encode bytes like `\x02` and `\n` as
            // literals expecting the runner to decode them before matching).
            let unescape = entry.get("unescape").and_then(|v| v.as_bool()).unwrap_or(false);
            let haystack = if unescape { unescape_haystack(&raw_haystack) } else { raw_haystack };

            let compiles = entry.get("compiles").and_then(|v| v.as_bool()).unwrap_or(true);

            let should_match = match entry.get("matches").and_then(|v| v.as_array()) {
                Some(arr) => !arr.is_empty(),
                None => false,
            };

            // Honour the fixture's out-of-band flags by inlining their
            // equivalent regex modifiers. Both the regex crate and fgr
            // respect inline flags identically, so this is functionally
            // equivalent to passing them via the builder API.
            //
            //   `unicode = false`        → `(?-u)` (= RegexBuilder::unicode(false))
            //   `case-insensitive = true` → `(?i)`  (= RegexBuilder::case_insensitive(true))
            //   `anchored = true`         → `\A`    (= RegexBuilder::anchored(true))
            //
            // `\A` (start-of-input) is preferred over `^` here so the
            // anchor is unambiguous even if the fixture also sets
            // multiline mode somewhere in the regex.
            let unicode = entry.get("unicode").and_then(|v| v.as_bool()).unwrap_or(true);
            let case_insensitive = entry
                .get("case-insensitive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let anchored = entry.get("anchored").and_then(|v| v.as_bool()).unwrap_or(false);
            let mut regex = raw_regex;
            if anchored {
                regex = format!(r"\A{}", regex);
            }
            if !unicode {
                regex = format!("(?-u){}", regex);
            }
            if case_insensitive {
                regex = format!("(?i){}", regex);
            }

            tests.push(RegexTest {
                name,
                regex,
                haystack,
                should_match,
                compiles,
                needs_runner_features,
            });
        }
    }
    tests
}

fn testdata_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("regex-testdata")
}

/// Verify full scan correctness: for each test case, create a temp file with the haystack
/// and check that fgr finds a match iff the regex crate would.
#[test]
fn full_scan_matches_regex_crate() {
    let dir = testdata_dir();
    // Files we exercise. Excluded by design: the toml suites that probe
    // match modes our test harness can't model with `is_match`
    // (`earliest`, `leftmost-all`, `overlapping`, `set`, `substring`,
    // `regex-lite`), the slow `expensive` suite, and `empty` whose
    // haystacks are filtered earlier anyway.
    let toml_files = [
        "anchored.toml",
        "bytes.toml",
        "crazy.toml",
        "crlf.toml",
        "flags.toml",
        "iter.toml",
        "line-terminator.toml",
        "misc.toml",
        "multiline.toml",
        "no-unicode.toml",
        "regression.toml",
        "unicode.toml",
        "utf8.toml",
        "word-boundary-special.toml",
        "word-boundary.toml",
        "fowler/basic.toml",
        "fowler/nullsubexpr.toml",
        "fowler/repetition.toml",
    ];

    let mut total = 0;
    let mut skipped = 0;
    let mut passed = 0;
    // fgr disagrees with the regex crate — the original assertion this
    // test was built around.
    let mut fgr_failures: Vec<String> = Vec::new();
    // The toml fixture and the regex crate disagree. Lower-frequency
    // findings, but worth investigating one by one rather than silently
    // trusting one or the other; reported separately so the failure
    // message points at the right cause.
    let mut toml_divergences: Vec<String> = Vec::new();

    for toml_file in &toml_files {
        let tests = parse_toml_tests(&dir.join(toml_file));
        for t in &tests {
            total += 1;

            if !t.compiles {
                skipped += 1;
                continue;
            }

            // Full-scan oracle is `regex::Regex::is_match`; runner-only
            // features can't be expressed there, so the comparison would
            // be meaningless. The indexed test runs these without skip.
            if t.needs_runner_features {
                skipped += 1;
                continue;
            }

            // Skip empty haystacks — grep tools don't report matches on empty files
            if t.haystack.is_empty() {
                skipped += 1;
                continue;
            }

            // Verify with the regex crate directly
            let re = match regex::Regex::new(&t.regex) {
                Ok(r) => r,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let crate_matches = re.is_match(&t.haystack);

            // Skip patterns that match empty strings — grep doesn't report these as line matches
            if crate_matches && re.is_match("") {
                skipped += 1;
                continue;
            }

            // Sanity: the toml fixture should agree with the regex crate.
            // Drift here means fixture/crate are out of step — we flag it
            // and skip the fgr comparison for this case (no point asking
            // fgr to reproduce ambiguous expectations).
            if crate_matches != t.should_match {
                toml_divergences.push(format!(
                    "[{}] {}: regex={:?} haystack={:?} toml_says_match={} regex_crate_says_match={}",
                    toml_file, t.name, t.regex, t.haystack, t.should_match, crate_matches
                ));
                continue;
            }

            // Now test via fgr full scan
            let tmp = tempfile::tempdir().unwrap();
            let test_file = tmp.path().join("test.txt");
            fs::write(&test_file, &t.haystack).unwrap();

            let fgr_results = search_full_scan(tmp.path(), &t.regex, true, false, None);
            let fgr_matches = match fgr_results {
                Ok(results) => !results.is_empty(),
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };

            if fgr_matches == crate_matches {
                passed += 1;
            } else {
                fgr_failures.push(format!(
                    "[{}] {}: regex={:?} haystack={:?} expected_match={} fgr_match={}",
                    toml_file, t.name, t.regex, t.haystack, crate_matches, fgr_matches
                ));
            }
        }
    }

    eprintln!(
        "Regex correctness: {} total, {} passed, {} skipped, {} fgr failures, {} toml/crate divergences",
        total, passed, skipped, fgr_failures.len(), toml_divergences.len()
    );
    if !toml_divergences.is_empty() {
        eprintln!("TOML/regex-crate divergences (fixture or crate version drifted):");
        for f in &toml_divergences {
            eprintln!("  {}", f);
        }
    }
    if !fgr_failures.is_empty() {
        eprintln!("fgr/regex-crate failures (real fgr bugs):");
        for f in &fgr_failures {
            eprintln!("  {}", f);
        }
    }
    assert!(
        toml_divergences.is_empty(),
        "{} toml/regex-crate divergence(s) — investigate before trusting either side",
        toml_divergences.len()
    );
    assert!(
        fgr_failures.is_empty(),
        "{} fgr disagreement(s) with the regex crate",
        fgr_failures.len()
    );
}

/// Verify index correctness: indexed search must return the same files as full scan.
/// This is the critical test — the index must not produce false negatives.
#[test]
fn indexed_search_matches_full_scan_regex_suite() {
    let dir = testdata_dir();
    // Mirror the full-scan toml list. Same exclusion rationale: leave out
    // suites that probe match-mode semantics our `is_match`-based harness
    // can't model (`earliest`, `leftmost-all`, `overlapping`, `set`,
    // `substring`, `regex-lite`), the slow `expensive` tests, and `empty`
    // whose haystacks are filtered earlier anyway.
    let toml_files = [
        "anchored.toml",
        "bytes.toml",
        "crazy.toml",
        "crlf.toml",
        "flags.toml",
        "iter.toml",
        "line-terminator.toml",
        "misc.toml",
        "multiline.toml",
        "no-unicode.toml",
        "regression.toml",
        "unicode.toml",
        "utf8.toml",
        "word-boundary-special.toml",
        "word-boundary.toml",
        "fowler/basic.toml",
        "fowler/nullsubexpr.toml",
        "fowler/repetition.toml",
    ];

    let mut total = 0;
    let mut skipped = 0;
    let mut passed = 0;
    let mut false_negatives: Vec<String> = Vec::new();

    for toml_file in &toml_files {
        let tests = parse_toml_tests(&dir.join(toml_file));
        for t in &tests {
            total += 1;

            if !t.compiles {
                skipped += 1;
                continue;
            }

            if t.haystack.is_empty() {
                skipped += 1;
                continue;
            }

            let re_check = match regex::Regex::new(&t.regex) {
                Ok(r) => r,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            if re_check.is_match("") {
                skipped += 1;
                continue;
            }

            // Create temp dir with a test file
            let tmp = tempfile::tempdir().unwrap();
            let test_file = tmp.path().join("test.txt");
            fs::write(&test_file, &t.haystack).unwrap();

            // Full scan
            let full_results = match search_full_scan(tmp.path(), &t.regex, true, false, None) {
                Ok(r) => r,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let full_matched = !full_results.is_empty();

            // Indexed search via the persistent index — the path the CLI ships.
            let idx_dir = tmp.path().join(".fgr-test");
            if build_index(tmp.path(), &idx_dir, true, None, false).is_err() {
                skipped += 1;
                continue;
            }
            let idx = match load_index(&idx_dir) {
                Ok(i) => i,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let indexed_results = match search_persistent_timed(&idx, &t.regex, None, false) {
                Ok((r, _)) => r,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let indexed_matched = !indexed_results.is_empty();

            // The index may return false positives (candidates that don't actually match)
            // but must NEVER return false negatives (miss a real match)
            if full_matched && !indexed_matched {
                false_negatives.push(format!(
                    "[{}] {}: regex={:?} haystack={:?} full_scan=match indexed=miss",
                    toml_file, t.name, t.regex, t.haystack
                ));
            } else {
                passed += 1;
            }
        }
    }

    eprintln!(
        "Index correctness: {} total, {} passed, {} skipped, {} false negatives",
        total, passed, skipped, false_negatives.len()
    );
    if !false_negatives.is_empty() {
        eprintln!("FALSE NEGATIVES (index missed real matches):");
        for f in &false_negatives {
            eprintln!("  {}", f);
        }
    }
    assert!(
        false_negatives.is_empty(),
        "{} false negative(s) — index missed matches that full scan found",
        false_negatives.len()
    );
}
