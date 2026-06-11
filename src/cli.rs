use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::render::{self, ContextOpts, Dispatch, RenderOpts, C_BOLD, C_LINENO, C_PATH, C_RESET};
use crate::{index, persist, searcher};

/// Search options extracted from CLI flags
struct SearchOpts {
    count: bool,
    files_only: bool,
    quiet: bool,
    no_ignore: bool,
    hidden: bool,
    /// `-i` / `--ignore-case`: the search is case-insensitive (the pattern has
    /// been wrapped in `(?i)`). Lets the indexed path route to the
    /// case-insensitive companion index and, on auto-build, build it.
    ignore_case: bool,
    /// `-v` / `--invert-match`: emit lines that do NOT match. Forces the
    /// CLI to route through the direct-scan path even when --index is
    /// set, since the trigram index can only locate matches.
    invert: bool,
    /// `-o` / `--only-matching`: emit one output entry per regex-match
    /// substring rather than per matching line. Doesn't affect --count
    /// (still per-file line counts) or --files-with-matches.
    only_matching: bool,
    /// Allowed file extensions. Empty list = no filter. Repeated flags
    /// accumulate so `--type rs --type ts` matches both.
    file_type: Vec<String>,
    /// Glob patterns; a file is searched if it matches any include glob.
    /// Empty list = no positive filter.
    include: Vec<String>,
    /// Glob patterns; a file is excluded if it matches any exclude glob.
    /// Empty list = no negative filter.
    exclude: Vec<String>,
    /// Force the grouped (heading) output format on/off. None = follow TTY.
    heading_override: Option<bool>,
    /// Resolved before/after context window for `-A` / `-B` / `-C`.
    context: ContextOpts,
    /// Effective regex pattern (may have (?i) prefix etc.) — used by the
    /// renderer to highlight matched substrings.
    pattern: Option<String>,
}

#[derive(Parser)]
#[command(
    name = "fgr",
    version,
    about = "Fast grep with sparse n-gram index — drop-in grep replacement",
    args_conflicts_with_subcommands = true
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
    #[arg(
        short = 'B',
        long = "before-context",
        value_name = "NUM",
        global = true
    )]
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

    /// Include only files matching GLOB. May be specified multiple times;
    /// the union of all globs is included.
    #[arg(long = "include", value_name = "GLOB", global = true)]
    pub include: Vec<String>,

    /// Exclude files matching GLOB. May be specified multiple times; a file
    /// is excluded if it matches any of the globs.
    #[arg(long = "exclude", value_name = "GLOB", global = true)]
    pub exclude: Vec<String>,

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

    /// Group results under a file-name heading (default when stdout is a TTY)
    #[arg(long = "heading", global = true, overrides_with = "no_heading")]
    pub heading: bool,

    /// Print one match per line as `path:line:content` (default when stdout is piped)
    #[arg(
        short = 'N',
        long = "no-heading",
        global = true,
        overrides_with = "heading"
    )]
    pub no_heading: bool,

    /// Disable Unicode matching mode — `\b`, `\w`, `\s` etc. fall back to
    /// ASCII-only definitions.
    #[arg(short = 'U', long = "no-unicode", global = true)]
    pub no_unicode: bool,

    /// Filter by file extension (e.g., --type rs). May be specified
    /// multiple times; a file is searched if its extension matches any
    /// of the listed types.
    #[arg(long = "type", value_name = "EXT", global = true)]
    pub file_type: Vec<String>,
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
    Bench { pattern: String, dir: PathBuf },
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

