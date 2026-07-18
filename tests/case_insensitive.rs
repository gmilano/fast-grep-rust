//! Integration tests for the case-insensitive companion index (`fgr index -i`).
//!
//! A CI index stores trigrams over case-folded text so `(?i)` queries can be
//! answered from the index. These tests assert the indexed CI path returns
//! exactly what a full `(?i)` scan returns — including a Unicode simple-case
//! fold (U+212A KELVIN SIGN ≡ `k`) that a plain ASCII lowercase would miss —
//! and that incremental updates keep the CI index in sync.

use std::fs;
use std::path::Path;

use fast_grep::persist::{build as build_index, compact, load as load_index, update_incremental};
use fast_grep::searcher::{search_full_scan, search_persistent_timed, Match};

const FILES: &[(&str, &str)] = &[
    (
        "alpha.ts",
        "Hello WORLD\nexport function Foo() {}\nEXPORT_symbol(bar)",
    ),
    (
        "beta.rs",
        "let Temperature = 1;\n// MixedCase Identifier\nReturn VALUE",
    ),
    // U+212A KELVIN SIGN starts this word; `(?i)kelvin` must still match it.
    (
        "kfile.txt",
        "\u{212A}elvin sign appears here\nplain kelvin too",
    ),
];

fn setup() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("temp dir");
    for &(name, content) in FILES {
        let full = tmp.path().join(name);
        fs::write(&full, content).unwrap();
    }
    tmp
}

fn search_set(
    index: &fast_grep::persist::PersistentIndex,
    tmp: &Path,
    pattern: &str,
) -> Vec<String> {
    let mut v: Vec<String> = search_persistent_timed(index, pattern, None, false, &[], &[], &[])
        .expect("indexed search")
        .0
        .iter()
        .map(|m| line_key(m, tmp))
        .collect();
    v.sort();
    v
}

fn full_scan_set(tmp: &Path, pattern: &str) -> Vec<String> {
    let mut v: Vec<String> = search_full_scan(tmp, pattern, true, false, &[], &[], &[], false)
        .expect("full scan")
        .iter()
        .map(|m| line_key(m, tmp))
        .collect();
    v.sort();
    v
}

fn line_key(m: &Match, tmp: &Path) -> String {
    format!(
        "{}:{}",
        m.path.strip_prefix(tmp).unwrap().display(),
        m.line_number
    )
}

#[test]
fn ci_index_matches_full_scan_for_ignore_case_patterns() {
    let tmp = setup();
    let idx_dir = tmp.path().join(".fgr-ci");
    build_index(tmp.path(), &idx_dir, true, &[], false, true).expect("build CI index");
    let idx = load_index(&idx_dir).expect("load");
    assert!(idx.has_ci(), "index should carry a CI companion");

    // Mix of cases the query is unaware of; each must match its full (?i) scan.
    for pattern in [
        "(?i)hello",
        "(?i)EXPORT",
        "(?i)mixedcase",
        "(?i)return value",
    ] {
        assert_eq!(
            search_set(&idx, tmp.path(), pattern),
            full_scan_set(tmp.path(), pattern),
            "CI index vs full scan mismatch for {pattern:?}"
        );
    }
}

#[test]
fn ci_index_is_sound_for_unicode_case_fold() {
    let tmp = setup();
    let idx_dir = tmp.path().join(".fgr-ci");
    build_index(tmp.path(), &idx_dir, true, &[], false, true).expect("build CI index");
    let idx = load_index(&idx_dir).expect("load");

    // `(?i)kelvin` matches both the ASCII "kelvin" and the line that starts
    // with U+212A. The full scan finds both lines; the CI index must not drop
    // the Kelvin-sign line (which an ASCII-only fold would).
    let indexed = search_set(&idx, tmp.path(), "(?i)kelvin");
    let full = full_scan_set(tmp.path(), "(?i)kelvin");
    assert_eq!(indexed, full, "CI index unsound for Unicode case fold");
    assert!(
        indexed.iter().any(|k| k.starts_with("kfile.txt:1")),
        "Kelvin-sign line missing from results: {indexed:?}"
    );
}

#[test]
fn cs_only_index_still_correct_for_ignore_case() {
    // Without a CI companion, an `(?i)` query must fall back and still return
    // the right lines (via the all-files path), matching a full scan.
    let tmp = setup();
    let idx_dir = tmp.path().join(".fgr-cs");
    build_index(tmp.path(), &idx_dir, true, &[], false, false).expect("build CS index");
    let idx = load_index(&idx_dir).expect("load");
    assert!(!idx.has_ci());

    assert_eq!(
        search_set(&idx, tmp.path(), "(?i)hello"),
        full_scan_set(tmp.path(), "(?i)hello"),
    );
}

