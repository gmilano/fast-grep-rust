use std::path::{Path, PathBuf};
use std::sync::Mutex;

use aho_corasick::AhoCorasick;
use anyhow::Result;
use ignore::overrides::{Override, OverrideBuilder};
use memchr::memmem;
use memmap2::Mmap;
use rayon::prelude::*;
use regex::bytes::Regex as BytesRegex;

use std::collections::HashMap;

#[cfg(target_os = "macos")]
use crate::metal::metal_impl::global_verifier;
use crate::persist::SearchResult;

/// Build an `ignore::overrides::Override` from include/exclude glob lists.
/// The `ignore` crate uses the `!` prefix to negate a glob (i.e. exclude);
/// we translate `--include`/`--exclude` into that shape so a `WalkBuilder`
/// can do filtering during the parallel walk, and `passes_overrides` can
/// apply the same predicate per-path on the indexed search path.
///
/// Multiple includes are OR'd (a file is in if it matches any), same for
/// excludes. Returns `Ok(None)` when both lists are empty so callers can
/// skip the WalkBuilder configuration step entirely.
pub(crate) fn build_overrides(
    root: &Path,
    includes: &[String],
    excludes: &[String],
) -> Result<Option<Override>> {
    if includes.is_empty() && excludes.is_empty() {
        return Ok(None);
    }
    let mut builder = OverrideBuilder::new(root);
    for pat in includes {
        builder.add(pat)?;
    }
    for pat in excludes {
        builder.add(&format!("!{}", pat))?;
    }
    Ok(Some(builder.build()?))
}

/// Apply `--include` / `--exclude` globs to a single candidate path.
/// Used by the indexed search path where we already have the candidate
/// list and just need to filter it post-lookup. Equivalent to checking
/// `Override::matched(path).is_ignore()`.
pub(crate) fn passes_overrides(path: &Path, ov: Option<&Override>) -> bool {
    let ov = match ov {
        Some(o) => o,
        None => return true,
    };
    !ov.matched(path, false).is_ignore()
}

/// Apply `--type EXT` (potentially repeated) to a single candidate path.
/// The list is OR'd: a file passes if its extension matches any entry.
/// An empty list short-circuits to true so callers can thread the flag
/// through unconditionally.
pub(crate) fn passes_type_filter(path: &Path, type_filters: &[String]) -> bool {
    if type_filters.is_empty() {
        return true;
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => type_filters.iter().any(|t| t == ext),
        None => false,
    }
}

/// Match record for the legacy aggregate-mode APIs (`search_full_scan` and
/// `search_persistent_timed`). The render pipeline writes formatted bytes
/// directly and never goes through this type, so the binary itself reads
/// only `path` (for `--count` / `--files-with-matches` summaries). The
/// `line_number` and `line` fields stay live for the integration tests in
/// `tests/searcher_integration.rs` and `tests/regex_correctness.rs`,
/// which need full match records to assert correctness against the regex
/// crate's reference behaviour.
#[allow(dead_code)]
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

/// Trim a single trailing `\r` from a line slice. Files with CRLF endings
/// keep the `\r` attached because we split on `\n`; leaving it in rendered
/// output produces stray `^M` glyphs and breaks ANSI cursor positioning
/// across consecutive matches in the same file (the CR returns the cursor
/// to column 0 mid-line, then the following match's escape codes start
/// drawing on top of the previous content).
#[inline]
pub(crate) fn strip_trailing_cr(line: &[u8]) -> &[u8] {
    match line.split_last() {
        Some((b'\r', rest)) => rest,
        _ => line,
    }
}

