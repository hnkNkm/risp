//! Type checker.

use crate::ast::*;
use std::collections::{HashMap, HashSet};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TypeError {
    #[error("undefined variable {0:?}")]
    UndefinedVar(String, Span),
    #[error("undefined function {0:?}")]
    UndefinedFn(String, Span),
    #[error("undefined type {0:?}")]
    UndefinedType(String, Span),
    #[error("type mismatch: expected {expected}, found {found}")]
    Mismatch { expected: Type, found: Type, span: Span },
    #[error("arity mismatch for {name:?}: expected {expected}, got {got}")]
    Arity {
        name: String,
        expected: usize,
        got: usize,
        span: Span,
    },
    #[error("operator {op:?} cannot be applied to {ty}")]
    BadOperand { op: String, ty: Type, span: Span },
    #[error("integer literal does not fit type {0}")]
    IntLitOverflow(Type, Span),
    #[error("cannot cast from {from} to {to}")]
    BadCast { from: Type, to: Type, span: Span },
    #[error("duplicate definition of {0:?}")]
    Duplicate(String, Span),
    #[error("const initializer must be a literal")]
    ConstNotLiteral(Span),
    #[error("cannot assign to constant {0:?}")]
    AssignConst(String, Span),
    #[error("array element type {0} is not supported")]
    BadArrayElem(Type, Span),
    #[error("ADT field/payload type {0} is not supported")]
    BadAdtField(Type, Span),
    #[error("arrays cannot be used as function parameters or return types yet")]
    ArrayInSignature(Span),
    #[error("unknown field {0:?} on type {1}")]
    UnknownField(String, Type, Span),
    #[error("field access requires a struct, found {0}")]
    FieldOnNonStruct(Type, Span),
    #[error("match requires an enum, found {0}")]
    MatchNonEnum(Type, Span),
    #[error("unknown variant {0:?}")]
    UnknownVariant(String, Span),
    #[error("variant {0:?} belongs to {1}, not {2}")]
    VariantWrongEnum(String, String, String, Span),
    #[error("match is not exhaustive (missing {0:?})")]
    MatchNonExhaustive(String, Span),
    #[error("duplicate match arm for variant {0:?}")]
    MatchDuplicateArm(String, Span),
    #[error("unit variant {0:?} must not bind a value")]
    MatchUnitBinding(String, Span),
    #[error("payload variant {0:?} requires a binding")]
    MatchMissingBinding(String, Span),
    #[error("struct {0:?} must have at least one field")]
    EmptyStruct(String, Span),
    #[error("enum {0:?} must have at least one variant")]
    EmptyEnum(String, Span),
    #[error("`break` outside of loop")]
    BreakOutsideLoop(Span),
    #[error("unsupported extern ABI {0:?} (only \"C\" is supported)")]
    BadExternAbi(String, Span),
    #[error("extern parameter/return type {0} is not supported")]
    BadExternType(Type, Span),
    #[error("missing impl of trait {trait_name} for {ty}")]
    MissingImpl {
        trait_name: String,
        ty: Type,
        span: Span,
    },
    #[error("duplicate impl of trait {trait_name} for {ty}")]
    DuplicateImpl {
        trait_name: String,
        ty: Type,
        span: Span,
    },
    #[error("method {0:?} is not a member of trait {1}")]
    MethodNotInTrait(String, String, Span),
    #[error("ambiguous trait method name {0:?} (method names must be unique across traits)")]
    AmbiguousTraitMethod(String, Span),
    #[error("undefined trait {0:?}")]
    UndefinedTrait(String, Span),
    #[error("trait method call requires at least one argument (receiver)")]
    TraitCallNoReceiver(Span),
    #[error("cannot infer type parameter {0:?}")]
    InferTypeParam(String, Span),
    #[error("missing main function")]
    NoMain,
}

impl TypeError {
    /// Returns the source span for this error, if it has one.
    pub fn span(&self) -> Option<Span> {
        match self {
            TypeError::UndefinedVar(_, s)
            | TypeError::UndefinedFn(_, s)
            | TypeError::UndefinedType(_, s)
            | TypeError::Mismatch { span: s, .. }
            | TypeError::Arity { span: s, .. }
            | TypeError::BadOperand { span: s, .. }
            | TypeError::IntLitOverflow(_, s)
            | TypeError::BadCast { span: s, .. }
            | TypeError::Duplicate(_, s)
            | TypeError::ConstNotLiteral(s)
            | TypeError::AssignConst(_, s)
            | TypeError::BadArrayElem(_, s)
            | TypeError::BadAdtField(_, s)
            | TypeError::ArrayInSignature(s)
            | TypeError::UnknownField(_, _, s)
            | TypeError::FieldOnNonStruct(_, s)
            | TypeError::MatchNonEnum(_, s)
            | TypeError::UnknownVariant(_, s)
            | TypeError::VariantWrongEnum(_, _, _, s)
            | TypeError::MatchNonExhaustive(_, s)
            | TypeError::MatchDuplicateArm(_, s)
            | TypeError::MatchUnitBinding(_, s)
            | TypeError::MatchMissingBinding(_, s)
            | TypeError::EmptyStruct(_, s)
            | TypeError::EmptyEnum(_, s)
            | TypeError::BreakOutsideLoop(s)
            | TypeError::BadExternAbi(_, s)
            | TypeError::BadExternType(_, s)
            | TypeError::MissingImpl { span: s, .. }
            | TypeError::DuplicateImpl { span: s, .. }
            | TypeError::MethodNotInTrait(_, _, s)
            | TypeError::AmbiguousTraitMethod(_, s)
            | TypeError::UndefinedTrait(_, s)
            | TypeError::TraitCallNoReceiver(s)
            | TypeError::InferTypeParam(_, s) => Some(*s),
            TypeError::NoMain => None,
        }
    }
}

