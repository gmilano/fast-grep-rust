use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::render::{
    self, ContextOpts, Dispatch, JsonEnvelope, Limits, RenderKind, RenderOpts, RenderStats, C_BOLD,
    C_LINENO, C_PATH, C_RESET,
};
use crate::{index, persist, searcher};

/// Output layout. Resolved from `--format`, then `--agent-aggressive` /
/// `--agent`, then `--heading`/`--no-heading`, then the `FGR_FORMAT` env var,
/// then a TTY default (heading in a terminal, grep when piped — preserving
/// drop-in grep behaviour for scripts).
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    /// `path:line:content` per match — flat, grep-compatible (piped default).
    Grep,
    /// File path as a heading, then `line:content` lines (TTY default).
    Heading,
    /// Heading + paths relative to the search root: fewest tokens for LLM/agent
    /// consumers, without losing the path, line number, or content.
    Compact,
    /// One JSON document: `{query, root, indexed, elapsed_ms, total_matches,
    /// files:[{path, matches:[{line, text}]}], truncated}`. Always valid.
    Json,
    /// One JSON object per match: `{"path","line","text"}` — streaming-friendly.
    Jsonl,
}

impl OutputFormat {
    /// Parse a `FGR_FORMAT` value (case-insensitive). Unknown values are ignored.
    fn parse_env(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "grep" => Some(Self::Grep),
            "heading" => Some(Self::Heading),
            "compact" => Some(Self::Compact),
            "json" => Some(Self::Json),
            "jsonl" => Some(Self::Jsonl),
            _ => None,
        }
    }

    fn is_json(self) -> bool {
        matches!(self, Self::Json | Self::Jsonl)
    }
}

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
    /// Resolved output layout (grep / heading / compact / json / jsonl).
    format: OutputFormat,
    /// `--agent-aggressive` took effect (no explicit `--format` overrode it):
    /// cut content longer than 200 characters with a `…`.
    aggressive: bool,
    /// `--agent-stats`: print latency, counts, output bytes and a token
    /// estimate to stderr instead of the plain timing line.
    agent_stats: bool,
    /// Output caps (`--max-results` etc.), applied deterministically.
    limits: Limits,
    /// Whether the search resolves through the persistent index (for the
    /// JSON envelope's `indexed` field).
    indexed: bool,
    /// Refresh the index before searching when the tree has moved past it.
    /// `--no-auto-update` clears it; the per-index `[search] auto_update`
    /// config can also turn it off.
    auto_update: bool,
    /// The pattern exactly as the user typed it (JSON envelope `query`).
    query: String,
    /// `--trim`: strip leading indentation from emitted content (lossy).
    trim: bool,
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
    /// Use persistent index for searching (path to .fgr dir). Built on first
    /// use if missing. Without this flag fast-grep looks for a `.fgr` index in
    /// the search path and its parents and uses it when one is there (never
    /// building one), so an indexed repository stays indexed for every caller.
    /// Env: FGR_INDEX (same meaning as the flag).
    #[arg(long = "index", value_name = "PATH", global = true)]
    pub index_path: Option<PathBuf>,

    /// Ignore any index and scan the tree directly. Overrides `--index` and
    /// `FGR_INDEX`, and turns off the `.fgr` auto-discovery.
    #[arg(long = "no-index", global = true)]
    pub no_index: bool,

    /// Don't refresh a stale index before searching it. Results then come from
    /// the index as it stands, which is faster but can miss edits made since it
    /// was built. Per-index default lives in `<index>/config.toml`
    /// (`[search] auto_update`).
    #[arg(long = "no-auto-update", global = true)]
    pub no_auto_update: bool,

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

    /// Output format: `grep` (flat `path:line:content`, piped default), `heading`
    /// (grouped under a file heading, TTY default), `compact` (grouped +
    /// relative paths — fewest tokens for LLM/agent consumers), `json` (one
    /// document) or `jsonl` (one object per match). Overrides --agent and
    /// --heading/--no-heading. Env: FGR_FORMAT.
    #[arg(long = "format", value_name = "FORMAT", value_enum, global = true)]
    pub format: Option<OutputFormat>,

    /// Agent mode: compact output — path once per file, then `line: text`,
    /// paths relative to the search root. Lossless. Same as `--format compact`.
    #[arg(long = "agent", global = true)]
    pub agent: bool,

    /// Agent mode plus long-line trimming: content longer than 200 characters
    /// is cut and suffixed with `…` (UTF-8 boundary safe). Lossy.
    #[arg(long = "agent-aggressive", global = true)]
    pub agent_aggressive: bool,

    /// Print search latency, match/file counts, output bytes and a token
    /// estimate (~4 bytes/token) to stderr.
    #[arg(long = "agent-stats", global = true)]
    pub agent_stats: bool,

    /// Cap the total number of matches emitted (the first N by path, then
    /// line). A truncation notice goes to stderr (or the `truncated` field
    /// for JSON/JSONL).
    #[arg(long = "max-results", value_name = "N", global = true)]
    pub max_results: Option<usize>,

    /// Cap the number of matches emitted per file.
    #[arg(long = "max-results-per-file", value_name = "N", global = true)]
    pub max_results_per_file: Option<usize>,

    /// Cap the number of files in the output.
    #[arg(long = "max-files", value_name = "N", global = true)]
    pub max_files: Option<usize>,

    /// Cap the output size in bytes. Never splits a line or a UTF-8 sequence.
    #[arg(long = "max-output-bytes", value_name = "N", global = true)]
    pub max_output_bytes: Option<usize>,

    /// Strip leading indentation from each match's content. Lossy (discards
    /// indentation structure) but trims a few % more tokens — pairs with
    /// `--format compact` for the leanest LLM/agent output.
    #[arg(long = "trim", global = true)]
    pub trim: bool,

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
    /// Incrementally update an existing index. Index dir from the global
    /// `--index` flag (default `.fgr`).
    #[command(name = "update")]
    Update {
        dir: Option<PathBuf>,
        /// Skip the automatic rebaseline even if divergence crosses the
        /// configured threshold (see config.toml `[compaction]`).
        #[arg(long = "no-compact")]
        no_compact: bool,
    },
    /// Show index statistics. Index dir from the global `--index` flag
    /// (default `.fgr`).
    #[command(name = "stats")]
    Stats,
    /// Rebaseline the index: fold the delta + tombstones into the primary and
    /// densify. Index dir from the global `--index` flag (default `.fgr`).
    #[command(name = "compact")]
    Compact,
    /// Watch DIR for changes and keep index up-to-date
    #[command(name = "daemon")]
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    /// Print the integration guides (Claude Code, Codex, OpenCode, Aider, MCP)
    #[command(name = "integrations")]
    Integrations,
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

