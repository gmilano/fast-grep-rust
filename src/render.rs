//! Per-file render pipeline for fgr.
//!
//! `searcher.rs` finds where matches live in a file (line numbers, byte
//! offsets); this module turns those positions into the bytes the user sees,
//! including before/after context, ripgrep-style chunk separators, ANSI
//! colour, and the heading-vs-flat layout choice.
//!
//! The unit of work is a single file: open mmap, iterate lines, capture
//! match + context windows, emit formatted bytes into an `out_buf`. Nothing
//! about chunks crosses thread boundaries — only the formatted `Vec<u8>`
//! does. That keeps lifetimes trivial (no `MatchChunk<'a>` parameters
//! propagated through the API) while preserving zero-copy at the byte
//! level: context lines are written straight from the mmap slice into
//! the output buffer, only match lines pass through the highlight buffer.
//!
//! See `MEMORY.md`/conversation `feat/search-improvements` for the design
//! discussion that landed on this shape.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;
use memmap2::Mmap;

use crate::searcher::{
    is_binary, is_known_text_ext, needs_line_by_line, num_cpus, strip_trailing_cr, Matcher,
};

// ANSI escape codes for the TTY rendering path. Same set the CLI used to
// own; centralised here now that all rendering goes through this module.
pub(crate) const C_RESET: &str = "\x1b[0m";
pub(crate) const C_BOLD: &str = "\x1b[1m";
pub(crate) const C_PATH: &str = "\x1b[35m"; // magenta — file paths
pub(crate) const C_LINENO: &str = "\x1b[32m"; // green — line numbers
pub(crate) const C_MATCH: &str = "\x1b[1;31m"; // bold red — matched substring

/// Marks whether a line in the output stream is the actual matching line
/// or part of the surrounding context window. Determines the delimiter
/// (`:` vs `-`) and whether the matched substring gets highlighted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LineKind {
    Match,
    Context,
}

/// Number of context lines to capture around a match. A zero `before` and
/// zero `after` (the default) collapses each match to a single-line chunk
/// and emits no `--` separators — matches the pre-context output exactly.
#[derive(Debug, Clone, Copy, Default)]
pub struct ContextOpts {
    pub before: usize,
    pub after: usize,
}

impl ContextOpts {
    /// Resolve grep-style flags into a single set of context lines.
    /// `--before-context` and `--after-context` win over `--context` for
    /// their direction; `--context` is the symmetric fallback.
    pub fn resolve(context: Option<usize>, before: Option<usize>, after: Option<usize>) -> Self {
        Self {
            before: before.or(context).unwrap_or(0),
            after: after.or(context).unwrap_or(0),
        }
    }

    #[inline]
    pub fn is_zero(&self) -> bool {
        self.before == 0 && self.after == 0
    }
}

/// Find the byte offset of the start of the line that sits `n` lines
/// *before* the line containing `from_offset`. Returns `0` if the start
/// of file is reached first (fewer than `n` preceding lines exist).
///
/// `from_offset` may point anywhere on the reference line; the function
/// first locates the start of that reference line and then walks back.
/// Uses `memchr::memrchr_iter` for SIMD-accelerated reverse newline scan.
///
/// NOTE: not currently consumed by `render_file_into` (which iterates the
/// full file line-by-line). Kept — together with `forward_n_lines` and
/// their tests — for the planned line-level indexed optimisation: when
/// the index gives us candidate byte offsets, jumping to each and
/// scanning ±N lines is cheaper than walking the full file. See the
/// TODO in `search_persistent_render`.
#[allow(dead_code)]
pub(crate) fn back_n_lines(buf: &[u8], from_offset: usize, n: usize) -> usize {
    let from = from_offset.min(buf.len());
    // Walk back at most n+1 newlines: the first one (if any) marks the
    // start of the *current* line, then each additional one marks an
    // earlier line boundary. We want the n-th one back from current.
    let mut iter = memchr::memrchr_iter(b'\n', &buf[..from]);
    let mut found = 0usize;
    let mut last_pos: Option<usize> = None;
    // Need n+1 newlines: first locates current line start, n more for the
    // n preceding line starts. The n-th one back is the answer (+1 to
    // skip the newline byte itself).
    for pos in iter.by_ref() {
        last_pos = Some(pos);
        found += 1;
        if found == n + 1 {
            return pos + 1;
        }
    }
    // Reached SOF without finding n+1 newlines.
    // last_pos is the very first newline in the buf (or None if no newlines).
    // If we found at least one newline but ran out before n+1, we've reached
    // the actual start of file — return 0.
    let _ = last_pos;
    let _ = iter;
    0
}

