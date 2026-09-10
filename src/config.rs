//! Per-index configuration (`<index>/config.toml`).
//!
//! The config lives in the index *root*, next to `current` — never inside a
//! slot — so it survives every `build` / `update` / `compact` (those only touch
//! slots + the pointer). It is written with sensible commented defaults the
//! first time an index is built and is never clobbered afterwards, so a user's
//! hand edits stick. Missing file or missing fields fall back to the defaults,
//! and a malformed file degrades to defaults with a warning rather than failing.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::filetype::{self, ExtClass};

/// File name (in the index root) holding the editable per-index config.
pub const CONFIG_FILE: &str = "config.toml";

/// Commented default written on first build. Kept in sync with
/// [`CompactionConfig`]'s defaults; editing this only changes the *template*,
/// not the effective defaults (those live in the `default_*` fns below).
pub const DEFAULT_CONFIG_TOML: &str = "\
# fast-grep per-index configuration. Edit and save — it is re-read on each run
# and never overwritten once created.

[compaction]
# Rebaseline the primary index by folding the delta + dropping tombstones once
# the working tree diverges enough from the frozen baseline. `fgr compact`
# always runs regardless of these; they only gate the automatic triggers
# (`fgr update` and the daemon).
auto = true              # enable automatic compaction
delta_docs_abs = 500     # compact once the live delta exceeds this many docs
delta_docs_ratio = 0.05  # ...or once it exceeds this fraction of the baseline
tombstone_ratio = 0.10   # ...or once tombstones exceed this fraction of it
min_main_docs = 500      # never auto-compact a baseline smaller than this

[index]
# What gets indexed. Binary files are detected by extension AND a confirmed
# magic signature, so a text file misnamed `.png` is still indexed.
max_file_size_mb = 64            # skip files larger than this (0 = no limit)
# Known text extensions (.rs, .log, .txt, .csv, ...) are always indexed even
# past the cap. Add more extensions and/or relative-path globs that should also
# bypass the cap and always be indexed:
always_index_extensions = []     # e.g. [\"ndjson\", \"dump\"]
always_index_paths = []          # e.g. [\"logs/**\", \"data/*.bin\"]
# Binary extensions with no magic (bin/dat/o/obj/lzma/eot/pyc/pyo/tar) are
# classified by content: a NUL or >this%% high bytes (in NUL-free, non-UTF-8
# data) means binary; UTF-8 text (incl. CJK) is always kept. Tune 30-40.
binary_high_byte_pct = 30
# Bound the peak RAM of a full `fgr index` build. Postings are accumulated in a
# buffer of about this many MiB, spilled to sorted temp segments when it fills,
# then k-way merged into the final index — so peak memory stays flat instead of
# growing with the repository. 0 disables spilling (fastest, but the whole index
# is held in RAM). The final index is byte-identical either way.
build_buffer_mb = 256
";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub compaction: CompactionConfig,
    #[serde(default)]
    pub index: IndexConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionConfig {
    /// Whether the automatic triggers (`fgr update`, daemon) may compact.
    #[serde(default = "default_auto")]
    pub auto: bool,
    /// Compact once the live delta has at least this many docs.
    #[serde(default = "default_delta_docs_abs")]
    pub delta_docs_abs: usize,
    /// ...or once the delta reaches this fraction of the baseline doc count.
    #[serde(default = "default_delta_docs_ratio")]
    pub delta_docs_ratio: f64,
    /// ...or once tombstones reach this fraction of the baseline doc count.
    #[serde(default = "default_tombstone_ratio")]
    pub tombstone_ratio: f64,
    /// Never auto-compact a baseline smaller than this (not worth the churn).
    #[serde(default = "default_min_main_docs")]
    pub min_main_docs: usize,
}

