use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use ignore::WalkBuilder;

use crate::casefold;
use crate::config::{self, Admission, Candidate};
use crate::postenc::PostingWriter;
use crate::searcher::is_known_text_ext;

pub struct IndexStats {
    pub num_docs: usize,
    pub num_ngrams: usize,
    pub estimated_ram_bytes: usize,
    pub avg_postings_len: f64,
}

/// A posting entry: (doc_id, line_no, byte_offset).
/// - line_no: 1-based line number where this trigram appears
/// - byte_offset: byte offset of the start of that line in the file
pub type Posting = (u32, u32, u32);

/// Accumulates one trigram's posting list already in the compact
/// (delta-varint) wire format. Postings are encoded into `bytes` as they are
/// added, so the build never materializes the decoded `Vec<Posting>` for the
/// whole corpus — that is what keeps the peak RAM near the on-disk size.
#[derive(Default)]
pub struct TrigramBuilder {
    /// Compact-encoded postings, ready to be written to `ngrams.postings`
    /// verbatim at serialize time.
    pub bytes: Vec<u8>,
    /// Delta state (prev doc/line/offset) for `bytes`.
    writer: PostingWriter,
    /// Number of postings encoded, for `stats()` / `avg_postings_len`.
    count: u32,
}

pub struct SparseIndex {
    /// Trigram → compact-encoded posting list of (doc_id, line_no, byte_offset)
    pub ngrams: HashMap<[u8; 3], TrigramBuilder>,
    /// Case-folded trigrams over the *same* documents/lines, built in the same
    /// filesystem pass when the index is case-insensitive. `None` for a plain
    /// case-sensitive index. Postings carry the original-file byte offsets, so
    /// verification still reads the un-folded line.
    pub ngrams_ci: Option<HashMap<[u8; 3], TrigramBuilder>>,
    pub doc_ids: Vec<PathBuf>,
}

impl SparseIndex {
    /// Create an index; when `case_insensitive` is set it also accumulates the
    /// case-folded (CI) trigram map alongside the case-sensitive one.
    pub fn with_case_insensitive(case_insensitive: bool) -> Self {
        SparseIndex {
            ngrams: HashMap::new(),
            ngrams_ci: if case_insensitive {
                Some(HashMap::new())
            } else {
                None
            },
            doc_ids: Vec::new(),
        }
    }

    pub fn add_document(&mut self, path: &Path, content: &[u8]) {
        let doc_id = self.doc_ids.len() as u32;
        self.doc_ids.push(path.to_path_buf());
        let mut fold_buf = Vec::new();
        extract_document(
            doc_id,
            content,
            &mut self.ngrams,
            self.ngrams_ci.as_mut(),
            &mut fold_buf,
        );
    }

    pub fn stats(&self) -> IndexStats {
        let num_docs = self.doc_ids.len();
        let num_ngrams = self.ngrams.len();
        let mut estimated_ram: usize = self
            .ngrams
            .values()
            .map(|b| 3 + b.bytes.len() + 48) // key + packed postings + overhead
            .sum();
        if let Some(ci) = &self.ngrams_ci {
            estimated_ram += ci.values().map(|b| 3 + b.bytes.len() + 48).sum::<usize>();
        }
        let avg_len = if num_ngrams > 0 {
            self.ngrams.values().map(|b| b.count as f64).sum::<f64>() / num_ngrams as f64
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
        type_filter: &[String],
        verbose: bool,
        case_insensitive: bool,
        admission: &Admission,
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

            if !crate::searcher::passes_type_filter(path, type_filter) {
                continue;
            }

            paths.push(path.to_path_buf());
        }

        // Phase 2: index all files. The admission policy (binary-extension +
        // magic signature, and the size cap with text/config exemptions) skips
        // known binaries WITHOUT reading their bodies; the NUL heuristic stays
        // as the content backstop for everything that gets read. The same
        // `config::admit_file` gates the incremental update + stale walk, so all
        // three agree on the file set.
        let mut index = SparseIndex::with_case_insensitive(case_insensitive);
        let mut count = 0u32;
        let (mut skipped_binary, mut skipped_large) = (0u32, 0u32);
        for path in &paths {
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
            // check (unknown extension with NUL bytes). Known/configured text
            // extensions are trusted and bypass it — fixtures can legitimately
            // contain `\0` and the direct scan would still search them.
            if !admission.is_text_ext(path)
                && !is_known_text_ext(path)
                && content.iter().take(512).any(|&b| b == 0)
            {
                skipped_binary += 1;
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
                "  indexed {} files total, {} trigrams ({} binary, {} too-large skipped)",
                count,
                index.ngrams.len(),
                skipped_binary,
                skipped_large
            );
        }

        Ok(index)
    }
}