fn sanitize_ident_part(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Mangle an impl method to a unique LLVM / fn-table name.
/// Type display is sanitized: non-alphanumeric chars become `_`.
pub fn mangle_method(trait_name: &str, ty: &Type, method: &str) -> String {
    let ty_s = sanitize_ident_part(&ty.to_string());
    format!("__risp_{trait_name}_{ty_s}_{method}")
}

/// Mangle a monomorphized generic function.
pub fn mangle_mono(name: &str, tys: &[Type]) -> String {
    let name_s = sanitize_ident_part(name);
    let ty_parts: Vec<String> = tys.iter().map(|t| sanitize_ident_part(&t.to_string())).collect();
    format!("__risp_mono_{name_s}_{}", ty_parts.join("_"))
}

#[derive(Debug, Clone)]
pub struct FnSig {
    pub params: Vec<Type>,
    pub ret: Type,
}

#[derive(Debug, Clone)]
pub struct VariantInfo {
    pub enum_name: String,
    pub tag: u32,
    pub payload: Option<Type>,
}

pub struct TypeCk {
    pub fns: HashMap<String, FnSig>,
    pub consts: HashMap<String, Type>,
    pub structs: HashMap<String, StructDef>,
    pub enums: HashMap<String, EnumDef>,
    /// Variant constructor name -> info
    pub variants: HashMap<String, VariantInfo>,
    /// Names declared via `(extern "C" …)`.
    pub externs: HashMap<String, FnSig>,
    /// Trait name -> definition
    pub traits: HashMap<String, TraitDef>,
    /// Trait method name -> owning trait name (globally unique in MVP)
    pub trait_methods: HashMap<String, String>,
    /// (trait_name, for_ty) that have an impl
    pub impls: HashSet<(String, Type)>,
    /// Generic function templates (not in `fns` until monomorphized).
    generic_fns: HashMap<String, GenericFunction>,
    /// Already-emitted monomorphizations: (generic name, type args) -> mangled name.
    mono_cache: HashMap<(String, Vec<Type>), String>,
    /// Concrete functions produced by monomorphization (appended to Program).
    mono_fns: Vec<Function>,
    /// Nesting depth of `while` / `loop` while checking expressions.
    loop_depth: usize,
}

impl TypeCk {
    pub fn new() -> Self {
        Self {
            fns: HashMap::new(),
            consts: HashMap::new(),
            structs: HashMap::new(),
            enums: HashMap::new(),
            variants: HashMap::new(),
            externs: HashMap::new(),
            traits: HashMap::new(),
            trait_methods: HashMap::new(),
            impls: HashSet::new(),
            generic_fns: HashMap::new(),
            mono_cache: HashMap::new(),
            mono_fns: Vec::new(),
            loop_depth: 0,
        }
    }

    fn is_extern_type_ok(ty: &Type) -> bool {
        matches!(
            ty,
            Type::I32 | Type::I64 | Type::F32 | Type::F64 | Type::Bool | Type::Str | Type::Unit
        )
    }

    pub fn check(&mut self, prog: &mut Program) -> Result<(), TypeError> {
        self.check_ex(prog, true)
    }

    /// Type-check a program. When `require_main` is false (REPL definitions),
    /// a missing `main` is allowed; a present `main` must still be `[] -> i32`.
    pub fn check_ex(&mut self, prog: &mut Program, require_main: bool) -> Result<(), TypeError> {
        self.fns.clear();
        self.consts.clear();
        self.structs.clear();
        self.enums.clear();
        self.variants.clear();
        self.externs.clear();
        self.traits.clear();
        self.trait_methods.clear();
        self.impls.clear();
        self.generic_fns.clear();
        self.mono_cache.clear();
        self.mono_fns.clear();

        // Collect type definitions first.
        for it in &prog.items {
            match it {
                TopLevel::Struct(s) => {
                    self.register_name(&s.name, s.span)?;
                    if s.fields.is_empty() {
                        return Err(TypeError::EmptyStruct(s.name.clone(), s.span));
                    }
                    let mut seen = HashSet::new();
                    for f in &s.fields {
                        if !seen.insert(f.name.clone()) {
                            return Err(TypeError::Duplicate(f.name.clone(), f.span));
                        }
                        if !f.ty.is_adt_field_allowed() {
                            return Err(TypeError::BadAdtField(f.ty.clone(), f.span));
                        }
                    }
                    self.structs.insert(s.name.clone(), s.clone());
                }
                TopLevel::Enum(e) => {
                    self.register_name(&e.name, e.span)?;
                    if e.variants.is_empty() {
                        return Err(TypeError::EmptyEnum(e.name.clone(), e.span));
                    }
                    let mut seen = HashSet::new();
                    for (i, v) in e.variants.iter().enumerate() {
                        if !seen.insert(v.name.clone()) {
                            return Err(TypeError::Duplicate(v.name.clone(), v.span));
                        }
                        self.register_name(&v.name, v.span)?;
                        if let Some(p) = &v.payload {
                            if !p.is_adt_field_allowed() {
                                return Err(TypeError::BadAdtField(p.clone(), v.span));
                            }
                        }
                        self.variants.insert(
                            v.name.clone(),
                            VariantInfo {
                                enum_name: e.name.clone(),
                                tag: i as u32,
                                payload: v.payload.clone(),
                            },
                        );
                    }
                    self.enums.insert(e.name.clone(), e.clone());
                }
                _ => {}
            }
        }

        // Collect traits (method names must be globally unique across traits).
        for it in &prog.items {
            if let TopLevel::Trait(t) = it {
                self.register_name(&t.name, t.span)?;
                let mut seen_methods = HashSet::new();
                for m in &t.methods {
                    if !seen_methods.insert(m.name.clone()) {
                        return Err(TypeError::Duplicate(m.name.clone(), m.span));
                    }
                    if self.trait_methods.contains_key(&m.name) {
                        return Err(TypeError::AmbiguousTraitMethod(m.name.clone(), m.span));
                    }
                    self.register_name(&m.name, m.span)?;
                    if m.params.is_empty() {
                        return Err(TypeError::Arity {
                            name: m.name.clone(),
                            expected: 1,
                            got: 0,
                            span: m.span,
                        });
                    }
                    for (i, p) in m.params.iter().enumerate() {
                        if i == 0 && p.name == "self" {
                            // Bare / receiver self — concrete type comes from impl.
                            continue;
                        }
                        self.resolve_type(&p.ty, p.span)?;
                        if matches!(p.ty, Type::Array { .. }) {
                            return Err(TypeError::ArrayInSignature(p.span));
                        }
                    }
                    self.resolve_type(&m.ret, m.span)?;
                    if matches!(m.ret, Type::Array { .. }) {
                        return Err(TypeError::ArrayInSignature(m.span));
                    }
                    self.trait_methods.insert(m.name.clone(), t.name.clone());
                }
                self.traits.insert(t.name.clone(), t.clone());
            }
        }

        // Collect function / generic / const signatures.
        for it in &prog.items {
            match it {
                TopLevel::Function(f) => {
                    self.register_name(&f.name, f.span)?;
                    for p in &f.params {
                        self.resolve_type(&p.ty, p.span)?;
                        if matches!(p.ty, Type::Array { .. }) {
                            return Err(TypeError::ArrayInSignature(p.span));
                        }
                    }
                    self.resolve_type(&f.ret, f.span)?;
                    if matches!(f.ret, Type::Array { .. }) {
                        return Err(TypeError::ArrayInSignature(f.span));
                    }
                    let sig = FnSig {
                        params: f.params.iter().map(|p| p.ty.clone()).collect(),
                        ret: f.ret.clone(),
                    };
                    self.fns.insert(f.name.clone(), sig);
                }
                TopLevel::GenericFunction(g) => {
                    self.register_name(&g.name, g.span)?;
                    let mut tp_names: HashSet<String> = HashSet::new();
                    for (name, bound) in &g.type_params {
                        if !tp_names.insert(name.clone()) {
                            return Err(TypeError::Duplicate(name.clone(), g.span));
                        }
                        if let Some(trait_name) = bound {
                            if !self.traits.contains_key(trait_name) {
                                return Err(TypeError::UndefinedTrait(trait_name.clone(), g.span));
                            }
                        }
                    }
                    for p in &g.params {
                        self.resolve_type_with_params(&p.ty, p.span, &tp_names)?;
                        if matches!(p.ty, Type::Array { .. }) {
                            return Err(TypeError::ArrayInSignature(p.span));
                        }
                    }
                    self.resolve_type_with_params(&g.ret, g.span, &tp_names)?;
                    if matches!(g.ret, Type::Array { .. }) {
                        return Err(TypeError::ArrayInSignature(g.span));
                    }
                    self.generic_fns.insert(g.name.clone(), g.clone());
                }
                TopLevel::Extern(e) => {
                    if e.abi != "C" {
                        return Err(TypeError::BadExternAbi(e.abi.clone(), e.span));
                    }
                    self.register_name(&e.name, e.span)?;
                    for p in &e.params {
                        if !Self::is_extern_type_ok(&p.ty) || matches!(p.ty, Type::Unit) {
                            return Err(TypeError::BadExternType(p.ty.clone(), p.span));
                        }
                        if matches!(p.ty, Type::Named(_)) {
                            return Err(TypeError::BadExternType(p.ty.clone(), p.span));
                        }
                    }
                    if !Self::is_extern_type_ok(&e.ret) {
                        return Err(TypeError::BadExternType(e.ret.clone(), e.span));
                    }
                    if matches!(e.ret, Type::Named(_) | Type::Array { .. }) {
                        return Err(TypeError::BadExternType(e.ret.clone(), e.span));
                    }
                    let sig = FnSig {
                        params: e.params.iter().map(|p| p.ty.clone()).collect(),
                        ret: e.ret.clone(),
                    };
                    self.fns.insert(e.name.clone(), sig.clone());
                    self.externs.insert(e.name.clone(), sig);
                }
                TopLevel::Const(c) => {
                    self.register_name(&c.name, c.span)?;
                    self.resolve_type(&c.ty, c.span)?;
                    self.consts.insert(c.name.clone(), c.ty.clone());
                }
                TopLevel::Struct(_) | TopLevel::Enum(_) | TopLevel::Trait(_) => {}
                TopLevel::Impl(_) | TopLevel::DefMacro(_) => {}
            }
        }

        // Collect impls: match trait sigs, fill self type, register mangled fns.
        for it in &mut prog.items {
            if let TopLevel::Impl(ib) = it {
                let trait_def = self
                    .traits
                    .get(&ib.trait_name)
                    .cloned()
                    .ok_or_else(|| TypeError::UndefinedTrait(ib.trait_name.clone(), ib.span))?;
                self.resolve_type(&ib.for_ty, ib.span)?;
                let key = (ib.trait_name.clone(), ib.for_ty.clone());
                if !self.impls.insert(key) {
                    return Err(TypeError::DuplicateImpl {
                        trait_name: ib.trait_name.clone(),
                        ty: ib.for_ty.clone(),
                        span: ib.span,
                    });
                }

                let mut impl_methods: HashSet<String> = HashSet::new();
                for m in &mut ib.methods {
                    if !impl_methods.insert(m.name.clone()) {
                        return Err(TypeError::Duplicate(m.name.clone(), m.span));
                    }
                    let Some(tsig) = trait_def.methods.iter().find(|t| t.name == m.name) else {
                        return Err(TypeError::MethodNotInTrait(
                            m.name.clone(),
                            ib.trait_name.clone(),
                            m.span,
                        ));
                    };
                    if m.params.len() != tsig.params.len() {
                        return Err(TypeError::Arity {
                            name: m.name.clone(),
                            expected: tsig.params.len(),
                            got: m.params.len(),
                            span: m.span,
                        });
                    }
                    if m.ret != tsig.ret {
                        return Err(TypeError::Mismatch {
                            expected: tsig.ret.clone(),
                            found: m.ret.clone(),
                            span: m.span,
                        });
                    }
                    // Fill self / first param type from `for T`.
                    if let Some(first) = m.params.first_mut() {
                        if first.name == "self" {
                            first.ty = ib.for_ty.clone();
                        }
                    }
                    for (i, p) in m.params.iter().enumerate() {
                        if i == 0 && p.name == "self" {
                            continue;
                        }
                        // Non-self params must match trait (after trait self is abstract).
                        let tp = &tsig.params[i];
                        if i > 0 || tp.name != "self" {
                            if p.ty != tp.ty {
                                return Err(TypeError::Mismatch {
                                    expected: tp.ty.clone(),
                                    found: p.ty.clone(),
                                    span: p.span,
                                });
                            }
                        }
                        self.resolve_type(&p.ty, p.span)?;
                        if matches!(p.ty, Type::Array { .. }) {
                            return Err(TypeError::ArrayInSignature(p.span));
                        }
                    }
                    self.resolve_type(&m.ret, m.span)?;
                    if matches!(m.ret, Type::Array { .. }) {
                        return Err(TypeError::ArrayInSignature(m.span));
                    }

                    let mangled = mangle_method(&ib.trait_name, &ib.for_ty, &m.name);
                    let sig = FnSig {
                        params: m.params.iter().map(|p| p.ty.clone()).collect(),
                        ret: m.ret.clone(),
                    };
                    self.fns.insert(mangled, sig);
                }
            }
        }

        // ensure main exists with signature `[] -> i32` (optional in REPL)
        match self.fns.get("main") {
            Some(sig) if sig.params.is_empty() && sig.ret == Type::I32 => {}
            Some(_) => {
                let main_span = prog
                    .items
                    .iter()
                    .find_map(|it| match it {
                        TopLevel::Function(f) if f.name == "main" => Some(f.span),
                        _ => None,
                    })
                    .unwrap_or_else(Span::dummy);
                return Err(TypeError::Mismatch {
                    expected: Type::I32,
                    found: self.fns["main"].ret.clone(),
                    span: main_span,
                });
            }
            None if require_main => return Err(TypeError::NoMain),
            None => {}
        }

        for it in &mut prog.items {
            match it {
                TopLevel::Function(f) => {
                    let mut env: HashMap<String, Type> = HashMap::new();
                    for p in &f.params {
                        env.insert(p.name.clone(), p.ty.clone());
                    }
                    let body_span = f.body.span;
                    let body_ty = self.check_expr(&mut f.body, &mut env)?;
                    expect(&f.ret, &body_ty, body_span)?;
                }
                TopLevel::Impl(ib) => {
                    for m in &mut ib.methods {
                        let mut env: HashMap<String, Type> = HashMap::new();
                        for p in &m.params {
                            env.insert(p.name.clone(), p.ty.clone());
                        }
                        let body_span = m.body.span;
                        let body_ty = self.check_expr(&mut m.body, &mut env)?;
                        expect(&m.ret, &body_ty, body_span)?;
                    }
                }
                TopLevel::Const(c) => {
                    if !matches!(c.value.kind, ExprKind::Lit(_)) {
                        return Err(TypeError::ConstNotLiteral(c.value.span));
                    }
                    let mut env = HashMap::new();
                    let val_span = c.value.span;
                    let ty = self.check_expr(&mut c.value, &mut env)?;
                    expect(&c.ty, &ty, val_span)?;
                }
                TopLevel::Struct(_)
                | TopLevel::Enum(_)
                | TopLevel::Extern(_)
                | TopLevel::Trait(_)
                | TopLevel::GenericFunction(_)
                | TopLevel::DefMacro(_) => {}
            }
        }

        // Emit monomorphized functions so codegen sees them as normal Functions.
        for f in self.mono_fns.drain(..) {
            prog.items.push(TopLevel::Function(f));
        }
        Ok(())
    }

    fn register_name(&self, name: &str, span: Span) -> Result<(), TypeError> {
        if self.fns.contains_key(name)
            || self.consts.contains_key(name)
            || self.structs.contains_key(name)
            || self.enums.contains_key(name)
            || self.variants.contains_key(name)
            || self.externs.contains_key(name)
            || self.traits.contains_key(name)
            || self.trait_methods.contains_key(name)
            || self.generic_fns.contains_key(name)
        {
            return Err(TypeError::Duplicate(name.to_string(), span));
        }
        Ok(())
    }

    fn resolve_type(&self, ty: &Type, span: Span) -> Result<(), TypeError> {
        match ty {
            Type::Named(n) => {
                if self.structs.contains_key(n) || self.enums.contains_key(n) {
                    Ok(())
                } else {
                    Err(TypeError::UndefinedType(n.clone(), span))
                }
            }
            Type::Array { elem, .. } => self.resolve_type(elem, span),
            _ => Ok(()),
        }
    }

    fn resolve_type_with_params(
        &self,
        ty: &Type,
        span: Span,
        type_params: &HashSet<String>,
    ) -> Result<(), TypeError> {
        match ty {
            Type::Named(n) if type_params.contains(n) => Ok(()),
            Type::Named(n) => {
                if self.structs.contains_key(n) || self.enums.contains_key(n) {
                    Ok(())
                } else {
                    Err(TypeError::UndefinedType(n.clone(), span))
                }
            }
            Type::Array { elem, .. } => self.resolve_type_with_params(elem, span, type_params),
            _ => Ok(()),
        }
    }

    fn check_expr(
        &mut self,
        e: &mut Expr,
        env: &mut HashMap<String, Type>,
    ) -> Result<Type, TypeError> {
        let span = e.span;
        let ty = match &mut e.kind {
            ExprKind::Lit(l) => {
                let ty = lit_type(l);
                if let Lit::Int(v, _) = l {
                    if ty == Type::I32 && (*v < i32::MIN as i64 || *v > i32::MAX as i64) {
                        return Err(TypeError::IntLitOverflow(Type::I32, span));
                    }
                }
                ty
            }
            ExprKind::Var(name) => {
                if let Some(t) = env.get(name) {
                    t.clone()
                } else if let Some(t) = self.consts.get(name) {
                    t.clone()
                } else {
                    return Err(TypeError::UndefinedVar(name.clone(), span));
                }
            }
            ExprKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                let cond_span = cond.span;
                let ct = self.check_expr(cond, env)?;
                expect(&Type::Bool, &ct, cond_span)?;
                let tt = self.check_expr(then_branch, env)?;
                let et_span = else_branch.span;
                let et = self.check_expr(else_branch, env)?;
                if tt != et {
                    return Err(TypeError::Mismatch {
                        expected: tt,
                        found: et,
                        span: et_span,
                    });
                }
                tt
            }
            ExprKind::Let { bindings, body } => {
                let snapshot: Vec<(String, Option<Type>)> = bindings
                    .iter()
                    .map(|b| (b.name.clone(), env.get(&b.name).cloned()))
                    .collect();
                for b in bindings.iter_mut() {
                    self.resolve_type(&b.ty, b.span)?;
                    let val_span = b.value.span;
                    let vt = self.check_expr(&mut b.value, env)?;
                    expect(&b.ty, &vt, val_span)?;
                    env.insert(b.name.clone(), b.ty.clone());
                }
                let bt = self.check_expr(body, env)?;
                for (name, prev) in snapshot {
                    match prev {
                        Some(t) => {
                            env.insert(name, t);
                        }
                        None => {
                            env.remove(&name);
                        }
                    }
                }
                bt
            }
            ExprKind::Do(exprs) => {
                let mut last = Type::Unit;
                for ex in exprs.iter_mut() {
                    last = self.check_expr(ex, env)?;
                }
                last
            }
            ExprKind::Cast { ty, expr } => {
                let from = self.check_expr(expr, env)?;
                let to = ty.clone();
                if !cast_allowed(&from, &to) {
                    return Err(TypeError::BadCast { from, to, span });
                }
                to
            }
            ExprKind::Set { name, value } => {
                let expected = if let Some(t) = env.get(name) {
                    t.clone()
                } else if self.consts.contains_key(name) {
                    return Err(TypeError::AssignConst(name.clone(), span));
                } else {
                    return Err(TypeError::UndefinedVar(name.clone(), span));
                };
                let val_span = value.span;
                let vt = self.check_expr(value, env)?;
                expect(&expected, &vt, val_span)?;
                Type::Unit
            }
            ExprKind::While { cond, body } => {
                let cond_span = cond.span;
                let ct = self.check_expr(cond, env)?;
                expect(&Type::Bool, &ct, cond_span)?;
                self.loop_depth += 1;
                let body_res = self.check_expr(body, env);
                self.loop_depth -= 1;
                let _ = body_res?;
                Type::Unit
            }
            ExprKind::Loop { body } => {
                self.loop_depth += 1;
                let body_res = self.check_expr(body, env);
                self.loop_depth -= 1;
                let _ = body_res?;
                Type::Unit
            }
            ExprKind::Break => {
                if self.loop_depth == 0 {
                    return Err(TypeError::BreakOutsideLoop(span));
                }
                Type::Unit
            }
            ExprKind::ArrayLit { elem_ty, elems } => {
                if !elem_ty.is_array_elem_allowed() {
                    return Err(TypeError::BadArrayElem(elem_ty.clone(), span));
                }
                for el in elems.iter_mut() {
                    let s = el.span;
                    let t = self.check_expr(el, env)?;
                    expect(elem_ty, &t, s)?;
                }
                Type::Array {
                    elem: Box::new(elem_ty.clone()),
                    len: elems.len() as u32,
                }
            }
            ExprKind::Field { base, field } => {
                let bt = self.check_expr(base, env)?;
                let Type::Named(ref n) = bt else {
                    return Err(TypeError::FieldOnNonStruct(bt, span));
                };
                let Some(sdef) = self.structs.get(n) else {
                    return Err(TypeError::FieldOnNonStruct(bt, span));
                };
                let Some(f) = sdef.fields.iter().find(|f| f.name == *field) else {
                    return Err(TypeError::UnknownField(field.clone(), bt, span));
                };
                f.ty.clone()
            }
            ExprKind::Match { scrutinee, arms } => self.check_match(scrutinee, arms, env, span)?,
            ExprKind::Call { callee, args } => self.check_call(callee, args, env, span)?,
        };
        e.ty = Some(ty.clone());
        Ok(ty)
    }

    fn check_match(
        &mut self,
        scrutinee: &mut Expr,
        arms: &mut [MatchArm],
        env: &mut HashMap<String, Type>,
        span: Span,
    ) -> Result<Type, TypeError> {
        let st = self.check_expr(scrutinee, env)?;
        let Type::Named(ref ename) = st else {
            return Err(TypeError::MatchNonEnum(st, span));
        };
        let Some(edef) = self.enums.get(ename).cloned() else {
            return Err(TypeError::MatchNonEnum(st, span));
        };

        let mut seen = HashSet::new();
        let mut result_ty: Option<Type> = None;

        for arm in arms.iter_mut() {
            let info = self
                .variants
                .get(&arm.variant)
                .ok_or_else(|| TypeError::UnknownVariant(arm.variant.clone(), arm.span))?;
            if info.enum_name != edef.name {
                return Err(TypeError::VariantWrongEnum(
                    arm.variant.clone(),
                    info.enum_name.clone(),
                    edef.name.clone(),
                    arm.span,
                ));
            }
            if !seen.insert(arm.variant.clone()) {
                return Err(TypeError::MatchDuplicateArm(arm.variant.clone(), arm.span));
            }

            match (&info.payload, &arm.binding) {
                (None, Some(_)) => {
                    return Err(TypeError::MatchUnitBinding(arm.variant.clone(), arm.span));
                }
                (Some(_), None) => {
                    return Err(TypeError::MatchMissingBinding(arm.variant.clone(), arm.span));
                }
                _ => {}
            }

            let prev = if let (Some(pty), Some(bname)) = (&info.payload, &arm.binding) {
                let prev = env.insert(bname.clone(), pty.clone());
                Some((bname.clone(), prev))
            } else {
                None
            };

            let bt = self.check_expr(&mut arm.body, env)?;
            if let Some((bname, prev)) = prev {
                match prev {
                    Some(t) => {
                        env.insert(bname, t);
                    }
                    None => {
                        env.remove(&bname);
                    }
                }
            }

            match &result_ty {
                None => result_ty = Some(bt),
                Some(expected) => expect(expected, &bt, arm.body.span)?,
            }
        }

        for v in &edef.variants {
            if !seen.contains(&v.name) {
                return Err(TypeError::MatchNonExhaustive(v.name.clone(), span));
            }
        }

        Ok(result_ty.unwrap_or(Type::Unit))
    }

    /// Check that all args are the same numeric type; return that type.
    fn check_numeric_args(
        &mut self,
        op: &str,
        args: &mut [Expr],
        env: &mut HashMap<String, Type>,
    ) -> Result<Type, TypeError> {
        let first_span = args[0].span;
        let first = self.check_expr(&mut args[0], env)?;
        if !is_numeric(&first) {
            return Err(TypeError::BadOperand {
                op: op.into(),
                ty: first,
                span: first_span,
            });
        }
        for arg in args.iter_mut().skip(1) {
            let s = arg.span;
            let t = self.check_expr(arg, env)?;
            expect(&first, &t, s)?;
        }
        Ok(first)
    }

    fn check_call(
        &mut self,
        callee: &mut String,
        args: &mut [Expr],
        env: &mut HashMap<String, Type>,
        call_span: Span,
    ) -> Result<Type, TypeError> {
        // Trait method: resolve impl by first arg type and rewrite callee to mangled name.
        if let Some(trait_name) = self.trait_methods.get(callee).cloned() {
            if args.is_empty() {
                return Err(TypeError::TraitCallNoReceiver(call_span));
            }
            let recv_ty = self.check_expr(&mut args[0], env)?;
            if !self.impls.contains(&(trait_name.clone(), recv_ty.clone())) {
                return Err(TypeError::MissingImpl {
                    trait_name,
                    ty: recv_ty,
                    span: call_span,
                });
            }
            let mangled = mangle_method(&trait_name, &recv_ty, callee);
            let sig = self
                .fns
                .get(&mangled)
                .cloned()
                .ok_or_else(|| TypeError::UndefinedFn(mangled.clone(), call_span))?;
            if sig.params.len() != args.len() {
                return Err(TypeError::Arity {
                    name: callee.clone(),
                    expected: sig.params.len(),
                    got: args.len(),
                    span: call_span,
                });
            }
            // Receiver already checked; remaining args against mangled sig.
            expect(&sig.params[0], &recv_ty, args[0].span)?;
            for (param_ty, arg) in sig.params.iter().skip(1).zip(args.iter_mut().skip(1)) {
                let s = arg.span;
                let at = self.check_expr(arg, env)?;
                expect(param_ty, &at, s)?;
            }
            *callee = mangled;
            return Ok(sig.ret.clone());
        }

        // Struct constructor
        if self.structs.contains_key(callee.as_str()) {
            let sdef = self.structs[callee.as_str()].clone();
            if sdef.fields.len() != args.len() {
                return Err(TypeError::Arity {
                    name: callee.clone(),
                    expected: sdef.fields.len(),
                    got: args.len(),
                    span: call_span,
                });
            }
            for (field, arg) in sdef.fields.iter().zip(args.iter_mut()) {
                let s = arg.span;
                let at = self.check_expr(arg, env)?;
                expect(&field.ty, &at, s)?;
            }
            return Ok(Type::Named(sdef.name));
        }

        // Enum variant constructor
        if self.variants.contains_key(callee.as_str()) {
            let info = self.variants[callee.as_str()].clone();
            match &info.payload {
                None => {
                    if !args.is_empty() {
                        return Err(TypeError::Arity {
                            name: callee.clone(),
                            expected: 0,
                            got: args.len(),
                            span: call_span,
                        });
                    }
                }
                Some(pty) => {
                    if args.len() != 1 {
                        return Err(TypeError::Arity {
                            name: callee.clone(),
                            expected: 1,
                            got: args.len(),
                            span: call_span,
                        });
                    }
                    let s = args[0].span;
                    let at = self.check_expr(&mut args[0], env)?;
                    expect(pty, &at, s)?;
                }
            }
            return Ok(Type::Named(info.enum_name));
        }

        // builtins first
        match callee.as_str() {
            "/" | "mod" => {
                if args.len() != 2 {
                    return Err(TypeError::Arity {
                        name: callee.clone(),
                        expected: 2,
                        got: args.len(),
                        span: call_span,
                    });
                }
                self.check_numeric_args(callee, args, env)
            }
            "+" | "*" => {
                if args.is_empty() {
                    return Err(TypeError::Arity {
                        name: callee.clone(),
                        expected: 1,
                        got: 0,
                        span: call_span,
                    });
                }
                self.check_numeric_args(callee, args, env)
            }
            "-" => {
                if args.is_empty() {
                    return Err(TypeError::Arity {
                        name: callee.clone(),
                        expected: 1,
                        got: 0,
                        span: call_span,
                    });
                }
                self.check_numeric_args(callee, args, env)
            }
            "<" | "<=" | ">" | ">=" | "=" | "!=" => {
                if args.len() != 2 {
                    return Err(TypeError::Arity {
                        name: callee.clone(),
                        expected: 2,
                        got: args.len(),
                        span: call_span,
                    });
                }
                let a_span = args[0].span;
                let a = self.check_expr(&mut args[0], env)?;
                let b_span = args[1].span;
                let b = self.check_expr(&mut args[1], env)?;
                if a != b {
                    return Err(TypeError::Mismatch {
                        expected: a,
                        found: b,
                        span: b_span,
                    });
                }
                if !(is_numeric(&a) || a == Type::Bool) {
                    return Err(TypeError::BadOperand {
                        op: callee.clone(),
                        ty: a,
                        span: a_span,
                    });
                }
                Ok(Type::Bool)
            }
            "and" | "or" => {
                if args.len() != 2 {
                    return Err(TypeError::Arity {
                        name: callee.clone(),
                        expected: 2,
                        got: args.len(),
                        span: call_span,
                    });
                }
                for a in args.iter_mut() {
                    let s = a.span;
                    let t = self.check_expr(a, env)?;
                    expect(&Type::Bool, &t, s)?;
                }
                Ok(Type::Bool)
            }
            "not" => {
                if args.len() != 1 {
                    return Err(TypeError::Arity {
                        name: callee.clone(),
                        expected: 1,
                        got: args.len(),
                        span: call_span,
                    });
                }
                let s = args[0].span;
                let t = self.check_expr(&mut args[0], env)?;
                expect(&Type::Bool, &t, s)?;
                Ok(Type::Bool)
            }
            "print" | "println" => {
                if args.len() != 1 {
                    return Err(TypeError::Arity {
                        name: callee.clone(),
                        expected: 1,
                        got: args.len(),
                        span: call_span,
                    });
                }
                let s = args[0].span;
                let t = self.check_expr(&mut args[0], env)?;
                match t {
                    Type::Str | Type::I32 | Type::I64 | Type::F32 | Type::F64 | Type::Bool => {
                        Ok(Type::Unit)
                    }
                    other => Err(TypeError::BadOperand {
                        op: callee.clone(),
                        ty: other,
                        span: s,
                    }),
                }
            }
            "str-concat" => {
                if args.len() != 2 {
                    return Err(TypeError::Arity {
                        name: callee.clone(),
                        expected: 2,
                        got: args.len(),
                        span: call_span,
                    });
                }
                for a in args.iter_mut() {
                    let s = a.span;
                    let t = self.check_expr(a, env)?;
                    expect(&Type::Str, &t, s)?;
                }
                Ok(Type::Str)
            }
            "str-len" => {
                if args.len() != 1 {
                    return Err(TypeError::Arity {
                        name: callee.clone(),
                        expected: 1,
                        got: args.len(),
                        span: call_span,
                    });
                }
                let s = args[0].span;
                let t = self.check_expr(&mut args[0], env)?;
                expect(&Type::Str, &t, s)?;
                Ok(Type::I32)
            }
            "aget" => {
                if args.len() != 2 {
                    return Err(TypeError::Arity {
                        name: callee.clone(),
                        expected: 2,
                        got: args.len(),
                        span: call_span,
                    });
                }
                let a_span = args[0].span;
                let at = self.check_expr(&mut args[0], env)?;
                let Type::Array { elem, .. } = at else {
                    return Err(TypeError::BadOperand {
                        op: callee.clone(),
                        ty: at,
                        span: a_span,
                    });
                };
                let i_span = args[1].span;
                let it = self.check_expr(&mut args[1], env)?;
                expect(&Type::I32, &it, i_span)?;
                Ok(*elem)
            }
            "aset!" => {
                if args.len() != 3 {
                    return Err(TypeError::Arity {
                        name: callee.clone(),
                        expected: 3,
                        got: args.len(),
                        span: call_span,
                    });
                }
                let a_span = args[0].span;
                let at = self.check_expr(&mut args[0], env)?;
                let Type::Array { elem, .. } = at else {
                    return Err(TypeError::BadOperand {
                        op: callee.clone(),
                        ty: at,
                        span: a_span,
                    });
                };
                let i_span = args[1].span;
                let it = self.check_expr(&mut args[1], env)?;
                expect(&Type::I32, &it, i_span)?;
                let v_span = args[2].span;
                let vt = self.check_expr(&mut args[2], env)?;
                expect(&elem, &vt, v_span)?;
                Ok(Type::Unit)
            }
            "alen" => {
                if args.len() != 1 {
                    return Err(TypeError::Arity {
                        name: callee.clone(),
                        expected: 1,
                        got: args.len(),
                        span: call_span,
                    });
                }
                let a_span = args[0].span;
                let at = self.check_expr(&mut args[0], env)?;
                if !matches!(at, Type::Array { .. }) {
                    return Err(TypeError::BadOperand {
                        op: callee.clone(),
                        ty: at,
                        span: a_span,
                    });
                }
                Ok(Type::I32)
            }
            _ => {
                if self.generic_fns.contains_key(callee.as_str()) {
                    return self.check_generic_call(callee, args, env, call_span);
                }
                let sig = self
                    .fns
                    .get(callee.as_str())
                    .cloned()
                    .ok_or_else(|| TypeError::UndefinedFn(callee.clone(), call_span))?;
                if sig.params.len() != args.len() {
                    return Err(TypeError::Arity {
                        name: callee.clone(),
                        expected: sig.params.len(),
                        got: args.len(),
                        span: call_span,
                    });
                }
                for (param_ty, arg) in sig.params.iter().zip(args.iter_mut()) {
                    let s = arg.span;
                    let at = self.check_expr(arg, env)?;
                    expect(param_ty, &at, s)?;
                }
                Ok(sig.ret.clone())
            }
        }
    }

    fn check_generic_call(
        &mut self,
        callee: &mut String,
        args: &mut [Expr],
        env: &mut HashMap<String, Type>,
        call_span: Span,
    ) -> Result<Type, TypeError> {
        let g = self.generic_fns[callee.as_str()].clone();
        if g.params.len() != args.len() {
            return Err(TypeError::Arity {
                name: callee.clone(),
                expected: g.params.len(),
                got: args.len(),
                span: call_span,
            });
        }

        let tp_names: HashSet<String> = g.type_params.iter().map(|(n, _)| n.clone()).collect();
        let mut subst: HashMap<String, Type> = HashMap::new();
        let mut arg_tys = Vec::with_capacity(args.len());
        for (param, arg) in g.params.iter().zip(args.iter_mut()) {
            let s = arg.span;
            let at = self.check_expr(arg, env)?;
            unify_type_param(&param.ty, &at, &tp_names, &mut subst, s)?;
            arg_tys.push(at);
        }

        let mut type_args = Vec::with_capacity(g.type_params.len());
        for (name, bound) in &g.type_params {
            let Some(ty) = subst.get(name).cloned() else {
                return Err(TypeError::InferTypeParam(name.clone(), call_span));
            };
            if let Some(trait_name) = bound {
                if !self.impls.contains(&(trait_name.clone(), ty.clone())) {
                    return Err(TypeError::MissingImpl {
                        trait_name: trait_name.clone(),
                        ty,
                        span: call_span,
                    });
                }
            }
            type_args.push(ty);
        }

        let mangled = self.instantiate_generic(&g, &type_args, &subst)?;
        let sig = self.fns[&mangled].clone();
        for (param_ty, (arg, at)) in sig
            .params
            .iter()
            .zip(args.iter().zip(arg_tys.iter()))
        {
            expect(param_ty, at, arg.span)?;
        }
        *callee = mangled;
        Ok(sig.ret)
    }

    /// Clone a generic template with `subst`, type-check the body, and cache it.
    fn instantiate_generic(
        &mut self,
        g: &GenericFunction,
        type_args: &[Type],
        subst: &HashMap<String, Type>,
    ) -> Result<String, TypeError> {
        let key = (g.name.clone(), type_args.to_vec());
        if let Some(mangled) = self.mono_cache.get(&key) {
            return Ok(mangled.clone());
        }

        let mangled = mangle_mono(&g.name, type_args);
        let params: Vec<Param> = g
            .params
            .iter()
            .map(|p| Param {
                name: p.name.clone(),
                ty: subst_type(&p.ty, subst),
                span: p.span,
            })
            .collect();
        let ret = subst_type(&g.ret, subst);
        let mut body = g.body.clone();
        subst_expr(&mut body, subst);

        // Register signature before checking body (allows recursion / mutual calls).
        let sig = FnSig {
            params: params.iter().map(|p| p.ty.clone()).collect(),
            ret: ret.clone(),
        };
        self.fns.insert(mangled.clone(), sig);
        self.mono_cache.insert(key, mangled.clone());

        let mut env: HashMap<String, Type> = HashMap::new();
        for p in &params {
            env.insert(p.name.clone(), p.ty.clone());
        }
        let body_span = body.span;
        let body_ty = self.check_expr(&mut body, &mut env)?;
        expect(&ret, &body_ty, body_span)?;

        self.mono_fns.push(Function {
            name: mangled.clone(),
            params,
            ret,
            body,
            span: g.span,
        });
        Ok(mangled)
    }
}

