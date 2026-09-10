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
use crate::index::TrigramBuilder;
use crate::postenc::{PostingReader, PostingWriter};
use crate::trigram;

/// On-disk index format version. Bumped to 4 for the compact (delta-varint)
/// posting format; the loader rejects older indices so they get rebuilt rather
/// than decoded with the wrong reader.
const INDEX_VERSION: u32 = 4;

// --- Slot layout (two-slot baseline swap) ---
//
// The index *root* (the `.fgr` dir the user/daemon points at) holds only
// coordination files: the `current` pointer, the daemon pid/port, and the
// `lock`. The actual index content lives in a *slot* subdirectory named by
// `current` (`slot-a` / `slot-b`). Writers stage a fresh baseline into the
// non-live slot and flip `current` atomically, so readers mapped on the live
// slot are never disturbed. Absent `current` → legacy flat layout, where the
// content files sit directly in the root; `live_slot_dir` falls back to it so
// pre-slot indexes keep loading without a rebuild.

/// Pointer file (in the index root) naming the live slot. Absent → flat layout.
const CURRENT_FILE: &str = "current";

/// Content filenames that live inside a slot. The root additionally holds
/// `current`, the daemon pid/port/lock, and (later) `config.toml` — none of
/// which appear here, so cleanup never touches them.
const CONTENT_FILES: &[&str] = &[
    "meta.json",
    "docids.bin",
    "deleted.bin",
    "delta.postings",
    "delta.lookup",
    "delta.docids",
    "ngrams.postings",
    "ngrams.lookup",
    "ngrams.bitmaps",
    "ngrams.bitmaps.lookup",
    "ngrams.ci.postings",
    "ngrams.ci.lookup",
    "ngrams.ci.bitmaps",
    "ngrams.ci.bitmaps.lookup",
    "delta.ci.postings",
    "delta.ci.lookup",
];

