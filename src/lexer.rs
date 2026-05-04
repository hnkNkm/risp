//! Lexer for Risp.

use crate::ast::Span;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    LParen,
    RParen,
    LBracket,
    RBracket,
    Colon,
    Comma,
    Arrow, // ->
    /// Identifier or keyword. We don't pre-classify keywords here;
    /// the parser decides based on position.
    Ident(String),
    Int(i64, Option<IntSuffix>),
    Float(f64, Option<FloatSuffix>),
    Str(String),
    Bool(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntSuffix {
    I32,
    I64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatSuffix {
    F32,
    F64,
}

#[derive(Debug, Clone)]
pub struct Spanned {
    pub tok: Token,
    pub span: Span,
}

#[derive(Debug, Error)]
pub enum LexError {
    #[error("unexpected character {0:?} at byte {1}")]
    UnexpectedChar(char, usize),
    #[error("unterminated string starting at byte {0}")]
    UnterminatedString(usize),
    #[error("invalid escape sequence \\{0} at byte {1}")]
    InvalidEscape(char, usize),
    #[error("invalid number literal {0:?} at byte {1}")]
    InvalidNumber(String, usize),
}

pub fn lex(src: &str) -> Result<Vec<Spanned>, LexError> {
    let bytes = src.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();

    while i < bytes.len() {
        let c = bytes[i] as char;

        // whitespace
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }

        // line comment
        if c == ';' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }

        let start = i;

        match c {
            '(' => {
                out.push(Spanned { tok: Token::LParen, span: Span::new(start, start + 1) });
                i += 1;
            }
            ')' => {
                out.push(Spanned { tok: Token::RParen, span: Span::new(start, start + 1) });
                i += 1;
            }
            '[' => {
                out.push(Spanned { tok: Token::LBracket, span: Span::new(start, start + 1) });
                i += 1;
            }
            ']' => {
                out.push(Spanned { tok: Token::RBracket, span: Span::new(start, start + 1) });
                i += 1;
            }
            ':' => {
                out.push(Spanned { tok: Token::Colon, span: Span::new(start, start + 1) });
                i += 1;
            }
            ',' => {
                out.push(Spanned { tok: Token::Comma, span: Span::new(start, start + 1) });
                i += 1;
            }
            '"' => {
                let (s, end) = lex_string(bytes, i)?;
                out.push(Spanned { tok: Token::Str(s), span: Span::new(start, end) });
                i = end;
            }
            // '-' could be arrow, negative number, or identifier-start
            '-' if i + 1 < bytes.len() && bytes[i + 1] == b'>' => {
                out.push(Spanned { tok: Token::Arrow, span: Span::new(start, start + 2) });
                i += 2;
            }
            '-' if i + 1 < bytes.len() && (bytes[i + 1] as char).is_ascii_digit() => {
                let (tok, end) = lex_number(bytes, i)?;
                out.push(Spanned { tok, span: Span::new(start, end) });
                i = end;
            }
            c if c.is_ascii_digit() => {
                let (tok, end) = lex_number(bytes, i)?;
                out.push(Spanned { tok, span: Span::new(start, end) });
                i = end;
            }
            c if is_ident_start(c) => {
                let (tok, end) = lex_ident(bytes, i);
                out.push(Spanned { tok, span: Span::new(start, end) });
                i = end;
            }
            _ => return Err(LexError::UnexpectedChar(c, i)),
        }
    }

    Ok(out)
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic()
        || matches!(
            c,
            '_' | '+' | '-' | '*' | '/' | '<' | '>' | '=' | '!' | '?' | '%'
        )
}

fn is_ident_continue(c: char) -> bool {
    is_ident_start(c) || c.is_ascii_digit()
}

fn lex_ident(bytes: &[u8], start: usize) -> (Token, usize) {
    let mut i = start;
    while i < bytes.len() && is_ident_continue(bytes[i] as char) {
        i += 1;
    }
    let s = std::str::from_utf8(&bytes[start..i]).unwrap().to_string();
    let tok = match s.as_str() {
        "true" => Token::Bool(true),
        "false" => Token::Bool(false),
        _ => Token::Ident(s),
    };
    (tok, i)
}

