//! Tests for reading Office part names out of a zip's central directory.

use super::*;

/// A stored (uncompressed) zip holding `entries`, with an optional archive
/// comment.
fn archive(entries: &[&str], comment: &str) -> Vec<u8> {
    use std::io::Write;

    let mut buffer = std::io::Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(&mut buffer);
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for entry in entries {
        writer.start_file(*entry, options).unwrap();
        writer.write_all(b"<x/>").unwrap();
    }
    writer.set_comment(comment.to_string()).unwrap();
    writer.finish().unwrap();
    buffer.into_inner()
}

#[test]
fn every_entry_name_is_read_in_directory_order() {
    let bytes = archive(&["[Content_Types].xml", "xl/workbook.xml"], "");
    assert_eq!(
        entry_names(&bytes).unwrap(),
        vec![
            b"[Content_Types].xml".as_slice(),
            b"xl/workbook.xml".as_slice()
        ]
    );
}

#[test]
fn a_trailing_archive_comment_does_not_hide_the_directory() {
    // The end record sits before the comment, so the search has to walk back
    // past it — including a comment that itself mentions a part name.
    let bytes = archive(&["ppt/presentation.xml"], "exported from word/ by hand");
    assert_eq!(kind(&bytes), Some(DocumentFormat::Pptx));
}

#[test]
fn a_truncated_archive_names_nothing() {
    // An upload cut short loses its central directory first, since it is at
    // the end. That is "no evidence", not a guess.
    let bytes = archive(&["xl/workbook.xml"], "");
    assert_eq!(kind(&bytes[..bytes.len() - 10]), None);
}

#[test]
fn a_directory_pointing_outside_the_buffer_names_nothing() {
    let mut bytes = archive(&["xl/workbook.xml"], "");
    let eocd = find_eocd(&bytes).unwrap();
    bytes[eocd + 16..eocd + 20].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(kind(&bytes), None);
}

#[test]
fn parts_from_two_office_formats_are_not_evidence() {
    // No Office package mixes `word/` and `xl/` parts; a zip that does is not
    // one, and picking either would be a coin toss.
    let bytes = archive(&["word/document.xml", "xl/workbook.xml"], "");
    assert_eq!(kind(&bytes), None);
}

#[test]
fn a_zip_with_no_office_parts_names_nothing() {
    let bytes = archive(&["photo.jpg", "notes/readme.txt"], "");
    assert_eq!(kind(&bytes), None);
}

#[test]
fn a_buffer_too_short_for_an_end_record_names_nothing() {
    assert_eq!(kind(b"PK\x03\x04"), None);
    assert_eq!(kind(b""), None);
}