/// Path-component-based heuristic for "hidden". Returns true if any component
/// of `p` *under* `root` starts with `.` (Unix dotfile convention, also used
/// cross-platform by `.git/`, `.github/`, `.cargo/`, etc.). Mirrors what the
/// `ignore` crate does when `.hidden(true)` is set: the filter applies to
/// entries inside the search tree, not to ancestors of the root itself.
/// Stripping the root prefix matters on systems where the path *to* the root
/// happens to contain dot-components (e.g. Windows tempdirs are
/// `…\Temp\.tmpXXXX`, and a project under `~/.config/foo/` is not "hidden"
/// from the user's perspective just because `.config` sits above it).
pub(crate) fn is_hidden_path(p: &Path, root: &Path) -> bool {
    let rel = p.strip_prefix(root).unwrap_or(p);
    rel.components().any(|c| match c {
        std::path::Component::Normal(s) => s.to_str().map_or(false, |s| s.starts_with('.')),
        _ => false,
    })
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
                    // Regex escape class (\d, \w, \s, \n, \p, \P, \1, etc.) — break segment
                    if current.len() > best.len() {
                        best = std::mem::take(&mut current);
                    } else {
                        current.clear();
                    }
                    // \p{...} and \P{...} — skip property name in braces
                    if (next == 'p' || next == 'P') && chars.peek() == Some(&'{') {
                        chars.next();
                        while let Some(c) = chars.next() {
                            if c == '}' {
                                break;
                            }
                        }
                    }
                } else {
                    // Escaped punctuation (\., \*, etc.) — literal character
                    let mut buf = [0u8; 4];
                    current.extend_from_slice(next.encode_utf8(&mut buf).as_bytes());
                }
            }
        } else if ch == '[' {
            // Skip entire character class — contents are not literals
            if current.len() > best.len() {
                best = std::mem::take(&mut current);
            } else {
                current.clear();
            }
            // Handle '^' after '[' and ']' as first char in class (e.g., [^]b] or []b])
            let mut first = true;
            if chars.peek() == Some(&'^') {
                chars.next();
            }
            while let Some(c) = chars.next() {
                if c == '\\' {
                    chars.next();
                    first = false;
                } else if c == '[' && chars.peek() == Some(&':') {
                    // POSIX class like [:alnum:] inside a bracket expression.
                    // Pattern [[:space:]] — inner [:space:] closes at :],
                    // outer ] remains for the bracket expression.
                    chars.next(); // consume ':'
                    while let Some(p) = chars.next() {
                        if p == ':' {
                            if chars.peek() == Some(&']') {
                                chars.next(); // consume closing ']'
                            }
                            break;
                        }
                    }
                    first = false;
                } else if c == ']' && !first {
                    break;
                } else {
                    first = false;
                }
            }
        } else if ch == '(' {
            // Skip entire group if it contains alternation — literals from one
            // branch of an OR cannot be used as a required pre-filter
            if current.len() > best.len() {
                best = std::mem::take(&mut current);
            } else {
                current.clear();
            }
            let mut depth = 1i32;
            let mut has_alt = false;
            {
                let mut scan = chars.clone();
                while let Some(c) = scan.next() {
                    match c {
                        '\\' => {
                            scan.next();
                        }
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        '|' if depth == 1 => has_alt = true,
                        _ => {}
                    }
                }
            }
            if has_alt {
                // Skip to matching ')' — don't extract literals from alternation
                depth = 1;
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => {
                            chars.next();
                        }
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            } else {
                // No alternation — skip group syntax but parse contents
                if chars.peek() == Some(&'?') {
                    let mut lookahead = chars.clone();
                    lookahead.next();
                    if let Some(&after) = lookahead.peek() {
                        // Same flag-group recognition rule as trigram::extract_literal_runs;
                        // `R` (CRLF mode) was missing there too — see that file's note.
                        if ":PimRsxuU-<!=".contains(after) {
                            chars.next();
                            while let Some(&c) = chars.peek() {
                                if c == ':' || c == ')' {
                                    chars.next();
                                    break;
                                }
                                chars.next();
                            }
                        }
                    }
                }
            }
        } else if ch == '{' {
            // Skip repetition quantifier {n}, {n,}, {n,m} — contents are not literals
            if current.len() > best.len() {
                best = std::mem::take(&mut current);
            } else {
                current.clear();
            }
            while let Some(c) = chars.next() {
                if c == '}' {
                    break;
                }
            }
        } else if ".+*?})|^$".contains(ch) {
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

/// Returns true if the pattern could produce cross-line matches,
/// meaning we need to search line-by-line to match grep/rg behavior.
/// This is the case when the pattern uses [[:space:]], \s, or similar
/// that match \n, but does NOT use explicit (?s) or (?m) multiline flags.
pub(crate) fn needs_line_by_line(pattern: &str) -> bool {
    // If pattern explicitly opts into multiline/dotall, respect it
    if pattern.contains("(?s)") || pattern.contains("(?m)") || pattern.contains("(?is)") {
        return false;
    }
    // If pattern contains a literal \n, it intentionally crosses lines
    if pattern.contains(r"\n") || pattern.contains("\n") {
        return false;
    }
    // If pattern uses POSIX classes that include \n, force line-by-line
    if pattern.contains("[[:space:]]") || pattern.contains("[[:blank:]]") {
        return true;
    }
    // \s class also matches \n
    if pattern.contains(r"\s") {
        return true;
    }
    false
}

/// Matcher abstraction with SIMD-accelerated pre-filters.
pub(crate) enum Matcher {
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
    AhoCorasickThenRegex { ac: AhoCorasick, regex: BytesRegex },
}

impl Matcher {
    pub(crate) fn new(pattern: &str) -> Result<Self> {
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
    fn search_buffer(&self, buf: &[u8], lbl: bool) -> Vec<(usize, String)> {
        match self {
            Matcher::Literal(finder) => search_literal(buf, finder),
            Matcher::Regex(re) => search_regex(buf, re, lbl),
            Matcher::LiteralThenRegex { finder, regex } => {
                if finder.find(buf).is_none() {
                    return Vec::new();
                }
                search_regex(buf, regex, lbl)
            }
            Matcher::AhoCorasickThenRegex { ac, regex } => {
                if ac.find(buf).is_none() {
                    return Vec::new();
                }
                search_regex(buf, regex, lbl)
            }
        }
    }

    /// Quick check: does the buffer contain any match at all?
    /// IMPORTANT: We check line-by-line for regex patterns to avoid cross-line
    /// false positives (e.g. [[:space:]] matching \n between two lines).
    #[inline]
    pub(crate) fn has_match(&self, buf: &[u8], lbl: bool) -> bool {
        match self {
            Matcher::Literal(finder) => finder.find(buf).is_some(),
            Matcher::Regex(re) => {
                if lbl {
                    buf.split(|&b| b == b'\n').any(|line| re.is_match(line))
                } else {
                    re.is_match(buf)
                }
            }
            Matcher::LiteralThenRegex { finder, regex } => {
                finder.find(buf).is_some() && {
                    if lbl {
                        buf.split(|&b| b == b'\n').any(|line| regex.is_match(line))
                    } else {
                        regex.is_match(buf)
                    }
                }
            }
            Matcher::AhoCorasickThenRegex { ac, regex } => {
                ac.find(buf).is_some() && {
                    if lbl {
                        buf.split(|&b| b == b'\n').any(|line| regex.is_match(line))
                    } else {
                        regex.is_match(buf)
                    }
                }
            }
        }
    }

    /// Inverse of `search_buffer`: collect lines that **do not** match the
    /// pattern. Used when `--invert-match` (`-v`) is on. The pre-filter
    /// optimisations (memmem, Aho-Corasick) don't help here — we have to
    /// look at every line — so this is plain per-line iteration that
    /// delegates to `has_match` for the predicate.
    #[inline]
    fn scan_non_match_lines(&self, buf: &[u8], lbl: bool) -> Vec<(usize, String)> {
        let mut results = Vec::new();
        let mut line_no: usize = 0;
        let mut pos = 0;
        while pos < buf.len() {
            let end = memchr::memchr(b'\n', &buf[pos..])
                .map(|p| pos + p)
                .unwrap_or(buf.len());
            line_no += 1;
            let line = &buf[pos..end];
            if !self.has_match(line, lbl) {
                results.push((line_no, String::from_utf8_lossy(line).into_owned()));
            }
            pos = end + 1;
        }
        results
    }

    /// Inverse of `count_lines`: count lines that **do not** match. Same
    /// shape as `scan_non_match_lines` but skips the String allocation.
    #[inline]
    fn count_non_match_lines(&self, buf: &[u8], lbl: bool) -> usize {
        let mut count = 0;
        let mut pos = 0;
        while pos < buf.len() {
            let end = memchr::memchr(b'\n', &buf[pos..])
                .map(|p| pos + p)
                .unwrap_or(buf.len());
            if !self.has_match(&buf[pos..end], lbl) {
                count += 1;
            }
            pos = end + 1;
        }
        count
    }

    /// Count matching lines without allocating Strings.
    #[inline]
    fn count_lines(&self, buf: &[u8], lbl: bool) -> usize {
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
            Matcher::Regex(re) => count_regex_lines(buf, re, lbl),
            Matcher::LiteralThenRegex { finder, regex } => {
                if finder.find(buf).is_none() {
                    return 0;
                }
                count_regex_lines(buf, regex, lbl)
            }
            Matcher::AhoCorasickThenRegex { ac, regex } => {
                if ac.find(buf).is_none() {
                    return 0;
                }
                count_regex_lines(buf, regex, lbl)
            }
        }
    }
}

/// Count regex matching lines without allocating Strings.
#[inline]
fn count_regex_lines(buf: &[u8], re: &BytesRegex, line_by_line: bool) -> usize {
    if line_by_line {
        let mut count = 0;
        let mut pos = 0;
        while pos <= buf.len() {
            let end = memchr::memchr(b'\n', &buf[pos..])
                .map(|p| pos + p)
                .unwrap_or(buf.len());
            if re.is_match(&buf[pos..end]) {
                count += 1;
            }
            pos = end + 1;
        }
        return count;
    }
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
        let line =
            String::from_utf8_lossy(strip_trailing_cr(&buf[line_start..line_end])).into_owned();
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
/// line_by_line=true: search per-line (prevents [[:space:]] matching \n).
/// line_by_line=false: whole-buffer (for (?s)/(?m)/explicit \n patterns).
#[inline]
fn search_regex(buf: &[u8], re: &BytesRegex, line_by_line: bool) -> Vec<(usize, String)> {
    if line_by_line {
        let mut results = Vec::new();
        let mut line_num: usize = 1;
        let mut pos = 0;
        while pos <= buf.len() {
            let end = memchr::memchr(b'\n', &buf[pos..])
                .map(|p| pos + p)
                .unwrap_or(buf.len());
            let line = &buf[pos..end];
            if re.is_match(line) {
                results.push((
                    line_num,
                    String::from_utf8_lossy(strip_trailing_cr(line)).into_owned(),
                ));
            }
            line_num += 1;
            pos = end + 1;
        }
        return results;
    }
    let mut results = Vec::new();
    let mut last_line_start = usize::MAX;
    let mut line_num: usize = 1;
    let mut counted_to: usize = 0;
    for m in re.find_iter(buf) {
        let start = m.start();
        line_num += memchr::memchr_iter(b'\n', &buf[counted_to..start]).count();
        counted_to = start;
        let (line_start, line_end) = line_bounds(buf, start);
        if line_start != last_line_start {
            let line =
                String::from_utf8_lossy(strip_trailing_cr(&buf[line_start..line_end])).into_owned();
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

/// Check if buffer looks binary (null byte in first 512 bytes).
#[inline]
pub(crate) fn is_binary(buf: &[u8]) -> bool {
    let check_len = buf.len().min(512);
    memchr::memchr(0, &buf[..check_len]).is_some()
}

/// Known text extensions — skip binary check for these (major perf win)
#[inline(always)]
pub(crate) fn is_known_text_ext(path: &std::path::Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(e) => matches!(
            e,
            "rs" | "ts"
                | "tsx"
                | "js"
                | "jsx"
                | "py"
                | "go"
                | "rb"
                | "java"
                | "c"
                | "h"
                | "cpp"
                | "cc"
                | "hpp"
                | "cs"
                | "swift"
                | "kt"
                | "scala"
                | "php"
                | "html"
                | "css"
                | "scss"
                | "less"
                | "json"
                | "toml"
                | "yaml"
                | "yml"
                | "md"
                | "txt"
                | "sh"
                | "bash"
                | "zsh"
                | "fish"
                | "vim"
                | "lua"
                | "r"
                | "sql"
                | "xml"
                | "svg"
                | "tf"
                | "hcl"
                | "nix"
                | "ex"
                | "exs"
                | "erl"
                | "hrl"
                | "ml"
                | "mli"
                | "hs"
                | "clj"
                | "cljs"
                | "lisp"
                | "el"
                | "dart"
                | "zig"
                | "v"
                | "proto"
                | "graphql"
                | "gql"
        ),
        None => false,
    }
}

/// Count-optimized search: line-level verify for indexed, full-file for fallback.
pub fn search_persistent_count(
    index: &crate::persist::PersistentIndex,
    pattern: &str,
    path_filter: Option<&Path>,
    hidden: bool,
    type_filter: &[String],
    include: &[String],
    exclude: &[String],
) -> Result<(usize, crate::persist::SearchTiming)> {
    let matcher = Matcher::new(pattern)?;
    let lbl = needs_line_by_line(pattern);
    let index_root = PathBuf::from(&index.meta.root_dir);
    let ov = build_overrides(&index_root, include, exclude)?;
    let (result, mut timing) = index.search_timed(pattern);

    let t_verify = std::time::Instant::now();
    let count = match result {
        SearchResult::LineHits(hits) => {
            if hits.is_empty() {
                timing.matches = 0;
                timing.strategy = "line-level".into();
                timing.density = 0.0;
                return Ok((0, timing));
            }
            let mut by_file: HashMap<&Path, Vec<u32>> = HashMap::new();
            for hit in &hits {
                if !hidden && is_hidden_path(hit.path, &index_root) {
                    continue;
                }
                if let Some(filter) = path_filter {
                    if !hit.path.starts_with(filter) {
                        continue;
                    }
                }
                if !passes_overrides(hit.path, ov.as_ref()) {
                    continue;
                }
                if !passes_type_filter(hit.path, type_filter) {
                    continue;
                }
                by_file.entry(hit.path).or_default().push(hit.byte_offset);
            }
            let total_line_hits = hits.len();
            let unique_files = by_file.len();
            let density = total_line_hits as f64 / unique_files.max(1) as f64;
            timing.density = density;

            let file_groups: Vec<_> = by_file.into_iter().collect();

            if density > 10.0 {
                timing.strategy = "file-level".into();
                file_groups
                    .par_iter()
                    .map(|(path, _offsets)| {
                        let mmap = match open_mmap(path) {
                            Some(m) => m,
                            None => return 0,
                        };
                        matcher.count_lines(&mmap, lbl)
                    })
                    .sum()
            } else {
                timing.strategy = "line-level".into();
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
                                matcher.has_match(&mmap[start..end], needs_line_by_line(pattern))
                            })
                            .count()
                    })
                    .sum()
            }
        }
        SearchResult::BitmapFiles(paths) => {
            let filtered: Vec<&Path> = paths
                .into_iter()
                .filter(|p| {
                    (hidden || !is_hidden_path(p, &index_root))
                        && path_filter.map_or(true, |f| p.starts_with(f))
                        && passes_overrides(p, ov.as_ref())
                        && passes_type_filter(p, type_filter)
                })
                .collect();
            count_file_level(&matcher, &filtered, &mut timing, "bitmap-only", lbl)
        }
        SearchResult::AllFiles(paths) => {
            let filtered: Vec<&Path> = paths
                .into_iter()
                .filter(|p| {
                    (hidden || !is_hidden_path(p, &index_root))
                        && path_filter.map_or(true, |f| p.starts_with(f))
                        && passes_overrides(p, ov.as_ref())
                        && passes_type_filter(p, type_filter)
                })
                .collect();
            count_file_level(
                &matcher,
                &filtered,
                &mut timing,
                "file-level (fallback)",
                lbl,
            )
        }
    };

    timing.verify_ms = t_verify.elapsed().as_secs_f64() * 1000.0;
    timing.matches = count;
    Ok((count, timing))
}

