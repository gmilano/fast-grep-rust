use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;
use memchr::memmem;
use memmap2::Mmap;
use rayon::prelude::*;
use regex::bytes::Regex as BytesRegex;

use crate::index::SparseIndex;

#[derive(Debug, Clone)]
pub struct Match {
    pub path: PathBuf,
    pub line_number: usize,
    pub line: String,
}

/// Determine if a pattern is a plain literal (no regex metacharacters).
fn is_literal(pattern: &str) -> bool {
    let mut chars = pattern.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if chars.next().is_none() {
                return false;
            }
        } else if matches!(
            ch,
            '.' | '*' | '+' | '?' | '[' | ']' | '(' | ')' | '{' | '}' | '|' | '^' | '$'
        ) {
            return false;
        }
    }
    true
}

/// Matcher abstraction: literal (SIMD memmem) or regex.
enum Matcher {
    Literal(memmem::Finder<'static>),
    Regex(BytesRegex),
}

impl Matcher {
    fn new(pattern: &str) -> Result<Self> {
        if is_literal(pattern) {
            // Leak pattern bytes so Finder has 'static lifetime — tiny, one-time cost
            let needle: &'static [u8] = Vec::leak(pattern.as_bytes().to_vec());
            Ok(Matcher::Literal(memmem::Finder::new(needle)))
        } else {
            Ok(Matcher::Regex(BytesRegex::new(pattern)?))
        }
    }

    /// Search a buffer and return (line_number, line_text) for each match.
    /// Uses whole-buffer searching, computes line numbers only for hits.
    #[inline]
    fn search_buffer(&self, buf: &[u8]) -> Vec<(usize, String)> {
        match self {
            Matcher::Literal(finder) => search_literal(buf, finder),
            Matcher::Regex(re) => search_regex(buf, re),
        }
    }

    /// Quick check: does the buffer contain any match at all?
    #[inline]
    fn has_match(&self, buf: &[u8]) -> bool {
        match self {
            Matcher::Literal(finder) => finder.find(buf).is_some(),
            Matcher::Regex(re) => re.is_match(buf),
        }
    }

    /// Count matching lines without allocating Strings.
    #[inline]
    fn count_lines(&self, buf: &[u8]) -> usize {
        match self {
            Matcher::Literal(finder) => {
                let mut count = 0;
                let mut offset = 0;
                while let Some(pos) = finder.find(&buf[offset..]) {
                    count += 1;
                    let abs_pos = offset + pos;
                    let (_, line_end) = line_bounds(buf, abs_pos);
                    offset = line_end + 1;
                    if offset >= buf.len() {
                        break;
                    }
                }
                count
            }
            Matcher::Regex(re) => {
                let mut count = 0;
                let mut last_line_start = usize::MAX;
                for m in re.find_iter(buf) {
                    let (line_start, _) = line_bounds(buf, m.start());
                    if line_start != last_line_start {
                        count += 1;
                        last_line_start = line_start;
                    }
                }
                count
            }
        }
    }
}

/// Literal search using SIMD memmem. Incremental line counting.
#[inline]
fn search_literal(buf: &[u8], finder: &memmem::Finder) -> Vec<(usize, String)> {
    let mut results = Vec::new();
    let mut offset = 0;
    let mut line_num: usize = 1;
    let mut counted_to: usize = 0; // how far we've counted newlines

    while let Some(pos) = finder.find(&buf[offset..]) {
        let abs_pos = offset + pos;

        // Incrementally count newlines from where we left off
        line_num += memchr::memchr_iter(b'\n', &buf[counted_to..abs_pos]).count();
        counted_to = abs_pos;

        let (line_start, line_end) = line_bounds(buf, abs_pos);
        let line = String::from_utf8_lossy(&buf[line_start..line_end]).into_owned();
        results.push((line_num, line));

        // Advance past this line to avoid duplicates
        offset = line_end + 1;
        if offset >= buf.len() {
            break;
        }
    }

    results
}

/// Regex search on raw byte buffer. Incremental line counting.
#[inline]
fn search_regex(buf: &[u8], re: &BytesRegex) -> Vec<(usize, String)> {
    let mut results = Vec::new();
    let mut last_line_start = usize::MAX;
    let mut line_num: usize = 1;
    let mut counted_to: usize = 0;

    for m in re.find_iter(buf) {
        let start = m.start();

        // Incrementally count newlines
        line_num += memchr::memchr_iter(b'\n', &buf[counted_to..start]).count();
        counted_to = start;

        let (line_start, line_end) = line_bounds(buf, start);

        // Deduplicate: skip if same line
        if line_start != last_line_start {
            let line = String::from_utf8_lossy(&buf[line_start..line_end]).into_owned();
            results.push((line_num, line));
            last_line_start = line_start;
        }
    }

    results
}

/// Find line start and end boundaries around an offset.
#[inline]
fn line_bounds(buf: &[u8], offset: usize) -> (usize, usize) {
    let line_start = match memchr::memrchr(b'\n', &buf[..offset]) {
        Some(p) => p + 1,
        None => 0,
    };

    let line_end = match memchr::memchr(b'\n', &buf[offset..]) {
        Some(p) => offset + p,
        None => buf.len(),
    };

    (line_start, line_end)
}

/// Given a byte buffer and an offset, find line number and line boundaries.
/// Uses SIMD memchr for newline scanning. Used when incremental counting
/// is not available (e.g. single-match lookups).
#[inline]
fn line_at_offset(buf: &[u8], offset: usize) -> (usize, usize, usize) {
    let line_num = memchr::memchr_iter(b'\n', &buf[..offset]).count() + 1;
    let (line_start, line_end) = line_bounds(buf, offset);
    (line_num, line_start, line_end)
}

/// Check if buffer looks binary (null byte in first 512 bytes).
#[inline]
fn is_binary(buf: &[u8]) -> bool {
    let check_len = buf.len().min(512);
    memchr::memchr(0, &buf[..check_len]).is_some()
}

pub struct Searcher {
    index: SparseIndex,
}

impl Searcher {
    pub fn new(root: &Path, no_ignore: bool, type_filter: Option<&str>) -> Result<Self> {
        let index = SparseIndex::build_from_directory(root, no_ignore, type_filter, false)?;
        Ok(Searcher { index })
    }

