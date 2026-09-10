//! Bounded (external-merge) index build.
//!
//! The default build accumulates the whole inverted index in a
//! `HashMap<[u8;3], TrigramBuilder>` and only writes it at the end, so peak RAM
//! grows with the repository. This module keeps peak RAM flat: postings are
//! accumulated in a buffer of a configured size, and whenever it fills (checked
//! at a document boundary) the current map is **spilled** to a sorted temp
//! segment and cleared. After the walk, the segments are **k-way merged** by
//! trigram key and streamed straight to the final `{prefix}.*` files.
//!
//! Byte-identity: each trigram's posting blob is an independent delta chain
//! (see [`crate::postenc`]). Files are processed in order, so doc ids only
//! increase across segments; concatenating a key's per-segment blobs in segment
//! order yields the exact `(doc,line)`-ascending sequence the single-pass build
//! produces. Re-encoding that sequence and feeding it to the same
//! [`crate::persist::NgramFileWriter`] as the in-memory path makes the resulting
//! index byte-for-byte identical — no on-disk format change.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use ignore::WalkBuilder;

use crate::config::{self, Admission, Candidate};
use crate::index::{extract_document, TrigramBuilder};
use crate::persist::{remove_ci_files, write_ngram_files, NgramFileWriter};
use crate::postenc::{PostingReader, PostingWriter};
use crate::searcher::{is_known_text_ext, passes_type_filter};

/// What `build_bounded` produces beyond the on-disk `{prefix}.*` files, so the
/// caller can finish writing docids/meta/mtimes.
pub struct BuildOutput {
    pub doc_paths: Vec<PathBuf>,
    pub num_ngrams: usize,
    pub postings_len: u64,
    pub has_ci: bool,
}

/// Trigram key for this on-disk format: the packed u32 (see
/// [`crate::trigram::trigram_key`]), injective by construction.
#[inline]
fn key_of(tri: &[u8; 3]) -> u32 {
    crate::trigram::trigram_key(tri)
}

/// Spill one trigram map to a sorted segment: `[key u32][len u32][blob]` per
/// trigram, keys ascending. Clears the map.
fn spill(map: &mut HashMap<[u8; 3], TrigramBuilder>, path: &Path) -> Result<()> {
    let mut entries: Vec<(u32, &[u8])> = map
        .iter()
        .map(|(k, b)| (key_of(k), b.bytes.as_slice()))
        .collect();
    entries.sort_by_key(|(k, _)| *k);
    let mut w = BufWriter::new(File::create(path)?);
    for (k, blob) in &entries {
        w.write_u32::<LittleEndian>(*k)?;
        w.write_u32::<LittleEndian>(blob.len() as u32)?;
        w.write_all(blob)?;
    }
    w.flush()?;
    drop(entries);
    map.clear();
    Ok(())
}

/// A read cursor over one spilled segment, exposing the current `(key, blob)`
/// and advancing in key order.
struct SegmentCursor {
    rdr: BufReader<File>,
    key: u32,
    blob: Vec<u8>,
    done: bool,
}

impl SegmentCursor {
    fn open(path: &Path) -> Result<Self> {
        let mut c = SegmentCursor {
            rdr: BufReader::new(File::open(path)?),
            key: 0,
            blob: Vec::new(),
            done: false,
        };
        c.advance()?;
        Ok(c)
    }

