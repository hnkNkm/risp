//! Parser: tokens -> AST.

use crate::ast::*;
use crate::lexer::{FloatSuffix, IntSuffix, Spanned, Token};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("unexpected end of input")]
    UnexpectedEof,
    #[error("unexpected token {0}")]
    Unexpected(String, usize),
    #[error("unknown type {0:?}")]
    UnknownType(String, usize),
    #[error("expected {expected}, found {found}")]
    Expected {
        expected: &'static str,
        found: String,
        at: usize,
    },
}

impl ParseError {
    /// Best-effort byte offset for diagnostics. `None` for EOF errors,
    /// where the caller should fall back to "end of file".
    pub fn byte(&self) -> Option<usize> {
        match self {
            ParseError::UnexpectedEof => None,
            ParseError::Unexpected(_, b)
            | ParseError::UnknownType(_, b)
            | ParseError::Expected { at: b, .. } => Some(*b),
        }
    }
}

pub struct Parser<'a> {
    toks: &'a [Spanned],
    pos: usize,
}

impl<'a> Parser<'a> {
    pub fn new(toks: &'a [Spanned]) -> Self {
        Self { toks, pos: 0 }
    }

    fn peek(&self) -> Option<&Spanned> {
        self.toks.get(self.pos)
    }

    fn bump(&mut self) -> Option<&Spanned> {
        let t = self.toks.get(self.pos);
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn eat(&mut self, expected: &Token) -> Result<&Spanned, ParseError> {
        let pos_at = self.peek().map(|s| s.span.start).unwrap_or(0);
        let tok = self.peek().ok_or(ParseError::UnexpectedEof)?;
        if std::mem::discriminant(&tok.tok) == std::mem::discriminant(expected) {
            Ok(self.bump().unwrap())
        } else {
            Err(ParseError::Expected {
                expected: token_name(expected),
                found: format!("{:?}", tok.tok),
                at: pos_at,
            })
        }
    }

    pub fn parse_program(&mut self) -> Result<Program, ParseError> {
        let mut items = Vec::new();
        while self.peek().is_some() {
            items.push(self.parse_toplevel()?);
        }
        Ok(Program { items })
    }

    fn parse_toplevel(&mut self) -> Result<TopLevel, ParseError> {
        let lparen = self.eat(&Token::LParen)?;
        let start = lparen.span.start;
        let head = self.bump().ok_or(ParseError::UnexpectedEof)?;
        let head_name = match &head.tok {
            Token::Ident(s) => s.clone(),
            other => {
                return Err(ParseError::Unexpected(format!("{:?}", other), head.span.start));
            }
        };
        match head_name.as_str() {
            "defn" => {
                let f = self.parse_defn_body(start)?;
                Ok(TopLevel::Function(f))
            }
            "def" => {
                let c = self.parse_def_body(start)?;
                Ok(TopLevel::Const(c))
            }
            other => Err(ParseError::Unexpected(
                format!("top-level form {:?}", other),
                head.span.start,
            )),
        }
    }

    /// `defn name [params] -> ret body)`  -- LParen + "defn" already consumed
    fn parse_defn_body(&mut self, start: usize) -> Result<Function, ParseError> {
        let name_tok = self.bump().ok_or(ParseError::UnexpectedEof)?;
        let name = match &name_tok.tok {
            Token::Ident(s) => s.clone(),
            other => return Err(ParseError::Unexpected(format!("{:?}", other), name_tok.span.start)),
        };

        // parameter list `[ ... ]`
        self.eat(&Token::LBracket)?;
        let mut params = Vec::new();
        loop {
            if matches!(self.peek().map(|s| &s.tok), Some(Token::RBracket)) {
                break;
            }
            let p = self.parse_param()?;
            params.push(p);
            // optional comma
            if matches!(self.peek().map(|s| &s.tok), Some(Token::Comma)) {
                self.bump();
            }
        }
        self.eat(&Token::RBracket)?;

        // -> ret_ty
        self.eat(&Token::Arrow)?;
        let ret = self.parse_type()?;

        // body
        let body = self.parse_expr()?;

        let rp = self.eat(&Token::RParen)?;
        Ok(Function {
            name,
            params,
            ret,
            body,
            span: Span::new(start, rp.span.end),
        })
    }

    /// `def name: ty value)` -- LParen + "def" already consumed
    fn parse_def_body(&mut self, start: usize) -> Result<Const, ParseError> {
        let name_tok = self.bump().ok_or(ParseError::UnexpectedEof)?;
        let name = match &name_tok.tok {
            Token::Ident(s) => s.clone(),
            other => return Err(ParseError::Unexpected(format!("{:?}", other), name_tok.span.start)),
        };
        self.eat(&Token::Colon)?;
        let ty = self.parse_type()?;
        let value = self.parse_expr()?;
        let rp = self.eat(&Token::RParen)?;
        Ok(Const {
            name,
            ty,
            value,
            span: Span::new(start, rp.span.end),
        })
    }

    fn parse_param(&mut self) -> Result<Param, ParseError> {
        let name_tok = self.bump().ok_or(ParseError::UnexpectedEof)?;
        let start = name_tok.span.start;
        let name = match &name_tok.tok {
            Token::Ident(s) => s.clone(),
            other => return Err(ParseError::Unexpected(format!("{:?}", other), start)),
        };
        self.eat(&Token::Colon)?;
        let ty = self.parse_type()?;
        let end = self.toks.get(self.pos.saturating_sub(1)).map(|s| s.span.end).unwrap_or(start);
        Ok(Param { name, ty, span: Span::new(start, end) })
    }

    fn parse_type(&mut self) -> Result<Type, ParseError> {
        let t = self.peek().ok_or(ParseError::UnexpectedEof)?;
        match &t.tok {
            Token::Ident(_) => {
                let t = self.bump().unwrap();
                match &t.tok {
                    Token::Ident(s) => match s.as_str() {
                        "i32" => Ok(Type::I32),
                        "i64" => Ok(Type::I64),
                        "f32" => Ok(Type::F32),
                        "f64" => Ok(Type::F64),
                        "bool" => Ok(Type::Bool),
                        "str" => Ok(Type::Str),
                        "unit" => Ok(Type::Unit),
                        other => Err(ParseError::UnknownType(other.to_string(), t.span.start)),
                    },
                    _ => unreachable!(),
                }
            }
            Token::LParen => {
                let lp = self.bump().unwrap();
                let start = lp.span.start;
                let head = self.bump().ok_or(ParseError::UnexpectedEof)?;
                let head_name = match &head.tok {
                    Token::Ident(s) => s.as_str(),
                    other => {
                        return Err(ParseError::Unexpected(format!("{:?}", other), head.span.start));
                    }
                };
                if head_name != "Array" {
                    return Err(ParseError::UnknownType(head_name.to_string(), head.span.start));
                }
                let elem = self.parse_type()?;
                let len_tok = self.bump().ok_or(ParseError::UnexpectedEof)?;
                let len = match &len_tok.tok {
                    Token::Int(v, _) if *v >= 0 && *v <= u32::MAX as i64 => *v as u32,
                    Token::Int(..) => {
                        return Err(ParseError::Expected {
                            expected: "non-negative array length fitting u32",
                            found: format!("{:?}", len_tok.tok),
                            at: len_tok.span.start,
                        });
                    }
                    other => {
                        return Err(ParseError::Expected {
                            expected: "array length (int literal)",
                            found: format!("{:?}", other),
                            at: len_tok.span.start,
                        });
                    }
                };
                self.eat(&Token::RParen)?;
                let _ = start;
                Ok(Type::Array {
                    elem: Box::new(elem),
                    len,
                })
            }
            other => Err(ParseError::Unexpected(format!("{:?}", other), t.span.start)),
        }
    }

    fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        let t = self.peek().ok_or(ParseError::UnexpectedEof)?;
        let start = t.span.start;
        match &t.tok {
            Token::Int(v, suf) => {
                let v = *v;
                let kind = suf.map(|s| match s {
                    IntSuffix::I32 => IntKind::I32,
                    IntSuffix::I64 => IntKind::I64,
                });
                let span = Span::new(start, t.span.end);
                self.bump();
                Ok(Expr::new(ExprKind::Lit(Lit::Int(v, kind)), span))
            }
            Token::Float(v, suf) => {
                let v = *v;
                let kind = suf.map(|s| match s {
                    FloatSuffix::F32 => FloatKind::F32,
                    FloatSuffix::F64 => FloatKind::F64,
                });
                let span = Span::new(start, t.span.end);
                self.bump();
                Ok(Expr::new(ExprKind::Lit(Lit::Float(v, kind)), span))
            }
            Token::Bool(b) => {
                let b = *b;
                let span = Span::new(start, t.span.end);
                self.bump();
                Ok(Expr::new(ExprKind::Lit(Lit::Bool(b)), span))
            }
            Token::Str(s) => {
                let s = s.clone();
                let span = Span::new(start, t.span.end);
                self.bump();
                Ok(Expr::new(ExprKind::Lit(Lit::Str(s)), span))
            }
            Token::Ident(name) => {
                let name = name.clone();
                let span = Span::new(start, t.span.end);
                self.bump();
                Ok(Expr::new(ExprKind::Var(name), span))
            }
            Token::LParen => self.parse_compound(),
            other => Err(ParseError::Unexpected(format!("{:?}", other), start)),
        }
    }

