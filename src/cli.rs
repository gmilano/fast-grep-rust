use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::{index, persist, searcher};

#[derive(Parser)]
#[command(
    name = "fgr",
    version,
    about = "Fast grep with sparse n-gram index — drop-in grep replacement",
    // Allow `fgr PATTERN [DIR]` without a subcommand
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

    // ── grep-compatible flags ──

    /// Recurse into directories (default: on, for grep compat)
    #[arg(short = 'r', long = "recursive", global = true)]
    pub recursive: bool,

    /// Only print count of matching lines per file
    #[arg(short = 'c', long = "count", global = true)]
    pub count: bool,

    /// Only print names of files with matches
    #[arg(short = 'l', long = "files-with-matches", global = true)]
    pub files_only: bool,

    /// Print line numbers with output
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

    // ── fast-grep specific flags ──

    /// Use persistent index for searching (path to .fgr dir)
    #[arg(long = "index", value_name = "PATH", global = true)]
    pub index_path: Option<PathBuf>,

    /// Don't respect .gitignore
    #[arg(long, global = true)]
    pub no_ignore: bool,

    /// Filter by file extension (e.g., --type rs)
    #[arg(long = "type", value_name = "EXT", global = true)]
    pub file_type: Option<String>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Build a persistent index
    #[command(name = "index")]
    Index {
        /// Directory to index
        dir: PathBuf,
        /// Output directory for index files
        #[arg(long, default_value = ".fgr")]
        output: PathBuf,
    },
    /// Benchmark search performance
    #[command(name = "bench")]
    Bench {
        /// Regex pattern to benchmark
        pattern: String,
        /// Directory to search
        dir: PathBuf,
    },
    /// Incrementally update a persistent index
    #[command(name = "update")]
    Update {
        /// Directory that was indexed
        dir: Option<PathBuf>,
        /// Path to persistent index
        #[arg(long, default_value = ".fgr")]
        index: PathBuf,
    },
    /// Show index statistics
    #[command(name = "stats")]
    Stats {
        /// Path to persistent index
        #[arg(long, default_value = ".fgr")]
        index: PathBuf,
    },
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();

    // If a subcommand was given, dispatch to it
    // Destructure to avoid partial move issues
    let Cli {
        pattern,
        path,
        command,
        count,
        files_only,
        line_number,
        ignore_case,
        fixed_strings,
        quiet,
        index_path,
        no_ignore,
        file_type,
        ..
    } = cli;

    if let Some(cmd) = command {
        return run_subcommand(cmd, no_ignore, file_type.as_deref());
    }

    // grep-compatible search mode
    let pattern = match pattern {
        Some(p) => p,
        None => {
            eprintln!("Usage: fgr [OPTIONS] PATTERN [PATH]");
            eprintln!("Try 'fgr --help' for more information.");
            std::process::exit(2);
        }
    };

    let dir = path.unwrap_or_else(|| PathBuf::from("."));

    // Build the effective regex pattern
    let effective_pattern = if fixed_strings {
        regex::escape(&pattern)
    } else {
        pattern.clone()
    };
    let effective_pattern = if ignore_case {
        format!("(?i){}", effective_pattern)
    } else {
        effective_pattern
    };

    if let Some(idx_path) = &index_path {
        // Indexed search
        let start = Instant::now();
        let idx = persist::load(idx_path)?;
        let load_time = start.elapsed();

        let start = Instant::now();
        if count {
            let (n, timing) = searcher::search_persistent_count(&idx, &effective_pattern)?;
            let search_time = start.elapsed();
            println!("{}", n);
            if !quiet {
                eprintln!(
                    "Load: {:.1}ms | Search: {:.1}ms (ngrams:{:.2}ms lookup:{:.2}ms intersect:{:.2}ms verify:{:.2}ms) | {} candidates -> {} matches | {} trigrams",
                    load_time.as_secs_f64() * 1000.0,
                    search_time.as_secs_f64() * 1000.0,
                    timing.covering_ngrams_ms, timing.lookup_ms,
                    timing.bitmap_intersect_ms, timing.verify_ms,
                    timing.candidates, n, timing.ngrams_queried,
                );
            }
        } else {
            let (matches, timing) = searcher::search_persistent_timed(&idx, &effective_pattern)?;
            let search_time = start.elapsed();

            if !quiet {
                if files_only {
                    let mut files: Vec<_> = matches.iter().map(|m| &m.path).collect();
                    files.sort();
                    files.dedup();
                    for f in files {
                        println!("{}", f.display());
                    }
                } else {
                    use std::io::Write;
                    let stdout = std::io::stdout();
                    let mut out = std::io::BufWriter::new(stdout.lock());
                    for m in &matches {
                        let _ = writeln!(out, "{}:{}:{}", m.path.display(), m.line_number, m.line);
                    }
                }
                eprintln!(
                    "Load: {:.1}ms | Search: {:.1}ms (ngrams:{:.2}ms lookup:{:.2}ms intersect:{:.2}ms verify:{:.2}ms) | {} candidates -> {} matches | {} trigrams",
                    load_time.as_secs_f64() * 1000.0,
                    search_time.as_secs_f64() * 1000.0,
                    timing.covering_ngrams_ms, timing.lookup_ms,
                    timing.bitmap_intersect_ms, timing.verify_ms,
                    timing.candidates, matches.len(), timing.ngrams_queried,
                );
            }
        }
    } else {
        // Direct full scan
        let start = Instant::now();
        let matches = searcher::search_full_scan(&dir, &effective_pattern, no_ignore, file_type.as_deref())?;
        let elapsed = start.elapsed();

        if !quiet {
            if count {
                println!("{}", matches.len());
            } else if files_only {
                let mut files: Vec<_> = matches.iter().map(|m| &m.path).collect();
                files.sort();
                files.dedup();
                for f in files {
                    println!("{}", f.display());
                }
            } else {
                use std::io::Write;
                let stdout = std::io::stdout();
                let mut out = std::io::BufWriter::new(stdout.lock());
                for m in &matches {
                    let _ = writeln!(out, "{}:{}:{}", m.path.display(), m.line_number, m.line);
                }
            }
            eprintln!("Full scan: {:.2}ms, {} matches", elapsed.as_secs_f64() * 1000.0, matches.len());
        }

        if quiet && matches.is_empty() {
            std::process::exit(1);
        }
    }

    Ok(())
}

