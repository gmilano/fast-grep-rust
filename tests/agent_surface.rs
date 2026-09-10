//! The agent-oriented CLI surface: `--agent`, `--agent-aggressive`,
//! `--agent-stats`, the `--max-*` output caps, `--format json|jsonl`, and
//! `fgr integrations`. Runs the real binary against a tiny corpus, both
//! without and with `--index`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn fgr() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_fgr"));
    // Keep the environment from steering the format.
    c.env_remove("FGR_FORMAT");
    c
}

fn run(args: &[&str], cwd: &Path) -> Output {
    fgr().args(args).current_dir(cwd).output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn lines(o: &Output) -> Vec<String> {
    stdout(o).lines().map(str::to_string).collect()
}

/// Three files × three `hit` lines = 9 matches, so path/line ordering and
/// every cap have something to bite on.
fn corpus() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    for f in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(d.path().join(f), "hit 1\nhit 2\nhit 3\n").unwrap();
    }
    d
}

fn index(d: &Path) -> PathBuf {
    let idx = d.join(".fgr");
    let o = run(&["index", ".", "--output", ".fgr"], d);
    assert!(o.status.success(), "index build failed: {}", stderr(&o));
    idx
}

// ---------------------------------------------------------------- --agent

#[test]
fn agent_is_format_compact() {
    let d = corpus();
    let a = run(&["--agent", "hit", "."], d.path());
    let c = run(&["--format", "compact", "hit", "."], d.path());
    assert!(a.status.success());
    assert_eq!(
        a.stdout, c.stdout,
        "--agent must be byte-identical to --format compact"
    );
    // Compact shape: relative path heading, then `line:text`.
    let out = stdout(&a);
    assert!(out.starts_with("a.txt\n1:hit 1\n"), "got: {out}");
}

#[test]
fn agent_aggressive_cuts_lines_at_200_chars() {
    let d = tempfile::tempdir().unwrap();
    // 199 ASCII chars, then a 3-byte char straddling the cut, then filler.
    let long = format!(
        "{}€{}",
        "hit".to_string() + &"x".repeat(196),
        "y".repeat(100)
    );
    assert_eq!(long.chars().count(), 300);
    std::fs::write(d.path().join("f.txt"), format!("{long}\nhit short\n")).unwrap();

    let agg = run(&["--agent-aggressive", "hit", "."], d.path());
    assert!(agg.status.success());
    let out = stdout(&agg);
    let cut_line = out.lines().nth(1).unwrap(); // "1:<content>"
    let content = cut_line.strip_prefix("1:").unwrap();
    assert!(content.ends_with('…'), "cut must end with …: {content}");
    let kept: String = content.chars().take(content.chars().count() - 1).collect();
    assert_eq!(kept.chars().count(), 200, "exactly 200 characters kept");
    assert!(
        kept.ends_with('€'),
        "the multi-byte char at the boundary is intact"
    );
    assert_eq!(
        out.lines().nth(2).unwrap(),
        "2:hit short",
        "short lines untouched"
    );

    // Identical to --agent apart from the cut line.
    let plain = stdout(&run(&["--agent", "hit", "."], d.path()));
    assert_eq!(plain.lines().nth(2), Some("2:hit short"));
    assert_eq!(plain.lines().nth(1).unwrap(), format!("1:{long}"));
}

#[test]
fn format_precedence_agent_vs_env_vs_format() {
    let d = corpus();
    // env says grep, --agent wins over env → compact.
    let o = fgr()
        .args(["--agent", "hit", "."])
        .env("FGR_FORMAT", "grep")
        .current_dir(d.path())
        .output()
        .unwrap();
    assert!(
        stdout(&o).starts_with("a.txt\n1:hit 1\n"),
        "--agent beats FGR_FORMAT"
    );
    // explicit --format beats --agent → grep (flat `path:line:text`, no
    // heading). Plain grep streams in completion order, so check the shape
    // on the path-sorted lines rather than assuming which file came first.
    let o = run(&["--agent", "--format", "grep", "hit", "."], d.path());
    let mut sorted = lines(&o);
    sorted.sort();
    assert_eq!(sorted.len(), 9);
    assert!(
        sorted[0].ends_with("a.txt:1:hit 1"),
        "--format beats --agent: {}",
        sorted[0]
    );
    assert!(
        !stdout(&o).starts_with("a.txt\n"),
        "must not be the compact heading layout"
    );
    // env alone is honoured.
    let o = fgr()
        .args(["hit", "."])
        .env("FGR_FORMAT", "compact")
        .current_dir(d.path())
        .output()
        .unwrap();
    assert!(
        stdout(&o).starts_with("a.txt\n1:hit 1\n"),
        "FGR_FORMAT=compact honoured"
    );
}

