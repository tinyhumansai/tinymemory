//! Telling the Office Open XML formats apart without a zip reader.
//!
//! Every `.docx`, `.xlsx` and `.pptx` opens with the same four bytes, because
//! each is a zip archive of XML parts. What distinguishes them is *which*
//! parts: a word-processing package keeps its body under `word/`, a workbook
//! under `xl/`, a presentation under `ppt/`. Those names sit uncompressed in
//! the archive's central directory, so reading them is a bounded walk over a
//! few hundred bytes at the end of the buffer — cheap enough to run on every
//! sniff, and with no dependency, so format detection stays available in a
//! build that converts nothing.

use super::DocumentFormat;

/// The end-of-central-directory record's signature.
const EOCD_SIGNATURE: &[u8; 4] = b"PK\x05\x06";
/// A central-directory file header's signature.
const CENTRAL_HEADER_SIGNATURE: &[u8; 4] = b"PK\x01\x02";
/// The fixed part of the end-of-central-directory record.
const EOCD_LEN: usize = 22;
/// The fixed part of a central-directory file header, before its name.
const CENTRAL_HEADER_LEN: usize = 46;
/// The longest archive comment a zip can carry, which bounds how far back
/// from the end the end-of-central-directory record can sit.
const MAX_COMMENT_LEN: usize = u16::MAX as usize;

/// The Office Open XML format a zip archive's part names say it is.
///
/// `None` when the central directory cannot be found or read, or when the
/// parts name no format — or more than one, which no Office package does and
/// which is therefore not evidence of anything.
pub(super) fn kind(bytes: &[u8]) -> Option<DocumentFormat> {
    let mut found = None;
    for name in entry_names(bytes)? {
        let format = if name.starts_with(b"word/") {
            DocumentFormat::Docx
        } else if name.starts_with(b"xl/") {
            DocumentFormat::Xlsx
        } else if name.starts_with(b"ppt/") {
            DocumentFormat::Pptx
        } else {
            continue;
        };
        match found {
            None => found = Some(format),
            Some(previous) if previous != format => return None,
            Some(_) => {}
        }
    }
    found
}

/// Every entry name the archive's central directory lists.
///
/// `None` when there is no end-of-central-directory record, when it points
/// outside the buffer (a zip64 archive, a truncated upload), or when a header
/// it leads to is malformed. Every read is bounds-checked: this runs on
/// untrusted bytes before anything else has looked at them.
fn entry_names(bytes: &[u8]) -> Option<Vec<&[u8]>> {
    let eocd = find_eocd(bytes)?;
    let entries = usize::from(read_u16(bytes, eocd + 10)?);
    let mut cursor = usize::try_from(read_u32(bytes, eocd + 16)?).ok()?;
    let mut names = Vec::with_capacity(entries);
    for _ in 0..entries {
        if bytes.get(cursor..cursor + 4)? != CENTRAL_HEADER_SIGNATURE {
            return None;
        }
        let name_len = usize::from(read_u16(bytes, cursor + 28)?);
        let extra_len = usize::from(read_u16(bytes, cursor + 30)?);
        let comment_len = usize::from(read_u16(bytes, cursor + 32)?);
        let name_start = cursor + CENTRAL_HEADER_LEN;
        names.push(bytes.get(name_start..name_start + name_len)?);
        cursor = name_start + name_len + extra_len + comment_len;
    }
    Some(names)
}

/// Offset of the end-of-central-directory record, searched backwards from the
/// end so a comment that happens to contain the signature cannot shadow it.
fn find_eocd(bytes: &[u8]) -> Option<usize> {
    let last = bytes.len().checked_sub(EOCD_LEN)?;
    let first = last.saturating_sub(MAX_COMMENT_LEN);
    (first..=last)
        .rev()
        .find(|&offset| bytes.get(offset..offset + 4) == Some(EOCD_SIGNATURE.as_slice()))
}

/// A little-endian `u16` at `offset`, if the buffer holds one there.
fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let raw = bytes.get(offset..offset + 2)?;
    Some(u16::from_le_bytes([raw[0], raw[1]]))
}

/// A little-endian `u32` at `offset`, if the buffer holds one there.
fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let raw = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

#[cfg(test)]
#[path = "ooxml_tests.rs"]
mod tests;
