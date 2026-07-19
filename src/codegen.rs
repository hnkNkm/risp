//! LLVM IR codegen using inkwell.

use crate::ast::*;
use crate::typeck::TypeCk;
use inkwell::AddressSpace;
use inkwell::FloatPredicate;
use inkwell::IntPredicate;
use inkwell::basic_block::BasicBlock;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::types::{BasicMetadataTypeEnum, BasicTypeEnum, FunctionType};
use inkwell::values::{BasicMetadataValueEnum, BasicValueEnum, FunctionValue, PointerValue};
use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CodegenError {
    #[error("LLVM error: {0}")]
    Llvm(String),
    #[error("internal: {0}")]
    Internal(String),
}

pub struct Codegen<'ctx> {
    pub context: &'ctx Context,
    pub module: Module<'ctx>,
    pub builder: Builder<'ctx>,
    fns: HashMap<String, FunctionValue<'ctx>>,
    fn_types: HashMap<String, (Vec<Type>, Type)>,
    consts: HashMap<String, (Type, BasicValueEnum<'ctx>)>,
    /// per-function locals: name -> (alloca, type)
    locals: HashMap<String, (PointerValue<'ctx>, Type)>,
    /// Parameter locals at function entry (for TCO reset).
    param_locals: HashMap<String, (PointerValue<'ctx>, Type)>,
    /// Ordered parameter names of the function being emitted.
    param_names: Vec<String>,
    /// Name of the function currently being emitted (for self-TCO).
    current_fn: Option<String>,
    /// Loop header for self-tail-call optimization.
    loop_header: Option<BasicBlock<'ctx>>,
    str_count: usize,
}

impl<'ctx> Codegen<'ctx> {
    pub fn new(context: &'ctx Context, module_name: &str) -> Self {
        let module = context.create_module(module_name);
        let builder = context.create_builder();
        Self {
            context,
            module,
            builder,
            fns: HashMap::new(),
            fn_types: HashMap::new(),
            consts: HashMap::new(),
            locals: HashMap::new(),
            param_locals: HashMap::new(),
            param_names: Vec::new(),
            current_fn: None,
            loop_header: None,
            str_count: 0,
        }
    }

    /// Consume the codegen wrapper and return the LLVM module (e.g. for JIT).
    pub fn into_module(self) -> Module<'ctx> {
        self.module
    }

    pub fn compile_program(&mut self, prog: &Program, _tyck: &TypeCk) -> Result<(), CodegenError> {
        // declare external `puts(i8*) -> i32` for println
        let i32_ty = self.context.i32_type();
        let i8ptr_ty = self.context.ptr_type(AddressSpace::default());
        let puts_ty = i32_ty.fn_type(&[i8ptr_ty.into()], false);
        let puts_fn = self.module.add_function("puts", puts_ty, None);
        self.fns.insert("__puts".into(), puts_fn);

        // also `printf(i8*, ...) -> i32` for non-newline print
        let printf_ty = i32_ty.fn_type(&[i8ptr_ty.into()], true);
        let printf_fn = self.module.add_function("printf", printf_ty, None);
        self.fns.insert("__printf".into(), printf_fn);

        // declare all user functions first (allow forward refs)
        for it in &prog.items {
            if let TopLevel::Function(f) = it {
                let fn_ty = self.fn_type(&f.params.iter().map(|p| p.ty.clone()).collect::<Vec<_>>(), &f.ret);
                let fv = self.module.add_function(&f.name, fn_ty, None);
                self.fns.insert(f.name.clone(), fv);
                self.fn_types.insert(
                    f.name.clone(),
                    (f.params.iter().map(|p| p.ty.clone()).collect(), f.ret.clone()),
                );
            }
        }

        // emit consts as globals
        for it in &prog.items {
            if let TopLevel::Const(c) = it {
                let v = self.const_eval(&c.value, &c.ty)?;
                self.consts.insert(c.name.clone(), (c.ty.clone(), v));
            }
        }

        // emit function bodies
        for it in &prog.items {
            if let TopLevel::Function(f) = it {
                self.emit_function(f)?;
            }
        }

        Ok(())
    }

    fn fn_type(&self, params: &[Type], ret: &Type) -> FunctionType<'ctx> {
        let param_tys: Vec<BasicMetadataTypeEnum> =
            params.iter().map(|t| basic_metadata(self.context, t)).collect();
        match ret {
            Type::Unit => self.context.void_type().fn_type(&param_tys, false),
            other => match basic_type(self.context, other) {
                BasicTypeEnum::IntType(t) => t.fn_type(&param_tys, false),
                BasicTypeEnum::FloatType(t) => t.fn_type(&param_tys, false),
                BasicTypeEnum::PointerType(t) => t.fn_type(&param_tys, false),
                _ => unreachable!(),
            },
        }
    }

    fn emit_function(&mut self, f: &Function) -> Result<(), CodegenError> {
        let fv = self.fns[&f.name];
        let entry = self.context.append_basic_block(fv, "entry");
        let loop_bb = self.context.append_basic_block(fv, "loop");
        self.builder.position_at_end(entry);

        // allocate parameters as locals
        self.locals.clear();
        self.param_names.clear();
        for (i, p) in f.params.iter().enumerate() {
            let arg = fv
                .get_nth_param(i as u32)
                .ok_or_else(|| CodegenError::Internal("missing param".into()))?;
            let alloca = self.create_entry_alloca(fv, &p.name, &p.ty);
            self.builder
                .build_store(alloca, arg)
                .map_err(|e| CodegenError::Llvm(e.to_string()))?;
            self.locals.insert(p.name.clone(), (alloca, p.ty.clone()));
            self.param_names.push(p.name.clone());
        }
        self.param_locals = self.locals.clone();
        self.current_fn = Some(f.name.clone());
        self.loop_header = Some(loop_bb);

        self.builder
            .build_unconditional_branch(loop_bb)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;

        self.builder.position_at_end(loop_bb);
        // Body in tail position: ends with `ret` or a self-TCO branch back to `loop`.
        self.emit_tail(&f.body, &f.ret)?;

        self.current_fn = None;
        self.loop_header = None;
        self.param_locals.clear();
        self.param_names.clear();
        Ok(())
    }

    /// Emit an expression in tail position: always terminates with `ret` or a
    /// self-tail-call branch to the loop header (TCO).
    fn emit_tail(&mut self, e: &Expr, ret_ty: &Type) -> Result<(), CodegenError> {
        match &e.kind {
            ExprKind::If {
                cond,
                then_branch,
                else_branch,
            } => {
                let cv = self
                    .emit_expr(cond)?
                    .ok_or_else(|| CodegenError::Internal("if cond".into()))?
                    .into_int_value();
                let fv = self
                    .builder
                    .get_insert_block()
                    .unwrap()
                    .get_parent()
                    .unwrap();
                let then_bb = self.context.append_basic_block(fv, "then.tail");
                let else_bb = self.context.append_basic_block(fv, "else.tail");
                self.builder
                    .build_conditional_branch(cv, then_bb, else_bb)
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;

                self.builder.position_at_end(then_bb);
                self.emit_tail(then_branch, ret_ty)?;
                self.builder.position_at_end(else_bb);
                self.emit_tail(else_branch, ret_ty)?;
                Ok(())
            }
            ExprKind::Let { bindings, body } => {
                let mut prev: Vec<(String, Option<(PointerValue<'ctx>, Type)>)> = Vec::new();
                let fv = self
                    .builder
                    .get_insert_block()
                    .unwrap()
                    .get_parent()
                    .unwrap();
                for b in bindings {
                    let v = self
                        .emit_expr(&b.value)?
                        .ok_or_else(|| CodegenError::Internal("let value".into()))?;
                    let alloca = self.create_entry_alloca(fv, &b.name, &b.ty);
                    self.builder
                        .build_store(alloca, v)
                        .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                    prev.push((
                        b.name.clone(),
                        self.locals.insert(b.name.clone(), (alloca, b.ty.clone())),
                    ));
                }
                self.emit_tail(body, ret_ty)?;
                // Body always terminates; restore is only for map hygiene if we
                // ever resume — keep param_locals authoritative for TCO.
                let _ = prev;
                Ok(())
            }
            ExprKind::Do(exprs) => {
                if exprs.is_empty() {
                    return self.emit_return(None, ret_ty);
                }
                let last = exprs.len() - 1;
                for ex in &exprs[..last] {
                    let _ = self.emit_expr(ex)?;
                }
                self.emit_tail(&exprs[last], ret_ty)
            }
            ExprKind::Call { callee, args }
                if self.current_fn.as_deref() == Some(callee.as_str()) =>
            {
                self.emit_self_tco(args)
            }
            _ => {
                let v = self.emit_expr(e)?;
                self.emit_return(v, ret_ty)
            }
        }
    }

    fn emit_return(
        &self,
        ret_val: Option<BasicValueEnum<'ctx>>,
        ret_ty: &Type,
    ) -> Result<(), CodegenError> {
        match (ret_ty, ret_val) {
            (Type::Unit, _) => {
                self.builder
                    .build_return(None)
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
            }
            (_, Some(v)) => {
                self.builder
                    .build_return(Some(&v))
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
            }
            (_, None) => {
                return Err(CodegenError::Internal(
                    "tail expr produced no value but return type is non-unit".into(),
                ));
            }
        }
        Ok(())
    }

    /// Self-tail-call: update parameter allocas and jump to the loop header.
    fn emit_self_tco(&mut self, args: &[Expr]) -> Result<(), CodegenError> {
        if args.len() != self.param_names.len() {
            return Err(CodegenError::Internal(
                "self-TCO arity mismatch (typeck should have caught this)".into(),
            ));
        }
        // Evaluate all arguments first (left-to-right) before storing, so that
        // reads of current parameters see pre-update values.
        let mut values = Vec::with_capacity(args.len());
        for a in args {
            let v = self
                .emit_expr(a)?
                .ok_or_else(|| CodegenError::Internal("tco arg".into()))?;
            values.push(v);
        }
        for (name, v) in self.param_names.iter().zip(values) {
            let (ptr, _) = self.param_locals[name];
            self.builder
                .build_store(ptr, v)
                .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        }
        self.locals = self.param_locals.clone();
        let loop_bb = self
            .loop_header
            .ok_or_else(|| CodegenError::Internal("TCO without loop header".into()))?;
        self.builder
            .build_unconditional_branch(loop_bb)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        Ok(())
    }

    fn create_entry_alloca(&self, fv: FunctionValue<'ctx>, name: &str, ty: &Type) -> PointerValue<'ctx> {
        let entry = fv.get_first_basic_block().unwrap();
        let tmp_builder = self.context.create_builder();
        match entry.get_first_instruction() {
            Some(inst) => tmp_builder.position_before(&inst),
            None => tmp_builder.position_at_end(entry),
        }
        let bt = basic_type(self.context, ty);
        match bt {
            BasicTypeEnum::IntType(t) => tmp_builder.build_alloca(t, name).unwrap(),
            BasicTypeEnum::FloatType(t) => tmp_builder.build_alloca(t, name).unwrap(),
            BasicTypeEnum::PointerType(t) => tmp_builder.build_alloca(t, name).unwrap(),
            _ => panic!("unsupported alloca type"),
        }
    }

    fn expr_ty(e: &Expr) -> Result<&Type, CodegenError> {
        e.ty.as_ref()
            .ok_or_else(|| CodegenError::Internal("expression missing type info (typeck not run?)".into()))
    }

    fn emit_expr(&mut self, e: &Expr) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        let ty = Self::expr_ty(e)?.clone();
        match &e.kind {
            ExprKind::Lit(l) => Ok(Some(self.emit_lit(l, &ty))),
            ExprKind::Var(name) => {
                if let Some((ptr, vty)) = self.locals.get(name).cloned() {
                    let bt = basic_type(self.context, &vty);
                    let v = self
                        .builder
                        .build_load(bt, ptr, name)
                        .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                    Ok(Some(v))
                } else if let Some((_, v)) = self.consts.get(name) {
                    Ok(Some(*v))
                } else {
                    Err(CodegenError::Internal(format!("unresolved var {name}")))
                }
            }
            ExprKind::If { cond, then_branch, else_branch } => {
                let cv = self
                    .emit_expr(cond)?
                    .ok_or_else(|| CodegenError::Internal("if cond".into()))?
                    .into_int_value();
                let fv = self.builder.get_insert_block().unwrap().get_parent().unwrap();
                let then_bb = self.context.append_basic_block(fv, "then");
                let else_bb = self.context.append_basic_block(fv, "else");
                let merge_bb = self.context.append_basic_block(fv, "ifcont");
                self.builder
                    .build_conditional_branch(cv, then_bb, else_bb)
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;

                self.builder.position_at_end(then_bb);
                let tv = self.emit_expr(then_branch)?;
                let then_end = self.builder.get_insert_block().unwrap();
                self.builder.build_unconditional_branch(merge_bb).map_err(|e| CodegenError::Llvm(e.to_string()))?;

                self.builder.position_at_end(else_bb);
                let ev = self.emit_expr(else_branch)?;
                let else_end = self.builder.get_insert_block().unwrap();
                self.builder.build_unconditional_branch(merge_bb).map_err(|e| CodegenError::Llvm(e.to_string()))?;

                self.builder.position_at_end(merge_bb);

                if ty == Type::Unit {
                    return Ok(None);
                }

                let bt = basic_type(self.context, &ty);
                let phi = self.builder.build_phi(bt, "iftmp").map_err(|e| CodegenError::Llvm(e.to_string()))?;
                let tv = tv.ok_or_else(|| CodegenError::Internal("if then no value".into()))?;
                let ev = ev.ok_or_else(|| CodegenError::Internal("if else no value".into()))?;
                phi.add_incoming(&[(&tv, then_end), (&ev, else_end)]);
                Ok(Some(phi.as_basic_value()))
            }
            ExprKind::Let { bindings, body } => {
                // save shadowed
                let mut prev: Vec<(String, Option<(PointerValue<'ctx>, Type)>)> = Vec::new();
                let fv = self.builder.get_insert_block().unwrap().get_parent().unwrap();
                for b in bindings {
                    let v = self
                        .emit_expr(&b.value)?
                        .ok_or_else(|| CodegenError::Internal("let value".into()))?;
                    let alloca = self.create_entry_alloca(fv, &b.name, &b.ty);
                    self.builder.build_store(alloca, v).map_err(|e| CodegenError::Llvm(e.to_string()))?;
                    prev.push((b.name.clone(), self.locals.insert(b.name.clone(), (alloca, b.ty.clone()))));
                }
                let result = self.emit_expr(body)?;
                // restore
                for (name, p) in prev.into_iter().rev() {
                    match p {
                        Some(x) => { self.locals.insert(name, x); }
                        None => { self.locals.remove(&name); }
                    }
                }
                Ok(result)
            }
            ExprKind::Do(exprs) => {
                let mut last: Option<BasicValueEnum<'ctx>> = None;
                for ex in exprs {
                    last = self.emit_expr(ex)?;
                }
                Ok(last)
            }
            ExprKind::Cast { ty: to_ty, expr } => {
                let from_ty = Self::expr_ty(expr)?.clone();
                let v = self
                    .emit_expr(expr)?
                    .ok_or_else(|| CodegenError::Internal("cast expr".into()))?;
                Ok(Some(self.emit_cast(v, &from_ty, to_ty)?))
            }
            ExprKind::Set { name, value } => {
                let v = self
                    .emit_expr(value)?
                    .ok_or_else(|| CodegenError::Internal("set! value".into()))?;
                let (ptr, _) = self
                    .locals
                    .get(name)
                    .cloned()
                    .ok_or_else(|| CodegenError::Internal(format!("set! unresolved {name}")))?;
                self.builder
                    .build_store(ptr, v)
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                Ok(None)
            }
            ExprKind::Call { callee, args } => self.emit_call(callee, args, &ty),
        }
    }

    fn emit_cast(
        &self,
        v: BasicValueEnum<'ctx>,
        from: &Type,
        to: &Type,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        if from == to {
            return Ok(v);
        }
        let llvm = |e: inkwell::builder::BuilderError| CodegenError::Llvm(e.to_string());
        match (from, to) {
            (Type::I32, Type::I64) => Ok(self
                .builder
                .build_int_s_extend(v.into_int_value(), self.context.i64_type(), "sext")
                .map_err(llvm)?
                .into()),
            (Type::I64, Type::I32) => Ok(self
                .builder
                .build_int_truncate(v.into_int_value(), self.context.i32_type(), "trunc")
                .map_err(llvm)?
                .into()),
            (Type::F32, Type::F64) => Ok(self
                .builder
                .build_float_ext(v.into_float_value(), self.context.f64_type(), "fpext")
                .map_err(llvm)?
                .into()),
            (Type::F64, Type::F32) => Ok(self
                .builder
                .build_float_trunc(v.into_float_value(), self.context.f32_type(), "fptrunc")
                .map_err(llvm)?
                .into()),
            (Type::I32, Type::F32) => Ok(self
                .builder
                .build_signed_int_to_float(v.into_int_value(), self.context.f32_type(), "sitofp")
                .map_err(llvm)?
                .into()),
            (Type::I32, Type::F64) => Ok(self
                .builder
                .build_signed_int_to_float(v.into_int_value(), self.context.f64_type(), "sitofp")
                .map_err(llvm)?
                .into()),
            (Type::I64, Type::F32) => Ok(self
                .builder
                .build_signed_int_to_float(v.into_int_value(), self.context.f32_type(), "sitofp")
                .map_err(llvm)?
                .into()),
            (Type::I64, Type::F64) => Ok(self
                .builder
                .build_signed_int_to_float(v.into_int_value(), self.context.f64_type(), "sitofp")
                .map_err(llvm)?
                .into()),
            (Type::F32, Type::I32) => Ok(self
                .builder
                .build_float_to_signed_int(v.into_float_value(), self.context.i32_type(), "fptosi")
                .map_err(llvm)?
                .into()),
            (Type::F32, Type::I64) => Ok(self
                .builder
                .build_float_to_signed_int(v.into_float_value(), self.context.i64_type(), "fptosi")
                .map_err(llvm)?
                .into()),
            (Type::F64, Type::I32) => Ok(self
                .builder
                .build_float_to_signed_int(v.into_float_value(), self.context.i32_type(), "fptosi")
                .map_err(llvm)?
                .into()),
            (Type::F64, Type::I64) => Ok(self
                .builder
                .build_float_to_signed_int(v.into_float_value(), self.context.i64_type(), "fptosi")
                .map_err(llvm)?
                .into()),
            _ => Err(CodegenError::Internal(format!("unsupported cast {from} -> {to}"))),
        }
    }

    fn emit_call(
        &mut self,
        callee: &str,
        args: &[Expr],
        ret_ty: &Type,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        match callee {
            "+" | "-" | "*" | "/" | "mod" => Ok(Some(self.emit_arith_call(callee, args, ret_ty)?)),
            "<" | "<=" | ">" | ">=" | "=" | "!=" => {
                let a = self.emit_expr(&args[0])?.unwrap();
                let b = self.emit_expr(&args[1])?.unwrap();
                let v = self.emit_cmp(callee, a, b)?;
                Ok(Some(v.into()))
            }
            "and" => Ok(Some(self.emit_short_circuit_and(args)?.into())),
            "or" => Ok(Some(self.emit_short_circuit_or(args)?.into())),
            "not" => {
                let a = self.emit_expr(&args[0])?.unwrap().into_int_value();
                let v = self.builder.build_not(a, "nottmp").map_err(|e| CodegenError::Llvm(e.to_string()))?;
                Ok(Some(v.into()))
            }
            "println" => {
                self.emit_print(args, true)?;
                Ok(None)
            }
            "print" => {
                self.emit_print(args, false)?;
                Ok(None)
            }
            _ => {
                let fv = *self
                    .fns
                    .get(callee)
                    .ok_or_else(|| CodegenError::Internal(format!("undef fn {callee}")))?;
                let (_, fn_ret_ty) = self
                    .fn_types
                    .get(callee)
                    .cloned()
                    .ok_or_else(|| CodegenError::Internal(format!("no sig {callee}")))?;
                let mut argv: Vec<BasicMetadataValueEnum> = Vec::with_capacity(args.len());
                for a in args.iter() {
                    let v = self.emit_expr(a)?.unwrap();
                    argv.push(v.into());
                }
                let call = self
                    .builder
                    .build_call(fv, &argv, "calltmp")
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                if fn_ret_ty == Type::Unit {
                    Ok(None)
                } else {
                    Ok(call.try_as_basic_value().basic())
                }
            }
        }
    }

    /// `(and a b)` — evaluate `b` only when `a` is true.
    fn emit_short_circuit_and(
        &mut self,
        args: &[Expr],
    ) -> Result<inkwell::values::IntValue<'ctx>, CodegenError> {
        let bool_ty = self.context.bool_type();
        let a = self
            .emit_expr(&args[0])?
            .ok_or_else(|| CodegenError::Internal("and lhs".into()))?
            .into_int_value();
        let fv = self.builder.get_insert_block().unwrap().get_parent().unwrap();
        let rhs_bb = self.context.append_basic_block(fv, "and.rhs");
        let merge_bb = self.context.append_basic_block(fv, "and.end");
        let entry_bb = self.builder.get_insert_block().unwrap();

        self.builder
            .build_conditional_branch(a, rhs_bb, merge_bb)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;

        self.builder.position_at_end(rhs_bb);
        let b = self
            .emit_expr(&args[1])?
            .ok_or_else(|| CodegenError::Internal("and rhs".into()))?
            .into_int_value();
        let rhs_end = self.builder.get_insert_block().unwrap();
        self.builder
            .build_unconditional_branch(merge_bb)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;

        self.builder.position_at_end(merge_bb);
        let phi = self
            .builder
            .build_phi(bool_ty, "andtmp")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        let false_v = bool_ty.const_int(0, false);
        phi.add_incoming(&[(&false_v, entry_bb), (&b, rhs_end)]);
        Ok(phi.as_basic_value().into_int_value())
    }

    /// `(or a b)` — evaluate `b` only when `a` is false.
    fn emit_short_circuit_or(
        &mut self,
        args: &[Expr],
    ) -> Result<inkwell::values::IntValue<'ctx>, CodegenError> {
        let bool_ty = self.context.bool_type();
        let a = self
            .emit_expr(&args[0])?
            .ok_or_else(|| CodegenError::Internal("or lhs".into()))?
            .into_int_value();
        let fv = self.builder.get_insert_block().unwrap().get_parent().unwrap();
        let rhs_bb = self.context.append_basic_block(fv, "or.rhs");
        let merge_bb = self.context.append_basic_block(fv, "or.end");
        let entry_bb = self.builder.get_insert_block().unwrap();

        self.builder
            .build_conditional_branch(a, merge_bb, rhs_bb)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;

        self.builder.position_at_end(rhs_bb);
        let b = self
            .emit_expr(&args[1])?
            .ok_or_else(|| CodegenError::Internal("or rhs".into()))?
            .into_int_value();
        let rhs_end = self.builder.get_insert_block().unwrap();
        self.builder
            .build_unconditional_branch(merge_bb)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;

        self.builder.position_at_end(merge_bb);
        let phi = self
            .builder
            .build_phi(bool_ty, "ortmp")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        let true_v = bool_ty.const_int(1, false);
        phi.add_incoming(&[(&true_v, entry_bb), (&b, rhs_end)]);
        Ok(phi.as_basic_value().into_int_value())
    }

    fn emit_arith_call(
        &mut self,
        op: &str,
        args: &[Expr],
        result_ty: &Type,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        // Unary minus: `(- x)`
        if op == "-" && args.len() == 1 {
            let a = self
                .emit_expr(&args[0])?
                .ok_or_else(|| CodegenError::Internal("unary -".into()))?;
            return self.emit_neg(a, result_ty);
        }
        // Identity for single-arg `+` / `*`
        if matches!(op, "+" | "*") && args.len() == 1 {
            return self
                .emit_expr(&args[0])?
                .ok_or_else(|| CodegenError::Internal("arith identity".into()));
        }
        // Binary / n-ary: left fold
        let mut acc = self
            .emit_expr(&args[0])?
            .ok_or_else(|| CodegenError::Internal("arith lhs".into()))?;
        for arg in args.iter().skip(1) {
            let b = self
                .emit_expr(arg)?
                .ok_or_else(|| CodegenError::Internal("arith rhs".into()))?;
            acc = self.emit_arith(op, acc, b, result_ty)?;
        }
        Ok(acc)
    }

    fn emit_neg(
        &self,
        a: BasicValueEnum<'ctx>,
        result_ty: &Type,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        match result_ty {
            Type::I32 | Type::I64 => {
                let v = self
                    .builder
                    .build_int_neg(a.into_int_value(), "neg")
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                Ok(v.into())
            }
            Type::F32 | Type::F64 => {
                let v = self
                    .builder
                    .build_float_neg(a.into_float_value(), "fneg")
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                Ok(v.into())
            }
            other => Err(CodegenError::Internal(format!("neg on {other}"))),
        }
    }

    /// Print a value with `printf`. `newline` appends `\n` (or uses `puts` for `str`).
    fn emit_print(&mut self, args: &[Expr], newline: bool) -> Result<(), CodegenError> {
        let arg = &args[0];
        let ty = Self::expr_ty(arg)?.clone();
        let v = self
            .emit_expr(arg)?
            .ok_or_else(|| CodegenError::Internal("print arg".into()))?;

        // Fast path: println of str → puts
        if newline && ty == Type::Str {
            let puts = self.fns["__puts"];
            let argv: [BasicMetadataValueEnum; 1] = [v.into()];
            self.builder
                .build_call(puts, &argv, "putscall")
                .map_err(|e| CodegenError::Llvm(e.to_string()))?;
            return Ok(());
        }

        let printf = self.fns["__printf"];
        let (fmt, arg_v): (&str, BasicMetadataValueEnum) = match ty {
            Type::Str => ("%s", v.into()),
            Type::I32 => ("%d", v.into()),
            Type::I64 => ("%lld", v.into()),
            Type::F64 => ("%g", v.into()),
            Type::F32 => {
                let ext = self
                    .builder
                    .build_float_ext(v.into_float_value(), self.context.f64_type(), "fpext")
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                ("%g", ext.into())
            }
            Type::Bool => {
                let true_s = self.intern_str("true");
                let false_s = self.intern_str("false");
                let selected = self
                    .builder
                    .build_select(v.into_int_value(), true_s, false_s, "boolstr")
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                let ptr = selected.into_pointer_value();
                ("%s", ptr.into())
            }
            Type::Unit => {
                return Err(CodegenError::Internal("cannot print unit".into()));
            }
        };

        let fmt_s = if newline {
            self.intern_str(&format!("{fmt}\n"))
        } else {
            self.intern_str(fmt)
        };
        let argv: [BasicMetadataValueEnum; 2] = [fmt_s.into(), arg_v];
        self.builder
            .build_call(printf, &argv, "printfcall")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        Ok(())
    }

    fn emit_arith(
        &self,
        op: &str,
        a: BasicValueEnum<'ctx>,
        b: BasicValueEnum<'ctx>,
        result_ty: &Type,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        match result_ty {
            Type::I32 | Type::I64 => {
                let a = a.into_int_value();
                let b = b.into_int_value();
                let v = match op {
                    "+" => self.builder.build_int_add(a, b, "add"),
                    "-" => self.builder.build_int_sub(a, b, "sub"),
                    "*" => self.builder.build_int_mul(a, b, "mul"),
                    "/" => self.builder.build_int_signed_div(a, b, "div"),
                    "mod" => self.builder.build_int_signed_rem(a, b, "mod"),
                    _ => unreachable!(),
                }
                .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                Ok(v.into())
            }
            Type::F32 | Type::F64 => {
                let a = a.into_float_value();
                let b = b.into_float_value();
                let v = match op {
                    "+" => self.builder.build_float_add(a, b, "fadd"),
                    "-" => self.builder.build_float_sub(a, b, "fsub"),
                    "*" => self.builder.build_float_mul(a, b, "fmul"),
                    "/" => self.builder.build_float_div(a, b, "fdiv"),
                    "mod" => self.builder.build_float_rem(a, b, "fmod"),
                    _ => unreachable!(),
                }
                .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                Ok(v.into())
            }
            other => Err(CodegenError::Internal(format!("arith on {other}"))),
        }
    }

    fn emit_cmp(
        &self,
        op: &str,
        a: BasicValueEnum<'ctx>,
        b: BasicValueEnum<'ctx>,
    ) -> Result<inkwell::values::IntValue<'ctx>, CodegenError> {
        if a.is_int_value() {
            let pred = match op {
                "<" => IntPredicate::SLT,
                "<=" => IntPredicate::SLE,
                ">" => IntPredicate::SGT,
                ">=" => IntPredicate::SGE,
                "=" => IntPredicate::EQ,
                "!=" => IntPredicate::NE,
                _ => unreachable!(),
            };
            self.builder
                .build_int_compare(pred, a.into_int_value(), b.into_int_value(), "cmp")
                .map_err(|e| CodegenError::Llvm(e.to_string()))
        } else {
            let pred = match op {
                "<" => FloatPredicate::OLT,
                "<=" => FloatPredicate::OLE,
                ">" => FloatPredicate::OGT,
                ">=" => FloatPredicate::OGE,
                "=" => FloatPredicate::OEQ,
                "!=" => FloatPredicate::ONE,
                _ => unreachable!(),
            };
            self.builder
                .build_float_compare(pred, a.into_float_value(), b.into_float_value(), "fcmp")
                .map_err(|e| CodegenError::Llvm(e.to_string()))
        }
    }

    fn emit_lit(&mut self, l: &Lit, ty: &Type) -> BasicValueEnum<'ctx> {
        match l {
            Lit::Int(v, _) => match ty {
                Type::I32 => self.context.i32_type().const_int(*v as u64, true).into(),
                Type::I64 => self.context.i64_type().const_int(*v as u64, true).into(),
                _ => self.context.i32_type().const_int(*v as u64, true).into(),
            },
            Lit::Float(v, _) => match ty {
                Type::F32 => self.context.f32_type().const_float(*v).into(),
                Type::F64 => self.context.f64_type().const_float(*v).into(),
                _ => self.context.f64_type().const_float(*v).into(),
            },
            Lit::Bool(b) => self
                .context
                .bool_type()
                .const_int(if *b { 1 } else { 0 }, false)
                .into(),
            Lit::Str(s) => self.intern_str(s).into(),
        }
    }

    fn intern_str(&mut self, s: &str) -> PointerValue<'ctx> {
        let name = format!(".str{}", self.str_count);
        self.str_count += 1;
        let global = self
            .builder
            .build_global_string_ptr(s, &name)
            .expect("global string");
        global.as_pointer_value()
    }

    fn const_eval(&mut self, e: &Expr, expected: &Type) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        // For MVP: only literals as const initializers.
        match &e.kind {
            ExprKind::Lit(l) => Ok(self.emit_lit(l, expected)),
            _ => Err(CodegenError::Internal(
                "only literal constants supported in `def` for MVP".into(),
            )),
        }
    }
}

fn basic_type<'ctx>(ctx: &'ctx Context, t: &Type) -> BasicTypeEnum<'ctx> {
    match t {
        Type::I32 => ctx.i32_type().into(),
        Type::I64 => ctx.i64_type().into(),
        Type::F32 => ctx.f32_type().into(),
        Type::F64 => ctx.f64_type().into(),
        Type::Bool => ctx.bool_type().into(),
        Type::Str => ctx.ptr_type(AddressSpace::default()).into(),
        Type::Unit => panic!("unit has no basic type"),
    }
}

fn basic_metadata<'ctx>(ctx: &'ctx Context, t: &Type) -> BasicMetadataTypeEnum<'ctx> {
    match basic_type(ctx, t) {
        BasicTypeEnum::IntType(t) => t.into(),
        BasicTypeEnum::FloatType(t) => t.into(),
        BasicTypeEnum::PointerType(t) => t.into(),
        _ => unreachable!(),
    }
}