fn subst_type(ty: &Type, subst: &HashMap<String, Type>) -> Type {
    match ty {
        Type::Named(n) => subst.get(n).cloned().unwrap_or_else(|| ty.clone()),
        Type::Array { elem, len } => Type::Array {
            elem: Box::new(subst_type(elem, subst)),
            len: *len,
        },
        other => other.clone(),
    }
}

fn subst_expr(e: &mut Expr, subst: &HashMap<String, Type>) {
    match &mut e.kind {
        ExprKind::Lit(_) | ExprKind::Var(_) | ExprKind::Break => {}
        ExprKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            subst_expr(cond, subst);
            subst_expr(then_branch, subst);
            subst_expr(else_branch, subst);
        }
        ExprKind::Let { bindings, body } => {
            for b in bindings.iter_mut() {
                b.ty = subst_type(&b.ty, subst);
                subst_expr(&mut b.value, subst);
            }
            subst_expr(body, subst);
        }
        ExprKind::Do(es) => {
            for ex in es.iter_mut() {
                subst_expr(ex, subst);
            }
        }
        ExprKind::Cast { ty, expr } => {
            *ty = subst_type(ty, subst);
            subst_expr(expr, subst);
        }
        ExprKind::Set { value, .. } => subst_expr(value, subst),
        ExprKind::While { cond, body } => {
            subst_expr(cond, subst);
            subst_expr(body, subst);
        }
        ExprKind::Loop { body } => subst_expr(body, subst),
        ExprKind::ArrayLit { elem_ty, elems } => {
            *elem_ty = subst_type(elem_ty, subst);
            for el in elems.iter_mut() {
                subst_expr(el, subst);
            }
        }
        ExprKind::Field { base, .. } => subst_expr(base, subst),
        ExprKind::Match { scrutinee, arms } => {
            subst_expr(scrutinee, subst);
            for arm in arms.iter_mut() {
                subst_expr(&mut arm.body, subst);
            }
        }
        ExprKind::Call { args, .. } => {
            for a in args.iter_mut() {
                subst_expr(a, subst);
            }
        }
    }
    e.ty = None;
}

