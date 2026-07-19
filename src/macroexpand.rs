//! Naive syntactic macro expansion (`defmacro`).
//!
//! Collects `TopLevel::DefMacro` items, strips them from the program, then
//! expands `Call` forms whose callee is a macro name by substituting argument
//! expressions into the template (unhygienic Var replacement).

use crate::ast::*;
use std::collections::HashMap;
use thiserror::Error;

const MAX_EXPAND_DEPTH: u32 = 64;

#[derive(Debug, Error)]
pub enum MacroError {
    #[error("duplicate macro definition {0:?}")]
    Duplicate(String, Span),
    #[error("macro arity mismatch for {name:?}: expected {expected}, got {got}")]
    Arity {
        name: String,
        expected: usize,
        got: usize,
        span: Span,
    },
    #[error("macro expansion exceeded {0} iterations (possible recursive macro)")]
    ExpansionLimit(u32, Span),
}

impl MacroError {
    pub fn span(&self) -> Option<Span> {
        match self {
            MacroError::Duplicate(_, s)
            | MacroError::Arity { span: s, .. }
            | MacroError::ExpansionLimit(_, s) => Some(*s),
        }
    }
}

/// Expand all macros in `prog` in place. Removes `DefMacro` items.
pub fn expand(prog: &mut Program) -> Result<(), MacroError> {
    let mut macros: HashMap<String, MacroDef> = HashMap::new();
    let mut kept = Vec::with_capacity(prog.items.len());
    for it in prog.items.drain(..) {
        match it {
            TopLevel::DefMacro(m) => {
                if macros.contains_key(&m.name) {
                    return Err(MacroError::Duplicate(m.name, m.span));
                }
                macros.insert(m.name.clone(), m);
            }
            other => kept.push(other),
        }
    }
    prog.items = kept;

    if macros.is_empty() {
        return Ok(());
    }

    for it in &mut prog.items {
        match it {
            TopLevel::Function(f) => expand_expr(&mut f.body, &macros, 0)?,
            TopLevel::GenericFunction(g) => expand_expr(&mut g.body, &macros, 0)?,
            TopLevel::Const(c) => expand_expr(&mut c.value, &macros, 0)?,
            TopLevel::Impl(ib) => {
                for m in &mut ib.methods {
                    expand_expr(&mut m.body, &macros, 0)?;
                }
            }
            TopLevel::Struct(_)
            | TopLevel::Enum(_)
            | TopLevel::Extern(_)
            | TopLevel::Trait(_)
            | TopLevel::DefMacro(_)
            | TopLevel::Module { .. }
            | TopLevel::Import { .. } => {}
        }
    }
    Ok(())
}

fn expand_expr(
    expr: &mut Expr,
    macros: &HashMap<String, MacroDef>,
    depth: u32,
) -> Result<(), MacroError> {
    // Expand children / handle Call first.
    match &mut expr.kind {
        ExprKind::Lit(_) | ExprKind::Var(_) | ExprKind::Break => {}
        ExprKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            expand_expr(cond, macros, depth)?;
            expand_expr(then_branch, macros, depth)?;
            expand_expr(else_branch, macros, depth)?;
        }
        ExprKind::Let { bindings, body } => {
            for b in bindings.iter_mut() {
                expand_expr(&mut b.value, macros, depth)?;
            }
            expand_expr(body, macros, depth)?;
        }
        ExprKind::Do(exprs) => {
            for e in exprs.iter_mut() {
                expand_expr(e, macros, depth)?;
            }
        }
        ExprKind::Cast { expr: inner, .. } => expand_expr(inner, macros, depth)?,
        ExprKind::Set { value, .. } => expand_expr(value, macros, depth)?,
        ExprKind::While { cond, body } => {
            expand_expr(cond, macros, depth)?;
            expand_expr(body, macros, depth)?;
        }
        ExprKind::Loop { body } => expand_expr(body, macros, depth)?,
        ExprKind::ArrayLit { elems, .. } => {
            for e in elems.iter_mut() {
                expand_expr(e, macros, depth)?;
            }
        }
        ExprKind::Field { base, .. } => expand_expr(base, macros, depth)?,
        ExprKind::Match { scrutinee, arms } => {
            expand_expr(scrutinee, macros, depth)?;
            for arm in arms.iter_mut() {
                expand_expr(&mut arm.body, macros, depth)?;
            }
        }
        ExprKind::Call { callee, args } => {
            if let Some(mac) = macros.get(callee) {
                if args.len() != mac.params.len() {
                    return Err(MacroError::Arity {
                        name: callee.clone(),
                        expected: mac.params.len(),
                        got: args.len(),
                        span: expr.span,
                    });
                }
                if depth >= MAX_EXPAND_DEPTH {
                    return Err(MacroError::ExpansionLimit(MAX_EXPAND_DEPTH, expr.span));
                }
                let args = std::mem::take(args);
                let mut expanded = substitute(&mac.template, &mac.params, &args);
                // Preserve call-site span on the outermost substituted form.
                expanded.span = expr.span;
                *expr = expanded;
                expand_expr(expr, macros, depth + 1)?;
                return Ok(());
            }
            for a in args.iter_mut() {
                expand_expr(a, macros, depth)?;
            }
        }
    }
    Ok(())
}

