use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::{index, persist, searcher};

// ANSI escape codes for the TTY rendering path. We write them by hand to keep
// the dep footprint zero (no `colored`, no `termcolor`) — the codes that
// survive on every modern terminal we care about (macOS Terminal/iTerm2,
// Linux xterm/gnome-terminal/alacritty, Windows Terminal).
const C_RESET: &str = "\x1b[0m";
const C_BOLD: &str = "\x1b[1m";
const C_DIM: &str = "\x1b[2m";
const C_PATH: &str = "\x1b[35m"; // magenta — file paths
const C_LINENO: &str = "\x1b[32m"; // green — line numbers
const C_MATCH: &str = "\x1b[1;31m"; // bold red — matched substring

/// Search options extracted from CLI flags
struct SearchOpts {
    count: bool,
    files_only: bool,
    quiet: bool,
    no_ignore: bool,
    hidden: bool,
    file_type: Option<String>,
    /// Effective regex pattern (may have (?i) prefix etc.) — used by the TTY
    /// renderer to highlight matched substrings.
    pattern: Option<String>,
}

#[derive(Parser)]
#[command(
    name = "fgr",
    version,
    about = "Fast grep with sparse n-gram index — drop-in grep replacement",
    args_conflicts_with_subcommands = true,
)]
pub struct Cli {
    /// Regex pattern to search (grep-compatible)
    #[arg(value_name = "PATTERN")]
    pub pattern: Option<String>,

    /// Directory or file to search (default: current dir)
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Commands>,

    // -- grep-compatible flags --

    /// Recurse into directories (on by default)
    #[arg(short = 'r', long = "recursive", global = true)]
    pub recursive: bool,

    /// Only print count of matching lines per file
    #[arg(short = 'c', long = "count", global = true)]
    pub count: bool,

    /// Only print names of files with matches
    #[arg(short = 'l', long = "files-with-matches", global = true)]
    pub files_only: bool,

    /// Print line numbers with output (on by default)
    #[arg(short = 'n', long = "line-number", global = true)]
    pub line_number: bool,

    /// Ignore case distinctions
    #[arg(short = 'i', long = "ignore-case", global = true)]
    pub ignore_case: bool,

    /// Select only lines that do NOT match
    #[arg(short = 'v', long = "invert-match", global = true)]
    pub invert_match: bool,

    /// Print only the matched parts
    #[arg(short = 'o', long = "only-matching", global = true)]
    pub only_matching: bool,

    /// Suppress normal output; exit with 0 if match found
    #[arg(short = 'q', long = "quiet", global = true)]
    pub quiet: bool,

    /// Print NUM lines of context after match
    #[arg(short = 'A', long = "after-context", value_name = "NUM", global = true)]
    pub after_context: Option<usize>,

    /// Print NUM lines of context before match
    #[arg(short = 'B', long = "before-context", value_name = "NUM", global = true)]
    pub before_context: Option<usize>,

    /// Print NUM lines of context around match
    #[arg(short = 'C', long = "context", value_name = "NUM", global = true)]
    pub context: Option<usize>,

    /// Use PATTERN as a fixed string, not a regex
    #[arg(short = 'F', long = "fixed-strings", global = true)]
    pub fixed_strings: bool,

    /// Use PATTERN as an extended regex (default)
    #[arg(short = 'E', long = "extended-regexp", global = true)]
    pub extended_regexp: bool,

    /// Include only files matching GLOB
    #[arg(long = "include", value_name = "GLOB", global = true)]
    pub include: Option<String>,

    /// Exclude files matching GLOB
    #[arg(long = "exclude", value_name = "GLOB", global = true)]
    pub exclude: Option<String>,

    // -- fast-grep specific flags --

    /// Use persistent index for searching (path to .fgr dir)
    #[arg(long = "index", value_name = "PATH", global = true)]
    pub index_path: Option<PathBuf>,

    /// Don't respect .gitignore
    #[arg(long, global = true)]
    pub no_ignore: bool,

    /// Include hidden files and directories (dotfiles like .git, .github)
    #[arg(short = '.', long = "hidden", global = true)]
    pub hidden: bool,