// Default thresholds, calibrated on the 79K-file Linux kernel corpus:
// - Every update re-reads + re-trigrams the whole carried delta (~1 ms/file),
//   so an uncompacted delta makes ALL subsequent updates slower — 500 caps
//   that carry cost at ~0.5 s.
// - While the delta sits below the selective-bitmap fast-path threshold
//   (max(500, 0.7% of docs)), every selective query full-scans every live
//   delta file (~0.14 ms/file/query measured) — 500 keeps that transient
//   penalty at ~70 ms and short-lived.
// - Compaction itself costs ~3 s on that corpus (parallel + overlapped), so
//   folding at 500 changed docs is cheap.
fn default_auto() -> bool {
    true
}
fn default_delta_docs_abs() -> usize {
    500
}
fn default_delta_docs_ratio() -> f64 {
    0.05
}
fn default_tombstone_ratio() -> f64 {
    0.10
}
fn default_min_main_docs() -> usize {
    500
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            auto: default_auto(),
            delta_docs_abs: default_delta_docs_abs(),
            delta_docs_ratio: default_delta_docs_ratio(),
            tombstone_ratio: default_tombstone_ratio(),
            min_main_docs: default_min_main_docs(),
        }
    }
}

fn default_build_buffer_mb() -> usize {
    256
}

impl IndexConfig {
    /// Build buffer budget in bytes; `None` means unbounded (never spill).
    pub fn build_budget_bytes(&self) -> Option<usize> {
        if self.build_buffer_mb == 0 {
            None
        } else {
            Some(self.build_buffer_mb * 1024 * 1024)
        }
    }
}

impl CompactionConfig {
    /// Whether divergence warrants an automatic rebaseline. `main_docs` is the
    /// baseline doc count (the denominator for the ratios), `delta_docs` the
    /// live delta size, `tombstones` the number of deleted baseline docs.
    /// The explicit `fgr compact` bypasses this entirely.
    pub fn should_compact(&self, main_docs: usize, delta_docs: usize, tombstones: usize) -> bool {
        if !self.auto || main_docs < self.min_main_docs {
            return false;
        }
        let denom = main_docs.max(1) as f64;
        delta_docs >= self.delta_docs_abs
            || (delta_docs as f64 / denom) >= self.delta_docs_ratio
            || (tombstones as f64 / denom) >= self.tombstone_ratio
    }
}

/// What gets indexed: the binary/size admission policy, editable per index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexConfig {
    /// Files larger than this are skipped, unless their extension is known-text,
    /// listed in `always_index_extensions`, or matched by `always_index_paths`.
    /// `0` disables the cap.
    #[serde(default = "default_max_file_size_mb")]
    pub max_file_size_mb: u64,
    /// Extra extensions (no dot, any case) treated as always-text: indexed past
    /// the size cap and exempt from the NUL heuristic.
    #[serde(default)]
    pub always_index_extensions: Vec<String>,
    /// Relative-path globs (from the index root) that bypass the size cap.
    #[serde(default)]
    pub always_index_paths: Vec<String>,
    /// For binary extensions with no magic signature (bin, dat, o, obj, lzma,
    /// eot, pyc, pyo, tar): a NUL-free, non-UTF-8 sample is treated as binary
    /// when more than this %% of its bytes are > 127. UTF-8 text (incl. CJK) is
    /// always kept. Tune 30–40; higher indexes more of these as text.
    #[serde(default = "default_binary_high_byte_pct")]
    pub binary_high_byte_pct: u8,
    /// Peak build buffer in MiB before postings are spilled to a sorted temp
    /// segment and later k-way merged. Keeps peak RAM flat regardless of corpus
    /// size. `0` disables spilling (the whole index is assembled in RAM).
    #[serde(default = "default_build_buffer_mb")]
    pub build_buffer_mb: usize,
}

fn default_max_file_size_mb() -> u64 {
    64
}
fn default_binary_high_byte_pct() -> u8 {
    filetype::DEFAULT_HIGH_BYTE_PCT
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            max_file_size_mb: default_max_file_size_mb(),
            always_index_extensions: Vec::new(),
            always_index_paths: Vec::new(),
            binary_high_byte_pct: default_binary_high_byte_pct(),
            build_buffer_mb: default_build_buffer_mb(),
        }
    }
}

/// Outcome of the indexing admission decision for one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Candidate {
    /// Index this file (caller still reads it and runs the NUL backstop).
    Admit,
    /// A confirmed / marker-less binary — skip without reading the body.
    SkipBinary,
    /// Over the size cap and not exempt — skip.
    SkipTooLarge,
}

