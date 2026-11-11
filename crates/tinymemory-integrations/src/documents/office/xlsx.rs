//! Spreadsheets, flattened to one `sheet | cell | cell` line per row.

use std::io::{BufReader, Cursor, Read};

use calamine::{Data, DataRef, Reader, Xlsx};
use quick_xml::events::Event;
use zip::ZipArchive;

use super::{MAX_SPREADSHEET_DENSE_CELLS, MAX_SPREADSHEET_TEXT_BYTES, ooxml, unreadable};
use crate::documents::error::Result;

/// Most entries a shared-string table may declare. Equal to
/// [`MAX_SPREADSHEET_DENSE_CELLS`]: a sheet that can hold at most that many
/// cells has no use for more distinct strings.
const MAX_SHARED_STRINGS: u64 = 1_000_000;

/// Extracts every non-empty row of every sheet, in sheet order.
pub(super) fn extract(bytes: &[u8]) -> Result<String> {
    // The zip-bomb guard and the shared-string check run before calamine
    // materializes anything.
    let mut archive = ooxml::open(bytes, "spreadsheet")?;
    check_shared_strings(&mut archive)?;
    drop(archive);

    // Opened as a concrete `Xlsx` rather than auto-detected: the other formats
    // calamine's auto-open falls back to (`.xls`, `.xlsb`, `.ods`) build their
    // dense ranges during open — before the extent guard below could run — so
    // accepting a mislabelled file would reopen the same allocation attack.
    let mut workbook: Xlsx<_> = calamine::open_workbook_from_rs(Cursor::new(bytes))
        .map_err(|error| unreadable(format!("the spreadsheet could not be read: {error}")))?;
    let mut out = String::new();
    // Text materialized so far, across every sheet: `worksheet_range` gives
    // each shared-string reference its own copy, so the cell bound alone does
    // not bound memory.
    let mut text = 0u64;
    for name in workbook.sheet_names() {
        let Some(extent) = scan(&mut workbook, &name) else {
            continue;
        };
        if extent.cells > MAX_SPREADSHEET_DENSE_CELLS {
            return Err(unreadable(
                "the spreadsheet's used range exceeds the size this build can read safely"
                    .to_string(),
            ));
        }
        text = text.saturating_add(extent.text);
        if text > MAX_SPREADSHEET_TEXT_BYTES {
            return Err(unreadable(
                "the spreadsheet's cell text exceeds the size this build can read safely"
                    .to_string(),
            ));
        }
        let Ok(range) = workbook.worksheet_range(&name) else {
            continue;
        };
        for row in range.rows() {
            let cells: Vec<String> = row
                .iter()
                .map(|cell| match cell {
                    Data::Empty => String::new(),
                    other => other.to_string(),
                })
                .collect();
            if cells.iter().all(|cell| cell.trim().is_empty()) {
                continue;
            }
            out.push_str(&name);
            for cell in cells {
                out.push_str(" | ");
                out.push_str(cell.trim());
            }
            out.push('\n');
        }
    }
    Ok(out)
}

/// What `worksheet_range` would allocate for a sheet.
struct Extent {
    /// Cells in the bounding box of the sheet's actual cells.
    cells: usize,
    /// Bytes of cell text, counting every reference to a shared string.
    text: u64,
}

/// A sheet's [`Extent`], found by a sparse scan that allocates no grid and
/// copies no text (shared strings are borrowed). `None` for a sheet that
/// cannot be read or has no cells.
fn scan(workbook: &mut Xlsx<Cursor<&[u8]>>, sheet: &str) -> Option<Extent> {
    let mut reader = workbook.worksheet_cells_reader(sheet).ok()?;
    let (mut row_min, mut row_max) = (u32::MAX, 0);
    let (mut col_min, mut col_max) = (u32::MAX, 0);
    let mut text = 0u64;
    while let Ok(Some(cell)) = reader.next_cell() {
        text = text.saturating_add(text_bytes(cell.get_value()));
        let (row, col) = cell.get_position();
        row_min = row_min.min(row);
        row_max = row_max.max(row);
        col_min = col_min.min(col);
        col_max = col_max.max(col);
    }
    if row_min == u32::MAX {
        return None;
    }
    let rows = u64::from(row_max - row_min) + 1;
    let cols = u64::from(col_max - col_min) + 1;
    Some(Extent {
        cells: usize::try_from(rows.saturating_mul(cols)).unwrap_or(usize::MAX),
        text,
    })
}

/// The text a cell becomes: its own string, or a number's or date's short
/// rendering.
fn text_bytes(value: &DataRef<'_>) -> u64 {
    let bytes = match value {
        DataRef::String(s) | DataRef::DateTimeIso(s) | DataRef::DurationIso(s) => s.len(),
        DataRef::SharedString(s) => s.len(),
        DataRef::Empty => 0,
        _ => 32,
    };
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

/// Refuses a workbook whose shared-string table declares more entries than
/// [`MAX_SHARED_STRINGS`]. calamine reserves `sst/@uniqueCount` entries up
/// front while it opens the workbook, so an absurd count would abort the
/// process before any later guard ran. Every entry named `sharedStrings.xml`
/// is checked, wherever the workbook's relationships place it.
fn check_shared_strings(archive: &mut ZipArchive<Cursor<&[u8]>>) -> Result<()> {
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|error| unreadable(format!("the spreadsheet could not be read: {error}")))?;
        let name = entry
            .name()
            .map_err(|error| unreadable(format!("the spreadsheet could not be read: {error}")))?;
        if !name.to_ascii_lowercase().ends_with("sharedstrings.xml") {
            continue;
        }
        if declared_unique_count(entry).is_some_and(|count| count > MAX_SHARED_STRINGS) {
            return Err(unreadable(
                "the spreadsheet's shared-string table declares more entries than this build can \
                 read safely"
                    .to_string(),
            ));
        }
    }
    Ok(())
}

/// The `uniqueCount` an `<sst>` element declares. A value that is present but
/// not a plain decimal that fits is reported as `u64::MAX`, so it is refused
/// rather than trusted; `None` when the attribute or element is absent.
fn declared_unique_count(part: impl Read) -> Option<u64> {
    let mut reader = quick_xml::Reader::from_reader(BufReader::new(part));
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e) | Event::Empty(e)) if e.local_name().as_ref() == "sst" => {
                let attribute = e
                    .attributes()
                    .flatten()
                    .find(|a| a.key.local_name().as_ref() == "uniqueCount")?;
                let value = attribute.value;
                let plain = !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit());
                return Some(
                    Some(value.as_ref())
                        .filter(|_| plain)
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(u64::MAX),
                );
            }
            Ok(Event::Eof) | Err(_) => return None,
            Ok(_) => buf.clear(),
        }
    }
}
