use std::path::{Path, PathBuf};
use std::sync::Mutex;

use aho_corasick::AhoCorasick;
use anyhow::Result;
use memchr::memmem;
use memmap2::Mmap;
use rayon::prelude::*;
use regex::bytes::Regex as BytesRegex;

use std::collections::HashMap;

use crate::index::SparseIndex;
use crate::persist::SearchResult;

#[derive(Debug, Clone)]
pub struct Match {
    pub path: PathBuf,
    pub line_number: usize,
    pub line: String,
}

/// Determine if a pattern is a plain literal (no regex metacharacters or escapes).
fn is_literal(pattern: &str) -> bool {
    for ch in pattern.chars() {
        if matches!(
            ch,
            '.' | '*' | '+' | '?' | '[' | ']' | '(' | ')' | '{' | '}' | '|' | '^' | '$' | '\\'
        ) {
            return false;
        }
    }
    true
}

/// Extract the longest literal substring from a regex pattern for pre-filtering.
/// Returns None if no useful literal (< 3 bytes) can be extracted.
fn extract_longest_literal(pattern: &str) -> Option<Vec<u8>> {
    // Skip patterns with inline flags like (?i) — literal pre-filter is case-sensitive
    if pattern.starts_with("(?") {
        return None;
    }

    let mut best: Vec<u8> = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    let mut chars = pattern.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(&next) = chars.peek() {
                chars.next();
                if next.is_ascii_alphanumeric() {
                    // Regex escape class (\d, \w, \s, \n, \1, etc.) — break segment
                    if current.len() > best.len() {
                        best = std::mem::take(&mut current);
                    } else {
                        current.clear();
                    }
                } else {
                    // Escaped punctuation (\., \*, etc.) — literal character
                    let mut buf = [0u8; 4];
                    current.extend_from_slice(next.encode_utf8(&mut buf).as_bytes());
                }
            }
        } else if ".+*?[]{}()|^$".contains(ch) {
            // Regex metachar — break segment
            if current.len() > best.len() {
                best = std::mem::take(&mut current);
            } else {
                current.clear();
            }
        } else {
            let mut buf = [0u8; 4];
            current.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
    }
    if current.len() > best.len() {
        best = current;
    }

    if best.len() >= 3 {
        Some(best)
    } else {
        None
    }
}

/// Check if pattern is a pure alternation of literals like "TODO|FIXME|HACK".
fn try_literal_alternation(pattern: &str) -> Option<Vec<Vec<u8>>> {
    if pattern.starts_with("(?") {
        return None;
    }
    if !pattern.contains('|') {
        return None;
    }

    // Split on unescaped top-level |
    let mut parts: Vec<&str> = Vec::new();
    let mut last = 0;
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            i += 2;
        } else if bytes[i] == b'|' {
            parts.push(&pattern[last..i]);
            last = i + 1;
            i += 1;
        } else {
            i += 1;
        }
    }
    parts.push(&pattern[last..]);

    if parts.len() < 2 {
        return None;
    }

    let mut literals = Vec::new();
    for part in &parts {
        if part.is_empty() || !is_literal(part) {
            return None;
        }
        literals.push(part.as_bytes().to_vec());
    }
    Some(literals)
}

/// Matcher abstraction with SIMD-accelerated pre-filters.
enum Matcher {
    /// Pure literal — SIMD memmem only, no regex needed
    Literal(memmem::Finder<'static>),
    /// Pure regex — no pre-filter available
    Regex(BytesRegex),
    /// Literal pre-filter + regex verify: skip file if literal not found
    LiteralThenRegex {
        finder: memmem::Finder<'static>,
        regex: BytesRegex,
    },
    /// Aho-Corasick pre-filter for alternations + regex verify
    AhoCorasickThenRegex {
        ac: AhoCorasick,
        regex: BytesRegex,
    },
}

impl Matcher {
    fn new(pattern: &str) -> Result<Self> {
        // 1. Pure literal — no regex metacharacters at all
        if is_literal(pattern) {
            let needle: &'static [u8] = Vec::leak(pattern.as_bytes().to_vec());
            return Ok(Matcher::Literal(memmem::Finder::new(needle)));
        }

        // 2. Pure alternation of literals — use Aho-Corasick SIMD
        if let Some(literals) = try_literal_alternation(pattern) {
            let ac = AhoCorasick::new(&literals)?;
            let regex = BytesRegex::new(pattern)?;
            return Ok(Matcher::AhoCorasickThenRegex { ac, regex });
        }

        // 3. Extract longest literal for pre-filter
        if let Some(literal) = extract_longest_literal(pattern) {
            let needle: &'static [u8] = Vec::leak(literal);
            let finder = memmem::Finder::new(needle);
            let regex = BytesRegex::new(pattern)?;
            return Ok(Matcher::LiteralThenRegex { finder, regex });
        }

        // 4. Fallback: pure regex
        Ok(Matcher::Regex(BytesRegex::new(pattern)?))
    }

