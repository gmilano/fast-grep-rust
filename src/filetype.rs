//! File classification: known-text extensions, the NUL binary heuristic, and a
//! non-naive binary-extension detector that confirms a magic signature before
//! trusting the extension.
//!
//! This module is dependency-free (no `config`/`searcher`) so both the indexing
//! side (`config::admit_file`, `index`, `persist`) and the search side
//! (`searcher`, `render`) can share one source of truth for what counts as a
//! searchable text file — the review flagged that build, update, and search
//! disagreeing about the file set makes an index churn.

use std::path::Path;

/// Bytes peeked from the front of a file to (a) confirm a binary magic and
/// (b) run the NUL heuristic. 512 covers every signature below (max offset 8).
pub const HEADER_PEEK: usize = 512;

/// Whether the first bytes look binary — a NUL in the first 512 bytes. Kept as
/// the content backstop after the extension checks.
#[inline]
pub fn is_binary(buf: &[u8]) -> bool {
    let check_len = buf.len().min(512);
    memchr::memchr(0, &buf[..check_len]).is_some()
}

/// Bytes sampled for the content heuristic on no-marker binary extensions —
/// larger than `HEADER_PEEK` so the high-byte ratio is stable.
pub const CONTENT_PEEK: usize = 8192;

/// Default high-byte (>127) percentage above which a NUL-free, non-UTF-8 block
/// counts as binary. 30%: ASCII/code ≈ 0%, Latin-with-accents a few %,
/// random/compressed ≈ 50% — 30% cleanly separates text from binary while
/// leaning "binary" for these already-binary-typed extensions. Configurable.
pub const DEFAULT_HIGH_BYTE_PCT: u8 = 30;

/// Content heuristic for binary extensions we can't confirm by magic (the
/// no-marker set). A block is binary if it holds a NUL, or — when it is not
/// even leniently valid UTF-8 — if more than `high_byte_pct`% of its bytes
/// exceed 127. Valid UTF-8 (tolerating a multibyte sequence truncated at the
/// sample boundary) is always treated as text, so CJK / accented text is never
/// misflagged as binary.
pub fn looks_binary_content(block: &[u8], high_byte_pct: u8) -> bool {
    if block.is_empty() {
        return false;
    }
    if memchr::memchr(0, block).is_some() {
        return true;
    }
    if is_text_utf8(block) {
        return false;
    }
    let high = block.iter().filter(|&&b| b > 127).count();
    high * 100 > block.len() * high_byte_pct as usize
}

/// Valid UTF-8, tolerating a final multibyte sequence cut off by sampling.
fn is_text_utf8(block: &[u8]) -> bool {
    match std::str::from_utf8(block) {
        Ok(_) => true,
        // error_len() == None means the input merely ended mid-sequence (a
        // truncated tail from sampling), not a genuinely invalid byte.
        Err(e) => e.error_len().is_none(),
    }
}

/// Known text extensions — trusted as text, so they bypass the NUL heuristic
/// *and* the index size cap (someone may legitimately search a huge `.log`).
#[inline]
pub fn is_known_text_ext(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| is_known_text_ext_str(&e))
}

/// Extension form of [`is_known_text_ext`]; `ext` must already be lowercased.
pub fn is_known_text_ext_str(ext: &str) -> bool {
    matches!(
        ext,
        "rs" | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "py"
            | "go"
            | "rb"
            | "java"
            | "c"
            | "h"
            | "cpp"
            | "cc"
            | "hpp"
            | "cs"
            | "swift"
            | "kt"
            | "scala"
            | "php"
            | "html"
            | "css"
            | "scss"
            | "less"
            | "json"
            | "toml"
            | "yaml"
            | "yml"
            | "md"
            | "markdown"
            | "txt"
            | "text"
            | "log"
            | "csv"
            | "tsv"
            | "cfg"
            | "conf"
            | "ini"
            | "rst"
            | "tex"
            | "csproj"
            | "props"
            | "targets"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "vim"
            | "lua"
            | "r"
            | "sql"
            | "xml"
            | "svg"
            | "tf"
            | "hcl"
            | "nix"
            | "ex"
            | "exs"
            | "erl"
            | "hrl"
            | "ml"
            | "mli"
            | "hs"
            | "clj"
            | "cljs"
            | "lisp"
            | "el"
            | "dart"
            | "zig"
            | "v"
            | "proto"
            | "graphql"
            | "gql"
    )
}

