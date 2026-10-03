//! Tests for document format detection.

use super::*;

#[test]
fn magic_bytes_beat_a_wrong_mime_type_and_a_wrong_extension() {
    let pdf = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n";
    assert_eq!(
        DocumentFormat::sniff(pdf, Some("notes.txt"), Some("text/plain")),
        DocumentFormat::Pdf
    );
}

#[test]
fn a_zip_container_is_reported_as_docx() {
    // Every OOXML file is a zip; a bare header with no central directory has
    // no part names to tell docx from xlsx, so the container is reported.
    let zip = b"PK\x03\x04\x14\x00\x06\x00";
    assert_eq!(DocumentFormat::from_magic(zip), Some(DocumentFormat::Docx));
}

#[test]
fn from_magic_says_keep_looking_rather_than_unknown() {
    assert_eq!(DocumentFormat::from_magic(b"# A heading"), None);
    assert_eq!(DocumentFormat::from_magic(b""), None);
}

#[test]
fn a_declared_mime_type_beats_the_filename() {
    assert_eq!(
        DocumentFormat::sniff(b"hello", Some("notes.txt"), Some("text/markdown")),
        DocumentFormat::Markdown
    );
}

#[test]
fn mime_parameters_and_casing_are_ignored() {
    assert_eq!(
        DocumentFormat::from_mime("Text/HTML; charset=UTF-8"),
        Some(DocumentFormat::Html)
    );
    assert_eq!(
        DocumentFormat::from_mime("text/plain ; charset=utf-8"),
        Some(DocumentFormat::PlainText)
    );
}

#[test]
fn an_octet_stream_mime_falls_through_to_the_filename() {
    assert_eq!(
        DocumentFormat::sniff(
            b"hello there",
            Some("notes.md"),
            Some("application/octet-stream")
        ),
        DocumentFormat::Markdown
    );
}

#[test]
fn every_recognised_extension_maps_to_a_format() {
    for (filename, expected) in [
        ("a.md", DocumentFormat::Markdown),
        ("a.markdown", DocumentFormat::Markdown),
        ("a.txt", DocumentFormat::PlainText),
        ("a.log", DocumentFormat::PlainText),
        ("a.html", DocumentFormat::Html),
        ("a.htm", DocumentFormat::Html),
        ("a.pdf", DocumentFormat::Pdf),
        ("a.docx", DocumentFormat::Docx),
        ("a.xlsx", DocumentFormat::Xlsx),
        ("a.xlsm", DocumentFormat::Xlsx),
        ("a.pptx", DocumentFormat::Pptx),
        ("path/to/report.PDF", DocumentFormat::Pdf),
    ] {
        assert_eq!(
            DocumentFormat::from_filename(filename),
            Some(expected),
            "{filename}"
        );
    }
}

#[test]
fn a_filename_with_no_extension_maps_to_nothing() {
    assert_eq!(DocumentFormat::from_filename("README"), None);
    assert_eq!(DocumentFormat::from_filename(""), None);
}

#[test]
fn legacy_doc_is_not_claimed_as_docx() {
    // `.doc` and `application/msword` name the legacy binary Word format, not
    // the Open XML `.docx` package the `Docx` converter targets.
    assert_eq!(DocumentFormat::from_filename("report.doc"), None);
    assert_eq!(DocumentFormat::from_mime("application/msword"), None);
}

#[test]
fn html_is_recognised_from_its_opening_alone() {
    assert_eq!(
        DocumentFormat::sniff(b"<!DOCTYPE html><html><body>hi</body></html>", None, None),
        DocumentFormat::Html
    );
    assert_eq!(
        DocumentFormat::sniff(b"  <html lang=\"en\">hi</html>", None, None),
        DocumentFormat::Html
    );
}

#[test]
fn unlabelled_text_is_plain_text() {
    assert_eq!(
        DocumentFormat::sniff(b"just some prose", None, None),
        DocumentFormat::PlainText
    );
}

#[test]
fn unlabelled_binary_is_unknown() {
    assert_eq!(
        DocumentFormat::sniff(&[0x00, 0x01, 0x02, 0xFF], None, None),
        DocumentFormat::Unknown
    );
}

#[test]
fn an_empty_buffer_is_unknown() {
    assert_eq!(
        DocumentFormat::sniff(b"", None, None),
        DocumentFormat::Unknown
    );
}