// ---------------------------------------------------------------- caps

fn assert_first_n_by_path(o: &Output, expected_tail: &[&str]) {
    let got = lines(o);
    assert_eq!(got.len(), expected_tail.len(), "lines: {got:?}");
    for (g, e) in got.iter().zip(expected_tail) {
        assert!(g.ends_with(e), "line {g:?} should end with {e:?}");
    }
}

#[test]
fn max_results_keeps_first_n_by_path_then_line() {
    let d = corpus();
    for extra in [vec![], vec!["--index", ".fgr"]] {
        if !extra.is_empty() {
            index(d.path());
        }
        let mut args = vec!["--format", "grep", "--max-results", "4", "hit", "."];
        args.extend(extra.iter());
        let o = run(&args, d.path());
        assert!(o.status.success());
        assert_first_n_by_path(
            &o,
            &[
                "a.txt:1:hit 1",
                "a.txt:2:hit 2",
                "a.txt:3:hit 3",
                "b.txt:1:hit 1",
            ],
        );
        let err = stderr(&o);
        assert!(err.contains("output truncated"), "stderr: {err}");
        assert!(
            err.contains("4 of 9 matches (2 of 3 files)"),
            "stderr: {err}"
        );
    }
}

#[test]
fn max_files_caps_file_count() {
    let d = corpus();
    let o = run(
        &["--format", "grep", "--max-files", "2", "hit", "."],
        d.path(),
    );
    assert_first_n_by_path(
        &o,
        &[
            "a.txt:1:hit 1",
            "a.txt:2:hit 2",
            "a.txt:3:hit 3",
            "b.txt:1:hit 1",
            "b.txt:2:hit 2",
            "b.txt:3:hit 3",
        ],
    );
    assert!(stderr(&o).contains("6 of 9 matches (2 of 3 files)"));
}

#[test]
fn max_results_per_file_caps_each_file() {
    let d = corpus();
    let o = run(
        &[
            "--format",
            "grep",
            "--max-results-per-file",
            "1",
            "hit",
            ".",
        ],
        d.path(),
    );
    assert_first_n_by_path(&o, &["a.txt:1:hit 1", "b.txt:1:hit 1", "c.txt:1:hit 1"]);
    // Each file stopped at its first hit, so the total is a lower bound.
    assert!(
        stderr(&o).contains("3 of 3+ matches (3 of 3 files)"),
        "stderr: {}",
        stderr(&o)
    );
}

/// A per-file cap stops scanning that file, so totals become "at least N":
/// `N+` on stderr, `exact: false` in JSON. File counts stay exact.
#[test]
fn per_file_cap_reports_lower_bound_totals() {
    let d = corpus();
    // --max-results 2 also caps each file at 2 → every file stops early:
    // shown 2 (a.txt), at least 3×2 = 6 in total, 1 of 3 files.
    let o = run(
        &["--format", "grep", "--max-results", "2", "hit", "."],
        d.path(),
    );
    assert_first_n_by_path(&o, &["a.txt:1:hit 1", "a.txt:2:hit 2"]);
    assert!(
        stderr(&o).contains("2 of 6+ matches (1 of 3 files)"),
        "stderr: {}",
        stderr(&o)
    );

    let o = run(
        &[
            "--format",
            "json",
            "--max-results-per-file",
            "1",
            "hit",
            ".",
        ],
        d.path(),
    );
    let v = parse_json(&o);
    assert_eq!(
        v["truncated"],
        serde_json::json!({"total_matches": 3, "shown_matches": 3, "total_files": 3, "shown_files": 3, "exact": false})
    );
    assert_eq!(
        v["total_matches"], 3,
        "envelope total is the same lower bound"
    );
    assert_eq!(v["files"].as_array().unwrap().len(), 3);

    // No file stopped early → totals exact.
    let o = run(
        &["--format", "json", "--max-files", "1", "hit", "."],
        d.path(),
    );
    let v = parse_json(&o);
    assert_eq!(v["truncated"]["exact"], true);
    assert_eq!(v["truncated"]["total_matches"], 9);
}