    /// Search a buffer and return (line_number, line_text) for each match.
    /// Uses whole-buffer searching, computes line numbers only for hits.
    /// For pure literals, SIMD memmem pre-filter skips non-matching files in O(n/32).
    #[inline]
    fn search_buffer(&self, buf: &[u8]) -> Vec<(usize, String)> {
        match self {
            Matcher::Literal(finder) => search_literal(buf, finder),
            Matcher::Regex(re) => search_regex(buf, re),
            Matcher::LiteralThenRegex { finder, regex } => {
                if finder.find(buf).is_none() {
                    return Vec::new();
                }
                search_regex(buf, regex)
            }
            Matcher::AhoCorasickThenRegex { ac, regex } => {
                if ac.find(buf).is_none() {
                    return Vec::new();
                }
                search_regex(buf, regex)
            }
        }
    }

    /// Quick check: does the buffer contain any match at all?
    #[inline]
    fn has_match(&self, buf: &[u8]) -> bool {
        match self {
            Matcher::Literal(finder) => finder.find(buf).is_some(),
            Matcher::Regex(re) => re.is_match(buf),
            Matcher::LiteralThenRegex { finder, regex } => {
                finder.find(buf).is_some() && regex.is_match(buf)
            }
            Matcher::AhoCorasickThenRegex { ac, regex } => {
                ac.find(buf).is_some() && regex.is_match(buf)
            }
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
            Matcher::Regex(re) => count_regex_lines(buf, re),
            Matcher::LiteralThenRegex { finder, regex } => {
                if finder.find(buf).is_none() {
                    return 0;
                }
                count_regex_lines(buf, regex)
            }
            Matcher::AhoCorasickThenRegex { ac, regex } => {
                if ac.find(buf).is_none() {
                    return 0;
                }
                count_regex_lines(buf, regex)
            }
        }
    }
}

/// Count regex matching lines without allocating Strings.
#[inline]
fn count_regex_lines(buf: &[u8], re: &BytesRegex) -> usize {
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

/// Known text extensions — skip binary check for these (major perf win)
#[inline(always)]
fn is_known_text_ext(path: &std::path::Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(e) => matches!(e,
            "rs"|"ts"|"tsx"|"js"|"jsx"|"py"|"go"|"rb"|"java"|"c"|"h"|"cpp"|"cc"|"hpp"|
            "cs"|"swift"|"kt"|"scala"|"php"|"html"|"css"|"scss"|"less"|"json"|"toml"|
            "yaml"|"yml"|"md"|"txt"|"sh"|"bash"|"zsh"|"fish"|"vim"|"lua"|"r"|"sql"|
            "xml"|"svg"|"tf"|"hcl"|"nix"|"ex"|"exs"|"erl"|"hrl"|"ml"|"mli"|"hs"|
            "clj"|"cljs"|"lisp"|"el"|"dart"|"zig"|"v"|"proto"|"graphql"|"gql"
        ),
        None => false,
    }
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
                let mmap = match open_mmap(path) {
                    Some(m) => m,
                    None => return Vec::new(),
                };
                let buf = &*mmap;
                if !is_known_text_ext(path) && is_binary(buf) {
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

        Ok(matches)
    }

    pub fn search_files_only(&self, pattern: &str) -> Result<Vec<PathBuf>> {
        let matcher = Matcher::new(pattern)?;
        let candidates = self.index.search(pattern);

        let files: Vec<PathBuf> = candidates
            .par_iter()
            .filter(|path| {
                let mmap = match open_mmap(path) {
                    Some(m) => m,
                    None => return false,
                };
                let buf = &*mmap;
                if !is_known_text_ext(path) && is_binary(buf) {
                    return false;
                }
                matcher.has_match(buf)
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

/// Count-optimized search: line-level verify for indexed, full-file for fallback.
pub fn search_persistent_count(
    index: &crate::persist::PersistentIndex,
    pattern: &str,
) -> Result<(usize, crate::persist::SearchTiming)> {
    let matcher = Matcher::new(pattern)?;
    let (result, mut timing) = index.search_timed(pattern);

    let t_verify = std::time::Instant::now();
    let count = match result {
        SearchResult::LineHits(hits) => {
            if hits.is_empty() {
                timing.matches = 0;
                return Ok((0, timing));
            }
            // Group by file path for efficient mmap
            let mut by_file: HashMap<&Path, Vec<u32>> = HashMap::new();
            for hit in &hits {
                by_file.entry(hit.path).or_default().push(hit.byte_offset);
            }
            let file_groups: Vec<_> = by_file.into_iter().collect();
            file_groups
                .par_iter()
                .map(|(path, offsets)| {
                    let mmap = match open_mmap(path) {
                        Some(m) => m,
                        None => return 0,
                    };
                    offsets
                        .iter()
                        .filter(|&&byte_offset| {
                            let start = byte_offset as usize;
                            if start >= mmap.len() {
                                return false;
                            }
                            let end = memchr::memchr(b'\n', &mmap[start..])
                                .map(|p| start + p)
                                .unwrap_or(mmap.len());
                            matcher.has_match(&mmap[start..end])
                        })
                        .count()
                })
                .sum()
        }
        SearchResult::AllFiles(paths) => {
            if paths.is_empty() {
                timing.matches = 0;
                return Ok((0, timing));
            }
            paths
                .par_iter()
                .map(|path| {
                    let mmap = match open_mmap(path) {
                        Some(m) => m,
                        None => return 0,
                    };
                    matcher.count_lines(&mmap)
                })
                .sum()
        }
    };

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

/// Search with detailed timing breakdown. Uses line-level verify when index provides line hits.
pub fn search_persistent_timed(
    index: &crate::persist::PersistentIndex,
    pattern: &str,
) -> Result<(Vec<Match>, crate::persist::SearchTiming)> {
    let matcher = Matcher::new(pattern)?;
    let (result, mut timing) = index.search_timed(pattern);

    let t_verify = std::time::Instant::now();
    let matches: Vec<Match> = match result {
        SearchResult::LineHits(hits) => {
            if hits.is_empty() {
                timing.matches = 0;
                return Ok((Vec::new(), timing));
            }
            // Group by file path for efficient mmap (one mmap per file)
            let mut by_file: HashMap<&Path, Vec<(u32, u32)>> = HashMap::new();
            for hit in &hits {
                by_file
                    .entry(hit.path)
                    .or_default()
                    .push((hit.line_no, hit.byte_offset));
            }
            let file_groups: Vec<_> = by_file.into_iter().collect();

            file_groups
                .par_iter()
                .flat_map(|(path, lines)| {
                    let mmap = match open_mmap(path) {
                        Some(m) => m,
                        None => return Vec::new(),
                    };
                    let path_buf = path.to_path_buf();
                    let mut file_matches = Vec::new();
                    for &(line_no, byte_offset) in lines {
                        let start = byte_offset as usize;
                        if start >= mmap.len() {
                            continue;
                        }
                        let end = memchr::memchr(b'\n', &mmap[start..])
                            .map(|p| start + p)
                            .unwrap_or(mmap.len());
                        let line_bytes = &mmap[start..end];
                        if matcher.has_match(line_bytes) {
                            let line =
                                String::from_utf8_lossy(line_bytes).into_owned();
                            file_matches.push(Match {
                                path: path_buf.clone(),
                                line_number: line_no as usize,
                                line,
                            });
                        }
                    }
                    file_matches
                })
                .collect()
        }
        SearchResult::AllFiles(paths) => {
            if paths.is_empty() {
                timing.matches = 0;
                return Ok((Vec::new(), timing));
            }
            // Fallback: full-file verify (pattern too short for trigrams)
            paths
                .par_iter()
                .flat_map(|path| {
                    let mmap = match open_mmap(path) {
                        Some(m) => m,
                        None => return Vec::new(),
                    };
                    let hits = matcher.search_buffer(&mmap);
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
                .collect()
        }
    };

    timing.verify_ms = t_verify.elapsed().as_secs_f64() * 1000.0;
    timing.matches = matches.len();
    Ok((matches, timing))
}

/// Fast full scan — optimized hot path:
/// - Raw bytes (no UTF-8 validation)
/// - SIMD memmem for literal patterns
/// - Parallel file walking + searching
/// - Buffer reuse per thread (no allocation per file)
/// - Line numbers computed only for actual matches
pub fn search_full_scan(
    root: &Path,
    pattern: &str,
    no_ignore: bool,
    type_filter: Option<&str>,
) -> Result<Vec<Match>> {
    let matcher = Matcher::new(pattern)?;
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
        let mut local_results: Vec<Match> = Vec::with_capacity(256);
        // Thread-local read buffer — reused across files
        let mut read_buf: Vec<u8> = Vec::with_capacity(64 * 1024);

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

            // Use metadata from the walk entry (already stat'd, no extra syscall)
            let flen = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if flen == 0 { return ignore::WalkState::Continue; }

            // Read with reusable buffer (fast for small files) or mmap (for large)
            read_buf.clear();
            let file = match std::fs::File::open(path) {
                Ok(f) => f,
                Err(_) => return ignore::WalkState::Continue,
            };

            let _mmap_holder;
            let buf: &[u8] = if flen > 256 * 1024 {
                _mmap_holder = unsafe { memmap2::Mmap::map(&file).ok() };
                match _mmap_holder.as_ref() {
                    Some(m) => m,
                    None => return ignore::WalkState::Continue,
                }
            } else {
                _mmap_holder = None;
                use std::io::Read;
                let mut f = file;
                if f.read_to_end(&mut read_buf).is_err() {
                    return ignore::WalkState::Continue;
                }
                &read_buf[..]
            };

            if !is_known_text_ext(path) && is_binary(buf) {
                return ignore::WalkState::Continue;
            }

            let hits = matcher.search_buffer(buf);
            if !hits.is_empty() {
                let path_buf = path.to_path_buf();
                for (ln, line) in hits {
                    local_results.push(Match {
                        path: path_buf.clone(),
                        line_number: ln,
                        line,
                    });
                }

                // Flush per file for correctness (closures can't flush on drop)
                let batch = std::mem::replace(&mut local_results, Vec::with_capacity(64));
                collector.lock().unwrap().push(batch);
            }

            ignore::WalkState::Continue
        })
    });

    let batches = collector.into_inner().unwrap();
    let total: usize = batches.iter().map(|b| b.len()).sum();
    let mut results = Vec::with_capacity(total);
    for batch in batches {
        results.extend(batch);
    }

    Ok(results)
}

/// Count-only full scan — zero allocation per match, just counts.
/// Fastest possible path for benchmarking and `-c` flag.
pub fn search_full_scan_count(
    root: &Path,
    pattern: &str,
    no_ignore: bool,
    type_filter: Option<&str>,
) -> Result<usize> {
    let matcher = Matcher::new(pattern)?;
    let total_count = std::sync::atomic::AtomicUsize::new(0);

    let walker = ignore::WalkBuilder::new(root)
        .git_ignore(!no_ignore)
        .hidden(false)
        .threads(num_cpus())
        .build_parallel();

    let type_filter_owned = type_filter.map(|s| s.to_string());

    walker.run(|| {
        let matcher = &matcher;
        let total_count = &total_count;
        let type_filter = type_filter_owned.as_deref();
        let mut read_buf: Vec<u8> = Vec::with_capacity(64 * 1024);

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

            read_buf.clear();
            let file = match std::fs::File::open(path) {
                Ok(f) => f,
                Err(_) => return ignore::WalkState::Continue,
            };
            let flen = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if flen == 0 { return ignore::WalkState::Continue; }

            let _mmap_holder;
            let buf: &[u8] = if flen > 256 * 1024 {
                _mmap_holder = unsafe { memmap2::Mmap::map(&file).ok() };
                match _mmap_holder.as_ref() {
                    Some(m) => m,
                    None => return ignore::WalkState::Continue,
                }
            } else {
                _mmap_holder = None;
                use std::io::Read;
                let mut f = file;
                if f.read_to_end(&mut read_buf).is_err() {
                    return ignore::WalkState::Continue;
                }
                &read_buf[..]
            };

            if !is_known_text_ext(path) && is_binary(buf) {
                return ignore::WalkState::Continue;
            }

            let count = matcher.count_lines(buf);
            if count > 0 {
                total_count.fetch_add(count, std::sync::atomic::Ordering::Relaxed);
            }

            ignore::WalkState::Continue
        })
    });

    Ok(total_count.load(std::sync::atomic::Ordering::Relaxed))
}

/// Streaming full scan — writes directly to output, minimal allocations.
/// Uses capped read buffers (like ripgrep) to limit memory usage.
pub fn search_full_scan_streaming<W: std::io::Write + Send>(
    root: &Path,
    pattern: &str,
    no_ignore: bool,
    type_filter: Option<&str>,
    output: &Mutex<W>,
) -> Result<usize> {
    let matcher = Matcher::new(pattern)?;
    let match_count = std::sync::atomic::AtomicUsize::new(0);

    let walker = ignore::WalkBuilder::new(root)
        .git_ignore(!no_ignore)
        .hidden(false)
        .threads(num_cpus())
        .build_parallel();

    let type_filter_owned = type_filter.map(|s| s.to_string());

    walker.run(|| {
        let matcher = &matcher;
        let output = output;
        let match_count = &match_count;
        let type_filter = type_filter_owned.as_deref();
        // Fixed-capacity read buffer — caps memory at ~1MB per thread regardless of file size
        // Thread-local reusable read buffer for small files
        let mut read_buf: Vec<u8> = Vec::with_capacity(64 * 1024);
        // Thread-local output buffer to batch writes
        let mut out_buf: Vec<u8> = Vec::with_capacity(64 * 1024);

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

            // Hybrid read strategy: reusable buffer for small files, mmap for large
            read_buf.clear();
            let file = match std::fs::File::open(path) {
                Ok(f) => f,
                Err(_) => return ignore::WalkState::Continue,
            };
            let flen = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if flen == 0 { return ignore::WalkState::Continue; }

            // For files that would bloat our buffer, use mmap
            let _mmap_holder;
            let buf: &[u8] = if flen > 256 * 1024 {
                _mmap_holder = match unsafe { memmap2::Mmap::map(&file) } {
                    Ok(m) => Some(m),
                    Err(_) => return ignore::WalkState::Continue,
                };
                _mmap_holder.as_ref().unwrap()
            } else {
                _mmap_holder = None;
                use std::io::Read;
                let mut f = file;
                if f.read_to_end(&mut read_buf).is_err() {
                    return ignore::WalkState::Continue;
                }
                &read_buf[..]
            };

            if !is_known_text_ext(path) && is_binary(buf) {
                return ignore::WalkState::Continue;
            }

            // Pre-filter: skip entire file if literal/AC not found
            match matcher {
                Matcher::LiteralThenRegex { ref finder, .. } => {
                    if finder.find(buf).is_none() {
                        return ignore::WalkState::Continue;
                    }
                }
                Matcher::AhoCorasickThenRegex { ref ac, .. } => {
                    if ac.find(buf).is_none() {
                        return ignore::WalkState::Continue;
                    }
                }
                _ => {}
            }

            let path_bytes = path.to_string_lossy();
            let mut file_count = 0usize;

            match matcher {
                Matcher::Literal(ref finder) => {
                    let mut offset = 0;
                    let mut line_num: usize = 1;
                    let mut counted_to: usize = 0;

                    while let Some(pos) = finder.find(&buf[offset..]) {
                        let abs_pos = offset + pos;
                        line_num += memchr::memchr_iter(b'\n', &buf[counted_to..abs_pos]).count();
                        counted_to = abs_pos;
                        let (line_start, line_end) = line_bounds(buf, abs_pos);

                        use std::io::Write;
                        let _ = write!(out_buf, "{}:{}:", path_bytes, line_num);
                        out_buf.extend_from_slice(&buf[line_start..line_end]);
                        out_buf.push(b'\n');
                        file_count += 1;

                        offset = line_end + 1;
                        if offset >= buf.len() { break; }
                    }
                }
                Matcher::Regex(ref re)
                | Matcher::LiteralThenRegex { regex: ref re, .. }
                | Matcher::AhoCorasickThenRegex { regex: ref re, .. } => {
                    let mut last_line_start = usize::MAX;
                    let mut line_num: usize = 1;
                    let mut counted_to: usize = 0;

                    for m in re.find_iter(buf) {
                        let start = m.start();
                        line_num += memchr::memchr_iter(b'\n', &buf[counted_to..start]).count();
                        counted_to = start;
                        let (line_start, line_end) = line_bounds(buf, start);

                        if line_start != last_line_start {
                            use std::io::Write;
                            let _ = write!(out_buf, "{}:{}:", path_bytes, line_num);
                            out_buf.extend_from_slice(&buf[line_start..line_end]);
                            out_buf.push(b'\n');
                            file_count += 1;
                            last_line_start = line_start;
                        }
                    }
                }
            }

            if file_count > 0 {
                match_count.fetch_add(file_count, std::sync::atomic::Ordering::Relaxed);
            }

            // Flush output buffer when large enough
            if out_buf.len() >= 32 * 1024 {
                use std::io::Write;
                let mut out = output.lock().unwrap();
                let _ = out.write_all(&out_buf);
                out_buf.clear();
            }

            ignore::WalkState::Continue
        })
    });

    Ok(match_count.load(std::sync::atomic::Ordering::Relaxed))
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
