//! LLVM IR codegen using inkwell.

use crate::ast::*;
use crate::typeck::TypeCk;
use inkwell::AddressSpace;
use inkwell::FloatPredicate;
use inkwell::IntPredicate;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum, FunctionType};
use inkwell::values::{BasicMetadataValueEnum, BasicValue, BasicValueEnum, FunctionValue, PointerValue};
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
            str_count: 0,
        }
    }

    pub fn compile_program(&mut self, prog: &Program, tyck: &TypeCk) -> Result<(), CodegenError> {
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
                self.emit_function(f, tyck)?;
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

    fn emit_function(&mut self, f: &Function, _tyck: &TypeCk) -> Result<(), CodegenError> {
        let fv = self.fns[&f.name];
        let entry = self.context.append_basic_block(fv, "entry");
        self.builder.position_at_end(entry);

        // allocate parameters as locals
        self.locals.clear();
        for (i, p) in f.params.iter().enumerate() {
            let arg = fv.get_nth_param(i as u32).ok_or_else(|| CodegenError::Internal("missing param".into()))?;
            let alloca = self.create_entry_alloca(fv, &p.name, &p.ty);
            self.builder.build_store(alloca, arg).map_err(|e| CodegenError::Llvm(e.to_string()))?;
            self.locals.insert(p.name.clone(), (alloca, p.ty.clone()));
        }

        let ret_val = self.emit_expr(&f.body, &f.ret)?;

        match (&f.ret, ret_val) {
            (Type::Unit, _) => {
                self.builder.build_return(None).map_err(|e| CodegenError::Llvm(e.to_string()))?;
            }
            (_, Some(v)) => {
                self.builder
                    .build_return(Some(&v))
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
            }
            (_, None) => {
                return Err(CodegenError::Internal(
                    "function body produced no value but return type is non-unit".into(),
                ));
            }
        }
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

    fn emit_expr(&mut self, e: &Expr, expected: &Type) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        match &e.kind {
            ExprKind::Lit(l) => Ok(Some(self.emit_lit(l, expected))),
            ExprKind::Var(name) => {
                if let Some((ptr, ty)) = self.locals.get(name).cloned() {
                    let bt = basic_type(self.context, &ty);
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
                    .emit_expr(cond, &Type::Bool)?
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
                let tv = self.emit_expr(then_branch, expected)?;
                let then_end = self.builder.get_insert_block().unwrap();
                self.builder.build_unconditional_branch(merge_bb).map_err(|e| CodegenError::Llvm(e.to_string()))?;

                self.builder.position_at_end(else_bb);
                let ev = self.emit_expr(else_branch, expected)?;
                let else_end = self.builder.get_insert_block().unwrap();
                self.builder.build_unconditional_branch(merge_bb).map_err(|e| CodegenError::Llvm(e.to_string()))?;

                self.builder.position_at_end(merge_bb);

                if expected == &Type::Unit {
                    return Ok(None);
                }

                let bt = basic_type(self.context, expected);
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
                        .emit_expr(&b.value, &b.ty)?
                        .ok_or_else(|| CodegenError::Internal("let value".into()))?;
                    let alloca = self.create_entry_alloca(fv, &b.name, &b.ty);
                    self.builder.build_store(alloca, v).map_err(|e| CodegenError::Llvm(e.to_string()))?;
                    prev.push((b.name.clone(), self.locals.insert(b.name.clone(), (alloca, b.ty.clone()))));
                }
                let result = self.emit_expr(body, expected)?;
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
                let len = exprs.len();
                for (i, ex) in exprs.iter().enumerate() {
                    let exp = if i + 1 == len { expected.clone() } else { Type::Unit };
                    last = self.emit_expr(ex, &exp)?;
                }
                Ok(last)
            }
            ExprKind::Call { callee, args } => self.emit_call(callee, args, expected),
        }
    }

    fn emit_call(
        &mut self,
        callee: &str,
        args: &[Expr],
        expected: &Type,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        // numeric / comparison / logical builtins
        match callee {
            "+" | "-" | "*" | "/" | "mod" => {
                let a = self.emit_expr(&args[0], expected)?.unwrap();
                let b = self.emit_expr(&args[1], expected)?.unwrap();
                let v = self.emit_arith(callee, a, b, expected)?;
                Ok(Some(v))
            }
            "<" | "<=" | ">" | ">=" | "=" | "!=" => {
                // operands share a type; we don't statically know it here, so infer from arg
                // using a best-effort: emit arg with no expected (reuse type from first lit)
                let a = self.emit_expr(&args[0], &Type::I32)?.unwrap();
                let b = self.emit_expr(&args[1], &Type::I32)?.unwrap();
                let v = self.emit_cmp(callee, a, b)?;
                Ok(Some(v.into()))
            }
            "and" => {
                let a = self.emit_expr(&args[0], &Type::Bool)?.unwrap().into_int_value();
                let b = self.emit_expr(&args[1], &Type::Bool)?.unwrap().into_int_value();
                let v = self.builder.build_and(a, b, "andtmp").map_err(|e| CodegenError::Llvm(e.to_string()))?;
                Ok(Some(v.into()))
            }
            "or" => {
                let a = self.emit_expr(&args[0], &Type::Bool)?.unwrap().into_int_value();
                let b = self.emit_expr(&args[1], &Type::Bool)?.unwrap().into_int_value();
                let v = self.builder.build_or(a, b, "ortmp").map_err(|e| CodegenError::Llvm(e.to_string()))?;
                Ok(Some(v.into()))
            }
            "not" => {
                let a = self.emit_expr(&args[0], &Type::Bool)?.unwrap().into_int_value();
                let v = self.builder.build_not(a, "nottmp").map_err(|e| CodegenError::Llvm(e.to_string()))?;
                Ok(Some(v.into()))
            }
            "println" => {
                let s = self.emit_expr(&args[0], &Type::Str)?.unwrap();
                let puts = self.fns["__puts"];
                let argv: [BasicMetadataValueEnum; 1] = [s.into()];
                self.builder
                    .build_call(puts, &argv, "putscall")
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                Ok(None)
            }
            "print" => {
                let s = self.emit_expr(&args[0], &Type::Str)?.unwrap();
                // printf("%s", s)
                let fmt = self.intern_str("%s");
                let printf = self.fns["__printf"];
                let argv: [BasicMetadataValueEnum; 2] = [fmt.into(), s.into()];
                self.builder
                    .build_call(printf, &argv, "printfcall")
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                Ok(None)
            }
            _ => {
                let fv = *self
                    .fns
                    .get(callee)
                    .ok_or_else(|| CodegenError::Internal(format!("undef fn {callee}")))?;
                let (param_tys, ret_ty) = self
                    .fn_types
                    .get(callee)
                    .cloned()
                    .ok_or_else(|| CodegenError::Internal(format!("no sig {callee}")))?;
                let mut argv: Vec<BasicMetadataValueEnum> = Vec::with_capacity(args.len());
                for (a, pt) in args.iter().zip(param_tys.iter()) {
                    let v = self.emit_expr(a, pt)?.unwrap();
                    argv.push(v.into());
                }
                let call = self
                    .builder
                    .build_call(fv, &argv, "calltmp")
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                if ret_ty == Type::Unit {
                    Ok(None)
                } else {
                    Ok(call.try_as_basic_value().basic())
                }
            }
        }
    }

    fn emit_arith(
        &self,
        op: &str,
        a: BasicValueEnum<'ctx>,
        b: BasicValueEnum<'ctx>,
        expected: &Type,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        match expected {
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

    fn emit_lit(&mut self, l: &Lit, expected: &Type) -> BasicValueEnum<'ctx> {
        match l {
            Lit::Int(v, _) => match expected {
                Type::I32 => self.context.i32_type().const_int(*v as u64, true).into(),
                Type::I64 => self.context.i64_type().const_int(*v as u64, true).into(),
                _ => self.context.i32_type().const_int(*v as u64, true).into(),
            },
            Lit::Float(v, _) => match expected {
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
            ExprKind::Lit(l) => Ok(self.emit_lit_const(l, expected)),
            _ => Err(CodegenError::Internal(
                "only literal constants supported in `def` for MVP".into(),
            )),
        }
    }

    fn emit_lit_const(&mut self, l: &Lit, expected: &Type) -> BasicValueEnum<'ctx> {
        // Same as emit_lit but doesn't need a builder position.
        self.emit_lit(l, expected)
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