fn run_subcommand(cmd: Commands, no_ignore: bool, type_filter: Option<&str>) -> Result<()> {
    match cmd {
        Commands::Index { dir, output } => {
            let start = Instant::now();
            persist::build(&dir, &output, no_ignore, type_filter, true)?;
            eprintln!("Index built in {:.2}s", start.elapsed().as_secs_f64());
        }

        Commands::Bench { pattern, dir } => {
            run_bench(&pattern, &dir, no_ignore, type_filter)?;
        }

        Commands::Update {
            dir,
            index: idx_path,
        } => {
            let root = if let Some(d) = dir {
                d
            } else {
                let probe = persist::load(&idx_path)?;
                PathBuf::from(&probe.meta.root_dir)
            };
            let stats = persist::update_incremental(&idx_path, &root, true)?;
            if stats.added == 0 && stats.modified == 0 && stats.deleted == 0 {
                eprintln!("Index is up to date ({} files)", stats.unchanged);
            } else {
                eprintln!(
                    "Updated index: +{} added, {} modified, {} deleted (unchanged: {}) in {}ms",
                    stats.added, stats.modified, stats.deleted, stats.unchanged, stats.duration_ms
                );
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
            } else {
                let idx = index::SparseIndex::build_from_directory(
                    &index_path,
                    no_ignore,
                    type_filter,
                    false,
                )?;
                let stats = idx.stats();
                println!("In-memory Index Stats:");
                println!("  Documents:    {}", stats.num_docs);
                println!("  N-grams:      {}", stats.num_ngrams);
                println!(
                    "  Estimated RAM: {}MB",
                    stats.estimated_ram_bytes / (1024 * 1024)
                );
                println!("  Avg postings len: {:.1}", stats.avg_postings_len);
            }
        }
    }

    Ok(())
}

