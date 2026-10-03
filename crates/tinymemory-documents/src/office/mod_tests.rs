//! Tests for the office converter: PDF, DOCX, PPTX and XLSX to markdown.
//!
//! Every fixture is built here rather than checked in, so each test says what
//! it is asserting about instead of pointing at an opaque binary.

use super::*;

use crate::convert::ConverterChain;

/// A deflated zip archive of `(path, contents)` parts.
fn package(parts: &[(&str, &str)]) -> Vec<u8> {
    use std::io::Write;

    let mut buffer = std::io::Cursor::new(Vec::new());
    let mut writer = zip::ZipWriter::new(&mut buffer);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (path, contents) in parts {
        writer.start_file(*path, options).unwrap();
        writer.write_all(contents.as_bytes()).unwrap();
    }
    writer.finish().unwrap();
    buffer.into_inner()
}

/// A Word document whose body is `paragraphs`, each a list of text runs.
fn docx(paragraphs: &[&[&str]]) -> Vec<u8> {
    let body: String = paragraphs
        .iter()
        .map(|runs| {
            let runs: String = runs
                .iter()
                .map(|run| format!("<w:r><w:t>{run}</w:t></w:r>"))
                .collect();
            format!("<w:p>{runs}</w:p>")
        })
        .collect();
    let document = format!(
        r#"<?xml version="1.0"?><w:document xmlns:w="x"><w:body>{body}</w:body></w:document>"#
    );
    package(&[("word/document.xml", &document)])
}

/// A deck holding one slide per `(slide number, text)` pair, written to the
/// archive in the order given.
fn pptx(slides: &[(u32, &str)]) -> Vec<u8> {
    let mut parts = vec![(
        "ppt/presentation.xml".to_string(),
        r#"<?xml version="1.0"?><p:presentation xmlns:p="x"/>"#.to_string(),
    )];
    for (number, text) in slides {
        parts.push((
            format!("ppt/slides/slide{number}.xml"),
            format!(
                r#"<?xml version="1.0"?><p:sld xmlns:p="x" xmlns:a="y"><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:sld>"#
            ),
        ));
    }
    let borrowed: Vec<(&str, &str)> = parts
        .iter()
        .map(|(path, contents)| (path.as_str(), contents.as_str()))
        .collect();
    package(&borrowed)
}

/// A minimal XLSX archive from a worksheet's `sheetData` fragment.
///
/// The parts are the smallest set calamine's Xlsx reader accepts: the
/// content-type map, the root and workbook relationships, and the workbook
/// itself. No shared strings or styles, which the reader tolerates.
fn xlsx_with_sheet(sheet_data: &str) -> Vec<u8> {
    let content_types = r#"<?xml version="1.0"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#;
    let root_rels = r#"<?xml version="1.0"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;
    let workbook = r#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#;
    let workbook_rels = r#"<?xml version="1.0"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#;
    let worksheet = format!(
        r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<sheetData>{sheet_data}</sheetData>
</worksheet>"#
    );
    package(&[
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", root_rels),
        ("xl/workbook.xml", workbook),
        ("xl/_rels/workbook.xml.rels", workbook_rels),
        ("xl/worksheets/sheet1.xml", &worksheet),
    ])
}

/// A one-page PDF whose content stream is `content`, with a correct
/// cross-reference table so the parser takes the normal path rather than a
/// recovery one.
fn pdf(content: &str) -> Vec<u8> {
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R \
         /Resources << /Font << /F1 5 0 R >> >> >>"
            .to_string(),
        format!(
            "<< /Length {} >>\nstream\n{content}\nendstream",
            content.len()
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];
    let mut out = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.push_str(&format!("{} 0 obj\n{object}\nendobj\n", index + 1));
    }
    let xref = out.len();
    out.push_str(&format!(
        "xref\n0 {}\n0000000000 65535 f \n",
        objects.len() + 1
    ));
    for offset in offsets {
        out.push_str(&format!("{offset:010} 00000 n \n"));
    }
    out.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    ));
    out.into_bytes()
}

/// Run a buffer through the converter as an unlabelled upload named `name`.
async fn convert(name: &str, bytes: Vec<u8>) -> Result<ConvertedDocument> {
    OfficeConverter
        .convert(&RawDocument::new(bytes).with_filename(name))
        .await
}

