//! Type checker.

use crate::ast::*;
use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TypeError {
    #[error("undefined variable {0:?}")]
    UndefinedVar(String),
    #[error("undefined function {0:?}")]
    UndefinedFn(String),
    #[error("type mismatch: expected {expected}, found {found}")]
    Mismatch { expected: Type, found: Type },
    #[error("arity mismatch for {name:?}: expected {expected}, got {got}")]
    Arity { name: String, expected: usize, got: usize },
    #[error("operator {op:?} cannot be applied to {ty}")]
    BadOperand { op: String, ty: Type },
    #[error("integer literal does not fit type {0}")]
    IntLitOverflow(Type),
    #[error("missing main function")]
    NoMain,
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

    pub fn check(&mut self, prog: &Program) -> Result<(), TypeError> {
        // collect signatures first (allow forward references)
        for it in &prog.items {
            match it {
                TopLevel::Function(f) => {
                    let sig = FnSig {
                        params: f.params.iter().map(|p| p.ty.clone()).collect(),
                        ret: f.ret.clone(),
                    };
                    self.fns.insert(f.name.clone(), sig);
                }
                TopLevel::Const(c) => {
                    self.consts.insert(c.name.clone(), c.ty.clone());
                }
            }
        }

        // ensure main exists with signature `[] -> i32`
        match self.fns.get("main") {
            Some(sig) if sig.params.is_empty() && sig.ret == Type::I32 => {}
            Some(_) => return Err(TypeError::Mismatch {
                expected: Type::I32,
                found: self.fns["main"].ret.clone(),
            }),
            None => return Err(TypeError::NoMain),
        }

        for it in &prog.items {
            match it {
                TopLevel::Function(f) => {
                    let mut env: HashMap<String, Type> = HashMap::new();
                    for p in &f.params {
                        env.insert(p.name.clone(), p.ty.clone());
                    }
                    let body_ty = self.check_expr(&f.body, &mut env)?;
                    expect(&f.ret, &body_ty)?;
                }
                TopLevel::Const(c) => {
                    let mut env = HashMap::new();
                    let ty = self.check_expr(&c.value, &mut env)?;
                    expect(&c.ty, &ty)?;
                }
            }
        }
        Ok(())
    }

    fn check_expr(&self, e: &Expr, env: &mut HashMap<String, Type>) -> Result<Type, TypeError> {
        match &e.kind {
            ExprKind::Lit(l) => Ok(lit_type(l)),
            ExprKind::Var(name) => {
                if let Some(t) = env.get(name) {
                    Ok(t.clone())
                } else if let Some(t) = self.consts.get(name) {
                    Ok(t.clone())
                } else {
                    Err(TypeError::UndefinedVar(name.clone()))
                }
            }
            ExprKind::If { cond, then_branch, else_branch } => {
                let ct = self.check_expr(cond, env)?;
                expect(&Type::Bool, &ct)?;
                let tt = self.check_expr(then_branch, env)?;
                let et = self.check_expr(else_branch, env)?;
                if tt != et {
                    return Err(TypeError::Mismatch { expected: tt, found: et });
                }
                Ok(tt)
            }
            ExprKind::Let { bindings, body } => {
                let snapshot: Vec<(String, Option<Type>)> = bindings
                    .iter()
                    .map(|b| (b.name.clone(), env.get(&b.name).cloned()))
                    .collect();
                for b in bindings {
                    let vt = self.check_expr(&b.value, env)?;
                    expect(&b.ty, &vt)?;
                    env.insert(b.name.clone(), b.ty.clone());
                }
                let bt = self.check_expr(body, env)?;
                // restore
                for (name, prev) in snapshot {
                    match prev {
                        Some(t) => { env.insert(name, t); }
                        None => { env.remove(&name); }
                    }
                }
                Ok(bt)
            }
            ExprKind::Do(exprs) => {
                let mut last = Type::Unit;
                for ex in exprs {
                    last = self.check_expr(ex, env)?;
                }
                Ok(last)
            }
            ExprKind::Call { callee, args } => self.check_call(callee, args, env, &e.span),
        }
    }

    fn check_call(
        &self,
        callee: &str,
        args: &[Expr],
        env: &mut HashMap<String, Type>,
        _span: &Span,
    ) -> Result<Type, TypeError> {
        // builtins first
        match callee {
            "+" | "-" | "*" | "/" | "mod" => {
                if args.len() != 2 {
                    return Err(TypeError::Arity { name: callee.into(), expected: 2, got: args.len() });
                }
                let a = self.check_expr(&args[0], env)?;
                let b = self.check_expr(&args[1], env)?;
                if a != b {
                    return Err(TypeError::Mismatch { expected: a, found: b });
                }
                if !is_numeric(&a) {
                    return Err(TypeError::BadOperand { op: callee.into(), ty: a });
                }
                Ok(a)
            }
            "<" | "<=" | ">" | ">=" | "=" | "!=" => {
                if args.len() != 2 {
                    return Err(TypeError::Arity { name: callee.into(), expected: 2, got: args.len() });
                }
                let a = self.check_expr(&args[0], env)?;
                let b = self.check_expr(&args[1], env)?;
                if a != b {
                    return Err(TypeError::Mismatch { expected: a, found: b });
                }
                if !(is_numeric(&a) || a == Type::Bool) {
                    return Err(TypeError::BadOperand { op: callee.into(), ty: a });
                }
                Ok(Type::Bool)
            }
            "and" | "or" => {
                if args.len() != 2 {
                    return Err(TypeError::Arity { name: callee.into(), expected: 2, got: args.len() });
                }
                for a in args {
                    let t = self.check_expr(a, env)?;
                    expect(&Type::Bool, &t)?;
                }
                Ok(Type::Bool)
            }
            "not" => {
                if args.len() != 1 {
                    return Err(TypeError::Arity { name: callee.into(), expected: 1, got: args.len() });
                }
                let t = self.check_expr(&args[0], env)?;
                expect(&Type::Bool, &t)?;
                Ok(Type::Bool)
            }
            "print" | "println" => {
                if args.len() != 1 {
                    return Err(TypeError::Arity { name: callee.into(), expected: 1, got: args.len() });
                }
                let t = self.check_expr(&args[0], env)?;
                expect(&Type::Str, &t)?;
                Ok(Type::Unit)
            }
            // user-defined function
            _ => {
                let sig = self
                    .fns
                    .get(callee)
                    .cloned()
                    .ok_or_else(|| TypeError::UndefinedFn(callee.to_string()))?;
                if sig.params.len() != args.len() {
                    return Err(TypeError::Arity {
                        name: callee.into(),
                        expected: sig.params.len(),
                        got: args.len(),
                    });
                }
                for (param_ty, arg) in sig.params.iter().zip(args) {
                    let at = self.check_expr(arg, env)?;
                    expect(param_ty, &at)?;
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

fn expect(expected: &Type, found: &Type) -> Result<(), TypeError> {
    if expected == found {
        Ok(())
    } else {
        Err(TypeError::Mismatch { expected: expected.clone(), found: found.clone() })
    }
}