/// Unify a (possibly generic) parameter type against a concrete argument type.
fn unify_type_param(
    pattern: &Type,
    concrete: &Type,
    type_params: &HashSet<String>,
    subst: &mut HashMap<String, Type>,
    span: Span,
) -> Result<(), TypeError> {
    match pattern {
        Type::Named(n) if type_params.contains(n) => {
            if let Some(prev) = subst.get(n) {
                expect(prev, concrete, span)
            } else {
                subst.insert(n.clone(), concrete.clone());
                Ok(())
            }
        }
        Type::Array {
            elem: pe,
            len: plen,
        } => match concrete {
            Type::Array {
                elem: ce,
                len: clen,
            } if plen == clen => unify_type_param(pe, ce, type_params, subst, span),
            _ => Err(TypeError::Mismatch {
                expected: pattern.clone(),
                found: concrete.clone(),
                span,
            }),
        },
        _ => expect(pattern, concrete, span),
    }
}

fn lit_type(l: &Lit) -> Type {
    match l {
        Lit::Int(_, kind) => match kind {
            Some(IntKind::I32) => Type::I32,
            Some(IntKind::I64) => Type::I64,
            None => Type::I32, // default
        },
        Lit::Float(_, kind) => match kind {
            Some(FloatKind::F32) => Type::F32,
            Some(FloatKind::F64) => Type::F64,
            None => Type::F64, // default
        },
        Lit::Bool(_) => Type::Bool,
        Lit::Str(_) => Type::Str,
    }
}

fn is_numeric(t: &Type) -> bool {
    matches!(t, Type::I32 | Type::I64 | Type::F32 | Type::F64)
}

fn cast_allowed(from: &Type, to: &Type) -> bool {
    is_numeric(from) && is_numeric(to)
}

fn expect(expected: &Type, found: &Type, span: Span) -> Result<(), TypeError> {
    if expected == found {
        Ok(())
    } else {
        Err(TypeError::Mismatch {
            expected: expected.clone(),
            found: found.clone(),
            span,
        })
    }
}