/// Find the byte offset (exclusive) of the end of the line that sits `n`
/// lines *after* the line ending at `from_end_offset` (which should be
/// the position of the `\n` terminator of the reference line, or
/// `buf.len()` if the reference line was the last with no trailing
/// newline). Returns `buf.len()` if EOF is reached.
///
/// "End offset" excludes the terminating `\n` so callers can slice
/// `&buf[start..end]` and get the line content directly.
///
/// See the note on `back_n_lines` — kept for the planned line-level
/// indexed optimisation.
#[allow(dead_code)]
pub(crate) fn forward_n_lines(buf: &[u8], from_end_offset: usize, n: usize) -> usize {
    if n == 0 {
        return from_end_offset.min(buf.len());
    }
    let start = (from_end_offset + 1).min(buf.len()); // skip the terminator
    let mut found = 0usize;
    for pos in memchr::memchr_iter(b'\n', &buf[start..]) {
        found += 1;
        if found == n {
            return start + pos;
        }
    }
    buf.len()
}

/// How to dispatch each per-file output buffer to the shared writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    /// Write each file's bytes to the sink as soon as the worker finishes
    /// it. Output order is "as completed" — random under parallel walk —
    /// matching today's piped-fgr behaviour. Used when the heading
    /// auto-detect picks flat (`fgr ... | grep ...`).
    Streaming,
    /// Collect every file's bytes, sort by path, then write them all out.
    /// Used for heading mode and TTY rendering so the user sees a stable,
    /// alphabetised file order.
    Sorted,
}

/// Per-thread workspace: reusable buffers for the worker closure. Avoids
/// allocating a fresh `Vec<u8>` per file when the walk processes many.
struct WorkerBufs {
    /// Output bytes formatted by `render_file_into` for the current file.
    /// `mem::take`'d into the dispatch sink (or written and cleared) at
    /// the end of each file.
    out: Vec<u8>,
    /// Read fallback for files small enough that mmap overhead dominates;
    /// reused across files to avoid per-file allocation.
    read_buf: Vec<u8>,
}

impl WorkerBufs {
    fn new() -> Self {
        Self {
            out: Vec::with_capacity(64 * 1024),
            read_buf: Vec::with_capacity(64 * 1024),
        }
    }
}

/// Open a file for reading and either mmap it (large) or read into the
/// supplied buffer (small). The split mirrors what `search_full_scan` did
/// before this refactor — mmap setup costs ~1µs, so files smaller than
/// a few hundred KB are faster to plain-read.
///
/// Returns `None` for empty / unopenable files (skipped by the caller).
fn read_or_mmap<'a>(
    path: &Path,
    flen: u64,
    read_buf: &'a mut Vec<u8>,
    holder: &'a mut Option<Mmap>,
) -> Option<&'a [u8]> {
    if flen == 0 {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    if flen > 256 * 1024 {
        *holder = unsafe { Mmap::map(&file).ok() };
        holder.as_deref()
    } else {
        use std::io::Read;
        read_buf.clear();
        let mut f = file;
        f.read_to_end(read_buf).ok()?;
        Some(&read_buf[..])
    }
}

/// Hand off a per-file output buffer to the shared writer (or the
/// collector for sorted dispatch). Empty buffers are skipped so files
/// without matches generate no output.
fn dispatch_file<W: Write>(
    path: &Path,
    out_buf: &mut Vec<u8>,
    dispatch: Dispatch,
    streaming_sink: &Mutex<W>,
    collector: &Mutex<Vec<(PathBuf, Vec<u8>)>>,
) {
    if out_buf.is_empty() {
        return;
    }
    match dispatch {
        Dispatch::Streaming => {
            let mut sink = streaming_sink.lock().unwrap();
            let _ = sink.write_all(out_buf);
            out_buf.clear();
        }
        Dispatch::Sorted => {
            let bytes = std::mem::take(out_buf);
            collector.lock().unwrap().push((path.to_path_buf(), bytes));
        }
    }
}