fn run_bench(
    pattern: &str,
    dir: &std::path::Path,
    no_ignore: bool,
    type_filter: Option<&str>,
) -> Result<()> {
    println!("Benchmarking pattern '{}' in {:?}", pattern, dir);
    println!("{}", "=".repeat(70));

    // 1. Full scan (our optimized path)
    let start = Instant::now();
    let full_scan_matches = searcher::search_full_scan(dir, pattern, no_ignore, type_filter)?;
    let full_scan_time = start.elapsed();

    // 2. Persistent: build + search
    let tmp_dir = std::env::temp_dir().join("fgr_bench_index");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    let start = Instant::now();
    persist::build(dir, &tmp_dir, no_ignore, type_filter, false)?;
    let persist_build_time = start.elapsed();

    let start = Instant::now();
    let pidx = persist::load(&tmp_dir)?;
    let persist_load_time = start.elapsed();

    let start = Instant::now();
    let (persist_matches, _timing) = searcher::search_persistent_timed(&pidx, pattern)?;
    let persist_search_time = start.elapsed();

    // 3. External tools
    let grep_time = bench_external("grep", &["-rn", pattern, &dir.to_string_lossy()]);
    let ag_time = bench_external("ag", &["--nocolor", pattern, &dir.to_string_lossy()]);
    let rg_time = bench_external("rg", &["-n", pattern, &dir.to_string_lossy()]);
    let ugrep_time = bench_external("ugrep", &["-rn", pattern, &dir.to_string_lossy()]);

    // Print results
    println!();
    println!(
        "{:<35} {:>10} {:>10} {:>8}",
        "Tool", "Time", "Matches", "Index?"
    );
    println!("{}", "-".repeat(67));

    // fast-grep variants
    println!(
        "{:<35} {:>10} {:>10} {:>8}",
        "fgr (no index)",
        format_duration(full_scan_time),
        full_scan_matches.len(),
        "no"
    );
    println!(
        "{:<35} {:>10} {:>10} {:>8}",
        "fgr --index (load+search)",
        format_duration(persist_load_time + persist_search_time),
        persist_matches.len(),
        "yes"
    );
    println!(
        "{:<35} {:>10} {:>10} {:>8}",
        "  index build (one-time cost)",
        format_duration(persist_build_time),
        "-",
        "-"
    );

    println!("{}", "-".repeat(67));

    // External tools
    if let Some(t) = grep_time {
        println!(
            "{:<35} {:>10} {:>10} {:>8}",
            "grep -rn",
            format_duration(t),
            "?",
            "no"
        );
    }
    if let Some(t) = ag_time {
        println!(
            "{:<35} {:>10} {:>10} {:>8}",
            "ag (the_silver_searcher)",
            format_duration(t),
            "?",
            "no"
        );
    }
    if let Some(t) = rg_time {
        println!(
            "{:<35} {:>10} {:>10} {:>8}",
            "rg (ripgrep)",
            format_duration(t),
            "?",
            "no"
        );
    }
    if let Some(t) = ugrep_time {
        println!(
            "{:<35} {:>10} {:>10} {:>8}",
            "ugrep",
            format_duration(t),
            "?",
            "no"
        );
    }

    // Cleanup
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

fn format_duration(d: std::time::Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms < 1.0 {
        format!("{:.1}us", ms * 1000.0)
    } else if ms < 1000.0 {
        format!("{:.1}ms", ms)
    } else {
        format!("{:.2}s", ms / 1000.0)
    }
}
