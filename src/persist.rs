use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use ignore::WalkBuilder;
use memmap2::Mmap;
use rayon::prelude::*;

use roaring::RoaringBitmap;

use crate::index::{Posting, SparseIndex};
use crate::sparse::BigramFreq;
use crate::trigram;

// --- Zero-copy read helpers ---

#[inline(always)]
fn read_u32_le(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}

#[inline(always)]
fn read_u64_le(data: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        data[off],
        data[off + 1],
        data[off + 2],
        data[off + 3],
        data[off + 4],
        data[off + 5],
        data[off + 6],
        data[off + 7],
    ])
}

/// Sorted merge intersection of two sorted line-posting slices.
/// Intersects on (doc_id, line_no), keeps byte_offset and prefix from `a`.
fn sorted_intersect_lines(
    a: &[(u32, u32, u32, [u8; 4])],
    b: &[(u32, u32, u32, [u8; 4])],
) -> Vec<(u32, u32, u32, [u8; 4])> {
    let mut result = Vec::with_capacity(a.len().min(b.len()));
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let ka = (a[i].0, a[i].1);
        let kb = (b[j].0, b[j].1);
        match ka.cmp(&kb) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                result.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    result
}

/// Merge two sorted line-posting slices into a new sorted Vec (union).
fn merge_sorted_lines(
    a: &[(u32, u32, u32, [u8; 4])],
    b: &[(u32, u32, u32, [u8; 4])],
) -> Vec<(u32, u32, u32, [u8; 4])> {
    let mut result = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let ka = (a[i].0, a[i].1);
        let kb = (b[j].0, b[j].1);
        match ka.cmp(&kb) {
            std::cmp::Ordering::Less => {
                result.push(a[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                result.push(b[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                result.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    result.extend_from_slice(&a[i..]);
    result.extend_from_slice(&b[j..]);
    result
}

// --- Timing ---

pub struct SearchTiming {
    pub covering_ngrams_ms: f64,
    pub lookup_ms: f64,
    pub bitmap_intersect_ms: f64,
    pub verify_ms: f64,
    pub candidates: usize,
    pub matches: usize,
    pub ngrams_queried: usize,
    /// Verify strategy used: "line-level", "file-level", or "file-level (fallback)"
    pub strategy: String,
    /// Match density (lines per file) that drove the strategy decision
    pub density: f64,
    /// Number of candidates eliminated by the 4-byte line prefix filter (no I/O)
    pub prefix_filtered: usize,
}

// --- Line-level search result ---

pub struct LineHit<'a> {
    pub path: &'a Path,
    pub line_no: u32,
    pub byte_offset: u32,
    pub line_prefix: [u8; 4],
}

/// Search result from the index: either precise line-level hits or fallback to all files.
pub enum SearchResult<'a> {
    /// Line-level candidates from trigram intersection
    LineHits(Vec<LineHit<'a>>),
    /// Bitmap-only candidates: selective bitmap AND produced few files, skip posting load
    BitmapFiles(Vec<&'a Path>),
    /// Fallback: all files (pattern too short for trigrams)
    AllFiles(Vec<&'a Path>),
}

// --- Index structures ---

#[derive(serde::Serialize, serde::Deserialize)]
pub struct IndexMeta {
    pub version: u32,
    pub num_docs: usize,
    pub num_ngrams: usize,
    pub root_dir: String,
    pub built_at: String,
    pub file_mtimes: HashMap<String, u64>,
    /// Directory mtimes — used for fast stale detection without walking the tree.
    /// A changed dir mtime means files were added, deleted, or renamed in that dir.
    #[serde(default)]
    pub dir_mtimes: HashMap<String, u64>,
    /// Number of docs in the main (non-delta) index. Set on full build.
    #[serde(default)]
    pub main_num_docs: Option<usize>,
    /// Corpus-adaptive bigram frequency table, base64-encoded.
    #[serde(default)]
    pub bigram_freq_b64: Option<String>,
}

#[derive(Clone)]
pub struct LookupEntry {
    pub hash: u32,
    pub offset: u64,
    pub len: u32,
}

const LOOKUP_ENTRY_SIZE: usize = 4 + 8 + 4; // 16 bytes
const POSTING_ENTRY_SIZE: usize = 16; // doc_id(u32) + line_no(u32) + byte_offset(u32) + prefix([u8;4])

pub struct PersistentIndex {
    /// Main lookup table — memory-mapped for zero-copy binary search
    pub lookup_mmap: Mmap,
    pub lookup_count: usize,
    /// Main postings — memory-mapped
    pub postings_mmap: Mmap,
    /// Roaring bitmap file — memory-mapped (one serialized bitmap per trigram)
    pub bitmap_mmap: Option<Mmap>,
    /// Bitmap lookup table — memory-mapped (hash → offset/len into bitmap_mmap)
    pub bitmap_lookup_mmap: Option<Mmap>,
    pub bitmap_lookup_count: usize,
    /// Doc IDs: mmap'd flat buffer + offset table for zero-alloc load
    pub docids_mmap: Mmap,
    pub docid_offsets: Vec<(u32, u16)>, // (offset, length) for main docs
    pub delta_doc_ids: Vec<PathBuf>,    // delta docs (small count)
    pub meta: IndexMeta,
    // Overlay for incremental updates
    pub deleted_docs: HashSet<u32>,
    pub delta_lookup: Vec<LookupEntry>,
    pub delta_postings: Vec<u8>,
    pub main_num_docs: usize,
    pub freq: BigramFreq,
}

impl PersistentIndex {
    /// Resolve a doc_id to its file path (zero-alloc for main docs).
    #[inline]
    pub fn doc_path(&self, id: u32) -> Option<&Path> {
        let id = id as usize;
        if id < self.docid_offsets.len() {
            let (off, len) = self.docid_offsets[id];
            let end = off as usize + len as usize;
            if end <= self.docids_mmap.len() {
                let bytes = &self.docids_mmap[off as usize..end];
                std::str::from_utf8(bytes).ok().map(Path::new)
            } else {
                None
            }
        } else {
            let delta_idx = id - self.docid_offsets.len();
            self.delta_doc_ids.get(delta_idx).map(|p| p.as_path())
        }
    }

    /// Total number of docs (main + delta).
    pub fn num_docs(&self) -> usize {
        self.docid_offsets.len() + self.delta_doc_ids.len()
    }

    // --- Low-level lookup methods (zero-copy) ---

    /// Binary search in mmap'd main lookup table.
    #[inline]
    fn find_in_main_lookup(&self, hash: u32) -> Option<(u64, u32)> {
        let data = &*self.lookup_mmap;
        let mut lo = 0usize;
        let mut hi = self.lookup_count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let h = read_u32_le(data, mid * LOOKUP_ENTRY_SIZE);
            match h.cmp(&hash) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let off = read_u64_le(data, mid * LOOKUP_ENTRY_SIZE + 4);
                    let len = read_u32_le(data, mid * LOOKUP_ENTRY_SIZE + 12);
                    return Some((off, len));
                }
            }
        }
        None
    }

    /// Get raw posting bytes for a trigram hash from main index.
    #[inline]
    fn main_posting_data(&self, hash: u32) -> Option<&[u8]> {
        let (offset, len) = self.find_in_main_lookup(hash)?;
        let start = offset as usize;
        let end = start + len as usize;
        if end <= self.postings_mmap.len() && (end - start) % POSTING_ENTRY_SIZE == 0 {
            Some(&self.postings_mmap[start..end])
        } else {
            None
        }
    }

    /// Get raw posting bytes for a trigram hash from delta index.
    #[inline]
    fn delta_posting_data(&self, hash: u32) -> Option<&[u8]> {
        if self.delta_lookup.is_empty() {
            return None;
        }
        let idx = self
            .delta_lookup
            .binary_search_by_key(&hash, |e| e.hash)
            .ok()?;
        let entry = &self.delta_lookup[idx];
        let start = entry.offset as usize;
        let end = start + entry.len as usize;
        if end <= self.delta_postings.len() && (end - start) % POSTING_ENTRY_SIZE == 0 {
            Some(&self.delta_postings[start..end])
        } else {
            None
        }
    }

    // --- Roaring bitmap lookup (Tier 1) ---

    /// Binary search in mmap'd bitmap lookup table.
    #[inline]
    fn find_in_bitmap_lookup(&self, hash: u32) -> Option<(u64, u32)> {
        let data = self.bitmap_lookup_mmap.as_ref()?;
        let count = self.bitmap_lookup_count;
        let mut lo = 0usize;
        let mut hi = count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let h = read_u32_le(data, mid * LOOKUP_ENTRY_SIZE);
            match h.cmp(&hash) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let off = read_u64_le(data, mid * LOOKUP_ENTRY_SIZE + 4);
                    let len = read_u32_le(data, mid * LOOKUP_ENTRY_SIZE + 12);
                    return Some((off, len));
                }
            }
        }
        None
    }

    /// Deserialize a RoaringBitmap for a trigram hash from the bitmap mmap.
    /// Uses unchecked deserialization for speed (data was written by us).
    #[inline]
    fn lookup_bitmap(&self, hash: u32) -> Option<RoaringBitmap> {
        let (offset, len) = self.find_in_bitmap_lookup(hash)?;
        let bm_data = self.bitmap_mmap.as_ref()?;
        let start = offset as usize;
        let end = start + len as usize;
        if end > bm_data.len() {
            return None;
        }
        RoaringBitmap::deserialize_unchecked_from(&bm_data[start..end]).ok()
    }

    /// Extract sorted line postings from raw posting bytes, excluding deleted docs.
    fn extract_line_postings(&self, data: &[u8]) -> Vec<(u32, u32, u32, [u8; 4])> {
        let num = data.len() / POSTING_ENTRY_SIZE;
        let mut postings = Vec::with_capacity(num);
        for i in 0..num {
            let base = i * POSTING_ENTRY_SIZE;
            let doc_id = read_u32_le(data, base);
            if !self.deleted_docs.contains(&doc_id) {
                let line_no = read_u32_le(data, base + 4);
                let byte_offset = read_u32_le(data, base + 8);
                let mut prefix = [0u8; 4];
                prefix.copy_from_slice(&data[base + 12..base + 16]);
                postings.push((doc_id, line_no, byte_offset, prefix));
            }
        }
        postings
    }

    /// Extract sorted line postings, filtering to only doc_ids in the bitmap.
    fn extract_line_postings_filtered(
        &self,
        data: &[u8],
        filter: &RoaringBitmap,
    ) -> Vec<(u32, u32, u32, [u8; 4])> {
        let num = data.len() / POSTING_ENTRY_SIZE;
        let mut postings = Vec::with_capacity(num / 4); // expect significant filtering
        for i in 0..num {
            let base = i * POSTING_ENTRY_SIZE;
            let doc_id = read_u32_le(data, base);
            if filter.contains(doc_id) && !self.deleted_docs.contains(&doc_id) {
                let line_no = read_u32_le(data, base + 4);
                let byte_offset = read_u32_le(data, base + 8);
                let mut prefix = [0u8; 4];
                prefix.copy_from_slice(&data[base + 12..base + 16]);
                postings.push((doc_id, line_no, byte_offset, prefix));
            }
        }
        postings
    }

    /// Get merged (main + delta) sorted line postings for a trigram hash,
    /// filtered to only doc_ids in the candidate bitmap.
    fn trigram_line_postings_filtered(
        &self,
        hash: u32,
        filter: &RoaringBitmap,
    ) -> Option<Vec<(u32, u32, u32, [u8; 4])>> {
        let main = self.main_posting_data(hash);
        let delta = self.delta_posting_data(hash);

        if main.is_none() && delta.is_none() {
            return None;
        }

        let main_postings = main
            .map(|d| self.extract_line_postings_filtered(d, filter))
            .unwrap_or_default();
        let delta_postings = delta
            .map(|d| self.extract_line_postings_filtered(d, filter))
            .unwrap_or_default();

        let postings = if delta_postings.is_empty() {
            main_postings
        } else if main_postings.is_empty() {
            delta_postings
        } else {
            merge_sorted_lines(&main_postings, &delta_postings)
        };

        if postings.is_empty() {
            None
        } else {
            Some(postings)
        }
    }

    /// Get merged (main + delta) sorted line postings for a trigram hash.
    fn trigram_line_postings(&self, hash: u32) -> Option<Vec<(u32, u32, u32, [u8; 4])>> {
        let main = self.main_posting_data(hash);
        let delta = self.delta_posting_data(hash);

        if main.is_none() && delta.is_none() {
            return None;
        }

        let main_postings = main
            .map(|d| self.extract_line_postings(d))
            .unwrap_or_default();
        let delta_postings = delta
            .map(|d| self.extract_line_postings(d))
            .unwrap_or_default();

        let postings = if delta_postings.is_empty() {
            main_postings
        } else if main_postings.is_empty() {
            delta_postings
        } else {
            merge_sorted_lines(&main_postings, &delta_postings)
        };

        if postings.is_empty() {
            None
        } else {
            Some(postings)
        }
    }

    // --- Search methods ---

    pub fn search(&self, pattern: &str) -> Vec<&Path> {
        match self.search_timed(pattern).0 {
            SearchResult::LineHits(hits) => {
                let mut paths: Vec<&Path> = hits.iter().map(|h| h.path).collect();
                paths.sort();
                paths.dedup();
                paths
            }
            SearchResult::BitmapFiles(paths) | SearchResult::AllFiles(paths) => paths,
        }
    }

    pub fn search_timed(&self, pattern: &str) -> (SearchResult<'_>, SearchTiming) {
        let t0 = Instant::now();

        let alternatives = trigram::decompose_pattern(pattern);
        let covering_ngrams_ms = t0.elapsed().as_secs_f64() * 1000.0;

        if alternatives.is_empty() || alternatives.iter().all(|a| a.is_empty()) {
            let docs = self.live_doc_ids();
            let n = docs.len();
            return (
                SearchResult::AllFiles(docs),
                SearchTiming {
                    covering_ngrams_ms,
                    lookup_ms: 0.0,
                    bitmap_intersect_ms: 0.0,
                    verify_ms: 0.0,
                    candidates: n,
                    matches: 0,
                    ngrams_queried: 0,
                    strategy: String::new(),
                    density: 0.0,
                    prefix_filtered: 0,
                },
            );
        }

        let mut ngrams_queried = 0usize;
        let mut result_lines: Vec<(u32, u32, u32, [u8; 4])> = Vec::new();
        let mut bitmap_dur = Duration::ZERO;
        let mut intersect_dur = Duration::ZERO;
        let has_bitmaps = self.bitmap_mmap.is_some();

        for alt_trigrams in &alternatives {
            if alt_trigrams.is_empty() {
                let docs = self.live_doc_ids();
                let n = docs.len();
                return (
                    SearchResult::AllFiles(docs),
                    SearchTiming {
                        covering_ngrams_ms,
                        lookup_ms: 0.0,
                        bitmap_intersect_ms: 0.0,
                        verify_ms: 0.0,
                        candidates: n,
                        matches: 0,
                        ngrams_queried: 0,
                        strategy: String::new(),
                        density: 0.0,
                        prefix_filtered: 0,
                    },
                );
            }

            ngrams_queried += alt_trigrams.len();
            let hashes: Vec<u32> = alt_trigrams.iter().map(|tri| crc32fast::hash(tri)).collect();

            if has_bitmaps {
                // === Two-tier search: Roaring Bitmap → filtered postings ===

                // Phase 1: Bitmap intersection (parallel load, then serial AND)
                let t_bitmap = Instant::now();

                // Parallel deserialization of all bitmaps
                let mut bitmaps: Vec<Option<RoaringBitmap>> = hashes
                    .par_iter()
                    .map(|&h| self.lookup_bitmap(h))
                    .collect();

                // If any trigram is missing from bitmaps, fall back to full posting list search
                if bitmaps.iter().any(|b| b.is_none()) {
                    bitmap_dur += t_bitmap.elapsed();
                    // Fallback: load postings directly without bitmap pre-filter
                    let t_fallback = Instant::now();
                    let posting_lists: Vec<Option<Vec<(u32, u32, u32, [u8; 4])>>> = hashes
                        .par_iter()
                        .map(|&h| self.trigram_line_postings(h))
                        .collect();
                    if posting_lists.iter().any(|p| p.is_none()) {
                        intersect_dur += t_fallback.elapsed();
                        continue;
                    }
                    let mut posting_lists: Vec<Vec<(u32, u32, u32, [u8; 4])>> =
                        posting_lists.into_iter().map(|p| p.unwrap()).collect();
                    posting_lists.sort_by_key(|v| v.len());
                    let mut candidates = posting_lists.swap_remove(0);
                    for other in &posting_lists {
                        if candidates.is_empty() { break; }
                        candidates = sorted_intersect_lines(&candidates, other);
                    }
                    intersect_dur += t_fallback.elapsed();
                    result_lines.extend(candidates);
                    continue;
                }

                // Sort by cardinality (smallest first) for faster AND
                bitmaps.sort_by_key(|b| b.as_ref().map_or(0, |bm| bm.len()));

                // Sequential AND with early termination
                let mut candidate_docs = bitmaps.swap_remove(0).unwrap();
                for bm in bitmaps.into_iter().flatten() {
                    candidate_docs &= bm;
                    if candidate_docs.is_empty() {
                        break;
                    }
                }

                if candidate_docs.is_empty() {
                    bitmap_dur += t_bitmap.elapsed();
                    continue;
                }

                let bm_card = candidate_docs.len() as usize;
                bitmap_dur += t_bitmap.elapsed();

                // Fast path: if bitmap is very selective (< 0.7% of corpus),
                // skip posting load entirely — just verify the candidate files.
                let bitmap_threshold = (self.num_docs() as f64 * 0.007) as usize;
                if bm_card <= bitmap_threshold {
                    let paths: Vec<&Path> = candidate_docs
                        .iter()
                        .filter_map(|id| self.doc_path(id))
                        .collect();
                    let n = paths.len();
                    return (
                        SearchResult::BitmapFiles(paths),
                        SearchTiming {
                            covering_ngrams_ms,
                            lookup_ms: bitmap_dur.as_secs_f64() * 1000.0,
                            bitmap_intersect_ms: 0.0,
                            verify_ms: 0.0,
                            candidates: n,
                            matches: 0,
                            ngrams_queried,
                            strategy: String::new(),
                            density: 0.0,
                            prefix_filtered: 0,
                        },
                    );
                }

                // Phase 2: Load line postings (filtered by bitmap if selective), then intersect
                let t_intersect = Instant::now();
                let bm_selectivity =
                    bm_card as f64 / self.num_docs().max(1) as f64;

                let posting_lists: Vec<Option<Vec<(u32, u32, u32, [u8; 4])>>> = if bm_selectivity < 0.5 {
                    // Bitmap is selective — use filtered extraction
                    hashes
                        .par_iter()
                        .map(|&h| self.trigram_line_postings_filtered(h, &candidate_docs))
                        .collect()
                } else {
                    // Bitmap isn't selective — full extraction is faster
                    hashes
                        .par_iter()
                        .map(|&h| self.trigram_line_postings(h))
                        .collect()
                };

                if posting_lists.iter().any(|p| p.is_none()) {
                    intersect_dur += t_intersect.elapsed();
                    continue;
                }

                let mut posting_lists: Vec<Vec<(u32, u32, u32, [u8; 4])>> =
                    posting_lists.into_iter().map(|p| p.unwrap()).collect();
                posting_lists.sort_by_key(|v| v.len());

                let mut candidates = posting_lists.swap_remove(0);
                for other in &posting_lists {
                    if candidates.is_empty() {
                        break;
                    }
                    candidates = sorted_intersect_lines(&candidates, other);
                }
                intersect_dur += t_intersect.elapsed();
                result_lines.extend(candidates);
            } else {
                // === Fallback: original sorted merge approach (no bitmap files) ===

                let t_lookup = Instant::now();
                let posting_lists: Vec<Option<Vec<(u32, u32, u32, [u8; 4])>>> = hashes
                    .par_iter()
                    .map(|&h| self.trigram_line_postings(h))
                    .collect();
                bitmap_dur += t_lookup.elapsed();

                if posting_lists.iter().any(|p| p.is_none()) {
                    continue;
                }

                let t_intersect = Instant::now();
                let mut posting_lists: Vec<Vec<(u32, u32, u32, [u8; 4])>> =
                    posting_lists.into_iter().map(|p| p.unwrap()).collect();
                posting_lists.sort_by_key(|v| v.len());

                let mut candidates = posting_lists.swap_remove(0);
                for other in &posting_lists {
                    if candidates.is_empty() {
                        break;
                    }
                    candidates = sorted_intersect_lines(&candidates, other);
                }
                intersect_dur += t_intersect.elapsed();
                result_lines.extend(candidates);
            }
        }

        // Dedup result lines on (doc_id, line_no)
        result_lines.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        result_lines.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);

        let num_candidates = result_lines.len();
        let hits: Vec<LineHit<'_>> = result_lines
            .iter()
            .filter_map(|&(doc_id, line_no, byte_offset, line_prefix)| {
                self.doc_path(doc_id).map(|path| LineHit {
                    path,
                    line_no,
                    byte_offset,
                    line_prefix,
                })
            })
            .collect();

        (
            SearchResult::LineHits(hits),
            SearchTiming {
                covering_ngrams_ms,
                lookup_ms: bitmap_dur.as_secs_f64() * 1000.0,
                bitmap_intersect_ms: intersect_dur.as_secs_f64() * 1000.0,
                verify_ms: 0.0,
                candidates: num_candidates,
                matches: 0,
                ngrams_queried,
                strategy: String::new(),
                density: 0.0,
                prefix_filtered: 0,
            },
        )
    }

    /// Return paths for all non-deleted docs (fallback for short patterns).
    fn live_doc_ids(&self) -> Vec<&Path> {
        (0..self.num_docs() as u32)
            .filter(|id| !self.deleted_docs.contains(id))
            .filter_map(|id| self.doc_path(id))
            .collect()
    }

    pub fn is_stale(&self) -> bool {
        // Phase 1: Check directory mtimes (no walk needed).
        // Detects file additions, deletions, and renames.
        for (path_str, &stored_mtime) in &self.meta.dir_mtimes {
            let path = Path::new(path_str);
            match fs::metadata(path) {
                Ok(m) => {
                    if mtime_secs(&m) != stored_mtime {
                        return true;
                    }
                }
                Err(_) => return true, // directory was removed
            }
        }
        // Phase 2: Sample file mtimes to detect content modifications.
        for (path_str, &stored_mtime) in self.meta.file_mtimes.iter().take(100) {
            let path = Path::new(path_str);
            match fs::metadata(path) {
                Ok(m) => {
                    if mtime_secs(&m) != stored_mtime {
                        return true;
                    }
                }
                Err(_) => return true,
            }
        }
        false
    }
}