/// Render path for full-text scan (no index). Walks `root` in parallel
/// and pipes every matching file through `render_file_into`. Returns the
/// total match-line count for the timing line in `cli.rs`.
pub fn search_full_scan_render<W: Write + Send>(
    root: &Path,
    pattern: &str,
    no_ignore: bool,
    hidden: bool,
    type_filter: Option<&str>,
    ctx: &ContextOpts,
    render: &RenderOpts,
    dispatch: Dispatch,
    output: &Mutex<W>,
) -> Result<usize> {
    let matcher = Matcher::new(pattern)?;
    let total_count = std::sync::atomic::AtomicUsize::new(0);
    let collector: Mutex<Vec<(PathBuf, Vec<u8>)>> = Mutex::new(Vec::new());

    let walker = ignore::WalkBuilder::new(root)
        .git_ignore(!no_ignore)
        .hidden(!hidden)
        .threads(num_cpus())
        .build_parallel();

    let type_filter_owned = type_filter.map(|s| s.to_string());

    walker.run(|| {
        let matcher = &matcher;
        let total_count = &total_count;
        let collector = &collector;
        let type_filter = type_filter_owned.as_deref();
        let mut bufs = WorkerBufs::new();

        Box::new(move |entry| {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => return ignore::WalkState::Continue,
            };
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return ignore::WalkState::Continue;
            }
            let path = entry.path();

            if let Some(ext_filter) = type_filter {
                match path.extension().and_then(|e| e.to_str()) {
                    Some(ext) if ext == ext_filter => {}
                    _ => return ignore::WalkState::Continue,
                }
            }

            let flen = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let mut holder: Option<Mmap> = None;
            let buf = match read_or_mmap(path, flen, &mut bufs.read_buf, &mut holder) {
                Some(b) => b,
                None => return ignore::WalkState::Continue,
            };

            bufs.out.clear();
            let count = render_file_into(path, buf, matcher, pattern, ctx, render, &mut bufs.out);

            if count > 0 {
                total_count.fetch_add(count, std::sync::atomic::Ordering::Relaxed);
                dispatch_file(path, &mut bufs.out, dispatch, output, collector);
            }

            ignore::WalkState::Continue
        })
    });

    if dispatch == Dispatch::Sorted {
        let mut entries = collector.into_inner().unwrap();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let mut sink = output.lock().unwrap();
        for (_, bytes) in &entries {
            let _ = sink.write_all(bytes);
        }
    }

    Ok(total_count.load(std::sync::atomic::Ordering::Relaxed))
}