/// Entry point. Returns whether the search matched anything (`true` for
/// subcommands, which have no notion of a match); `main` maps that to the
/// grep-compatible exit status (0 matched / 1 no match / 2 error).
pub fn run() -> Result<bool> {
    enable_ansi_on_windows();

    let cli = Cli::parse();

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
        format: resolve_format(&cli),
        aggressive: cli.agent_aggressive && cli.format.is_none(),
        agent_stats: cli.agent_stats,
        limits: Limits {
            max_results: cli.max_results,
            max_results_per_file: cli.max_results_per_file,
            max_files: cli.max_files,
            max_output_bytes: cli.max_output_bytes,
        },
        indexed: false, // resolved below, once we know which index answers
        auto_update: !cli.no_auto_update,
        query: String::new(), // populated below from the raw pattern
        trim: cli.trim,
        context,
        pattern: None, // populated below once the effective pattern is built
    };

    if let Some(cmd) = cli.command {
        run_subcommand(
            cmd,
            cli.index_path.clone(),
            opts.no_ignore,
            opts.hidden,
            &opts.file_type,
            opts.ignore_case,
        )?;
        return Ok(true);
    }

    let pattern = match cli.pattern.as_ref() {
        Some(p) => p.clone(),
        None => {
            eprintln!("Usage: fgr [OPTIONS] PATTERN [PATH]");
            eprintln!("Try 'fgr --help' for more information.");
            std::process::exit(2);
        }
    };

    // Aggregate modes produce per-file counts / file lists, not match records,
    // so a structured format has nothing to carry: refuse rather than emit
    // something that only looks like JSON.
    if (cli.count || cli.files_only) && opts.format.is_json() {
        eprintln!(
            "fgr: --count / --files-with-matches cannot be combined with --format json or jsonl"
        );
        std::process::exit(2);
    }

    let dir = cli.path.clone().unwrap_or_else(|| PathBuf::from("."));

    // Which index answers this search: `--index` / `FGR_INDEX` when given,
    // otherwise a `.fgr` discovered in the search path or one of its parents.
    let index = resolve_index(&cli, &dir);

    let mut effective = if cli.fixed_strings {
        regex::escape(&pattern)
    } else {
        pattern.clone()
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
    opts.query = pattern;
    opts.indexed = index.is_some() && !cli.invert_match;

    // Invert-match can't use the index — the trigram index locates *matches*,
    // so a "lines that don't match" query can't be answered from it; it always
    // routes through the direct-scan path.
    //
    // Case-insensitive search CAN use the index when a case-insensitive
    // companion (`fgr index -i`) is present: `search_timed` resolves `(?i)`
    // against the folded store, and transparently falls back to scanning all
    // live docs when no CI index exists. Routing it through the indexed path
    // also lets a first `-i` search auto-build the CI index.
    let found = match index {
        // Invert-match never uses the index, discovered or not.
        Some(idx) if !cli.invert_match => {
            let search_path = idx.enter_root(&dir)?;
            run_indexed_search(&effective, &idx.path, search_path.as_path(), &opts)?
        }
        _ => run_direct_search(&effective, &dir, &opts)?,
    };

    Ok(found)
}