pub fn build(
    root: &Path,
    output: &Path,
    no_ignore: bool,
    type_filter: Option<&str>,
    verbose: bool,
) -> Result<()> {
    if verbose {
        eprintln!("Building index for {:?}...", root);
    }

    let index = SparseIndex::build_from_directory(root, no_ignore, type_filter, verbose)?;

    fs::create_dir_all(output).context("creating output directory")?;

    // Build corpus-adaptive bigram frequency table
    let freq = BigramFreq::from_corpus(&index.doc_ids, 5000);
    let freq_b64 = {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(freq.to_bytes())
    };

    // Collect file mtimes
    let mut file_mtimes = HashMap::new();
    for path in &index.doc_ids {
        if let Ok(m) = fs::metadata(path) {
            file_mtimes.insert(path.to_string_lossy().into_owned(), mtime_secs(&m));
        }
    }

    // Collect directory mtimes for fast stale detection
    let dir_mtimes = collect_dir_mtimes(root, no_ignore, Some(output));

    // Write postings and build lookup (12 bytes per entry: doc_id + line_no + byte_offset)
    let postings_path = output.join("ngrams.postings");
    let mut postings_file = BufWriter::new(File::create(&postings_path)?);
    let mut lookup_entries: Vec<(u32, u64, u32)> = Vec::new();
    let mut offset: u64 = 0;

    // Sort trigrams by hash for binary search
    let mut trigram_list: Vec<(&[u8; 3], &Vec<Posting>)> = index.ngrams.iter().collect();
    trigram_list.sort_by_key(|(k, _)| crc32fast::hash(*k));

    for (tri, postings) in &trigram_list {
        let mut buf = Vec::with_capacity(postings.len() * POSTING_ENTRY_SIZE);
        for &(doc_id, line_no, byte_offset, prefix) in *postings {
            buf.write_u32::<LittleEndian>(doc_id)?;
            buf.write_u32::<LittleEndian>(line_no)?;
            buf.write_u32::<LittleEndian>(byte_offset)?;
            buf.write_all(&prefix)?;
        }
        let len = buf.len() as u32;
        postings_file.write_all(&buf)?;
        lookup_entries.push((crc32fast::hash(*tri), offset, len));
        offset += len as u64;
    }
    postings_file.flush()?;

    // Write lookup table
    let lookup_path = output.join("ngrams.lookup");
    let mut lookup_file = BufWriter::new(File::create(&lookup_path)?);
    for (hash, off, len) in &lookup_entries {
        lookup_file.write_u32::<LittleEndian>(*hash)?;
        lookup_file.write_u64::<LittleEndian>(*off)?;
        lookup_file.write_u32::<LittleEndian>(*len)?;
    }
    lookup_file.flush()?;

    // Write Roaring Bitmaps (Tier 1: doc_id sets per trigram)
    let bitmaps_path = output.join("ngrams.bitmaps");
    let mut bitmaps_file = BufWriter::new(File::create(&bitmaps_path)?);
    let mut bitmap_lookup_entries: Vec<(u32, u64, u32)> = Vec::new();
    let mut bm_offset: u64 = 0;

    for (tri, postings) in &trigram_list {
        let mut bitmap = RoaringBitmap::new();
        for &(doc_id, _, _, _) in *postings {
            bitmap.insert(doc_id);
        }
        let mut bm_buf = Vec::new();
        bitmap.serialize_into(&mut bm_buf)?;
        let bm_len = bm_buf.len() as u32;
        bitmaps_file.write_all(&bm_buf)?;
        bitmap_lookup_entries.push((crc32fast::hash(*tri), bm_offset, bm_len));
        bm_offset += bm_len as u64;
    }
    bitmaps_file.flush()?;

    let bitmaps_lookup_path = output.join("ngrams.bitmaps.lookup");
    let mut bm_lookup_file = BufWriter::new(File::create(&bitmaps_lookup_path)?);
    for (hash, off, len) in &bitmap_lookup_entries {
        bm_lookup_file.write_u32::<LittleEndian>(*hash)?;
        bm_lookup_file.write_u64::<LittleEndian>(*off)?;
        bm_lookup_file.write_u32::<LittleEndian>(*len)?;
    }
    bm_lookup_file.flush()?;

    // Write docids
    let docids_path = output.join("docids.bin");
    let mut docids_file = BufWriter::new(File::create(&docids_path)?);
    for path in &index.doc_ids {
        let path_bytes = path.to_string_lossy();
        let bytes = path_bytes.as_bytes();
        docids_file.write_u16::<LittleEndian>(bytes.len() as u16)?;
        docids_file.write_all(bytes)?;
    }
    docids_file.flush()?;

    // Write meta
    let num_docs = index.doc_ids.len();
    let meta = IndexMeta {
        version: 3, // bump version for line-level index format
        num_docs,
        num_ngrams: index.ngrams.len(),
        root_dir: root.to_string_lossy().into_owned(),
        built_at: chrono_now(),
        file_mtimes,
        dir_mtimes,
        main_num_docs: Some(num_docs),
        bigram_freq_b64: Some(freq_b64),
    };
    let meta_path = output.join("meta.json");
    let meta_json = serde_json::to_string_pretty(&meta)?;
    fs::write(&meta_path, meta_json)?;

    // Clean up any delta files and stale lock from previous runs
    let _ = fs::remove_file(output.join("delta.postings"));
    let _ = fs::remove_file(output.join("delta.lookup"));
    let _ = fs::remove_file(output.join("delta.docids"));
    let _ = fs::remove_file(output.join("deleted.bin"));
    let _ = fs::remove_file(output.join("lock"));
    // Old bitmap files are overwritten by the new ones above

    if verbose {
        eprintln!(
            "Index built: {} docs, {} trigrams, postings {}KB",
            meta.num_docs,
            meta.num_ngrams,
            fs::metadata(&postings_path)?.len() / 1024
        );
    }

    Ok(())
}