    /// Filter by file extension (e.g., --type rs)
    #[arg(long = "type", value_name = "EXT", global = true)]
    pub file_type: Option<String>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Build a persistent index for DIR
    #[command(name = "index")]
    Index {
        /// Directory to index
        dir: PathBuf,
        /// Index output directory (created inside DIR)
        #[arg(long, default_value = ".fgr")]
        output: PathBuf,
        /// Start daemon after building index
        #[arg(short = 'D', long)]
        daemon: bool,
    },
    /// Benchmark PATTERN search in DIR
    #[command(name = "bench")]
    Bench {
        pattern: String,
        dir: PathBuf,
    },
    /// Incrementally update an existing index
    #[command(name = "update")]
    Update {
        dir: Option<PathBuf>,
        #[arg(long, default_value = ".fgr")]
        index: PathBuf,
    },
    /// Show index statistics
    #[command(name = "stats")]
    Stats {
        #[arg(long, default_value = ".fgr")]
        index: PathBuf,
    },
    /// Watch DIR for changes and keep index up-to-date
    #[command(name = "daemon")]
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
}

#[cfg(feature = "daemon")]
#[derive(Subcommand)]
pub enum DaemonAction {
    /// Start the daemon (runs in foreground)
    Start {
        /// Directory to watch (default: current dir)
        dir: Option<PathBuf>,
        #[arg(long, default_value = ".fgr")]
        output: PathBuf,
    },
    /// Stop a running daemon
    Stop {
        dir: Option<PathBuf>,
        #[arg(long, default_value = ".fgr")]
        output: PathBuf,
    },
    /// Check daemon status
    Status {
        dir: Option<PathBuf>,
        #[arg(long, default_value = ".fgr")]
        output: PathBuf,
    },
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();

    let opts = SearchOpts {
        count: cli.count,
        files_only: cli.files_only,
        quiet: cli.quiet,
        no_ignore: cli.no_ignore,
        hidden: cli.hidden,
        file_type: cli.file_type.clone(),
        pattern: None, // populated below once the effective pattern is built
    };

    if let Some(cmd) = cli.command {
        return run_subcommand(cmd, opts.no_ignore, opts.hidden, opts.file_type.as_deref());
    }

    let pattern = match cli.pattern.as_ref() {
        Some(p) => p.clone(),
        None => {
            eprintln!("Usage: fgr [OPTIONS] PATTERN [PATH]");
            eprintln!("Try 'fgr --help' for more information.");
            std::process::exit(2);
        }
    };

    let dir = cli.path.clone().unwrap_or_else(|| PathBuf::from("."));

    let mut effective = if cli.fixed_strings {
        regex::escape(&pattern)
    } else {
        pattern
    };
    if cli.ignore_case {
        effective = format!("(?i){}", effective);
    }

    let mut opts = opts;
    opts.pattern = Some(effective.clone());

    // Case-insensitive search can't use the index reliably — the index stores
    // trigrams from the original (case-sensitive) text, so (?i) patterns miss
    // uppercase variants. Fall back to full scan when -i is active.
    if let Some(ref idx_path) = cli.index_path {
        if cli.ignore_case {
            run_direct_search(&effective, &dir, &opts)?;
        } else {
            run_indexed_search(&effective, idx_path, dir.as_path(), &opts)?;
        }
    } else {
        run_direct_search(&effective, &dir, &opts)?;
    }

    Ok(())
}

fn run_direct_search(pattern: &str, dir: &std::path::Path, opts: &SearchOpts) -> Result<()> {
    // For count/files-only/quiet, use the collecting API
    if opts.count || opts.files_only || opts.quiet {
        let start = Instant::now();
        let matches = searcher::search_full_scan(dir, pattern, opts.no_ignore, opts.hidden, opts.file_type.as_deref())?;
        let elapsed = start.elapsed();
        output_matches(&matches, opts)?;
        if !opts.quiet {
            eprintln!("Searched in {:.2}ms, {} matches", elapsed.as_secs_f64() * 1000.0, matches.len());
        }
        if opts.quiet && matches.is_empty() {
            std::process::exit(1);
        }
        return Ok(());
    }

    // TTY path: collect matches, then render grouped+colored. The collecting
    // overhead is negligible compared to the regex+IO, and grouped rendering
    // requires sorted-by-file matches anyway.
    if std::io::stdout().is_terminal() {
        let start = Instant::now();
        let mut matches = searcher::search_full_scan(dir, pattern, opts.no_ignore, opts.hidden, opts.file_type.as_deref())?;
        // Stable sort by path so streaming-order races don't interleave files
        // in the rendered output. Within a file, search_full_scan already
        // returns matches in line order.
        matches.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.line_number.cmp(&b.line_number)));
        let elapsed = start.elapsed();
        let n = matches.len();
        output_matches(&matches, opts)?;
        eprintln!("Searched in {:.2}ms, {} matches", elapsed.as_secs_f64() * 1000.0, n);
        return Ok(());
    }

    // Piped path: streaming `path:line:content` for grep-compatible pipelines.
    let start = Instant::now();
    let output = std::sync::Mutex::new(std::io::BufWriter::new(std::io::stdout()));
    let count = searcher::search_full_scan_streaming(
        dir, pattern, opts.no_ignore, opts.hidden, opts.file_type.as_deref(), &output,
    )?;
    {
        let mut out = output.lock().unwrap();
        let _ = out.flush();
    }
    let elapsed = start.elapsed();
    eprintln!("Searched in {:.2}ms, {} matches", elapsed.as_secs_f64() * 1000.0, count);
    Ok(())
}