/// Render path for the persistent-index search. Resolves candidate files
/// via the index (bitmap, line-level, or fallback) and renders each one
/// through `render_file_into`. The line-level optimisation that the
/// pre-render path used (jump to candidate offsets, skip the rest of the
/// file) is currently not preserved — see TODO below — so for very
/// low-density patterns the indexed path now reads the full content of
/// each candidate file. The bitmap-level filtering still excludes
/// non-candidate files, so the outer loop is still bounded by index
/// selectivity.
///
/// TODO: for `SearchResult::LineHits` with `ctx.is_zero()`, restoring the
/// jump-to-offset path would skip whole-file iteration. Worth doing when
/// the regression shows up in benchmarks; skipped for v1 to keep one
/// formatting code path.
pub fn search_persistent_render<W: Write + Send>(
    index: &crate::persist::PersistentIndex,
    pattern: &str,
    path_filter: Option<&Path>,
    hidden: bool,
    ctx: &ContextOpts,
    render: &RenderOpts,
    dispatch: Dispatch,
    output: &Mutex<W>,
) -> Result<(usize, crate::persist::SearchTiming)> {
    use crate::persist::SearchResult;

    let matcher = Matcher::new(pattern)?;
    let index_root = PathBuf::from(&index.meta.root_dir);
    let (result, mut timing) = index.search_timed(pattern);
    let t_verify = std::time::Instant::now();

    // Collect candidate file paths (index-internal references).
    let candidate_paths: Vec<&Path> = match result {
        SearchResult::LineHits(hits) => {
            let mut paths: Vec<&Path> = hits.iter().map(|h| h.path).collect();
            paths.sort();
            paths.dedup();
            paths
        }
        SearchResult::BitmapFiles(paths) | SearchResult::AllFiles(paths) => paths,
    };

    // Apply hidden + path filter (same logic as before — see is_hidden_path
    // doc for why the index-root prefix is stripped before checking).
    let candidate_paths: Vec<PathBuf> = candidate_paths
        .into_iter()
        .filter(|p| {
            (hidden || !crate::searcher::is_hidden_path(p, &index_root))
                && path_filter.map_or(true, |f| p.starts_with(f))
        })
        .map(|p| p.to_path_buf())
        .collect();

    let total_count = std::sync::atomic::AtomicUsize::new(0);
    let collector: Mutex<Vec<(PathBuf, Vec<u8>)>> = Mutex::new(Vec::new());

    use rayon::prelude::*;
    candidate_paths
        .par_iter()
        .for_each_init(WorkerBufs::new, |bufs, path| {
            let flen = match std::fs::metadata(path) {
                Ok(m) => m.len(),
                Err(_) => return,
            };
            let mut holder: Option<Mmap> = None;
            let buf = match read_or_mmap(path, flen, &mut bufs.read_buf, &mut holder) {
                Some(b) => b,
                None => return,
            };

            bufs.out.clear();
            let count = render_file_into(path, buf, &matcher, pattern, ctx, render, &mut bufs.out);

            if count > 0 {
                total_count.fetch_add(count, std::sync::atomic::Ordering::Relaxed);
                dispatch_file(path, &mut bufs.out, dispatch, output, &collector);
            }
        });

    if dispatch == Dispatch::Sorted {
        let mut entries = collector.into_inner().unwrap();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let mut sink = output.lock().unwrap();
        for (_, bytes) in &entries {
            let _ = sink.write_all(bytes);
        }
    }

    let count = total_count.load(std::sync::atomic::Ordering::Relaxed);
    timing.verify_ms = t_verify.elapsed().as_secs_f64() * 1000.0;
    timing.matches = count;
    // strategy: kept simple here; the pre-render code reported "line-level"
    // / "file-level" / "bitmap-only" — we can resurrect that if the bench
    // line in cli.rs misses it.
    Ok((count, timing))
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use std::path::PathBuf;

    fn opts(heading: bool) -> RenderOpts {
        RenderOpts {
            heading,
            color: false,
            pattern: None,
        }
    }

    /// Test helper: run render_file_into against raw bytes and return UTF-8.
    fn render(buf: &[u8], pattern: &str, ctx: ContextOpts, heading: bool) -> (usize, String) {
        let path = PathBuf::from("test.txt");
        let matcher = Matcher::new(pattern).unwrap();
        let mut out = Vec::new();
        let n = render_file_into(
            &path,
            buf,
            &matcher,
            pattern,
            &ctx,
            &opts(heading),
            &mut out,
        );
        (n, String::from_utf8_lossy(&out).into_owned())
    }

    // --- zero-context behaviour: must match historical fast-grep output ---

    #[test]
    fn zero_context_flat_one_match() {
        let (n, out) = render(
            b"alpha\nbeta\ngamma\n",
            "beta",
            ContextOpts::default(),
            false,
        );
        assert_eq!(n, 1);
        assert_eq!(out, "test.txt:2:beta\n");
    }

    #[test]
    fn zero_context_flat_multiple_matches_no_separator() {
        // Two consecutive matches should have NO -- separator in zero-context.
        let (n, out) = render(b"hit\nmiss\nhit\n", "hit", ContextOpts::default(), false);
        assert_eq!(n, 2);
        assert_eq!(out, "test.txt:1:hit\ntest.txt:3:hit\n");
    }

    #[test]
    fn zero_context_heading_emits_path_once() {
        let (n, out) = render(b"hit\nmiss\nhit\n", "hit", ContextOpts::default(), true);
        assert_eq!(n, 2);
        assert_eq!(out, "test.txt\n1:hit\n3:hit\n");
    }

    #[test]
    fn no_match_emits_nothing() {
        let (n, out) = render(b"alpha\nbeta\n", "zzz", ContextOpts::default(), false);
        assert_eq!(n, 0);
        assert_eq!(out, "");
    }

    // --- before/after context ---

    #[test]
    fn after_context_only() {
        let buf = b"a\nb\nMATCH\nc\nd\ne\n";
        let (n, out) = render(
            buf,
            "MATCH",
            ContextOpts {
                before: 0,
                after: 2,
            },
            false,
        );
        assert_eq!(n, 1);
        assert_eq!(out, "test.txt:3:MATCH\ntest.txt-4-c\ntest.txt-5-d\n");
    }

    #[test]
    fn before_context_only() {
        let buf = b"a\nb\nc\nMATCH\nd\n";
        let (n, out) = render(
            buf,
            "MATCH",
            ContextOpts {
                before: 2,
                after: 0,
            },
            false,
        );
        assert_eq!(n, 1);
        assert_eq!(out, "test.txt-2-b\ntest.txt-3-c\ntest.txt:4:MATCH\n");
    }

    #[test]
    fn match_at_line_one_clamps_before() {
        let buf = b"MATCH\nrest\n";
        let (n, out) = render(
            buf,
            "MATCH",
            ContextOpts {
                before: 5,
                after: 0,
            },
            false,
        );
        assert_eq!(n, 1);
        // No before-context lines because we're at SOF.
        assert_eq!(out, "test.txt:1:MATCH\n");
    }

    #[test]
    fn match_at_eof_clamps_after() {
        let buf = b"a\nMATCH"; // no trailing newline
        let (n, out) = render(
            buf,
            "MATCH",
            ContextOpts {
                before: 0,
                after: 5,
            },
            false,
        );
        assert_eq!(n, 1);
        // No after-context lines because we're at EOF.
        assert_eq!(out, "test.txt:2:MATCH\n");
    }

    // --- chunk merging vs separator ---

    #[test]
    fn nearby_matches_merge_into_one_chunk() {
        // Two matches, distance 2, with -A 1 -B 0 → after-context absorbs.
        let buf = b"a\nMATCH\nb\nMATCH\nc\n";
        //         line1  2     3   4     5
        let (n, out) = render(
            buf,
            "MATCH",
            ContextOpts {
                before: 0,
                after: 1,
            },
            false,
        );
        assert_eq!(n, 2);
        // Expected: m2, ctx3, m4, ctx5 — no `--` because m4 lands inside m2's after-window.
        let expected = "test.txt:2:MATCH\ntest.txt-3-b\ntest.txt:4:MATCH\ntest.txt-5-c\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn distant_matches_get_separator() {
        // Two matches, distance 8, with -C 1 → not adjacent, separator expected.
        let buf = b"a\nM\nb\nc\nd\ne\nf\ng\nM\nh\n";
        //         1  2 3 4 5 6 7 8 9 10
        let (n, out) = render(
            buf,
            "M",
            ContextOpts {
                before: 1,
                after: 1,
            },
            false,
        );
        assert_eq!(n, 2);
        let expected = "test.txt-1-a\ntest.txt:2:M\ntest.txt-3-b\n--\ntest.txt-8-g\ntest.txt:9:M\ntest.txt-10-h\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn line_that_is_match_and_context_renders_as_match() {
        // Match on line 2 (with -A 2) extends through line 4. Match also on line 4.
        // Line 4 must render as `:` (Match), not `-` (Context).
        let buf = b"a\nM\nb\nM\nc\n";
        let (n, out) = render(
            buf,
            "M",
            ContextOpts {
                before: 0,
                after: 2,
            },
            false,
        );
        assert_eq!(n, 2);
        // Expected: m2, ctx3, m4 (NOT ctx4!), ctx5.
        let expected = "test.txt:2:M\ntest.txt-3-b\ntest.txt:4:M\ntest.txt-5-c\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn heading_mode_with_context() {
        let buf = b"a\nM\nb\n";
        let (n, out) = render(
            buf,
            "M",
            ContextOpts {
                before: 1,
                after: 1,
            },
            true,
        );
        assert_eq!(n, 1);
        let expected = "test.txt\n1-a\n2:M\n3-b\n";
        assert_eq!(out, expected);
    }

    // --- ContextOpts::resolve flag precedence ---

    #[test]
    fn resolve_only_context() {
        let c = ContextOpts::resolve(Some(3), None, None);
        assert_eq!(c.before, 3);
        assert_eq!(c.after, 3);
    }

    #[test]
    fn resolve_only_before_or_after() {
        let c = ContextOpts::resolve(None, Some(2), None);
        assert_eq!(c.before, 2);
        assert_eq!(c.after, 0);
        let c = ContextOpts::resolve(None, None, Some(5));
        assert_eq!(c.before, 0);
        assert_eq!(c.after, 5);
    }

    #[test]
    fn resolve_before_after_override_context() {
        // -C 3 -A 5 -B 1 → before=1, after=5
        let c = ContextOpts::resolve(Some(3), Some(1), Some(5));
        assert_eq!(c.before, 1);
        assert_eq!(c.after, 5);
    }
}