pub fn load(index_path: &Path) -> Result<PersistentIndex> {
    let meta_path = index_path.join("meta.json");
    let meta_str = fs::read_to_string(&meta_path).context("reading meta.json")?;
    let meta: IndexMeta = serde_json::from_str(&meta_str).context("parsing meta.json")?;

    // Load main lookup via mmap (zero-copy binary search)
    let lookup_path = index_path.join("ngrams.lookup");
    let lookup_file = File::open(&lookup_path).context("opening ngrams.lookup")?;
    let lookup_mmap = unsafe { Mmap::map(&lookup_file)? };
    let lookup_count = lookup_mmap.len() / LOOKUP_ENTRY_SIZE;

    // Load main postings mmap
    let postings_path = index_path.join("ngrams.postings");
    let postings_file = File::open(&postings_path).context("opening ngrams.postings")?;
    let postings_mmap = unsafe { Mmap::map(&postings_file)? };

    // Load Roaring Bitmap files (optional — backward compatible with older indexes)
    let bitmaps_path = index_path.join("ngrams.bitmaps");
    let bitmaps_lookup_path = index_path.join("ngrams.bitmaps.lookup");
    let (bitmap_mmap, bitmap_lookup_mmap, bitmap_lookup_count) =
        if bitmaps_path.exists() && bitmaps_lookup_path.exists() {
            let bf = File::open(&bitmaps_path).context("opening ngrams.bitmaps")?;
            let bm = unsafe { Mmap::map(&bf)? };
            let blf = File::open(&bitmaps_lookup_path).context("opening ngrams.bitmaps.lookup")?;
            let blm = unsafe { Mmap::map(&blf)? };
            let count = blm.len() / LOOKUP_ENTRY_SIZE;
            (Some(bm), Some(blm), count)
        } else {
            (None, None, 0)
        };

    // Load main doc_ids via mmap (zero-alloc offset table)
    let docids_path = index_path.join("docids.bin");
    let docids_file = File::open(&docids_path).context("opening docids.bin")?;
    let docids_mmap = unsafe { Mmap::map(&docids_file)? };
    let mut docid_offsets = Vec::new();
    {
        let data = &*docids_mmap;
        let mut pos = 0usize;
        while pos + 2 <= data.len() {
            let len = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;
            if pos + len > data.len() {
                break;
            }
            docid_offsets.push((pos as u32, len as u16));
            pos += len;
        }
    }

    let main_num_docs = meta.main_num_docs.unwrap_or(docid_offsets.len());

    // Load deleted set (if exists)
    let deleted_path = index_path.join("deleted.bin");
    let deleted_docs = if deleted_path.exists() {
        let data = fs::read(&deleted_path)?;
        let mut set = HashSet::new();
        let mut cursor = std::io::Cursor::new(&data);
        while (cursor.position() as usize) + 4 <= data.len() {
            if let Ok(id) = cursor.read_u32::<LittleEndian>() {
                set.insert(id);
            }
        }
        set
    } else {
        HashSet::new()
    };

    // Load delta index (if exists)
    let delta_lookup_path = index_path.join("delta.lookup");
    let delta_postings_path = index_path.join("delta.postings");
    let delta_docids_path = index_path.join("delta.docids");
    let entry_size = 4 + 8 + 4;

    let (delta_lookup, delta_postings) = if delta_lookup_path.exists() {
        let ldata = fs::read(&delta_lookup_path)?;
        let num = ldata.len() / entry_size;
        let mut dlookup = Vec::with_capacity(num);
        let mut cursor = std::io::Cursor::new(&ldata);
        for _ in 0..num {
            let hash = cursor.read_u32::<LittleEndian>()?;
            let offset = cursor.read_u64::<LittleEndian>()?;
            let len = cursor.read_u32::<LittleEndian>()?;
            dlookup.push(LookupEntry { hash, offset, len });
        }
        let dpostings = fs::read(&delta_postings_path).unwrap_or_default();
        (dlookup, dpostings)
    } else {
        (Vec::new(), Vec::new())
    };

    // Load delta doc_ids (small count, keep as PathBuf)
    let mut delta_doc_ids = Vec::new();
    if delta_docids_path.exists() {
        let ddata = fs::read(&delta_docids_path)?;
        let mut cursor = std::io::Cursor::new(&ddata);
        while (cursor.position() as usize) < ddata.len() {
            let len = cursor.read_u16::<LittleEndian>()? as usize;
            let pos = cursor.position() as usize;
            if pos + len > ddata.len() {
                break;
            }
            let path_str = std::str::from_utf8(&ddata[pos..pos + len])?;
            delta_doc_ids.push(PathBuf::from(path_str));
            cursor.set_position((pos + len) as u64);
        }
    }

    // Load bigram frequency table from meta (or fall back to flat)
    let freq = if let Some(ref b64) = meta.bigram_freq_b64 {
        use base64::Engine;
        match base64::engine::general_purpose::STANDARD.decode(b64) {
            Ok(bytes) => BigramFreq::from_bytes(&bytes),
            Err(_) => BigramFreq::flat(),
        }
    } else {
        BigramFreq::flat()
    };

    Ok(PersistentIndex {
        lookup_mmap,
        lookup_count,
        postings_mmap,
        bitmap_mmap,
        bitmap_lookup_mmap,
        bitmap_lookup_count,
        docids_mmap,
        docid_offsets,
        delta_doc_ids,
        meta,
        deleted_docs,
        delta_lookup,
        delta_postings,
        main_num_docs,
        freq,
    })
}

