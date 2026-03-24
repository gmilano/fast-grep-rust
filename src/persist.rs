use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use ignore::WalkBuilder;
use memmap2::Mmap;

use crate::index::{Posting, SparseIndex};
use crate::trigram;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct IndexMeta {
    pub version: u32,
    pub num_docs: usize,
    pub num_ngrams: usize,
    pub root_dir: String,
    pub built_at: String,
    pub file_mtimes: HashMap<String, u64>,
}

#[derive(Clone)]
pub struct LookupEntry {
    pub hash: u32,
    pub offset: u64,
    pub len: u32,
}

pub struct PersistentIndex {
    pub lookup: Vec<LookupEntry>,
    pub postings_mmap: Mmap,
    pub doc_ids: Vec<PathBuf>,
    pub meta: IndexMeta,
}

impl PersistentIndex {
    pub fn search(&self, pattern: &str) -> Vec<&Path> {
        let alternatives = trigram::decompose_pattern(pattern);
        let ordered_alternatives = trigram::decompose_pattern_ordered(pattern);

        if alternatives.is_empty() || alternatives.iter().all(|a| a.is_empty()) {
            return self.doc_ids.iter().map(|p| p.as_path()).collect();
        }

        let mut result_docs: std::collections::HashSet<u32> = std::collections::HashSet::new();

        for (i, alt_trigrams) in alternatives.iter().enumerate() {
            if alt_trigrams.is_empty() {
                return self.doc_ids.iter().map(|p| p.as_path()).collect();
            }

            // Check all trigrams exist
            let postings_list: Vec<Option<Vec<Posting>>> = alt_trigrams
                .iter()
                .map(|tri| self.lookup_trigram(tri))
                .collect();

            if postings_list.iter().any(|p| p.is_none()) {
                continue;
            }

            let postings_list: Vec<Vec<Posting>> =
                postings_list.into_iter().map(|p| p.unwrap()).collect();

            // Sort by posting list size for fast intersection
            let mut indices: Vec<usize> = (0..alt_trigrams.len()).collect();
            indices.sort_by_key(|&idx| postings_list[idx].len());

            // Intersect on doc_ids
            let first = &postings_list[indices[0]];
            let mut candidate_docs: std::collections::HashSet<u32> =
                first.iter().map(|&(doc_id, _, _)| doc_id).collect();

            for &idx in &indices[1..] {
                if candidate_docs.is_empty() {
                    break;
                }
                let doc_set: std::collections::HashSet<u32> =
                    postings_list[idx].iter().map(|&(doc_id, _, _)| doc_id).collect();
                candidate_docs.retain(|d| doc_set.contains(d));
            }

            // Adjacency filtering with position masks (ordered trigrams)
            let ordered = &ordered_alternatives[i];
            if ordered.len() >= 2 && !candidate_docs.is_empty() {
                // Build a lookup for ordered trigrams from their postings
                let ordered_postings: Vec<Option<Vec<Posting>>> = ordered
                    .iter()
                    .map(|tri| self.lookup_trigram(tri))
                    .collect();

                for pair_idx in 0..ordered.len() - 1 {
                    let masks_a: HashMap<u32, (u8, u8)> = ordered_postings[pair_idx]
                        .as_ref()
                        .map(|p| p.iter()
                            .filter(|(doc_id, _, _)| candidate_docs.contains(doc_id))
                            .map(|&(doc_id, loc, next)| (doc_id, (loc, next)))
                            .collect())
                        .unwrap_or_default();
                    let masks_b: HashMap<u32, (u8, u8)> = ordered_postings[pair_idx + 1]
                        .as_ref()
                        .map(|p| p.iter()
                            .filter(|(doc_id, _, _)| candidate_docs.contains(doc_id))
                            .map(|&(doc_id, loc, next)| (doc_id, (loc, next)))
                            .collect())
                        .unwrap_or_default();

                    candidate_docs.retain(|doc_id| {
                        if let (Some((_, next_a)), Some((loc_b, _))) =
                            (masks_a.get(doc_id), masks_b.get(doc_id))
                        {
                            next_a & loc_b != 0
                        } else {
                            false
                        }
                    });

                    if candidate_docs.is_empty() {
                        break;
                    }
                }
            }

            if i == 0 {
                result_docs = candidate_docs;
            } else {
                result_docs.extend(candidate_docs);
            }
        }

        result_docs
            .iter()
            .filter_map(|&id| self.doc_ids.get(id as usize).map(|p| p.as_path()))
            .collect()
    }