/// Resolve the output format: explicit `--format` wins, then
/// `--agent-aggressive` / `--agent` (both compact), then the legacy
/// `--heading`/`--no-heading` booleans, then the `FGR_FORMAT` env var, then a
/// TTY default (heading in a terminal, grep when piped — so `fgr | script`
/// stays drop-in grep-compatible).
fn resolve_format(cli: &Cli) -> OutputFormat {
    if let Some(f) = cli.format {
        return f;
    }
    if cli.agent_aggressive || cli.agent {
        return OutputFormat::Compact;
    }
    if cli.no_heading {
        return OutputFormat::Grep;
    }
    if cli.heading {
        return OutputFormat::Heading;
    }
    if let Ok(v) = std::env::var("FGR_FORMAT") {
        if let Some(f) = OutputFormat::parse_env(&v) {
            return f;
        }
    }
    if std::io::stdout().is_terminal() {
        OutputFormat::Heading
    } else {
        OutputFormat::Grep
    }
}

/// Whether to emit ANSI colour escapes. Strictly TTY-driven so a forced
/// `--heading` while piped (`fgr --heading | less -R` excepted) doesn't
/// leak raw escape sequences into a non-terminal sink. Heading and colour
/// are independent: you can group without colour and colour without group.
fn use_color() -> bool {
    std::io::stdout().is_terminal()
}

