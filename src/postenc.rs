//! Compact posting encoding for the line-level trigram index.
//!
//! Each posting is `(doc_id, line_no, byte_offset)`. Within a trigram's posting
//! list the postings are sorted ascending by `(doc_id, line_no)`, which lets us
//! delta-encode instead of storing absolute 32-bit fields. The old format spent
//! a flat 16 bytes per posting (doc/line/offset/prefix); this packs the three
//! surviving fields to ~2.9 bytes. See `COMPACT_POSTINGS.md` for the rationale
//! and measurements.
//!
//! ## Wire format (per posting)
//!
//! 1. A **flagged varint** carrying the same-doc flag plus doc/line:
//!    - `byte0 = [bit7 same-doc flag][bit6 continuation][bits5..0 payload]`
//!    - further bytes: standard LEB128 (7 payload bits each)
//!    - `flag = 1` (same doc as previous posting): payload = `line_no - prev_line`
//!    - `flag = 0` (new document): payload = `doc_id - prev_doc` (absolute for the
//!      first posting in the list), followed by a standard varint with the
//!      **absolute** `line_no` (the line resets at a document boundary).
//! 2. A standard varint for `byte_offset`:
//!    - same doc -> `byte_offset - prev_offset`
//!    - new doc  -> absolute `byte_offset`
//!
//! Because ~88% of postings stay within the same document, the flag bit lets a
//! same-doc posting spend a single byte for doc+line in the common case.

/// Append a standard LEB128 varint.
#[inline]
pub fn write_varint(buf: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        buf.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

/// Read a standard LEB128 varint. Returns `(value, new_pos)`.
#[inline]
fn read_varint(buf: &[u8], mut pos: usize) -> (u64, usize) {
    let mut v = 0u64;
    let mut shift = 0u32;
    loop {
        let b = buf[pos];
        pos += 1;
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    (v, pos)
}

/// Append a flagged varint: a bool flag in bit7 of byte0, six payload bits in
/// byte0 (bit6 = continuation), the remainder as standard LEB128.
#[inline]
fn write_flagged(buf: &mut Vec<u8>, flag: bool, v: u64) {
    let mut b0 = ((flag as u8) << 7) | (v as u8 & 0x3f);
    let rest = v >> 6;
    if rest == 0 {
        buf.push(b0);
    } else {
        b0 |= 0x40; // continuation
        buf.push(b0);
        write_varint(buf, rest);
    }
}

/// Read a flagged varint. Returns `(flag, value, new_pos)`.
#[inline]
fn read_flagged(buf: &[u8], mut pos: usize) -> (bool, u64, usize) {
    let b0 = buf[pos];
    pos += 1;
    let flag = b0 & 0x80 != 0;
    let mut v = (b0 & 0x3f) as u64;
    if b0 & 0x40 != 0 {
        let (rest, p) = read_varint(buf, pos);
        v |= rest << 6;
        pos = p;
    }
    (flag, v, pos)
}

/// Encodes a sorted run of postings (one trigram's list), tracking previous
/// values to delta-encode. Callers must `push` in ascending `(doc, line)` order.
#[derive(Default)]
pub struct PostingWriter {
    started: bool,
    prev_doc: u32,
    prev_line: u32,
    prev_off: u32,
}

impl PostingWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The `(doc, line)` of the last posting pushed, or `None` if empty.
    /// Used by the packed in-memory builder to dedup `(trigram, doc, line)`
    /// without keeping the decoded postings around.
    #[inline]
    pub fn last_dl(&self) -> Option<(u32, u32)> {
        if self.started {
            Some((self.prev_doc, self.prev_line))
        } else {
            None
        }
    }

    /// Append one posting's bytes to `buf`.
    pub fn push(&mut self, buf: &mut Vec<u8>, doc: u32, line: u32, off: u32) {
        if self.started && doc == self.prev_doc {
            write_flagged(buf, true, (line - self.prev_line) as u64);
            write_varint(buf, (off - self.prev_off) as u64);
        } else {
            let doc_delta = if self.started {
                doc - self.prev_doc
            } else {
                doc
            };
            write_flagged(buf, false, doc_delta as u64);
            write_varint(buf, line as u64);
            write_varint(buf, off as u64);
        }
        self.started = true;
        self.prev_doc = doc;
        self.prev_line = line;
        self.prev_off = off;
    }
}

/// Decodes a posting blob back into `(doc_id, line_no, byte_offset)` tuples.
pub struct PostingReader<'a> {
    buf: &'a [u8],
    pos: usize,
    started: bool,
    prev_doc: u32,
    prev_line: u32,
    prev_off: u32,
}