#[test]
fn max_output_bytes_never_splits_a_line() {
    let d = corpus();
    let full = run(&["--format", "grep", "hit", "."], d.path());
    // The uncapped grep run streams in completion order; the capped run is
    // sorted by path, so compare against the path-sorted baseline.
    let mut all = lines(&full);
    all.sort();
    // Budget for exactly two full lines (+ their newlines) plus one stray byte.
    let two = all[0].len() + 1 + all[1].len() + 1;
    let cap = (two + 1).to_string();
    let o = run(
        &["--format", "grep", "--max-output-bytes", &cap, "hit", "."],
        d.path(),
    );
    assert!(o.stdout.len() <= two + 1);
    assert!(o.stdout.ends_with(b"\n"), "output ends at a line boundary");
    assert_eq!(lines(&o), all[..2].to_vec());
    assert!(stderr(&o).contains("2 of 9 matches"));
}

#[test]
fn caps_keep_context_of_kept_matches_only() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("f.txt"), "a\nhit\nb\nc\nd\nhit\ne\n").unwrap();
    let o = run(
        &[
            "--format",
            "grep",
            "-C",
            "1",
            "--max-results",
            "1",
            "hit",
            ".",
        ],
        d.path(),
    );
    let got = lines(&o);
    assert_eq!(
        got.len(),
        3,
        "context of the kept match only, no stranded separator: {got:?}"
    );
    assert!(got[0].ends_with("f.txt-1-a"));
    assert!(got[1].ends_with("f.txt:2:hit"));
    assert!(got[2].ends_with("f.txt-3-b"));
    assert!(!stdout(&o).contains("--\n"));
}

// ---------------------------------------------------------------- json

fn parse_json(o: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(o)).unwrap_or_else(|e| panic!("invalid JSON ({e}): {}", stdout(o)))
}

#[test]
fn json_document_matches_documented_schema() {
    let d = corpus();
    let o = run(&["--format", "json", "hit", "."], d.path());
    assert!(o.status.success());
    let v = parse_json(&o);
    let obj = v.as_object().unwrap();
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "elapsed_ms",
            "files",
            "indexed",
            "query",
            "root",
            "total_matches",
            "truncated"
        ]
    );
    assert_eq!(v["query"], "hit");
    assert!(
        Path::new(v["root"].as_str().unwrap()).is_absolute(),
        "root is absolute: {}",
        v["root"]
    );
    assert_eq!(v["indexed"], false);
    assert_eq!(v["total_matches"], 9);
    assert!(v["truncated"].is_null());
    let files = v["files"].as_array().unwrap();
    let paths: Vec<&str> = files.iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert_eq!(paths, ["a.txt", "b.txt", "c.txt"], "relative paths, sorted");
    let m = &files[0]["matches"];
    assert_eq!(m[0], serde_json::json!({"line": 1, "text": "hit 1"}));
    assert_eq!(m.as_array().unwrap().len(), 3);

    // Indexed search reports indexed=true and the same content.
    index(d.path());
    let o = run(
        &["--format", "json", "--index", ".fgr", "hit", "."],
        d.path(),
    );
    let v = parse_json(&o);
    assert_eq!(v["indexed"], true);
    assert_eq!(v["total_matches"], 9);
}

#[test]
fn json_is_valid_with_zero_matches() {
    let d = corpus();
    let o = run(&["--format", "json", "zzz_no_such_thing", "."], d.path());
    let v = parse_json(&o);
    assert_eq!(v["total_matches"], 0);
    assert_eq!(v["files"], serde_json::json!([]));
    assert!(v["truncated"].is_null());
}

