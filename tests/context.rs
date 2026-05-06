//! End-to-end tests for `-A` / `-B` / `-C` context flags.
//!
//! Exercises `render::search_full_scan_render` and
//! `render::search_persistent_render` (the same paths the CLI takes when
//! `--context > 0`) with in-memory output sinks. The chunk-merging and
//! line-emission semantics are covered in `render.rs::render_tests` at
//! the unit level; this file pins the cross-file behaviour, dispatch
//! ordering, and indexed-vs-direct equivalence.

use std::fs;
use std::path::Path;
use std::sync::Mutex;

use fast_grep::persist::{build as build_index, load as load_index};
use fast_grep::render::{
    search_full_scan_render, search_persistent_render, ContextOpts, Dispatch, RenderOpts,
};

fn sink() -> Mutex<Vec<u8>> {
    Mutex::new(Vec::new())
}

fn render_opts(heading: bool, color: bool, pattern: &str) -> RenderOpts {
    RenderOpts {
        heading,
        color,
        pattern: Some(pattern.to_string()),
    }
}

fn write_files(tmp: &Path, files: &[(&str, &str)]) {
    for (name, content) in files {
        let full = tmp.join(name);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&full, content).unwrap();
    }
}

/// Build a persistent index inside the temp dir at `.fgr-test`.
fn build_test_index(tmp: &Path) -> fast_grep::persist::PersistentIndex {
    let idx_dir = tmp.join(".fgr-test");
    build_index(tmp, &idx_dir, true, None, false).expect("build index");
    load_index(&idx_dir).expect("load index")
}

// --- direct (no-index) full scan ---

#[test]
fn full_scan_zero_context_one_file() {
    let tmp = tempfile::tempdir().unwrap();
    write_files(tmp.path(), &[("a.txt", "alpha\nbeta\ngamma\n")]);
    let out = sink();

    let n = search_full_scan_render(
        tmp.path(),
        "beta",
        true,  // no_ignore
        false, // hidden
        None,
        &ContextOpts::default(),
        &render_opts(false, false, "beta"),
        Dispatch::Streaming,
        &out,
    )
    .unwrap();

    assert_eq!(n, 1);
    let s = String::from_utf8(out.into_inner().unwrap()).unwrap();
    assert!(s.contains(":2:beta\n"));
}

#[test]
fn full_scan_with_context_emits_separator_between_distant_chunks() {
    let tmp = tempfile::tempdir().unwrap();
    let content = "a\nM\nb\nc\nd\ne\nf\ng\nM\nh\n";
    write_files(tmp.path(), &[("f.txt", content)]);
    let out = sink();

    search_full_scan_render(
        tmp.path(),
        "M",
        true,
        false,
        None,
        &ContextOpts {
            before: 1,
            after: 1,
        },
        &render_opts(false, false, "M"),
        Dispatch::Streaming,
        &out,
    )
    .unwrap();

    let s = String::from_utf8(out.into_inner().unwrap()).unwrap();
    // Two chunks (lines 2 and 9) separated by `--`.
    assert!(s.contains("\n--\n"), "expected `--` separator, got:\n{}", s);
    // Both matches present.
    assert!(s.contains(":2:M\n"));
    assert!(s.contains(":9:M\n"));
}

#[test]
fn full_scan_sorted_dispatch_orders_files() {
    let tmp = tempfile::tempdir().unwrap();
    write_files(
        tmp.path(),
        &[
            ("z.txt", "MATCH in z\n"),
            ("a.txt", "MATCH in a\n"),
            ("m.txt", "MATCH in m\n"),
        ],
    );
    let out = sink();

    search_full_scan_render(
        tmp.path(),
        "MATCH",
        true,
        false,
        None,
        &ContextOpts::default(),
        &render_opts(false, false, "MATCH"),
        Dispatch::Sorted,
        &out,
    )
    .unwrap();

    let s = String::from_utf8(out.into_inner().unwrap()).unwrap();
    let pos_a = s.find("a.txt").expect("a.txt missing");
    let pos_m = s.find("m.txt").expect("m.txt missing");
    let pos_z = s.find("z.txt").expect("z.txt missing");
    assert!(
        pos_a < pos_m && pos_m < pos_z,
        "files not in sorted order:\n{}",
        s
    );
}

#[test]
fn heading_mode_emits_path_once_per_file() {
    let tmp = tempfile::tempdir().unwrap();
    write_files(tmp.path(), &[("f.txt", "hit\nmiss\nhit\nmiss\nhit\n")]);
    let out = sink();

    search_full_scan_render(
        tmp.path(),
        "hit",
        true,
        false,
        None,
        &ContextOpts::default(),
        &render_opts(true, false, "hit"),
        Dispatch::Sorted,
        &out,
    )
    .unwrap();

    let s = String::from_utf8(out.into_inner().unwrap()).unwrap();
    // Path header appears exactly once.
    let header_count = s.matches("f.txt\n").count();
    assert_eq!(header_count, 1, "expected one path header, output:\n{}", s);
    // Indented match lines (no path prefix).
    assert!(s.contains("\n1:hit\n"));
    assert!(s.contains("\n3:hit\n"));
    assert!(s.contains("\n5:hit\n"));
}