    fn lookup_trigram(&self, trigram: &[u8; 3]) -> Option<Vec<Posting>> {
        let hash = crc32fast::hash(trigram);
        let idx = self
            .lookup
            .binary_search_by_key(&hash, |e| e.hash)
            .ok()?;

        let entry = &self.lookup[idx];
        let start = entry.offset as usize;
        let end = start + entry.len as usize;

        if end > self.postings_mmap.len() {
            return None;
        }

        let data = &self.postings_mmap[start..end];
        // Each posting is 6 bytes: u32 doc_id + u8 loc_mask + u8 next_mask
        let posting_size = 6;
        if data.len() % posting_size != 0 {
            return None;
        }

        let mut postings = Vec::with_capacity(data.len() / posting_size);
        let mut cursor = std::io::Cursor::new(data);
        while (cursor.position() as usize) < data.len() {
            let doc_id = cursor.read_u32::<LittleEndian>().ok()?;
            let loc_mask = cursor.read_u8().ok()?;
            let next_mask = cursor.read_u8().ok()?;
            postings.push((doc_id, loc_mask, next_mask));
        }

        Some(postings)
    }

    pub fn is_stale(&self) -> bool {
        for (path_str, &stored_mtime) in self.meta.file_mtimes.iter().take(100) {
            let path = Path::new(path_str);
            match fs::metadata(path) {
                Ok(meta) => {
                    let mtime = meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    if mtime != stored_mtime {
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

    // Collect file mtimes
    let mut file_mtimes = HashMap::new();
    for path in &index.doc_ids {
        if let Ok(meta) = fs::metadata(path) {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            file_mtimes.insert(path.to_string_lossy().into_owned(), mtime);
        }
    }

    // Write postings and build lookup
    let postings_path = output.join("ngrams.postings");
    let mut postings_file = BufWriter::new(File::create(&postings_path)?);
    let mut lookup_entries: Vec<(u32, u64, u32)> = Vec::new();
    let mut offset: u64 = 0;

    // Sort trigrams by hash for binary search
    let mut trigram_list: Vec<(&[u8; 3], &Vec<Posting>)> = index.ngrams.iter().collect();
    trigram_list.sort_by_key(|(k, _)| crc32fast::hash(*k));

    for (tri, postings) in &trigram_list {
        let mut buf = Vec::with_capacity(postings.len() * 6);
        for &(doc_id, loc_mask, next_mask) in *postings {
            buf.write_u32::<LittleEndian>(doc_id)?;
            buf.write_u8(loc_mask)?;
            buf.write_u8(next_mask)?;
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
    let meta = IndexMeta {
        version: 2, // bumped for new posting format
        num_docs: index.doc_ids.len(),
        num_ngrams: index.ngrams.len(),
        root_dir: root.to_string_lossy().into_owned(),
        built_at: chrono_now(),
        file_mtimes,
    };
    let meta_path = output.join("meta.json");
    let meta_json = serde_json::to_string_pretty(&meta)?;
    fs::write(&meta_path, meta_json)?;

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

    let lookup_path = index_path.join("ngrams.lookup");
    let lookup_data = fs::read(&lookup_path).context("reading ngrams.lookup")?;
    let entry_size = 4 + 8 + 4;
    let num_entries = lookup_data.len() / entry_size;
    let mut lookup = Vec::with_capacity(num_entries);
    let mut cursor = std::io::Cursor::new(&lookup_data);
    for _ in 0..num_entries {
        let hash = cursor.read_u32::<LittleEndian>()?;
        let offset = cursor.read_u64::<LittleEndian>()?;
        let len = cursor.read_u32::<LittleEndian>()?;
        lookup.push(LookupEntry { hash, offset, len });
    }

    let postings_path = index_path.join("ngrams.postings");
    let postings_file = File::open(&postings_path).context("opening ngrams.postings")?;
    let postings_mmap = unsafe { Mmap::map(&postings_file)? };

    let docids_path = index_path.join("docids.bin");
    let docids_data = fs::read(&docids_path).context("reading docids.bin")?;
    let mut doc_ids = Vec::new();
    let mut cursor = std::io::Cursor::new(&docids_data);
    while (cursor.position() as usize) < docids_data.len() {
        let len = cursor.read_u16::<LittleEndian>()? as usize;
        let pos = cursor.position() as usize;
        if pos + len > docids_data.len() {
            break;
        }
        let path_str = std::str::from_utf8(&docids_data[pos..pos + len])?;
        doc_ids.push(PathBuf::from(path_str));
        cursor.set_position((pos + len) as u64);
    }

    Ok(PersistentIndex {
        lookup,
        postings_mmap,
        doc_ids,
        meta,
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

    // 2. Walk root — get current file mtimes
    let walker = WalkBuilder::new(root)
        .git_ignore(true)
        .hidden(false)
        .build();
    let mut current_files: HashMap<String, u64> = HashMap::new();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        if let Ok(m) = fs::metadata(path) {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            current_files.insert(path.to_string_lossy().into_owned(), mtime);
        }
    }

    // 3. Classify: added, modified, deleted
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

    // 5. Load existing index metadata (lookup + docids + mmap)
    let pidx = load(index_path)?;

    // Build set of doc_ids to remove (deleted + modified; modified will be re-added)
    let remove_ids: HashSet<u32> = pidx
        .doc_ids
        .iter()
        .enumerate()
        .filter_map(|(id, path)| {
            let ps = path.to_string_lossy();
            if deleted_set.contains(ps.as_ref()) || modified_set.contains(ps.as_ref()) {
                Some(id as u32)
            } else {
                None
            }
        })
        .collect();

    // Build new doc_ids list (keep unchanged files, remap IDs)
    let mut new_doc_ids: Vec<PathBuf> = Vec::with_capacity(pidx.doc_ids.len());
    let mut old_to_new: HashMap<u32, u32> = HashMap::new();
    for (old_id, path) in pidx.doc_ids.iter().enumerate() {
        if remove_ids.contains(&(old_id as u32)) {
            continue;
        }
        let new_id = new_doc_ids.len() as u32;
        old_to_new.insert(old_id as u32, new_id);
        new_doc_ids.push(path.clone());
    }

    // 6. Index added/modified files — collect new postings in a small HashMap
    let mut actual_added = 0usize;
    let mut actual_modified = 0usize;
    let mut new_postings: HashMap<u32, Vec<Posting>> = HashMap::new();
    for path_str in added_set.iter().chain(modified_set.iter()) {
        let path = Path::new(path_str);
        let content = match fs::read(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if content.iter().take(512).any(|&b| b == 0) {
            continue;
        }

        let doc_id = new_doc_ids.len() as u32;
        new_doc_ids.push(path.to_path_buf());

        if added_set.contains(path_str) {
            actual_added += 1;
        } else {
            actual_modified += 1;
        }

        if content.len() < 3 {
            continue;
        }

        let mut masks: HashMap<[u8; 3], (u8, u8)> = HashMap::new();
        for (pos, w) in content.windows(3).enumerate() {
            let tri = [w[0], w[1], w[2]];
            let loc_bit = 1u8 << (pos % 8) as u32;
            let next_bit = 1u8 << ((pos + 1) % 8) as u32;
            let entry = masks.entry(tri).or_insert((0u8, 0u8));
            entry.0 |= loc_bit;
            entry.1 |= next_bit;
        }

        for (tri, (loc_mask, next_mask)) in masks {
            let hash = crc32fast::hash(&tri);
            new_postings
                .entry(hash)
                .or_default()
                .push((doc_id, loc_mask, next_mask));
        }
    }

    // Collect and sort new-only hashes (not in existing lookup)
    let existing_hashes: HashSet<u32> = pidx.lookup.iter().map(|e| e.hash).collect();
    let mut new_only_hashes: Vec<u32> = new_postings
        .keys()
        .filter(|h| !existing_hashes.contains(h))
        .copied()
        .collect();
    new_only_hashes.sort();

    // 7. Streaming merge: read existing postings from mmap, filter/remap,
    //    merge with new postings, write directly to output file.
    //    This avoids loading 757MB of postings into a HashMap.
    let tmp_postings_path = index_path.join("ngrams.postings.tmp");
    let mut postings_out = BufWriter::new(File::create(&tmp_postings_path)?);
    let mut lookup_entries: Vec<(u32, u64, u32)> = Vec::new();
    let mut write_offset: u64 = 0;
    let mut num_ngrams = 0usize;

    let needs_filter = !remove_ids.is_empty();

    // Process existing posting lists + any new postings for the same hash
    let mut new_only_idx = 0;
    for entry in &pidx.lookup {
        // Emit any new-only hashes that sort before this existing hash
        while new_only_idx < new_only_hashes.len() && new_only_hashes[new_only_idx] < entry.hash {
            let nh = new_only_hashes[new_only_idx];
            if let Some(postings) = new_postings.get(&nh) {
                let mut buf = Vec::with_capacity(postings.len() * 6);
                for &(doc_id, loc_mask, next_mask) in postings {
                    buf.write_u32::<LittleEndian>(doc_id)?;
                    buf.write_u8(loc_mask)?;
                    buf.write_u8(next_mask)?;
                }
                let len = buf.len() as u32;
                postings_out.write_all(&buf)?;
                lookup_entries.push((nh, write_offset, len));
                write_offset += len as u64;
                num_ngrams += 1;
            }
            new_only_idx += 1;
        }

        let s = entry.offset as usize;
        let e = s + entry.len as usize;
        if e > pidx.postings_mmap.len() {
            continue;
        }
        let data = &pidx.postings_mmap[s..e];

        // Stream through existing postings: filter removed, remap IDs
        let mut buf = Vec::with_capacity(data.len());
        let mut cursor = std::io::Cursor::new(data);
        while (cursor.position() as usize) < data.len() {
            let doc_id = cursor.read_u32::<LittleEndian>()?;
            let loc_mask = cursor.read_u8()?;
            let next_mask = cursor.read_u8()?;
            if needs_filter && remove_ids.contains(&doc_id) {
                continue;
            }
            let mapped_id = if needs_filter {
                old_to_new[&doc_id]
            } else {
                doc_id
            };
            buf.write_u32::<LittleEndian>(mapped_id)?;
            buf.write_u8(loc_mask)?;
            buf.write_u8(next_mask)?;
        }

        // Append new postings for this same hash
        if let Some(extra) = new_postings.get(&entry.hash) {
            for &(doc_id, loc_mask, next_mask) in extra {
                buf.write_u32::<LittleEndian>(doc_id)?;
                buf.write_u8(loc_mask)?;
                buf.write_u8(next_mask)?;
            }
        }

        if !buf.is_empty() {
            let len = buf.len() as u32;
            postings_out.write_all(&buf)?;
            lookup_entries.push((entry.hash, write_offset, len));
            write_offset += len as u64;
            num_ngrams += 1;
        }
    }

    // Emit remaining new-only hashes
    while new_only_idx < new_only_hashes.len() {
        let nh = new_only_hashes[new_only_idx];
        if let Some(postings) = new_postings.get(&nh) {
            let mut buf = Vec::with_capacity(postings.len() * 6);
            for &(doc_id, loc_mask, next_mask) in postings {
                buf.write_u32::<LittleEndian>(doc_id)?;
                buf.write_u8(loc_mask)?;
                buf.write_u8(next_mask)?;
            }
            let len = buf.len() as u32;
            postings_out.write_all(&buf)?;
            lookup_entries.push((nh, write_offset, len));
            write_offset += len as u64;
            num_ngrams += 1;
        }
        new_only_idx += 1;
    }
    postings_out.flush()?;

    // Drop the mmap before renaming files
    drop(pidx);

    // Atomically replace postings file
    let postings_path = index_path.join("ngrams.postings");
    fs::rename(&tmp_postings_path, &postings_path)?;

    // Write lookup table
    let lookup_path = index_path.join("ngrams.lookup");
    let mut lookup_file = BufWriter::new(File::create(&lookup_path)?);
    for (hash, off, len) in &lookup_entries {
        lookup_file.write_u32::<LittleEndian>(*hash)?;
        lookup_file.write_u64::<LittleEndian>(*off)?;
        lookup_file.write_u32::<LittleEndian>(*len)?;
    }
    lookup_file.flush()?;

    // Write docids
    let docids_path = index_path.join("docids.bin");
    let mut docids_file = BufWriter::new(File::create(&docids_path)?);
    for path in &new_doc_ids {
        let path_bytes = path.to_string_lossy();
        let bytes = path_bytes.as_bytes();
        docids_file.write_u16::<LittleEndian>(bytes.len() as u16)?;
        docids_file.write_all(bytes)?;
    }
    docids_file.flush()?;

    // 8. Update meta.json with new mtimes
    let mut new_mtimes: HashMap<String, u64> = HashMap::with_capacity(new_doc_ids.len());
    for path in &new_doc_ids {
        let ps = path.to_string_lossy().into_owned();
        if let Some(&mtime) = current_files.get(&ps) {
            new_mtimes.insert(ps, mtime);
        } else if let Some(&mtime) = saved_mtimes.get(&ps) {
            new_mtimes.insert(ps, mtime);
        }
    }
    let new_meta = IndexMeta {
        version: 2,
        num_docs: new_doc_ids.len(),
        num_ngrams,
        root_dir: root.to_string_lossy().into_owned(),
        built_at: chrono_now(),
        file_mtimes: new_mtimes,
    };
    let meta_json = serde_json::to_string_pretty(&new_meta)?;
    fs::write(&meta_path, meta_json)?;

    let unchanged = new_doc_ids.len() - actual_added - actual_modified;

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

fn chrono_now() -> String {
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}s_since_epoch", dur.as_secs())
}
