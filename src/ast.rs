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
    /// Fixed-length array. Values are represented as pointers to stack storage.
    Array { elem: Box<Type>, len: u32 },
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::I32 => f.write_str("i32"),
            Type::I64 => f.write_str("i64"),
            Type::F32 => f.write_str("f32"),
            Type::F64 => f.write_str("f64"),
            Type::Bool => f.write_str("bool"),
            Type::Str => f.write_str("str"),
            Type::Unit => f.write_str("unit"),
            Type::Array { elem, len } => write!(f, "(Array {elem} {len})"),
        }
    }
}

impl Type {
    pub fn is_array_elem_allowed(&self) -> bool {
        matches!(
            self,
            Type::I32 | Type::I64 | Type::F32 | Type::F64 | Type::Bool
        )
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
    /// `(array T e1 e2 ... en)` — fixed array literal
    ArrayLit {
        elem_ty: Type,
        elems: Vec<Expr>,
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
