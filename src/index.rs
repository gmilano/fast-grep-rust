use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use ignore::WalkBuilder;
use roaring::RoaringBitmap;

use crate::trigram;

pub struct IndexStats {
    pub num_docs: usize,
    pub num_ngrams: usize,
    pub estimated_ram_bytes: usize,
    pub avg_bitmap_cardinality: f64,
}

pub struct SparseIndex {
    /// Trigram → bitmap of doc IDs that contain this trigram
    pub ngrams: HashMap<[u8; 3], RoaringBitmap>,
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

        // Extract unique trigrams from the file content
        if content.len() < 3 {
            return;
        }
        let mut seen = HashSet::new();
        for w in content.windows(3) {
            let tri = [w[0], w[1], w[2]];
            if seen.insert(tri) {
                self.ngrams
                    .entry(tri)
                    .or_insert_with(RoaringBitmap::new)
                    .insert(doc_id);
            }
        }
    }

    pub fn search(&self, pattern: &str) -> Vec<&Path> {
        let alternatives = trigram::decompose_pattern(pattern);

        // If no useful trigrams, fall back to full scan
        if alternatives.is_empty() || alternatives.iter().all(|a| a.is_empty()) {
            return self.doc_ids.iter().map(|p| p.as_path()).collect();
        }

        let mut result_bitmap = RoaringBitmap::new();

        for (i, alt_trigrams) in alternatives.iter().enumerate() {
            if alt_trigrams.is_empty() {
                return self.doc_ids.iter().map(|p| p.as_path()).collect();
            }

            // Sort trigrams by posting list size (smallest first) for fast intersection
            let mut sorted: Vec<(&[u8; 3], u64)> = alt_trigrams
                .iter()
                .filter_map(|tri| self.ngrams.get(tri).map(|bm| (tri, bm.len())))
                .collect();
            sorted.sort_by_key(|&(_, len)| len);

            let mut alt_bitmap: Option<RoaringBitmap> = None;

            if sorted.len() < alt_trigrams.len() {
                // Some trigrams not in index → no matches for this alternative
                if i == 0 {
                    // Keep empty bitmap
                } else {
                    // OR with empty = no change
                }
                continue;
            }

            for (tri, _) in &sorted {
                if let Some(bm) = self.ngrams.get(*tri) {
                    match &mut alt_bitmap {
                        None => alt_bitmap = Some(bm.clone()),
                        Some(acc) => {
                            *acc &= bm;
                            if acc.is_empty() {
                                break;
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

    pub fn stats(&self) -> IndexStats {
        let num_docs = self.doc_ids.len();
        let num_ngrams = self.ngrams.len();
        let estimated_ram = self
            .ngrams
            .iter()
            .map(|(_, v)| 3 + v.serialized_size() + 48)
            .sum::<usize>();
        let avg_card = if num_ngrams > 0 {
            self.ngrams.values().map(|b| b.len() as f64).sum::<f64>() / num_ngrams as f64
        } else {
            0.0
        };
        IndexStats {
            num_docs,
            num_ngrams,
            estimated_ram_bytes: estimated_ram,
            avg_bitmap_cardinality: avg_card,
        }
    }

    pub fn build_from_directory(
        root: &Path,
        no_ignore: bool,
        type_filter: Option<&str>,
        verbose: bool,
    ) -> Result<Self> {
        let mut index = SparseIndex::new();
        let walker = WalkBuilder::new(root)
            .git_ignore(!no_ignore)
            .hidden(false)
            .build();

        let mut count = 0u32;
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

            // Skip binary files
            let content = match std::fs::read(path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            if content.iter().take(512).any(|&b| b == 0) {
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
    fn test_add_and_search() {
        let mut index = SparseIndex::new();
        index.add_document(
            Path::new("test1.rs"),
            b"fn main() { println!(\"hello world\"); }",
        );
        index.add_document(Path::new("test2.rs"), b"fn helper() { return 42; }");
        index.add_document(
            Path::new("test3.txt"),
            b"This is a completely different file about cats.",
        );

        let results = index.search("println");
        assert!(
            results.iter().any(|p| p == Path::new("test1.rs")),
            "Should find test1.rs for 'println'"
        );
    }

    #[test]
    fn test_stats() {
        let mut index = SparseIndex::new();
        index.add_document(Path::new("a.rs"), b"hello world foo bar baz");
        let stats = index.stats();
        assert_eq!(stats.num_docs, 1);
        assert!(stats.num_ngrams > 0);
    }
}
