//! Module resolution for `(module …)` / `(import …)`.
//!
//! Pipeline position: parse → **resolve** → macroexpand → typeck → codegen.
//!
//! - One file = one module. Optional `(module name)` as the first top-level form;
//!   otherwise the module name is the file stem.
//! - `(import name)` loads `name.rsp` from the importer's directory, then the
//!   entry file's directory.
//! - Imported globals are prefixed with `mod/` (e.g. `add` → `math/add`) and
//!   merged into the entry program. Import cycles are rejected.

use crate::ast::*;
use crate::parser::{self, FrontendError};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("import not supported in REPL")]
    ImportInRepl,
    #[error("(module …) must be the first top-level form")]
    ModuleNotFirst(Span),
    #[error("module name mismatch: imported as {imported:?}, declared {declared:?}")]
    NameMismatch {
        imported: String,
        declared: String,
        span: Span,
    },
    #[error("circular import: {}", .0.join(" -> "))]
    Cycle(Vec<String>),
    #[error("module `{name}` not found (searched {})", searched.join(", "))]
    NotFound {
        name: String,
        searched: Vec<String>,
        span: Span,
    },
    #[error("could not read {}: {msg}", path.display())]
    Io {
        path: PathBuf,
        msg: String,
        span: Span,
    },
    /// Parse/lex error in an imported file (or, rarely, re-parse of entry).
    #[error("{err}")]
    Frontend {
        file: String,
        src: String,
        err: FrontendError,
    },
}

impl ResolveError {
    /// Span in the *entry* (or importer) file, when applicable.
    pub fn span(&self) -> Option<Span> {
        match self {
            ResolveError::ModuleNotFirst(s)
            | ResolveError::NameMismatch { span: s, .. }
            | ResolveError::NotFound { span: s, .. }
            | ResolveError::Io { span: s, .. } => Some(*s),
            ResolveError::ImportInRepl | ResolveError::Cycle(_) | ResolveError::Frontend { .. } => {
                None
            }
        }
    }
}

/// Resolve imports in `prog` (entry file at `entry_path` with source `entry_src`).
/// Strips `(module …)` / `(import …)` and merges imported items (name-prefixed).
pub fn resolve(prog: &mut Program, entry_path: &Path, _entry_src: &str) -> Result<(), ResolveError> {
    let entry_dir = entry_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let entry_stem = entry_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("main");

    let (mod_name, imports, mut items) =
        split_module(std::mem::take(&mut prog.items), entry_stem, true)?;

    // Canonical paths already fully loaded (deps merged).
    let mut loaded: HashSet<PathBuf> = HashSet::new();
    let mut stack: Vec<String> = Vec::new();
    // Entry module name on the stack so `import` of self is a cycle.
    stack.push(mod_name.clone());

    let mut merged = Vec::new();
    for (imp, span) in &imports {
        load_module(
            imp,
            &entry_dir,
            &entry_dir,
            *span,
            &mut stack,
            &mut loaded,
            &mut merged,
        )?;
    }
    stack.pop();

    merged.append(&mut items);
    prog.items = merged;
    Ok(())
}

/// REPL: reject `(import …)`; strip `(module …)`.
pub fn prepare_repl(prog: &mut Program) -> Result<(), ResolveError> {
    let mut kept = Vec::with_capacity(prog.items.len());
    for it in prog.items.drain(..) {
        match it {
            TopLevel::Import { .. } => return Err(ResolveError::ImportInRepl),
            TopLevel::Module { .. } => {}
            other => kept.push(other),
        }
    }
    prog.items = kept;
    Ok(())
}