    fn advance(&mut self) -> Result<()> {
        let key = match self.rdr.read_u32::<LittleEndian>() {
            Ok(k) => k,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                self.done = true;
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        let len = self.rdr.read_u32::<LittleEndian>()? as usize;
        self.blob.clear();
        self.blob.resize(len, 0);
        self.rdr.read_exact(&mut self.blob)?;
        self.key = key;
        Ok(())
    }
}

/// K-way merge the segments by key, calling `emit(key, blob)` once per distinct
/// key with its postings re-encoded into one continuous chain. For a key present
/// in several segments, the blobs are concatenated in segment order (= doc
/// order). Key-agnostic: used by both the full-index build (trigram keys) and
/// the delta build (posting-hash keys).
fn kway_merge<F>(segs: &[PathBuf], mut emit: F) -> Result<()>
where
    F: FnMut(u32, &[u8]) -> Result<()>,
{
    let mut cursors: Vec<SegmentCursor> = segs
        .iter()
        .map(|p| SegmentCursor::open(p))
        .collect::<Result<_>>()?;

    // Min-heap on (key, cursor_index); for equal keys the smaller index (earlier
    // segment = earlier docs) pops first, preserving global doc order.
    let mut heap: BinaryHeap<Reverse<(u32, usize)>> = BinaryHeap::new();
    for (i, c) in cursors.iter().enumerate() {
        if !c.done {
            heap.push(Reverse((c.key, i)));
        }
    }

    let mut blob = Vec::new();
    while let Some(&Reverse((min_key, _))) = heap.peek() {
        // Collect every segment currently at this key (ascending index).
        let mut group: Vec<usize> = Vec::new();
        while let Some(&Reverse((k, idx))) = heap.peek() {
            if k != min_key {
                break;
            }
            heap.pop();
            group.push(idx);
        }

        blob.clear();
        let mut pw = PostingWriter::new();
        for &idx in &group {
            for (doc, line, off) in PostingReader::new(&cursors[idx].blob) {
                pw.push(&mut blob, doc, line, off);
            }
        }
        emit(min_key, &blob)?;

        for idx in group {
            cursors[idx].advance()?;
            if !cursors[idx].done {
                heap.push(Reverse((cursors[idx].key, idx)));
            }
        }
    }
    Ok(())
}

/// K-way merge segments into the four `{prefix}.*` full-index files.
/// Returns the postings byte length and the number of trigrams written.
fn merge_segments(segs: &[PathBuf], out_dir: &Path, prefix: &str) -> Result<(u64, usize)> {
    let mut w = NgramFileWriter::create(out_dir, prefix)?;
    kway_merge(segs, |k, b| w.emit(k, b))?;
    w.finish()
}

// --- Delta build (bounded), used by persist::update_incremental ---
//
// The delta store is keyed directly by the u32 posting hash (not a trigram) and
// has no bitmaps — just `{name}.postings` + `{name}.lookup`. The accumulator map
// therefore uses the u32 hash as the key; everything else (segment format,
// spill, k-way merge) is shared with the full build.

/// Spill a u32-keyed trigram map to a sorted segment and clear it.
pub(crate) fn spill_u32(map: &mut HashMap<u32, TrigramBuilder>, path: &Path) -> Result<()> {
    let mut entries: Vec<(u32, &[u8])> =
        map.iter().map(|(k, b)| (*k, b.bytes.as_slice())).collect();
    entries.sort_by_key(|(k, _)| *k);
    let mut w = BufWriter::new(File::create(path)?);
    for (k, blob) in &entries {
        w.write_u32::<LittleEndian>(*k)?;
        w.write_u32::<LittleEndian>(blob.len() as u32)?;
        w.write_all(blob)?;
    }
    w.flush()?;
    drop(entries);
    map.clear();
    Ok(())
}

/// Streams a delta store's two files, emitting `(key, blob)` in ascending-key
/// order. `{name}.postings` = compact blobs concatenated; `{name}.lookup` =
/// `[key u32][off u64][len u32]` per key (key-sorted, binary-searched).
struct DeltaWriter {
    postings: BufWriter<File>,
    lookup: Vec<(u32, u64, u32)>,
    lookup_path: PathBuf,
    offset: u64,
}

impl DeltaWriter {
    fn create(postings_path: &Path, lookup_path: &Path) -> Result<Self> {
        Ok(Self {
            postings: BufWriter::new(File::create(postings_path)?),
            lookup: Vec::new(),
            lookup_path: lookup_path.to_path_buf(),
            offset: 0,
        })
    }