#[cfg(test)]
mod scan_tests {
    use super::*;

    #[test]
    fn back_n_at_sof_returns_zero() {
        let buf = b"line1\nline2\nline3\n";
        // from anywhere in line1, asking for any back distance
        assert_eq!(back_n_lines(buf, 2, 3), 0);
        assert_eq!(back_n_lines(buf, 0, 5), 0);
    }

    #[test]
    fn back_n_walks_correct_count() {
        let buf = b"a\nbb\nccc\ndddd\neeeee\n";
        // offsets:    0  2   5    9    14
        // line1 = "a" (offset 0..1), line2 = "bb" (2..4), ...
        // From inside "eeeee" (offset 14..19), back 1 = start of "dddd" (9).
        assert_eq!(back_n_lines(buf, 16, 1), 9);
        // back 2 = start of "ccc" (5)
        assert_eq!(back_n_lines(buf, 16, 2), 5);
        // back 4 = start of "a" (0)
        assert_eq!(back_n_lines(buf, 16, 4), 0);
        // back 10 (more than available) = 0
        assert_eq!(back_n_lines(buf, 16, 10), 0);
    }

    #[test]
    fn back_n_zero_returns_current_line_start() {
        let buf = b"a\nbb\nccc\n";
        // offset 5 is inside "ccc" (5..8). Back 0 = start of ccc = 5.
        assert_eq!(back_n_lines(buf, 5, 0), 5);
        assert_eq!(back_n_lines(buf, 7, 0), 5);
    }