#[test]
fn textual_formats_are_the_ones_intake_can_decode_itself() {
    assert!(DocumentFormat::Markdown.is_textual());
    assert!(DocumentFormat::PlainText.is_textual());
    assert!(DocumentFormat::Html.is_textual());
    assert!(DocumentFormat::Code.is_textual());
    assert!(!DocumentFormat::Pdf.is_textual());
    assert!(!DocumentFormat::Docx.is_textual());
    assert!(!DocumentFormat::Xlsx.is_textual());
    assert!(!DocumentFormat::Pptx.is_textual());
    assert!(!DocumentFormat::Unknown.is_textual());
}

#[test]
fn a_canonical_mime_round_trips_back_to_its_format() {
    for format in [
        DocumentFormat::Markdown,
        DocumentFormat::PlainText,
        DocumentFormat::Html,
        DocumentFormat::Code,
        DocumentFormat::Pdf,
        DocumentFormat::Docx,
        DocumentFormat::Xlsx,
        DocumentFormat::Pptx,
    ] {
        assert_eq!(DocumentFormat::from_mime(format.mime()), Some(format));
    }
}

#[test]
fn a_canonical_extension_round_trips_back_to_its_format() {
    for format in [
        DocumentFormat::Markdown,
        DocumentFormat::PlainText,
        DocumentFormat::Html,
        DocumentFormat::Pdf,
        DocumentFormat::Docx,
        DocumentFormat::Xlsx,
        DocumentFormat::Pptx,
    ] {
        assert_eq!(
            DocumentFormat::from_filename(&format!("file.{}", format.extension())),
            Some(format)
        );
    }
}

#[test]
fn a_format_round_trips_through_json() {
    for format in [
        DocumentFormat::Markdown,
        DocumentFormat::PlainText,
        DocumentFormat::Html,
        DocumentFormat::Code,
        DocumentFormat::Pdf,
        DocumentFormat::Docx,
        DocumentFormat::Xlsx,
        DocumentFormat::Pptx,
        DocumentFormat::Unknown,
    ] {
        let wire = serde_json::to_string(&format).unwrap();
        assert_eq!(
            serde_json::from_str::<DocumentFormat>(&wire).unwrap(),
            format
        );
    }
}

#[test]
fn display_matches_the_wire_spelling() {
    assert_eq!(DocumentFormat::PlainText.to_string(), "plain_text");
    assert_eq!(DocumentFormat::Html.to_string(), "html");
}

#[test]
fn invalid_utf8_without_magic_bytes_is_unknown_not_text() {
    assert_eq!(
        DocumentFormat::sniff(&[0xFF, 0xFE, 0xFD], None, None),
        DocumentFormat::Unknown
    );
}

#[test]
fn source_files_are_detected_as_code_by_extension_and_name() {
    for filename in [
        "src/main.rs",
        "app.py",
        "web/App.tsx",
        "Dockerfile",
        "Makefile",
    ] {
        assert_eq!(
            DocumentFormat::from_filename(filename),
            Some(DocumentFormat::Code),
            "{filename}"
        );
    }
}

#[test]
fn html_stays_html_and_cmake_lists_is_code_not_text() {
    assert_eq!(
        DocumentFormat::from_filename("index.html"),
        Some(DocumentFormat::Html)
    );
    assert_eq!(
        DocumentFormat::from_filename("CMakeLists.txt"),
        Some(DocumentFormat::Code)
    );
}

#[test]
fn an_unlabelled_upload_named_like_code_sniffs_as_code() {
    assert_eq!(
        DocumentFormat::sniff(b"fn main() {}", Some("main.rs"), None),
        DocumentFormat::Code
    );
    assert_eq!(
        DocumentFormat::sniff(
            b"fn main() {}",
            Some("main.rs"),
            Some("application/octet-stream")
        ),
        DocumentFormat::Code
    );
}

#[test]
fn code_displays_as_code() {
    assert_eq!(DocumentFormat::Code.to_string(), "code");
    assert_eq!(DocumentFormat::Code.mime(), "text/x-source");
}

/// A zip archive holding `entries`, each with a few bytes of content — the
/// shape of an OOXML package as far as sniffing is concerned.
fn archive(entries: &[&str]) -> Vec<u8> {
    use std::io::Write;

    let mut buffer = std::io::Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(&mut buffer);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for entry in entries {
        writer.start_file(*entry, options).unwrap();
        writer.write_all(b"<x/>").unwrap();
    }
    writer.finish().unwrap();
    buffer.into_inner()
}

