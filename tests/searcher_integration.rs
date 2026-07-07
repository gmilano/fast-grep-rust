//! Integration tests for the persistent index search path.
//!
//! Creates a temp directory with test files, builds a persistent index
//! into a sub-directory, loads it, and verifies search results match
//! expected output (literal matches, line numbers, nested files, full
//! scan equivalence).

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use std::time::{Duration, SystemTime};

use fast_grep::persist::{
    acquire_index_lock, build as build_index, compact, compact_into, load as load_index,
    release_index_lock, try_acquire_index_lock, update_incremental, PersistentIndex,
};
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
    build_index(tmp, &idx_dir, true, &[], false, false).expect("build persistent index");
    load_index(&idx_dir).expect("load persistent index")
}

fn search(index: &PersistentIndex, pattern: &str) -> Vec<Match> {
    search_persistent_timed(index, pattern, None, false, &[], &[], &[])
        .expect("search")
        .0
}

/// Sorted `path:line` hits for a pattern — used to compare two indexes for
/// exact search-result equivalence.
fn hitset(index: &PersistentIndex, pattern: &str) -> Vec<String> {
    let mut v: Vec<String> = search(index, pattern)
        .iter()
        .map(|m| format!("{}:{}", m.path.display(), m.line_number))
        .collect();
    v.sort();
    v
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

        let mut full: Vec<String> =
            search_full_scan(tmp.path(), pattern, true, false, &[], &[], &[], false)
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

/// A fresh build materializes the slot layout: content lives under `slot-a`
/// and `current` points at it.
#[test]
fn build_materializes_slot_layout() {
    let tmp = setup_test_dir();
    let idx_dir = tmp.path().join(".fgr-slot");
    build_index(tmp.path(), &idx_dir, true, &[], false, false).expect("build");

    let current = fs::read_to_string(idx_dir.join("current")).expect("current pointer");
    assert_eq!(current.trim(), "slot-a");
    assert!(idx_dir.join("slot-a/meta.json").exists());
    // No flat content leaked into the root.
    assert!(!idx_dir.join("meta.json").exists());

    let idx = load_index(&idx_dir).expect("load");
    assert!(!search(&idx, "function").is_empty());
}

/// A pre-slot (flat) index on disk — no `current`, content files in the root —
/// still loads and searches, so existing indexes keep working without a rebuild.
#[test]
fn legacy_flat_index_still_loads() {
    let tmp = setup_test_dir();
    let idx_dir = tmp.path().join(".fgr-legacy");
    build_index(tmp.path(), &idx_dir, true, &[], false, false).expect("build");

    // Flatten: move the live slot's files up to the root, drop the slot
    // machinery — this is exactly a pre-slot on-disk layout.
    let slot = idx_dir.join("slot-a");
    for entry in fs::read_dir(&slot).unwrap() {
        let entry = entry.unwrap();
        fs::rename(entry.path(), idx_dir.join(entry.file_name())).unwrap();
    }
    fs::remove_dir_all(&slot).unwrap();
    fs::remove_file(idx_dir.join("current")).unwrap();

    let idx = load_index(&idx_dir).expect("load flat");
    let results = search(&idx, "express");
    assert!(!results.is_empty());
    assert!(results
        .iter()
        .all(|r| r.path.file_name().unwrap() == "server.ts"));
}

/// Rebuilding an existing index stages into the *other* slot, flips `current`,
/// and reclaims the previous slot — so exactly one slot dir remains.
#[test]
fn rebuild_alternates_slots() {
    let tmp = setup_test_dir();
    let idx_dir = tmp.path().join(".fgr-rebuild");

    build_index(tmp.path(), &idx_dir, true, &[], false, false).expect("build 1");
    assert_eq!(
        fs::read_to_string(idx_dir.join("current")).unwrap().trim(),
        "slot-a"
    );

    build_index(tmp.path(), &idx_dir, true, &[], false, false).expect("build 2");
    assert_eq!(
        fs::read_to_string(idx_dir.join("current")).unwrap().trim(),
        "slot-b"
    );
    assert!(idx_dir.join("slot-b/meta.json").exists());
    assert!(
        !idx_dir.join("slot-a").exists(),
        "stale slot should be reclaimed"
    );

    let idx = load_index(&idx_dir).expect("load after rebuild");
    assert!(!search(&idx, "function").is_empty());
}

/// An incremental update writes its delta into the live slot (not the root) and
/// the reloaded index reflects added / modified / deleted files.
#[test]
fn update_writes_delta_into_slot() {
    let tmp = setup_test_dir();
    let idx_dir = tmp.path().join(".fgr-upd");
    build_index(tmp.path(), &idx_dir, true, &[], false, false).expect("build");

    // Added file.
    fs::write(
        tmp.path().join("added.ts"),
        "export const zetaMarker = 42;\n",
    )
    .unwrap();
    // Modified file — bump mtime forward so the 2s-granularity check detects it
    // deterministically without a sleep.
    let modpath = tmp.path().join("utils.ts");
    fs::write(
        &modpath,
        "export function newlyModifiedFn() { return 7; }\n",
    )
    .unwrap();
    let f = fs::OpenOptions::new().write(true).open(&modpath).unwrap();
    f.set_modified(SystemTime::now() + Duration::from_secs(10))
        .unwrap();
    drop(f);
    // Deleted file.
    fs::remove_file(tmp.path().join("server.ts")).unwrap();

    update_incremental(&idx_dir, tmp.path(), false).expect("update");

    // Delta landed inside the slot, not the root.
    assert!(idx_dir.join("slot-a/delta.postings").exists());
    assert!(!idx_dir.join("delta.postings").exists());

    let idx = load_index(&idx_dir).expect("reload");
    assert!(
        !search(&idx, "zetaMarker").is_empty(),
        "added file searchable"
    );
    assert!(
        !search(&idx, "newlyModifiedFn").is_empty(),
        "modified content searchable"
    );
    assert!(
        search(&idx, "capitalize").is_empty(),
        "pre-modification content gone (old doc tombstoned)"
    );
    assert!(
        search(&idx, "express").is_empty(),
        "deleted file's content gone"
    );
}

/// A compacted baseline (delta folded, tombstones dropped, doc_ids densified)
/// answers every query identically to a full rebuild of the same on-disk state.
#[test]
fn compaction_matches_full_rebuild() {
    let tmp = setup_test_dir();
    // Keep all index dirs OUTSIDE the corpus, so a rebuild walking the corpus
    // never picks up a sibling index's files as documents.
    let idxtmp = tempfile::tempdir().unwrap();
    let idx_dir = idxtmp.path().join("cmp");
    build_index(tmp.path(), &idx_dir, true, &[], false, false).unwrap();

    // Mutate the corpus: add, modify (bump mtime so it's detected), delete.
    fs::write(
        tmp.path().join("added.ts"),
        "export const zetaMarker = 42;\nfunction addedHelper() {}\n",
    )
    .unwrap();
    let modpath = tmp.path().join("utils.ts");
    fs::write(
        &modpath,
        "export function newlyModifiedFn() { return 7; }\n",
    )
    .unwrap();
    let f = fs::OpenOptions::new().write(true).open(&modpath).unwrap();
    f.set_modified(SystemTime::now() + Duration::from_secs(10))
        .unwrap();
    drop(f);
    fs::remove_file(tmp.path().join("server.ts")).unwrap();

    // Update → delta + tombstones present.
    update_incremental(&idx_dir, tmp.path(), false).unwrap();
    let pidx = load_index(&idx_dir).unwrap();
    assert!(
        idx_dir.join("slot-a/delta.postings").exists(),
        "precondition: delta present before compaction"
    );

    // Compact into a standalone flat dir (no `current` → loads as flat).
    let comp_dir = idxtmp.path().join("compacted");
    let stats = compact_into(&pidx, &comp_dir).expect("compact");
    drop(pidx);
    assert!(
        !comp_dir.join("delta.postings").exists(),
        "no delta overlay"
    );
    assert!(!comp_dir.join("deleted.bin").exists(), "no tombstones");

    // Full rebuild of the current on-disk state, for reference.
    let rb_dir = idxtmp.path().join("rebuild");
    build_index(tmp.path(), &rb_dir, true, &[], false, false).unwrap();

    let comp = load_index(&comp_dir).unwrap();
    let rb = load_index(&rb_dir).unwrap();
    assert_eq!(
        stats.live_docs,
        rb.num_docs(),
        "same live doc count (compact={} rebuild={})",
        stats.live_docs,
        rb.num_docs()
    );

    for p in [
        "function",
        "zetaMarker",
        "newlyModifiedFn",
        "addedHelper",
        "export",
        "import",
        "Hello",
        "const",
        "return",
        "localhost",
    ] {
        assert_eq!(
            hitset(&comp, p),
            hitset(&rb, p),
            "compacted vs rebuild mismatch for '{p}'"
        );
    }
    // Deleted-file and modified-away content is gone in the compacted baseline.
    assert!(hitset(&comp, "express").is_empty(), "deleted file gone");
    assert!(hitset(&comp, "capitalize").is_empty(), "old content gone");
}

/// Compaction folds the case-insensitive companion (`ngrams.ci.*`) in lockstep,
/// so `(?i)` queries on a compacted CI index match a CI rebuild.
#[test]
fn compaction_matches_full_rebuild_case_insensitive() {
    let tmp = setup_test_dir();
    let idxtmp = tempfile::tempdir().unwrap();
    let idx_dir = idxtmp.path().join("cmp");
    build_index(tmp.path(), &idx_dir, true, &[], false, true).unwrap();

    // Add mixed-case content, modify, delete.
    fs::write(
        tmp.path().join("added.ts"),
        "const ZetaMARKER = 1;\nfunction MixedCaseHelper() {}\n",
    )
    .unwrap();
    let modpath = tmp.path().join("app.ts");
    fs::write(&modpath, "export function ReNamedThing() { return 1; }\n").unwrap();
    let f = fs::OpenOptions::new().write(true).open(&modpath).unwrap();
    f.set_modified(SystemTime::now() + Duration::from_secs(10))
        .unwrap();
    drop(f);
    fs::remove_file(tmp.path().join("server.ts")).unwrap();

    update_incremental(&idx_dir, tmp.path(), false).unwrap();
    let pidx = load_index(&idx_dir).unwrap();
    assert!(pidx.has_ci(), "precondition: CI companion present");

    let comp_dir = idxtmp.path().join("compacted");
    compact_into(&pidx, &comp_dir).expect("compact");
    drop(pidx);
    assert!(
        comp_dir.join("ngrams.ci.postings").exists(),
        "CI store folded into compacted baseline"
    );

    let rb_dir = idxtmp.path().join("rebuild");
    build_index(tmp.path(), &rb_dir, true, &[], false, true).unwrap();

    let comp = load_index(&comp_dir).unwrap();
    let rb = load_index(&rb_dir).unwrap();
    assert!(comp.has_ci());

    // Case-insensitive queries (mixed case in the pattern) must match a rebuild.
    for p in [
        "(?i)zetamarker",
        "(?i)mixedcasehelper",
        "(?i)renamedthing",
        "(?i)FUNCTION",
        "(?i)EXPORT",
    ] {
        assert_eq!(
            hitset(&comp, p),
            hitset(&rb, p),
            "compacted vs rebuild (CI) mismatch for '{p}'"
        );
    }
    // Deleted content gone even case-insensitively.
    assert!(hitset(&comp, "(?i)EXPRESS").is_empty(), "deleted file gone");
}

/// `compact()` swaps the live slot: it stages the folded baseline into the
/// non-live slot, flips `current`, reclaims the old slot, and clears the delta.
/// The result matches a full rebuild, and a second compact is a no-op.
#[test]
fn compact_swaps_slot_and_folds_delta() {
    let tmp = setup_test_dir();
    let idxtmp = tempfile::tempdir().unwrap();
    let idx_dir = idxtmp.path().join("idx");
    build_index(tmp.path(), &idx_dir, true, &[], false, false).unwrap();
    assert_eq!(
        fs::read_to_string(idx_dir.join("current")).unwrap().trim(),
        "slot-a"
    );

    // Mutate + update → delta + tombstones in slot-a.
    fs::write(tmp.path().join("added.ts"), "const zetaMarker = 5;\n").unwrap();
    fs::remove_file(tmp.path().join("server.ts")).unwrap();
    update_incremental(&idx_dir, tmp.path(), false).unwrap();
    assert!(idx_dir.join("slot-a/delta.postings").exists());

    // Compact → swap to slot-b, reclaim slot-a, no delta/tombstones left.
    let outcome = compact(&idx_dir, false).expect("compact");
    assert!(outcome.compacted);
    assert_eq!(
        fs::read_to_string(idx_dir.join("current")).unwrap().trim(),
        "slot-b"
    );
    assert!(!idx_dir.join("slot-a").exists(), "old slot reclaimed");
    assert!(
        !idx_dir.join("slot-b/delta.postings").exists(),
        "delta folded"
    );
    assert!(
        !idx_dir.join("slot-b/deleted.bin").exists(),
        "tombstones gone"
    );

    // Correctness vs a full rebuild of the current on-disk state.
    let rb_dir = idxtmp.path().join("rebuild");
    build_index(tmp.path(), &rb_dir, true, &[], false, false).unwrap();
    let comp = load_index(&idx_dir).unwrap();
    let rb = load_index(&rb_dir).unwrap();
    for p in ["function", "zetaMarker", "export", "import", "const"] {
        assert_eq!(hitset(&comp, p), hitset(&rb, p), "mismatch for '{p}'");
    }
    assert!(hitset(&comp, "express").is_empty(), "deleted file gone");
    drop(comp);

    // Second compact: nothing to fold → no-op, pointer unchanged.
    let again = compact(&idx_dir, false).expect("compact again");
    assert!(!again.compacted, "already compact");
    assert_eq!(
        fs::read_to_string(idx_dir.join("current")).unwrap().trim(),
        "slot-b"
    );
}

/// `build` drops a default `config.toml` in the index root (not a slot), and a
/// user's edits survive every update / compact / rebuild — it is never clobbered.
#[test]
fn config_written_on_build_and_survives_mutations() {
    let tmp = setup_test_dir();
    let idxtmp = tempfile::tempdir().unwrap();
    let idx_dir = idxtmp.path().join("idx");
    build_index(tmp.path(), &idx_dir, true, &[], false, false).unwrap();

    let cfg_path = idx_dir.join("config.toml");
    assert!(cfg_path.exists(), "default config written on build");
    assert!(
        !idx_dir.join("slot-a/config.toml").exists(),
        "config lives in the root, not a slot"
    );
    // Defaults load correctly.
    assert!(fast_grep::config::load(&idx_dir).compaction.auto);

    // User edits the file.
    fs::write(
        &cfg_path,
        "[compaction]\nauto = false\ndelta_docs_abs = 7\n",
    )
    .unwrap();

    // Mutate + update + compact — none of these may touch config.toml.
    fs::write(tmp.path().join("added.ts"), "let somethingNew = 1;\n").unwrap();
    update_incremental(&idx_dir, tmp.path(), false).unwrap();
    compact(&idx_dir, false).unwrap();
    // Rebuild too (write_default_if_absent must not clobber).
    build_index(tmp.path(), &idx_dir, true, &[], false, false).unwrap();

    let cfg = fast_grep::config::load(&idx_dir);
    assert!(!cfg.compaction.auto, "user edit preserved");
    assert_eq!(cfg.compaction.delta_docs_abs, 7, "user edit preserved");
}

fn current_slot(idx_dir: &Path) -> String {
    fs::read_to_string(idx_dir.join("current"))
        .unwrap()
        .trim()
        .to_string()
}

fn run_fgr_update(idx_dir: &Path, extra: &[&str]) {
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_fgr"))
        .arg("update")
        .arg("--index")
        .arg(idx_dir)
        .args(extra)
        .status()
        .expect("run fgr update");
    assert!(status.success(), "fgr update exited with {status}");
}

/// `fgr update` auto-compacts once divergence crosses the config threshold, and
/// `--no-compact` opts out of that for a single run.
#[test]
fn update_auto_compacts_past_threshold() {
    let tmp = setup_test_dir();
    let idxtmp = tempfile::tempdir().unwrap();
    let idx_dir = idxtmp.path().join("idx");
    build_index(tmp.path(), &idx_dir, true, &[], false, false).unwrap();
    // Aggressive thresholds so a single added file trips the auto-compaction.
    fs::write(
        idx_dir.join("config.toml"),
        "[compaction]\nauto = true\ndelta_docs_abs = 1\nmin_main_docs = 0\n",
    )
    .unwrap();
    assert_eq!(current_slot(&idx_dir), "slot-a");

    // Add a file, then `fgr update` → auto-compaction should fire and swap slot.
    fs::write(tmp.path().join("newfile.ts"), "const autoCompactMe = 1;\n").unwrap();
    run_fgr_update(&idx_dir, &[]);
    assert_eq!(
        current_slot(&idx_dir),
        "slot-b",
        "auto-compaction swapped the slot"
    );
    assert!(
        !idx_dir.join("slot-b/delta.postings").exists(),
        "delta folded away"
    );
    let idx = load_index(&idx_dir).unwrap();
    assert!(
        !search(&idx, "autoCompactMe").is_empty(),
        "added content searchable after auto-compact"
    );
    drop(idx);

    // With --no-compact, a further change updates the delta but must NOT swap.
    fs::write(tmp.path().join("newfile2.ts"), "const secondFile = 2;\n").unwrap();
    run_fgr_update(&idx_dir, &["--no-compact"]);
    assert_eq!(
        current_slot(&idx_dir),
        "slot-b",
        "--no-compact left the slot in place"
    );
    assert!(
        idx_dir.join("slot-b/delta.postings").exists(),
        "delta present, not folded"
    );
}

/// `try_acquire_index_lock` is non-blocking: it returns None while the lock is
/// held (letting the daemon skip an update round during a background
/// compaction) and Some once it is released.
#[test]
fn try_lock_is_nonblocking_when_held() {
    let tmp = tempfile::tempdir().unwrap();
    let idx = tmp.path();

    let (_held, _waited) = acquire_index_lock(idx).unwrap();
    assert!(
        try_acquire_index_lock(idx).unwrap().is_none(),
        "held → None (no wait)"
    );
    release_index_lock(idx);

    let got = try_acquire_index_lock(idx).unwrap();
    assert!(got.is_some(), "released → Some");
    drop(got);
    release_index_lock(idx);
}