/// Counts the bytes written through it, so `--agent-stats` can report output
/// size while the renderer stays generic over its writer.
struct CountingWriter<W: Write> {
    inner: W,
    bytes: u64,
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// The search root as an absolute path for the JSON envelope. Windows'
/// canonical form carries a `\\?\` prefix that is noise for consumers.
fn absolute_display(root: &std::path::Path) -> String {
    let abs = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let s = abs.to_string_lossy();
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
}

/// Post-render stderr line(s): the plain timing summary — or, with
/// `--agent-stats`, a stats line in its place — plus, for text formats, the
/// truncation notice when an output cap cut something.
fn print_render_summary(
    opts: &SearchOpts,
    stats: &RenderStats,
    output_bytes: u64,
    load: Option<Duration>,
    search: Duration,
) {
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    if opts.agent_stats {
        // ~4 bytes/token heuristic (not any particular model's tokenizer).
        let est_tokens = output_bytes.div_ceil(4);
        match load {
            Some(l) => eprintln!(
                "fgr-stats: load_ms={:.1} search_ms={:.1} matches={} files={} output_bytes={} est_tokens={}",
                ms(l), ms(search), stats.matches, stats.files, output_bytes, est_tokens
            ),
            None => eprintln!(
                "fgr-stats: search_ms={:.1} matches={} files={} output_bytes={} est_tokens={}",
                ms(search), stats.matches, stats.files, output_bytes, est_tokens
            ),
        }
    } else if !opts.quiet {
        match load {
            Some(l) => eprintln!(
                "Load: {:.1}ms, Search: {:.1}ms, {} matches",
                ms(l),
                ms(search),
                stats.matches
            ),
            None => eprintln!("Searched in {:.2}ms, {} matches", ms(search), stats.matches),
        }
    }
    if !opts.format.is_json() {
        if let Some(t) = stats.truncated {
            // `N+`: a per-file cap stopped a file early, so the total is a
            // lower bound (we stopped looking rather than scan just to count).
            let plus = if t.exact { "" } else { "+" };
            eprintln!(
                "fgr: output truncated — {} of {}{} matches ({} of {} files) shown; raise --max-results / --max-files / --max-output-bytes to see more",
                t.shown_matches, t.total_matches, plus, t.shown_files, t.total_files
            );
        }
    }
}

/// Returns whether anything matched (for the exit status).
fn run_direct_search(pattern: &str, dir: &std::path::Path, opts: &SearchOpts) -> Result<bool> {
    // count/files-only/quiet bypass the render pipeline entirely — they
    // produce per-file aggregates (counts, file lists) or no output at
    // all, so context flags don't apply and the simpler Vec<Match> API
    // is what we want.
    // quiet only needs a yes/no answer — stop the walk at the first match
    // instead of opening and scanning the entire tree.
    if opts.quiet {
        let found = searcher::search_full_scan_any(
            dir,
            pattern,
            opts.no_ignore,
            opts.hidden,
            &opts.file_type,
            &opts.include,
            &opts.exclude,
            opts.invert,
        )?;
        return Ok(found);
    }

    // count / files-only produce per-file aggregates; they bypass the render
    // pipeline (context flags don't apply) and use the simpler Vec<Match> API.
    if opts.count || opts.files_only {
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
        eprintln!(
            "Searched in {:.2}ms, {} matches",
            elapsed.as_secs_f64() * 1000.0,
            matches.len()
        );
        return Ok(!matches.is_empty());
    }

    let render_opts = render_opts_for(opts, dir);
    let dispatch = dispatch_for(&render_opts);

    let start = Instant::now();
    let stdout = std::io::stdout();
    let output = Mutex::new(CountingWriter {
        inner: std::io::BufWriter::new(stdout),
        bytes: 0,
    });
    let stats = render::search_full_scan_render(
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
    let output_bytes = {
        let mut out = output.lock().unwrap();
        let _ = out.flush();
        out.bytes
    };
    let elapsed = start.elapsed();
    print_render_summary(opts, &stats, output_bytes, None, elapsed);
    Ok(matched(&stats))
}

/// A render found something if it emitted a match — or if a cap hid the ones
/// it found (`truncated` is only ever set when at least one match existed).
fn matched(stats: &RenderStats) -> bool {
    stats.matches > 0 || stats.truncated.is_some()
}

/// Directory name fast-grep looks for when no index was named explicitly.
const INDEX_DIR_NAME: &str = ".fgr";

/// The index a search resolved to, and where it lives relative to the process.
struct ResolvedIndex {
    /// Path to the index directory (the `.fgr`).
    path: PathBuf,
    /// The directory the index was discovered in, when that is not the current
    /// working directory. An index records its documents relative to the
    /// directory it was built in, so a search run from a subdirectory only
    /// lines up once the process moves to that root — see [`Self::enter_root`].
    root: Option<PathBuf>,
}

impl ResolvedIndex {
    /// Move the process to the index root when the index was discovered above
    /// the current directory, and return the search path re-expressed relative
    /// to it (so `fgr foo` inside `src/` still only reports matches in `src/`).
    fn enter_root(&self, search_path: &Path) -> Result<PathBuf> {
        let Some(root) = self.root.as_deref() else {
            return Ok(search_path.to_path_buf());
        };
        let abs = std::fs::canonicalize(search_path).unwrap_or_else(|_| search_path.to_path_buf());
        // `root` is an ancestor of the search path by construction; if that
        // ever fails to hold, stay put rather than search the wrong tree.
        let Ok(rel) = abs.strip_prefix(root) else {
            return Ok(search_path.to_path_buf());
        };
        let rel = if rel.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            rel.to_path_buf()
        };
        std::env::set_current_dir(root)?;
        Ok(rel)
    }
}

/// Pick the index for this search: `--index`, then `FGR_INDEX`, then a `.fgr`
/// discovered in the search path or one of its parents. `--no-index` skips all
/// three. An explicitly named index is built on first use (see
/// [`run_indexed_search`]); a discovered one is only ever used as found, so a
/// plain `fgr pattern` in an unindexed tree stays a direct scan.
fn resolve_index(cli: &Cli, search_path: &Path) -> Option<ResolvedIndex> {
    if cli.no_index {
        return None;
    }
    if let Some(path) = cli.index_path.clone() {
        return Some(ResolvedIndex { path, root: None });
    }
    if let Some(path) = index_path_from_env() {
        return Some(ResolvedIndex { path, root: None });
    }
    discover_index(search_path)
}

/// `FGR_INDEX`, treating an empty value as unset (so `FGR_INDEX= fgr ...`
/// disables an inherited setting without unsetting the variable).
fn index_path_from_env() -> Option<PathBuf> {
    let raw = std::env::var_os("FGR_INDEX")?;
    if raw.is_empty() {
        return None;
    }
    Some(PathBuf::from(raw))
}

/// Walk up from the search path looking for a usable `.fgr`. The nearest one
/// wins, so a nested project's own index beats its parent's.
fn discover_index(search_path: &Path) -> Option<ResolvedIndex> {
    let start = std::fs::canonicalize(search_path).ok()?;
    // A file argument is searched through its directory's index.
    let start = if start.is_dir() {
        start
    } else {
        start.parent()?.to_path_buf()
    };
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|d| std::fs::canonicalize(d).ok());

