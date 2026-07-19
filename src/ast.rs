//! AST definitions for Risp (.rsp)
//!
//! Some fields (e.g. `span`) are not yet read by the codegen path but will be
//! once richer error reporting lands. Suppress dead-code warnings until then.
#![allow(dead_code)]

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }
    pub fn dummy() -> Self {
        Self { start: 0, end: 0 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    I32,
    I64,
    F32,
    F64,
    Bool,
    Str,
    Unit,
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Type::I32 => "i32",
            Type::I64 => "i64",
            Type::F32 => "f32",
            Type::F64 => "f64",
            Type::Bool => "bool",
            Type::Str => "str",
            Type::Unit => "unit",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub ty: Type,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Binding {
    pub name: String,
    pub ty: Type,
    pub value: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Lit {
    Int(i64, Option<IntKind>),
    Float(f64, Option<FloatKind>),
    Bool(bool),
    Str(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntKind {
    I32,
    I64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatKind {
    F32,
    F64,
}

#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
    /// Filled by the type checker. `None` until typeck has run.
    pub ty: Option<Type>,
}

impl Expr {
    pub fn new(kind: ExprKind, span: Span) -> Self {
        Self { kind, span, ty: None }
    }
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    Lit(Lit),
    Var(String),
    /// `(if cond then else)`
    If {
        cond: Box<Expr>,
        then_branch: Box<Expr>,
        else_branch: Box<Expr>,
    },
    /// `(let [x: T v, y: T v] body)`
    Let {
        bindings: Vec<Binding>,
        body: Box<Expr>,
    },
    /// `(do e1 e2 ... en)` — value is the last expr
    Do(Vec<Expr>),
    /// `(as T e)` — numeric cast
    Cast {
        ty: Type,
        expr: Box<Expr>,
    },
    /// `(set! name value)` — assign to a local / parameter
    Set {
        name: String,
        value: Box<Expr>,
    },
    /// `(while cond body)` — loop while cond is true; value is unit
    While {
        cond: Box<Expr>,
        body: Box<Expr>,
    },
    /// Function call or builtin operator: `(f a b ...)`
    Call {
        callee: String,
        args: Vec<Expr>,
    },
}

#[derive(Debug, Clone)]
pub struct Function {
    pub name: String,
    pub params: Vec<Param>,
    pub ret: Type,
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Const {
    pub name: String,
    pub ty: Type,
    pub value: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum TopLevel {
    Function(Function),
    Const(Const),
}

#[derive(Debug, Clone)]
pub struct Program {
    pub items: Vec<TopLevel>,
}
