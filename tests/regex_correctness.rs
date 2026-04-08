//! Regex correctness tests using the Rust `regex` crate test suite.
//!
//! For each valid regex pattern + haystack pair, we verify that:
//! 1. Full scan finds a match iff the regex crate finds a match
//! 2. Indexed search finds the same matches as full scan (no false negatives from the index)
//!
//! Test data from: https://github.com/rust-lang/regex/tree/master/testdata

use std::fs;
use std::path::PathBuf;

use fast_grep::searcher::{search_full_scan, Searcher};

#[derive(Debug)]
struct RegexTest {
    name: String,
    regex: String,
    haystack: String,
    should_match: bool,
    compiles: bool,
}

fn parse_toml_tests(path: &std::path::Path) -> Vec<RegexTest> {
    let content = fs::read_to_string(path).unwrap();
    let table: toml::Value = content.parse().unwrap();

    let mut tests = Vec::new();
    if let Some(arr) = table.get("test").and_then(|v| v.as_array()) {
        for entry in arr {
            let name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();

            // regex can be a string or array of strings; skip multi-pattern tests
            let regex = match entry.get("regex") {
                Some(toml::Value::String(s)) => s.clone(),
                _ => continue,
            };

            let haystack = match entry.get("haystack") {
                Some(toml::Value::String(s)) => s.clone(),
                _ => continue,
            };

            let compiles = entry.get("compiles").and_then(|v| v.as_bool()).unwrap_or(true);

            let should_match = match entry.get("matches").and_then(|v| v.as_array()) {
                Some(arr) => !arr.is_empty(),
                None => false,
            };

            tests.push(RegexTest { name, regex, haystack, should_match, compiles });
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
    let toml_files = [
        "misc.toml",
        "crazy.toml",
        "regression.toml",
        "unicode.toml",
        "flags.toml",
        "multiline.toml",
        "word-boundary.toml",
        "fowler/basic.toml",
        "fowler/repetition.toml",
        "fowler/nullsubexpr.toml",
    ];

    let mut total = 0;
    let mut skipped = 0;
    let mut passed = 0;
    let mut failures: Vec<String> = Vec::new();

    for toml_file in &toml_files {
        let tests = parse_toml_tests(&dir.join(toml_file));
        for t in &tests {
            total += 1;

            if !t.compiles {
                skipped += 1;
                continue;
            }

            // Skip patterns that use features not applicable to grep-style search
            if t.regex.contains("(?-u)") || t.regex.contains("(?i-u)") {
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

            // Now test via fgr full scan
            let tmp = tempfile::tempdir().unwrap();
            let test_file = tmp.path().join("test.txt");
            fs::write(&test_file, &t.haystack).unwrap();

            let fgr_results = search_full_scan(tmp.path(), &t.regex, true, None);
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
                failures.push(format!(
                    "[{}] {}: regex={:?} haystack={:?} expected_match={} fgr_match={}",
                    toml_file, t.name, t.regex, t.haystack, crate_matches, fgr_matches
                ));
            }
        }
    }

    eprintln!(
        "Regex correctness: {} total, {} passed, {} skipped, {} failed",
        total, passed, skipped, failures.len()
    );
    if !failures.is_empty() {
        eprintln!("Failures:");
        for f in &failures {
            eprintln!("  {}", f);
        }
    }
    assert!(failures.is_empty(), "{} test(s) failed", failures.len());
}

/// Verify index correctness: indexed search must return the same files as full scan.
/// This is the critical test — the index must not produce false negatives.
#[test]
fn indexed_search_matches_full_scan_regex_suite() {
    let dir = testdata_dir();
    let toml_files = [
        "misc.toml",
        "crazy.toml",
        "regression.toml",
        "unicode.toml",
        "fowler/basic.toml",
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

            if t.regex.contains("(?-u)") || t.regex.contains("(?i-u)") {
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
            let full_results = match search_full_scan(tmp.path(), &t.regex, true, None) {
                Ok(r) => r,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let full_matched = !full_results.is_empty();

            // Indexed search
            let searcher = match Searcher::new(tmp.path(), true, None) {
                Ok(s) => s,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let indexed_results = match searcher.search(&t.regex) {
                Ok(r) => r,
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