    #[test]
    fn forward_n_at_eof_returns_buf_len() {
        let buf = b"line1\nline2\nline3\n";
        // from end of line3 (offset 17 = position of trailing \n).
        // After the terminator there's nothing; asking for 1 line forward → EOF.
        assert_eq!(forward_n_lines(buf, 17, 1), buf.len());
        assert_eq!(forward_n_lines(buf, 17, 5), buf.len());
    }

    #[test]
    fn forward_n_walks_correct_count() {
        let buf = b"a\nbb\nccc\ndddd\neeeee\n";
        // offsets of \n:  1   4   8   13   19
        // From end of "a" (offset 1, the \n), forward 1 = end of "bb" = 4.
        assert_eq!(forward_n_lines(buf, 1, 1), 4);
        assert_eq!(forward_n_lines(buf, 1, 2), 8);
        assert_eq!(forward_n_lines(buf, 1, 4), 19);
        assert_eq!(forward_n_lines(buf, 1, 10), buf.len());
    }

    #[test]
    fn forward_n_zero_returns_input_offset() {
        let buf = b"a\nbb\n";
        assert_eq!(forward_n_lines(buf, 1, 0), 1);
    }

    #[test]
    fn no_trailing_newline_handled() {
        let buf = b"a\nbb\nlast"; // no trailing \n
                                  // Last line "last" runs from 5..9. From end of "bb" (offset 4),
                                  // forward 1 should be EOF since there's no terminator after "last".
        assert_eq!(forward_n_lines(buf, 4, 1), buf.len());
        // Back from inside "last" (offset 7): 1 line back = start of "bb" = 2.
        assert_eq!(back_n_lines(buf, 7, 1), 2);
    }

    #[test]
    fn empty_buf_returns_zero() {
        let buf = b"";
        assert_eq!(back_n_lines(buf, 0, 5), 0);
        assert_eq!(forward_n_lines(buf, 0, 5), 0);
    }
}

/// Knobs the renderer needs to format output. Ownership lives at the CLI
/// layer; the per-file pipeline reads it by reference.
#[derive(Debug, Clone)]
pub struct RenderOpts {
    /// Group matches under a file-name heading (path on its own line,
    /// lines indented). When false, emit `path:line:content` per match.
    pub heading: bool,
    /// Emit ANSI colour escapes around path, line number, and matched
    /// substring. Strictly TTY-driven at the call site — independent of
    /// `heading` so `fgr --heading | cat` doesn't leak escapes.
    pub color: bool,
    /// Effective regex pattern (with `(?i)` prefix etc. already applied)
    /// used for highlighting matched substrings inside match lines. The
    /// renderer compiles it once per file. `None` skips highlighting.
    pub pattern: Option<String>,
}