    for dir in start.ancestors() {
        let candidate = dir.join(INDEX_DIR_NAME);
        if !persist::index_exists(&candidate) {
            continue;
        }
        // Present but written by an older format: rebuilding it behind the
        // user's back would be a surprise for a search that never asked for an
        // index, so say so once and scan directly.
        if !persist::is_current(&candidate) {
            eprintln!(
                "fgr: index at {} was written by an older format — scanning directly (run `fgr index {}` to rebuild)",
                candidate.display(),
                dir.display()
            );
            return None;
        }
        let root = (Some(dir) != cwd.as_deref()).then(|| dir.to_path_buf());
        return Some(ResolvedIndex {
            path: candidate,
            root,
        });
    }
    None
}

/// Whether the root recorded in an index still names the tree this process
/// would walk. An absolute root is cwd-independent and always trustworthy; a
/// relative one (`.`, `sub/dir`) only means what it meant in the directory the
/// index was built in, and the conventional `<root>/.fgr` layout is what lets
/// us check that we are still there. Getting this wrong is not a missed
/// refresh but a shredded index — an update run from the wrong directory walks
/// that directory and indexes it.
fn root_is_trustworthy(idx_path: &Path, root: &Path) -> bool {
    if root.is_absolute() {
        return true;
    }
    let parent = match idx_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        // A bare `.fgr` lives in the current directory.
        _ => Path::new("."),
    };
    match (std::fs::canonicalize(root), std::fs::canonicalize(parent)) {
        (Ok(resolved_root), Ok(resolved_parent)) => resolved_root == resolved_parent,
        _ => false,
    }
}