fn run_indexed_search(pattern: &str, idx_path: &std::path::Path, search_path: &std::path::Path, opts: &SearchOpts) -> Result<()> {
    // Auto-build the index on first use. We detect "no index" by the absence of
    // meta.json (the same probe persist::load uses internally). The build root
    // is the search PATH the user passed — this matches the natural intent
    // "give me a fast search over this directory."
    if !idx_path.join("meta.json").exists() {
        eprintln!(
            "Index not found at {} — building one-time (subsequent searches will be <200ms)…",
            idx_path.display()
        );
        let build_start = Instant::now();
        persist::build(search_path, idx_path, opts.no_ignore, opts.file_type.as_deref(), true)?;
        eprintln!("Index built in {:.2}s", build_start.elapsed().as_secs_f64());
    }

    // If a daemon is managing this index, ensure it's up-to-date before searching
    #[cfg(feature = "daemon")]
    if crate::daemon::is_daemon_running(idx_path) {
        if let Ok(status) = crate::daemon::send_command(idx_path, "status") {
            if status == "dirty" {
                let _ = crate::daemon::send_command(idx_path, "flush");
            }
        }
    }

    let start = Instant::now();
    let idx = persist::load(idx_path)?;

    // Resolve path filter: only return results under search_path.
    // Compare using the same path form as stored in the index (relative from cwd).
    let root_dir = PathBuf::from(&idx.meta.root_dir);
    let path_filter = if search_path != root_dir && search_path != std::path::Path::new(".") {
        Some(search_path.to_path_buf())
    } else {
        None
    };

    let load_time = start.elapsed();
    let start = Instant::now();
    if opts.count {
        let (n, _) = searcher::search_persistent_count(&idx, pattern, path_filter.as_deref(), opts.hidden)?;
        let search_time = start.elapsed();
        println!("{}", n);
        if !opts.quiet {
            eprintln!("Load: {:.1}ms, Search: {:.1}ms", load_time.as_secs_f64() * 1000.0, search_time.as_secs_f64() * 1000.0);
        }
    } else {
        let (mut matches, _) = searcher::search_persistent_timed(&idx, pattern, path_filter.as_deref(), opts.hidden)?;
        matches.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.line_number.cmp(&b.line_number)));
        let search_time = start.elapsed();
        output_matches(&matches, opts)?;
        if !opts.quiet {
            eprintln!("Load: {:.1}ms, Search: {:.1}ms, {} matches", load_time.as_secs_f64() * 1000.0, search_time.as_secs_f64() * 1000.0, matches.len());
        }
    }
    Ok(())
}

fn output_matches(matches: &[searcher::Match], opts: &SearchOpts) -> Result<()> {
    if opts.quiet { return Ok(()); }
    if opts.count {
        let mut counts: std::collections::HashMap<&PathBuf, usize> = std::collections::HashMap::new();
        for m in matches { *counts.entry(&m.path).or_insert(0) += 1; }
        let mut pairs: Vec<_> = counts.into_iter().collect();
        pairs.sort_by(|a, b| a.0.cmp(b.0));
        let tty = std::io::stdout().is_terminal();
        for (path, count) in pairs {
            if tty {
                println!("{}{}{}{}:{}{}{}", C_BOLD, C_PATH, path.display(), C_RESET, C_LINENO, count, C_RESET);
            } else {
                println!("{}:{}", path.display(), count);
            }
        }
        return Ok(());
    }
    if opts.files_only {
        let mut files: Vec<_> = matches.iter().map(|m| &m.path).collect();
        files.sort(); files.dedup();
        let tty = std::io::stdout().is_terminal();
        for f in files {
            if tty {
                println!("{}{}{}{}", C_BOLD, C_PATH, f.display(), C_RESET);
            } else {
                println!("{}", f.display());
            }
        }
        return Ok(());
    }

    let stdout = std::io::stdout();
    let tty = stdout.is_terminal();
    let mut out = std::io::BufWriter::new(stdout.lock());
    if tty {
        write_grouped_matches(&mut out, matches, opts.pattern.as_deref())?;
    } else {
        for m in matches {
            let _ = writeln!(out, "{}:{}:{}", m.path.display(), m.line_number, m.line);
        }
    }
    Ok(())
}

