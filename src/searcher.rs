use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;
use memchr::memmem;
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
}

/// Literal search using SIMD memmem. Deduplicates per-line.
#[inline]
fn search_literal(buf: &[u8], finder: &memmem::Finder) -> Vec<(usize, String)> {
    let mut results = Vec::new();
    let mut offset = 0;

    while let Some(pos) = finder.find(&buf[offset..]) {
        let abs_pos = offset + pos;
        let (line_num, line_start, line_end) = line_at_offset(buf, abs_pos);
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

/// Regex search on raw byte buffer.
#[inline]
fn search_regex(buf: &[u8], re: &BytesRegex) -> Vec<(usize, String)> {
    let mut results = Vec::new();
    let mut last_line_num = 0;

    for m in re.find_iter(buf) {
        let start = m.start();
        let (line_num, line_start, line_end) = line_at_offset(buf, start);
        // Deduplicate: skip if same line as previous match
        if line_num != last_line_num || results.is_empty() {
            let line = String::from_utf8_lossy(&buf[line_start..line_end]).into_owned();
            results.push((line_num, line));
            last_line_num = line_num;
        }
    }

    results
}

/// Given a byte buffer and an offset, find line number and line boundaries.
/// Uses SIMD memchr for newline scanning.
#[inline]
fn line_at_offset(buf: &[u8], offset: usize) -> (usize, usize, usize) {
    let line_num = memchr::memchr_iter(b'\n', &buf[..offset]).count() + 1;

    let line_start = match memchr::memrchr(b'\n', &buf[..offset]) {
        Some(p) => p + 1,
        None => 0,
    };

    let line_end = match memchr::memchr(b'\n', &buf[offset..]) {
        Some(p) => offset + p,
        None => buf.len(),
    };

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

/// Search using a persistent index with Rayon parallel verify.
pub fn search_persistent(
    index: &crate::persist::PersistentIndex,
    pattern: &str,
) -> Result<Vec<Match>> {
    let matcher = Matcher::new(pattern)?;
    let candidates = index.search(pattern);

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