fn load_module(
    name: &str,
    from_dir: &Path,
    entry_dir: &Path,
    at: Span,
    stack: &mut Vec<String>,
    loaded: &mut HashSet<PathBuf>,
    merged: &mut Vec<TopLevel>,
) -> Result<(), ResolveError> {
    if stack.iter().any(|s| s == name) {
        let mut cycle = stack.clone();
        cycle.push(name.to_string());
        return Err(ResolveError::Cycle(cycle));
    }

    let path = find_module(name, from_dir, entry_dir, at)?;
    let key = canonicalize_key(&path);

    if loaded.contains(&key) {
        return Ok(());
    }

    let src = fs::read_to_string(&path).map_err(|e| ResolveError::Io {
        path: path.clone(),
        msg: e.to_string(),
        span: at,
    })?;
    let file = path.display().to_string();
    let parsed = parser::parse(&src).map_err(|err| ResolveError::Frontend {
        file: file.clone(),
        src: src.clone(),
        err,
    })?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    let (declared, imports, items) = split_module(parsed.items, stem, true)?;
    if declared != name {
        return Err(ResolveError::NameMismatch {
            imported: name.to_string(),
            declared,
            span: at,
        });
    }

    stack.push(name.to_string());
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(from_dir);

    for (imp, span) in &imports {
        load_module(imp, parent, entry_dir, *span, stack, loaded, merged)?;
    }

    merged.extend(prefix_items(&declared, items));
    loaded.insert(key);
    stack.pop();
    Ok(())
}

fn find_module(
    name: &str,
    from_dir: &Path,
    entry_dir: &Path,
    span: Span,
) -> Result<PathBuf, ResolveError> {
    let mut searched = Vec::new();
    for dir in [from_dir, entry_dir] {
        let cand = dir.join(format!("{name}.rsp"));
        searched.push(cand.display().to_string());
        if cand.is_file() {
            return Ok(cand);
        }
    }
    Err(ResolveError::NotFound {
        name: name.to_string(),
        searched,
        span,
    })
}

fn canonicalize_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Split out module/import forms. If `require_module_first`, `(module …)` may
/// only appear as the first item.
fn split_module(
    raw: Vec<TopLevel>,
    file_stem: &str,
    require_module_first: bool,
) -> Result<(String, Vec<(String, Span)>, Vec<TopLevel>), ResolveError> {
    let mut mod_name: Option<String> = None;
    let mut imports = Vec::new();
    let mut items = Vec::new();
    let mut seen_non_module = false;

    for it in raw {
        match it {
            TopLevel::Module { name, span } => {
                if require_module_first && (seen_non_module || mod_name.is_some()) {
                    return Err(ResolveError::ModuleNotFirst(span));
                }
                if mod_name.is_some() {
                    return Err(ResolveError::ModuleNotFirst(span));
                }
                mod_name = Some(name);
            }
            TopLevel::Import { name, span } => {
                seen_non_module = true;
                imports.push((name, span));
            }
            other => {
                seen_non_module = true;
                items.push(other);
            }
        }
    }

    let name = mod_name.unwrap_or_else(|| file_stem.to_string());
    Ok((name, imports, items))
}

fn prefix_items(mod_name: &str, mut items: Vec<TopLevel>) -> Vec<TopLevel> {
    let globals = collect_globals(&items);
    for it in &mut items {
        prefix_toplevel(it, mod_name, &globals);
    }
    items
}

fn collect_globals(items: &[TopLevel]) -> HashSet<String> {
    let mut g = HashSet::new();
    for it in items {
        match it {
            TopLevel::Function(f) => {
                g.insert(f.name.clone());
            }
            TopLevel::GenericFunction(f) => {
                g.insert(f.name.clone());
            }
            TopLevel::Const(c) => {
                g.insert(c.name.clone());
            }
            TopLevel::Extern(e) => {
                g.insert(e.name.clone());
            }
            TopLevel::Struct(s) => {
                g.insert(s.name.clone());
            }
            TopLevel::Enum(e) => {
                g.insert(e.name.clone());
                for v in &e.variants {
                    g.insert(v.name.clone());
                }
            }
            TopLevel::Trait(t) => {
                g.insert(t.name.clone());
            }
            TopLevel::DefMacro(m) => {
                g.insert(m.name.clone());
            }
            TopLevel::Impl(_) | TopLevel::Module { .. } | TopLevel::Import { .. } => {}
        }
    }
    g
}

fn qualify(mod_name: &str, name: &str) -> String {
    format!("{mod_name}/{name}")
}

fn rename_if_global(name: &mut String, mod_name: &str, globals: &HashSet<String>) {
    if globals.contains(name.as_str()) {
        *name = qualify(mod_name, name);
    }
}