/// ripgrep-style grouped output: file path on its own line, then each match
/// indented as `<lineno>: <content>` with the matched substring highlighted.
/// Improves on rg by right-aligning line numbers within each file group, so a
/// file that contains both line 3 and line 1042 renders cleanly aligned.
fn write_grouped_matches<W: Write>(out: &mut W, matches: &[searcher::Match], pattern: Option<&str>) -> Result<()> {
    // Compile the regex once for highlight-position lookups. If pattern is
    // None or doesn't compile, we still print the line — just unhighlighted.
    let re = pattern.and_then(|p| regex::Regex::new(p).ok());

    // Reused across all matches: avoids per-line String allocation when the
    // regex can be applied. For the no-regex fallback we just write `m.line`
    // by reference, no clone needed.
    let mut hl_buf = String::with_capacity(256);

    let mut current: Option<&std::path::Path> = None;
    let mut i = 0;
    while i < matches.len() {
        // Find the end of the current file's run.
        let path = matches[i].path.as_path();
        let mut j = i + 1;
        while j < matches.len() && matches[j].path.as_path() == path { j += 1; }

        // Width of the largest line number in this group (for right-align).
        let max_lineno = matches[i..j].iter().map(|m| m.line_number).max().unwrap_or(1);
        let lineno_width = max_lineno.to_string().len();

        // Blank line between groups.
        if current.is_some() {
            let _ = writeln!(out);
        }
        let _ = writeln!(out, "{}{}{}{}", C_BOLD, C_PATH, path.display(), C_RESET);

        for m in &matches[i..j] {
            let rendered: &str = if let Some(ref r) = re {
                hl_buf.clear();
                highlight_into(&m.line, r, &mut hl_buf);
                hl_buf.as_str()
            } else {
                m.line.as_str()
            };
            let _ = writeln!(
                out,
                "{}{:>w$}{}{}:{} {}",
                C_LINENO, m.line_number, C_RESET,
                C_DIM, C_RESET,
                rendered,
                w = lineno_width,
            );
        }

        current = Some(path);
        i = j;
    }
    Ok(())
}

/// Writes `line` into `out` with every regex match wrapped in ANSI codes.
/// Caller is responsible for clearing `out` before calling if a fresh result
/// is needed; we append, so the same buffer can be reused across many lines.
fn highlight_into(line: &str, re: &regex::Regex, out: &mut String) {
    out.reserve(line.len() + 16);
    let mut last_end = 0usize;
    for mat in re.find_iter(line) {
        if mat.start() > last_end {
            out.push_str(&line[last_end..mat.start()]);
        }
        out.push_str(C_MATCH);
        out.push_str(&line[mat.start()..mat.end()]);
        out.push_str(C_RESET);
        last_end = mat.end();
    }
    if last_end < line.len() {
        out.push_str(&line[last_end..]);
    }
}

