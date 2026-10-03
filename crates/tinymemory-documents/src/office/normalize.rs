//! Whitespace normalization for extracted text.

/// Collapses runs of blank lines to one, interior whitespace runs to a single
/// space, and trims every line's end and the whole text.
///
/// PDF extraction pads columns with dozens of spaces and OOXML paragraph
/// breaks stack up around empty paragraphs; neither carries meaning, and both
/// would cost a chunker and an embedder for nothing.
pub(super) fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0;
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run == 1 {
                out.push('\n');
            }
            continue;
        }
        blank_run = 0;
        let mut last_space = false;
        for ch in line.chars() {
            if ch.is_whitespace() {
                if !last_space {
                    out.push(' ');
                }
                last_space = true;
            } else {
                out.push(ch);
                last_space = false;
            }
        }
        out.push('\n');
    }
    out.trim().to_string()
}