/// Compiled admission policy shared by the index build, incremental update, and
/// the stale check, so all three agree on the file set (a disagreement makes an
/// index churn — a file one path includes and another drops is re-indexed then
/// evicted on every update). Cheap to clone across rayon workers.
#[derive(Debug, Clone)]
pub struct Admission {
    max_bytes: Option<u64>,
    extra_text: HashSet<String>,
    path_globs: Option<GlobSet>,
    binary_high_byte_pct: u8,
    root: PathBuf,
}

impl Admission {
    /// Build from an `IndexConfig`, resolving path globs against `root`.
    /// Invalid globs are warned about and skipped rather than failing the build.
    pub fn from_config(cfg: &IndexConfig, root: &Path) -> Self {
        let max_bytes = (cfg.max_file_size_mb > 0).then(|| cfg.max_file_size_mb * 1024 * 1024);
        let extra_text = cfg
            .always_index_extensions
            .iter()
            .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
            .collect();
        let path_globs = if cfg.always_index_paths.is_empty() {
            None
        } else {
            let mut b = GlobSetBuilder::new();
            for pat in &cfg.always_index_paths {
                match Glob::new(pat) {
                    Ok(g) => {
                        b.add(g);
                    }
                    Err(e) => {
                        eprintln!("warning: ignoring invalid always_index_paths glob `{pat}`: {e}")
                    }
                }
            }
            b.build().ok()
        };
        Self {
            max_bytes,
            extra_text,
            path_globs,
            binary_high_byte_pct: cfg.binary_high_byte_pct,
            root: root.to_path_buf(),
        }
    }

    /// A file exempt from the size cap: known-text extension, a configured extra
    /// text extension, or a configured relative-path glob.
    fn size_exempt(&self, path: &Path, ext: &str) -> bool {
        if filetype::is_known_text_ext_str(ext) || self.extra_text.contains(ext) {
            return true;
        }
        match &self.path_globs {
            Some(gs) => {
                let rel = path.strip_prefix(&self.root).unwrap_or(path);
                gs.is_match(rel)
            }
            None => false,
        }
    }

    /// Whether the NUL backstop should be skipped for this extension (it is
    /// trusted text). Path-glob exemptions keep the NUL check.
    pub fn is_text_ext(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        match ext.as_deref() {
            Some(e) => filetype::is_known_text_ext_str(e) || self.extra_text.contains(e),
            None => false,
        }
    }
}

/// Decide whether to index `path` under `adm`. `size_hint` is the on-disk size
/// when the caller already has it (e.g. the update walk, which stats for mtime
/// anyway); pass `None` and this stats lazily — but only when a size cap is in
/// effect and the file isn't cap-exempt, so the common case (text sources under
/// a cap) does no stat at all. Reads a bounded header only for signature-binary
/// extensions, returning `SkipBinary` without touching the body when confirmed.
pub fn admit_file(
    path: &Path,
    size_hint: Option<u64>,
    adm: &Admission,
) -> std::io::Result<Candidate> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    match filetype::classify_ext(&ext) {
        ExtClass::NoMarker => {
            // No magic to confirm; decide by content instead of skipping blind.
            let block = read_header(path, filetype::CONTENT_PEEK)?;
            if filetype::looks_binary_content(&block, adm.binary_high_byte_pct) {
                return Ok(Candidate::SkipBinary);
            }
            // Looks like text → fall through to the size check and index it.
        }
        ExtClass::Signature => {
            let header = read_header(path, filetype::HEADER_PEEK)?;
            if filetype::header_confirms_binary(&ext, &header) {
                return Ok(Candidate::SkipBinary);
            }
            // Magic absent → misnamed; fall through to the size check as text.
        }
        ExtClass::NotBinary => {}
    }

    if let Some(max) = adm.max_bytes {
        if !adm.size_exempt(path, &ext) {
            let size = match size_hint {
                Some(s) => s,
                None => std::fs::metadata(path)?.len(),
            };
            if size > max {
                return Ok(Candidate::SkipTooLarge);
            }
        }
    }
    Ok(Candidate::Admit)
}