/// Enable ANSI/VT escape processing on Windows consoles. Win10 build 1607+
/// supports VT but each process must opt in via SetConsoleMode — without this,
/// cmd.exe renders our color escapes as raw text. Best-effort: failure (older
/// Windows, redirected stdout) leaves the console mode untouched, which falls
/// back to the same behavior as before.
#[cfg(windows)]
fn enable_ansi_on_windows() {
    use std::os::windows::io::AsRawHandle;
    extern "system" {
        fn GetConsoleMode(h: *mut core::ffi::c_void, m: *mut u32) -> i32;
        fn SetConsoleMode(h: *mut core::ffi::c_void, m: u32) -> i32;
    }
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    let h = std::io::stdout().as_raw_handle() as *mut core::ffi::c_void;
    let mut mode = 0u32;
    unsafe {
        if GetConsoleMode(h, &mut mode) != 0 {
            let _ = SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}

#[cfg(not(windows))]
fn enable_ansi_on_windows() {}

pub fn run() -> Result<()> {
    enable_ansi_on_windows();

    let cli = Cli::parse();

    // overrides_with on the flags means clap surfaces only one of them as
    // true — whichever was passed last on the CLI. None here means neither
    // was set, so the renderer falls back to TTY auto-detect.
    let heading_override = if cli.no_heading {
        Some(false)
    } else if cli.heading {
        Some(true)
    } else {
        None
    };

    let context = ContextOpts::resolve(cli.context, cli.before_context, cli.after_context);

    let opts = SearchOpts {
        count: cli.count,
        files_only: cli.files_only,
        quiet: cli.quiet,
        no_ignore: cli.no_ignore,
        hidden: cli.hidden,
        ignore_case: cli.ignore_case,
        invert: cli.invert_match,
        only_matching: cli.only_matching,
        file_type: cli.file_type.clone(),
        include: cli.include.clone(),
        exclude: cli.exclude.clone(),
        heading_override,
        context,
        pattern: None, // populated below once the effective pattern is built
    };

    if let Some(cmd) = cli.command {
        return run_subcommand(
            cmd,
            opts.no_ignore,
            opts.hidden,
            &opts.file_type,
            opts.ignore_case,
        );
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
    // `--no-unicode` is honoured by inlining `(?-u)` at the start of the
    // pattern. Both the regex crate and our `Matcher` respect the inline
    // flag, so no separate plumbing through `Matcher::new` is needed.
    // Trade-off: this disables the pure-literal fast path for patterns
    // that would otherwise have hit it (literals don't actually care about
    // Unicode mode, but `(?-u)<literal>` is no longer a "pure literal"
    // syntactically). We accept that — `--no-unicode` is niche enough
    // that the slow regex path is fine.
    if cli.no_unicode {
        effective = format!("(?-u){}", effective);
    }

    let mut opts = opts;
    opts.pattern = Some(effective.clone());

    // Invert-match can't use the index — the trigram index locates *matches*,
    // so a "lines that don't match" query can't be answered from it; it always
    // routes through the direct-scan path.
    //
    // Case-insensitive search CAN use the index when a case-insensitive
    // companion (`fgr index -i`) is present: `search_timed` resolves `(?i)`
    // against the folded store, and transparently falls back to scanning all
    // live docs when no CI index exists. Routing it through the indexed path
    // also lets a first `-i` search auto-build the CI index.
    if let Some(ref idx_path) = cli.index_path {
        if cli.invert_match {
            run_direct_search(&effective, &dir, &opts)?;
        } else {
            run_indexed_search(&effective, idx_path, dir.as_path(), &opts)?;
        }
    } else {
        run_direct_search(&effective, &dir, &opts)?;
    }

    Ok(())
}

/// Effective grouping mode: explicit --heading / --no-heading wins; otherwise
/// auto-detect from the TTY.
fn use_heading(opts: &SearchOpts) -> bool {
    opts.heading_override
        .unwrap_or_else(|| std::io::stdout().is_terminal())
}

/// Whether to emit ANSI colour escapes. Strictly TTY-driven so a forced
/// `--heading` while piped (`fgr --heading | less -R` excepted) doesn't
/// leak raw escape sequences into a non-terminal sink. Heading and colour
/// are independent: you can group without colour and colour without group.
fn use_color() -> bool {
    std::io::stdout().is_terminal()
}

fn run_direct_search(pattern: &str, dir: &std::path::Path, opts: &SearchOpts) -> Result<()> {
    // count/files-only/quiet bypass the render pipeline entirely — they
    // produce per-file aggregates (counts, file lists) or no output at
    // all, so context flags don't apply and the simpler Vec<Match> API
    // is what we want.
    if opts.count || opts.files_only || opts.quiet {
        let start = Instant::now();
        let matches = searcher::search_full_scan(
            dir,
            pattern,
            opts.no_ignore,
            opts.hidden,
            &opts.file_type,
            &opts.include,
            &opts.exclude,
            opts.invert,
        )?;
        let elapsed = start.elapsed();
        output_summary(&matches, opts)?;
        if !opts.quiet {
            eprintln!(
                "Searched in {:.2}ms, {} matches",
                elapsed.as_secs_f64() * 1000.0,
                matches.len()
            );
        }
        if opts.quiet && matches.is_empty() {
            std::process::exit(1);
        }
        return Ok(());
    }

    let render_opts = render_opts_for(opts);
    let dispatch = dispatch_for(&render_opts);

    let start = Instant::now();
    let stdout = std::io::stdout();
    let output = Mutex::new(std::io::BufWriter::new(stdout));
    let count = render::search_full_scan_render(
        dir,
        pattern,
        opts.no_ignore,
        opts.hidden,
        &opts.file_type,
        &opts.include,
        &opts.exclude,
        &opts.context,
        &render_opts,
        dispatch,
        &output,
    )?;
    {
        let mut out = output.lock().unwrap();
        let _ = out.flush();
    }
    let elapsed = start.elapsed();
    eprintln!(
        "Searched in {:.2}ms, {} matches",
        elapsed.as_secs_f64() * 1000.0,
        count
    );
    Ok(())
}

fn run_indexed_search(
    pattern: &str,
    idx_path: &std::path::Path,
    search_path: &std::path::Path,
    opts: &SearchOpts,
) -> Result<()> {
    // Auto-build the index on first use. We detect "no index" by the absence of
    // meta.json (the same probe persist::load uses internally). The build root
    // is the search PATH the user passed — this matches the natural intent
    // "give me a fast search over this directory."
    if !persist::is_current(idx_path) {
        let reason = if idx_path.join("meta.json").exists() {
            "outdated (format changed)"
        } else {
            "not found"
        };
        eprintln!(
            "Index {} at {} — building one-time (subsequent searches will be <200ms)…",
            reason,
            idx_path.display()
        );
        let build_start = Instant::now();
        persist::build(
            search_path,
            idx_path,
            opts.no_ignore,
            &opts.file_type,
            true,
            opts.ignore_case,
        )?;
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
        let (n, _) = searcher::search_persistent_count(
            &idx,
            pattern,
            path_filter.as_deref(),
            opts.hidden,
            &opts.file_type,
            &opts.include,
            &opts.exclude,
        )?;
        let search_time = start.elapsed();
        println!("{}", n);
        if !opts.quiet {
            eprintln!(
                "Load: {:.1}ms, Search: {:.1}ms",
                load_time.as_secs_f64() * 1000.0,
                search_time.as_secs_f64() * 1000.0
            );
        }
        return Ok(());
    }

    if opts.files_only || opts.quiet {
        // Same as direct: bypass render pipeline for these aggregate modes.
        let (matches, _) = searcher::search_persistent_timed(
            &idx,
            pattern,
            path_filter.as_deref(),
            opts.hidden,
            &opts.file_type,
            &opts.include,
            &opts.exclude,
        )?;
        output_summary(&matches, opts)?;
        let search_time = start.elapsed();
        if !opts.quiet {
            eprintln!(
                "Load: {:.1}ms, Search: {:.1}ms, {} matches",
                load_time.as_secs_f64() * 1000.0,
                search_time.as_secs_f64() * 1000.0,
                matches.len()
            );
        }
        if opts.quiet && matches.is_empty() {
            std::process::exit(1);
        }
        return Ok(());
    }

    let render_opts = render_opts_for(opts);
    let dispatch = dispatch_for(&render_opts);

    let stdout = std::io::stdout();
    let output = Mutex::new(std::io::BufWriter::new(stdout));
    let (count, _) = render::search_persistent_render(
        &idx,
        pattern,
        path_filter.as_deref(),
        opts.hidden,
        &opts.file_type,
        &opts.include,
        &opts.exclude,
        &opts.context,
        &render_opts,
        dispatch,
        &output,
    )?;
    {
        let mut out = output.lock().unwrap();
        let _ = out.flush();
    }
    let search_time = start.elapsed();
    if !opts.quiet {
        eprintln!(
            "Load: {:.1}ms, Search: {:.1}ms, {} matches",
            load_time.as_secs_f64() * 1000.0,
            search_time.as_secs_f64() * 1000.0,
            count
        );
    }
    Ok(())
}

/// Build a `RenderOpts` from CLI flags. Heading honours --heading/--no-heading
/// then falls back to TTY auto-detect; colour is strictly TTY-driven so a
/// piped `--heading` doesn't leak escapes.
fn render_opts_for(opts: &SearchOpts) -> RenderOpts {
    RenderOpts {
        heading: use_heading(opts),
        color: use_color(),
        invert: opts.invert,
        only_matching: opts.only_matching,
        pattern: opts.pattern.clone(),
    }
}

/// Streaming dispatch is only safe when neither heading nor colour are on:
/// both require buffered, sorted output (heading wants stable file order,
/// colour can't be applied per-byte during a streaming write).
fn dispatch_for(render_opts: &RenderOpts) -> Dispatch {
    if render_opts.heading || render_opts.color {
        Dispatch::Sorted
    } else {
        Dispatch::Streaming
    }
}

/// Aggregate-mode output: `--count` (one `path:N` per file) and
/// `--files-with-matches` (one path per file). `--quiet` is a no-op here.
/// The full match-line render path lives in `render::*` now; this function
/// is the leftover that used to handle every output mode.
fn output_summary(matches: &[searcher::Match], opts: &SearchOpts) -> Result<()> {
    if opts.quiet {
        return Ok(());
    }
    if opts.count {
        let mut counts: std::collections::HashMap<&PathBuf, usize> =
            std::collections::HashMap::new();
        for m in matches {
            *counts.entry(&m.path).or_insert(0) += 1;
        }
        let mut pairs: Vec<_> = counts.into_iter().collect();
        pairs.sort_by(|a, b| a.0.cmp(b.0));
        let tty = use_color();
        for (path, count) in pairs {
            if tty {
                println!(
                    "{}{}{}{}:{}{}{}",
                    C_BOLD,
                    C_PATH,
                    path.display(),
                    C_RESET,
                    C_LINENO,
                    count,
                    C_RESET
                );
            } else {
                println!("{}:{}", path.display(), count);
            }
        }
        return Ok(());
    }
    if opts.files_only {
        let mut files: Vec<_> = matches.iter().map(|m| &m.path).collect();
        files.sort();
        files.dedup();
        let tty = use_color();
        for f in files {
            if tty {
                println!("{}{}{}{}", C_BOLD, C_PATH, f.display(), C_RESET);
            } else {
                println!("{}", f.display());
            }
        }
    }
    Ok(())
}

fn run_subcommand(
    cmd: Commands,
    no_ignore: bool,
    hidden: bool,
    type_filter: &[String],
    case_insensitive: bool,
) -> Result<()> {
    match cmd {
        Commands::Index {
            dir,
            output,
            daemon,
        } => {
            let idx_path = dir.join(&output);
            let start = Instant::now();
            persist::build(
                &dir,
                &idx_path,
                no_ignore,
                type_filter,
                true,
                case_insensitive,
            )?;
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
                if let Some(ref bm) = idx.bitmap_mmap {
                    println!("  Bitmaps size:  {}KB", bm.len() / 1024);
                }
            } else {
                let idx = index::SparseIndex::build_from_directory(
                    &index_path,
                    no_ignore,
                    type_filter,
                    false,
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
        #[cfg(feature = "daemon")]
        Commands::Daemon { action } => match action {
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
        },
        #[cfg(not(feature = "daemon"))]
        Commands::Daemon { .. } => {
            eprintln!("Daemon feature not enabled. Rebuild with --features daemon");
        }
    }
    Ok(())
}

fn run_bench(
    pattern: &str,
    dir: &std::path::Path,
    no_ignore: bool,
    hidden: bool,
    type_filter: &[String],
) -> Result<()> {
    println!("Benchmarking pattern '{}' in {:?}", pattern, dir);
    println!("{}", "=".repeat(70));

    let start = Instant::now();
    // Bench is a built-in self-comparison; include/exclude/invert are
    // search-time options that don't apply here.
    let full_scan_count = searcher::search_full_scan_count(
        dir,
        pattern,
        no_ignore,
        hidden,
        type_filter,
        &[],
        &[],
        false,
    )?;
    let full_scan_time = start.elapsed();

    let tmp_dir = std::env::temp_dir().join("fgr_bench_index");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    let start = Instant::now();
    persist::build(dir, &tmp_dir, no_ignore, type_filter, false, false)?;
    let persist_build_time = start.elapsed();

    let start = Instant::now();
    let pidx = persist::load(&tmp_dir)?;
    let persist_load_time = start.elapsed();

    let start = Instant::now();
    let (persist_matches, timing) =
        searcher::search_persistent_timed(&pidx, pattern, None, hidden, &[], &[], &[])?;
    let persist_search_time = start.elapsed();

    // Get rg match count for correctness comparison
    let rg_count = bench_external_count(
        "rg",
        &["-c", "--no-filename", pattern, &dir.to_string_lossy()],
    );
    let grep_time = bench_external("grep", &["-rn", pattern, &dir.to_string_lossy()]);
    let ag_time = bench_external("ag", &["--nocolor", pattern, &dir.to_string_lossy()]);
    let rg_time = bench_external("rg", &["-n", pattern, &dir.to_string_lossy()]);
    let ugrep_time = bench_external("ugrep", &["-rn", pattern, &dir.to_string_lossy()]);

    // Strategy info
    let strategy_label = if timing.strategy.is_empty() {
        "unknown".to_string()
    } else {
        timing.strategy.clone()
    };
    println!();
    println!(
        "  Strategy: {} (density={:.1} lines/file)",
        strategy_label, timing.density
    );

    // Match correctness vs rg
    let fg_count = persist_matches.len();
    if let Some(rg_c) = rg_count {
        if fg_count == rg_c {
            println!(
                "  Matches: {} \u{2713} (matches rg count)",
                format_num(fg_count)
            );
        } else {
            println!(
                "  MISMATCH: fg={} rg={}",
                format_num(fg_count),
                format_num(rg_c)
            );
        }
    } else {
        println!(
            "  Matches: {} (rg not available for comparison)",
            format_num(fg_count)
        );
    }

    println!();
    println!(
        "{:<35} {:>10} {:>10} {:>8}",
        "Tool", "Time", "Matches", "Index?"
    );
    println!("{}", "-".repeat(67));
    println!(
        "{:<35} {:>10} {:>10} {:>8}",
        "fgr (no index)",
        format_duration(full_scan_time),
        format_num(full_scan_count),
        "no"
    );
    let index_label = format!("fgr --index ({})", strategy_label);
    println!(
        "{:<35} {:>10} {:>10} {:>8}",
        index_label,
        format_duration(persist_load_time + persist_search_time),
        format_num(fg_count),
        "yes"
    );
    println!(
        "{:<35} {:>10} {:>10} {:>8}",
        "  index build (one-time cost)",
        format_duration(persist_build_time),
        "-",
        "-"
    );
    println!("  Timing breakdown: bitmap={:.1}ms postings+intersect={:.1}ms verify={:.1}ms candidates={} prefix_filtered={}",
        timing.lookup_ms, timing.bitmap_intersect_ms, timing.verify_ms, timing.candidates, timing.prefix_filtered);
    println!("{}", "-".repeat(67));
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
            rg_count.map(|c| format_num(c)).unwrap_or("?".into()),
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
    let total: usize = output
        .stdout
        .split(|&b| b == b'\n')
        .filter_map(|line| {
            if line.is_empty() {
                return None;
            }
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
    if ms < 1.0 {
        format!("{:.1}us", ms * 1000.0)
    } else if ms < 1000.0 {
        format!("{:.1}ms", ms)
    } else {
        format!("{:.2}s", ms / 1000.0)
    }
}