#[test]
fn heading_mode_with_context_uses_dash_for_context() {
    let tmp = tempfile::tempdir().unwrap();
    write_files(tmp.path(), &[("f.txt", "a\nM\nb\n")]);
    let out = sink();

    search_full_scan_render(
        tmp.path(),
        "M",
        true,
        false,
        None,
        &ContextOpts {
            before: 1,
            after: 1,
        },
        &render_opts(true, false, "M"),
        Dispatch::Sorted,
        &out,
    )
    .unwrap();

    let s = String::from_utf8(out.into_inner().unwrap()).unwrap();
    assert!(
        s.contains("\n1-a\n"),
        "context line missing `-` delimiter:\n{}",
        s
    );
    assert!(s.contains("\n2:M\n"));
    assert!(s.contains("\n3-b\n"));
}

// --- indexed (persistent) search ---

#[test]
fn indexed_search_with_context_matches_full_scan() {
    let tmp = tempfile::tempdir().unwrap();
    let content =
        "alpha\nbravo\nMATCH one\ncharlie\ndelta\necho\nfoxtrot\nMATCH two\ngolf\nhotel\n";
    write_files(tmp.path(), &[("f.txt", content)]);

    let ctx = ContextOpts {
        before: 1,
        after: 1,
    };
    let opts = render_opts(false, false, "MATCH");

    let scan_out = sink();
    search_full_scan_render(
        tmp.path(),
        "MATCH",
        true,
        false,
        None,
        &ctx,
        &opts,
        Dispatch::Sorted,
        &scan_out,
    )
    .unwrap();

    let idx = build_test_index(tmp.path());
    let idx_out = sink();
    search_persistent_render(
        &idx,
        "MATCH",
        None,
        false,
        &ctx,
        &opts,
        Dispatch::Sorted,
        &idx_out,
    )
    .unwrap();

    let s_scan = String::from_utf8(scan_out.into_inner().unwrap()).unwrap();
    let s_idx = String::from_utf8(idx_out.into_inner().unwrap()).unwrap();
    assert_eq!(s_scan, s_idx, "indexed and full-scan output diverged");
}

#[test]
fn match_at_line_one_clamps_before_context() {
    let tmp = tempfile::tempdir().unwrap();
    write_files(tmp.path(), &[("f.txt", "MATCH\nrest\n")]);
    let out = sink();

    search_full_scan_render(
        tmp.path(),
        "MATCH",
        true,
        false,
        None,
        &ContextOpts {
            before: 5,
            after: 0,
        },
        &render_opts(false, false, "MATCH"),
        Dispatch::Sorted,
        &out,
    )
    .unwrap();

    let s = String::from_utf8(out.into_inner().unwrap()).unwrap();
    // Only the match line — no synthetic before-context, no panic.
    assert_eq!(
        s.lines().count(),
        1,
        "expected single output line, got:\n{}",
        s
    );
    assert!(s.contains(":1:MATCH"));
}

#[test]
fn match_at_eof_without_trailing_newline_clamps_after_context() {
    let tmp = tempfile::tempdir().unwrap();
    write_files(tmp.path(), &[("f.txt", "a\nMATCH")]);
    let out = sink();

    search_full_scan_render(
        tmp.path(),
        "MATCH",
        true,
        false,
        None,
        &ContextOpts {
            before: 0,
            after: 5,
        },
        &render_opts(false, false, "MATCH"),
        Dispatch::Sorted,
        &out,
    )
    .unwrap();

    let s = String::from_utf8(out.into_inner().unwrap()).unwrap();
    assert!(s.contains(":2:MATCH"));
    // No phantom empty after-context line beyond EOF.
    assert!(!s.contains(":3:"));
}

// --- multi-file with `--` between files? ---
//
// ripgrep does NOT emit `--` between files (separator is only inside one
// file's chunks). We follow the same convention because each file's render
// runs on its own `Vec<u8>` and the dispatch joins them without a
// separator. This test pins that: matches in two files with context shouldn't
// produce inter-file `--`.

#[test]
fn no_separator_between_files() {
    let tmp = tempfile::tempdir().unwrap();
    write_files(tmp.path(), &[("a.txt", "M\n"), ("b.txt", "M\n")]);
    let out = sink();

    search_full_scan_render(
        tmp.path(),
        "M",
        true,
        false,
        None,
        &ContextOpts {
            before: 2,
            after: 2,
        },
        &render_opts(false, false, "M"),
        Dispatch::Sorted,
        &out,
    )
    .unwrap();

    let s = String::from_utf8(out.into_inner().unwrap()).unwrap();
    // Each file produces exactly one chunk; no `--` should appear between
    // them (the separator lives strictly *inside* one file's output).
    assert!(
        !s.contains("\n--\n"),
        "unexpected inter-file separator:\n{}",
        s
    );
}