    pub fn search(&self, pattern: &str) -> Result<Vec<Match>> {
        let matcher = Matcher::new(pattern)?;
        let candidates = self.index.search(pattern);

        let matches: Vec<Match> = candidates
            .par_iter()
            .flat_map(|path| {
                let buf = match std::fs::read(path) {
                    Ok(b) => b,
                    Err(_) => return Vec::new(),
                };
                if is_binary(&buf) {
                    return Vec::new();
                }
                let hits = matcher.search_buffer(&buf);
                if hits.is_empty() {
                    return Vec::new();
                }
                let path_buf = path.to_path_buf();
                hits.into_iter()
                    .map(|(ln, line)| Match {
                        path: path_buf.clone(),
                        line_number: ln,
                        line,
                    })
                    .collect()
            })
            .collect();

        Ok(matches)
    }

    pub fn search_files_only(&self, pattern: &str) -> Result<Vec<PathBuf>> {
        let matcher = Matcher::new(pattern)?;
        let candidates = self.index.search(pattern);

        let files: Vec<PathBuf> = candidates
            .par_iter()
            .filter(|path| {
                if let Ok(buf) = std::fs::read(path) {
                    if is_binary(&buf) {
                        return false;
                    }
                    matcher.has_match(&buf)
                } else {
                    false
                }
            })
            .map(|p| p.to_path_buf())
            .collect();

        Ok(files)
    }