#[test]
fn json_truncated_field_reports_totals_vs_shown() {
    let d = corpus();
    let o = run(
        &["--format", "json", "--max-results", "4", "hit", "."],
        d.path(),
    );
    let v = parse_json(&o);
    assert_eq!(
        v["truncated"],
        serde_json::json!({"total_matches": 9, "shown_matches": 4, "total_files": 3, "shown_files": 2, "exact": true})
    );
    assert_eq!(v["files"].as_array().unwrap().len(), 2);
    assert_eq!(v["files"][1]["matches"].as_array().unwrap().len(), 1);
    assert!(
        !stderr(&o).contains("truncated"),
        "JSON carries truncation itself, not stderr"
    );
}

#[test]
fn jsonl_one_object_per_match_and_trailing_truncated() {
    let d = corpus();
    let o = run(&["--format", "jsonl", "hit", "."], d.path());
    let got = lines(&o);
    assert_eq!(got.len(), 9);
    for l in &got {
        let v: serde_json::Value = serde_json::from_str(l).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["line", "path", "text"]);
    }
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&got[0]).unwrap(),
        serde_json::json!({"path":"a.txt","line":1,"text":"hit 1"})
    );

    let o = run(
        &["--format", "jsonl", "--max-results", "4", "hit", "."],
        d.path(),
    );
    let got = lines(&o);
    assert_eq!(got.len(), 5, "4 matches + the truncated line: {got:?}");
    let last: serde_json::Value = serde_json::from_str(&got[4]).unwrap();
    assert_eq!(last["truncated"]["shown_matches"], 4);
    assert_eq!(last["truncated"]["total_matches"], 9);
}

#[test]
fn json_text_round_trips_escapes() {
    let d = tempfile::tempdir().unwrap();
    let line = "hit \"quoted\" back\\slash\ttab";
    std::fs::write(d.path().join("f.txt"), format!("{line}\n")).unwrap();
    let o = run(&["--format", "jsonl", "hit", "."], d.path());
    let v: serde_json::Value = serde_json::from_str(lines(&o)[0].as_str()).unwrap();
    assert_eq!(v["text"], line);
}

#[test]
fn count_and_files_only_reject_json_formats() {
    let d = corpus();
    for args in [["-c", "--format", "json"], ["-l", "--format", "jsonl"]] {
        let mut a = args.to_vec();
        a.extend(["hit", "."]);
        let o = run(&a, d.path());
        assert_eq!(o.status.code(), Some(2), "exit code for {args:?}");
        assert!(
            stderr(&o).contains("cannot be combined"),
            "stderr: {}",
            stderr(&o)
        );
        assert!(o.stdout.is_empty());
    }
}

// ---------------------------------------------------------------- stats & guides

#[test]
fn agent_stats_reports_output_bytes_and_token_estimate() {
    let d = corpus();
    let o = run(&["--agent", "--agent-stats", "hit", "."], d.path());
    let err = stderr(&o);
    let line = err
        .lines()
        .find(|l| l.starts_with("fgr-stats:"))
        .unwrap_or_else(|| panic!("no fgr-stats line in: {err}"));
    let bytes = o.stdout.len() as u64;
    assert!(line.contains(&format!("output_bytes={bytes}")), "{line}");
    assert!(
        line.contains(&format!("est_tokens={}", bytes.div_ceil(4))),
        "{line}"
    );
    assert!(line.contains("matches=9"), "{line}");
    assert!(line.contains("files=3"), "{line}");
    assert!(line.contains("search_ms="), "{line}");
}

#[test]
fn integrations_prints_all_guides() {
    let d = corpus();
    let o = run(&["integrations"], d.path());
    assert!(o.status.success());
    let out = stdout(&o);
    for name in ["Claude Code", "Codex", "OpenCode", "Aider", "MCP"] {
        assert!(
            out.contains(&format!("==== {name} ====")),
            "missing guide {name}"
        );
    }
}
