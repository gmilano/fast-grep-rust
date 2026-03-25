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

/// A posting entry: (doc_id, loc_mask, next_mask).
/// - loc_mask: bitmask of (position % 8) where this trigram appears in the document
/// - next_mask: bitmask of ((position + 1) % 8) — expected position of the next adjacent trigram
pub type Posting = (u32, u8, u8);

pub struct SparseIndex {
    /// Trigram → list of (doc_id, loc_mask, next_mask)
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

        // Accumulate masks per trigram for this document
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
            self.ngrams
                .entry(tri)
                .or_insert_with(Vec::new)
                .push((doc_id, loc_mask, next_mask));
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
        let ordered_alternatives = trigram::decompose_pattern_ordered(pattern);

        // If no useful trigrams, fall back to full scan
        if alternatives.is_empty() || alternatives.iter().all(|a| a.is_empty()) {
            let all: Vec<&Path> = self.doc_ids.iter().map(|p| p.as_path()).collect();
            let len = all.len();
            return (all, len);
        }

        let mut result_docs: HashSet<u32> = HashSet::new();

        for (i, alt_trigrams) in alternatives.iter().enumerate() {
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

            // Step 1: bitmap intersection on doc_ids only
            let first_postings = match self.ngrams.get(sorted[0]) {
                Some(p) => p,
                None => continue,
            };
            let mut candidate_docs: HashSet<u32> =
                first_postings.iter().map(|&(doc_id, _, _)| doc_id).collect();

            for tri in &sorted[1..] {
                if candidate_docs.is_empty() {
                    break;
                }
                if let Some(postings) = self.ngrams.get(*tri) {
                    let doc_set: HashSet<u32> =
                        postings.iter().map(|&(doc_id, _, _)| doc_id).collect();
                    candidate_docs.retain(|d| doc_set.contains(d));
                } else {
                    candidate_docs.clear();
                    break;
                }
            }

            // Step 2: adjacency filtering using position masks (ordered trigrams)
            // Use the ordered (non-sorted) trigrams so consecutive pairs are truly adjacent.
            let ordered = &ordered_alternatives[i];
            if ordered.len() >= 2 && !candidate_docs.is_empty() {
                for pair in ordered.windows(2) {
                    let masks_a: HashMap<u32, (u8, u8)> = self
                        .ngrams
                        .get(&pair[0])
                        .map(|postings| {
                            postings
                                .iter()
                                .filter(|(doc_id, _, _)| candidate_docs.contains(doc_id))
                                .map(|&(doc_id, loc, next)| (doc_id, (loc, next)))
                                .collect()
                        })
                        .unwrap_or_default();

                    let masks_b: HashMap<u32, (u8, u8)> = self
                        .ngrams
                        .get(&pair[1])
                        .map(|postings| {
                            postings
                                .iter()
                                .filter(|(doc_id, _, _)| candidate_docs.contains(doc_id))
                                .map(|&(doc_id, loc, next)| (doc_id, (loc, next)))
                                .collect()
                        })
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

        let results: Vec<&Path> = result_docs
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
            .map(|(_, v)| 3 + v.len() * 6 + 48) // key + Vec<(u32,u8,u8)> + overhead
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
            eprintln!("  built bigram freq table from {} file sample", paths.len().min(3000));
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

    // --- TrigramIndex-equivalent tests (ported from index.test.ts) ---

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

        // Short pattern = no trigrams = fallback to all docs
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

        // "React" trigrams: Rea, eac, act — should match a.ts and c.ts
        let results = idx.search("React");
        assert!(results.contains(&&*Path::new("a.ts")));
        assert!(results.contains(&&*Path::new("c.ts")));
        assert!(!results.contains(&&*Path::new("b.ts")));
    }

    // --- adjacency filtering ---

    #[test]
    fn adjacency_filtering_finds_real_match() {
        let mut idx = SparseIndex::new();
        idx.add_document(
            Path::new("test1.rs"),
            b"fn main() { println!(\"hello\"); }",
        );
        idx.add_document(Path::new("test2.rs"), b"priority control ntly");

        let results = idx.search("println");
        assert!(results.contains(&&*Path::new("test1.rs")));
    }

    // --- search_with_stats ---

    #[test]
    fn search_with_stats_returns_valid_metrics() {
        let mut idx = SparseIndex::new();
        idx.add_document(Path::new("a.rs"), b"hello world foo bar baz");
        let stats = idx.search_with_stats("hello", 1);
        assert!(stats.candidates >= 1);
        assert_eq!(stats.verified, 1);
    }
}