fn run_subcommand(cmd: Commands, no_ignore: bool, hidden: bool, type_filter: Option<&str>) -> Result<()> {
    match cmd {
        Commands::Index { dir, output, daemon } => {
            let idx_path = dir.join(&output);
            let start = Instant::now();
            persist::build(&dir, &idx_path, no_ignore, type_filter, true)?;
            eprintln!("Index built in {:.2}s", start.elapsed().as_secs_f64());
            #[cfg(feature = "daemon")]
            if daemon {
                crate::daemon::start_daemon(&idx_path)?;
            }
            #[cfg(not(feature = "daemon"))]
            if daemon {
                eprintln!("Daemon feature not enabled. Rebuild with --features daemon");
            }
        }
        Commands::Bench { pattern, dir } => {
            run_bench(&pattern, &dir, no_ignore, hidden, type_filter)?;
        }
        Commands::Update { dir, index: idx_path } => {
            let root = if let Some(d) = dir { d } else {
                let probe = persist::load(&idx_path)?;
                PathBuf::from(&probe.meta.root_dir)
            };
            let (_lock, waited) = persist::acquire_index_lock(&idx_path)?;
            // If we waited for another process, reload and re-check — it may
            // have already done the work we were going to do.
            if waited {
                let idx = persist::load(&idx_path)?;
                if !idx.is_stale() {
                    persist::release_index_lock(&idx_path);
                    eprintln!("Index already up to date (updated by another process)");
                    return Ok(());
                }
            }
            let stats = persist::update_incremental(&idx_path, &root, true)?;
            persist::release_index_lock(&idx_path);
            if stats.added == 0 && stats.modified == 0 && stats.deleted == 0 {
                eprintln!("Index is up to date ({} files)", stats.unchanged);
            } else {
                eprintln!("Updated index: +{} added, {} modified, {} deleted (unchanged: {}) in {}ms",
                    stats.added, stats.modified, stats.deleted, stats.unchanged, stats.duration_ms);
            }
        }
        Commands::Stats { index: index_path } => {
            if index_path.exists() {
                let idx = persist::load(&index_path)?;
                println!("Persistent Index Stats:");
                println!("  Documents:    {}", idx.meta.num_docs);
                println!("  N-grams:      {}", idx.meta.num_ngrams);
                println!("  Root dir:     {}", idx.meta.root_dir);
                println!("  Built at:     {}", idx.meta.built_at);
                println!("  Stale:        {}", idx.is_stale());
                println!("  Postings size: {}KB", idx.postings_mmap.len() / 1024);
                if let Some(ref bm) = idx.bitmap_mmap {
                    println!("  Bitmaps size:  {}KB", bm.len() / 1024);
                }
            } else {
                let idx = index::SparseIndex::build_from_directory(&index_path, no_ignore, type_filter, false)?;
                let stats = idx.stats();
                println!("In-memory Index Stats:");
                println!("  Documents:    {}", stats.num_docs);
                println!("  N-grams:      {}", stats.num_ngrams);
                println!("  Estimated RAM: {}MB", stats.estimated_ram_bytes / (1024 * 1024));
                println!("  Avg postings len: {:.1}", stats.avg_postings_len);
            }
        }
        #[cfg(feature = "daemon")]
        Commands::Daemon { action } => {
            match action {
                DaemonAction::Start { dir, output } => {
                    let idx_path = dir.unwrap_or_else(|| PathBuf::from(".")).join(&output);
                    crate::daemon::start_daemon(&idx_path)?;
                }
                DaemonAction::Stop { dir, output } => {
                    let idx_path = dir.unwrap_or_else(|| PathBuf::from(".")).join(&output);
                    let resp = crate::daemon::send_command(&idx_path, "stop")?;
                    eprintln!("Daemon: {}", resp);
                }
                DaemonAction::Status { dir, output } => {
                    let idx_path = dir.unwrap_or_else(|| PathBuf::from(".")).join(&output);
                    if crate::daemon::is_daemon_running(&idx_path) {
                        match crate::daemon::send_command(&idx_path, "status") {
                            Ok(resp) => eprintln!("Daemon running, state: {}", resp),
                            Err(e) => eprintln!("Daemon running but not responding: {}", e),
                        }
                    } else {
                        eprintln!("No daemon running");
                    }
                }
            }
        }
        #[cfg(not(feature = "daemon"))]
        Commands::Daemon { .. } => {
            eprintln!("Daemon feature not enabled. Rebuild with --features daemon");
        }
    }
    Ok(())
}