pub struct UpdateStats {
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
    pub unchanged: usize,
    pub duration_ms: u64,
}

pub fn update_incremental(index_path: &Path, root: &Path, verbose: bool) -> Result<UpdateStats> {
    let start = Instant::now();

    // 1. Load meta.json — get saved file_mtimes
    let meta_path = index_path.join("meta.json");
    let meta_str = fs::read_to_string(&meta_path).context("reading meta.json")?;
    let meta: IndexMeta = serde_json::from_str(&meta_str).context("parsing meta.json")?;
    let saved_mtimes = meta.file_mtimes;
    let main_num_docs = meta.main_num_docs.unwrap_or(meta.num_docs);

    // 2. Walk root — get current file mtimes and directory mtimes
    let walker = WalkBuilder::new(root)
        .git_ignore(true)
        .hidden(false)
        .build();
    let mut current_files: HashMap<String, u64> = HashMap::new();
    let mut new_dir_mtimes: HashMap<String, u64> = HashMap::new();
    for entry in walker.flatten() {
        if entry.file_type().is_some_and(|ft| ft.is_dir()) {
            // Exclude the index directory itself to avoid self-invalidation
            if !entry.path().starts_with(index_path) {
                if let Ok(m) = entry.metadata() {
                    new_dir_mtimes.insert(entry.path().to_string_lossy().into_owned(), mtime_secs(&m));
                }
            }
            continue;
        }
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        // Skip files inside the index directory
        if path.starts_with(index_path) {
            continue;
        }
        if let Ok(m) = fs::metadata(path) {
            current_files.insert(path.to_string_lossy().into_owned(), mtime_secs(&m));
        }
    }

    // 3. Classify: added, modified, deleted (vs last known state)
    let mut added_set: HashSet<String> = HashSet::new();
    let mut modified_set: HashSet<String> = HashSet::new();
    for (path, &mtime) in &current_files {
        match saved_mtimes.get(path) {
            None => {
                added_set.insert(path.clone());
            }
            Some(&saved) if saved != mtime => {
                modified_set.insert(path.clone());
            }
            _ => {}
        }
    }
    let mut deleted_set: HashSet<String> = HashSet::new();
    for path in saved_mtimes.keys() {
        if !current_files.contains_key(path) {
            deleted_set.insert(path.clone());
        }
    }

    // 4. Early return if no changes
    if added_set.is_empty() && modified_set.is_empty() && deleted_set.is_empty() {
        return Ok(UpdateStats {
            added: 0,
            modified: 0,
            deleted: 0,
            unchanged: saved_mtimes.len(),
            duration_ms: start.elapsed().as_millis() as u64,
        });
    }

    // 5. Load existing index to get doc_ids and current delta/deleted state
    let pidx = load(index_path)?;

    // Build path -> doc_id mapping
    let path_to_docid: HashMap<String, u32> = (0..pidx.num_docs() as u32)
        .filter_map(|id| pidx.doc_path(id).map(|p| (p.to_string_lossy().into_owned(), id)))
        .collect();

    // 6. Update deleted set: mark deleted/modified docs
    let mut new_deleted: HashSet<u32> = pidx.deleted_docs.clone();
    for path in deleted_set.iter().chain(modified_set.iter()) {
        if let Some(&doc_id) = path_to_docid.get(path) {
            new_deleted.insert(doc_id);
        }
    }

    // 7. Determine which files go in the new delta.
    let mut delta_files_to_index: Vec<String> = Vec::new();

    // Keep existing delta files that haven't changed
    for id in main_num_docs..pidx.num_docs() {
        if new_deleted.contains(&(id as u32)) {
            continue;
        }
        if let Some(p) = pidx.doc_path(id as u32) {
            delta_files_to_index.push(p.to_string_lossy().into_owned());
        }
    }

    // Add newly added/modified files
    delta_files_to_index.extend(added_set.iter().cloned());
    delta_files_to_index.extend(modified_set.iter().cloned());

    // Drop the loaded index (releases mmap)
    drop(pidx);

    // 8. Index all delta files with line-level postings
    let mut delta_ngrams: HashMap<u32, Vec<Posting>> = HashMap::new();
    let mut delta_doc_ids: Vec<PathBuf> = Vec::new();
    let mut actual_added = 0usize;
    let mut actual_modified = 0usize;

    for path_str in &delta_files_to_index {
        let path = Path::new(path_str);
        let content = match fs::read(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // Skip binary files
        if content.iter().take(512).any(|&b| b == 0) {
            continue;
        }

        // Doc_id in combined space: main_num_docs + delta_doc_ids.len()
        let doc_id = (main_num_docs + delta_doc_ids.len()) as u32;
        delta_doc_ids.push(path.to_path_buf());

        if added_set.contains(path_str) {
            actual_added += 1;
        } else if modified_set.contains(path_str) {
            actual_modified += 1;
        }

        if content.len() < 3 {
            continue;
        }

        // Line-level trigram indexing (same as SparseIndex::add_document)
        let mut line_no = 1u32;
        let mut line_start = 0usize;
        let mut seen_on_line: HashSet<[u8; 3]> = HashSet::new();

        loop {
            let line_end = content[line_start..]
                .iter()
                .position(|&b| b == b'\n')
                .map(|p| line_start + p)
                .unwrap_or(content.len());

            let line = &content[line_start..line_end];
            if line.len() >= 3 {
                seen_on_line.clear();
                let byte_offset = line_start as u32;
                let mut prefix = [0u8; 4];
                let copy_len = line.len().min(4);
                prefix[..copy_len].copy_from_slice(&line[..copy_len]);
                for w in line.windows(3) {
                    let tri = [w[0], w[1], w[2]];
                    if seen_on_line.insert(tri) {
                        let hash = crc32fast::hash(&tri);
                        delta_ngrams
                            .entry(hash)
                            .or_default()
                            .push((doc_id, line_no, byte_offset, prefix));
                    }
                }
            }

            if line_end >= content.len() {
                break;
            }
            line_start = line_end + 1;
            line_no += 1;
        }
    }

    // 9. Write delta files (small -- only changed files)

    // Write deleted.bin: only main doc_ids that are deleted
    let deleted_path = index_path.join("deleted.bin");
    let main_deleted: Vec<u32> = new_deleted
        .iter()
        .filter(|&&id| (id as usize) < main_num_docs)
        .copied()
        .collect();
    if main_deleted.is_empty() {
        let _ = fs::remove_file(&deleted_path);
    } else {
        let mut f = BufWriter::new(File::create(&deleted_path)?);
        for id in &main_deleted {
            f.write_u32::<LittleEndian>(*id)?;
        }
        f.flush()?;
    }

    // Write delta postings + lookup (12 bytes per entry)
    let mut sorted_ngrams: Vec<(u32, Vec<Posting>)> = delta_ngrams.into_iter().collect();
    sorted_ngrams.sort_by_key(|(hash, _)| *hash);

    let delta_postings_path = index_path.join("delta.postings");
    let delta_lookup_path = index_path.join("delta.lookup");
    let delta_docids_path = index_path.join("delta.docids");

    if sorted_ngrams.is_empty() && delta_doc_ids.is_empty() {
        let _ = fs::remove_file(&delta_postings_path);
        let _ = fs::remove_file(&delta_lookup_path);
        let _ = fs::remove_file(&delta_docids_path);
    } else {
        let mut postings_file = BufWriter::new(File::create(&delta_postings_path)?);
        let mut lookup_entries: Vec<(u32, u64, u32)> = Vec::new();
        let mut offset: u64 = 0;

        for (hash, postings) in &sorted_ngrams {
            let mut buf = Vec::with_capacity(postings.len() * POSTING_ENTRY_SIZE);
            for &(doc_id, line_no, byte_offset, prefix) in postings {
                buf.write_u32::<LittleEndian>(doc_id)?;
                buf.write_u32::<LittleEndian>(line_no)?;
                buf.write_u32::<LittleEndian>(byte_offset)?;
                buf.write_all(&prefix)?;
            }
            let len = buf.len() as u32;
            postings_file.write_all(&buf)?;
            lookup_entries.push((*hash, offset, len));
            offset += len as u64;
        }
        postings_file.flush()?;

        let mut lookup_file = BufWriter::new(File::create(&delta_lookup_path)?);
        for (hash, off, len) in &lookup_entries {
            lookup_file.write_u32::<LittleEndian>(*hash)?;
            lookup_file.write_u64::<LittleEndian>(*off)?;
            lookup_file.write_u32::<LittleEndian>(*len)?;
        }
        lookup_file.flush()?;

        // Write delta docids
        let mut docids_file = BufWriter::new(File::create(&delta_docids_path)?);
        for path in &delta_doc_ids {
            let path_bytes = path.to_string_lossy();
            let bytes = path_bytes.as_bytes();
            docids_file.write_u16::<LittleEndian>(bytes.len() as u16)?;
            docids_file.write_all(bytes)?;
        }
        docids_file.flush()?;
    }

    // 10. Update meta.json with current file_mtimes
    let mut new_mtimes: HashMap<String, u64> = HashMap::with_capacity(saved_mtimes.len());
    for (path, &mtime) in &saved_mtimes {
        if !deleted_set.contains(path) && !modified_set.contains(path) {
            new_mtimes.insert(path.clone(), mtime);
        }
    }
    for path_str in added_set.iter().chain(modified_set.iter()) {
        if let Some(&mtime) = current_files.get(path_str) {
            if delta_doc_ids
                .iter()
                .any(|p| p.to_string_lossy() == *path_str)
            {
                new_mtimes.insert(path_str.clone(), mtime);
            }
        }
    }

    let total_docs = main_num_docs - main_deleted.len() + delta_doc_ids.len();
    let new_meta = IndexMeta {
        version: 3,
        num_docs: total_docs,
        num_ngrams: meta.num_ngrams,
        root_dir: root.to_string_lossy().into_owned(),
        built_at: chrono_now(),
        file_mtimes: new_mtimes,
        dir_mtimes: new_dir_mtimes,
        main_num_docs: Some(main_num_docs),
        bigram_freq_b64: meta.bigram_freq_b64,
    };
    let meta_json = serde_json::to_string_pretty(&new_meta)?;
    fs::write(&meta_path, meta_json)?;

    let unchanged = total_docs - actual_added - actual_modified;

    if verbose {
        eprintln!(
            "Changes: +{} added, {} modified, {} deleted",
            actual_added, actual_modified, deleted_set.len()
        );
    }

    Ok(UpdateStats {
        added: actual_added,
        modified: actual_modified,
        deleted: deleted_set.len(),
        unchanged,
        duration_ms: start.elapsed().as_millis() as u64,
    })
}

/// Extract mtime from filesystem metadata, truncated to 2-second granularity.
/// This avoids false stale detection from sub-second timestamp jitter on NTFS.
fn mtime_secs(meta: &std::fs::Metadata) -> u64 {
    let secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs / 2 * 2
}

/// Walk a directory tree and collect mtime for each directory (not files).
/// Uses the `ignore` crate to respect .gitignore rules.
/// Excludes `exclude_dir` (the index directory itself) to avoid self-invalidation.
fn collect_dir_mtimes(root: &Path, no_ignore: bool, exclude_dir: Option<&Path>) -> HashMap<String, u64> {
    let mut dir_mtimes = HashMap::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(!no_ignore)
        .build();
    for entry in walker.filter_map(|e| e.ok()) {
        if entry.file_type().map_or(false, |ft| ft.is_dir()) {
            if let Some(excl) = exclude_dir {
                if entry.path().starts_with(excl) {
                    continue;
                }
            }
            if let Ok(m) = entry.metadata() {
                dir_mtimes.insert(entry.path().to_string_lossy().into_owned(), mtime_secs(&m));
            }
        }
    }
    dir_mtimes
}

fn chrono_now() -> String {
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}s_since_epoch", dur.as_secs())
}