impl<'a> PostingReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            started: false,
            prev_doc: 0,
            prev_line: 0,
            prev_off: 0,
        }
    }
}

impl Iterator for PostingReader<'_> {
    type Item = (u32, u32, u32);

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let (flag, v, p) = read_flagged(self.buf, self.pos);
        self.pos = p;
        let (doc, line, off);
        if flag {
            doc = self.prev_doc;
            line = self.prev_line + v as u32;
            let (od, p2) = read_varint(self.buf, self.pos);
            self.pos = p2;
            off = self.prev_off + od as u32;
        } else {
            doc = if self.started {
                self.prev_doc + v as u32
            } else {
                v as u32
            };
            let (l, p2) = read_varint(self.buf, self.pos);
            let (o, p3) = read_varint(self.buf, p2);
            self.pos = p3;
            line = l as u32;
            off = o as u32;
        }
        self.started = true;
        self.prev_doc = doc;
        self.prev_line = line;
        self.prev_off = off;
        Some((doc, line, off))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(postings: &[(u32, u32, u32)]) {
        let mut buf = Vec::new();
        let mut w = PostingWriter::new();
        for &(d, l, o) in postings {
            w.push(&mut buf, d, l, o);
        }
        let got: Vec<_> = PostingReader::new(&buf).collect();
        assert_eq!(got, postings, "roundtrip mismatch");
    }

    #[test]
    fn empty() {
        roundtrip(&[]);
    }

    #[test]
    fn single() {
        roundtrip(&[(0, 1, 0)]);
    }

    #[test]
    fn same_doc_run() {
        roundtrip(&[(5, 1, 0), (5, 2, 20), (5, 10, 300), (5, 200, 5000)]);
    }

    #[test]
    fn multi_doc() {
        roundtrip(&[(0, 1, 0), (0, 5, 80), (3, 1, 0), (3, 2, 40), (100, 9, 1234)]);
    }

    #[test]
    fn doc_zero_first() {
        roundtrip(&[(0, 1, 0), (1, 1, 0)]);
    }

    #[test]
    fn large_values() {
        roundtrip(&[
            (0, 1, 0),
            (0, 70_000, 4_000_000),
            (500_000, 1, 0),
            (500_000, 300_000, 2_000_000_000),
        ]);
    }

    #[test]
    fn varint_edges() {
        for v in [
            0u64,
            1,
            63,
            64,
            127,
            128,
            16_383,
            16_384,
            1 << 20,
            1 << 28,
            u32::MAX as u64,
        ] {
            let mut b = Vec::new();
            write_varint(&mut b, v);
            let (got, n) = read_varint(&b, 0);
            assert_eq!(got, v);
            assert_eq!(n, b.len());
        }
    }

    #[test]
    fn flagged_edges() {
        for &flag in &[true, false] {
            for v in [0u64, 1, 63, 64, 65, 4095, 4096, 1 << 20, u32::MAX as u64] {
                let mut b = Vec::new();
                write_flagged(&mut b, flag, v);
                let (f, got, n) = read_flagged(&b, 0);
                assert_eq!(f, flag);
                assert_eq!(got, v, "flagged v={v}");
                assert_eq!(n, b.len());
            }
        }
    }
}
