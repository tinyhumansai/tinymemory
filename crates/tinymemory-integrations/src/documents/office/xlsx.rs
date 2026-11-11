//! Spreadsheets, flattened to one `sheet | cell | cell` line per row.

use std::io::Cursor;

use calamine::{Data, Reader, Xlsx};

use super::{MAX_SPREADSHEET_DENSE_CELLS, ooxml, unreadable};
use crate::documents::error::Result;

/// Extracts every non-empty row of every sheet, in sheet order.
pub(super) fn extract(bytes: &[u8]) -> Result<String> {
    // The zip-bomb guard runs before calamine materializes anything.
    drop(ooxml::open(bytes, "spreadsheet")?);

    // Opened as a concrete `Xlsx` rather than auto-detected: the other formats
    // calamine's auto-open falls back to (`.xls`, `.xlsb`, `.ods`) build their
    // dense ranges during open — before the extent guard below could run — so
    // accepting a mislabelled file would reopen the same allocation attack.
    let mut workbook: Xlsx<_> = calamine::open_workbook_from_rs(Cursor::new(bytes))
        .map_err(|error| unreadable(format!("the spreadsheet could not be read: {error}")))?;
    let mut out = String::new();
    for name in workbook.sheet_names() {
        match dense_cells(&mut workbook, &name) {
            None => continue,
            Some(cells) if cells > MAX_SPREADSHEET_DENSE_CELLS => {
                return Err(unreadable(
                    "the spreadsheet's used range exceeds the size this build can read safely"
                        .to_string(),
                ));
            }
            Some(_) => {}
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

/// The number of cells in the bounding box of a sheet's actual cells — what
/// `worksheet_range` would allocate — found by a sparse scan that allocates
/// no grid. `None` for a sheet that cannot be read or has no cells.
fn dense_cells(workbook: &mut Xlsx<Cursor<&[u8]>>, sheet: &str) -> Option<usize> {
    let mut reader = workbook.worksheet_cells_reader(sheet).ok()?;
    let (mut row_min, mut row_max) = (u32::MAX, 0);
    let (mut col_min, mut col_max) = (u32::MAX, 0);
    while let Ok(Some(cell)) = reader.next_cell() {
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
    Some(usize::try_from(rows.saturating_mul(cols)).unwrap_or(usize::MAX))
}