    pub fn search_count(&self, pattern: &str) -> Result<usize> {
        let matches = self.search(pattern)?;
        Ok(matches.len())
    }
}

/// Count-optimized search: chunked parallel with buffer reuse, no String allocation.
pub fn search_persistent_count(
    index: &crate::persist::PersistentIndex,
    pattern: &str,
) -> Result<(usize, crate::persist::SearchTiming)> {
    let matcher = Matcher::new(pattern)?;
    let (candidates, mut timing) = index.search_timed(pattern);

    if candidates.is_empty() {
        timing.matches = 0;
        return Ok((0, timing));
    }

    let t_verify = std::time::Instant::now();
    let nthreads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let chunk_size = (candidates.len() / nthreads).max(64);

    let count: usize = candidates
        .par_chunks(chunk_size)
        .map(|chunk| {
            let mut total = 0usize;
            let mut buf = Vec::with_capacity(128 * 1024);
            for path in chunk {
                buf.clear();
                let mut f = match std::fs::File::open(path) {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                use std::io::Read;
                if f.read_to_end(&mut buf).is_err() || buf.is_empty() {
                    continue;
                }
                if is_binary(&buf) {
                    continue;
                }
                total += matcher.count_lines(&buf);
            }
            total
        })
        .sum();

    timing.verify_ms = t_verify.elapsed().as_secs_f64() * 1000.0;
    timing.matches = count;
    Ok((count, timing))
}

/// mmap a file for zero-copy read. Returns None on error or empty file.
#[inline]
fn open_mmap(path: &Path) -> Option<Mmap> {
    let file = std::fs::File::open(path).ok()?;
    let mmap = unsafe { Mmap::map(&file).ok()? };
    if mmap.is_empty() {
        None
    } else {
        Some(mmap)
    }
}

/// Search using a persistent index with Rayon parallel verify.
pub fn search_persistent(
    index: &crate::persist::PersistentIndex,
    pattern: &str,
) -> Result<Vec<Match>> {
    Ok(search_persistent_timed(index, pattern)?.0)
}

/// Search with detailed timing breakdown.
pub fn search_persistent_timed(
    index: &crate::persist::PersistentIndex,
    pattern: &str,
) -> Result<(Vec<Match>, crate::persist::SearchTiming)> {
    let matcher = Matcher::new(pattern)?;
    let (candidates, mut timing) = index.search_timed(pattern);

    // Opt 4: skip verify if 0 candidates
    if candidates.is_empty() {
        timing.matches = 0;
        return Ok((Vec::new(), timing));
    }

    let t_verify = std::time::Instant::now();
    let matches: Vec<Match> = candidates
        .par_iter()
        .flat_map(|path| {
            let mmap = match open_mmap(path) {
                Some(m) => m,
                None => return Vec::new(),
            };
            let buf = &*mmap;
            if is_binary(buf) {
                return Vec::new();
            }
            let hits = matcher.search_buffer(buf);
            if hits.is_empty() {
                return Vec::new();
            }
            let path_buf = path.to_path_buf();
            hits.into_iter()
                .map(|(ln, line)| Match {
                    path: path_buf.clone(),
                    line_number: ln,
                    line,
                })
                .collect()
        })
        .collect();

    timing.verify_ms = t_verify.elapsed().as_secs_f64() * 1000.0;
    timing.matches = matches.len();
    Ok((matches, timing))
}

/// Fast full scan — optimized hot path:
/// - Raw bytes (no UTF-8 validation)
/// - SIMD memmem for literal patterns
/// - Parallel file walking + searching (no Mutex collection)
/// - Line numbers computed only for actual matches
pub fn search_full_scan(
    root: &Path,
    pattern: &str,
    no_ignore: bool,
    type_filter: Option<&str>,
) -> Result<Vec<Match>> {
    let matcher = Matcher::new(pattern)?;

    // Use ignore's parallel walker — each thread collects locally
    let collector: Mutex<Vec<Vec<Match>>> = Mutex::new(Vec::new());

    let walker = ignore::WalkBuilder::new(root)
        .git_ignore(!no_ignore)
        .hidden(false)
        .threads(num_cpus())
        .build_parallel();

    let type_filter_owned = type_filter.map(|s| s.to_string());

    walker.run(|| {
        let matcher = &matcher;
        let collector = &collector;
        let type_filter = type_filter_owned.as_deref();

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

            let buf = match std::fs::read(path) {
                Ok(b) => b,
                Err(_) => return ignore::WalkState::Continue,
            };

            if is_binary(&buf) {
                return ignore::WalkState::Continue;
            }

            let hits = matcher.search_buffer(&buf);
            if !hits.is_empty() {
                let path_buf = path.to_path_buf();
                let file_matches: Vec<Match> = hits
                    .into_iter()
                    .map(|(ln, line)| Match {
                        path: path_buf.clone(),
                        line_number: ln,
                        line,
                    })
                    .collect();
                collector.lock().unwrap().push(file_matches);
            }

            ignore::WalkState::Continue
        })
    });

    // Flatten all thread-local batches
    let batches = collector.into_inner().unwrap();
    let total: usize = batches.iter().map(|b| b.len()).sum();
    let mut results = Vec::with_capacity(total);
    for batch in batches {
        results.extend(batch);
    }

    Ok(results)
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Verify using mmap instead of fs::read — avoids heap allocation per file.
pub fn search_persistent_mmap(
    index: &crate::persist::PersistentIndex,
    pattern: &str,
) -> Result<Vec<Match>> {
    let matcher = Matcher::new(pattern)?;
    let candidates = index.search(pattern);

    let matches: Vec<Match> = candidates
        .par_iter()
        .flat_map(|path| {
            let file = match std::fs::File::open(path) {
                Ok(f) => f,
                Err(_) => return Vec::new(),
            };
            let buf = match unsafe { memmap2::Mmap::map(&file) } {
                Ok(m) => m,
                Err(_) => return Vec::new(),
            };
            if is_binary(&buf) {
                return Vec::new();
            }
            let hits = matcher.search_buffer(&buf);
            if hits.is_empty() {
                return Vec::new();
            }
            let path_buf = path.to_path_buf();
            hits.into_iter()
                .map(|(ln, line)| Match {
                    path: path_buf.clone(),
                    line_number: ln,
                    line,
                })
                .collect()
        })
        .collect();

    Ok(matches)
}
