//! Per-index configuration (`<index>/config.toml`).
//!
//! The config lives in the index *root*, next to `current` — never inside a
//! slot — so it survives every `build` / `update` / `compact` (those only touch
//! slots + the pointer). It is written with sensible commented defaults the
//! first time an index is built and is never clobbered afterwards, so a user's
//! hand edits stick. Missing file or missing fields fall back to the defaults,
//! and a malformed file degrades to defaults with a warning rather than failing.

use std::path::Path;

use serde::{Deserialize, Serialize};

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
";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub compaction: CompactionConfig,
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
    }
}