/// The message of a conversion that was expected to fail.
async fn refusal(name: &str, bytes: Vec<u8>) -> String {
    match convert(name, bytes).await {
        Ok(converted) => panic!("expected a refusal, got {:?}", converted.markdown),
        Err(error) => error.to_string(),
    }
}

#[test]
fn it_claims_the_four_office_formats_and_nothing_else() {
    for format in [
        DocumentFormat::Pdf,
        DocumentFormat::Docx,
        DocumentFormat::Xlsx,
        DocumentFormat::Pptx,
    ] {
        assert!(OfficeConverter.supports(format), "{format}");
    }
    for format in [
        DocumentFormat::Markdown,
        DocumentFormat::PlainText,
        DocumentFormat::Html,
        DocumentFormat::Unknown,
    ] {
        assert!(!OfficeConverter.supports(format), "{format}");
    }
}

#[tokio::test]
async fn a_docx_yields_its_paragraphs_in_order() {
    let bytes = docx(&[&["First heading"], &["Second ", "paragraph."]]);
    let converted = convert("spec.docx", bytes).await.unwrap();
    let text = &converted.markdown;
    assert!(text.starts_with("First heading"), "{text}");
    assert!(
        text.contains("Second paragraph."),
        "runs inside one paragraph join without a break: {text}"
    );
    assert!(
        text.contains("First heading\n\nSecond"),
        "paragraphs keep their break: {text:?}"
    );
    assert_eq!(converted.format, DocumentFormat::Docx);
}

#[tokio::test]
async fn escaped_characters_in_a_run_survive_extraction() {
    // XML escapes `&` and `<` in text, and the parser reports each escape as
    // its own event: dropping those events would turn "Q&A" into "QA".
    let bytes = docx(&[&["Q&amp;A: 3 &lt; 4 &#38; &quot;done&quot;"]]);
    let converted = convert("faq.docx", bytes).await.unwrap();
    assert_eq!(converted.markdown, "Q&A: 3 < 4 & \"done\"");
}

#[tokio::test]
async fn a_deck_reads_its_slides_in_numeric_order() {
    // `slide10.xml` sorts before `slide2.xml` as a string, and a deck that
    // recalls out of order is worse than one that does not recall at all.
    let bytes = pptx(&[(10, "Tenth"), (2, "Second"), (1, "First")]);
    let converted = convert("deck.pptx", bytes).await.unwrap();
    assert_eq!(converted.markdown, "First\n\nSecond\n\nTenth");
    assert_eq!(converted.format, DocumentFormat::Pptx);
}

#[tokio::test]
async fn a_small_spreadsheet_yields_one_line_per_row() {
    // The dense-range guard must not refuse ordinary files, and the concrete
    // (non-auto) Xlsx open must not either.
    let bytes = xlsx_with_sheet(
        r#"<row r="1"><c r="A1"><v>1</v></c><c r="B1"><v>alpha</v></c></row>
           <row r="2"><c r="A2"><v>2</v></c><c r="B2"><v>beta</v></c></row>"#,
    );
    let converted = convert("ledger.xlsx", bytes).await.unwrap();
    assert_eq!(converted.markdown, "Sheet1 | 1 | alpha\nSheet1 | 2 | beta");
    assert_eq!(converted.format, DocumentFormat::Xlsx);
}

#[tokio::test]
async fn a_spreadsheet_with_far_cells_is_refused_not_allocated() {
    // One cell at `A1` and one at `XFD1048576` passes the decompression cap
    // (the archive is a few hundred bytes) yet would make calamine
    // materialize a ~17-billion-cell dense grid. Refused before that.
    let bytes = xlsx_with_sheet(
        r#"<row r="1"><c r="A1"><v>1</v></c></row>
           <row r="1048576"><c r="XFD1048576"><v>2</v></c></row>"#,
    );
    let reason = refusal("spread.xlsx", bytes).await;
    assert!(reason.contains("used range"), "{reason}");
}

#[tokio::test]
async fn an_overexpanding_document_is_refused_not_allocated() {
    // One entry of zero bytes just over the cap: zeroes compress to almost
    // nothing, so the archive is tiny while its declared expansion exceeds
    // the limit — exactly the shape of a crafted bomb.
    let zeroes = "\0".repeat(MAX_DECOMPRESSED_BYTES as usize + 1);
    let bytes = package(&[("word/document.xml", &zeroes)]);
    assert!(
        bytes.len() < MAX_DOCUMENT_BYTES,
        "the fixture must pass intake's own cap"
    );
    let reason = refusal("bomb.docx", bytes).await;
    assert!(reason.contains("expands"), "{reason}");
}