#[test]
fn the_office_mime_types_map_to_their_formats() {
    assert_eq!(
        DocumentFormat::from_mime(
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        ),
        Some(DocumentFormat::Xlsx)
    );
    assert_eq!(
        DocumentFormat::from_mime("application/vnd.ms-excel.sheet.macroEnabled.12"),
        Some(DocumentFormat::Xlsx)
    );
    assert_eq!(
        DocumentFormat::from_mime(
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        ),
        Some(DocumentFormat::Pptx)
    );
}

#[test]
fn legacy_binary_office_formats_are_not_claimed_as_open_xml() {
    // `.xls` and `.ppt` are the pre-2007 binary formats; the Open XML readers
    // cannot open them, so claiming them would hand an extractor input it
    // cannot read — the same rule `.doc` follows.
    assert_eq!(DocumentFormat::from_filename("budget.xls"), None);
    assert_eq!(DocumentFormat::from_filename("deck.ppt"), None);
    assert_eq!(DocumentFormat::from_mime("application/vnd.ms-excel"), None);
    assert_eq!(
        DocumentFormat::from_mime("application/vnd.ms-powerpoint"),
        None
    );
}

#[test]
fn an_unlabelled_workbook_is_told_apart_by_its_parts() {
    // A browser that cannot place the file sends octet-stream and, from a
    // folder drop, sometimes no usable name: the archive's own part names are
    // the only evidence left, and they cannot be wrong.
    let workbook = archive(&[
        "[Content_Types].xml",
        "xl/workbook.xml",
        "xl/worksheets/sheet1.xml",
    ]);
    assert_eq!(
        DocumentFormat::sniff(&workbook, None, Some("application/octet-stream")),
        DocumentFormat::Xlsx
    );
}

#[test]
fn an_unlabelled_deck_is_told_apart_by_its_parts() {
    let deck = archive(&[
        "[Content_Types].xml",
        "ppt/presentation.xml",
        "ppt/slides/slide1.xml",
    ]);
    assert_eq!(
        DocumentFormat::sniff(&deck, None, None),
        DocumentFormat::Pptx
    );
}

#[test]
fn an_unlabelled_word_document_is_told_apart_by_its_parts() {
    let document = archive(&["[Content_Types].xml", "word/document.xml"]);
    assert_eq!(
        DocumentFormat::sniff(&document, None, None),
        DocumentFormat::Docx
    );
}

#[test]
fn the_archive_parts_beat_a_wrong_filename() {
    // Parts are content, and content outranks a label the same way `%PDF-`
    // does: a workbook saved as `report.docx` is still a workbook.
    let workbook = archive(&["xl/workbook.xml"]);
    assert_eq!(
        DocumentFormat::sniff(&workbook, Some("report.docx"), None),
        DocumentFormat::Xlsx
    );
}

#[test]
fn a_zip_without_office_parts_defers_to_the_labels() {
    // A truncated or unusual package whose parts say nothing still has its
    // declared type and name to go on before the container fallback.
    let opaque = archive(&["data.bin"]);
    assert_eq!(
        DocumentFormat::sniff(&opaque, Some("q3.pptx"), None),
        DocumentFormat::Pptx
    );
    assert_eq!(
        DocumentFormat::sniff(
            &opaque,
            None,
            Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet")
        ),
        DocumentFormat::Xlsx
    );
}

#[test]
fn a_zip_label_that_is_not_office_does_not_override_the_container() {
    // `notes.md` on a zip is a wrong label, not evidence of a text file:
    // magic bytes still win, and the container fallback is what they say.
    let opaque = archive(&["data.bin"]);
    assert_eq!(
        DocumentFormat::sniff(&opaque, Some("notes.md"), Some("text/plain")),
        DocumentFormat::Docx
    );
}

#[test]
fn the_office_formats_have_their_own_wire_names() {
    assert_eq!(DocumentFormat::Xlsx.to_string(), "xlsx");
    assert_eq!(DocumentFormat::Pptx.to_string(), "pptx");
    assert_eq!(
        serde_json::to_string(&DocumentFormat::Xlsx).unwrap(),
        "\"xlsx\""
    );
}
