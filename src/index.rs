use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;
use ignore::WalkBuilder;

use crate::trigram;

pub struct IndexStats {
    pub num_docs: usize,
    pub num_ngrams: usize,
    pub estimated_ram_bytes: usize,
    pub avg_postings_len: f64,
}

pub struct SearchStats {
    pub candidates: usize,
    pub verified: usize,
    pub false_positive_rate: f64,
}

/// A posting entry: (doc_id, line_no, byte_offset).
/// - line_no: 1-based line number where this trigram appears
/// - byte_offset: byte offset of the start of that line in the file
pub type Posting = (u32, u32, u32);

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
                for w in line.windows(3) {
                    let tri = [w[0], w[1], w[2]];
                    let entry = self.ngrams.entry(tri).or_default();
                    // Dedup: only one posting per (doc_id, line_no) per trigram
                    if entry
                        .last()
                        .map_or(true, |&(d, l, _)| d != doc_id || l != line_no)
                    {
                        entry.push((doc_id, line_no, byte_offset));
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

    pub fn search(&self, pattern: &str) -> Vec<&Path> {
        let (candidates, _) = self.search_inner(pattern);
        candidates
    }

    pub fn search_with_stats(&self, pattern: &str, verified_count: usize) -> SearchStats {
        let (candidates, _) = self.search_inner(pattern);
        let num_candidates = candidates.len();
        let false_positives = if num_candidates > 0 {
            num_candidates.saturating_sub(verified_count)
        } else {
            0
        };
        let fp_rate = if num_candidates > 0 {
            false_positives as f64 / num_candidates as f64
        } else {
            0.0
        };
        SearchStats {
            candidates: num_candidates,
            verified: verified_count,
            false_positive_rate: fp_rate,
        }
    }

    fn search_inner(&self, pattern: &str) -> (Vec<&Path>, usize) {
        let alternatives = trigram::decompose_pattern(pattern);

        // If no useful trigrams, fall back to full scan
        if alternatives.is_empty() || alternatives.iter().all(|a| a.is_empty()) {
            let all: Vec<&Path> = self.doc_ids.iter().map(|p| p.as_path()).collect();
            let len = all.len();
            return (all, len);
        }

        let mut result_lines: HashSet<(u32, u32)> = HashSet::new();

        for alt_trigrams in &alternatives {
            if alt_trigrams.is_empty() {
                let all: Vec<&Path> = self.doc_ids.iter().map(|p| p.as_path()).collect();
                let len = all.len();
                return (all, len);
            }

            // Check if all trigrams exist in index
            let all_present = alt_trigrams.iter().all(|tri| self.ngrams.contains_key(tri));
            if !all_present {
                continue;
            }

            // Sort trigrams by posting list size (smallest first) for fast intersection
            let mut sorted: Vec<&[u8; 3]> = alt_trigrams.iter().collect();
            sorted.sort_by_key(|tri| self.ngrams.get(*tri).map_or(0, |v| v.len()));

            // Intersect on (doc_id, line_no) — trigrams must appear on the same line
            let first_postings = match self.ngrams.get(sorted[0]) {
                Some(p) => p,
                None => continue,
            };
            let mut candidates: HashSet<(u32, u32)> = first_postings
                .iter()
                .map(|&(doc_id, line_no, _)| (doc_id, line_no))
                .collect();

            for tri in &sorted[1..] {
                if candidates.is_empty() {
                    break;
                }
                if let Some(postings) = self.ngrams.get(*tri) {
                    let line_set: HashSet<(u32, u32)> = postings
                        .iter()
                        .map(|&(doc_id, line_no, _)| (doc_id, line_no))
                        .collect();
                    candidates.retain(|k| line_set.contains(k));
                } else {
                    candidates.clear();
                    break;
                }
            }

            result_lines.extend(candidates);
        }

        // Extract unique doc_ids and map to paths
        let mut unique_docs: HashSet<u32> = HashSet::new();
        for &(doc_id, _) in &result_lines {
            unique_docs.insert(doc_id);
        }

        let results: Vec<&Path> = unique_docs
            .iter()
            .filter_map(|&id| self.doc_ids.get(id as usize).map(|p| p.as_path()))
            .collect();
        let len = results.len();
        (results, len)
    }

    pub fn stats(&self) -> IndexStats {
        let num_docs = self.doc_ids.len();
        let num_ngrams = self.ngrams.len();
        let estimated_ram: usize = self
            .ngrams
            .iter()
            .map(|(_, v)| 3 + v.len() * 12 + 48) // key + Vec<(u32,u32,u32)> + overhead
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

        // Phase 2: build corpus-adaptive bigram frequency table
        let _freq = crate::sparse::BigramFreq::from_corpus(&paths, 3000);
        if verbose {
            eprintln!(
                "  built bigram freq table from {} file sample",
                paths.len().min(3000)
            );
        }

        // Phase 3: index all files
        let mut index = SparseIndex::new();
        let mut count = 0u32;
        for path in &paths {
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
    fn finds_documents_containing_a_literal_pattern() {
        let mut idx = SparseIndex::new();
        idx.add_document(Path::new("a.ts"), b"const hello = 'world';");
        idx.add_document(Path::new("b.ts"), b"function goodbye() {}");
        idx.add_document(Path::new("c.ts"), b"say hello to everyone");

        let results = idx.search("hello");
        assert!(results.contains(&&*Path::new("a.ts")));
        assert!(results.contains(&&*Path::new("c.ts")));
        assert!(!results.contains(&&*Path::new("b.ts")));
    }

    #[test]
    fn returns_empty_when_pattern_not_found() {
        let mut idx = SparseIndex::new();
        idx.add_document(Path::new("a.ts"), b"const x = 1;");

        let results = idx.search("zzzzz");
        assert!(results.is_empty());
    }

    #[test]
    fn returns_all_docs_for_short_patterns() {
        let mut idx = SparseIndex::new();
        idx.add_document(Path::new("a.ts"), b"abc");
        idx.add_document(Path::new("b.ts"), b"def");

        let results = idx.search("xy");
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn handles_alternation_patterns() {
        let mut idx = SparseIndex::new();
        idx.add_document(Path::new("a.ts"), b"function hello() {}");
        idx.add_document(Path::new("b.ts"), b"const world = 1;");
        idx.add_document(Path::new("c.ts"), b"nothing here");

        let results = idx.search("hello|world");
        assert!(results.contains(&&*Path::new("a.ts")));
        assert!(results.contains(&&*Path::new("b.ts")));
        assert!(!results.contains(&&*Path::new("c.ts")));
    }

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

    #[test]
    fn intersects_trigrams_correctly_and_logic() {
        let mut idx = SparseIndex::new();
        idx.add_document(Path::new("a.ts"), b"import React from 'react'");
        idx.add_document(Path::new("b.ts"), b"import Vue from 'vue'");
        idx.add_document(Path::new("c.ts"), b"React component here");

        let results = idx.search("React");
        assert!(results.contains(&&*Path::new("a.ts")));
        assert!(results.contains(&&*Path::new("c.ts")));
        assert!(!results.contains(&&*Path::new("b.ts")));
    }

    #[test]
    fn line_level_intersection_finds_real_match() {
        let mut idx = SparseIndex::new();
        idx.add_document(
            Path::new("test1.rs"),
            b"fn main() { println!(\"hello\"); }",
        );
        idx.add_document(Path::new("test2.rs"), b"priority control ntly");

        let results = idx.search("println");
        assert!(results.contains(&&*Path::new("test1.rs")));
    }

    #[test]
    fn search_with_stats_returns_valid_metrics() {
        let mut idx = SparseIndex::new();
        idx.add_document(Path::new("a.rs"), b"hello world foo bar baz");
        let stats = idx.search_with_stats("hello", 1);
        assert!(stats.candidates >= 1);
        assert_eq!(stats.verified, 1);
    }
}
