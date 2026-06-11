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

use crate::casefold;
use crate::index::{Posting, SparseIndex, TrigramBuilder};
use crate::postenc::{PostingReader, PostingWriter};
use crate::trigram;

/// On-disk index format version. Bumped to 4 for the compact (delta-varint)
/// posting format; the loader rejects older indices so they get rebuilt rather
/// than decoded with the wrong reader.
const INDEX_VERSION: u32 = 4;

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
/// Intersects on (doc_id, line_no), keeps byte_offset from `a`.
fn sorted_intersect_lines(a: &[(u32, u32, u32)], b: &[(u32, u32, u32)]) -> Vec<(u32, u32, u32)> {
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
fn merge_sorted_lines(a: &[(u32, u32, u32)], b: &[(u32, u32, u32)]) -> Vec<(u32, u32, u32)> {
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
    pub lookup_ms: f64,
    pub bitmap_intersect_ms: f64,
    pub verify_ms: f64,
    pub candidates: usize,
    pub matches: usize,
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
    /// Whether this index was built over case-folded text (a case-insensitive
    /// index). `false` (default) = case-sensitive. Set when building the CI
    /// index so `-i` searches can route to it. Reserved in v4 for the planned
    /// dual case-sensitive / case-insensitive index pair.
    #[serde(default)]
    pub case_insensitive: bool,
}

#[derive(Clone)]
pub struct LookupEntry {
    pub hash: u32,
    pub offset: u64,
    pub len: u32,
}

const LOOKUP_ENTRY_SIZE: usize = 4 + 8 + 4; // 16 bytes

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
    // Case-insensitive companion index (`ngrams.ci.*`), present only when the
    // index was built with `-i`. Same docids/delta-docids/deleted set as the
    // case-sensitive index; only the trigram postings/bitmaps differ (folded).
    pub lookup_ci_mmap: Option<Mmap>,
    pub lookup_ci_count: usize,
    pub postings_ci_mmap: Option<Mmap>,
    pub bitmap_ci_mmap: Option<Mmap>,
    pub bitmap_lookup_ci_mmap: Option<Mmap>,
    pub bitmap_lookup_ci_count: usize,
    pub delta_lookup_ci: Vec<LookupEntry>,
    pub delta_postings_ci: Vec<u8>,
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

    /// Whether a case-insensitive companion index is loaded.
    #[inline]
    pub fn has_ci(&self) -> bool {
        self.postings_ci_mmap.is_some()
    }

    // --- Low-level lookup methods (zero-copy) ---
    //
    // Each takes `ci: bool` to select the case-sensitive store (the default
    // `ngrams.*` mmaps) or the case-insensitive companion (`ngrams.ci.*`). The
    // search code threads the same flag through so an `(?i)` query resolves
    // entirely against the folded store.

    /// Binary search in the mmap'd lookup table (CS or CI).
    #[inline]
    fn find_in_main_lookup(&self, hash: u32, ci: bool) -> Option<(u64, u32)> {
        let (data, count) = if ci {
            (&**self.lookup_ci_mmap.as_ref()?, self.lookup_ci_count)
        } else {
            (&*self.lookup_mmap, self.lookup_count)
        };
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

    /// Get raw posting bytes for a trigram hash from the main index (CS or CI).
    #[inline]
    fn main_posting_data(&self, hash: u32, ci: bool) -> Option<&[u8]> {
        let (offset, len) = self.find_in_main_lookup(hash, ci)?;
        let mmap = if ci {
            &**self.postings_ci_mmap.as_ref()?
        } else {
            &*self.postings_mmap
        };
        let start = offset as usize;
        let end = start + len as usize;
        if end <= mmap.len() {
            Some(&mmap[start..end])
        } else {
            None
        }
    }

    /// Get raw posting bytes for a trigram hash from the delta index (CS or CI).
    #[inline]
    fn delta_posting_data(&self, hash: u32, ci: bool) -> Option<&[u8]> {
        let (lookup, postings) = if ci {
            (&self.delta_lookup_ci, &self.delta_postings_ci)
        } else {
            (&self.delta_lookup, &self.delta_postings)
        };
        if lookup.is_empty() {
            return None;
        }
        let idx = lookup.binary_search_by_key(&hash, |e| e.hash).ok()?;
        let entry = &lookup[idx];
        let start = entry.offset as usize;
        let end = start + entry.len as usize;
        if end <= postings.len() {
            Some(&postings[start..end])
        } else {
            None
        }
    }

    // --- Roaring bitmap lookup (Tier 1) ---

    /// Binary search in the mmap'd bitmap lookup table (CS or CI).
    #[inline]
    fn find_in_bitmap_lookup(&self, hash: u32, ci: bool) -> Option<(u64, u32)> {
        let (data, count) = if ci {
            (
                self.bitmap_lookup_ci_mmap.as_ref()?,
                self.bitmap_lookup_ci_count,
            )
        } else {
            (self.bitmap_lookup_mmap.as_ref()?, self.bitmap_lookup_count)
        };
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
    fn lookup_bitmap(&self, hash: u32, ci: bool) -> Option<RoaringBitmap> {
        let (offset, len) = self.find_in_bitmap_lookup(hash, ci)?;
        let bm_data = if ci {
            self.bitmap_ci_mmap.as_ref()?
        } else {
            self.bitmap_mmap.as_ref()?
        };
        let start = offset as usize;
        let end = start + len as usize;
        if end > bm_data.len() {
            return None;
        }
        RoaringBitmap::deserialize_unchecked_from(&bm_data[start..end]).ok()
    }

    /// Extract sorted line postings from a compact posting blob, excluding deleted docs.
    fn extract_line_postings(&self, data: &[u8]) -> Vec<(u32, u32, u32)> {
        let mut postings = Vec::new();
        for (doc_id, line_no, byte_offset) in PostingReader::new(data) {
            if !self.deleted_docs.contains(&doc_id) {
                postings.push((doc_id, line_no, byte_offset));
            }
        }
        postings
    }

    /// Extract sorted line postings, filtering to only doc_ids in the bitmap.
    fn extract_line_postings_filtered(
        &self,
        data: &[u8],
        filter: &RoaringBitmap,
    ) -> Vec<(u32, u32, u32)> {
        let mut postings = Vec::new();
        for (doc_id, line_no, byte_offset) in PostingReader::new(data) {
            if filter.contains(doc_id) && !self.deleted_docs.contains(&doc_id) {
                postings.push((doc_id, line_no, byte_offset));
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
        ci: bool,
    ) -> Option<Vec<(u32, u32, u32)>> {
        let main = self.main_posting_data(hash, ci);
        let delta = self.delta_posting_data(hash, ci);

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
    fn trigram_line_postings(&self, hash: u32, ci: bool) -> Option<Vec<(u32, u32, u32)>> {
        let main = self.main_posting_data(hash, ci);
        let delta = self.delta_posting_data(hash, ci);

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

    pub fn search_timed(&self, pattern: &str) -> (SearchResult<'_>, SearchTiming) {
        // Patterns with inline `(?i)` (or any flag group enabling
        // case-insensitive matching) can't be answered from the
        // case-sensitive store — `(?i)abc` looking up the trigram "abc"
        // would miss files containing `ABC`. If a case-insensitive
        // companion index is loaded we resolve against it (folding the
        // query literals the same way the content was folded); otherwise
        // we fall back to scanning every live file.
        let pattern_ci = trigram::has_case_insensitive_flag(pattern);
        let ci = pattern_ci && self.has_ci();
        if pattern_ci && !ci {
            let docs = self.live_doc_ids();
            let n = docs.len();
            return (
                SearchResult::AllFiles(docs),
                SearchTiming {
                    lookup_ms: 0.0,
                    bitmap_intersect_ms: 0.0,
                    verify_ms: 0.0,
                    candidates: n,
                    matches: 0,
                    strategy: String::new(),
                    density: 0.0,
                    prefix_filtered: 0,
                },
            );
        }

        let alternatives = if ci {
            trigram::decompose_pattern_folded(pattern)
        } else {
            trigram::decompose_pattern(pattern)
        };

        if alternatives.is_empty() || alternatives.iter().all(|a| a.is_empty()) {
            let docs = self.live_doc_ids();
            let n = docs.len();
            return (
                SearchResult::AllFiles(docs),
                SearchTiming {
                    lookup_ms: 0.0,
                    bitmap_intersect_ms: 0.0,
                    verify_ms: 0.0,
                    candidates: n,
                    matches: 0,
                    strategy: String::new(),
                    density: 0.0,
                    prefix_filtered: 0,
                },
            );
        }

        let mut result_lines: Vec<(u32, u32, u32)> = Vec::new();
        let mut bitmap_dur = Duration::ZERO;
        let mut intersect_dur = Duration::ZERO;
        let has_bitmaps = if ci {
            self.bitmap_ci_mmap.is_some()
        } else {
            self.bitmap_mmap.is_some()
        };

        for alt_trigrams in &alternatives {
            if alt_trigrams.is_empty() {
                let docs = self.live_doc_ids();
                let n = docs.len();
                return (
                    SearchResult::AllFiles(docs),
                    SearchTiming {
                        lookup_ms: 0.0,
                        bitmap_intersect_ms: 0.0,
                        verify_ms: 0.0,
                        candidates: n,
                        matches: 0,
                        strategy: String::new(),
                        density: 0.0,
                        prefix_filtered: 0,
                    },
                );
            }

            let hashes: Vec<u32> = alt_trigrams
                .iter()
                .map(|tri| crc32fast::hash(tri))
                .collect();

            if has_bitmaps {
                // === Two-tier search: Roaring Bitmap → filtered postings ===

                // Phase 1: Bitmap intersection (parallel load, then serial AND)
                let t_bitmap = Instant::now();

                // Parallel deserialization of all bitmaps
                let mut bitmaps: Vec<Option<RoaringBitmap>> = hashes
                    .par_iter()
                    .map(|&h| self.lookup_bitmap(h, ci))
                    .collect();

                // If any trigram is missing from bitmaps, fall back to full posting list search
                if bitmaps.iter().any(|b| b.is_none()) {
                    bitmap_dur += t_bitmap.elapsed();
                    // Fallback: load postings directly without bitmap pre-filter
                    let t_fallback = Instant::now();
                    let posting_lists: Vec<Option<Vec<(u32, u32, u32)>>> = hashes
                        .par_iter()
                        .map(|&h| self.trigram_line_postings(h, ci))
                        .collect();
                    if posting_lists.iter().any(|p| p.is_none()) {
                        intersect_dur += t_fallback.elapsed();
                        continue;
                    }
                    let mut posting_lists: Vec<Vec<(u32, u32, u32)>> =
                        posting_lists.into_iter().map(|p| p.unwrap()).collect();
                    posting_lists.sort_by_key(|v| v.len());
                    let mut candidates = posting_lists.swap_remove(0);
                    for other in &posting_lists {
                        if candidates.is_empty() {
                            break;
                        }
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

                // Delta docs don't have bitmap entries — add all live delta
                // doc_ids so incremental updates are never invisible.
                let main_count = self.main_num_docs as u32;
                for delta_idx in 0..self.delta_doc_ids.len() as u32 {
                    let doc_id = main_count + delta_idx;
                    if !self.deleted_docs.contains(&doc_id) {
                        candidate_docs.insert(doc_id);
                    }
                }

                if candidate_docs.is_empty() {
                    bitmap_dur += t_bitmap.elapsed();
                    continue;
                }

                let bm_card = candidate_docs.len() as usize;
                bitmap_dur += t_bitmap.elapsed();

                // Fast path: if bitmap is very selective (< 0.7% of corpus, with a 500-doc
                // floor so tiny corpora still benefit) AND there's only one alternative —
                // skip posting load and verify files directly.
                // NOTE: with multiple alternatives (alternation like a|b|c), we must NOT
                // return early here because other alternatives still need to be processed.
                let bitmap_threshold = ((self.num_docs() as f64 * 0.007) as usize).max(500);
                if bm_card <= bitmap_threshold && alternatives.len() == 1 {
                    let paths: Vec<&Path> = candidate_docs
                        .iter()
                        .filter_map(|id| self.doc_path(id))
                        .collect();
                    let n = paths.len();
                    return (
                        SearchResult::BitmapFiles(paths),
                        SearchTiming {
                            lookup_ms: bitmap_dur.as_secs_f64() * 1000.0,
                            bitmap_intersect_ms: 0.0,
                            verify_ms: 0.0,
                            candidates: n,
                            matches: 0,
                            strategy: String::new(),
                            density: 0.0,
                            prefix_filtered: 0,
                        },
                    );
                }

                // Phase 2: Load line postings (filtered by bitmap if selective), then intersect
                let t_intersect = Instant::now();
                let bm_selectivity = bm_card as f64 / self.num_docs().max(1) as f64;

                let posting_lists: Vec<Option<Vec<(u32, u32, u32)>>> = if bm_selectivity < 0.5 {
                    // Bitmap is selective — use filtered extraction
                    hashes
                        .par_iter()
                        .map(|&h| self.trigram_line_postings_filtered(h, &candidate_docs, ci))
                        .collect()
                } else {
                    // Bitmap isn't selective — full extraction is faster
                    hashes
                        .par_iter()
                        .map(|&h| self.trigram_line_postings(h, ci))
                        .collect()
                };

                if posting_lists.iter().any(|p| p.is_none()) {
                    intersect_dur += t_intersect.elapsed();
                    continue;
                }

                let mut posting_lists: Vec<Vec<(u32, u32, u32)>> =
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
                let posting_lists: Vec<Option<Vec<(u32, u32, u32)>>> = hashes
                    .par_iter()
                    .map(|&h| self.trigram_line_postings(h, ci))
                    .collect();
                bitmap_dur += t_lookup.elapsed();

                if posting_lists.iter().any(|p| p.is_none()) {
                    continue;
                }

                let t_intersect = Instant::now();
                let mut posting_lists: Vec<Vec<(u32, u32, u32)>> =
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
            .filter_map(|&(doc_id, line_no, byte_offset)| {
                self.doc_path(doc_id).map(|path| LineHit {
                    path,
                    line_no,
                    byte_offset,
                })
            })
            .collect();

        (
            SearchResult::LineHits(hits),
            SearchTiming {
                lookup_ms: bitmap_dur.as_secs_f64() * 1000.0,
                bitmap_intersect_ms: intersect_dur.as_secs_f64() * 1000.0,
                verify_ms: 0.0,
                candidates: num_candidates,
                matches: 0,
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

/// Serialize one trigram map to its four on-disk files under `output`, named
/// `{prefix}.postings`, `{prefix}.lookup`, `{prefix}.bitmaps`,
/// `{prefix}.bitmaps.lookup`. Used for both the case-sensitive map
/// (`prefix = "ngrams"`) and the case-insensitive companion
/// (`prefix = "ngrams.ci"`). Returns the postings byte length.
fn write_ngram_files(
    output: &Path,
    prefix: &str,
    ngrams: &HashMap<[u8; 3], TrigramBuilder>,
) -> Result<u64> {
    // Sort trigrams by hash for binary search
    let mut trigram_list: Vec<(&[u8; 3], &TrigramBuilder)> = ngrams.iter().collect();
    trigram_list.sort_by_key(|(k, _)| crc32fast::hash(*k));

    // Write compact (delta-varint) postings and build lookup (offset/len per
    // trigram). Postings are already compact-encoded in `builder.bytes`, so we
    // write them verbatim — no re-encode pass.
    let postings_path = output.join(format!("{prefix}.postings"));
    let mut postings_file = BufWriter::new(File::create(&postings_path)?);
    let mut lookup_entries: Vec<(u32, u64, u32)> = Vec::new();
    let mut offset: u64 = 0;
    for (tri, builder) in &trigram_list {
        let len = builder.bytes.len() as u32;
        postings_file.write_all(&builder.bytes)?;
        lookup_entries.push((crc32fast::hash(*tri), offset, len));
        offset += len as u64;
    }
    postings_file.flush()?;
    let postings_len = offset;

    let lookup_path = output.join(format!("{prefix}.lookup"));
    let mut lookup_file = BufWriter::new(File::create(&lookup_path)?);
    for (hash, off, len) in &lookup_entries {
        lookup_file.write_u32::<LittleEndian>(*hash)?;
        lookup_file.write_u64::<LittleEndian>(*off)?;
        lookup_file.write_u32::<LittleEndian>(*len)?;
    }
    lookup_file.flush()?;

    // Write Roaring Bitmaps (Tier 1: doc_id sets per trigram)
    let bitmaps_path = output.join(format!("{prefix}.bitmaps"));
    let mut bitmaps_file = BufWriter::new(File::create(&bitmaps_path)?);
    let mut bitmap_lookup_entries: Vec<(u32, u64, u32)> = Vec::new();
    let mut bm_offset: u64 = 0;
    for (tri, builder) in &trigram_list {
        let mut bitmap = RoaringBitmap::new();
        for (doc_id, _, _) in PostingReader::new(&builder.bytes) {
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

    let bitmaps_lookup_path = output.join(format!("{prefix}.bitmaps.lookup"));
    let mut bm_lookup_file = BufWriter::new(File::create(&bitmaps_lookup_path)?);
    for (hash, off, len) in &bitmap_lookup_entries {
        bm_lookup_file.write_u32::<LittleEndian>(*hash)?;
        bm_lookup_file.write_u64::<LittleEndian>(*off)?;
        bm_lookup_file.write_u32::<LittleEndian>(*len)?;
    }
    bm_lookup_file.flush()?;

    Ok(postings_len)
}

/// Remove a case-insensitive companion index's files (used when (re)building a
/// case-sensitive-only index over a directory that previously had a CI index).
fn remove_ci_files(output: &Path) {
    for suffix in [
        "ngrams.ci.postings",
        "ngrams.ci.lookup",
        "ngrams.ci.bitmaps",
        "ngrams.ci.bitmaps.lookup",
        "delta.ci.postings",
        "delta.ci.lookup",
    ] {
        let _ = fs::remove_file(output.join(suffix));
    }
}

pub fn build(
    root: &Path,
    output: &Path,
    no_ignore: bool,
    type_filter: &[String],
    verbose: bool,
    case_insensitive: bool,
) -> Result<()> {
    if verbose {
        eprintln!("Building index for {:?}...", root);
    }

    let index =
        SparseIndex::build_from_directory(root, no_ignore, type_filter, verbose, case_insensitive)?;

    fs::create_dir_all(output).context("creating output directory")?;

    // Collect file mtimes
    let mut file_mtimes = HashMap::new();
    for path in &index.doc_ids {
        if let Ok(m) = fs::metadata(path) {
            file_mtimes.insert(path.to_string_lossy().into_owned(), mtime_secs(&m));
        }
    }

    // Collect directory mtimes for fast stale detection
    let dir_mtimes = collect_dir_mtimes(root, no_ignore, Some(output));

    // Write the case-sensitive trigram files, and the case-insensitive
    // companion (`ngrams.ci.*`) when this is a CI build.
    let postings_len = write_ngram_files(output, "ngrams", &index.ngrams)?;
    if let Some(ci) = &index.ngrams_ci {
        write_ngram_files(output, "ngrams.ci", ci)?;
    } else {
        // A prior CI build over this directory may have left stale CI files.
        remove_ci_files(output);
    }

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
        version: INDEX_VERSION,
        num_docs,
        num_ngrams: index.ngrams.len(),
        root_dir: root.to_string_lossy().into_owned(),
        built_at: chrono_now(),
        file_mtimes,
        dir_mtimes,
        main_num_docs: Some(num_docs),
        case_insensitive: index.ngrams_ci.is_some(),
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
            "Index built: {} docs, {} trigrams, postings {}KB{}",
            meta.num_docs,
            meta.num_ngrams,
            postings_len / 1024,
            if meta.case_insensitive {
                " (+ case-insensitive index)"
            } else {
                ""
            }
        );
    }

    Ok(())
}

/// True if an index exists at `idx_path` and is the current on-disk format
/// version. Callers use this to decide whether to (re)build before searching or
/// updating — a missing OR stale-version index returns `false`.
pub fn is_current(idx_path: &Path) -> bool {
    match fs::read_to_string(idx_path.join("meta.json")) {
        Ok(s) => serde_json::from_str::<IndexMeta>(&s)
            .map(|m| m.version == INDEX_VERSION)
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// Read a delta lookup + postings pair (used for both the CS and CI deltas).
/// Returns empty vecs when the lookup file is absent.
fn read_delta(lookup_path: &Path, postings_path: &Path) -> Result<(Vec<LookupEntry>, Vec<u8>)> {
    if !lookup_path.exists() {
        return Ok((Vec::new(), Vec::new()));
    }
    let ldata = fs::read(lookup_path)?;
    let num = ldata.len() / LOOKUP_ENTRY_SIZE;
    let mut dlookup = Vec::with_capacity(num);
    let mut cursor = std::io::Cursor::new(&ldata);
    for _ in 0..num {
        let hash = cursor.read_u32::<LittleEndian>()?;
        let offset = cursor.read_u64::<LittleEndian>()?;
        let len = cursor.read_u32::<LittleEndian>()?;
        dlookup.push(LookupEntry { hash, offset, len });
    }
    let dpostings = fs::read(postings_path).unwrap_or_default();
    Ok((dlookup, dpostings))
}

/// Write a delta store's `{postings, lookup}` files (compact-encoded), or
/// remove them when the trigram map is empty. Doc-ids are shared between the CS
/// and CI deltas, so this writes only the postings + lookup pair.
fn write_delta_store(
    postings_path: &Path,
    lookup_path: &Path,
    ngrams: HashMap<u32, Vec<Posting>>,
) -> Result<()> {
    let mut sorted: Vec<(u32, Vec<Posting>)> = ngrams.into_iter().collect();
    sorted.sort_by_key(|(hash, _)| *hash);

    if sorted.is_empty() {
        let _ = fs::remove_file(postings_path);
        let _ = fs::remove_file(lookup_path);
        return Ok(());
    }

    let mut postings_file = BufWriter::new(File::create(postings_path)?);
    let mut lookup_entries: Vec<(u32, u64, u32)> = Vec::new();
    let mut offset: u64 = 0;
    for (hash, postings) in &sorted {
        let mut buf = Vec::with_capacity(postings.len() * 3);
        let mut w = PostingWriter::new();
        for &(doc_id, line_no, byte_offset) in postings {
            w.push(&mut buf, doc_id, line_no, byte_offset);
        }
        let len = buf.len() as u32;
        postings_file.write_all(&buf)?;
        lookup_entries.push((*hash, offset, len));
        offset += len as u64;
    }
    postings_file.flush()?;

    let mut lookup_file = BufWriter::new(File::create(lookup_path)?);
    for (hash, off, len) in &lookup_entries {
        lookup_file.write_u32::<LittleEndian>(*hash)?;
        lookup_file.write_u64::<LittleEndian>(*off)?;
        lookup_file.write_u32::<LittleEndian>(*len)?;
    }
    lookup_file.flush()?;
    Ok(())
}

/// Open the four mmaps of a trigram store given its file prefix, returning
/// `None` for the whole set when the postings file is absent.
type StoreMmaps = (Mmap, usize, Mmap, Option<Mmap>, Option<Mmap>, usize);
fn load_store(index_path: &Path, prefix: &str) -> Result<Option<StoreMmaps>> {
    let postings_path = index_path.join(format!("{prefix}.postings"));
    if !postings_path.exists() {
        return Ok(None);
    }
    let lf = File::open(index_path.join(format!("{prefix}.lookup")))?;
    let lookup_mmap = unsafe { Mmap::map(&lf)? };
    let lookup_count = lookup_mmap.len() / LOOKUP_ENTRY_SIZE;
    let pf = File::open(&postings_path)?;
    let postings_mmap = unsafe { Mmap::map(&pf)? };

    let bitmaps_path = index_path.join(format!("{prefix}.bitmaps"));
    let bitmaps_lookup_path = index_path.join(format!("{prefix}.bitmaps.lookup"));
    let (bitmap_mmap, bitmap_lookup_mmap, bitmap_lookup_count) =
        if bitmaps_path.exists() && bitmaps_lookup_path.exists() {
            let bf = File::open(&bitmaps_path)?;
            let bm = unsafe { Mmap::map(&bf)? };
            let blf = File::open(&bitmaps_lookup_path)?;
            let blm = unsafe { Mmap::map(&blf)? };
            let count = blm.len() / LOOKUP_ENTRY_SIZE;
            (Some(bm), Some(blm), count)
        } else {
            (None, None, 0)
        };
    Ok(Some((
        lookup_mmap,
        lookup_count,
        postings_mmap,
        bitmap_mmap,
        bitmap_lookup_mmap,
        bitmap_lookup_count,
    )))
}

pub fn load(index_path: &Path) -> Result<PersistentIndex> {
    let meta_path = index_path.join("meta.json");
    let meta_str = fs::read_to_string(&meta_path).context("reading meta.json")?;
    let meta: IndexMeta = serde_json::from_str(&meta_str).context("parsing meta.json")?;

    if meta.version != INDEX_VERSION {
        anyhow::bail!(
            "index at {} is version {} but this build expects version {} (posting format changed); rebuild with `fgr index`",
            index_path.display(),
            meta.version,
            INDEX_VERSION
        );
    }

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
    let delta_docids_path = index_path.join("delta.docids");
    let (delta_lookup, delta_postings) = read_delta(
        &index_path.join("delta.lookup"),
        &index_path.join("delta.postings"),
    )?;

    // Load the case-insensitive companion store and its delta (if present).
    let (
        lookup_ci_mmap,
        lookup_ci_count,
        postings_ci_mmap,
        bitmap_ci_mmap,
        bitmap_lookup_ci_mmap,
        bitmap_lookup_ci_count,
    ) = match if meta.case_insensitive {
        load_store(index_path, "ngrams.ci")?
    } else {
        None
    } {
        Some((lm, lc, pm, bm, blm, bc)) => (Some(lm), lc, Some(pm), bm, blm, bc),
        None => (None, 0, None, None, None, 0),
    };
    let (delta_lookup_ci, delta_postings_ci) = read_delta(
        &index_path.join("delta.ci.lookup"),
        &index_path.join("delta.ci.postings"),
    )?;

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
        lookup_ci_mmap,
        lookup_ci_count,
        postings_ci_mmap,
        bitmap_ci_mmap,
        bitmap_lookup_ci_mmap,
        bitmap_lookup_ci_count,
        delta_lookup_ci,
        delta_postings_ci,
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
    // An incremental update only rewrites the delta; it cannot mix a new-format
    // delta into an old-format main index without corrupting it. Refuse on a
    // version mismatch — the caller (or the search auto-build) rebuilds instead.
    if meta.version != INDEX_VERSION {
        anyhow::bail!(
            "index at {} is version {} but this build expects version {}; run `fgr index` to rebuild",
            index_path.display(),
            meta.version,
            INDEX_VERSION
        );
    }
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
                    new_dir_mtimes
                        .insert(entry.path().to_string_lossy().into_owned(), mtime_secs(&m));
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
        .filter_map(|id| {
            pidx.doc_path(id)
                .map(|p| (p.to_string_lossy().into_owned(), id))
        })
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

    // 8. Index all delta files with line-level postings. When the index has a
    // case-insensitive companion, build the CI delta in lockstep (same delta
    // docs, case-folded trigrams) so `-i` searches stay correct after updates.
    let ci_enabled = meta.case_insensitive;
    let mut delta_ngrams: HashMap<u32, Vec<Posting>> = HashMap::new();
    let mut delta_ngrams_ci: HashMap<u32, Vec<Posting>> = HashMap::new();
    let mut fold_buf: Vec<u8> = Vec::new();
    let mut seen_on_line_ci: HashSet<[u8; 3]> = HashSet::new();
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
                for w in line.windows(3) {
                    let tri = [w[0], w[1], w[2]];
                    if seen_on_line.insert(tri) {
                        let hash = crc32fast::hash(&tri);
                        delta_ngrams
                            .entry(hash)
                            .or_default()
                            .push((doc_id, line_no, byte_offset));
                    }
                }

                // Lockstep CI delta: same posting, folded trigrams.
                if ci_enabled {
                    casefold::fold_into(line, &mut fold_buf);
                    if fold_buf.len() >= 3 {
                        seen_on_line_ci.clear();
                        for w in fold_buf.windows(3) {
                            let tri = [w[0], w[1], w[2]];
                            if seen_on_line_ci.insert(tri) {
                                let hash = crc32fast::hash(&tri);
                                delta_ngrams_ci.entry(hash).or_default().push((
                                    doc_id,
                                    line_no,
                                    byte_offset,
                                ));
                            }
                        }
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
            let mut buf = Vec::with_capacity(postings.len() * 3);
            let mut w = PostingWriter::new();
            for &(doc_id, line_no, byte_offset) in postings {
                w.push(&mut buf, doc_id, line_no, byte_offset);
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

    // 9b. Write the lockstep CI delta (postings + lookup only — docids are
    // shared with the CS delta written above). When the index has no CI
    // companion, make sure no stale CI delta lingers.
    if ci_enabled {
        write_delta_store(
            &index_path.join("delta.ci.postings"),
            &index_path.join("delta.ci.lookup"),
            delta_ngrams_ci,
        )?;
    } else {
        let _ = fs::remove_file(index_path.join("delta.ci.postings"));
        let _ = fs::remove_file(index_path.join("delta.ci.lookup"));
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
        version: INDEX_VERSION,
        num_docs: total_docs,
        num_ngrams: meta.num_ngrams,
        root_dir: root.to_string_lossy().into_owned(),
        built_at: chrono_now(),
        file_mtimes: new_mtimes,
        dir_mtimes: new_dir_mtimes,
        main_num_docs: Some(main_num_docs),
        case_insensitive: meta.case_insensitive,
    };
    let meta_json = serde_json::to_string_pretty(&new_meta)?;
    fs::write(&meta_path, meta_json)?;

    let unchanged = total_docs - actual_added - actual_modified;

    if verbose {
        eprintln!(
            "Changes: +{} added, {} modified, {} deleted",
            actual_added,
            actual_modified,
            deleted_set.len()
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
fn collect_dir_mtimes(
    root: &Path,
    no_ignore: bool,
    exclude_dir: Option<&Path>,
) -> HashMap<String, u64> {
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

/// Full staleness check: walks ALL files and directories, comparing every mtime
/// against the index metadata. Expensive (~850ms for 79K files) but zero false
/// negatives. Used by the daemon at startup.
pub fn full_stale_check(index: &PersistentIndex, index_path: &Path) -> bool {
    let root = Path::new(&index.meta.root_dir);
    let walker = ignore::WalkBuilder::new(root)
        .git_ignore(true)
        .hidden(false)
        .build();
    for entry in walker.flatten() {
        let path = entry.path();
        if path.starts_with(index_path) {
            continue;
        }
        if entry.file_type().is_some_and(|ft| ft.is_dir()) {
            let key = path.to_string_lossy();
            if let Some(&stored) = index.meta.dir_mtimes.get(key.as_ref()) {
                if let Ok(m) = entry.metadata() {
                    if mtime_secs(&m) != stored {
                        return true;
                    }
                }
            } else {
                return true; // new directory
            }
        } else if entry.file_type().is_some_and(|ft| ft.is_file()) {
            let key = path.to_string_lossy();
            if let Some(&stored) = index.meta.file_mtimes.get(key.as_ref()) {
                if let Ok(m) = fs::metadata(path) {
                    if mtime_secs(&m) != stored {
                        return true;
                    }
                }
            } else {
                return true; // new file
            }
        }
    }
    // Also check for deleted files (in index but not on disk)
    for path_str in index.meta.file_mtimes.keys() {
        if !Path::new(path_str).exists() {
            return true;
        }
    }
    false
}

/// Acquire an exclusive lock on the index directory for updates.
/// Returns the lock file handle and a flag indicating whether we had to wait.
pub fn acquire_index_lock(idx_path: &Path) -> anyhow::Result<(fs::File, bool)> {
    let lock_path = idx_path.join("lock");
    let mut waited = false;
    loop {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(f) => return Ok((f, waited)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if !waited {
                    eprintln!("Waiting for another process to finish updating index...");
                    waited = true;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

pub fn release_index_lock(idx_path: &Path) {
    let _ = fs::remove_file(idx_path.join("lock"));
}