/// Process one file end-to-end: iterate lines, find matches, capture
/// before/after context windows, format output bytes into `out_buf`.
/// Returns the number of *match* lines (context lines are not counted —
/// matches what `--count` reports).
///
/// `out_buf` is the per-file output buffer. The caller is responsible for
/// dispatching it to stdout (immediate flush under a `Mutex` for the
/// streaming path, or accumulate-and-sort for the heading path).
///
/// Returns 0 (and writes nothing) if the file is empty or detected as
/// binary, matching the historical full-scan behaviour.
pub(crate) fn render_file_into(
    path: &Path,
    mmap: &[u8],
    matcher: &Matcher,
    pattern: &str,
    ctx: &ContextOpts,
    render: &RenderOpts,
    out_buf: &mut Vec<u8>,
) -> usize {
    if mmap.is_empty() {
        return 0;
    }
    if !is_known_text_ext(path) && is_binary(mmap) {
        return 0;
    }

    let lbl = needs_line_by_line(pattern);

    // Highlighting is only meaningful when colour is on. We use the bytes
    // regex here so we never leave the byte domain — the line content
    // never gets UTF-8 validated for the sake of rendering.
    let hl_re: Option<regex::bytes::Regex> = if render.color {
        render
            .pattern
            .as_deref()
            .and_then(|p| regex::bytes::Regex::new(p).ok())
    } else {
        None
    };

    let path_str = path.to_string_lossy();

    // State machine for chunk merging:
    //   prev_lines    — ring buffer of last `before` non-match lines, used
    //                   as the before-context when a match arrives
    //   after_remaining — when > 0, every following line (match or not) is
    //                   emitted as part of the current chunk; transitions
    //                   from match (resets to ctx.after) and decrements
    //                   each subsequent non-match line
    //   chunks_emitted — for `--` separator decision; only inserted when
    //                   ctx is non-zero (zero-context produces no separators,
    //                   matching ripgrep)
    //   header_emitted — heading mode emits the path header lazily on the
    //                   first match in the file
    let mut prev_lines: VecDeque<(u32, usize, usize)> = VecDeque::with_capacity(ctx.before.max(1));
    let mut after_remaining: usize = 0;
    // Last line number we wrote out (match or context). Drives the chunk-
    // merge decision: a new match within `before + 1` lines of the last
    // emitted line is considered adjacent and absorbs into the current
    // chunk without a `--` separator. `None` until the first emission.
    let mut last_emitted_line: Option<u32> = None;
    let mut header_emitted = false;
    let mut match_count: usize = 0;

    let mut line_no: u32 = 1;
    let mut pos: usize = 0;

    while pos < mmap.len() {
        let end = memchr::memchr(b'\n', &mmap[pos..])
            .map(|p| pos + p)
            .unwrap_or(mmap.len());
        let line_bytes = &mmap[pos..end];
        let display_bytes = strip_trailing_cr(line_bytes);
        // Match against the *raw* line bytes so `[[:space:]]` etc. behave
        // identically to the non-rendering search paths. Only display gets
        // the CR stripped.
        let is_match = matcher.has_match(line_bytes, lbl);

        if is_match {
            if !header_emitted && render.heading {
                emit_heading(out_buf, &path_str, render);
                header_emitted = true;
            }

            // Decide: merge into the previous chunk, or start a fresh one.
            // Two situations merge: (a) we're still inside an after-context
            // window (after_remaining > 0), or (b) the chunk closed but
            // this new match is close enough that its before-context
            // overlaps the gap left in the ring buffer — formally,
            // `line_no - last_emitted <= before + 1` means there's no
            // visible gap once we drain the ring buffer.
            let merge = after_remaining > 0
                || last_emitted_line
                    .map(|l| line_no - l <= ctx.before as u32 + 1)
                    .unwrap_or(false);

            if !merge {
                // Real chunk break: emit `--` (zero-context never separates,
                // matching ripgrep) and treat the ring buffer as fresh
                // before-context.
                if last_emitted_line.is_some() && !ctx.is_zero() {
                    write_separator(out_buf);
                }
            }
            // In both merge and no-merge cases we drain the ring buffer:
            // those queued lines are either the "gap fillers" between two
            // chunks being merged, or the before-context of a new chunk.
            // No need to update `last_emitted_line` per ring entry — the
            // match emission immediately below overrides it.
            for &(ln, s, e) in &prev_lines {
                let bytes = strip_trailing_cr(&mmap[s..e]);
                emit_line(
                    out_buf,
                    &path_str,
                    ln,
                    bytes,
                    LineKind::Context,
                    render,
                    hl_re.as_ref(),
                );
            }
            prev_lines.clear();

            emit_line(
                out_buf,
                &path_str,
                line_no,
                display_bytes,
                LineKind::Match,
                render,
                hl_re.as_ref(),
            );
            last_emitted_line = Some(line_no);
            match_count += 1;
            after_remaining = ctx.after;
        } else if after_remaining > 0 {
            emit_line(
                out_buf,
                &path_str,
                line_no,
                display_bytes,
                LineKind::Context,
                render,
                hl_re.as_ref(),
            );
            last_emitted_line = Some(line_no);
            after_remaining -= 1;
        } else if ctx.before > 0 {
            // Feed the before-context ring buffer.
            if prev_lines.len() == ctx.before {
                prev_lines.pop_front();
            }
            prev_lines.push_back((line_no, pos, end));
        }

        line_no += 1;
        pos = end + 1;
    }

    match_count
}

