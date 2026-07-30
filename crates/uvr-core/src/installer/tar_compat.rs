//! Compatibility shim for tarballs produced by R's internal tar.
//!
//! R's `utils::tar(internal = TRUE)` — used by `R CMD INSTALL --build`, and
//! therefore by P3M's Linux binary packages — records the link *target's* file
//! size in the header of symlink/hardlink entries instead of 0. POSIX says
//! link entries have size 0 and carry no data blocks, and that's what these
//! archives actually contain: the next header follows immediately.
//!
//! GNU tar, bsdtar, and Python's `tarfile` all ignore the size field on link
//! entries, so the archives extract fine everywhere — except with the Rust
//! `tar` crate (through at least 0.4.46), which trusts the size field, skips
//! that many phantom bytes, lands in the middle of a later entry, and fails
//! with "numeric field was not a number ... when getting cksum" (#203).
//!
//! [`LinkSizeFix`] wraps the raw (decompressed) tar byte stream and rewrites
//! the size field of link-entry headers to 0 (recomputing the header
//! checksum) before the `tar` crate parses them, restoring the behavior every
//! other tar reader already has.

use std::io::Read;

const BLOCK: usize = 512;
const SIZE_OFFSET: usize = 124;
const SIZE_LEN: usize = 12;
const CKSUM_OFFSET: usize = 148;
const CKSUM_LEN: usize = 8;
const TYPEFLAG_OFFSET: usize = 156;

/// `Read` adapter that zeroes the size field of tar link-entry headers.
pub struct LinkSizeFix<R: Read> {
    inner: R,
    /// Current 512-byte block being served (header blocks only).
    buf: [u8; BLOCK],
    /// Bytes of `buf` already handed to the consumer.
    buf_pos: usize,
    /// Bytes of `buf` filled (fewer than BLOCK only at a truncated tail).
    buf_len: usize,
    /// Data bytes (incl. padding) to pass through before the next header.
    data_remaining: u64,
    /// Inner stream reached EOF.
    eof: bool,
}

impl<R: Read> LinkSizeFix<R> {
    pub fn new(inner: R) -> Self {
        LinkSizeFix {
            inner,
            buf: [0u8; BLOCK],
            buf_pos: 0,
            buf_len: 0,
            data_remaining: 0,
            eof: false,
        }
    }

    /// Read exactly one 512-byte block (or whatever remains at EOF).
    fn fill_block(&mut self) -> std::io::Result<()> {
        self.buf_pos = 0;
        self.buf_len = 0;
        while self.buf_len < BLOCK {
            match self.inner.read(&mut self.buf[self.buf_len..])? {
                0 => {
                    self.eof = true;
                    break;
                }
                n => self.buf_len += n,
            }
        }
        Ok(())
    }

    /// Parse the octal size field. Returns `None` for GNU base-256 encoding
    /// (top bit set — only produced for >8GB entries, never links) or
    /// unparseable content; those headers are passed through untouched.
    fn parse_octal_size(field: &[u8]) -> Option<u64> {
        if field.first().is_some_and(|b| b & 0x80 != 0) {
            return None;
        }
        let s: &[u8] = {
            let end = field
                .iter()
                .position(|&b| b == 0 || b == b' ')
                .unwrap_or(field.len());
            &field[..end]
        };
        let s = std::str::from_utf8(s).ok()?.trim();
        if s.is_empty() {
            return Some(0);
        }
        u64::from_str_radix(s, 8).ok()
    }