/// The full lifecycle a CI index goes through under the rebaseline feature:
/// build -i → mutate (add/modify/delete, mixed case) → update (CI delta) →
/// `compact()` slot swap (CI folded into the new slot) → further update (CI
/// delta must STILL be built against the compacted baseline) → compact again.
/// Every step's `(?i)` answers must match a full scan.
#[test]
fn ci_survives_update_and_slot_compact_cycle() {
    let tmp = setup();
    let idxtmp = tempfile::tempdir().unwrap();
    let idx_dir = idxtmp.path().join("idx");
    build_index(tmp.path(), &idx_dir, true, &[], false, true).expect("build CI index");

    // Mutate: add mixed-case, modify alpha.ts (mtime bumped for detection),
    // delete beta.rs.
    fs::write(tmp.path().join("gamma.ts"), "Goodbye MOON and STARS").unwrap();
    let modpath = tmp.path().join("alpha.ts");
    fs::write(&modpath, "Replaced CONTENT with NewCasing here").unwrap();
    let f = fs::OpenOptions::new().write(true).open(&modpath).unwrap();
    f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(10))
        .unwrap();
    drop(f);
    fs::remove_file(tmp.path().join("beta.rs")).unwrap();

    update_incremental(&idx_dir, tmp.path(), false).expect("update");

    // Pre-compact (delta + tombstones live): (?i) must already be exact.
    let idx = load_index(&idx_dir).expect("load pre-compact");
    for pattern in [
        "(?i)goodbye",
        "(?i)newcasing",
        "(?i)temperature",
        "(?i)kelvin",
    ] {
        assert_eq!(
            search_set(&idx, tmp.path(), pattern),
            full_scan_set(tmp.path(), pattern),
            "CI mismatch pre-compact for {pattern:?}"
        );
    }
    drop(idx);

    // Slot-swap compaction: CI store must be folded into the new slot.
    let outcome = compact(&idx_dir, false).expect("compact");
    assert!(outcome.compacted);
    let slot = fs::read_to_string(idx_dir.join("current")).unwrap();
    let slot = slot.trim();
    assert_eq!(slot, "slot-b", "compaction swapped the slot");
    assert!(
        idx_dir.join(slot).join("ngrams.ci.postings").exists(),
        "CI store missing from compacted slot"
    );

    let idx = load_index(&idx_dir).expect("load post-compact");
    assert!(idx.has_ci(), "CI companion lost across compaction");
    for pattern in [
        "(?i)goodbye",
        "(?i)newcasing",
        "(?i)temperature",
        "(?i)kelvin",
    ] {
        assert_eq!(
            search_set(&idx, tmp.path(), pattern),
            full_scan_set(tmp.path(), pattern),
            "CI mismatch post-compact for {pattern:?}"
        );
    }
    drop(idx);

    // A further update must still build the CI delta (meta.case_insensitive
    // carried through the compacted baseline).
    fs::write(tmp.path().join("delta2.ts"), "PostCompact MiXeD content").unwrap();
    update_incremental(&idx_dir, tmp.path(), false).expect("update post-compact");
    let idx = load_index(&idx_dir).expect("reload");
    assert!(idx.has_ci());
    let hits = search_set(&idx, tmp.path(), "(?i)postcompact");
    assert_eq!(
        hits,
        full_scan_set(tmp.path(), "(?i)postcompact"),
        "CI delta not built after compaction"
    );
    assert!(hits.iter().any(|k| k.starts_with("delta2.ts")));
    drop(idx);

    // Second compaction folds that CI delta too.
    assert!(compact(&idx_dir, false).expect("compact 2").compacted);
    let idx = load_index(&idx_dir).expect("load post-compact 2");
    assert_eq!(
        search_set(&idx, tmp.path(), "(?i)postcompact"),
        full_scan_set(tmp.path(), "(?i)postcompact"),
    );
}

#[test]
fn incremental_update_keeps_ci_index_in_sync() {
    let tmp = setup();
    let idx_dir = tmp.path().join(".fgr-ci");
    build_index(tmp.path(), &idx_dir, true, &[], false, true).expect("build CI index");

    // Add a new file with mixed-case content after the index was built.
    fs::write(tmp.path().join("gamma.ts"), "Goodbye MOON and STARS").unwrap();
    update_incremental(&idx_dir, tmp.path(), false).expect("update");

    let idx = load_index(&idx_dir).expect("reload");
    // The CI delta must make the new file visible to an (?i) search.
    let indexed = search_set(&idx, tmp.path(), "(?i)goodbye");
    let full = full_scan_set(tmp.path(), "(?i)goodbye");
    assert_eq!(indexed, full, "CI delta out of sync after update");
    assert!(
        indexed.iter().any(|k| k.starts_with("gamma.ts")),
        "new file not found via CI index: {indexed:?}"
    );
}
