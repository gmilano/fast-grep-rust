//! Metal GPU acceleration for the verify phase (macOS only).
//!
//! The GPU pre-filter checks candidate lines for a fixed literal string.
//! Lines that pass go on to full regex verification on CPU.
//! Falls back to CPU-only if Metal device init fails.

use std::path::Path;

#[cfg(target_os = "macos")]
pub mod metal_impl;

#[cfg(target_os = "macos")]
pub use metal_impl::MetalVerifier;

/// Extract the longest fixed literal from a regex pattern.
/// Returns None for patterns like `static.*inline` with no fixed literal.
pub fn extract_literal(pattern: &str) -> Option<String> {
    // Bail on common regex metacharacters that prevent literal extraction
    let has_meta = pattern.chars().any(|c| matches!(c, '*' | '+' | '?' | '[' | '(' | ')' | '|' | '^' | '$' | '{'));
    if has_meta {
        return None;
    }
    // Unescape simple escaped chars
    let literal = pattern.replace("\\.", ".").replace("\\\\", "\\");
    if literal.len() >= 3 {
        Some(literal)
    } else {
        None
    }
}

/// Result of verifying a single candidate line.
pub struct VerifyResult {
    pub doc_id: u32,
    pub line_no: u32,
    pub byte_offset: u32,
    pub path: std::path::PathBuf,
    pub line: String,
}