/// Bring `idx` up to date with the tree when it has drifted, returning the
/// reloaded index (or the original when it was already current). Mirrors what
/// `fgr update` does — incremental update plus the config's auto-rebaseline —
/// under the same lock, so a search, a daemon and an explicit update can race
/// without corrupting the index.
fn refresh_if_stale(
    idx: persist::PersistentIndex,
    idx_path: &Path,
    opts: &SearchOpts,
) -> Result<persist::PersistentIndex> {
    if !idx.is_stale() {
        return Ok(idx);
    }
    let root = PathBuf::from(&idx.meta.root_dir);
    if !root_is_trustworthy(idx_path, &root) {
        if !opts.quiet && !opts.format.is_json() {
            eprintln!(
                "fgr: index at {} is stale but its root `{}` doesn't resolve from here — \
                 searching it as-is (run `fgr update` from that directory to refresh)",
                idx_path.display(),
                root.display()
            );
        }
        return Ok(idx);
    }
    // The index is mmap'd: drop it before the update swaps the slot under us.
    drop(idx);

    let (_lock, waited) = persist::acquire_index_lock(idx_path)?;
    if waited {
        // Someone else was already updating; their work may be exactly ours.
        let reloaded = persist::load(idx_path)?;
        if !reloaded.is_stale() {
            persist::release_index_lock(idx_path);
            return Ok(reloaded);
        }
        drop(reloaded);
    }
    let refreshed = persist::update_incremental(idx_path, &root, false).and_then(|stats| {
        let compaction = persist::maybe_auto_compact(idx_path, &stats, false)?;
        Ok((stats, compaction))
    });
    persist::release_index_lock(idx_path);
    let (stats, _compaction) = refreshed?;

    if !opts.quiet && !opts.format.is_json() {
        eprintln!(
            "Index refreshed: +{} added, {} modified, {} deleted in {}ms",
            stats.added, stats.modified, stats.deleted, stats.duration_ms
        );
    }
    persist::load(idx_path)
}

