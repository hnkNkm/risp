//! Type checker.

use crate::ast::*;
use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TypeError {
    #[error("undefined variable {0:?}")]
    UndefinedVar(String, Span),
    #[error("undefined function {0:?}")]
    UndefinedFn(String, Span),
    #[error("type mismatch: expected {expected}, found {found}")]
    Mismatch { expected: Type, found: Type, span: Span },
    #[error("arity mismatch for {name:?}: expected {expected}, got {got}")]
    Arity { name: String, expected: usize, got: usize, span: Span },
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
    #[error("missing main function")]
    NoMain,
}

impl TypeError {
    /// Returns the source span for this error, if it has one.
    pub fn span(&self) -> Option<Span> {
        match self {
            TypeError::UndefinedVar(_, s)
            | TypeError::UndefinedFn(_, s)
            | TypeError::Mismatch { span: s, .. }
            | TypeError::Arity { span: s, .. }
            | TypeError::BadOperand { span: s, .. }
            | TypeError::IntLitOverflow(_, s)
            | TypeError::BadCast { span: s, .. }
            | TypeError::Duplicate(_, s)
            | TypeError::ConstNotLiteral(s)
            | TypeError::AssignConst(_, s) => Some(*s),
            TypeError::NoMain => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FnSig {
    pub params: Vec<Type>,
    pub ret: Type,
}

pub struct TypeCk {
    pub fns: HashMap<String, FnSig>,
    pub consts: HashMap<String, Type>,
}

impl TypeCk {
    pub fn new() -> Self {
        Self { fns: HashMap::new(), consts: HashMap::new() }
    }

    pub fn check(&mut self, prog: &mut Program) -> Result<(), TypeError> {
        self.check_ex(prog, true)
    }

    /// Type-check a program. When `require_main` is false (REPL definitions),
    /// a missing `main` is allowed; a present `main` must still be `[] -> i32`.
    pub fn check_ex(&mut self, prog: &mut Program, require_main: bool) -> Result<(), TypeError> {
        self.fns.clear();
        self.consts.clear();
        // collect signatures first (allow forward references)
        for it in &prog.items {
            match it {
                TopLevel::Function(f) => {
                    if self.fns.contains_key(&f.name) || self.consts.contains_key(&f.name) {
                        return Err(TypeError::Duplicate(f.name.clone(), f.span));
                    }
                    let sig = FnSig {
                        params: f.params.iter().map(|p| p.ty.clone()).collect(),
                        ret: f.ret.clone(),
                    };
                    self.fns.insert(f.name.clone(), sig);
                }
                TopLevel::Const(c) => {
                    if self.fns.contains_key(&c.name) || self.consts.contains_key(&c.name) {
                        return Err(TypeError::Duplicate(c.name.clone(), c.span));
                    }
                    self.consts.insert(c.name.clone(), c.ty.clone());
                }
            }
        }

        // ensure main exists with signature `[] -> i32` (optional in REPL)
        match self.fns.get("main") {
            Some(sig) if sig.params.is_empty() && sig.ret == Type::I32 => {}
            Some(_) => {
                // Find main's span for the diagnostic.
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
                TopLevel::Const(c) => {
                    if !matches!(c.value.kind, ExprKind::Lit(_)) {
                        return Err(TypeError::ConstNotLiteral(c.value.span));
                    }
                    let mut env = HashMap::new();
                    let val_span = c.value.span;
                    let ty = self.check_expr(&mut c.value, &mut env)?;
                    expect(&c.ty, &ty, val_span)?;
                }
            }
        }
        Ok(())
    }

    fn check_expr(&self, e: &mut Expr, env: &mut HashMap<String, Type>) -> Result<Type, TypeError> {
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
            ExprKind::If { cond, then_branch, else_branch } => {
                let cond_span = cond.span;
                let ct = self.check_expr(cond, env)?;
                expect(&Type::Bool, &ct, cond_span)?;
                let tt = self.check_expr(then_branch, env)?;
                let et_span = else_branch.span;
                let et = self.check_expr(else_branch, env)?;
                if tt != et {
                    return Err(TypeError::Mismatch { expected: tt, found: et, span: et_span });
                }
                tt
            }
            ExprKind::Let { bindings, body } => {
                let snapshot: Vec<(String, Option<Type>)> = bindings
                    .iter()
                    .map(|b| (b.name.clone(), env.get(&b.name).cloned()))
                    .collect();
                for b in bindings.iter_mut() {
                    let val_span = b.value.span;
                    let vt = self.check_expr(&mut b.value, env)?;
                    expect(&b.ty, &vt, val_span)?;
                    env.insert(b.name.clone(), b.ty.clone());
                }
                let bt = self.check_expr(body, env)?;
                // restore shadowed bindings
                for (name, prev) in snapshot {
                    match prev {
                        Some(t) => { env.insert(name, t); }
                        None => { env.remove(&name); }
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
                let _ = self.check_expr(body, env)?;
                Type::Unit
            }
            ExprKind::Call { callee, args } => {
                let callee = callee.clone();
                self.check_call(&callee, args, env, span)?
            }
        };
        e.ty = Some(ty.clone());
        Ok(ty)
    }

    /// Check that all args are the same numeric type; return that type.
    fn check_numeric_args(
        &self,
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
        &self,
        callee: &str,
        args: &mut [Expr],
        env: &mut HashMap<String, Type>,
        call_span: Span,
    ) -> Result<Type, TypeError> {
        // builtins first
        match callee {
            // `/` `mod` are binary-only; `+` `*` need ≥1 args; `-` is unary or n-ary (≥1).
            "/" | "mod" => {
                if args.len() != 2 {
                    return Err(TypeError::Arity {
                        name: callee.into(),
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
                        name: callee.into(),
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
                        name: callee.into(),
                        expected: 1,
                        got: 0,
                        span: call_span,
                    });
                }
                self.check_numeric_args(callee, args, env)
            }
            "<" | "<=" | ">" | ">=" | "=" | "!=" => {
                if args.len() != 2 {
                    return Err(TypeError::Arity { name: callee.into(), expected: 2, got: args.len(), span: call_span });
                }
                let a_span = args[0].span;
                let a = self.check_expr(&mut args[0], env)?;
                let b_span = args[1].span;
                let b = self.check_expr(&mut args[1], env)?;
                if a != b {
                    return Err(TypeError::Mismatch { expected: a, found: b, span: b_span });
                }
                if !(is_numeric(&a) || a == Type::Bool) {
                    return Err(TypeError::BadOperand { op: callee.into(), ty: a, span: a_span });
                }
                Ok(Type::Bool)
            }
            "and" | "or" => {
                if args.len() != 2 {
                    return Err(TypeError::Arity { name: callee.into(), expected: 2, got: args.len(), span: call_span });
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
                    return Err(TypeError::Arity { name: callee.into(), expected: 1, got: args.len(), span: call_span });
                }
                let s = args[0].span;
                let t = self.check_expr(&mut args[0], env)?;
                expect(&Type::Bool, &t, s)?;
                Ok(Type::Bool)
            }
            "print" | "println" => {
                if args.len() != 1 {
                    return Err(TypeError::Arity { name: callee.into(), expected: 1, got: args.len(), span: call_span });
                }
                let s = args[0].span;
                let t = self.check_expr(&mut args[0], env)?;
                match t {
                    Type::Str | Type::I32 | Type::I64 | Type::F32 | Type::F64 | Type::Bool => {
                        Ok(Type::Unit)
                    }
                    other => Err(TypeError::BadOperand {
                        op: callee.into(),
                        ty: other,
                        span: s,
                    }),
                }
            }
            // user-defined function
            _ => {
                let sig = self
                    .fns
                    .get(callee)
                    .cloned()
                    .ok_or_else(|| TypeError::UndefinedFn(callee.to_string(), call_span))?;
                if sig.params.len() != args.len() {
                    return Err(TypeError::Arity {
                        name: callee.into(),
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
        Err(TypeError::Mismatch { expected: expected.clone(), found: found.clone(), span })
    }
}
