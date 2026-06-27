//! File coding system: the BOM + line-ending convention of a visited file,
//! detected on open and restored on save — Emacs's `buffer-file-coding-system`,
//! pared down to what a UTF-8 editor needs.
//!
//! Emacs presents a clean buffer for editing (LF-only lines, no BOM character)
//! but does NOT silently rewrite a file's on-disk format: it remembers the
//! detected coding and round-trips it on save, so a CRLF Windows file stays
//! CRLF and a BOM stays a BOM unless you deliberately change the coding (e.g.
//! `set-buffer-file-coding-system 'utf-8-unix`). We do the same.
//!
//! Two conventions are modeled: a UTF-8 signature (BOM) and DOS (`\r\n`) line
//! endings. Classic-Mac CR-only endings are deliberately NOT a coding — that
//! format is extinct, and treating a lone `\r` as an EOL is lossy for any
//! embedded `\r\n` (it would round-trip to `\r\r`). A CR-only or `\r`-bearing
//! file is simply detected as plain Unix and kept byte-for-byte (its `\r`s stay
//! literal in the buffer), never mangled. Everything is UTF-8; charset
//! transcoding is out of scope (the engine rejects non-UTF-8 on open).

/// UTF-8 byte-order mark.
pub const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Line-ending convention of a file.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Eol {
    /// `\n` — the buffer's internal form; no conversion.
    #[default]
    Unix,
    /// `\r\n` — folded to `\n` in the buffer, restored on save.
    Dos,
}

/// The visited file's coding: whether it carries a UTF-8 BOM and its EOL style.
/// `default()` is `utf-8-unix` (no BOM, `\n`) — the no-op, zero-overhead case.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FileCoding {
    pub had_bom: bool,
    pub eol: Eol,
}

impl FileCoding {
    pub fn new(had_bom: bool, eol: Eol) -> FileCoding {
        FileCoding { had_bom, eol }
    }

    /// `true` when the buffer needs no normalization on open and no restoration
    /// on save — the fast path (a plain UTF-8/`\n` file).
    pub fn is_plain(self) -> bool {
        !self.had_bom && self.eol == Eol::Unix
    }

    /// The Emacs coding-system name, for display (`session_status`).
    pub fn name(self) -> &'static str {
        match (self.had_bom, self.eol) {
            (false, Eol::Unix) => "utf-8-unix",
            (false, Eol::Dos) => "utf-8-dos",
            (true, Eol::Unix) => "utf-8-with-signature-unix",
            (true, Eol::Dos) => "utf-8-with-signature-dos",
        }
    }

    /// Parse a coding-system name as accepted by `set-buffer-file-coding-system`,
    /// matched against a known set so a typo or an unsupported charset (e.g.
    /// `latin-1`, or the unsupported `…-mac`) is `None` — an error — rather than
    /// a silent fallback that would quietly rewrite the file on save. The bare
    /// EOL names (`unix`/`dos`) change only the EOL, keeping the current BOM.
    pub fn parse(name: &str, base: FileCoding) -> Option<FileCoding> {
        let coding = |had_bom, eol| Some(FileCoding { had_bom, eol });
        match name {
            "unix" => Some(FileCoding {
                eol: Eol::Unix,
                ..base
            }),
            "dos" => Some(FileCoding {
                eol: Eol::Dos,
                ..base
            }),
            "utf-8" | "utf-8-unix" => coding(false, Eol::Unix),
            "utf-8-dos" => coding(false, Eol::Dos),
            "utf-8-with-signature" | "utf-8-with-signature-unix" => coding(true, Eol::Unix),
            "utf-8-with-signature-dos" => coding(true, Eol::Dos),
            _ => None,
        }
    }
}

/// Decode a file's (already UTF-8-validated) text into the buffer's internal
/// form: drop a leading BOM and fold `\r\n` to `\n`. Takes `&str` so the caller
/// owns UTF-8 validation — an invalid file must be a hard open error, not a
/// silently-empty buffer. A lone `\r` (not part of `\r\n`) is content and is
/// preserved.
pub fn decode(text: &str, coding: FileCoding) -> String {
    let s = text.strip_prefix('\u{feff}').unwrap_or(text);
    match coding.eol {
        Eol::Unix => s.to_string(),
        Eol::Dos => s.replace("\r\n", "\n"),
    }
}

/// Write `buf` with every `\n` expanded to `\r\n`, returning the bytes written.
/// The shared DOS encoder for both [`CodingWriter`] and Quire's byte-exact save
/// of inserted text.
///
/// A `\r` already present passes through as content, so inserted text containing
/// a literal `\r\n` is written `\r\r\n` (the `\r` is a CR char, the `\n` becomes
/// the line ending) — Emacs-faithful and round-tripping (the buffer keeps the
/// inserted CR as a lone-CR char). Plain text insertion has no `\r` and is
/// unaffected.
pub(crate) fn write_lf_as_crlf(w: &mut dyn std::io::Write, buf: &[u8]) -> std::io::Result<usize> {
    let mut written = 0;
    let mut start = 0;
    for (i, &b) in buf.iter().enumerate() {
        if b == b'\n' {
            w.write_all(&buf[start..i])?;
            w.write_all(b"\r\n")?;
            written += (i - start) + 2;
            start = i + 1;
        }
    }
    w.write_all(&buf[start..])?;
    written += buf.len() - start;
    Ok(written)
}