    /// If this block is a link-entry header with a nonzero size, zero the
    /// size field and recompute the checksum. Returns the data length the
    /// *tar crate* should expect to follow (0 for patched link entries).
    fn normalize_header(&mut self) -> u64 {
        // All-zero blocks are end-of-archive markers; pass through.
        if self.buf[..self.buf_len].iter().all(|&b| b == 0) {
            return 0;
        }
        let size = match Self::parse_octal_size(&self.buf[SIZE_OFFSET..SIZE_OFFSET + SIZE_LEN]) {
            Some(s) => s,
            None => return 0, // base-256/unparseable: leave alone, assume no skew
        };
        let typeflag = self.buf[TYPEFLAG_OFFSET];
        // '1' = hardlink, '2' = symlink: POSIX mandates no data blocks. R's
        // internal tar writes the target size here; zero it so the tar crate
        // doesn't skip phantom bytes.
        if (typeflag == b'1' || typeflag == b'2') && size > 0 {
            let mut new_size = [b'0'; SIZE_LEN];
            new_size[SIZE_LEN - 1] = 0;
            self.buf[SIZE_OFFSET..SIZE_OFFSET + SIZE_LEN].copy_from_slice(&new_size);
            // Checksum: sum of all header bytes with the checksum field
            // treated as spaces, written as 6 octal digits + NUL + space.
            let mut sum: u64 = 0;
            for (i, &b) in self.buf[..BLOCK].iter().enumerate() {
                if (CKSUM_OFFSET..CKSUM_OFFSET + CKSUM_LEN).contains(&i) {
                    sum += b' ' as u64;
                } else {
                    sum += b as u64;
                }
            }
            let cksum = format!("{sum:06o}\0 ");
            self.buf[CKSUM_OFFSET..CKSUM_OFFSET + CKSUM_LEN].copy_from_slice(cksum.as_bytes());
            return 0;
        }
        // Everything else (regular files, dirs, GNU 'L'/'K' long names, PAX
        // 'x'/'g' extended headers, ...) carries `size` bytes of data,
        // rounded up to whole blocks.
        size.div_ceil(BLOCK as u64) * BLOCK as u64
    }
}