fn run_indexed_search(
    pattern: &str,
    idx_path: &std::path::Path,
    search_path: &std::path::Path,
    opts: &SearchOpts,
) -> Result<bool> {
    // Auto-build the index on first use. We detect "no index" by the absence of
    // meta.json (the same probe persist::load uses internally). The build root
    // is the search PATH the user passed — this matches the natural intent
    // "give me a fast search over this directory."
    if !persist::is_current(idx_path) {
        let reason = if persist::index_exists(idx_path) {
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

    // If a daemon is managing this index, ensure it's up-to-date before
    // searching — and remember that it did, so the refresh below stays out of
    // the way of the process that owns the updates.
    #[cfg(feature = "daemon")]
    let daemon_flushed = {
        let running = crate::daemon::is_daemon_running(idx_path);
        if running {
            if let Ok(status) = crate::daemon::send_command(idx_path, "status") {
                if status == "dirty" {
                    let _ = crate::daemon::send_command(idx_path, "flush");
                }
            }
        }
        running
    };
    #[cfg(not(feature = "daemon"))]
    let daemon_flushed = false;

    let start = Instant::now();
    let mut idx = persist::load(idx_path)?;

    // No daemon keeping this index honest: fold the edits made since it was
    // built in ourselves, so the answer describes the tree as it is now rather
    // than the snapshot the index froze. The cheap mtime probe runs on every
    // search; the update itself only when it actually diverged.
    if opts.auto_update && !daemon_flushed && crate::config::load(idx_path).search.auto_update {
        idx = refresh_if_stale(idx, idx_path, opts)?;
    }

    // Resolve path filter: only return results under search_path. Doc paths are
    // stored exactly as the build walk produced them — `root_dir` joined with
    // the tree-relative path — so the filter has to be expressed in that same
    // space or it matches nothing (a `.`-rooted index stores `./src/x`, which
    // no `src/…` prefix is a prefix of).
    let root_dir = PathBuf::from(&idx.meta.root_dir);
    let path_filter = if search_path == root_dir || search_path == Path::new(".") {
        None
    } else if search_path.is_absolute() {
        Some(search_path.to_path_buf())
    } else if root_dir.is_absolute() || root_dir == Path::new(".") {
        // Absolute root: docs carry it as a prefix. `.` root: docs carry `./`.
        Some(root_dir.join(search_path))
    } else {
        // Relative root other than `.`: docs and the search path are both
        // expressed from the current directory already.
        Some(search_path.to_path_buf())
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
        return Ok(n > 0);
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
        return Ok(!matches.is_empty());
    }

    let render_opts = render_opts_for(opts, search_path);
    let dispatch = dispatch_for(&render_opts);

    let stdout = std::io::stdout();
    let output = Mutex::new(CountingWriter {
        inner: std::io::BufWriter::new(stdout),
        bytes: 0,
    });
    let (stats, _) = render::search_persistent_render(
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
    let output_bytes = {
        let mut out = output.lock().unwrap();
        let _ = out.flush();
        out.bytes
    };
    let search_time = start.elapsed();
    print_render_summary(opts, &stats, output_bytes, Some(load_time), search_time);
    Ok(matched(&stats))
}

/// Build a `RenderOpts` from the resolved output format and the search root.
/// `grep` → flat; `heading` → grouped; `compact` → grouped + paths relative to
/// `root`. Colour stays strictly TTY-driven so a piped format never leaks escapes.
fn render_opts_for(opts: &SearchOpts, root: &std::path::Path) -> RenderOpts {
    // JSON kinds use root-relative paths (like compact) and never headings.
    let (heading, relative, kind) = match opts.format {
        OutputFormat::Grep => (false, false, RenderKind::Text),
        OutputFormat::Heading => (true, false, RenderKind::Text),
        OutputFormat::Compact => (true, true, RenderKind::Text),
        OutputFormat::Json => (false, true, RenderKind::Json),
        OutputFormat::Jsonl => (false, true, RenderKind::Jsonl),
    };
    let envelope = (kind == RenderKind::Json).then(|| JsonEnvelope {
        query: opts.query.clone(),
        root: absolute_display(root),
        indexed: opts.indexed,
        started: Instant::now(),
    });
    RenderOpts {
        kind,
        max_line_chars: opts.aggressive.then_some(200),
        limits: opts.limits,
        envelope,
        heading,
        // Never leak ANSI escapes into a JSON document.
        color: use_color() && kind == RenderKind::Text,
        invert: opts.invert,
        only_matching: opts.only_matching,
        pattern: opts.pattern.clone(),
        rel_base: relative.then(|| root.to_path_buf()),
        trim: opts.trim,
    }
}

/// Streaming dispatch is only safe when the output needs neither a stable
/// file order nor buffering: heading and colour want sorted output, the
/// output caps are defined as "first N by path", and JSON/JSONL are written
/// as one deterministic document / sequence.
fn dispatch_for(render_opts: &RenderOpts) -> Dispatch {
    if render_opts.heading
        || render_opts.color
        || render_opts.limits.any()
        || render_opts.kind != RenderKind::Text
    {
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
    index_path: Option<PathBuf>,
    no_ignore: bool,
    hidden: bool,
    type_filter: &[String],
    case_insensitive: bool,
) -> Result<()> {
    // `update` and `stats` take the index dir from the global `--index` flag,
    // defaulting to `.fgr` when it is omitted.
    let idx_arg = || {
        index_path
            .clone()
            .or_else(index_path_from_env)
            .unwrap_or_else(|| PathBuf::from(INDEX_DIR_NAME))
    };
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
        Commands::Update { dir, no_compact } => {
            let idx_path = idx_arg();
            let root = if let Some(d) = dir {
                d
            } else {
                let probe = persist::load(&idx_path)?;
                let root = PathBuf::from(&probe.meta.root_dir);
                // Refuse rather than re-index whatever tree we happen to be
                // standing in: the update walks `root`, so a relative root read
                // from the wrong directory would replace the index's contents
                // with this directory's.
                if !root_is_trustworthy(&idx_path, &root) {
                    anyhow::bail!(
                        "index at {} records a relative root `{}` that doesn't resolve from \
                         here — cd to the directory it was built in, or pass the directory \
                         explicitly (`fgr update DIR --index {}`)",
                        idx_path.display(),
                        root.display(),
                        idx_path.display()
                    );
                }
                root
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

            // Auto-rebaseline while we still hold the lock, if the config's
            // thresholds say divergence is high enough. Folds in-place under the
            // held lock; the cost is paid by this updater, never by a search.
            // `--no-compact` opts out per run.
            let compaction = if no_compact {
                None
            } else {
                persist::maybe_auto_compact(&idx_path, &stats, false)?
            };
            persist::release_index_lock(&idx_path);

            if stats.added == 0 && stats.modified == 0 && stats.deleted == 0 {
                eprintln!("Index is up to date ({} files)", stats.unchanged);
            } else {
                eprintln!(
                    "Updated index: +{} added, {} modified, {} deleted (unchanged: {}) in {}ms",
                    stats.added, stats.modified, stats.deleted, stats.unchanged, stats.duration_ms
                );
            }
            if let Some(out) = compaction {
                if let Some(s) = out.stats {
                    eprintln!(
                        "Auto-compacted: {} live docs, {} dropped, {} trigrams",
                        s.live_docs, s.dropped_docs, s.num_ngrams
                    );
                }
            }
        }
        Commands::Stats => {
            let index_path = idx_arg();
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
                // Divergence from the frozen baseline + whether the config's
                // thresholds say a rebaseline is due (see `fgr compact`).
                let delta_docs = idx.delta_doc_ids.len();
                let tombstones = idx.deleted_docs.len();
                let cfg = crate::config::load(&index_path);
                println!("  Delta docs:   {}", delta_docs);
                println!("  Tombstones:   {}", tombstones);
                println!(
                    "  Compaction due: {}",
                    cfg.compaction
                        .should_compact(idx.main_num_docs, delta_docs, tombstones)
                );
            } else {
                let admission = crate::config::Admission::from_config(
                    &crate::config::IndexConfig::default(),
                    &index_path,
                );
                let idx = index::SparseIndex::build_from_directory(
                    &index_path,
                    no_ignore,
                    type_filter,
                    false,
                    false,
                    &admission,
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
        Commands::Compact => {
            let idx_path = idx_arg();
            let start = Instant::now();
            let outcome = persist::compact(&idx_path, false)?;
            if outcome.compacted {
                let s = outcome.stats.expect("stats present when compacted");
                eprintln!(
                    "Compacted index: {} live docs, {} dropped, {} trigrams in {:.2}s",
                    s.live_docs,
                    s.dropped_docs,
                    s.num_ngrams,
                    start.elapsed().as_secs_f64()
                );
            } else {
                eprintln!("Index already compact (no delta or tombstones to fold)");
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
        Commands::Integrations => {
            // The guides ship inside the binary so `fgr integrations` works
            // wherever `fgr` is installed, without the source tree.
            let guides: [(&str, &str); 5] = [
                (
                    "Claude Code",
                    include_str!("../integrations/claude-code/README.md"),
                ),
                ("Codex", include_str!("../integrations/codex/README.md")),
                (
                    "OpenCode",
                    include_str!("../integrations/opencode/README.md"),
                ),
                ("Aider", include_str!("../integrations/aider/README.md")),
                ("MCP", include_str!("../integrations/mcp/README.md")),
            ];
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            for (name, body) in guides {
                let _ = writeln!(out, "==== {} ====\n", name);
                let _ = out.write_all(body.as_bytes());
                let _ = writeln!(out);
            }
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