/// Extract per-line trigrams from one document into the case-sensitive map (and
/// the case-folded companion when present), delta-encoding each posting on the
/// spot. Shared by [`SparseIndex::add_document`] and the bounded (external-merge)
/// build so both produce byte-identical posting lists. Returns the number of
/// bytes appended to the posting blobs — the caller uses this to bound the build
/// buffer. The dedup (one posting per `(doc,line)` per trigram) and the
/// `byte_offset`-points-at-original-line rule match the original inline code.
pub fn extract_document(
    doc_id: u32,
    content: &[u8],
    ngrams: &mut HashMap<[u8; 3], TrigramBuilder>,
    mut ngrams_ci: Option<&mut HashMap<[u8; 3], TrigramBuilder>>,
    fold_buf: &mut Vec<u8>,
) -> usize {
    if content.len() < 3 {
        return 0;
    }

    let mut added = 0usize;
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
                let b = ngrams.entry(tri).or_default();
                // Dedup: only one posting per (doc_id, line_no) per trigram.
                if b.writer.last_dl() != Some((doc_id, line_no)) {
                    let before = b.bytes.len();
                    b.writer.push(&mut b.bytes, doc_id, line_no, byte_offset);
                    added += b.bytes.len() - before;
                    b.count += 1;
                }
            }

            // Case-insensitive map: same posting, but trigrams come from the
            // case-folded line. `byte_offset` still points at the original line
            // so verification reads un-folded text.
            if let Some(ci) = ngrams_ci.as_deref_mut() {
                casefold::fold_into(line, fold_buf);
                if fold_buf.len() >= 3 {
                    for w in fold_buf.windows(3) {
                        let tri = [w[0], w[1], w[2]];
                        let b = ci.entry(tri).or_default();
                        if b.writer.last_dl() != Some((doc_id, line_no)) {
                            let before = b.bytes.len();
                            b.writer.push(&mut b.bytes, doc_id, line_no, byte_offset);
                            added += b.bytes.len() - before;
                            b.count += 1;
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

    added
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn reports_correct_stats() {
        let mut idx = SparseIndex::with_case_insensitive(false);
        idx.add_document(Path::new("a.ts"), b"hello world");
        idx.add_document(Path::new("b.ts"), b"hello again");

        let stats = idx.stats();
        assert_eq!(stats.num_docs, 2);
        assert!(stats.num_ngrams > 0);
        assert!(stats.avg_postings_len > 0.0);
    }

    #[test]
    fn case_insensitive_builds_folded_map() {
        let mut idx = SparseIndex::with_case_insensitive(true);
        idx.add_document(Path::new("a.ts"), b"Hello WORLD");
        let ci = idx.ngrams_ci.as_ref().expect("ci map present");
        // The folded line "hello world" must yield the lowercase trigram "hel",
        // and the original-case "Hel" must NOT appear in the CI map.
        assert!(ci.contains_key(b"hel"));
        assert!(!ci.contains_key(b"Hel"));
        // The case-sensitive map keeps the original case.
        assert!(idx.ngrams.contains_key(b"Hel"));
    }
}