/// Read up to `n` bytes from the start of `path`.
fn read_header(path: &Path, n: usize) -> std::io::Result<Vec<u8>> {
    let f = std::fs::File::open(path)?;
    let mut buf = Vec::with_capacity(n);
    f.take(n as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Load `<index_dir>/config.toml`, falling back to defaults when it is absent,
/// and to defaults-with-a-warning when it is present but malformed.
pub fn load(index_dir: &Path) -> Config {
    let path = index_dir.join(CONFIG_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Config::default(),
    };
    match toml::from_str::<Config>(&text) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!(
                "warning: ignoring malformed {} ({e}); using defaults",
                path.display()
            );
            Config::default()
        }
    }
}

/// Write the commented default config into the index root if none exists yet.
/// Never clobbers an existing file, so user edits are preserved across rebuilds.
pub fn write_default_if_absent(index_dir: &Path) {
    let path = index_dir.join(CONFIG_FILE);
    if !path.exists() {
        let _ = std::fs::write(&path, DEFAULT_CONFIG_TOML);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = CompactionConfig::default();
        assert!(c.auto);
        assert_eq!(c.delta_docs_abs, 500);
        assert_eq!(c.min_main_docs, 500);
    }

    #[test]
    fn should_compact_thresholds() {
        let c = CompactionConfig::default();
        // Below min_main_docs: never, no matter how diverged.
        assert!(!c.should_compact(100, 999_999, 999_999));
        // Absolute delta threshold (500) — big baseline so the ratio doesn't
        // fire first (500/1_000_000 = 0.05% << 5%).
        assert!(c.should_compact(1_000_000, 500, 0));
        assert!(!c.should_compact(1_000_000, 499, 0));
        // Delta ratio (5%) — small-enough baseline that it fires before abs.
        assert!(c.should_compact(4_000, 200, 0));
        assert!(!c.should_compact(4_000, 199, 0));
        // Tombstone ratio (10%).
        assert!(c.should_compact(10_000, 0, 1000));
        assert!(!c.should_compact(10_000, 0, 999));
        // auto = false disables the automatic decision entirely.
        let mut off = c.clone();
        off.auto = false;
        assert!(!off.should_compact(10_000, 999_999, 999_999));
    }

    #[test]
    fn partial_toml_fills_missing_with_defaults() {
        let cfg: Config = toml::from_str("[compaction]\nauto = false\n").unwrap();
        assert!(!cfg.compaction.auto);
        assert_eq!(cfg.compaction.delta_docs_abs, 500);
    }

    #[test]
    fn default_template_is_valid_and_matches_defaults() {
        let cfg: Config = toml::from_str(DEFAULT_CONFIG_TOML).expect("template parses");
        let d = CompactionConfig::default();
        assert_eq!(cfg.compaction.auto, d.auto);
        assert_eq!(cfg.compaction.delta_docs_abs, d.delta_docs_abs);
        assert_eq!(cfg.compaction.delta_docs_ratio, d.delta_docs_ratio);
        assert_eq!(cfg.compaction.tombstone_ratio, d.tombstone_ratio);
        assert_eq!(cfg.compaction.min_main_docs, d.min_main_docs);
        // The new [index] section parses to its defaults too.
        assert_eq!(cfg.index.max_file_size_mb, 64);
        assert!(cfg.index.always_index_extensions.is_empty());
        assert_eq!(
            cfg.index.build_buffer_mb,
            IndexConfig::default().build_buffer_mb
        );
    }

    // --- file admission ---

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, bytes).unwrap();
        p
    }

    const MB: u64 = 1024 * 1024;

    #[test]
    fn admit_confirmed_binary_skips_regardless_of_size() {
        let tmp = tempfile::tempdir().unwrap();
        let adm = Admission::from_config(&IndexConfig::default(), tmp.path());
        let png = write(tmp.path(), "logo.png", b"\x89PNG\r\n\x1a\n\x00\x00");
        // Confirmed binary short-circuits before the size check.
        assert_eq!(
            admit_file(&png, Some(0), &adm).unwrap(),
            Candidate::SkipBinary
        );
        assert_eq!(
            admit_file(&png, Some(500 * MB), &adm).unwrap(),
            Candidate::SkipBinary
        );
    }

    #[test]
    fn admit_misnamed_text_with_binary_ext_is_indexed() {
        let tmp = tempfile::tempdir().unwrap();
        let adm = Admission::from_config(&IndexConfig::default(), tmp.path());
        // A text file called `.png` fails the magic check → not skipped.
        let fake = write(tmp.path(), "notes.png", b"just some notes, not a png\n");
        assert_eq!(admit_file(&fake, Some(10), &adm).unwrap(), Candidate::Admit);
    }

    #[test]
    fn admit_no_marker_binary_decided_by_content() {
        let tmp = tempfile::tempdir().unwrap();
        let adm = Admission::from_config(&IndexConfig::default(), tmp.path());

        // A no-marker extension holding a NUL → binary.
        let nul = write(tmp.path(), "a.o", b"\x7fELF\x00\x00\x00stuff");
        assert_eq!(
            admit_file(&nul, Some(10), &adm).unwrap(),
            Candidate::SkipBinary
        );

        // A no-marker extension holding high-entropy, NUL-free, non-UTF-8 data
        // → binary.
        let hi: Vec<u8> = (0..4000u32)
            .map(|i| if i % 2 == 0 { 0xC0 } else { 0xFF })
            .collect();
        let blob = write(tmp.path(), "b.dat", &hi);
        assert_eq!(
            admit_file(&blob, Some(10), &adm).unwrap(),
            Candidate::SkipBinary
        );

        // A `.dat` that is actually plain ASCII text → now INDEXED (not naive).
        let txt = write(tmp.path(), "c.dat", b"key=value\nname=example\nport=8080\n");
        assert_eq!(admit_file(&txt, Some(10), &adm).unwrap(), Candidate::Admit);

        // A `.dat` of valid CJK UTF-8 → text (protected despite high bytes).
        let cjk = write(
            tmp.path(),
            "d.dat",
            "設定ファイル：バイナリではない".as_bytes(),
        );
        assert_eq!(admit_file(&cjk, Some(10), &adm).unwrap(), Candidate::Admit);
    }

    #[test]
    fn admit_size_cap_with_text_and_config_exemptions() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = IndexConfig {
            max_file_size_mb: 64,
            always_index_extensions: vec!["dump".into()],
            always_index_paths: vec!["big/**".into()],
            ..Default::default()
        };
        let adm = Admission::from_config(&cfg, tmp.path());

        // Oversized unknown extension → skipped (size is passed, not measured).
        let big = write(tmp.path(), "huge.xyz", b"x");
        assert_eq!(
            admit_file(&big, Some(100 * MB), &adm).unwrap(),
            Candidate::SkipTooLarge
        );
        // Oversized known-text extension → indexed.
        let log = write(tmp.path(), "huge.log", b"x");
        assert_eq!(
            admit_file(&log, Some(100 * MB), &adm).unwrap(),
            Candidate::Admit
        );
        // Oversized configured extension → indexed.
        let dump = write(tmp.path(), "huge.dump", b"x");
        assert_eq!(
            admit_file(&dump, Some(100 * MB), &adm).unwrap(),
            Candidate::Admit
        );
        // Oversized file under a configured path glob → indexed.
        let pathed = write(tmp.path(), "big/huge.xyz", b"x");
        assert_eq!(
            admit_file(&pathed, Some(100 * MB), &adm).unwrap(),
            Candidate::Admit
        );
        // Under the cap → admitted regardless.
        assert_eq!(admit_file(&big, Some(10), &adm).unwrap(), Candidate::Admit);
    }

    #[test]
    fn admit_zero_cap_disables_size_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = IndexConfig {
            max_file_size_mb: 0,
            ..Default::default()
        };
        let adm = Admission::from_config(&cfg, tmp.path());
        let big = write(tmp.path(), "huge.xyz", b"x");
        assert_eq!(
            admit_file(&big, Some(500 * MB), &adm).unwrap(),
            Candidate::Admit
        );
    }

    #[test]
    fn index_config_defaults_and_budget() {
        let d = IndexConfig::default();
        assert_eq!(d.build_buffer_mb, 256);
        assert_eq!(d.build_budget_bytes(), Some(256 * 1024 * 1024));
        let unbounded = IndexConfig {
            build_buffer_mb: 0,
            ..Default::default()
        };
        assert_eq!(unbounded.build_budget_bytes(), None);
    }
}
