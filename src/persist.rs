use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use memmap2::Mmap;
use roaring::RoaringBitmap;

use crate::index::SparseIndex;
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

        if alternatives.is_empty() || alternatives.iter().all(|a| a.is_empty()) {
            return self.doc_ids.iter().map(|p| p.as_path()).collect();
        }

        let mut result_bitmap = RoaringBitmap::new();

        for (i, alt_trigrams) in alternatives.iter().enumerate() {
            if alt_trigrams.is_empty() {
                return self.doc_ids.iter().map(|p| p.as_path()).collect();
            }

            let mut alt_bitmap: Option<RoaringBitmap> = None;
            let mut missing = false;

            // Sort by posting list size for fast intersection
            let mut with_sizes: Vec<(&[u8; 3], u64)> = alt_trigrams
                .iter()
                .filter_map(|tri| {
                    self.lookup_trigram(tri).map(|bm| (tri, bm.len()))
                })
                .collect();

            if with_sizes.len() < alt_trigrams.len() {
                missing = true;
            }

            if !missing {
                with_sizes.sort_by_key(|&(_, len)| len);
                for (tri, _) in &with_sizes {
                    if let Some(bm) = self.lookup_trigram(tri) {
                        match &mut alt_bitmap {
                            None => alt_bitmap = Some(bm),
                            Some(acc) => {
                                *acc &= &bm;
                                if acc.is_empty() {
                                    break;
                                }
                            }
                        }
                    }
                }
            }

            if let Some(bm) = alt_bitmap {
                if i == 0 {
                    result_bitmap = bm;
                } else {
                    result_bitmap |= bm;
                }
            }
        }

        result_bitmap
            .iter()
            .filter_map(|id| self.doc_ids.get(id as usize).map(|p| p.as_path()))
            .collect()
    }

    fn lookup_trigram(&self, trigram: &[u8; 3]) -> Option<RoaringBitmap> {
        let hash = crc32fast::hash(trigram);
        // Binary search in lookup table
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
        RoaringBitmap::deserialize_from(data).ok()
    }

    pub fn is_stale(&self) -> bool {
        // Sample check — don't check all files, just a few
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
    let mut trigram_list: Vec<(&[u8; 3], &RoaringBitmap)> = index.ngrams.iter().collect();
    trigram_list.sort_by_key(|(k, _)| crc32fast::hash(*k));

    for (tri, bitmap) in &trigram_list {
        let mut buf = Vec::new();
        bitmap.serialize_into(&mut buf)?;
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
        version: 1,
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

fn chrono_now() -> String {
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}s_since_epoch", dur.as_secs())
}