fn lex_number(bytes: &[u8], start: usize) -> Result<(Token, usize), LexError> {
    let mut i = start;
    if bytes[i] == b'-' {
        i += 1;
    }
    while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
        i += 1;
    }
    let mut is_float = false;
    if i < bytes.len() && bytes[i] == b'.' && i + 1 < bytes.len() && (bytes[i + 1] as char).is_ascii_digit() {
        is_float = true;
        i += 1;
        while i < bytes.len() && (bytes[i] as char).is_ascii_digit() {
            i += 1;
        }
    }

    let num_end = i;
    // optional suffix: i32, i64, f32, f64
    let mut suffix_int: Option<IntSuffix> = None;
    let mut suffix_float: Option<FloatSuffix> = None;
    if i + 2 < bytes.len() + 1 && i + 3 <= bytes.len() {
        let s = std::str::from_utf8(&bytes[i..i + 3]).unwrap_or("");
        match s {
            "i32" => { suffix_int = Some(IntSuffix::I32); i += 3; }
            "i64" => { suffix_int = Some(IntSuffix::I64); i += 3; }
            "f32" => { suffix_float = Some(FloatSuffix::F32); i += 3; is_float = true; }
            "f64" => { suffix_float = Some(FloatSuffix::F64); i += 3; is_float = true; }
            _ => {}
        }
    }

    let raw = std::str::from_utf8(&bytes[start..num_end]).unwrap();

    if is_float {
        let v: f64 = raw.parse().map_err(|_| LexError::InvalidNumber(raw.to_string(), start))?;
        Ok((Token::Float(v, suffix_float), i))
    } else {
        let v: i64 = raw.parse().map_err(|_| LexError::InvalidNumber(raw.to_string(), start))?;
        Ok((Token::Int(v, suffix_int), i))
    }
}

fn lex_string(bytes: &[u8], start: usize) -> Result<(String, usize), LexError> {
    debug_assert_eq!(bytes[start], b'"');
    let mut i = start + 1;
    let mut out = String::new();
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Ok((out, i + 1)),
            b'\\' => {
                if i + 1 >= bytes.len() {
                    return Err(LexError::UnterminatedString(start));
                }
                let esc = bytes[i + 1] as char;
                let ch = match esc {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    '\\' => '\\',
                    '"' => '"',
                    '0' => '\0',
                    other => return Err(LexError::InvalidEscape(other, i)),
                };
                out.push(ch);
                i += 2;
            }
            b => {
                out.push(b as char);
                i += 1;
            }
        }
    }
    Err(LexError::UnterminatedString(start))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lex_basic() {
        let src = "(defn add [x: i32, y: i32] -> i32 (+ x y))";
        let toks = lex(src).unwrap();
        assert!(toks.len() > 5);
    }

    #[test]
    fn lex_string_with_escape() {
        let src = r#""hello\nworld""#;
        let toks = lex(src).unwrap();
        match &toks[0].tok {
            Token::Str(s) => assert_eq!(s, "hello\nworld"),
            _ => panic!("expected string"),
        }
    }

    #[test]
    fn lex_numbers() {
        let toks = lex("42 -7 3.14 2i64 1.5f32").unwrap();
        assert!(matches!(toks[0].tok, Token::Int(42, None)));
        assert!(matches!(toks[1].tok, Token::Int(-7, None)));
        assert!(matches!(toks[2].tok, Token::Float(_, None)));
        assert!(matches!(toks[3].tok, Token::Int(2, Some(IntSuffix::I64))));
        assert!(matches!(toks[4].tok, Token::Float(_, Some(FloatSuffix::F32))));
    }

    #[test]
    fn lex_comment() {
        let toks = lex("; this is a comment\n42").unwrap();
        assert_eq!(toks.len(), 1);
    }
}
