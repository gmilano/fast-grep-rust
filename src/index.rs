use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use ignore::WalkBuilder;

use crate::searcher::is_known_text_ext;

pub struct IndexStats {
    pub num_docs: usize,
    pub num_ngrams: usize,
    pub estimated_ram_bytes: usize,
    pub avg_postings_len: f64,
}

/// A posting entry: (doc_id, line_no, byte_offset, line_prefix).
/// - line_no: 1-based line number where this trigram appears
/// - byte_offset: byte offset of the start of that line in the file
/// - line_prefix: first 4 bytes of the line (zero-padded if shorter)
pub type Posting = (u32, u32, u32, [u8; 4]);

pub struct SparseIndex {
    /// Trigram → list of (doc_id, line_no, byte_offset)
    pub ngrams: HashMap<[u8; 3], Vec<Posting>>,
    pub doc_ids: Vec<PathBuf>,
}

impl SparseIndex {
    pub fn new() -> Self {
        SparseIndex {
            ngrams: HashMap::new(),
            doc_ids: Vec::new(),
        }
    }

    pub fn add_document(&mut self, path: &Path, content: &[u8]) {
        let doc_id = self.doc_ids.len() as u32;
        self.doc_ids.push(path.to_path_buf());

        if content.len() < 3 {
            return;
        }

        // Index trigrams per line: one posting per (trigram, doc_id, line)
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
                let byte_offset = line_start as u32;
                let mut prefix = [0u8; 4];
                let copy_len = line.len().min(4);
                prefix[..copy_len].copy_from_slice(&line[..copy_len]);
                for w in line.windows(3) {
                    let tri = [w[0], w[1], w[2]];
                    let entry = self.ngrams.entry(tri).or_default();
                    // Dedup: only one posting per (doc_id, line_no) per trigram
                    if entry
                        .last()
                        .map_or(true, |&(d, l, _, _)| d != doc_id || l != line_no)
                    {
                        entry.push((doc_id, line_no, byte_offset, prefix));
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

    pub fn stats(&self) -> IndexStats {
        let num_docs = self.doc_ids.len();
        let num_ngrams = self.ngrams.len();
        let estimated_ram: usize = self
            .ngrams
            .iter()
            .map(|(_, v)| 3 + v.len() * 16 + 48) // key + Vec<(u32,u32,u32,[u8;4])> + overhead
            .sum();
        let avg_len = if num_ngrams > 0 {
            self.ngrams.values().map(|v| v.len() as f64).sum::<f64>() / num_ngrams as f64
        } else {
            0.0
        };
        IndexStats {
            num_docs,
            num_ngrams,
            estimated_ram_bytes: estimated_ram,
            avg_postings_len: avg_len,
        }
    }

    pub fn build_from_directory(
        root: &Path,
        no_ignore: bool,
        type_filter: Option<&str>,
        verbose: bool,
    ) -> Result<Self> {
        // Phase 1: collect all file paths
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

            if let Some(ext_filter) = type_filter {
                match path.extension().and_then(|e| e.to_str()) {
                    Some(ext) if ext == ext_filter => {}
                    _ => continue,
                }
            }

            paths.push(path.to_path_buf());
        }

        // Phase 2: index all files
        let mut index = SparseIndex::new();
        let mut count = 0u32;
        for path in &paths {
            let content = match std::fs::read(path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            // Match `search_full_scan`'s binary-detection rule so the
            // indexed and direct-scan paths see the same set of files.
            // Known text extensions (`.txt`, `.rs`, etc.) trust the
            // extension and bypass the null-byte heuristic — fixtures
            // can legitimately contain `\0` and we don't want to drop
            // them from the index when the direct scan would still
            // search them.
            if !is_known_text_ext(path) && content.iter().take(512).any(|&b| b == 0) {
                continue;
            }

            index.add_document(path, &content);
            count += 1;
            if verbose && count % 10000 == 0 {
                eprintln!("  indexed {} files...", count);
            }
        }

        if verbose {
            eprintln!(
                "  indexed {} files total, {} trigrams",
                count,
                index.ngrams.len()
            );
        }

        Ok(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn reports_correct_stats() {
        let mut idx = SparseIndex::new();
        idx.add_document(Path::new("a.ts"), b"hello world");
        idx.add_document(Path::new("b.ts"), b"hello again");

        let stats = idx.stats();
        assert_eq!(stats.num_docs, 2);
        assert!(stats.num_ngrams > 0);
        assert!(stats.avg_postings_len > 0.0);
    }
}
