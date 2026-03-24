use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::{index, persist, searcher};

#[derive(Parser)]
#[command(name = "fast-grep", version, about = "Fast grep with sparse n-gram index")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Don't respect .gitignore
    #[arg(long, global = true)]
    pub no_ignore: bool,

    /// Filter by file extension
    #[arg(long = "type", global = true)]
    pub file_type: Option<String>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Build a persistent index
    Index {
        /// Directory to index
        dir: PathBuf,
        /// Output directory for index files
        #[arg(long, default_value = ".fgr")]
        output: PathBuf,
    },
    /// Search for a pattern
    Search {
        /// Regex pattern to search
        pattern: String,
        /// Directory to search (default: current dir)
        dir: Option<PathBuf>,
        /// Path to persistent index
        #[arg(long)]
        index: Option<PathBuf>,
        /// Only show match count
        #[arg(long)]
        count: bool,
        /// Only show matching file paths
        #[arg(long)]
        files_only: bool,
    },
    /// Benchmark search performance
    Bench {
        /// Regex pattern to benchmark
        pattern: String,
        /// Directory to search
        dir: PathBuf,
    },
    /// Show index statistics
    Stats {
        /// Path to persistent index
        #[arg(long, default_value = ".fgr")]
        index: PathBuf,
    },
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Index { dir, output } => {
            let start = Instant::now();
            persist::build(
                &dir,
                &output,
                cli.no_ignore,
                cli.file_type.as_deref(),
                true,
            )?;
            eprintln!("Index built in {:.2}s", start.elapsed().as_secs_f64());
        }

        Commands::Search {
            pattern,
            dir,
            index: index_path,
            count,
            files_only,
        } => {
            if let Some(idx_path) = index_path {
                // Use persistent index
                let start = Instant::now();
                let idx = persist::load(&idx_path)?;
                let load_time = start.elapsed();

                let start = Instant::now();
                let matches = searcher::search_persistent(&idx, &pattern)?;
                let search_time = start.elapsed();

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
                    for m in &matches {
                        println!("{}:{}:{}", m.path.display(), m.line_number, m.line);
                    }
                }

                eprintln!(
                    "Index load: {:.2}ms, Search: {:.2}ms, {} matches",
                    load_time.as_secs_f64() * 1000.0,
                    search_time.as_secs_f64() * 1000.0,
                    matches.len()
                );
            } else {
                // In-memory search
                let dir = dir.unwrap_or_else(|| PathBuf::from("."));
                let start = Instant::now();
                let s = searcher::Searcher::new(&dir, cli.no_ignore, cli.file_type.as_deref())?;
                let build_time = start.elapsed();

                let start = Instant::now();
                if count {
                    let n = s.search_count(&pattern)?;
                    println!("{}", n);
                } else if files_only {
                    let files = s.search_files_only(&pattern)?;
                    for f in &files {
                        println!("{}", f.display());
                    }
                } else {
                    let matches = s.search(&pattern)?;
                    for m in &matches {
                        println!("{}:{}:{}", m.path.display(), m.line_number, m.line);
                    }
                }
                let search_time = start.elapsed();

                eprintln!(
                    "Index build: {:.2}ms, Search: {:.2}ms",
                    build_time.as_secs_f64() * 1000.0,
                    search_time.as_secs_f64() * 1000.0,
                );
            }
        }

        Commands::Bench { pattern, dir } => {
            run_bench(&pattern, &dir, cli.no_ignore, cli.file_type.as_deref())?;
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
                println!(
                    "  Postings size: {}KB",
                    idx.postings_mmap.len() / 1024
                );
            } else {
                // Build in-memory and show stats
                let idx =
                    index::SparseIndex::build_from_directory(&index_path, cli.no_ignore, cli.file_type.as_deref(), false)?;
                let stats = idx.stats();
                println!("In-memory Index Stats:");
                println!("  Documents:    {}", stats.num_docs);
                println!("  N-grams:      {}", stats.num_ngrams);
                println!("  Estimated RAM: {}MB", stats.estimated_ram_bytes / (1024 * 1024));
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

    // 1. Full scan with regex crate (baseline)
    let start = Instant::now();
    let full_scan_matches = searcher::search_full_scan(dir, pattern, no_ignore, type_filter)?;
    let full_scan_time = start.elapsed();

    // 2. In-memory: build + search
    let start = Instant::now();
    let s = searcher::Searcher::new(dir, no_ignore, type_filter)?;
    let build_time = start.elapsed();

    let start = Instant::now();
    let inmem_matches = s.search(pattern)?;
    let inmem_search_time = start.elapsed();

    // 3. Persistent: build + search
    let tmp_dir = std::env::temp_dir().join("fgr_bench_index");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    let start = Instant::now();
    persist::build(dir, &tmp_dir, no_ignore, type_filter, false)?;
    let persist_build_time = start.elapsed();

    let start = Instant::now();
    let pidx = persist::load(&tmp_dir)?;
    let persist_load_time = start.elapsed();

    let start = Instant::now();
    let persist_matches = searcher::search_persistent(&pidx, pattern)?;
    let persist_search_time = start.elapsed();

    // 4. System grep (if available)
    let grep_time = bench_external("grep", &["-r", "-n", pattern, &dir.to_string_lossy()]);

    // 5. ripgrep (if available)
    let rg_time = bench_external("rg", &["-n", pattern, &dir.to_string_lossy()]);

    // Position mask stats
    let idx_for_stats =
        index::SparseIndex::build_from_directory(dir, no_ignore, type_filter, false)?;
    let inmem_stats = idx_for_stats.search_with_stats(pattern, inmem_matches.len());

    // Print results
    println!();
    println!("{:<30} {:>12} {:>10}", "Method", "Time", "Matches");
    println!("{}", "-".repeat(54));
    println!(
        "{:<30} {:>12} {:>10}",
        "Full scan (regex crate)",
        format_duration(full_scan_time),
        full_scan_matches.len()
    );
    println!(
        "{:<30} {:>12} {:>10}",
        "In-memory (build)",
        format_duration(build_time),
        "-"
    );
    println!(
        "{:<30} {:>12} {:>10}",
        "In-memory (search)",
        format_duration(inmem_search_time),
        inmem_matches.len()
    );
    println!(
        "{:<30} {:>12} {:>10}",
        "In-memory (total)",
        format_duration(build_time + inmem_search_time),
        inmem_matches.len()
    );
    println!(
        "{:<30} {:>12} {:>10}",
        "Persistent (build)",
        format_duration(persist_build_time),
        "-"
    );
    println!(
        "{:<30} {:>12} {:>10}",
        "Persistent (load)",
        format_duration(persist_load_time),
        "-"
    );
    println!(
        "{:<30} {:>12} {:>10}",
        "Persistent (search)",
        format_duration(persist_search_time),
        persist_matches.len()
    );
    println!(
        "{:<30} {:>12} {:>10}",
        "Persistent (load+search)",
        format_duration(persist_load_time + persist_search_time),
        persist_matches.len()
    );

    if let Some(t) = grep_time {
        println!("{:<30} {:>12} {:>10}", "grep -r", format_duration(t), "?");
    }
    if let Some(t) = rg_time {
        println!("{:<30} {:>12} {:>10}", "ripgrep (rg)", format_duration(t), "?");
    }

    // Position mask statistics
    println!();
    println!("Position Mask Stats (Blackbird):");
    println!("  Index candidates:     {}", inmem_stats.candidates);
    println!("  Verified matches:     {}", inmem_stats.verified);
    println!(
        "  False positive rate:  {:.2}%",
        inmem_stats.false_positive_rate * 100.0
    );

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