fn prefix_toplevel(it: &mut TopLevel, mod_name: &str, globals: &HashSet<String>) {
    match it {
        TopLevel::Function(f) => {
            rename_if_global(&mut f.name, mod_name, globals);
            for p in &mut f.params {
                rename_type(&mut p.ty, mod_name, globals);
            }
            rename_type(&mut f.ret, mod_name, globals);
            let mut locals = HashSet::new();
            for p in &f.params {
                locals.insert(p.name.clone());
            }
            rename_expr(&mut f.body, mod_name, globals, &mut locals);
        }
        TopLevel::GenericFunction(g) => {
            rename_if_global(&mut g.name, mod_name, globals);
            for (_tp, bound) in &mut g.type_params {
                if let Some(b) = bound {
                    rename_if_global(b, mod_name, globals);
                }
            }
            let mut type_params: HashSet<String> =
                g.type_params.iter().map(|(n, _)| n.clone()).collect();
            for p in &mut g.params {
                rename_type_with_params(&mut p.ty, mod_name, globals, &type_params);
            }
            rename_type_with_params(&mut g.ret, mod_name, globals, &type_params);
            let mut locals = HashSet::new();
            for p in &g.params {
                locals.insert(p.name.clone());
            }
            // Type params are not value locals; keep them out of value rename.
            let _ = &mut type_params;
            rename_expr(&mut g.body, mod_name, globals, &mut locals);
        }
        TopLevel::Const(c) => {
            rename_if_global(&mut c.name, mod_name, globals);
            rename_type(&mut c.ty, mod_name, globals);
            let mut locals = HashSet::new();
            rename_expr(&mut c.value, mod_name, globals, &mut locals);
        }
        TopLevel::Extern(e) => {
            rename_if_global(&mut e.name, mod_name, globals);
            for p in &mut e.params {
                rename_type(&mut p.ty, mod_name, globals);
            }
            rename_type(&mut e.ret, mod_name, globals);
        }
        TopLevel::Struct(s) => {
            rename_if_global(&mut s.name, mod_name, globals);
            for f in &mut s.fields {
                rename_type(&mut f.ty, mod_name, globals);
            }
        }
        TopLevel::Enum(e) => {
            rename_if_global(&mut e.name, mod_name, globals);
            for v in &mut e.variants {
                rename_if_global(&mut v.name, mod_name, globals);
                if let Some(p) = &mut v.payload {
                    rename_type(p, mod_name, globals);
                }
            }
        }
        TopLevel::Trait(t) => {
            rename_if_global(&mut t.name, mod_name, globals);
            for m in &mut t.methods {
                for p in &mut m.params {
                    rename_type(&mut p.ty, mod_name, globals);
                }
                rename_type(&mut m.ret, mod_name, globals);
            }
        }
        TopLevel::Impl(ib) => {
            rename_if_global(&mut ib.trait_name, mod_name, globals);
            rename_type(&mut ib.for_ty, mod_name, globals);
            for m in &mut ib.methods {
                for p in &mut m.params {
                    rename_type(&mut p.ty, mod_name, globals);
                }
                rename_type(&mut m.ret, mod_name, globals);
                let mut locals = HashSet::new();
                for p in &m.params {
                    locals.insert(p.name.clone());
                }
                rename_expr(&mut m.body, mod_name, globals, &mut locals);
            }
        }
        TopLevel::DefMacro(m) => {
            rename_if_global(&mut m.name, mod_name, globals);
            // Macro params are binding names in the template; treat as locals.
            let mut locals: HashSet<String> = m.params.iter().cloned().collect();
            rename_expr(&mut m.template, mod_name, globals, &mut locals);
        }
        TopLevel::Module { .. } | TopLevel::Import { .. } => {}
    }
}

fn rename_type(ty: &mut Type, mod_name: &str, globals: &HashSet<String>) {
    rename_type_with_params(ty, mod_name, globals, &HashSet::new())
}