fn run_bench(pattern: &str, dir: &std::path::Path, no_ignore: bool, hidden: bool, type_filter: Option<&str>) -> Result<()> {
    println!("Benchmarking pattern '{}' in {:?}", pattern, dir);
    println!("{}", "=".repeat(70));

    let start = Instant::now();
    let full_scan_count = searcher::search_full_scan_count(dir, pattern, no_ignore, hidden, type_filter)?;
    let full_scan_time = start.elapsed();

    let tmp_dir = std::env::temp_dir().join("fgr_bench_index");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    let start = Instant::now();
    persist::build(dir, &tmp_dir, no_ignore, type_filter, false)?;
    let persist_build_time = start.elapsed();

    let start = Instant::now();
    let pidx = persist::load(&tmp_dir)?;
    let persist_load_time = start.elapsed();

    let start = Instant::now();
    let (persist_matches, timing) = searcher::search_persistent_timed(&pidx, pattern, None, hidden)?;
    let persist_search_time = start.elapsed();

    // Get rg match count for correctness comparison
    let rg_count = bench_external_count("rg", &["-c", "--no-filename", pattern, &dir.to_string_lossy()]);
    let grep_time = bench_external("grep", &["-rn", pattern, &dir.to_string_lossy()]);
    let ag_time = bench_external("ag", &["--nocolor", pattern, &dir.to_string_lossy()]);
    let rg_time = bench_external("rg", &["-n", pattern, &dir.to_string_lossy()]);
    let ugrep_time = bench_external("ugrep", &["-rn", pattern, &dir.to_string_lossy()]);

    // Strategy info
    let strategy_label = if timing.strategy.is_empty() { "unknown".to_string() } else { timing.strategy.clone() };
    println!();
    println!("  Strategy: {} (density={:.1} lines/file)", strategy_label, timing.density);

    // Match correctness vs rg
    let fg_count = persist_matches.len();
    if let Some(rg_c) = rg_count {
        if fg_count == rg_c {
            println!("  Matches: {} \u{2713} (matches rg count)", format_num(fg_count));
        } else {
            println!("  MISMATCH: fg={} rg={}", format_num(fg_count), format_num(rg_c));
        }
    } else {
        println!("  Matches: {} (rg not available for comparison)", format_num(fg_count));
    }

    println!();
    println!("{:<35} {:>10} {:>10} {:>8}", "Tool", "Time", "Matches", "Index?");
    println!("{}", "-".repeat(67));
    println!("{:<35} {:>10} {:>10} {:>8}", "fgr (no index)", format_duration(full_scan_time), format_num(full_scan_count), "no");
    let index_label = format!("fgr --index ({})", strategy_label);
    println!("{:<35} {:>10} {:>10} {:>8}", index_label, format_duration(persist_load_time + persist_search_time), format_num(fg_count), "yes");
    println!("{:<35} {:>10} {:>10} {:>8}", "  index build (one-time cost)", format_duration(persist_build_time), "-", "-");
    println!("  Timing breakdown: bitmap={:.1}ms postings+intersect={:.1}ms verify={:.1}ms candidates={} prefix_filtered={}",
        timing.lookup_ms, timing.bitmap_intersect_ms, timing.verify_ms, timing.candidates, timing.prefix_filtered);
    println!("{}", "-".repeat(67));
    if let Some(t) = grep_time { println!("{:<35} {:>10} {:>10} {:>8}", "grep -rn", format_duration(t), "?", "no"); }
    if let Some(t) = ag_time { println!("{:<35} {:>10} {:>10} {:>8}", "ag (the_silver_searcher)", format_duration(t), "?", "no"); }
    if let Some(t) = rg_time { println!("{:<35} {:>10} {:>10} {:>8}", "rg (ripgrep)", format_duration(t), rg_count.map(|c| format_num(c)).unwrap_or("?".into()), "no"); }
    if let Some(t) = ugrep_time { println!("{:<35} {:>10} {:>10} {:>8}", "ugrep", format_duration(t), "?", "no"); }

    let _ = std::fs::remove_dir_all(&tmp_dir);
    Ok(())
}

fn bench_external(cmd: &str, args: &[&str]) -> Option<std::time::Duration> {
    let start = Instant::now();
    let result = std::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match result {
        Ok(_) => Some(start.elapsed()),
        Err(_) => None,
    }
}

/// Run rg -c and sum the per-file counts to get total match count.
fn bench_external_count(cmd: &str, args: &[&str]) -> Option<usize> {
    let output = std::process::Command::new(cmd)
        .args(args)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() && output.stdout.is_empty() {
        return None;
    }
    let total: usize = output.stdout
        .split(|&b| b == b'\n')
        .filter_map(|line| {
            if line.is_empty() { return None; }
            std::str::from_utf8(line).ok()?.trim().parse::<usize>().ok()
        })
        .sum();
    Some(total)
}

fn format_num(n: usize) -> String {
    if n < 1000 {
        return n.to_string();
    }
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

fn format_duration(d: std::time::Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms < 1.0 { format!("{:.1}us", ms * 1000.0) }
    else if ms < 1000.0 { format!("{:.1}ms", ms) }
    else { format!("{:.2}s", ms / 1000.0) }
}