/// A `Write` adapter that re-applies a [`FileCoding`] on the fly: it emits the
/// BOM once (before any byte, so a BOM-only empty file still gets one) and
/// expands each `\n` to `\r\n` for DOS as bytes stream past. This lets the save
/// path restore BOM/CRLF without materializing the whole document — the large
/// `Quire` streams piece by piece straight through it.
pub struct CodingWriter<'a> {
    inner: &'a mut dyn std::io::Write,
    coding: FileCoding,
    bom_written: bool,
    /// Bytes actually written to `inner` (post-encoding), for the save report.
    written: usize,
}

impl<'a> CodingWriter<'a> {
    pub fn new(inner: &'a mut dyn std::io::Write, coding: FileCoding) -> Self {
        CodingWriter {
            inner,
            coding,
            bom_written: false,
            written: 0,
        }
    }

    fn put(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.inner.write_all(bytes)?;
        self.written += bytes.len();
        Ok(())
    }

    fn emit_bom(&mut self) -> std::io::Result<()> {
        if self.coding.had_bom && !self.bom_written {
            self.bom_written = true;
            self.put(&BOM)?;
        }
        Ok(())
    }

    /// Emit the BOM if nothing was written — so a BOM-only empty file still gets
    /// its signature — and return the on-disk byte count.
    pub fn finish(&mut self) -> std::io::Result<usize> {
        self.emit_bom()?;
        Ok(self.written)
    }
}

impl std::io::Write for CodingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if !buf.is_empty() {
            self.emit_bom()?;
        }
        match self.coding.eol {
            Eol::Unix => self.put(buf)?,
            Eol::Dos => self.written += write_lf_as_crlf(self.inner, buf)?,
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Drive the production `CodingWriter` to get the on-disk bytes for `text`
    /// — the inverse of `decode`, exercised exactly as the save path uses it.
    fn encode(text: &str, coding: FileCoding) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cw = CodingWriter::new(&mut out, coding);
        cw.write_all(text.as_bytes()).unwrap();
        cw.finish().unwrap();
        out
    }

    #[test]
    fn decode_then_encode_round_trips() {
        for coding in [
            FileCoding::new(false, Eol::Dos),
            FileCoding::new(true, Eol::Dos),
            FileCoding::new(true, Eol::Unix),
        ] {
            let original = encode("line one\nline two\n", coding);
            let text = decode(std::str::from_utf8(&original).unwrap(), coding);
            assert_eq!(text, "line one\nline two\n", "decode normalizes to LF");
            assert!(!text.contains('\r'), "buffer is LF-only");
            assert_eq!(
                encode(&text, coding),
                original,
                "save restores the file form"
            );
        }
    }

    #[test]
    fn lone_cr_is_preserved_not_treated_as_eol() {
        // A `\r` not part of `\r\n` is content: decode (Dos) keeps it, and it
        // survives a round-trip rather than becoming a spurious line break.
        let c = FileCoding::new(false, Eol::Dos);
        assert_eq!(decode("a\rb\r\nc", c), "a\rb\nc");
        assert_eq!(encode("a\rb\nc", c), b"a\rb\r\nc");
    }

    #[test]
    fn bom_only_empty_file_still_writes_bom() {
        let c = FileCoding::new(true, Eol::Unix);
        assert_eq!(encode("", c), BOM.to_vec());
        assert_eq!(decode("\u{feff}", c), "");
    }

    #[test]
    fn parse_names() {
        let plain = FileCoding::default();
        assert_eq!(FileCoding::parse("utf-8-unix", plain), Some(plain));
        assert_eq!(FileCoding::parse("utf-8-dos", plain).unwrap().eol, Eol::Dos);
        assert!(
            FileCoding::parse("utf-8-with-signature-dos", plain)
                .unwrap()
                .had_bom
        );
        // bare EOL name keeps the current BOM
        let withbom = FileCoding::new(true, Eol::Dos);
        assert_eq!(
            FileCoding::parse("unix", withbom),
            Some(FileCoding::new(true, Eol::Unix))
        );
        assert_eq!(FileCoding::default().name(), "utf-8-unix");
    }

    #[test]
    fn parse_rejects_unknown_or_unsupported_names() {
        // A typo, an unsupported charset, or the dropped `mac` EOL must error.
        let plain = FileCoding::default();
        assert_eq!(FileCoding::parse("latin-1", plain), None);
        assert_eq!(FileCoding::parse("utf-8-mac", plain), None);
        assert_eq!(FileCoding::parse("utf-8-with-signatuer-dos", plain), None);
        assert_eq!(FileCoding::parse("", plain), None);
    }
}