/// How an extension is treated by the binary detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtClass {
    /// Known-binary extension with a start-of-file magic: confirm via the header
    /// before skipping, so a misnamed text file (`notes.png`) still gets indexed.
    Signature,
    /// Known-binary extension with no reliable start marker: skip by extension.
    /// Enumerated in [`no_marker`]; deliberately called out for later refinement.
    NoMarker,
    /// Not a known binary extension.
    NotBinary,
}

/// Classify `ext` (already lowercased) for the binary detector.
pub fn classify_ext(ext: &str) -> ExtClass {
    if !signatures(ext).is_empty() {
        ExtClass::Signature
    } else if no_marker(ext) {
        ExtClass::NoMarker
    } else {
        ExtClass::NotBinary
    }
}

/// Whether `header` (the file's first bytes) matches any known magic for `ext`.
/// Only meaningful for `ExtClass::Signature` extensions.
pub fn header_confirms_binary(ext: &str, header: &[u8]) -> bool {
    signatures(ext)
        .iter()
        .any(|&(off, sig)| header.len() >= off + sig.len() && &header[off..off + sig.len()] == sig)
}

/// Binary extensions with **no reliable start-of-file marker**. Skipped by
/// extension alone. Refinement candidates: `tar` (`ustar` lives at offset 257),
/// `pyc`/`pyo` (4-byte magic changes every Python version), `o`/`obj` (varied
/// object formats), and the genuinely marker-less `bin`/`dat`/`lzma`/`eot`.
fn no_marker(ext: &str) -> bool {
    matches!(
        ext,
        "bin" | "dat" | "o" | "obj" | "lzma" | "eot" | "pyc" | "pyo" | "tar"
    )
}

