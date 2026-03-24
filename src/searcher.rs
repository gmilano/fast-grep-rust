use std::path::{Path, PathBuf};

use anyhow::Result;
use rayon::prelude::*;
use regex::Regex;

use crate::index::SparseIndex;

#[derive(Debug, Clone)]
pub struct Match {
    pub path: PathBuf,
    pub line_number: usize,
    pub line: String,
}

pub struct Searcher {
    index: SparseIndex,
}

impl Searcher {
    pub fn new(root: &Path, no_ignore: bool, type_filter: Option<&str>) -> Result<Self> {
        let index = SparseIndex::build_from_directory(root, no_ignore, type_filter, false)?;
        Ok(Searcher { index })
    }

    pub fn search(&self, pattern: &str) -> Result<Vec<Match>> {
        let candidates = self.index.search(pattern);
        let re = Regex::new(pattern)?;

        let matches: Vec<Match> = candidates
            .par_iter()
            .flat_map(|path| {
                let mut file_matches = Vec::new();
                if let Ok(content) = std::fs::read_to_string(path) {
                    for (i, line) in content.lines().enumerate() {
                        if re.is_match(line) {
                            file_matches.push(Match {
                                path: path.to_path_buf(),
                                line_number: i + 1,
                                line: line.to_string(),
                            });
                        }
                    }
                }
                file_matches
            })
            .collect();

        Ok(matches)
    }

    pub fn search_files_only(&self, pattern: &str) -> Result<Vec<PathBuf>> {
        let candidates = self.index.search(pattern);
        let re = Regex::new(pattern)?;

        let files: Vec<PathBuf> = candidates
            .par_iter()
            .filter(|path| {
                std::fs::read_to_string(path)
                    .map(|content| content.lines().any(|line| re.is_match(line)))
                    .unwrap_or(false)
            })
            .map(|p| p.to_path_buf())
            .collect();

        Ok(files)
    }

    pub fn search_count(&self, pattern: &str) -> Result<usize> {
        let matches = self.search(pattern)?;
        Ok(matches.len())
    }
}

/// Search using a persistent index with Rayon parallel verify.
pub fn search_persistent(
    index: &crate::persist::PersistentIndex,
    pattern: &str,
) -> Result<Vec<Match>> {
    let candidates = index.search(pattern);
    let re = Regex::new(pattern)?;

    let matches: Vec<Match> = candidates
        .par_iter()
        .flat_map(|path| {
            let mut file_matches = Vec::new();
            if let Ok(content) = std::fs::read_to_string(path) {
                for (i, line) in content.lines().enumerate() {
                    if re.is_match(line) {
                        file_matches.push(Match {
                            path: path.to_path_buf(),
                            line_number: i + 1,
                            line: line.to_string(),
                        });
                    }
                }
            }
            file_matches
        })
        .collect();

    Ok(matches)
}

/// Full scan without index (for benchmarking baseline).
pub fn search_full_scan(
    root: &Path,
    pattern: &str,
    no_ignore: bool,
    type_filter: Option<&str>,
) -> Result<Vec<Match>> {
    let re = Regex::new(pattern)?;
    let walker = ignore::WalkBuilder::new(root)
        .git_ignore(!no_ignore)
        .hidden(false)
        .build();

    let paths: Vec<PathBuf> = walker
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map_or(false, |ft| ft.is_file()))
        .filter(|e| {
            if let Some(ext_filter) = type_filter {
                e.path()
                    .extension()
                    .and_then(|e| e.to_str())
                    .map_or(false, |e| e == ext_filter)
            } else {
                true
            }
        })
        .map(|e| e.path().to_path_buf())
        .collect();

    let matches: Vec<Match> = paths
        .par_iter()
        .flat_map(|path| {
            let mut file_matches = Vec::new();
            if let Ok(content) = std::fs::read_to_string(path) {
                // Skip binary
                if content.as_bytes().iter().take(512).any(|&b| b == 0) {
                    return file_matches;
                }
                for (i, line) in content.lines().enumerate() {
                    if re.is_match(line) {
                        file_matches.push(Match {
                            path: path.clone(),
                            line_number: i + 1,
                            line: line.to_string(),
                        });
                    }
                }
            }
            file_matches
        })
        .collect();

    Ok(matches)
}
