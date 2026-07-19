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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    /// User-defined struct or enum (by name).
    Named(String),
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
            Type::Named(n) => f.write_str(n),
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

    /// Fields / enum payloads in MVP ADTs.
    pub fn is_adt_field_allowed(&self) -> bool {
        self.is_array_elem_allowed()
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
pub struct FieldDef {
    pub name: String,
    pub ty: Type,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct StructDef {
    pub name: String,
    pub fields: Vec<FieldDef>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct VariantDef {
    pub name: String,
    /// `None` = unit variant.
    pub payload: Option<Type>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct EnumDef {
    pub name: String,
    pub variants: Vec<VariantDef>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct MatchArm {
    pub variant: String,
    /// Binding for payload variants; must be `None` for unit variants.
    pub binding: Option<String>,
    pub body: Expr,
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
        Self {
            kind,
            span,
            ty: None,
        }
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
    /// `(loop body)` — infinite loop; value is unit (exit via `break`)
    Loop {
        body: Box<Expr>,
    },
    /// `(break)` — exit innermost while/loop; value is unit
    Break,
    /// `(array T e1 e2 ... en)` — fixed array literal
    ArrayLit {
        elem_ty: Type,
        elems: Vec<Expr>,
    },
    /// `(field e name)` — struct field access
    Field {
        base: Box<Expr>,
        field: String,
    },
    /// `(match e (Variant body) (Variant x body) ...)`
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
    },
    /// Function call, builtin, struct construct, or enum variant construct
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

/// Generic function template. Not emitted until monomorphized.
/// `type_params`: `(Name, OptionalTraitBound)`.
#[derive(Debug, Clone)]
pub struct GenericFunction {
    pub name: String,
    pub type_params: Vec<(String, Option<String>)>,
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
pub struct ExternFn {
    pub abi: String,
    pub name: String,
    pub params: Vec<Param>,
    pub ret: Type,
    pub span: Span,
}

/// Trait method signature (no body). First param may be bare `self` (type filled later).
#[derive(Debug, Clone)]
pub struct MethodSig {
    pub name: String,
    pub params: Vec<Param>,
    pub ret: Type,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct TraitDef {
    pub name: String,
    pub methods: Vec<MethodSig>,
    pub span: Span,
}

/// `impl Trait for T` block. Methods reuse `Function` (name is the method name;
/// codegen uses a mangled LLVM symbol).
#[derive(Debug, Clone)]
pub struct ImplBlock {
    pub trait_name: String,
    pub for_ty: Type,
    pub methods: Vec<Function>,
    pub span: Span,
}

/// Syntactic macro: `(defmacro name [params] template)`.
/// Expanded before type checking; not present in the program after macroexpand.
#[derive(Debug, Clone)]
pub struct MacroDef {
    pub name: String,
    pub params: Vec<String>,
    pub template: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum TopLevel {
    Function(Function),
    GenericFunction(GenericFunction),
    Const(Const),
    Struct(StructDef),
    Enum(EnumDef),
    Extern(ExternFn),
    Trait(TraitDef),
    Impl(ImplBlock),
    DefMacro(MacroDef),
    /// `(module name)` — optional; file stem is used when absent. Stripped by resolve.
    Module { name: String, span: Span },
    /// `(import name)` — load `name.rsp` and merge prefixed items. Stripped by resolve.
    Import { name: String, span: Span },
}

#[derive(Debug, Clone)]
pub struct Program {
    pub items: Vec<TopLevel>,
}