/// Known magic signatures per binary extension, as `(offset, bytes)`. A file is
/// confirmed binary if any entry matches. Empty slice = not a signature ext.
/// Starts from tgrep's binary-extension denylist (minus the no-marker set) and
/// extends it with common formats tgrep lacks (modern media, ZIP-based
/// packages, ML/data, native modules).
fn signatures(ext: &str) -> &'static [(usize, &'static [u8])] {
    match ext {
        // --- images ---
        "png" => &[(0, b"\x89PNG\r\n\x1a\n")],
        "jpg" | "jpeg" => &[(0, b"\xFF\xD8\xFF")],
        "gif" => &[(0, b"GIF87a"), (0, b"GIF89a")],
        "bmp" => &[(0, b"BM")],
        "ico" => &[(0, b"\x00\x00\x01\x00")],
        "tiff" | "tif" => &[(0, b"II*\x00"), (0, b"MM\x00*")],
        "psd" => &[(0, b"8BPS")],
        "jxl" => &[(0, b"\xFF\x0A"), (0, b"\x00\x00\x00\x0CJXL ")],
        "icns" => &[(0, b"icns")],
        "dds" => &[(0, b"DDS ")],
        "xcf" => &[(0, b"gimp xcf ")],
        // --- audio / video (RIFF, EBML, ISO-BMFF ftyp, ASF, …) ---
        "webp" | "avi" | "wav" => &[(0, b"RIFF")],
        "aiff" | "aif" => &[(0, b"FORM")],
        "mp3" => &[
            (0, b"ID3"),
            (0, b"\xFF\xFB"),
            (0, b"\xFF\xF3"),
            (0, b"\xFF\xF2"),
        ],
        "mp4" | "mov" | "m4a" | "m4v" | "heic" | "heif" | "avif" | "3gp" => &[(4, b"ftyp")],
        "mkv" | "webm" => &[(0, b"\x1A\x45\xDF\xA3")],
        "flac" => &[(0, b"fLaC")],
        "ogg" => &[(0, b"OggS")],
        "wma" | "wmv" => &[(0, b"\x30\x26\xB2\x75")],
        "aac" => &[(0, b"\xFF\xF1"), (0, b"\xFF\xF9"), (0, b"ADIF")],
        "flv" => &[(0, b"FLV")],
        "mid" | "midi" => &[(0, b"MThd")],
        "swf" => &[(0, b"FWS"), (0, b"CWS"), (0, b"ZWS")],
        // --- archives / compression ---
        "zip" | "jar" | "docx" | "xlsx" | "pptx" | "apk" | "ipa" | "aar" | "war" | "ear"
        | "nupkg" | "whl" | "egg" | "vsix" | "xpi" | "odt" | "ods" | "odp" | "epub" | "docm"
        | "xlsm" | "pptm" | "npz" => &[(0, b"PK\x03\x04"), (0, b"PK\x05\x06"), (0, b"PK\x07\x08")],
        "crx" => &[(0, b"Cr24")],
        "7z" => &[(0, b"7z\xBC\xAF\x27\x1C")],
        "rar" => &[(0, b"Rar!\x1A\x07")],
        "gz" | "tgz" => &[(0, b"\x1F\x8B")],
        "bz2" | "tbz2" => &[(0, b"BZh")],
        "xz" | "txz" => &[(0, b"\xFD7zXZ\x00")],
        "zst" => &[(0, b"\x28\xB5\x2F\xFD")],
        "lz4" => &[(0, b"\x04\x22\x4D\x18")],
        "lz" => &[(0, b"LZIP")],
        "z" => &[(0, b"\x1F\x9D")],
        "cab" => &[(0, b"MSCF")],
        "rpm" => &[(0, b"\xED\xAB\xEE\xDB")],
        // --- executables / objects / native modules ---
        "exe" | "dll" | "pyd" | "sys" | "ocx" | "scr" | "cpl" | "efi" => &[(0, b"MZ")],
        "so" | "ko" => &[(0, b"\x7FELF")],
        "dylib" => &[
            (0, b"\xFE\xED\xFA\xCE"),
            (0, b"\xFE\xED\xFA\xCF"),
            (0, b"\xCE\xFA\xED\xFE"),
            (0, b"\xCF\xFA\xED\xFE"),
            (0, b"\xCA\xFE\xBA\xBE"),
        ],
        "class" => &[(0, b"\xCA\xFE\xBA\xBE")],
        "wasm" => &[(0, b"\x00asm")],
        "beam" => &[(0, b"FOR1")],
        "a" | "lib" | "deb" => &[(0, b"!<arch>\n")],
        "msi" | "doc" | "xls" | "ppt" => &[(0, b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1")],
        "dmp" => &[(0, b"MDMP")],
        // --- documents / fonts ---
        "pdf" => &[(0, b"%PDF")],
        "ttf" => &[(0, b"\x00\x01\x00\x00"), (0, b"true")],
        "otf" => &[(0, b"OTTO")],
        "ttc" => &[(0, b"ttcf")],
        "woff" => &[(0, b"wOFF")],
        "woff2" => &[(0, b"wOF2")],
        // --- databases / data / ML ---
        "sqlite" | "sqlite3" | "db" => &[(0, b"SQLite format 3\x00")],
        "pdb" => &[(0, b"Microsoft C/C++ MSF")],
        "h5" | "hdf5" => &[(0, b"\x89HDF\r\n\x1a\n")],
        "npy" => &[(0, b"\x93NUMPY")],
        "gguf" => &[(0, b"GGUF")],
        "tflite" => &[(4, b"TFL3")],
        "parquet" => &[(0, b"PAR1")],
        "arrow" | "feather" => &[(0, b"ARROW1")],
        "mo" => &[(0, b"\xDE\x12\x04\x95"), (0, b"\x95\x04\x12\xDE")],
        "jks" | "keystore" => &[(0, b"\xFE\xED\xFE\xED")],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_text_recognizes_new_and_old() {
        assert!(is_known_text_ext_str("rs"));
        assert!(is_known_text_ext_str("log"));
        assert!(is_known_text_ext_str("csv"));
        assert!(!is_known_text_ext_str("png"));
        assert!(!is_known_text_ext_str("bin"));
    }

    #[test]
    fn classify_covers_the_three_buckets() {
        assert_eq!(classify_ext("png"), ExtClass::Signature);
        assert_eq!(classify_ext("zip"), ExtClass::Signature);
        assert_eq!(classify_ext("bin"), ExtClass::NoMarker);
        assert_eq!(classify_ext("tar"), ExtClass::NoMarker);
        assert_eq!(classify_ext("rs"), ExtClass::NotBinary);
        assert_eq!(classify_ext("txt"), ExtClass::NotBinary);
    }

    #[test]
    fn magic_confirmed_only_when_present() {
        assert!(header_confirms_binary("png", b"\x89PNG\r\n\x1a\nrest"));
        // A text file misnamed `.png` does not match the PNG magic.
        assert!(!header_confirms_binary("png", b"just some text\n"));
        assert!(header_confirms_binary("zip", b"PK\x03\x04\x14\x00"));
        assert!(header_confirms_binary("pdf", b"%PDF-1.7\n"));
        // ftyp lives at offset 4 for mp4.
        assert!(header_confirms_binary("mp4", b"\x00\x00\x00\x18ftypmp42"));
        assert!(!header_confirms_binary("mp4", b"ftyp at offset zero"));
        // Newly added Group-A formats.
        assert!(header_confirms_binary("heic", b"\x00\x00\x00\x18ftypheic"));
        assert!(header_confirms_binary("apk", b"PK\x03\x04\x14\x00"));
        assert!(header_confirms_binary("ko", b"\x7FELF\x02\x01"));
        assert!(header_confirms_binary("gguf", b"GGUF\x03\x00"));
        assert!(header_confirms_binary("npy", b"\x93NUMPY\x01\x00"));
        assert!(header_confirms_binary(
            "tflite",
            b"\x00\x00\x00\x14TFL3xxxx"
        ));
        assert!(header_confirms_binary("deb", b"!<arch>\ndebian"));
        assert!(!header_confirms_binary("apk", b"not a zip, just text\n"));
        // All Group-A extensions classify as Signature (magic-verified).
        for e in [
            "heic", "avif", "apk", "whl", "epub", "gguf", "npy", "parquet", "ttc", "msi",
        ] {
            assert_eq!(
                classify_ext(e),
                ExtClass::Signature,
                "{e} should be Signature"
            );
        }
    }

    #[test]
    fn short_header_never_panics() {
        assert!(!header_confirms_binary("png", b""));
        assert!(!header_confirms_binary("png", b"\x89"));
        assert!(!header_confirms_binary("mp4", b"abc"));
    }

    #[test]
    fn content_heuristic() {
        let pct = DEFAULT_HIGH_BYTE_PCT;
        // NUL → binary regardless.
        assert!(looks_binary_content(b"hello\x00world", pct));
        // Plain ASCII text → not binary.
        assert!(!looks_binary_content(
            b"fn main() { println!(\"hi\"); }\n",
            pct
        ));
        // Valid UTF-8 with heavy non-ASCII (CJK) → protected, NOT binary,
        // even though ~100% of bytes are > 127.
        let cjk = "日本語のテキストファイル、これはバイナリではない".as_bytes();
        assert!(cjk.iter().filter(|&&b| b > 127).count() * 100 > cjk.len() * pct as usize);
        assert!(!looks_binary_content(cjk, pct));
        // Accented Latin UTF-8 → text.
        assert!(!looks_binary_content(
            "café über niño señor".as_bytes(),
            pct
        ));
        // High-entropy, NUL-free, invalid UTF-8 (alternating high bytes) → binary.
        let hi: Vec<u8> = (0..2000u32)
            .map(|i| if i % 2 == 0 { 0xC0 } else { 0xFF })
            .collect();
        assert!(looks_binary_content(&hi, pct));
        // Truncated final multibyte sequence (sampling cut a char) → still text.
        let mut t = "áé".as_bytes().to_vec();
        t.push(0xC3); // lone UTF-8 lead byte at the end
        assert!(!looks_binary_content(&t, pct));
        // Empty → not binary.
        assert!(!looks_binary_content(b"", pct));
        // Tiny / malformed inputs must never panic and classify sanely.
        assert!(looks_binary_content(b"\xff", pct)); // 1 genuinely invalid byte → binary
        assert!(!looks_binary_content(b"\xc3", pct)); // lone lead byte (truncated) → text
        assert!(!looks_binary_content(b"ab", pct)); // 2 ASCII bytes → text
        assert!(looks_binary_content(b"\x80", pct)); // lone continuation → invalid + all-high → binary
    }
}