    fn parse_compound(&mut self) -> Result<Expr, ParseError> {
        let lp = self.eat(&Token::LParen)?;
        let start = lp.span.start;
        let head = self.peek().ok_or(ParseError::UnexpectedEof)?;
        let head_name = match &head.tok {
            Token::Ident(s) => Some(s.clone()),
            _ => None,
        };
        match head_name.as_deref() {
            Some("if") => {
                self.bump();
                let cond = self.parse_expr()?;
                let then_branch = self.parse_expr()?;
                let else_branch = self.parse_expr()?;
                let rp = self.eat(&Token::RParen)?;
                Ok(Expr::new(
                    ExprKind::If {
                        cond: Box::new(cond),
                        then_branch: Box::new(then_branch),
                        else_branch: Box::new(else_branch),
                    },
                    Span::new(start, rp.span.end),
                ))
            }
            Some("let") => {
                self.bump();
                self.eat(&Token::LBracket)?;
                let mut bindings = Vec::new();
                loop {
                    if matches!(self.peek().map(|s| &s.tok), Some(Token::RBracket)) {
                        break;
                    }
                    let b = self.parse_binding()?;
                    bindings.push(b);
                    if matches!(self.peek().map(|s| &s.tok), Some(Token::Comma)) {
                        self.bump();
                    }
                }
                self.eat(&Token::RBracket)?;
                let body = self.parse_expr()?;
                let rp = self.eat(&Token::RParen)?;
                Ok(Expr::new(
                    ExprKind::Let { bindings, body: Box::new(body) },
                    Span::new(start, rp.span.end),
                ))
            }
            Some("do") => {
                self.bump();
                let mut exprs = Vec::new();
                while !matches!(self.peek().map(|s| &s.tok), Some(Token::RParen)) {
                    exprs.push(self.parse_expr()?);
                }
                let rp = self.eat(&Token::RParen)?;
                Ok(Expr::new(
                    ExprKind::Do(exprs),
                    Span::new(start, rp.span.end),
                ))
            }
            Some("as") => {
                self.bump();
                let ty = self.parse_type()?;
                let expr = self.parse_expr()?;
                let rp = self.eat(&Token::RParen)?;
                Ok(Expr::new(
                    ExprKind::Cast {
                        ty,
                        expr: Box::new(expr),
                    },
                    Span::new(start, rp.span.end),
                ))
            }
            Some("set!") => {
                self.bump();
                let name_tok = self.bump().ok_or(ParseError::UnexpectedEof)?;
                let name = match &name_tok.tok {
                    Token::Ident(s) => s.clone(),
                    other => {
                        return Err(ParseError::Unexpected(
                            format!("{:?}", other),
                            name_tok.span.start,
                        ));
                    }
                };
                let value = self.parse_expr()?;
                let rp = self.eat(&Token::RParen)?;
                Ok(Expr::new(
                    ExprKind::Set {
                        name,
                        value: Box::new(value),
                    },
                    Span::new(start, rp.span.end),
                ))
            }
            Some("while") => {
                self.bump();
                let cond = self.parse_expr()?;
                let body = self.parse_expr()?;
                let rp = self.eat(&Token::RParen)?;
                Ok(Expr::new(
                    ExprKind::While {
                        cond: Box::new(cond),
                        body: Box::new(body),
                    },
                    Span::new(start, rp.span.end),
                ))
            }
            Some("array") => {
                self.bump();
                let elem_ty = self.parse_type()?;
                let mut elems = Vec::new();
                while !matches!(self.peek().map(|s| &s.tok), Some(Token::RParen)) {
                    elems.push(self.parse_expr()?);
                }
                let rp = self.eat(&Token::RParen)?;
                Ok(Expr::new(
                    ExprKind::ArrayLit { elem_ty, elems },
                    Span::new(start, rp.span.end),
                ))
            }
            Some(_) | None => {
                // generic call: callee must be ident
                let callee_tok = self.bump().ok_or(ParseError::UnexpectedEof)?;
                let callee = match &callee_tok.tok {
                    Token::Ident(s) => s.clone(),
                    other => return Err(ParseError::Unexpected(format!("{:?}", other), callee_tok.span.start)),
                };
                let mut args = Vec::new();
                while !matches!(self.peek().map(|s| &s.tok), Some(Token::RParen)) {
                    args.push(self.parse_expr()?);
                }
                let rp = self.eat(&Token::RParen)?;
                Ok(Expr::new(
                    ExprKind::Call { callee, args },
                    Span::new(start, rp.span.end),
                ))
            }
        }
    }

