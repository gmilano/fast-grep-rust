//! grep/ripgrep-compatible exit status: 0 = matched, 1 = no match, 2 = error.
//! Exercised through the real binary, with and without `--index`.

use std::path::Path;
use std::process::{Command, Output};

fn run(args: &[&str], cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fgr"))
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap()
}

fn code(args: &[&str], cwd: &Path) -> i32 {
    run(args, cwd).status.code().expect("exit code")
}

fn corpus() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a.txt"), "hit 1\nmiss\nhit 2\n").unwrap();
    d
}

fn with_index(d: &Path) {
    assert!(run(&["index", ".", "--output", ".fgr"], d).status.success());
}

#[test]
fn match_is_zero_and_no_match_is_one() {
    let d = corpus();
    for extra in [vec![], vec!["--index", ".fgr"]] {
        if !extra.is_empty() {
            with_index(d.path());
        }
        let mut hit = vec!["hit", "."];
        hit.extend(extra.iter());
        let mut nope = vec!["zzz_no_such_thing", "."];
        nope.extend(extra.iter());
        assert_eq!(code(&hit, d.path()), 0, "match → 0 ({extra:?})");
        assert_eq!(code(&nope, d.path()), 1, "no match → 1 ({extra:?})");
    }
}

#[test]
fn quiet_count_and_files_only_follow_the_same_rule() {
    let d = corpus();
    with_index(d.path());
    for flag in ["-q", "-c", "-l"] {
        for extra in [vec![], vec!["--index", ".fgr"]] {
            let mut hit = vec![flag, "hit", "."];
            hit.extend(extra.iter());
            let mut nope = vec![flag, "zzz_no_such_thing", "."];
            nope.extend(extra.iter());
            assert_eq!(code(&hit, d.path()), 0, "{flag} match ({extra:?})");
            assert_eq!(code(&nope, d.path()), 1, "{flag} no match ({extra:?})");
        }
    }
}

#[test]
fn invert_match_reports_selected_lines() {
    let d = corpus();
    // `-v hit` selects the `miss` line → something was selected.
    assert_eq!(code(&["-v", "hit", "."], d.path()), 0);
    // `-v .` selects nothing (every line matches `.`) → no match.
    assert_eq!(code(&["-v", ".", "."], d.path()), 1);
}

#[test]
fn output_caps_do_not_hide_that_something_matched() {
    let d = corpus();
    assert_eq!(code(&["--max-results", "1", "hit", "."], d.path()), 0);
    assert_eq!(code(&["--max-files", "0", "hit", "."], d.path()), 0);
}

#[test]
fn errors_exit_two() {
    let d = corpus();
    // Invalid regex.
    assert_eq!(code(&["(", "."], d.path()), 2);
    // Invalid flag combination.
    assert_eq!(code(&["-c", "--format", "json", "hit", "."], d.path()), 2);
    // Missing pattern.
    assert_eq!(code(&[], d.path()), 2);
}
