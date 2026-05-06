//! Integration tests for the persistent index search path.
//!
//! Creates a temp directory with test files, builds a persistent index
//! into a sub-directory, loads it, and verifies search results match
//! expected output (literal matches, line numbers, nested files, full
//! scan equivalence).

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use fast_grep::persist::{build as build_index, load as load_index, PersistentIndex};
use fast_grep::searcher::{search_full_scan, search_persistent_timed, Match};

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
  "apiKey": "<placeholder-not-a-real-key>"
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

/// Build a persistent index inside the temp dir and return it loaded.
/// The index is written to a `.fgr-test` subdirectory so it doesn't
/// collide with anything the search results might match.
fn build_test_index(tmp: &Path) -> PersistentIndex {
    let idx_dir = tmp.join(".fgr-test");
    build_index(tmp, &idx_dir, true, None, false).expect("build persistent index");
    load_index(&idx_dir).expect("load persistent index")
}

fn search(index: &PersistentIndex, pattern: &str) -> Vec<Match> {
    search_persistent_timed(index, pattern, None, false)
        .expect("search")
        .0
}

#[test]
fn builds_index_with_correct_file_count() {
    let tmp = setup_test_dir();
    let idx = build_test_index(tmp.path());
    let all_results = search(&idx, ".*");
    let files: HashSet<_> = all_results.iter().map(|m| m.path.clone()).collect();
    assert_eq!(files.len(), TEST_FILES.len());
}

#[test]
fn finds_literal_string_matches() {
    let tmp = setup_test_dir();
    let idx = build_test_index(tmp.path());
    let results = search(&idx, "express");
    assert!(!results.is_empty());
    assert!(results
        .iter()
        .all(|r| r.path.file_name().unwrap() == "server.ts"));
}

#[test]
fn finds_pattern_across_multiple_files() {
    let tmp = setup_test_dir();
    let idx = build_test_index(tmp.path());
    let results = search(&idx, "function");
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
    let idx = build_test_index(tmp.path());
    let results = search(&idx, "useState");
    assert!(!results.is_empty());
    assert_eq!(results[0].line_number, 3);
}

#[test]
fn finds_matches_in_nested_files() {
    let tmp = setup_test_dir();
    let idx = build_test_index(tmp.path());
    let results = search(&idx, "DeepModule");
    assert!(!results.is_empty());
    assert!(results[0].path.ends_with("nested/deep/module.ts"));
}

#[test]
fn indexed_search_matches_full_scan() {
    let tmp = setup_test_dir();
    let idx = build_test_index(tmp.path());
    let patterns = ["function", "import", "localhost", "Hello", "constructor"];

    for pattern in &patterns {
        let mut indexed: Vec<String> = search(&idx, pattern)
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

        let mut full: Vec<String> = search_full_scan(tmp.path(), pattern, true, false, None)
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
    let idx = build_test_index(tmp.path());
    let results = search(&idx, "xyzxyzxyz_nonexistent");
    assert!(results.is_empty());
}