    fn parse_binding(&mut self) -> Result<Binding, ParseError> {
        let name_tok = self.bump().ok_or(ParseError::UnexpectedEof)?;
        let start = name_tok.span.start;
        let name = match &name_tok.tok {
            Token::Ident(s) => s.clone(),
            other => return Err(ParseError::Unexpected(format!("{:?}", other), start)),
        };
        self.eat(&Token::Colon)?;
        let ty = self.parse_type()?;
        let value = self.parse_expr()?;
        let end = value.span.end;
        Ok(Binding { name, ty, value, span: Span::new(start, end) })
    }
}

fn token_name(t: &Token) -> &'static str {
    match t {
        Token::LParen => "'('",
        Token::RParen => "')'",
        Token::LBracket => "'['",
        Token::RBracket => "']'",
        Token::Colon => "':'",
        Token::Comma => "','",
        Token::Arrow => "'->'",
        Token::Ident(_) => "identifier",
        Token::Int(..) => "int literal",
        Token::Float(..) => "float literal",
        Token::Str(_) => "string literal",
        Token::Bool(_) => "boolean",
    }
}

/// Combined lex/parse error so callers can match on either stage.
#[derive(Debug, Error)]
pub enum FrontendError {
    #[error(transparent)]
    Lex(#[from] crate::lexer::LexError),
    #[error(transparent)]
    Parse(#[from] ParseError),
}

pub fn parse(src: &str) -> Result<Program, FrontendError> {
    let toks = crate::lexer::lex(src)?;
    let mut p = Parser::new(&toks);
    Ok(p.parse_program()?)
}

/// Parse a single expression (for REPL evaluation).
pub fn parse_expr_src(src: &str) -> Result<Expr, FrontendError> {
    let toks = crate::lexer::lex(src)?;
    let mut p = Parser::new(&toks);
    let expr = p.parse_expr()?;
    if let Some(extra) = p.peek() {
        return Err(ParseError::Unexpected(format!("{:?}", extra.tok), extra.span.start).into());
    }
    Ok(expr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hello() {
        let src = r#"
        (defn main [] -> i32
          (do
            (println "hi")
            0))
        "#;
        let prog = parse(src).unwrap();
        assert_eq!(prog.items.len(), 1);
    }

    #[test]
    fn parse_add() {
        let src = "(defn add [x: i32, y: i32] -> i32 (+ x y))";
        let prog = parse(src).unwrap();
        assert_eq!(prog.items.len(), 1);
    }

    #[test]
    fn parse_let_if() {
        let src = "(defn f [n: i32] -> i32 (let [x: i32 (+ n 1)] (if (< x 0) 0 x)))";
        let prog = parse(src).unwrap();
        assert_eq!(prog.items.len(), 1);
    }
}