#[tokio::test]
async fn an_overexpanding_spreadsheet_is_refused_before_calamine_opens_it() {
    let zeroes = "\0".repeat(MAX_DECOMPRESSED_BYTES as usize + 1);
    let bytes = package(&[("xl/workbook.xml", &zeroes)]);
    let reason = refusal("bomb.xlsx", bytes).await;
    assert!(reason.contains("expands"), "{reason}");
}

#[tokio::test]
async fn a_pdf_yields_its_text_layer() {
    let bytes = pdf("BT /F1 24 Tf 72 720 Td (Quarterly revenue rose) Tj ET");
    let converted = convert("report.pdf", bytes).await.unwrap();
    assert!(
        converted.markdown.contains("Quarterly revenue rose"),
        "{:?}",
        converted.markdown
    );
    assert_eq!(converted.format, DocumentFormat::Pdf);
}

#[tokio::test]
async fn a_pdf_without_a_text_layer_says_so_rather_than_storing_nothing() {
    // A scanned PDF carries pictures of words. That is not a parse failure,
    // but storing an empty body would lose the upload while looking like one
    // succeeded.
    let reason = refusal("scan.pdf", pdf("")).await;
    assert!(reason.contains("no text"), "{reason}");
}

#[tokio::test]
async fn a_malformed_pdf_is_an_error_not_a_crash() {
    let reason = refusal("broken.pdf", b"%PDF-1.7\nthis is not a pdf".to_vec()).await;
    assert!(reason.contains("PDF"), "{reason}");
}

#[tokio::test]
async fn a_docx_that_is_not_an_archive_is_an_error() {
    let error = OfficeConverter
        .convert(&RawDocument::new(b"plain words".to_vec()).with_mime(DocumentFormat::Docx.mime()))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Invalid(_)), "{error:?}");
}

#[tokio::test]
async fn a_document_with_no_text_is_an_error_not_an_empty_document() {
    let reason = refusal("blank.docx", docx(&[&[" "]])).await;
    assert!(reason.contains("produced no text"), "{reason}");
}

#[tokio::test]
async fn a_format_it_does_not_claim_is_refused_by_name() {
    let error = OfficeConverter
        .convert(&RawDocument::new("# notes").with_filename("notes.md"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("markdown"), "{error}");
}

#[tokio::test]
async fn the_size_cap_applies_before_any_parsing() {
    let error = OfficeConverter
        .convert(&RawDocument::new(Vec::new()).with_filename("empty.pdf"))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Invalid(_)), "{error:?}");
}

#[tokio::test]
async fn the_converter_records_its_own_name_in_metadata() {
    let converted = convert("spec.docx", docx(&[&["Body"]])).await.unwrap();
    assert_eq!(converted.metadata["converter"], "office");
}

#[test]
fn the_blocking_entry_point_converts_without_a_runtime() {
    // A host moves the CPU-bound parse onto its own blocking pool; this is
    // the call it makes there.
    let document = RawDocument::new(docx(&[&["Off the executor"]])).with_filename("a.docx");
    let converted = OfficeConverter.convert_blocking(&document).unwrap();
    assert_eq!(converted.markdown, "Off the executor");
}

#[tokio::test]
async fn prepended_to_the_default_chain_it_covers_every_format() {
    let chain = ConverterChain::default().prepend(Box::new(OfficeConverter));
    assert_eq!(
        chain.supported_formats(),
        vec![
            DocumentFormat::Markdown,
            DocumentFormat::PlainText,
            DocumentFormat::Html,
            DocumentFormat::Code,
            DocumentFormat::Pdf,
            DocumentFormat::Docx,
            DocumentFormat::Xlsx,
            DocumentFormat::Pptx,
        ]
    );
    // An unlabelled workbook routes by its parts to the office converter,
    // and markdown still goes to the native one behind it.
    let workbook = xlsx_with_sheet(r#"<row r="1"><c r="A1"><v>7</v></c></row>"#);
    let converted = chain.convert(&RawDocument::new(workbook)).await.unwrap();
    assert_eq!(converted.markdown, "Sheet1 | 7");
    let notes = chain
        .convert(&RawDocument::new("# Notes").with_filename("notes.md"))
        .await
        .unwrap();
    assert_eq!(notes.metadata["converter"], "native");
}