/// Deep-clone `template`, replacing `Var` nodes whose name is a macro param
/// with the corresponding argument expression.
fn substitute(template: &Expr, params: &[String], args: &[Expr]) -> Expr {
    let map: HashMap<&str, &Expr> = params
        .iter()
        .zip(args.iter())
        .map(|(p, a)| (p.as_str(), a))
        .collect();
    subst_expr(template, &map)
}

fn subst_expr(expr: &Expr, map: &HashMap<&str, &Expr>) -> Expr {
    let kind = match &expr.kind {
        ExprKind::Lit(l) => ExprKind::Lit(l.clone()),
        ExprKind::Var(name) => {
            if let Some(arg) = map.get(name.as_str()) {
                return (*arg).clone();
            }
            ExprKind::Var(name.clone())
        }
        ExprKind::Break => ExprKind::Break,
        ExprKind::If {
            cond,
            then_branch,
            else_branch,
        } => ExprKind::If {
            cond: Box::new(subst_expr(cond, map)),
            then_branch: Box::new(subst_expr(then_branch, map)),
            else_branch: Box::new(subst_expr(else_branch, map)),
        },
        ExprKind::Let { bindings, body } => ExprKind::Let {
            bindings: bindings
                .iter()
                .map(|b| Binding {
                    name: b.name.clone(),
                    ty: b.ty.clone(),
                    value: subst_expr(&b.value, map),
                    span: b.span,
                })
                .collect(),
            body: Box::new(subst_expr(body, map)),
        },
        ExprKind::Do(exprs) => ExprKind::Do(exprs.iter().map(|e| subst_expr(e, map)).collect()),
        ExprKind::Cast { ty, expr: inner } => ExprKind::Cast {
            ty: ty.clone(),
            expr: Box::new(subst_expr(inner, map)),
        },
        ExprKind::Set { name, value } => ExprKind::Set {
            name: name.clone(),
            value: Box::new(subst_expr(value, map)),
        },
        ExprKind::While { cond, body } => ExprKind::While {
            cond: Box::new(subst_expr(cond, map)),
            body: Box::new(subst_expr(body, map)),
        },
        ExprKind::Loop { body } => ExprKind::Loop {
            body: Box::new(subst_expr(body, map)),
        },
        ExprKind::ArrayLit { elem_ty, elems } => ExprKind::ArrayLit {
            elem_ty: elem_ty.clone(),
            elems: elems.iter().map(|e| subst_expr(e, map)).collect(),
        },
        ExprKind::Field { base, field } => ExprKind::Field {
            base: Box::new(subst_expr(base, map)),
            field: field.clone(),
        },
        ExprKind::Match { scrutinee, arms } => ExprKind::Match {
            scrutinee: Box::new(subst_expr(scrutinee, map)),
            arms: arms
                .iter()
                .map(|a| MatchArm {
                    variant: a.variant.clone(),
                    binding: a.binding.clone(),
                    body: subst_expr(&a.body, map),
                    span: a.span,
                })
                .collect(),
        },
        ExprKind::Call { callee, args } => ExprKind::Call {
            callee: callee.clone(),
            args: args.iter().map(|a| subst_expr(a, map)).collect(),
        },
    };
    Expr {
        kind,
        span: expr.span,
        ty: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    #[test]
    fn expand_when_macro() {
        let src = r#"
(defmacro when [cond body]
  (if cond body (do)))

(defn main [] -> i32
  (do
    (when true 1)
    0))
"#;
        let mut prog = parser::parse(src).unwrap();
        expand(&mut prog).unwrap();
        assert!(prog.items.iter().all(|it| !matches!(it, TopLevel::DefMacro(_))));
        let TopLevel::Function(f) = &prog.items[0] else {
            panic!("expected function");
        };
        // Body should contain an `if` (from when), not a call to `when`.
        let ExprKind::Do(exprs) = &f.body.kind else {
            panic!("expected do");
        };
        assert!(matches!(exprs[0].kind, ExprKind::If { .. }));
    }

    #[test]
    fn macro_arity_error() {
        let src = r#"
(defmacro when [cond body]
  (if cond body (do)))

(defn main [] -> i32
  (when true))
"#;
        let mut prog = parser::parse(src).unwrap();
        let err = expand(&mut prog).unwrap_err();
        assert!(matches!(err, MacroError::Arity { expected: 2, got: 1, .. }));
    }
}