impl<R: Read> Read for LinkSizeFix<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        // Serve any buffered header bytes first.
        if self.buf_pos < self.buf_len {
            let n = (self.buf_len - self.buf_pos).min(out.len());
            out[..n].copy_from_slice(&self.buf[self.buf_pos..self.buf_pos + n]);
            self.buf_pos += n;
            return Ok(n);
        }
        // Inside an entry's data: pass through without inspection.
        if self.data_remaining > 0 {
            let want = (self.data_remaining.min(out.len() as u64)) as usize;
            let n = self.inner.read(&mut out[..want])?;
            if n == 0 {
                self.eof = true;
            }
            self.data_remaining -= n as u64;
            return Ok(n);
        }
        if self.eof {
            return Ok(0);
        }
        // At a header boundary: buffer one block, normalize, serve from it.
        self.fill_block()?;
        if self.buf_len == 0 {
            return Ok(0);
        }
        if self.buf_len == BLOCK {
            self.data_remaining = self.normalize_header();
        }
        let n = self.buf_len.min(out.len());
        out[..n].copy_from_slice(&self.buf[..n]);
        self.buf_pos = n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a raw tar header the way R's internal tar does for a symlink:
    /// link target in `linkname`, but `size` set to the target's byte count.
    fn r_style_symlink_header(path: &str, target: &str, bogus_size: u64) -> [u8; BLOCK] {
        let mut h = tar::Header::new_gnu();
        h.set_path(path).unwrap();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_link_name(target).unwrap();
        h.set_size(bogus_size);
        h.set_mode(0o777);
        h.set_cksum();
        let mut out = [0u8; BLOCK];
        out.copy_from_slice(h.as_bytes());
        out
    }

    fn regular_file(path: &str, content: &[u8]) -> Vec<u8> {
        let mut h = tar::Header::new_gnu();
        h.set_path(path).unwrap();
        h.set_size(content.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        let mut out = Vec::new();
        out.extend_from_slice(h.as_bytes());
        out.extend_from_slice(content);
        let pad = (BLOCK - content.len() % BLOCK) % BLOCK;
        out.extend(std::iter::repeat_n(0u8, pad));
        out
    }

    /// Archive layout mirroring the RcppParallel P3M tarball (#203): a
    /// symlink whose header claims a multi-MB size, immediately followed by
    /// another entry. Without the fix, the tar crate skips "size" bytes and
    /// dies with a cksum parse error; with it, everything parses.
    fn bogus_archive() -> Vec<u8> {
        let mut ar = Vec::new();
        ar.extend_from_slice(&regular_file("pkg/DESCRIPTION", b"Package: pkg\n"));
        ar.extend_from_slice(&r_style_symlink_header(
            "pkg/lib/libtbb.so",
            "libtbb.so.2",
            5_078_648,
        ));
        ar.extend_from_slice(&regular_file("pkg/lib/after.txt", b"survived"));
        ar.extend(std::iter::repeat_n(0u8, BLOCK * 2)); // end-of-archive
        ar
    }

    #[test]
    fn unfixed_stream_reproduces_the_bug() {
        // Guard test: prove the raw stream really does break the tar crate,
        // so the fix below is testing something real.
        let raw = bogus_archive();
        let mut archive = tar::Archive::new(&raw[..]);
        let result: std::result::Result<Vec<_>, _> = archive.entries().unwrap().collect();
        assert!(
            result.is_err(),
            "tar crate should choke on R-style symlink sizes"
        );
    }

    #[test]
    fn fixed_stream_parses_all_entries_and_preserves_content() {
        let raw = bogus_archive();
        let mut archive = tar::Archive::new(LinkSizeFix::new(&raw[..]));
        let mut names = Vec::new();
        let mut after_content = String::new();
        let mut link_target = None;
        for entry in archive.entries().unwrap() {
            let mut e = entry.expect("entry parses after size fix");
            names.push(e.path().unwrap().display().to_string());
            if names.last().unwrap().ends_with("after.txt") {
                e.read_to_string(&mut after_content).unwrap();
            }
            if e.header().entry_type() == tar::EntryType::Symlink {
                link_target = e.link_name().unwrap().map(|p| p.display().to_string());
            }
        }
        assert_eq!(
            names,
            vec!["pkg/DESCRIPTION", "pkg/lib/libtbb.so", "pkg/lib/after.txt"]
        );
        assert_eq!(after_content, "survived");
        assert_eq!(link_target.as_deref(), Some("libtbb.so.2"));
    }

    #[test]
    fn conforming_archives_pass_through_byte_identical() {
        // A well-formed archive (symlink size 0, long names, subdirs) must
        // come through completely unmodified.
        let mut buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut buf);
            let mut h = tar::Header::new_gnu();
            h.set_path("pkg/DESCRIPTION").unwrap();
            h.set_size(4);
            h.set_mode(0o644);
            h.set_cksum();
            builder.append(&h, &b"abcd"[..]).unwrap();
            let mut s = tar::Header::new_gnu();
            s.set_path("pkg/link").unwrap();
            s.set_entry_type(tar::EntryType::Symlink);
            s.set_link_name("DESCRIPTION").unwrap();
            s.set_size(0);
            s.set_cksum();
            builder.append(&s, std::io::empty()).unwrap();
            // A name long enough to force a GNU 'L' long-name entry, whose
            // data blocks must NOT be treated as headers.
            let long = format!("pkg/{}/file.txt", "d".repeat(120));
            let mut l = tar::Header::new_gnu();
            l.set_size(2);
            l.set_mode(0o644);
            l.set_cksum();
            builder.append_data(&mut l, &long, &b"ok"[..]).unwrap();
            builder.finish().unwrap();
        }
        let mut fixed = Vec::new();
        LinkSizeFix::new(&buf[..]).read_to_end(&mut fixed).unwrap();
        assert_eq!(
            buf, fixed,
            "conforming archive must pass through unmodified"
        );
    }

    #[test]
    fn hardlink_with_bogus_size_is_also_normalized() {
        let mut ar = Vec::new();
        ar.extend_from_slice(&regular_file("pkg/a.txt", b"aa"));
        let mut h = tar::Header::new_gnu();
        h.set_path("pkg/b.txt").unwrap();
        h.set_entry_type(tar::EntryType::Link);
        h.set_link_name("pkg/a.txt").unwrap();
        h.set_size(1234);
        h.set_cksum();
        ar.extend_from_slice(h.as_bytes());
        ar.extend_from_slice(&regular_file("pkg/c.txt", b"cc"));
        ar.extend(std::iter::repeat_n(0u8, BLOCK * 2));

        let mut archive = tar::Archive::new(LinkSizeFix::new(&ar[..]));
        let names: Vec<String> = archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().display().to_string())
            .collect();
        assert_eq!(names, vec!["pkg/a.txt", "pkg/b.txt", "pkg/c.txt"]);
    }
}