    fn emit(&mut self, key: u32, blob: &[u8]) -> Result<()> {
        self.postings.write_all(blob)?;
        self.lookup.push((key, self.offset, blob.len() as u32));
        self.offset += blob.len() as u64;
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        self.postings.flush()?;
        let mut lf = BufWriter::new(File::create(&self.lookup_path)?);
        for (key, off, len) in &self.lookup {
            lf.write_u32::<LittleEndian>(*key)?;
            lf.write_u64::<LittleEndian>(*off)?;
            lf.write_u32::<LittleEndian>(*len)?;
        }
        lf.flush()?;
        Ok(())
    }
}

/// Write a delta store directly from an in-memory u32-keyed map (non-spilling
/// path). Byte-identical to the merged output.
pub(crate) fn write_delta_map(
    map: &HashMap<u32, TrigramBuilder>,
    postings_path: &Path,
    lookup_path: &Path,
) -> Result<()> {
    let mut entries: Vec<(u32, &[u8])> =
        map.iter().map(|(k, b)| (*k, b.bytes.as_slice())).collect();
    entries.sort_by_key(|(k, _)| *k);
    let mut w = DeltaWriter::create(postings_path, lookup_path)?;
    for (k, blob) in &entries {
        w.emit(*k, blob)?;
    }
    w.finish()
}

/// K-way merge spilled delta segments into `{name}.postings` + `{name}.lookup`.
pub(crate) fn merge_delta_segments(
    segs: &[PathBuf],
    postings_path: &Path,
    lookup_path: &Path,
) -> Result<()> {
    let mut w = DeltaWriter::create(postings_path, lookup_path)?;
    kway_merge(segs, |k, b| w.emit(k, b))?;
    w.finish()
}

/// Build the index for `root` into `slot_dir`, keeping peak RAM near `budget`
/// bytes (`None` = unbounded: assemble in RAM and write in one pass, identical
/// to the historical build). Writes `ngrams[.ci].*`; the caller writes
/// docids/meta/mtimes from the returned [`BuildOutput`].
#[allow(clippy::too_many_arguments)]
pub fn build_bounded(
    root: &Path,
    slot_dir: &Path,
    no_ignore: bool,
    type_filter: &[String],
    verbose: bool,
    case_insensitive: bool,
    budget: Option<usize>,
    admission: &Admission,
) -> Result<BuildOutput> {
    // Phase 1: collect file paths (matches index::build_from_directory).
    let walker = WalkBuilder::new(root)
        .git_ignore(!no_ignore)
        .hidden(false)
        .build();
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in walker {
        let entry = entry?;
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        if !passes_type_filter(path, type_filter) {
            continue;
        }
        paths.push(path.to_path_buf());
    }

    // Phase 2: accumulate, spilling sorted segments when the buffer fills.
    let seg_dir = slot_dir.join(".segtmp");
    if budget.is_some() {
        fs::create_dir_all(&seg_dir)?;
    }
    let mut ngrams: HashMap<[u8; 3], TrigramBuilder> = HashMap::new();
    let mut ngrams_ci: Option<HashMap<[u8; 3], TrigramBuilder>> =
        case_insensitive.then(HashMap::new);
    let mut doc_paths: Vec<PathBuf> = Vec::new();
    let mut fold_buf = Vec::new();
    let mut buffered = 0usize;
    let mut cs_segs: Vec<PathBuf> = Vec::new();
    let mut ci_segs: Vec<PathBuf> = Vec::new();
    let mut count = 0u32;
    let (mut skipped_binary, mut skipped_large) = (0u32, 0u32);

    for path in &paths {
        // Same admission policy as `index::build_from_directory` (and the
        // incremental update + stale walk): known binaries and over-cap files
        // are skipped without reading their bodies.
        match config::admit_file(path, None, admission) {
            Ok(Candidate::SkipBinary) => {
                skipped_binary += 1;
                continue;
            }
            Ok(Candidate::SkipTooLarge) => {
                skipped_large += 1;
                continue;
            }
            Ok(Candidate::Admit) => {}
            Err(_) => continue,
        }
        let content = match std::fs::read(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // Content backstop: reject binaries that slipped past the extension
        // check; known/configured text extensions are trusted and bypass it.
        if !admission.is_text_ext(path)
            && !is_known_text_ext(path)
            && content.iter().take(512).any(|&b| b == 0)
        {
            skipped_binary += 1;
            continue;
        }

        let doc_id = doc_paths.len() as u32;
        buffered += extract_document(
            doc_id,
            &content,
            &mut ngrams,
            ngrams_ci.as_mut(),
            &mut fold_buf,
        );
        doc_paths.push(path.clone());
        count += 1;
        if verbose && count % 10000 == 0 {
            eprintln!("  indexed {count} files...");
        }

        if let Some(budget) = budget {
            if buffered >= budget {
                let n = cs_segs.len();
                let cs_path = seg_dir.join(format!("cs-{n:05}.seg"));
                spill(&mut ngrams, &cs_path)?;
                cs_segs.push(cs_path);
                if let Some(ci) = ngrams_ci.as_mut() {
                    let ci_path = seg_dir.join(format!("ci-{n:05}.seg"));
                    spill(ci, &ci_path)?;
                    ci_segs.push(ci_path);
                }
                buffered = 0;
            }
        }
    }

    // Phase 3: write the final ngram files.
    let (num_ngrams, postings_len) = if cs_segs.is_empty() {
        // Never spilled (unbounded, or the whole corpus fit the buffer): the
        // historical single-pass write. Zero merge overhead.
        let postings_len = write_ngram_files(slot_dir, "ngrams", &ngrams)?;
        if let Some(ci) = &ngrams_ci {
            write_ngram_files(slot_dir, "ngrams.ci", ci)?;
        } else {
            remove_ci_files(slot_dir);
        }
        (ngrams.len(), postings_len)
    } else {
        // Spill the residual maps as the last segment, then k-way merge.
        let n = cs_segs.len();
        let cs_path = seg_dir.join(format!("cs-{n:05}.seg"));
        spill(&mut ngrams, &cs_path)?;
        cs_segs.push(cs_path);
        if let Some(ci) = ngrams_ci.as_mut() {
            let ci_path = seg_dir.join(format!("ci-{n:05}.seg"));
            spill(ci, &ci_path)?;
            ci_segs.push(ci_path);
        }

        let (postings_len, num) = merge_segments(&cs_segs, slot_dir, "ngrams")?;
        if ci_segs.is_empty() {
            remove_ci_files(slot_dir);
        } else {
            merge_segments(&ci_segs, slot_dir, "ngrams.ci")?;
        }
        (num, postings_len)
    };

    let _ = fs::remove_dir_all(&seg_dir);

    if verbose {
        eprintln!(
            "  indexed {count} files total, {num_ngrams} trigrams ({skipped_binary} binary, {skipped_large} too-large skipped)"
        );
    }

    Ok(BuildOutput {
        doc_paths,
        num_ngrams,
        postings_len,
        has_ci: ngrams_ci.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_into(corpus: &Path, slot: &Path, ci: bool, budget: Option<usize>) -> BuildOutput {
        fs::create_dir_all(slot).unwrap();
        let adm = Admission::from_config(&config::IndexConfig::default(), corpus);
        build_bounded(corpus, slot, true, &[], false, ci, budget, &adm).unwrap()
    }

    fn assert_ngram_files_eq(a: &Path, b: &Path, prefix: &str) {
        for suffix in ["postings", "lookup", "bitmaps", "bitmaps.lookup"] {
            let fa = a.join(format!("{prefix}.{suffix}"));
            let fb = b.join(format!("{prefix}.{suffix}"));
            assert_eq!(
                fs::read(&fa).unwrap(),
                fs::read(&fb).unwrap(),
                "{prefix}.{suffix} differs between bounded and unbounded builds"
            );
        }
    }

    #[test]
    fn segment_write_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut map: HashMap<[u8; 3], TrigramBuilder> = HashMap::new();
        let mut fold = Vec::new();
        extract_document(0, b"hello world\n", &mut map, None, &mut fold);
        extract_document(1, b"hello again\n", &mut map, None, &mut fold);

        let mut expected: Vec<(u32, Vec<u8>)> = map
            .iter()
            .map(|(k, b)| (key_of(k), b.bytes.clone()))
            .collect();
        expected.sort_by_key(|(k, _)| *k);

        let seg = dir.path().join("s.seg");
        spill(&mut map, &seg).unwrap();
        assert!(map.is_empty(), "spill clears the map");

        let mut cur = SegmentCursor::open(&seg).unwrap();
        let mut got = Vec::new();
        while !cur.done {
            got.push((cur.key, cur.blob.clone()));
            cur.advance().unwrap();
        }
        assert_eq!(got, expected);
    }

    /// A tiny budget forces a spill after every document, so the k-way merge
    /// runs across many segments (including the same trigram spanning several).
    /// Its output must be byte-identical to the single-pass (unbounded) build.
    #[test]
    fn bounded_build_is_byte_identical_to_unbounded() {
        let corpus = tempfile::tempdir().unwrap();
        for i in 0..25 {
            let mut s = String::new();
            for j in 0..40 {
                s.push_str(&format!(
                    "fn function_{i}_{j} calls helper then returns a value\n"
                ));
            }
            fs::write(corpus.path().join(format!("f{i}.txt")), s).unwrap();
        }

        let bounded = tempfile::tempdir().unwrap();
        let unbounded = tempfile::tempdir().unwrap();
        let ob = build_into(corpus.path(), bounded.path(), false, Some(1));
        let ou = build_into(corpus.path(), unbounded.path(), false, None);

        assert_eq!(ob.num_ngrams, ou.num_ngrams);
        assert_eq!(ob.doc_paths.len(), ou.doc_paths.len());
        assert_eq!(ob.postings_len, ou.postings_len);
        assert_ngram_files_eq(bounded.path(), unbounded.path(), "ngrams");
    }

    /// The delta merge (spill segments → k-way merge) must produce the same
    /// `delta.postings`/`delta.lookup` bytes as writing the accumulator directly.
    #[test]
    fn delta_merge_is_byte_identical_to_direct() {
        let dir = tempfile::tempdir().unwrap();

        // Direct: one accumulator holding all docs' postings, pushed in doc order.
        let mut full: HashMap<u32, TrigramBuilder> = HashMap::new();
        // Segmented: three per-batch accumulators (doc ranges 0..10, 10..20, 20..30).
        let mut batches: Vec<HashMap<u32, TrigramBuilder>> =
            vec![HashMap::new(), HashMap::new(), HashMap::new()];

        for doc in 0u32..30 {
            // A handful of overlapping hash keys per doc, several lines each.
            for k in 0u32..40 {
                let hash = k.wrapping_mul(2_654_435_761) & 0x0000_FFFF; // spread, with collisions across docs
                for line in 1u32..=3 {
                    let off = line * 7;
                    full.entry(hash).or_default().push(doc, line, off);
                    batches[(doc / 10) as usize]
                        .entry(hash)
                        .or_default()
                        .push(doc, line, off);
                }
            }
        }

        let direct_p = dir.path().join("d.postings");
        let direct_l = dir.path().join("d.lookup");
        write_delta_map(&full, &direct_p, &direct_l).unwrap();

        let mut segs = Vec::new();
        for (i, mut b) in batches.into_iter().enumerate() {
            let p = dir.path().join(format!("seg-{i}.seg"));
            spill_u32(&mut b, &p).unwrap();
            segs.push(p);
        }
        let merged_p = dir.path().join("m.postings");
        let merged_l = dir.path().join("m.lookup");
        merge_delta_segments(&segs, &merged_p, &merged_l).unwrap();

        assert_eq!(fs::read(&direct_p).unwrap(), fs::read(&merged_p).unwrap());
        assert_eq!(fs::read(&direct_l).unwrap(), fs::read(&merged_l).unwrap());
    }

    #[test]
    fn bounded_build_is_byte_identical_with_ci() {
        let corpus = tempfile::tempdir().unwrap();
        for i in 0..20 {
            let mut s = String::new();
            for j in 0..40 {
                s.push_str(&format!(
                    "Fn Function_{i}_{j} Calls HELPER then Returns VALUE\n"
                ));
            }
            fs::write(corpus.path().join(format!("f{i}.txt")), s).unwrap();
        }

        let bounded = tempfile::tempdir().unwrap();
        let unbounded = tempfile::tempdir().unwrap();
        build_into(corpus.path(), bounded.path(), true, Some(1));
        build_into(corpus.path(), unbounded.path(), true, None);

        assert_ngram_files_eq(bounded.path(), unbounded.path(), "ngrams");
        assert_ngram_files_eq(bounded.path(), unbounded.path(), "ngrams.ci");
    }
}