/// File-name header for heading mode. Emitted lazily on the first match in
/// a file (so files with no matches stay invisible).
fn emit_heading(out: &mut Vec<u8>, path_str: &str, render: &RenderOpts) {
    if render.color {
        let _ = write!(out, "{}{}{}{}\n", C_BOLD, C_PATH, path_str, C_RESET);
    } else {
        let _ = writeln!(out, "{}", path_str);
    }
}

/// `--` separator between non-overlapping context chunks. Only emitted in
/// non-zero-context mode; zero-context never produces separators (a single
/// continuous block of `path:line:content` matches the historical
/// behaviour and ripgrep's).
fn write_separator(out: &mut Vec<u8>) {
    out.extend_from_slice(b"--\n");
}

/// Emit a single output line. Layout depends on `render.heading`:
///   heading=true  → `<lineno><delim> <content>` (path header already emitted)
///   heading=false → `<path><delim><lineno><delim><content>`
/// The delimiter is `:` for match lines and `-` for context lines, the
/// ripgrep convention. When `render.color` is true the path is bold-magenta,
/// the line number green, and the matched substring inside content is
/// wrapped in bold-red via `highlight_bytes_into`.
fn emit_line(
    out: &mut Vec<u8>,
    path_str: &str,
    line_no: u32,
    content: &[u8],
    kind: LineKind,
    render: &RenderOpts,
    hl_re: Option<&regex::bytes::Regex>,
) {
    let delim: u8 = match kind {
        LineKind::Match => b':',
        LineKind::Context => b'-',
    };

    if !render.heading {
        if render.color {
            let _ = write!(out, "{}{}{}{}", C_BOLD, C_PATH, path_str, C_RESET);
        } else {
            let _ = write!(out, "{}", path_str);
        }
        out.push(delim);
    }

    if render.color {
        let _ = write!(out, "{}{}{}", C_LINENO, line_no, C_RESET);
    } else {
        let _ = write!(out, "{}", line_no);
    }
    out.push(delim);

    // Match lines get the matched substring highlighted; context lines are
    // emitted plain so the eye knows where the actual hit is.
    if matches!(kind, LineKind::Match) {
        if let Some(re) = hl_re {
            highlight_bytes_into(content, re, out);
        } else {
            out.extend_from_slice(content);
        }
    } else {
        out.extend_from_slice(content);
    }
    out.push(b'\n');
}

/// Bytes-domain analogue of cli.rs's `highlight_into`: scans `line` for
/// regex matches and writes the line into `out` with each match wrapped
/// in `C_MATCH`/`C_RESET`. Stays in the byte domain so non-UTF-8 source
/// content survives without lossy conversion.
fn highlight_bytes_into(line: &[u8], re: &regex::bytes::Regex, out: &mut Vec<u8>) {
    let mut last_end = 0;
    for m in re.find_iter(line) {
        if m.start() > last_end {
            out.extend_from_slice(&line[last_end..m.start()]);
        }
        out.extend_from_slice(C_MATCH.as_bytes());
        out.extend_from_slice(&line[m.start()..m.end()]);
        out.extend_from_slice(C_RESET.as_bytes());
        last_end = m.end();
    }
    if last_end < line.len() {
        out.extend_from_slice(&line[last_end..]);
    }
}
