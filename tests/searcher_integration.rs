//! Integration tests for Searcher — ported from searcher.test.ts
//!
//! Creates a temp directory with test files, builds the index, and verifies
//! search results match expected output (literal matches, line numbers,
//! nested files, full scan comparison).

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use fast_grep::searcher::{search_full_scan, Searcher};

/// Test file contents matching the TypeScript test suite
const TEST_FILES: &[(&str, &str)] = &[
    (
        "app.ts",
        "import React from 'react';
export function App() {
  const [count, setCount] = useState(0);
  return <div>Hello World</div>;
}",
    ),
    (
        "utils.ts",
        "export function capitalize(str: string): string {
  return str.charAt(0).toUpperCase() + str.slice(1);
}
export function isEmpty(val: unknown): boolean {
  return val === null || val === undefined;
}",
    ),
    (
        "server.ts",
        "import express from 'express';
const app = express();
app.get('/api/health', (req, res) => {
  res.json({ status: 'ok' });
});
app.listen(3000, () => console.log('Server running'));",
    ),
    (
        "config.json",
        r#"{
  "database": { "host": "localhost", "port": 5432 },
  "redis": { "host": "localhost", "port": 6379 },
  "apiKey": "sk-test-12345"
}"#,
    ),
    (
        "nested/deep/module.ts",
        "export class DeepModule {
  constructor(private name: string) {}
  greet() { return `Hello from ${this.name}`; }
}",
    ),
];

fn setup_test_dir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("failed to create temp dir");
    for &(file_path, content) in TEST_FILES {
        let full = tmp.path().join(file_path);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&full, content).unwrap();
    }
    tmp
}

#[test]
fn builds_index_with_correct_file_count() {
    let tmp = setup_test_dir();
    let searcher = Searcher::new(tmp.path(), true, None).unwrap();
    let all_results = searcher.search(".*").unwrap();
    let files: HashSet<_> = all_results.iter().map(|m| m.path.clone()).collect();
    assert_eq!(files.len(), TEST_FILES.len());
}

#[test]
fn finds_literal_string_matches() {
    let tmp = setup_test_dir();
    let searcher = Searcher::new(tmp.path(), true, None).unwrap();
    let results = searcher.search("express").unwrap();
    assert!(!results.is_empty());
    assert!(results
        .iter()
        .all(|r| r.path.file_name().unwrap() == "server.ts"));
}

#[test]
fn finds_pattern_across_multiple_files() {
    let tmp = setup_test_dir();
    let searcher = Searcher::new(tmp.path(), true, None).unwrap();
    let results = searcher.search("function").unwrap();
    let files: HashSet<_> = results
        .iter()
        .map(|r| r.path.file_name().unwrap().to_str().unwrap().to_string())
        .collect();
    assert!(files.contains("app.ts"));
    assert!(files.contains("utils.ts"));
}

#[test]
fn returns_correct_line_numbers() {
    let tmp = setup_test_dir();
    let searcher = Searcher::new(tmp.path(), true, None).unwrap();
    let results = searcher.search("useState").unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0].line_number, 3);
}

#[test]
fn finds_matches_in_nested_files() {
    let tmp = setup_test_dir();
    let searcher = Searcher::new(tmp.path(), true, None).unwrap();
    let results = searcher.search("DeepModule").unwrap();
    assert!(!results.is_empty());
    assert!(results[0]
        .path
        .ends_with("nested/deep/module.ts"));
}

#[test]
fn indexed_search_matches_full_scan() {
    let tmp = setup_test_dir();
    let searcher = Searcher::new(tmp.path(), true, None).unwrap();
    let patterns = ["function", "import", "localhost", "Hello", "constructor"];

    for pattern in &patterns {
        let mut indexed: Vec<String> = searcher
            .search(pattern)
            .unwrap()
            .iter()
            .map(|m| {
                format!(
                    "{}:{}",
                    m.path.strip_prefix(tmp.path()).unwrap().display(),
                    m.line_number
                )
            })
            .collect();
        indexed.sort();

        let mut full: Vec<String> = search_full_scan(tmp.path(), pattern, true, None)
            .unwrap()
            .iter()
            .map(|m| {
                format!(
                    "{}:{}",
                    m.path.strip_prefix(tmp.path()).unwrap().display(),
                    m.line_number
                )
            })
            .collect();
        full.sort();

        assert_eq!(
            indexed, full,
            "indexed vs full scan mismatch for pattern '{}'",
            pattern
        );
    }
}

#[test]
fn returns_empty_for_nonexistent_pattern() {
    let tmp = setup_test_dir();
    let searcher = Searcher::new(tmp.path(), true, None).unwrap();
    let results = searcher.search("xyzxyzxyz_nonexistent").unwrap();
    assert!(results.is_empty());
}