fn rename_type_with_params(
    ty: &mut Type,
    mod_name: &str,
    globals: &HashSet<String>,
    type_params: &HashSet<String>,
) {
    match ty {
        Type::Named(n) => {
            if !type_params.contains(n.as_str()) {
                rename_if_global(n, mod_name, globals);
            }
        }
        Type::Array { elem, .. } => rename_type_with_params(elem, mod_name, globals, type_params),
        Type::I32
        | Type::I64
        | Type::F32
        | Type::F64
        | Type::Bool
        | Type::Str
        | Type::Unit => {}
    }
}

fn rename_expr(
    expr: &mut Expr,
    mod_name: &str,
    globals: &HashSet<String>,
    locals: &mut HashSet<String>,
) {
    match &mut expr.kind {
        ExprKind::Lit(_) | ExprKind::Break => {}
        ExprKind::Var(name) => {
            if !locals.contains(name.as_str()) {
                rename_if_global(name, mod_name, globals);
            }
        }
        ExprKind::Set { name, value } => {
            // set! targets locals/params only; still rename the value.
            let _ = name;
            rename_expr(value, mod_name, globals, locals);
        }
        ExprKind::Call { callee, args } => {
            if !locals.contains(callee.as_str()) {
                rename_if_global(callee, mod_name, globals);
            }
            for a in args {
                rename_expr(a, mod_name, globals, locals);
            }
        }
        ExprKind::If {
            cond,
            then_branch,
            else_branch,
        } => {
            rename_expr(cond, mod_name, globals, locals);
            rename_expr(then_branch, mod_name, globals, locals);
            rename_expr(else_branch, mod_name, globals, locals);
        }
        ExprKind::Let { bindings, body } => {
            for b in bindings.iter_mut() {
                rename_type(&mut b.ty, mod_name, globals);
                rename_expr(&mut b.value, mod_name, globals, locals);
            }
            let mut inner = locals.clone();
            for b in bindings.iter() {
                inner.insert(b.name.clone());
            }
            rename_expr(body, mod_name, globals, &mut inner);
        }
        ExprKind::Do(es) => {
            for e in es {
                rename_expr(e, mod_name, globals, locals);
            }
        }
        ExprKind::Cast { ty, expr } => {
            rename_type(ty, mod_name, globals);
            rename_expr(expr, mod_name, globals, locals);
        }
        ExprKind::While { cond, body } => {
            rename_expr(cond, mod_name, globals, locals);
            rename_expr(body, mod_name, globals, locals);
        }
        ExprKind::Loop { body } => rename_expr(body, mod_name, globals, locals),
        ExprKind::ArrayLit { elem_ty, elems } => {
            rename_type(elem_ty, mod_name, globals);
            for e in elems {
                rename_expr(e, mod_name, globals, locals);
            }
        }
        ExprKind::Field { base, field: _ } => {
            rename_expr(base, mod_name, globals, locals);
        }
        ExprKind::Match { scrutinee, arms } => {
            rename_expr(scrutinee, mod_name, globals, locals);
            for arm in arms {
                rename_if_global(&mut arm.variant, mod_name, globals);
                let mut inner = locals.clone();
                if let Some(b) = &arm.binding {
                    inner.insert(b.clone());
                }
                rename_expr(&mut arm.body, mod_name, globals, &mut inner);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;

    #[test]
    fn prefix_function_and_call() {
        let src = r#"
        (defn helper [] -> i32 1)
        (defn add [x: i32, y: i32] -> i32 (+ x (helper)))
        "#;
        let prog = parser::parse(src).unwrap();
        let items = prefix_items("math", prog.items);
        match &items[0] {
            TopLevel::Function(f) => assert_eq!(f.name, "math/helper"),
            other => panic!("{other:?}"),
        }
        match &items[1] {
            TopLevel::Function(f) => {
                assert_eq!(f.name, "math/add");
                match &f.body.kind {
                    ExprKind::Call { args, .. } => match &args[1].kind {
                        ExprKind::Call { callee, .. } => assert_eq!(callee, "math/helper"),
                        other => panic!("{other:?}"),
                    },
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn prepare_repl_rejects_import() {
        let src = "(import math)\n(defn main [] -> i32 0)";
        let mut prog = parser::parse(src).unwrap();
        let err = prepare_repl(&mut prog).unwrap_err();
        assert!(matches!(err, ResolveError::ImportInRepl));
    }
}
