//! End-to-end tests for how a search picks its index.
//!
//! `fgr PATTERN` with no `--index` looks for a `.fgr` in the search path and
//! its parents and uses it when one is there; an index it finds that way is
//! used as found (never built), refreshed when the tree has moved past it, and
//! skipped entirely for `--no-index`. These go through the real binary, since
//! the whole point is the wiring between clap, the discovery walk, the working
//! directory, and the index.

use std::path::Path;
use std::process::{Command, Output};

fn fgr() -> Command {
    Command::new(env!("CARGO_BIN_EXE_fgr"))
}

fn write_files(tmp: &Path, files: &[(&str, &str)]) {
    for (name, content) in files {
        let full = tmp.join(name);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full, content).unwrap();
    }
}

fn run_in(args: &[&str], cwd: &Path) -> Output {
    fgr()
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn fgr")
}

fn stdout_lines(out: &Output) -> Vec<String> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|s| s.to_string())
        .collect()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The timing line fast-grep prints tells the two paths apart: an indexed
/// search reports load + search, a direct scan reports one elapsed number.
fn went_through_index(out: &Output) -> bool {
    let err = stderr(out);
    assert!(
        err.contains("Load:") || err.contains("Searched in"),
        "no timing line in stderr: {err}"
    );
    err.contains("Load:")
}

/// A small indexed project: two files with a match, one without.
fn project(tmp: &Path) {
    write_files(
        tmp,
        &[
            ("src/lib.rs", "fn alpha() {}\n"),
            ("src/deep/mod.rs", "fn alpha_deep() {}\n"),
            ("README.md", "nothing to see\n"),
        ],
    );
    let out = run_in(&["index", "."], tmp);
    assert!(out.status.success(), "index build failed: {}", stderr(&out));
}

fn matched_paths(out: &Output) -> Vec<String> {
    stdout_lines(out)
        .iter()
        .filter_map(|l| l.split(':').next().map(|p| p.replace('\\', "/")))
        .map(|p| p.trim_start_matches("./").to_string())
        .collect()
}

#[test]
fn a_discovered_index_answers_a_search_with_no_flag() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());

    let out = run_in(&["alpha", "."], tmp.path());
    assert!(went_through_index(&out), "search did not use the index");
    let mut paths = matched_paths(&out);
    paths.sort();
    assert_eq!(paths, vec!["src/deep/mod.rs", "src/lib.rs"]);
}

#[test]
fn discovery_walks_up_from_a_subdirectory_and_filters_to_it() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());

    // From `src/deep` the index is two levels up; only that subtree may answer.
    let out = run_in(&["alpha", "."], &tmp.path().join("src").join("deep"));
    assert!(went_through_index(&out), "search did not use the index");
    assert_eq!(matched_paths(&out), vec!["src/deep/mod.rs"]);
}

#[test]
fn an_explicit_index_also_filters_by_subdirectory() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());

    let out = run_in(&["alpha", "src/deep", "--index", ".fgr"], tmp.path());
    assert!(went_through_index(&out), "search did not use the index");
    assert_eq!(matched_paths(&out), vec!["src/deep/mod.rs"]);
}

#[test]
fn no_index_flag_scans_directly() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());

    let out = run_in(&["alpha", ".", "--no-index"], tmp.path());
    assert!(!went_through_index(&out), "--no-index still used the index");
    assert_eq!(matched_paths(&out).len(), 2);
}

#[test]
fn fgr_index_env_var_selects_the_index() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());

    let out = fgr()
        .args(["alpha", "."])
        .env("FGR_INDEX", ".fgr")
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(went_through_index(&out), "FGR_INDEX was ignored");
    assert_eq!(matched_paths(&out).len(), 2);
}

#[test]
fn a_plain_search_never_builds_an_index_behind_your_back() {
    let tmp = tempfile::tempdir().unwrap();
    write_files(tmp.path(), &[("src/lib.rs", "fn alpha() {}\n")]);

    let out = run_in(&["alpha", "."], tmp.path());
    assert!(!went_through_index(&out), "unindexed tree used an index");
    assert_eq!(matched_paths(&out), vec!["src/lib.rs"]);
    assert!(
        !tmp.path().join(".fgr").exists(),
        "a plain search created an index"
    );
}

/// Index staleness is mtime-based, so an edit is only detectable once it lands
/// on a stamp the filesystem tells apart from the build's. On the filesystems
/// that matter here that is nanoseconds, but a whole second keeps the test
/// honest on ones that only keep seconds.
fn wait_for_a_new_mtime() {
    std::thread::sleep(std::time::Duration::from_millis(1100));
}

#[test]
fn a_stale_index_is_refreshed_before_it_answers() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    wait_for_a_new_mtime();
    write_files(tmp.path(), &[("src/added.rs", "fn alpha_added() {}\n")]);

    let out = run_in(&["alpha", "."], tmp.path());
    assert!(went_through_index(&out), "search did not use the index");
    let mut paths = matched_paths(&out);
    paths.sort();
    assert_eq!(
        paths,
        vec!["src/added.rs", "src/deep/mod.rs", "src/lib.rs"],
        "a file added after the build never reached the results"
    );
}

#[test]
fn no_auto_update_answers_from_the_index_as_it_stands() {
    let tmp = tempfile::tempdir().unwrap();
    project(tmp.path());
    wait_for_a_new_mtime();
    write_files(tmp.path(), &[("src/added.rs", "fn alpha_added() {}\n")]);

    let out = run_in(&["alpha", ".", "--no-auto-update"], tmp.path());
    assert!(went_through_index(&out), "search did not use the index");
    let mut paths = matched_paths(&out);
    paths.sort();
    assert_eq!(paths, vec!["src/deep/mod.rs", "src/lib.rs"]);
}

#[test]
fn an_index_is_never_refreshed_against_the_wrong_tree() {
    // An index built as `fgr index .` records the root as `.`, which only means
    // what it meant in the directory it was built in. Used from elsewhere, a
    // refresh would walk *that* directory and replace the index's contents with
    // it — so both the search-time refresh and `fgr update` have to refuse.
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    project(&proj);
    // Something for a misdirected walk to pick up, outside the project.
    write_files(tmp.path(), &[("outside/stray.rs", "fn alpha_stray() {}\n")]);
    wait_for_a_new_mtime();
    write_files(&proj, &[("src/added.rs", "fn alpha_added() {}\n")]);

    let index = proj.join(".fgr");
    let index_arg = index.to_string_lossy().into_owned();

    let search = fgr()
        .args(["alpha", "proj"])
        .env("FGR_INDEX", &index_arg)
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert!(
        stderr(&search).contains("doesn't resolve from here"),
        "no warning about the unusable root: {}",
        stderr(&search)
    );

    let update = run_in(&["update", "--index", &index_arg], tmp.path());
    assert!(
        !update.status.success(),
        "update from the wrong directory should refuse, got: {}",
        stderr(&update)
    );

    // The index still describes the project, not the directory we ran from.
    let stats = run_in(&["stats", "--index", &index_arg], tmp.path());
    let text = String::from_utf8_lossy(&stats.stdout).into_owned();
    assert!(
        text.contains("Documents:    3"),
        "index was rewritten from the wrong tree: {text}"
    );
}
