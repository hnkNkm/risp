//! Source-aware error rendering.
//!
//! Given a byte offset (or span) into the source string, prints a rustc-style
//! diagnostic with file:line:col and a caret pointing at the offending span.

use crate::ast::Span;

/// A lightweight "where did this go wrong" pointer. Errors throughout the
/// compiler convert into this to be rendered uniformly.
#[derive(Debug, Clone, Copy)]
pub struct Loc {
    pub start: usize,
    pub end: usize,
}

impl Loc {
    pub fn point(byte: usize) -> Self {
        Self { start: byte, end: byte + 1 }
    }
    pub fn from_span(s: Span) -> Self {
        Self { start: s.start, end: s.end.max(s.start + 1) }
    }
}

/// Convert a byte offset into (line_idx_1based, col_idx_1based, line_text).
fn locate(src: &str, byte: usize) -> (usize, usize, &str) {
    let byte = byte.min(src.len());
    let mut line_start = 0;
    let mut line_no = 1;
    for (i, b) in src.bytes().enumerate() {
        if i == byte {
            break;
        }
        if b == b'\n' {
            line_no += 1;
            line_start = i + 1;
        }
    }
    let line_end = src[line_start..]
        .find('\n')
        .map(|p| line_start + p)
        .unwrap_or(src.len());
    let col = byte.saturating_sub(line_start) + 1;
    (line_no, col, &src[line_start..line_end])
}

/// Render a diagnostic to a String. `file` is shown in the header, `src`
/// is the full source text for line lookup.
pub fn render(file: &str, src: &str, loc: Loc, message: &str) -> String {
    let (line, col, line_text) = locate(src, loc.start);
    let gutter_w = line.to_string().len();
    let pad = " ".repeat(gutter_w);

    // Caret length: span width on the first line, clamped sanely.
    let span_len = loc.end.saturating_sub(loc.start).max(1);
    // Column is 1-based, byte-indexed. Multibyte chars would need width
    // accounting; for now assume ASCII source (which Risp's grammar enforces
    // outside string literals).
    let caret_off = col.saturating_sub(1);
    let caret = format!("{}{}", " ".repeat(caret_off), "^".repeat(span_len));

    format!(
        "error: {msg}\n\
         {pad}--> {file}:{line}:{col}\n\
         {pad} |\n\
         {line:>w$} | {line_text}\n\
         {pad} | {caret}\n",
        msg = message,
        pad = pad,
        file = file,
        line = line,
        col = col,
        line_text = line_text,
        caret = caret,
        w = gutter_w,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_caret_at_correct_column() {
        let src = "(defn main [] -> i32\n  (+ x 1))\n";
        // byte offset of 'x'
        let off = src.find('x').unwrap();
        let out = render("t.rsp", src, Loc::point(off), "undefined variable \"x\"");
        assert!(out.contains("t.rsp:2:6"), "{out}");
        assert!(out.contains("(+ x 1))"), "{out}");
        assert!(out.contains("^"), "{out}");
    }

    #[test]
    fn span_widens_caret() {
        let src = "abc def";
        let loc = Loc { start: 4, end: 7 };
        let out = render("x", src, loc, "bad");
        assert!(out.contains("^^^"), "{out}");
    }
}