/// Read the `current` pointer, returning the live slot name if set.
fn read_current(index_dir: &Path) -> Option<String> {
    let s = fs::read_to_string(index_dir.join(CURRENT_FILE)).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Atomically point `current` at `slot` (write temp, then rename over — the
/// pointer is never mmapped, so replacing it is safe on all platforms).
fn write_current(index_dir: &Path, slot: &str) -> Result<()> {
    let tmp = index_dir.join("current.tmp");
    fs::write(&tmp, slot)?;
    fs::rename(&tmp, index_dir.join(CURRENT_FILE))?;
    Ok(())
}

/// Resolve the directory holding the live index content. A valid `current`
/// pointer naming an existing slot wins; otherwise the index root itself
/// (legacy flat layout).
fn live_slot_dir(index_dir: &Path) -> PathBuf {
    if let Some(slot) = read_current(index_dir) {
        let p = index_dir.join(&slot);
        if p.join("meta.json").exists() {
            return p;
        }
    }
    index_dir.to_path_buf()
}

/// Pick the slot to stage a fresh baseline into: the one that is NOT currently
/// live, so readers mapped on the live slot are undisturbed. Defaults to
/// `slot-a` when there is no live slot yet.
fn next_slot(index_dir: &Path) -> &'static str {
    match read_current(index_dir).as_deref() {
        Some("slot-a") => "slot-b",
        _ => "slot-a",
    }
}

/// After flipping `current` to `live_slot`, best-effort reclaim disk: drop the
/// other slot and any legacy flat content files left in the root. Safe even if
/// a reader still maps them — on the target platforms the delete succeeds and
/// the reader keeps its existing mapping.
fn cleanup_non_live(index_dir: &Path, live_slot: &str) {
    let other = if live_slot == "slot-a" {
        "slot-b"
    } else {
        "slot-a"
    };
    let _ = fs::remove_dir_all(index_dir.join(other));
    for f in CONTENT_FILES {
        let _ = fs::remove_file(index_dir.join(f));
    }
}

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

                // Main-index bitmaps are stale w.r.t. tombstones — drop deleted
                // docs now so (a) the fast path below never emits a tombstoned
                // doc's path (a modified file would otherwise appear twice: once
                // via its dead main doc and once via its delta doc → duplicate
                // matches), and (b) bm_card reflects real candidates.
                for &id in &self.deleted_docs {
                    candidate_docs.remove(id);
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

/// Streams one trigram store's four on-disk files, emitting one `(key, blob)`
/// at a time in ascending-key order. Both the in-memory `write_ngram_files`
/// path and the external-merge build (`crate::buildsort`) feed this same
/// `emit`, so their output is byte-identical. Files: `{prefix}.postings`
/// (compact posting blobs concatenated), `{prefix}.lookup`
/// (`[key u32][off u64][len u32]` per trigram, key-sorted, binary-searched),
/// `{prefix}.bitmaps` (a RoaringBitmap of doc ids per trigram) and
/// `{prefix}.bitmaps.lookup`.
pub(crate) struct NgramFileWriter {
    postings: BufWriter<File>,
    bitmaps: BufWriter<File>,
    lookup_entries: Vec<(u32, u64, u32)>,
    bitmap_lookup_entries: Vec<(u32, u64, u32)>,
    lookup_path: PathBuf,
    bitmaps_lookup_path: PathBuf,
    offset: u64,
    bm_offset: u64,
}

impl NgramFileWriter {
    pub(crate) fn create(output: &Path, prefix: &str) -> Result<Self> {
        Ok(Self {
            postings: BufWriter::new(File::create(output.join(format!("{prefix}.postings")))?),
            bitmaps: BufWriter::new(File::create(output.join(format!("{prefix}.bitmaps")))?),
            lookup_entries: Vec::new(),
            bitmap_lookup_entries: Vec::new(),
            lookup_path: output.join(format!("{prefix}.lookup")),
            bitmaps_lookup_path: output.join(format!("{prefix}.bitmaps.lookup")),
            offset: 0,
            bm_offset: 0,
        })
    }

    /// Append one trigram's compact posting `blob` (already delta-encoded) and
    /// its derived doc-id bitmap. Keys MUST arrive in ascending order.
    pub(crate) fn emit(&mut self, key: u32, blob: &[u8]) -> Result<()> {
        self.postings.write_all(blob)?;
        self.lookup_entries
            .push((key, self.offset, blob.len() as u32));
        self.offset += blob.len() as u64;

        let mut bitmap = RoaringBitmap::new();
        for (doc_id, _, _) in PostingReader::new(blob) {
            bitmap.insert(doc_id);
        }
        let mut bm_buf = Vec::new();
        bitmap.serialize_into(&mut bm_buf)?;
        self.bitmaps.write_all(&bm_buf)?;
        self.bitmap_lookup_entries
            .push((key, self.bm_offset, bm_buf.len() as u32));
        self.bm_offset += bm_buf.len() as u64;
        Ok(())
    }

    /// Flush the postings/bitmaps and write the two lookup tables. Returns the
    /// postings byte length and the number of trigrams emitted.
    pub(crate) fn finish(mut self) -> Result<(u64, usize)> {
        self.postings.flush()?;
        self.bitmaps.flush()?;

        let mut lookup_file = BufWriter::new(File::create(&self.lookup_path)?);
        for (key, off, len) in &self.lookup_entries {
            lookup_file.write_u32::<LittleEndian>(*key)?;
            lookup_file.write_u64::<LittleEndian>(*off)?;
            lookup_file.write_u32::<LittleEndian>(*len)?;
        }
        lookup_file.flush()?;

        let mut bm_lookup_file = BufWriter::new(File::create(&self.bitmaps_lookup_path)?);
        for (key, off, len) in &self.bitmap_lookup_entries {
            bm_lookup_file.write_u32::<LittleEndian>(*key)?;
            bm_lookup_file.write_u64::<LittleEndian>(*off)?;
            bm_lookup_file.write_u32::<LittleEndian>(*len)?;
        }
        bm_lookup_file.flush()?;

        Ok((self.offset, self.lookup_entries.len()))
    }
}

/// Serialize one in-memory trigram map to its four `{prefix}.*` files under
/// `output`. Used for the case-sensitive map (`prefix = "ngrams"`) and the
/// case-insensitive companion (`prefix = "ngrams.ci"`) on the non-spilling
/// build path. Returns the postings byte length.
pub(crate) fn write_ngram_files(
    output: &Path,
    prefix: &str,
    ngrams: &HashMap<[u8; 3], TrigramBuilder>,
) -> Result<u64> {
    // Emit trigrams in key order so the lookup can be binary-searched.
    let mut trigram_list: Vec<(&[u8; 3], &TrigramBuilder)> = ngrams.iter().collect();
    trigram_list.sort_by_key(|(k, _)| crc32fast::hash(*k));

    let mut w = NgramFileWriter::create(output, prefix)?;
    for (tri, builder) in &trigram_list {
        w.emit(crc32fast::hash(*tri), &builder.bytes)?;
    }
    let (postings_len, _) = w.finish()?;
    Ok(postings_len)
}

/// Remove a case-insensitive companion index's files (used when (re)building a
/// case-sensitive-only index over a directory that previously had a CI index).
pub(crate) fn remove_ci_files(output: &Path) {
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

// --- Compaction (rebaseline: fold delta + drop tombstones) ---

pub struct CompactStats {
    /// Live docs in the new baseline (main survivors + live delta).
    pub live_docs: usize,
    /// Docs dropped (tombstoned main + deleted delta).
    pub dropped_docs: usize,
    /// Trigrams in the compacted case-sensitive store.
    pub num_ngrams: usize,
}

/// Read one `(hash, offset, len)` lookup entry from a raw mmap'd lookup table.
#[inline]
fn lookup_entry_at(data: &[u8], i: usize) -> (u32, u64, u32) {
    let base = i * LOOKUP_ENTRY_SIZE;
    (
        read_u32_le(data, base),
        read_u64_le(data, base + 4),
        read_u32_le(data, base + 12),
    )
}

/// Write a `(hash, offset, len)` lookup table to `path`.
fn write_lookup_file(path: &Path, entries: &[(u32, u64, u32)]) -> Result<()> {
    let mut f = BufWriter::new(File::create(path)?);
    for (hash, off, len) in entries {
        f.write_u32::<LittleEndian>(*hash)?;
        f.write_u64::<LittleEndian>(*off)?;
        f.write_u32::<LittleEndian>(*len)?;
    }
    f.flush()?;
    Ok(())
}

/// Merge one trigram store (CS or CI) from its main + delta postings into a
/// fresh, dense baseline under `out_dir`, writing the four `{prefix}.*` files.
/// Returns the trigram count. Encodes trigrams in parallel in bounded chunks and
/// writes them serially in hash order.
#[allow(clippy::too_many_arguments)]
fn write_compacted_store(
    out_dir: &Path,
    prefix: &str,
    main_lookup: &[u8],
    main_count: usize,
    main_postings: &[u8],
    delta_lookup: &[LookupEntry],
    delta_postings: &[u8],
    remap: &[u32],
) -> Result<usize> {
    // 1. Build the hash-ordered merge-join work list: per trigram, its byte
    //    range in the main postings and/or the delta postings. On an equal hash
    //    we take main then delta — main's remapped ids are all below delta's, so
    //    the concatenation stays globally sorted. Cheap and serial.
    type Ranges = (u32, Option<(usize, usize)>, Option<(usize, usize)>);
    let mut items: Vec<Ranges> = Vec::with_capacity(main_count + delta_lookup.len());
    let (mut i, mut j) = (0usize, 0usize);
    while i < main_count || j < delta_lookup.len() {
        let hmain = (i < main_count).then(|| lookup_entry_at(main_lookup, i));
        let hdelta = (j < delta_lookup.len()).then(|| &delta_lookup[j]);
        let (take_main, take_delta) = match (hmain, hdelta) {
            (Some(m), Some(d)) => match m.0.cmp(&d.hash) {
                std::cmp::Ordering::Less => (true, false),
                std::cmp::Ordering::Greater => (false, true),
                std::cmp::Ordering::Equal => (true, true),
            },
            (Some(_), None) => (true, false),
            (None, Some(_)) => (false, true),
            (None, None) => unreachable!(),
        };
        let hash = if take_main {
            hmain.unwrap().0
        } else {
            hdelta.unwrap().hash
        };
        let mr = take_main.then(|| {
            let (_, off, len) = hmain.unwrap();
            i += 1;
            (off as usize, off as usize + len as usize)
        });
        let dr = take_delta.then(|| {
            let d = hdelta.unwrap();
            j += 1;
            (d.offset as usize, d.offset as usize + d.len as usize)
        });
        items.push((hash, mr, dr));
    }

    // 2. Encode postings + build bitmaps in parallel (each trigram is
    //    independent) in bounded chunks, OVERLAPPED with the file writes: a
    //    dedicated writer thread drains a small bounded channel while the rayon
    //    pool encodes the next chunk. Chunks arrive in hash order (FIFO) and
    //    rayon's collect preserves input order, so the lookup tables stay
    //    hash-sorted. Total time ≈ max(encode, write) instead of their sum.
    let timing = std::env::var_os("FGR_TIMING").is_some();
    let postings_path = out_dir.join(format!("{prefix}.postings"));
    let bitmaps_path = out_dir.join(format!("{prefix}.bitmaps"));

    // ~8K trigrams/chunk bounds peak memory (≤ ~4 chunks in flight) while
    // keeping every core busy within a chunk.
    const CHUNK: usize = 8_192;
    type Encoded = Vec<(u32, Vec<u8>, Vec<u8>)>;
    let (tx, rx) = std::sync::mpsc::sync_channel::<Encoded>(2);

    type WriterOut = (Vec<(u32, u64, u32)>, Vec<(u32, u64, u32)>, Duration);
    let writer: std::thread::JoinHandle<Result<WriterOut>> = std::thread::spawn(move || {
        let mut pf = BufWriter::with_capacity(8 << 20, File::create(&postings_path)?);
        let mut bf = BufWriter::with_capacity(8 << 20, File::create(&bitmaps_path)?);
        let mut lookup_entries: Vec<(u32, u64, u32)> = Vec::new();
        let mut bm_entries: Vec<(u32, u64, u32)> = Vec::new();
        let (mut poff, mut boff) = (0u64, 0u64);
        let mut t_write = Duration::ZERO;
        while let Ok(chunk) = rx.recv() {
            let t0 = Instant::now();
            for (hash, buf, bm_buf) in chunk {
                pf.write_all(&buf)?;
                lookup_entries.push((hash, poff, buf.len() as u32));
                poff += buf.len() as u64;
                bf.write_all(&bm_buf)?;
                bm_entries.push((hash, boff, bm_buf.len() as u32));
                boff += bm_buf.len() as u64;
            }
            t_write += t0.elapsed();
        }
        let t0 = Instant::now();
        pf.flush()?;
        bf.flush()?;
        t_write += t0.elapsed();
        Ok((lookup_entries, bm_entries, t_write))
    });

    let mut t_encode = Duration::ZERO;
    let mut t_send = Duration::ZERO;
    for chunk in items.chunks(CHUNK) {
        let t0 = Instant::now();
        let encoded: Encoded = chunk
            .par_iter()
            .filter_map(|&(hash, mr, dr)| {
                // Stream decode → remap → re-encode without materializing the
                // combined tuple list. Source blob sizes bound the output size
                // (remapped ids only get smaller). Main is pushed before delta,
                // so the stream stays globally sorted.
                let cap = mr.map_or(0, |(s, e)| e - s) + dr.map_or(0, |(s, e)| e - s);
                let mut buf = Vec::with_capacity(cap);
                let mut w = PostingWriter::new();
                let mut bitmap = RoaringBitmap::new();
                let mut last_doc = u32::MAX;
                let mut encode_range = |bytes: &[u8], buf: &mut Vec<u8>| {
                    for (doc_id, line_no, byte_offset) in PostingReader::new(bytes) {
                        let nd = remap[doc_id as usize];
                        if nd == u32::MAX {
                            continue;
                        }
                        w.push(buf, nd, line_no, byte_offset);
                        // Postings are doc-sorted: insert once per doc run
                        // instead of once per line.
                        if nd != last_doc {
                            bitmap.insert(nd);
                            last_doc = nd;
                        }
                    }
                };
                if let Some((s, e)) = mr {
                    encode_range(&main_postings[s..e], &mut buf);
                }
                if let Some((s, e)) = dr {
                    encode_range(&delta_postings[s..e], &mut buf);
                }
                // Trigram present only in dropped docs → omit it entirely.
                if buf.is_empty() {
                    return None;
                }
                let mut bm_buf = Vec::new();
                bitmap
                    .serialize_into(&mut bm_buf)
                    .expect("serialize bitmap into Vec");
                Some((hash, buf, bm_buf))
            })
            .collect();
        t_encode += t0.elapsed();

        let t0 = Instant::now();
        if tx.send(encoded).is_err() {
            break; // writer died; join below surfaces its error
        }
        t_send += t0.elapsed();
    }
    drop(tx);
    let (lookup_entries, bm_entries, t_write) = writer
        .join()
        .map_err(|_| anyhow::anyhow!("compaction writer thread panicked"))??;

    if timing {
        eprintln!(
            "[timing] {prefix}: encode={:.2}s send-wait={:.2}s write-thread={:.2}s trigrams={}",
            t_encode.as_secs_f64(),
            t_send.as_secs_f64(),
            t_write.as_secs_f64(),
            lookup_entries.len()
        );
    }

    write_lookup_file(&out_dir.join(format!("{prefix}.lookup")), &lookup_entries)?;
    write_lookup_file(
        &out_dir.join(format!("{prefix}.bitmaps.lookup")),
        &bm_entries,
    )?;
    Ok(lookup_entries.len())
}

/// Fold `pidx`'s delta and tombstones into a fresh, dense baseline written into
/// `out_dir`. Pure with respect to the live index: it does not touch `current`,
/// reclaim slots, or re-read any source file — it only remaps and re-encodes the
/// postings already in `pidx`. The atomic slot swap is layered on top separately.
pub fn compact_into(pidx: &PersistentIndex, out_dir: &Path) -> Result<CompactStats> {
    fs::create_dir_all(out_dir).context("creating compaction output dir")?;

    // Dense remap: walk old ids in order (main before delta), skip deleted,
    // assign sequential new ids. Live main ids land below live delta ids.
    let total = pidx.num_docs();
    let mut remap = vec![u32::MAX; total];
    let mut new_paths: Vec<PathBuf> = Vec::new();
    for old in 0..total as u32 {
        if pidx.deleted_docs.contains(&old) {
            continue;
        }
        let path = pidx
            .doc_path(old)
            .ok_or_else(|| anyhow::anyhow!("compaction: missing path for doc {old}"))?
            .to_path_buf();
        remap[old as usize] = new_paths.len() as u32;
        new_paths.push(path);
    }
    let live_docs = new_paths.len();

    // Case-sensitive store.
    let num_ngrams = write_compacted_store(
        out_dir,
        "ngrams",
        &pidx.lookup_mmap,
        pidx.lookup_count,
        &pidx.postings_mmap,
        &pidx.delta_lookup,
        &pidx.delta_postings,
        &remap,
    )?;

    // Case-insensitive companion, in lockstep with the same remap.
    if pidx.has_ci() {
        write_compacted_store(
            out_dir,
            "ngrams.ci",
            pidx.lookup_ci_mmap.as_ref().unwrap(),
            pidx.lookup_ci_count,
            pidx.postings_ci_mmap.as_ref().unwrap(),
            &pidx.delta_lookup_ci,
            &pidx.delta_postings_ci,
            &remap,
        )?;
    } else {
        remove_ci_files(out_dir);
    }

    // Dense docids.
    let mut docids_file = BufWriter::new(File::create(out_dir.join("docids.bin"))?);
    for path in &new_paths {
        let bytes = path.to_string_lossy();
        let bytes = bytes.as_bytes();
        docids_file.write_u16::<LittleEndian>(bytes.len() as u16)?;
        docids_file.write_all(bytes)?;
    }
    docids_file.flush()?;

    // Carry forward mtimes for survivors only — compaction reuses existing
    // postings, so the recorded mtime must match the indexed content, not a
    // fresh stat. A file changed since it was indexed keeps its old mtime here
    // and gets picked up by the next stale check / update.
    let survivors: HashSet<String> = new_paths
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let file_mtimes: HashMap<String, u64> = pidx
        .meta
        .file_mtimes
        .iter()
        .filter(|(p, _)| survivors.contains(*p))
        .map(|(p, m)| (p.clone(), *m))
        .collect();

    let meta = IndexMeta {
        version: INDEX_VERSION,
        num_docs: live_docs,
        num_ngrams,
        root_dir: pidx.meta.root_dir.clone(),
        built_at: chrono_now(),
        file_mtimes,
        dir_mtimes: pidx.meta.dir_mtimes.clone(),
        main_num_docs: Some(live_docs),
        case_insensitive: pidx.has_ci(),
    };
    fs::write(
        out_dir.join("meta.json"),
        serde_json::to_string_pretty(&meta)?,
    )?;

    // A compacted baseline has no delta/deleted overlay.
    for f in [
        "delta.postings",
        "delta.lookup",
        "delta.docids",
        "deleted.bin",
    ] {
        let _ = fs::remove_file(out_dir.join(f));
    }

    Ok(CompactStats {
        live_docs,
        dropped_docs: total - live_docs,
        num_ngrams,
    })
}

pub struct CompactOutcome {
    /// Whether a rebaseline actually happened (false = nothing to fold).
    pub compacted: bool,
    pub stats: Option<CompactStats>,
}

/// Rebaseline the index at `index_path`: stage a fresh baseline (delta folded,
/// tombstones dropped) into the non-live slot, then flip `current` to it and
/// reclaim the old slot. Serialized with other writers via the index lock;
/// readers never take the lock, so search is never blocked. A no-op
/// (`compacted = false`) when there is no delta and no tombstone to fold.
pub fn compact(index_path: &Path, verbose: bool) -> Result<CompactOutcome> {
    let (_lock, _waited) = acquire_index_lock(index_path)?;
    // Release the lock on every path (the index lock is not reentrant).
    let result = compact_no_lock(index_path, verbose);
    release_index_lock(index_path);
    result
}

/// Same as [`compact`] but WITHOUT acquiring the index lock — for callers that
/// already hold it (`fgr update`'s auto-compaction, the daemon). The lock is not
/// reentrant, so calling [`compact`] while holding it would deadlock.
pub fn compact_no_lock(index_path: &Path, verbose: bool) -> Result<CompactOutcome> {
    let pidx = load(index_path)?;

    // Already dense and delta-free → nothing to do.
    if pidx.delta_doc_ids.is_empty() && pidx.deleted_docs.is_empty() {
        return Ok(CompactOutcome {
            compacted: false,
            stats: None,
        });
    }

    // Stage into the non-live slot (reclaim it first — safe even if a reader
    // still maps it on the target platforms), then commit with a pointer flip.
    let slot = next_slot(index_path);
    let slot_dir = index_path.join(slot);
    let _ = fs::remove_dir_all(&slot_dir);
    let stats = compact_into(&pidx, &slot_dir)?;

    // Release our own mmaps before flipping + reclaiming the previous slot.
    drop(pidx);
    write_current(index_path, slot)?;
    cleanup_non_live(index_path, slot);

    if verbose {
        eprintln!(
            "Compacted: {} live docs ({} dropped), {} trigrams -> {}",
            stats.live_docs, stats.dropped_docs, stats.num_ngrams, slot
        );
    }

    Ok(CompactOutcome {
        compacted: true,
        stats: Some(stats),
    })
}

/// Consult the index config and rebaseline (via [`compact_no_lock`]) when the
/// post-update divergence in `stats` crosses the configured thresholds. The
/// caller MUST already hold the index lock (this folds in-place under it).
/// Returns `Ok(None)` when compaction isn't warranted or is disabled.
pub fn maybe_auto_compact(
    index_path: &Path,
    stats: &UpdateStats,
    verbose: bool,
) -> Result<Option<CompactOutcome>> {
    let cfg = crate::config::load(index_path);
    if cfg
        .compaction
        .should_compact(stats.main_docs, stats.delta_docs, stats.tombstones)
    {
        Ok(Some(compact_no_lock(index_path, verbose)?))
    } else {
        Ok(None)
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

    // Admission policy from the index's config (defaults on a first build,
    // before `config.toml` is written below). The same policy gates the
    // incremental update + stale walk, so all three agree on the file set.
    let admission = crate::config::Admission::from_config(&crate::config::load(output).index, root);

    fs::create_dir_all(output).context("creating output directory")?;

    // Stage the fresh baseline into the non-live slot, then flip `current` — so
    // any reader mapped on the previous baseline is undisturbed until the swap.
    let slot = next_slot(output);
    let slot_dir = output.join(slot);
    let _ = fs::remove_dir_all(&slot_dir);
    fs::create_dir_all(&slot_dir).context("creating slot directory")?;

    // Build the trigram files directly into the slot with a bounded buffer:
    // postings are spilled to sorted temp segments when the buffer fills and
    // k-way merged at the end, so peak RAM stays flat regardless of corpus size.
    // The budget comes from `[index] build_buffer_mb` (0 = unbounded). The
    // resulting `ngrams[.ci].*` files are byte-identical to the single-pass build.
    let budget = crate::config::load(output).index.build_budget_bytes();
    let out = crate::buildsort::build_bounded(
        root,
        &slot_dir,
        no_ignore,
        type_filter,
        verbose,
        case_insensitive,
        budget,
        &admission,
    )?;
    let postings_len = out.postings_len;

    // Collect file mtimes
    let mut file_mtimes = HashMap::new();
    for path in &out.doc_paths {
        if let Ok(m) = fs::metadata(path) {
            file_mtimes.insert(path.to_string_lossy().into_owned(), mtime_secs(&m));
        }
    }

    // Collect directory mtimes for fast stale detection. Exclude the whole
    // index root (`output`) so slots never count as corpus dirs.
    let dir_mtimes = collect_dir_mtimes(root, no_ignore, Some(output));

    // Write docids
    let docids_path = slot_dir.join("docids.bin");
    let mut docids_file = BufWriter::new(File::create(&docids_path)?);
    for path in &out.doc_paths {
        let path_bytes = path.to_string_lossy();
        let bytes = path_bytes.as_bytes();
        docids_file.write_u16::<LittleEndian>(bytes.len() as u16)?;
        docids_file.write_all(bytes)?;
    }
    docids_file.flush()?;

    // Write meta
    let num_docs = out.doc_paths.len();
    let meta = IndexMeta {
        version: INDEX_VERSION,
        num_docs,
        num_ngrams: out.num_ngrams,
        root_dir: root.to_string_lossy().into_owned(),
        built_at: chrono_now(),
        file_mtimes,
        dir_mtimes,
        main_num_docs: Some(num_docs),
        case_insensitive: out.has_ci,
    };
    let meta_path = slot_dir.join("meta.json");
    let meta_json = serde_json::to_string_pretty(&meta)?;
    fs::write(&meta_path, meta_json)?;

    // A full build has no delta/deleted overlay — make sure none linger in the
    // freshly staged slot (it was just recreated, so these are normally no-ops).
    let _ = fs::remove_file(slot_dir.join("delta.postings"));
    let _ = fs::remove_file(slot_dir.join("delta.lookup"));
    let _ = fs::remove_file(slot_dir.join("delta.docids"));
    let _ = fs::remove_file(slot_dir.join("deleted.bin"));

    // Commit: flip the pointer to the new slot, drop a stale root lock, then
    // reclaim the previous slot and any legacy flat content in the root.
    write_current(output, slot)?;
    let _ = fs::remove_file(output.join("lock"));
    cleanup_non_live(output, slot);

    // Drop a commented default config in the root (never clobbers an existing
    // one, so user edits survive rebuilds). Lives outside the slots, so
    // compaction never touches it.
    crate::config::write_default_if_absent(output);

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
    let idx_path = live_slot_dir(idx_path);
    match fs::read_to_string(idx_path.join("meta.json")) {
        Ok(s) => serde_json::from_str::<IndexMeta>(&s)
            .map(|m| m.version == INDEX_VERSION)
            .unwrap_or(false),
        Err(_) => false,
    }
}

/// True if an index (of any format version) exists at `idx_path`, resolving the
/// live slot. Unlike `is_current`, this does not check the format version — it
/// only answers "is there something to load here?".
pub fn index_exists(idx_path: &Path) -> bool {
    live_slot_dir(idx_path).join("meta.json").exists()
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
    // Resolve the live slot up front; every content path below is relative to
    // it. Legacy flat indexes resolve back to the root, so they keep loading.
    let slot = live_slot_dir(index_path);
    let index_path = slot.as_path();
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
    /// Post-update divergence, for the auto-compaction decision (see
    /// `CompactionConfig::should_compact`). `main_docs` is the baseline size,
    /// `delta_docs` the live delta count, `tombstones` the deleted-baseline
    /// count. The no-change early return leaves these at 0 (nothing new to
    /// fold — accumulated delta is handled by `fgr compact` / the next real
    /// update).
    pub main_docs: usize,
    pub delta_docs: usize,
    pub tombstones: usize,
}

/// Express the index directory in the *walk's* path space so `starts_with`
/// reliably excludes it from the corpus. `index_path` (from `--index`) and
/// `walk_root` (from the index's stored root) can be relative or absolute in any
/// mix, so a raw `starts_with` mismatches (e.g. absolute walk paths vs a
/// relative `.fgr`). We canonicalize both to find the index dir relative to the
/// root, then re-join it onto `walk_root` in its own form. Costs two syscalls
/// total (not per entry). Falls back to the raw index path.
fn index_dir_in_walk(index_path: &Path, walk_root: &Path) -> PathBuf {
    if let (Ok(idx_c), Ok(root_c)) = (fs::canonicalize(index_path), fs::canonicalize(walk_root)) {
        if let Ok(rel) = idx_c.strip_prefix(&root_c) {
            return walk_root.join(rel);
        }
    }
    index_path.to_path_buf()
}

/// Walk `root` with the parallel walker, collecting `(file mtimes, dir mtimes)`
/// keyed by path string, skipping the index directory subtree entirely.
/// The stat syscalls dominate a big tree's walk, so parallelizing brings the
/// 79K-file scan from ~3s to well under a second. `exclude_dir` must be in the
/// walk's path space (see `index_dir_in_walk`).
fn collect_tree_state(
    root: &Path,
    exclude_dir: &Path,
    admission: &crate::config::Admission,
) -> (HashMap<String, u64>, HashMap<String, u64>) {
    use ignore::WalkState;
    let files: std::sync::Mutex<HashMap<String, u64>> = std::sync::Mutex::new(HashMap::new());
    let dirs: std::sync::Mutex<HashMap<String, u64>> = std::sync::Mutex::new(HashMap::new());
    let walker = WalkBuilder::new(root)
        .git_ignore(true)
        .hidden(false)
        .build_parallel();
    walker.run(|| {
        Box::new(|entry| {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => return WalkState::Continue,
            };
            let path = entry.path();
            if path.starts_with(exclude_dir) {
                // Skip prunes the whole index subtree instead of visiting it.
                return WalkState::Skip;
            }
            if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                if let Ok(m) = entry.metadata() {
                    dirs.lock()
                        .unwrap()
                        .insert(path.to_string_lossy().into_owned(), mtime_secs(&m));
                }
            } else if entry.file_type().is_some_and(|ft| ft.is_file()) {
                if let Ok(m) = entry.metadata() {
                    // Apply the same admission policy as the build, so a binary
                    // or over-cap file never enters the tracked set (otherwise
                    // it would be classified "added" every update and churn).
                    // Anything but Admit (SkipBinary / SkipTooLarge / IO error)
                    // is simply left untracked.
                    if let Ok(crate::config::Candidate::Admit) =
                        crate::config::admit_file(path, Some(m.len()), admission)
                    {
                        files
                            .lock()
                            .unwrap()
                            .insert(path.to_string_lossy().into_owned(), mtime_secs(&m));
                    }
                }
            }
            WalkState::Continue
        })
    });
    (files.into_inner().unwrap(), dirs.into_inner().unwrap())
}

/// One delta file's line-level trigram postings, doc-id-free: hash →
/// sorted `(line_no, byte_offset)` pairs. Built in parallel per file; the doc
/// id is assigned later during the serial in-order merge.
struct DeltaFileIndex {
    ngrams: HashMap<u32, Vec<(u32, u32)>>,
    ngrams_ci: HashMap<u32, Vec<(u32, u32)>>,
}

/// Read + trigram one delta file (same line-level logic as
/// `SparseIndex::add_document`, plus the lockstep case-folded map when the
/// index has a CI companion). Returns `None` for unreadable or binary files —
/// those get no doc id, mirroring the previous serial loop.
fn index_delta_file(path_str: &str, ci_enabled: bool) -> Option<DeltaFileIndex> {
    let content = fs::read(Path::new(path_str)).ok()?;
    // Skip binary files
    if content.iter().take(512).any(|&b| b == 0) {
        return None;
    }

    let mut dfi = DeltaFileIndex {
        ngrams: HashMap::new(),
        ngrams_ci: HashMap::new(),
    };
    if content.len() < 3 {
        return Some(dfi);
    }

    let mut fold_buf: Vec<u8> = Vec::new();
    let mut seen_on_line: HashSet<[u8; 3]> = HashSet::new();
    let mut seen_on_line_ci: HashSet<[u8; 3]> = HashSet::new();
    let mut line_no = 1u32;
    let mut line_start = 0usize;

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
                    dfi.ngrams
                        .entry(hash)
                        .or_default()
                        .push((line_no, byte_offset));
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
                            dfi.ngrams_ci
                                .entry(hash)
                                .or_default()
                                .push((line_no, byte_offset));
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
    Some(dfi)
}

pub fn update_incremental(index_path: &Path, root: &Path, verbose: bool) -> Result<UpdateStats> {
    let start = Instant::now();

    // Content files live in the live slot; `index_path` (the root) is still used
    // for `load` (which resolves the slot itself) and for excluding the whole
    // index dir from the corpus walk below.
    let slot_dir = live_slot_dir(index_path);
    // The index dir as it appears within the walk, so we never index our own
    // files (`current`, the slots, meta/postings) as if they were corpus.
    let index_in_walk = index_dir_in_walk(index_path, root);

    // 1. Load meta.json — get saved file_mtimes
    let meta_path = slot_dir.join("meta.json");
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

    // 2. Walk root — get current file mtimes and directory mtimes (parallel),
    // applying the index's admission policy so binaries / over-cap files are
    // never tracked (must match the build to avoid churn).
    let admission =
        crate::config::Admission::from_config(&crate::config::load(index_path).index, root);
    let (current_files, new_dir_mtimes) = collect_tree_state(root, &index_in_walk, &admission);

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
            main_docs: main_num_docs,
            delta_docs: 0,
            tombstones: 0,
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
    //
    // Two phases: read + trigram every file in PARALLEL into per-file maps
    // (doc-id-free), then merge serially in list order assigning doc ids — so
    // the id assignment is identical to the old serial loop (files that are
    // unreadable or binary get no id), and postings within a hash stay sorted
    // by (doc_id, line).
    let ci_enabled = meta.case_insensitive;

    // 8b. Accumulate the delta with bounded memory: read + trigram files in
    // parallel chunks, merge each chunk into a compact accumulator (postings
    // encoded on the spot), and spill a sorted segment whenever the buffer
    // exceeds the build budget. A huge one-shot update (e.g. a branch switch
    // before the first update) therefore keeps flat memory like the full build.
    // Doc ids are assigned serially in `delta_files_to_index` order (unreadable/
    // binary files get none), so the delta stays byte-identical to the old
    // single-pass merge.
    let budget = crate::config::load(index_path).index.build_budget_bytes();
    let deltatmp = slot_dir.join(".deltatmp");
    if budget.is_some() {
        fs::create_dir_all(&deltatmp)?;
    }

    let mut cs_map: HashMap<u32, TrigramBuilder> = HashMap::new();
    let mut ci_map: HashMap<u32, TrigramBuilder> = HashMap::new();
    let mut cs_segs: Vec<PathBuf> = Vec::new();
    let mut ci_segs: Vec<PathBuf> = Vec::new();
    let mut delta_doc_ids: Vec<PathBuf> = Vec::new();
    let mut actual_added = 0usize;
    let mut actual_modified = 0usize;
    let mut buffered = 0usize;

    const DELTA_CHUNK: usize = 2048;
    for chunk in delta_files_to_index.chunks(DELTA_CHUNK) {
        let per_file: Vec<Option<DeltaFileIndex>> = chunk
            .par_iter()
            .map(|path_str| index_delta_file(path_str, ci_enabled))
            .collect();

        for (path_str, dfi) in chunk.iter().zip(per_file) {
            let Some(dfi) = dfi else { continue };

            // Doc_id in combined space: main_num_docs + delta_doc_ids.len()
            let doc_id = (main_num_docs + delta_doc_ids.len()) as u32;
            delta_doc_ids.push(PathBuf::from(path_str));

            if added_set.contains(path_str) {
                actual_added += 1;
            } else if modified_set.contains(path_str) {
                actual_modified += 1;
            }

            for (hash, lines) in dfi.ngrams {
                let b = cs_map.entry(hash).or_default();
                for (l, o) in lines {
                    let before = b.bytes.len();
                    b.push(doc_id, l, o);
                    buffered += b.bytes.len() - before;
                }
            }
            for (hash, lines) in dfi.ngrams_ci {
                let b = ci_map.entry(hash).or_default();
                for (l, o) in lines {
                    let before = b.bytes.len();
                    b.push(doc_id, l, o);
                    buffered += b.bytes.len() - before;
                }
            }
        }

        if let Some(budget) = budget {
            if buffered >= budget {
                let n = cs_segs.len();
                let cp = deltatmp.join(format!("cs-{n:05}.seg"));
                crate::buildsort::spill_u32(&mut cs_map, &cp)?;
                cs_segs.push(cp);
                if ci_enabled {
                    let ip = deltatmp.join(format!("ci-{n:05}.seg"));
                    crate::buildsort::spill_u32(&mut ci_map, &ip)?;
                    ci_segs.push(ip);
                }
                buffered = 0;
            }
        }
    }

    // 9. Write delta files (small -- only changed files)

    // Write deleted.bin: only main doc_ids that are deleted
    let deleted_path = slot_dir.join("deleted.bin");
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

    // Write the case-sensitive delta postings + lookup + docids.
    let delta_postings_path = slot_dir.join("delta.postings");
    let delta_lookup_path = slot_dir.join("delta.lookup");
    let delta_docids_path = slot_dir.join("delta.docids");

    if delta_doc_ids.is_empty() {
        let _ = fs::remove_file(&delta_postings_path);
        let _ = fs::remove_file(&delta_lookup_path);
        let _ = fs::remove_file(&delta_docids_path);
    } else {
        if cs_segs.is_empty() {
            crate::buildsort::write_delta_map(&cs_map, &delta_postings_path, &delta_lookup_path)?;
        } else {
            if !cs_map.is_empty() {
                let cp = deltatmp.join(format!("cs-{:05}.seg", cs_segs.len()));
                crate::buildsort::spill_u32(&mut cs_map, &cp)?;
                cs_segs.push(cp);
            }
            crate::buildsort::merge_delta_segments(
                &cs_segs,
                &delta_postings_path,
                &delta_lookup_path,
            )?;
        }

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
    // shared with the CS delta written above). Absent/empty when there is no CI
    // companion or the delta produced no case-folded postings.
    let ci_postings_path = slot_dir.join("delta.ci.postings");
    let ci_lookup_path = slot_dir.join("delta.ci.lookup");
    if ci_enabled && !(ci_map.is_empty() && ci_segs.is_empty()) {
        if ci_segs.is_empty() {
            crate::buildsort::write_delta_map(&ci_map, &ci_postings_path, &ci_lookup_path)?;
        } else {
            if !ci_map.is_empty() {
                let ip = deltatmp.join(format!("ci-{:05}.seg", ci_segs.len()));
                crate::buildsort::spill_u32(&mut ci_map, &ip)?;
                ci_segs.push(ip);
            }
            crate::buildsort::merge_delta_segments(&ci_segs, &ci_postings_path, &ci_lookup_path)?;
        }
    } else {
        let _ = fs::remove_file(&ci_postings_path);
        let _ = fs::remove_file(&ci_lookup_path);
    }

    let _ = fs::remove_dir_all(&deltatmp);

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
        main_docs: main_num_docs,
        delta_docs: delta_doc_ids.len(),
        tombstones: main_deleted.len(),
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
    // Same parallel collector as update_incremental (and the same walk-space
    // index-dir exclusion), so "stale" here agrees exactly with what an update
    // would classify as changed.
    let exclude = index_dir_in_walk(index_path, root);
    let admission =
        crate::config::Admission::from_config(&crate::config::load(index_path).index, root);
    let (files, dirs) = collect_tree_state(root, &exclude, &admission);

    for (path, mtime) in &dirs {
        match index.meta.dir_mtimes.get(path) {
            Some(&stored) if stored == *mtime => {}
            _ => return true, // new or changed directory
        }
    }
    for (path, mtime) in &files {
        match index.meta.file_mtimes.get(path) {
            Some(&stored) if stored == *mtime => {}
            _ => return true, // new or changed file
        }
    }
    // Deleted files: indexed but no longer produced by the walk.
    for path_str in index.meta.file_mtimes.keys() {
        if !files.contains_key(path_str) {
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

/// Non-blocking variant of [`acquire_index_lock`]: returns `Ok(None)` when the
/// lock is already held (e.g. by a background compaction) instead of waiting.
/// Lets the daemon's event loop skip an update round rather than block.
pub fn try_acquire_index_lock(idx_path: &Path) -> anyhow::Result<Option<fs::File>> {
    let lock_path = idx_path.join("lock");
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(f) => Ok(Some(f)),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn release_index_lock(idx_path: &Path) {
    let _ = fs::remove_file(idx_path.join("lock"));
}