fn count_file_level(
    matcher: &Matcher,
    paths: &[&Path],
    timing: &mut crate::persist::SearchTiming,
    strategy: &str,
    lbl: bool,
) -> usize {
    if paths.is_empty() {
        timing.matches = 0;
        timing.strategy = strategy.into();
        timing.density = 0.0;
        return 0;
    }
    timing.strategy = strategy.into();
    timing.density = 0.0;
    paths
        .par_iter()
        .map(|path| {
            let mmap = match open_mmap(path) {
                Some(m) => m,
                None => return 0,
            };
            matcher.count_lines(&mmap, lbl)
        })
        .sum()
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

/// Search with detailed timing breakdown. Uses line-level verify when index provides line hits.
pub fn search_persistent_timed(
    index: &crate::persist::PersistentIndex,
    pattern: &str,
    path_filter: Option<&Path>,
    hidden: bool,
    type_filter: &[String],
    include: &[String],
    exclude: &[String],
) -> Result<(Vec<Match>, crate::persist::SearchTiming)> {
    let matcher = Matcher::new(pattern)?;
    let lbl = needs_line_by_line(pattern);
    let index_root = PathBuf::from(&index.meta.root_dir);
    let ov = build_overrides(&index_root, include, exclude)?;
    let (result, mut timing) = index.search_timed(pattern);

    let t_verify = std::time::Instant::now();
    let matches: Vec<Match> = match result {
        SearchResult::LineHits(hits) => {
            if hits.is_empty() {
                timing.matches = 0;
                timing.strategy = "line-level".into();
                timing.density = 0.0;
                return Ok((Vec::new(), timing));
            }

            // (The old 4-byte line-prefix pre-filter was removed: the prefix held
            // the line's first 4 bytes, not the match position, so it dropped
            // mid-line and indented matches. The line-level candidate already
            // narrows to the lines containing every trigram; verify reads only
            // those lines.)

            // Group by file path for efficient mmap (one mmap per file)
            let mut by_file: HashMap<&Path, Vec<(u32, u32)>> = HashMap::new();
            for hit in &hits {
                if !hidden && is_hidden_path(hit.path, &index_root) {
                    continue;
                }
                if let Some(filter) = path_filter {
                    if !hit.path.starts_with(filter) {
                        continue;
                    }
                }
                if !passes_overrides(hit.path, ov.as_ref()) {
                    continue;
                }
                if !passes_type_filter(hit.path, type_filter) {
                    continue;
                }
                by_file
                    .entry(hit.path)
                    .or_default()
                    .push((hit.line_no, hit.byte_offset));
            }
            let total_line_hits = hits.len();
            let unique_files = by_file.len();
            let density = total_line_hits as f64 / unique_files.max(1) as f64;
            timing.density = density;

            let file_groups: Vec<_> = by_file.into_iter().collect();

            if density > 10.0 {
                // High density: file-level verify (read whole file once)
                timing.strategy = "file-level".into();
                file_groups
                    .par_iter()
                    .flat_map(|(path, _lines)| {
                        let mmap = match open_mmap(path) {
                            Some(m) => m,
                            None => return Vec::new(),
                        };
                        let hits = matcher.search_buffer(&mmap, lbl);
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
            } else {
                // Low density: line-level verify (read only candidate lines)
                // On macOS, try Metal GPU pre-filter first for literal patterns
                // (only when FGR_METAL=1 is set).
                #[cfg(target_os = "macos")]
                let use_metal = std::env::var("FGR_METAL")
                    .map(|v| v == "1")
                    .unwrap_or(false);

                #[cfg(target_os = "macos")]
                let metal_literal: Option<Vec<u8>> = if use_metal {
                    extract_longest_literal(pattern)
                } else {
                    None
                };

                #[cfg(target_os = "macos")]
                let metal_verifier = if use_metal {
                    metal_literal.as_ref().and_then(|_| global_verifier())
                } else {
                    None
                };
                #[cfg(not(target_os = "macos"))]
                let metal_verifier: Option<&()> = None;

                if metal_verifier.is_some() {
                    timing.strategy = "line-level (metal)".into();
                } else {
                    timing.strategy = "line-level".into();
                }

                file_groups
                    .par_iter()
                    .flat_map(|(path, lines)| {
                        let mmap = match open_mmap(path) {
                            Some(m) => m,
                            None => return Vec::new(),
                        };
                        let path_buf = path.to_path_buf();

                        // --- Extract all candidate line bytes for this file ---
                        let mut line_data: Vec<u8> = Vec::new();
                        let mut slices: Vec<(u32, u32)> = Vec::new(); // (offset_in_line_data, len)
                        let mut byte_offsets: Vec<(u32, usize, usize)> = Vec::new(); // (line_no, start, end)

                        for &(line_no, byte_offset) in lines {
                            let start = byte_offset as usize;
                            if start >= mmap.len() {
                                continue;
                            }
                            let end = memchr::memchr(b'\n', &mmap[start..])
                                .map(|p| start + p)
                                .unwrap_or(mmap.len());
                            let off = line_data.len() as u32;
                            let len = (end - start) as u32;
                            line_data.extend_from_slice(&mmap[start..end]);
                            slices.push((off, len));
                            byte_offsets.push((line_no, start, end));
                        }

                        // --- Metal GPU pre-filter (macOS, literal patterns only) ---
                        #[cfg(target_os = "macos")]
                        let gpu_mask: Option<Vec<bool>> = {
                            if let (Some(lit), Some(ver)) = (&metal_literal, global_verifier()) {
                                Some(ver.filter(&line_data, &slices, lit))
                            } else {
                                None
                            }
                        };
                        #[cfg(not(target_os = "macos"))]
                        let gpu_mask: Option<Vec<bool>> = None;

                        // --- Final verify (regex on GPU-passed candidates) ---
                        let mut file_matches = Vec::new();
                        for (i, &(line_no, start, end)) in byte_offsets.iter().enumerate() {
                            // Skip if GPU said no
                            if let Some(ref mask) = gpu_mask {
                                if i < mask.len() && !mask[i] {
                                    continue;
                                }
                            }
                            let line_bytes = &mmap[start..end];
                            if matcher.has_match(line_bytes, needs_line_by_line(pattern)) {
                                let line = String::from_utf8_lossy(strip_trailing_cr(line_bytes))
                                    .into_owned();
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
        }
        SearchResult::BitmapFiles(paths) => {
            let filtered: Vec<&Path> = paths
                .into_iter()
                .filter(|p| {
                    (hidden || !is_hidden_path(p, &index_root))
                        && path_filter.map_or(true, |f| p.starts_with(f))
                        && passes_overrides(p, ov.as_ref())
                        && passes_type_filter(p, type_filter)
                })
                .collect();
            verify_file_level(&matcher, &filtered, &mut timing, "bitmap-only", lbl)
        }
        SearchResult::AllFiles(paths) => {
            let filtered: Vec<&Path> = paths
                .into_iter()
                .filter(|p| {
                    (hidden || !is_hidden_path(p, &index_root))
                        && path_filter.map_or(true, |f| p.starts_with(f))
                        && passes_overrides(p, ov.as_ref())
                        && passes_type_filter(p, type_filter)
                })
                .collect();
            verify_file_level(
                &matcher,
                &filtered,
                &mut timing,
                "file-level (fallback)",
                lbl,
            )
        }
    };

    timing.verify_ms = t_verify.elapsed().as_secs_f64() * 1000.0;
    timing.matches = matches.len();
    Ok((matches, timing))
}

/// Fast full scan — optimized hot path:
/// - Raw bytes (no UTF-8 validation)
fn verify_file_level(
    matcher: &Matcher,
    paths: &[&Path],
    timing: &mut crate::persist::SearchTiming,
    strategy: &str,
    lbl: bool,
) -> Vec<Match> {
    if paths.is_empty() {
        timing.matches = 0;
        timing.strategy = strategy.into();
        timing.density = 0.0;
        return Vec::new();
    }
    timing.strategy = strategy.into();
    timing.density = 0.0;
    paths
        .par_iter()
        .flat_map(|path| {
            let mmap = match open_mmap(path) {
                Some(m) => m,
                None => return Vec::new(),
            };
            let hits = matcher.search_buffer(&mmap, lbl);
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

/// - SIMD memmem for literal patterns
/// - Parallel file walking + searching
/// - Buffer reuse per thread (no allocation per file)
/// - Line numbers computed only for actual matches
pub fn search_full_scan(
    root: &Path,
    pattern: &str,
    no_ignore: bool,
    hidden: bool,
    type_filter: &[String],
    include: &[String],
    exclude: &[String],
    invert: bool,
) -> Result<Vec<Match>> {
    let matcher = Matcher::new(pattern)?;
    let collector: Mutex<Vec<Vec<Match>>> = Mutex::new(Vec::new());

    let mut wb = ignore::WalkBuilder::new(root);
    wb.git_ignore(!no_ignore)
        .hidden(!hidden)
        .threads(num_cpus());
    if let Some(ov) = build_overrides(root, include, exclude)? {
        wb.overrides(ov);
    }
    let walker = wb.build_parallel();

    let type_filter_owned: Vec<String> = type_filter.to_vec();

    walker.run(|| {
        let matcher = &matcher;
        let collector = &collector;
        let type_filter = type_filter_owned.as_slice();
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

            if !passes_type_filter(path, type_filter) {
                return ignore::WalkState::Continue;
            }

            // Use metadata from the walk entry (already stat'd, no extra syscall)
            let flen = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if flen == 0 {
                return ignore::WalkState::Continue;
            }

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

            let lbl = needs_line_by_line(pattern);
            let hits = if invert {
                matcher.scan_non_match_lines(buf, lbl)
            } else {
                matcher.search_buffer(buf, lbl)
            };
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
    hidden: bool,
    type_filter: &[String],
    include: &[String],
    exclude: &[String],
    invert: bool,
) -> Result<usize> {
    let matcher = Matcher::new(pattern)?;
    let total_count = std::sync::atomic::AtomicUsize::new(0);

    let mut wb = ignore::WalkBuilder::new(root);
    wb.git_ignore(!no_ignore)
        .hidden(!hidden)
        .threads(num_cpus());
    if let Some(ov) = build_overrides(root, include, exclude)? {
        wb.overrides(ov);
    }
    let walker = wb.build_parallel();

    let type_filter_owned: Vec<String> = type_filter.to_vec();

    walker.run(|| {
        let matcher = &matcher;
        let total_count = &total_count;
        let type_filter = type_filter_owned.as_slice();
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

            if !passes_type_filter(path, type_filter) {
                return ignore::WalkState::Continue;
            }

            read_buf.clear();
            let file = match std::fs::File::open(path) {
                Ok(f) => f,
                Err(_) => return ignore::WalkState::Continue,
            };
            let flen = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if flen == 0 {
                return ignore::WalkState::Continue;
            }

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

            let lbl = needs_line_by_line(pattern);
            let count = if invert {
                matcher.count_non_match_lines(buf, lbl)
            } else {
                matcher.count_lines(buf, lbl)
            };
            if count > 0 {
                total_count.fetch_add(count, std::sync::atomic::Ordering::Relaxed);
            }

            ignore::WalkState::Continue
        })
    });

    Ok(total_count.load(std::sync::atomic::Ordering::Relaxed))
}

/// Existence-only full scan for the `-q`/quiet flag. Returns `true` as soon as
/// any file matches and stops the entire parallel walk (`WalkState::Quit`) — no
/// need to open the rest of the tree once the yes/no answer is known. On a corpus
/// where the open() cost dominates (e.g. NTFS behind an EDR filter stack), this
/// turns a full-tree scan into "open files until the first hit".
pub fn search_full_scan_any(
    root: &Path,
    pattern: &str,
    no_ignore: bool,
    hidden: bool,
    type_filter: &[String],
    include: &[String],
    exclude: &[String],
    invert: bool,
) -> Result<bool> {
    let matcher = Matcher::new(pattern)?;
    let found = std::sync::atomic::AtomicBool::new(false);

    let mut wb = ignore::WalkBuilder::new(root);
    wb.git_ignore(!no_ignore)
        .hidden(!hidden)
        .threads(num_cpus());
    if let Some(ov) = build_overrides(root, include, exclude)? {
        wb.overrides(ov);
    }
    let walker = wb.build_parallel();

    let type_filter_owned: Vec<String> = type_filter.to_vec();

    walker.run(|| {
        let matcher = &matcher;
        let found = &found;
        let type_filter = type_filter_owned.as_slice();
        let mut read_buf: Vec<u8> = Vec::with_capacity(64 * 1024);

        Box::new(move |entry| {
            if found.load(std::sync::atomic::Ordering::Relaxed) {
                return ignore::WalkState::Quit;
            }

            let entry = match entry {
                Ok(e) => e,
                Err(_) => return ignore::WalkState::Continue,
            };

            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return ignore::WalkState::Continue;
            }

            let path = entry.path();

            if !passes_type_filter(path, type_filter) {
                return ignore::WalkState::Continue;
            }

            read_buf.clear();
            let file = match std::fs::File::open(path) {
                Ok(f) => f,
                Err(_) => return ignore::WalkState::Continue,
            };
            let flen = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if flen == 0 {
                return ignore::WalkState::Continue;
            }

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

            let lbl = needs_line_by_line(pattern);
            let hit = if invert {
                matcher.count_non_match_lines(buf, lbl) > 0
            } else {
                matcher.has_match(buf, lbl)
            };
            if hit {
                found.store(true, std::sync::atomic::Ordering::Relaxed);
                return ignore::WalkState::Quit;
            }

            ignore::WalkState::Continue
        })
    });

    Ok(found.load(std::sync::atomic::Ordering::Relaxed))
}

// `search_full_scan_streaming` (the inline-format streaming path) was
// removed in the render-pipeline refactor. Its responsibilities now belong
// to `crate::render::search_full_scan_render` with `Dispatch::Streaming`,
// which inherits context, heading, and colour for free.

pub(crate) fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}
