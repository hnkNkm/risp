//! LLVM IR codegen using inkwell.

use crate::ast::*;
use crate::typeck::{TypeCk, VariantInfo};
use inkwell::AddressSpace;
use inkwell::FloatPredicate;
use inkwell::IntPredicate;
use inkwell::basic_block::BasicBlock;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::types::{BasicMetadataTypeEnum, BasicTypeEnum, FunctionType, StructType};
use inkwell::values::{BasicMetadataValueEnum, BasicValueEnum, FunctionValue, IntValue, PointerValue};
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
    /// Exit blocks for nested `while` / `loop` (`break` targets).
    loop_exits: Vec<BasicBlock<'ctx>>,
    str_count: usize,
    structs: HashMap<String, StructDef>,
    enums: HashMap<String, EnumDef>,
    variants: HashMap<String, VariantInfo>,
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
            loop_exits: Vec::new(),
            str_count: 0,
            structs: HashMap::new(),
            enums: HashMap::new(),
            variants: HashMap::new(),
        }
    }

    fn block_terminated(&self) -> bool {
        self.builder
            .get_insert_block()
            .and_then(|b| b.get_terminator())
            .is_some()
    }

    /// Consume the codegen wrapper and return the LLVM module (e.g. for JIT).
    pub fn into_module(self) -> Module<'ctx> {
        self.module
    }

    pub fn compile_program(&mut self, prog: &Program, tyck: &TypeCk) -> Result<(), CodegenError> {
        self.structs = tyck.structs.clone();
        self.enums = tyck.enums.clone();
        self.variants = tyck.variants.clone();

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

        // Rc string runtime (see runtime/risp_rt.c)
        let void_ty = self.context.void_type();
        let from_ty = i8ptr_ty.fn_type(&[i8ptr_ty.into()], false);
        self.fns.insert(
            "risp_str_from_cstr".into(),
            self.module.add_function("risp_str_from_cstr", from_ty, None),
        );
        let retain_ty = i8ptr_ty.fn_type(&[i8ptr_ty.into()], false);
        self.fns.insert(
            "risp_str_retain".into(),
            self.module.add_function("risp_str_retain", retain_ty, None),
        );
        let release_ty = void_ty.fn_type(&[i8ptr_ty.into()], false);
        self.fns.insert(
            "risp_str_release".into(),
            self.module.add_function("risp_str_release", release_ty, None),
        );
        let concat_ty = i8ptr_ty.fn_type(&[i8ptr_ty.into(), i8ptr_ty.into()], false);
        self.fns.insert(
            "risp_str_concat".into(),
            self.module.add_function("risp_str_concat", concat_ty, None),
        );
        let len_ty = i32_ty.fn_type(&[i8ptr_ty.into()], false);
        self.fns.insert(
            "risp_str_len".into(),
            self.module.add_function("risp_str_len", len_ty, None),
        );
        let cstr_ty = i8ptr_ty.fn_type(&[i8ptr_ty.into()], false);
        self.fns.insert(
            "risp_str_cstr".into(),
            self.module.add_function("risp_str_cstr", cstr_ty, None),
        );

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
            params.iter().map(|t| self.basic_metadata(t)).collect();
        match ret {
            Type::Unit => self.context.void_type().fn_type(&param_tys, false),
            other => match self.llvm_basic(other) {
                BasicTypeEnum::IntType(t) => t.fn_type(&param_tys, false),
                BasicTypeEnum::FloatType(t) => t.fn_type(&param_tys, false),
                BasicTypeEnum::PointerType(t) => t.fn_type(&param_tys, false),
                BasicTypeEnum::StructType(t) => t.fn_type(&param_tys, false),
                _ => unreachable!(),
            },
        }
    }

    fn llvm_enum_ty(&self) -> StructType<'ctx> {
        self.context.struct_type(
            &[
                self.context.i32_type().into(),
                self.context.i64_type().into(),
            ],
            false,
        )
    }

    fn llvm_struct_ty(&self, name: &str) -> StructType<'ctx> {
        let sdef = &self.structs[name];
        let fields: Vec<BasicTypeEnum> = sdef
            .fields
            .iter()
            .map(|f| self.llvm_basic(&f.ty))
            .collect();
        self.context.struct_type(&fields, false)
    }

    fn llvm_basic(&self, t: &Type) -> BasicTypeEnum<'ctx> {
        match t {
            Type::I32 => self.context.i32_type().into(),
            Type::I64 => self.context.i64_type().into(),
            Type::F32 => self.context.f32_type().into(),
            Type::F64 => self.context.f64_type().into(),
            Type::Bool => self.context.bool_type().into(),
            Type::Str | Type::Array { .. } => self.context.ptr_type(AddressSpace::default()).into(),
            Type::Named(n) if self.structs.contains_key(n) => self.llvm_struct_ty(n).into(),
            Type::Named(n) if self.enums.contains_key(n) => self.llvm_enum_ty().into(),
            Type::Named(n) => panic!("unknown named type {n}"),
            Type::Unit => panic!("unit has no basic type"),
        }
    }

    fn basic_metadata(&self, t: &Type) -> BasicMetadataTypeEnum<'ctx> {
        match self.llvm_basic(t) {
            BasicTypeEnum::IntType(t) => t.into(),
            BasicTypeEnum::FloatType(t) => t.into(),
            BasicTypeEnum::PointerType(t) => t.into(),
            BasicTypeEnum::StructType(t) => t.into(),
            _ => unreachable!(),
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
                    prev.push(self.bind_local(fv, b)?);
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
                    let v = self.emit_expr(ex)?;
                    if ex.ty.as_ref() == Some(&Type::Str) {
                        if let Some(sv) = v {
                            self.rt_str_release(sv.into_pointer_value())?;
                        }
                    }
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
        // Drop local Rc strings before returning. The return value (if `str`) is
        // a separately owned reference produced by `emit_expr`.
        self.release_str_locals()?;
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
        // Release non-parameter str locals before overwriting params / looping.
        for (name, (ptr, ty)) in self.locals.clone() {
            if ty == Type::Str && !self.param_locals.contains_key(&name) {
                let ptr_ty = self.context.ptr_type(AddressSpace::default());
                let p = self
                    .builder
                    .build_load(ptr_ty, ptr, "tco.drop")
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?
                    .into_pointer_value();
                self.rt_str_release(p)?;
            }
        }
        for (name, v) in self.param_names.iter().zip(values) {
            let (ptr, ty) = self.param_locals[name].clone();
            if ty == Type::Str {
                self.store_str(ptr, v.into_pointer_value())?;
            } else {
                self.builder
                    .build_store(ptr, v)
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
            }
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
        if let Type::Array { elem, len } = ty {
            let at = llvm_array_type(self.context, elem, *len);
            return tmp_builder.build_alloca(at, name).unwrap();
        }
        let bt = self.llvm_basic(ty);
        let alloca = match bt {
            BasicTypeEnum::IntType(t) => tmp_builder.build_alloca(t, name).unwrap(),
            BasicTypeEnum::FloatType(t) => tmp_builder.build_alloca(t, name).unwrap(),
            BasicTypeEnum::PointerType(t) => tmp_builder.build_alloca(t, name).unwrap(),
            BasicTypeEnum::StructType(t) => tmp_builder.build_alloca(t, name).unwrap(),
            _ => panic!("unsupported alloca type"),
        };
        // Rc strings start as null so release-before-store is safe.
        if *ty == Type::Str {
            let null = self.context.ptr_type(AddressSpace::default()).const_null();
            tmp_builder.build_store(alloca, null).unwrap();
        }
        alloca
    }

    fn rt_call_ptr1(
        &self,
        name: &str,
        arg: PointerValue<'ctx>,
    ) -> Result<PointerValue<'ctx>, CodegenError> {
        let f = self.fns[name];
        let call = self
            .builder
            .build_call(f, &[arg.into()], name)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        Ok(call
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| CodegenError::Internal(format!("{name} returned void")))?
            .into_pointer_value())
    }

    fn rt_str_retain(&self, p: PointerValue<'ctx>) -> Result<PointerValue<'ctx>, CodegenError> {
        self.rt_call_ptr1("risp_str_retain", p)
    }

    fn rt_str_release(&self, p: PointerValue<'ctx>) -> Result<(), CodegenError> {
        let f = self.fns["risp_str_release"];
        self.builder
            .build_call(f, &[p.into()], "risp_str_release")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        Ok(())
    }

    fn rt_str_from_cstr(&self, cstr: PointerValue<'ctx>) -> Result<PointerValue<'ctx>, CodegenError> {
        self.rt_call_ptr1("risp_str_from_cstr", cstr)
    }

    /// Store an owned `str` into an alloca, releasing the previous value.
    fn store_str(
        &self,
        alloca: PointerValue<'ctx>,
        new_owned: PointerValue<'ctx>,
    ) -> Result<(), CodegenError> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let old = self
            .builder
            .build_load(ptr_ty, alloca, "oldstr")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?
            .into_pointer_value();
        self.rt_str_release(old)?;
        self.builder
            .build_store(alloca, new_owned)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        Ok(())
    }

    /// Load `str` from alloca and retain (returns owned).
    fn load_str_owned(&self, alloca: PointerValue<'ctx>) -> Result<PointerValue<'ctx>, CodegenError> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        let p = self
            .builder
            .build_load(ptr_ty, alloca, "loadstr")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?
            .into_pointer_value();
        self.rt_str_retain(p)
    }

    /// Release all local `str` slots (function exit).
    fn release_str_locals(&self) -> Result<(), CodegenError> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        for (ptr, ty) in self.locals.values() {
            if *ty == Type::Str {
                let p = self
                    .builder
                    .build_load(ptr_ty, *ptr, "dropstr")
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?
                    .into_pointer_value();
                self.rt_str_release(p)?;
            }
        }
        Ok(())
    }

    fn expr_ty(e: &Expr) -> Result<&Type, CodegenError> {
        e.ty.as_ref()
            .ok_or_else(|| CodegenError::Internal("expression missing type info (typeck not run?)".into()))
    }

    fn emit_expr(&mut self, e: &Expr) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        let ty = Self::expr_ty(e)?.clone();
        match &e.kind {
            ExprKind::Lit(l) => match l {
                Lit::Str(s) => {
                    let cstr = self.intern_str(s);
                    Ok(Some(self.rt_str_from_cstr(cstr)?.into()))
                }
                _ => Ok(Some(self.emit_lit(l, &ty))),
            },
            ExprKind::Var(name) => {
                if let Some((ptr, vty)) = self.locals.get(name).cloned() {
                    // Array locals store the alloca address itself (no load).
                    if matches!(vty, Type::Array { .. }) {
                        Ok(Some(ptr.into()))
                    } else if vty == Type::Str {
                        Ok(Some(self.load_str_owned(ptr)?.into()))
                    } else {
                        let bt = self.llvm_basic(&vty);
                        let v = self
                            .builder
                            .build_load(bt, ptr, name)
                            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                        Ok(Some(v))
                    }
                } else if let Some((cty, v)) = self.consts.get(name).cloned() {
                    if cty == Type::Str {
                        // Const holds a static cstr; build an owned Rc string.
                        Ok(Some(
                            self.rt_str_from_cstr(v.into_pointer_value())?.into(),
                        ))
                    } else {
                        Ok(Some(v))
                    }
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
                let then_ok = !self.block_terminated();
                if then_ok {
                    self.builder
                        .build_unconditional_branch(merge_bb)
                        .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                }

                self.builder.position_at_end(else_bb);
                let ev = self.emit_expr(else_branch)?;
                let else_end = self.builder.get_insert_block().unwrap();
                let else_ok = !self.block_terminated();
                if else_ok {
                    self.builder
                        .build_unconditional_branch(merge_bb)
                        .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                }

                self.builder.position_at_end(merge_bb);
                if !then_ok && !else_ok {
                    self.builder
                        .build_unreachable()
                        .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                    return Ok(None);
                }

                if ty == Type::Unit {
                    return Ok(None);
                }

                let bt = self.llvm_basic(&ty);
                let phi = self
                    .builder
                    .build_phi(bt, "iftmp")
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                if then_ok {
                    let tv = tv.ok_or_else(|| CodegenError::Internal("if then no value".into()))?;
                    phi.add_incoming(&[(&tv, then_end)]);
                }
                if else_ok {
                    let ev = ev.ok_or_else(|| CodegenError::Internal("if else no value".into()))?;
                    phi.add_incoming(&[(&ev, else_end)]);
                }
                Ok(Some(phi.as_basic_value()))
            }
            ExprKind::Let { bindings, body } => {
                // save shadowed
                let mut prev: Vec<(String, Option<(PointerValue<'ctx>, Type)>)> = Vec::new();
                let fv = self.builder.get_insert_block().unwrap().get_parent().unwrap();
                for b in bindings {
                    prev.push(self.bind_local(fv, b)?);
                }
                let result = self.emit_expr(body)?;
                // Drop this scope's str bindings when the body did not exit via break.
                if !self.block_terminated() {
                    self.release_let_str_bindings(bindings)?;
                }
                for (name, p) in prev.into_iter().rev() {
                    match p {
                        Some(x) => {
                            self.locals.insert(name, x);
                        }
                        None => {
                            self.locals.remove(&name);
                        }
                    }
                }
                Ok(result)
            }
            ExprKind::Do(exprs) => {
                if exprs.is_empty() {
                    return Ok(None);
                }
                let last_i = exprs.len() - 1;
                let mut last: Option<BasicValueEnum<'ctx>> = None;
                for (i, ex) in exprs.iter().enumerate() {
                    if self.block_terminated() {
                        break;
                    }
                    let v = self.emit_expr(ex)?;
                    if i != last_i {
                        if ex.ty.as_ref() == Some(&Type::Str) {
                            if let Some(sv) = v {
                                self.rt_str_release(sv.into_pointer_value())?;
                            }
                        }
                    } else {
                        last = v;
                    }
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
                let (ptr, vty) = self
                    .locals
                    .get(name)
                    .cloned()
                    .ok_or_else(|| CodegenError::Internal(format!("set! unresolved {name}")))?;
                if matches!(vty, Type::Array { .. }) {
                    // Rebind the local to a new array pointer (no element-wise copy).
                    self.locals
                        .insert(name.clone(), (v.into_pointer_value(), vty));
                } else if vty == Type::Str {
                    self.store_str(ptr, v.into_pointer_value())?;
                } else {
                    self.builder
                        .build_store(ptr, v)
                        .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                }
                Ok(None)
            }
            ExprKind::While { cond, body } => {
                self.emit_while(cond, body)?;
                Ok(None)
            }
            ExprKind::Loop { body } => {
                self.emit_loop(body)?;
                Ok(None)
            }
            ExprKind::Break => {
                let exit = *self
                    .loop_exits
                    .last()
                    .ok_or_else(|| CodegenError::Internal("break without loop".into()))?;
                self.builder
                    .build_unconditional_branch(exit)
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                Ok(None)
            }
            ExprKind::ArrayLit { elem_ty, elems } => {
                Ok(Some(self.emit_array_lit(elem_ty, elems)?.into()))
            }
            ExprKind::Field { base, field } => Ok(Some(self.emit_field(base, field)?)),
            ExprKind::Match { scrutinee, arms } => self.emit_match(scrutinee, arms, &ty),
            ExprKind::Call { callee, args } => self.emit_call(callee, args, &ty),
        }
    }

    fn emit_field(&mut self, base: &Expr, field: &str) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let base_ty = Self::expr_ty(base)?.clone();
        let Type::Named(ref n) = base_ty else {
            return Err(CodegenError::Internal("field on non-struct".into()));
        };
        let sdef = self
            .structs
            .get(n)
            .ok_or_else(|| CodegenError::Internal(format!("no struct {n}")))?
            .clone();
        let idx = sdef
            .fields
            .iter()
            .position(|f| f.name == field)
            .ok_or_else(|| CodegenError::Internal(format!("no field {field}")))?;
        let sv = self
            .emit_expr(base)?
            .ok_or_else(|| CodegenError::Internal("field base".into()))?
            .into_struct_value();
        self.builder
            .build_extract_value(sv, idx as u32, field)
            .map_err(|e| CodegenError::Llvm(e.to_string()))
    }

    fn emit_struct_lit(
        &mut self,
        name: &str,
        args: &[Expr],
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let sdef = self.structs[name].clone();
        let st = self.llvm_struct_ty(name);
        let mut agg = st.get_undef();
        for (i, (field, arg)) in sdef.fields.iter().zip(args.iter()).enumerate() {
            let v = self
                .emit_expr(arg)?
                .ok_or_else(|| CodegenError::Internal("struct field".into()))?;
            agg = self
                .builder
                .build_insert_value(agg, v, i as u32, &field.name)
                .map_err(|e| CodegenError::Llvm(e.to_string()))?
                .into_struct_value();
        }
        Ok(agg.into())
    }

    fn emit_variant_lit(
        &mut self,
        info: &VariantInfo,
        args: &[Expr],
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let et = self.llvm_enum_ty();
        let tag = self.context.i32_type().const_int(info.tag as u64, false);
        let payload_bits = if let Some(pty) = &info.payload {
            let v = self
                .emit_expr(&args[0])?
                .ok_or_else(|| CodegenError::Internal("variant payload".into()))?;
            self.pack_payload(v, pty)?
        } else {
            self.context.i64_type().const_int(0, false)
        };
        let mut agg = et.get_undef();
        agg = self
            .builder
            .build_insert_value(agg, tag, 0, "tag")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?
            .into_struct_value();
        agg = self
            .builder
            .build_insert_value(agg, payload_bits, 1, "payload")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?
            .into_struct_value();
        Ok(agg.into())
    }

    fn pack_payload(
        &self,
        v: BasicValueEnum<'ctx>,
        ty: &Type,
    ) -> Result<IntValue<'ctx>, CodegenError> {
        let i64_ty = self.context.i64_type();
        match ty {
            Type::I64 => Ok(v.into_int_value()),
            Type::I32 | Type::Bool => Ok(self
                .builder
                .build_int_z_extend(v.into_int_value(), i64_ty, "zext")
                .map_err(|e| CodegenError::Llvm(e.to_string()))?),
            Type::F64 => Ok(self
                .builder
                .build_bit_cast(v.into_float_value(), i64_ty, "f64bits")
                .map_err(|e| CodegenError::Llvm(e.to_string()))?
                .into_int_value()),
            Type::F32 => {
                let bits32 = self
                    .builder
                    .build_bit_cast(v.into_float_value(), self.context.i32_type(), "f32bits")
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?
                    .into_int_value();
                Ok(self
                    .builder
                    .build_int_z_extend(bits32, i64_ty, "f32zext")
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?)
            }
            other => Err(CodegenError::Internal(format!(
                "cannot pack payload type {other}"
            ))),
        }
    }

    fn unpack_payload(
        &self,
        bits: IntValue<'ctx>,
        ty: &Type,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        match ty {
            Type::I64 => Ok(bits.into()),
            Type::I32 => Ok(self
                .builder
                .build_int_truncate(bits, self.context.i32_type(), "trunc")
                .map_err(|e| CodegenError::Llvm(e.to_string()))?
                .into()),
            Type::Bool => Ok(self
                .builder
                .build_int_truncate(bits, self.context.bool_type(), "truncb")
                .map_err(|e| CodegenError::Llvm(e.to_string()))?
                .into()),
            Type::F64 => Ok(self
                .builder
                .build_bit_cast(bits, self.context.f64_type(), "f64")
                .map_err(|e| CodegenError::Llvm(e.to_string()))?),
            Type::F32 => {
                let lo = self
                    .builder
                    .build_int_truncate(bits, self.context.i32_type(), "f32lo")
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                Ok(self
                    .builder
                    .build_bit_cast(lo, self.context.f32_type(), "f32")
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?)
            }
            other => Err(CodegenError::Internal(format!(
                "cannot unpack payload type {other}"
            ))),
        }
    }

    fn emit_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        result_ty: &Type,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        let ev = self
            .emit_expr(scrutinee)?
            .ok_or_else(|| CodegenError::Internal("match scrutinee".into()))?
            .into_struct_value();
        let tag = self
            .builder
            .build_extract_value(ev, 0, "mtag")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?
            .into_int_value();
        let payload = self
            .builder
            .build_extract_value(ev, 1, "mpayload")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?
            .into_int_value();

        let fv = self.builder.get_insert_block().unwrap().get_parent().unwrap();
        let merge_bb = self.context.append_basic_block(fv, "match.end");
        let else_bb = self.context.append_basic_block(fv, "match.unreachable");

        let mut cases = Vec::with_capacity(arms.len());
        let mut arm_bbs = Vec::with_capacity(arms.len());
        for arm in arms {
            let info = &self.variants[&arm.variant];
            let bb = self
                .context
                .append_basic_block(fv, &format!("match.{}", arm.variant));
            let tag_c = self.context.i32_type().const_int(info.tag as u64, false);
            cases.push((tag_c, bb));
            arm_bbs.push(bb);
        }

        self.builder
            .build_switch(tag, else_bb, &cases)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;

        let mut incoming: Vec<(BasicValueEnum<'ctx>, BasicBlock<'ctx>)> = Vec::new();
        for (arm, bb) in arms.iter().zip(arm_bbs.into_iter()) {
            self.builder.position_at_end(bb);
            let info = self.variants[&arm.variant].clone();
            let prev = if let (Some(pty), Some(bname)) = (&info.payload, &arm.binding) {
                let unpacked = self.unpack_payload(payload, pty)?;
                let alloca = self.create_entry_alloca(fv, bname, pty);
                self.builder
                    .build_store(alloca, unpacked)
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                self.locals.insert(bname.clone(), (alloca, pty.clone()))
            } else {
                None
            };

            let body_v = self.emit_expr(&arm.body)?;
            let end_bb = self.builder.get_insert_block().unwrap();
            let reached_merge = !self.block_terminated();
            if reached_merge {
                self.builder
                    .build_unconditional_branch(merge_bb)
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
            }

            if let Some(bname) = &arm.binding {
                match prev {
                    Some(x) => {
                        self.locals.insert(bname.clone(), x);
                    }
                    None => {
                        self.locals.remove(bname);
                    }
                }
            }

            if reached_merge && *result_ty != Type::Unit {
                let v = body_v.ok_or_else(|| CodegenError::Internal("match arm value".into()))?;
                incoming.push((v, end_bb));
            }
        }

        self.builder.position_at_end(else_bb);
        self.builder
            .build_unreachable()
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;

        self.builder.position_at_end(merge_bb);
        if *result_ty == Type::Unit {
            return Ok(None);
        }
        let bt = self.llvm_basic(result_ty);
        let phi = self
            .builder
            .build_phi(bt, "matchtmp")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        for (v, bb) in &incoming {
            phi.add_incoming(&[(v, *bb)]);
        }
        Ok(Some(phi.as_basic_value()))
    }

    fn release_let_str_bindings(&self, bindings: &[Binding]) -> Result<(), CodegenError> {
        let ptr_ty = self.context.ptr_type(AddressSpace::default());
        for b in bindings.iter().rev() {
            if b.ty != Type::Str {
                continue;
            }
            let Some((ptr, _)) = self.locals.get(&b.name).cloned() else {
                continue;
            };
            let p = self
                .builder
                .build_load(ptr_ty, ptr, "letscope.drop")
                .map_err(|e| CodegenError::Llvm(e.to_string()))?
                .into_pointer_value();
            self.rt_str_release(p)?;
            let null = ptr_ty.const_null();
            self.builder
                .build_store(ptr, null)
                .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        }
        Ok(())
    }

    /// Bind a `let` local. Arrays keep the pointer from the initializer (no copy).
    fn bind_local(
        &mut self,
        fv: FunctionValue<'ctx>,
        b: &Binding,
    ) -> Result<(String, Option<(PointerValue<'ctx>, Type)>), CodegenError> {
        let v = self
            .emit_expr(&b.value)?
            .ok_or_else(|| CodegenError::Internal("let value".into()))?;
        let ptr = if matches!(b.ty, Type::Array { .. }) {
            v.into_pointer_value()
        } else if b.ty == Type::Str {
            let alloca = self.create_entry_alloca(fv, &b.name, &b.ty);
            self.store_str(alloca, v.into_pointer_value())?;
            alloca
        } else {
            let alloca = self.create_entry_alloca(fv, &b.name, &b.ty);
            self.builder
                .build_store(alloca, v)
                .map_err(|e| CodegenError::Llvm(e.to_string()))?;
            alloca
        };
        Ok((
            b.name.clone(),
            self.locals.insert(b.name.clone(), (ptr, b.ty.clone())),
        ))
    }

    fn emit_array_lit(
        &mut self,
        elem_ty: &Type,
        elems: &[Expr],
    ) -> Result<PointerValue<'ctx>, CodegenError> {
        let len = elems.len() as u32;
        let fv = self
            .builder
            .get_insert_block()
            .unwrap()
            .get_parent()
            .unwrap();
        let arr_ty = llvm_array_type(self.context, elem_ty, len);
        let alloca = {
            let entry = fv.get_first_basic_block().unwrap();
            let tmp_builder = self.context.create_builder();
            match entry.get_first_instruction() {
                Some(inst) => tmp_builder.position_before(&inst),
                None => tmp_builder.position_at_end(entry),
            }
            tmp_builder.build_alloca(arr_ty, "arrtmp").unwrap()
        };
        let zero = self.context.i32_type().const_int(0, false);
        for (i, el) in elems.iter().enumerate() {
            let v = self
                .emit_expr(el)?
                .ok_or_else(|| CodegenError::Internal("array elem".into()))?;
            let idx = self.context.i32_type().const_int(i as u64, false);
            let ep = unsafe {
                self.builder
                    .build_in_bounds_gep(arr_ty, alloca, &[zero, idx], "elemptr")
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?
            };
            self.builder
                .build_store(ep, v)
                .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        }
        Ok(alloca)
    }

    fn emit_aget(&mut self, args: &[Expr]) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let arr_ast_ty = Self::expr_ty(&args[0])?.clone();
        let Type::Array { elem, len } = arr_ast_ty else {
            return Err(CodegenError::Internal("aget on non-array".into()));
        };
        let arr_ptr = self
            .emit_expr(&args[0])?
            .ok_or_else(|| CodegenError::Internal("aget arr".into()))?
            .into_pointer_value();
        let idx = self
            .emit_expr(&args[1])?
            .ok_or_else(|| CodegenError::Internal("aget idx".into()))?
            .into_int_value();
        let arr_ty = llvm_array_type(self.context, &elem, len);
        let zero = self.context.i32_type().const_int(0, false);
        let ep = unsafe {
            self.builder
                .build_in_bounds_gep(arr_ty, arr_ptr, &[zero, idx], "agetptr")
                .map_err(|e| CodegenError::Llvm(e.to_string()))?
        };
        let bt = self.llvm_basic(&elem);
        self.builder
            .build_load(bt, ep, "aget")
            .map_err(|e| CodegenError::Llvm(e.to_string()))
    }

    fn emit_aset(&mut self, args: &[Expr]) -> Result<(), CodegenError> {
        let arr_ast_ty = Self::expr_ty(&args[0])?.clone();
        let Type::Array { elem, len } = arr_ast_ty else {
            return Err(CodegenError::Internal("aset! on non-array".into()));
        };
        let arr_ptr = self
            .emit_expr(&args[0])?
            .ok_or_else(|| CodegenError::Internal("aset! arr".into()))?
            .into_pointer_value();
        let idx = self
            .emit_expr(&args[1])?
            .ok_or_else(|| CodegenError::Internal("aset! idx".into()))?
            .into_int_value();
        let val = self
            .emit_expr(&args[2])?
            .ok_or_else(|| CodegenError::Internal("aset! val".into()))?;
        let arr_ty = llvm_array_type(self.context, &elem, len);
        let zero = self.context.i32_type().const_int(0, false);
        let ep = unsafe {
            self.builder
                .build_in_bounds_gep(arr_ty, arr_ptr, &[zero, idx], "asetptr")
                .map_err(|e| CodegenError::Llvm(e.to_string()))?
        };
        self.builder
            .build_store(ep, val)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        Ok(())
    }

    fn emit_alen(
        &mut self,
        args: &[Expr],
    ) -> Result<inkwell::values::IntValue<'ctx>, CodegenError> {
        let arr_ast_ty = Self::expr_ty(&args[0])?.clone();
        let Type::Array { len, .. } = arr_ast_ty else {
            return Err(CodegenError::Internal("alen on non-array".into()));
        };
        // Evaluate for sequencing; length is known statically.
        let _ = self.emit_expr(&args[0])?;
        Ok(self.context.i32_type().const_int(len as u64, false))
    }

    fn emit_while(&mut self, cond: &Expr, body: &Expr) -> Result<(), CodegenError> {
        let fv = self
            .builder
            .get_insert_block()
            .unwrap()
            .get_parent()
            .unwrap();
        let cond_bb = self.context.append_basic_block(fv, "while.cond");
        let body_bb = self.context.append_basic_block(fv, "while.body");
        let end_bb = self.context.append_basic_block(fv, "while.end");

        self.builder
            .build_unconditional_branch(cond_bb)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;

        self.builder.position_at_end(cond_bb);
        let cv = self
            .emit_expr(cond)?
            .ok_or_else(|| CodegenError::Internal("while cond".into()))?
            .into_int_value();
        self.builder
            .build_conditional_branch(cv, body_bb, end_bb)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;

        self.loop_exits.push(end_bb);
        self.builder.position_at_end(body_bb);
        let _ = self.emit_expr(body)?;
        if !self.block_terminated() {
            self.builder
                .build_unconditional_branch(cond_bb)
                .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        }
        self.loop_exits.pop();

        self.builder.position_at_end(end_bb);
        Ok(())
    }

    fn emit_loop(&mut self, body: &Expr) -> Result<(), CodegenError> {
        let fv = self
            .builder
            .get_insert_block()
            .unwrap()
            .get_parent()
            .unwrap();
        let body_bb = self.context.append_basic_block(fv, "loop.body");
        let end_bb = self.context.append_basic_block(fv, "loop.end");

        self.builder
            .build_unconditional_branch(body_bb)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;

        self.loop_exits.push(end_bb);
        self.builder.position_at_end(body_bb);
        let _ = self.emit_expr(body)?;
        if !self.block_terminated() {
            self.builder
                .build_unconditional_branch(body_bb)
                .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        }
        self.loop_exits.pop();

        self.builder.position_at_end(end_bb);
        Ok(())
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
            "str-concat" => Ok(Some(self.emit_str_concat(args)?.into())),
            "str-len" => Ok(Some(self.emit_str_len(args)?.into())),
            "aget" => Ok(Some(self.emit_aget(args)?)),
            "aset!" => {
                self.emit_aset(args)?;
                Ok(None)
            }
            "alen" => Ok(Some(self.emit_alen(args)?.into())),
            _ if self.structs.contains_key(callee) => {
                Ok(Some(self.emit_struct_lit(callee, args)?))
            }
            _ if self.variants.contains_key(callee) => {
                let info = self.variants[callee].clone();
                Ok(Some(self.emit_variant_lit(&info, args)?))
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

        if ty == Type::Str {
            let owned = v.into_pointer_value();
            let cstr = self.rt_call_ptr1("risp_str_cstr", owned)?;
            if newline {
                let puts = self.fns["__puts"];
                self.builder
                    .build_call(puts, &[cstr.into()], "putscall")
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
            } else {
                let fmt = self.intern_str("%s");
                let printf = self.fns["__printf"];
                self.builder
                    .build_call(printf, &[fmt.into(), cstr.into()], "printfcall")
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
            }
            self.rt_str_release(owned)?;
            return Ok(());
        }

        let printf = self.fns["__printf"];
        let (fmt, arg_v): (&str, BasicMetadataValueEnum) = match ty {
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
            Type::Str | Type::Unit | Type::Array { .. } | Type::Named(_) => {
                return Err(CodegenError::Internal("cannot print this type".into()));
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

    fn emit_str_concat(&mut self, args: &[Expr]) -> Result<PointerValue<'ctx>, CodegenError> {
        let a = self
            .emit_expr(&args[0])?
            .ok_or_else(|| CodegenError::Internal("str-concat a".into()))?
            .into_pointer_value();
        let b = self
            .emit_expr(&args[1])?
            .ok_or_else(|| CodegenError::Internal("str-concat b".into()))?
            .into_pointer_value();
        let f = self.fns["risp_str_concat"];
        let call = self
            .builder
            .build_call(f, &[a.into(), b.into()], "strconcat")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        let out = call
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| CodegenError::Internal("str-concat void".into()))?
            .into_pointer_value();
        self.rt_str_release(a)?;
        self.rt_str_release(b)?;
        Ok(out)
    }

    fn emit_str_len(
        &mut self,
        args: &[Expr],
    ) -> Result<inkwell::values::IntValue<'ctx>, CodegenError> {
        let s = self
            .emit_expr(&args[0])?
            .ok_or_else(|| CodegenError::Internal("str-len".into()))?
            .into_pointer_value();
        let f = self.fns["risp_str_len"];
        let call = self
            .builder
            .build_call(f, &[s.into()], "strlen")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        let n = call
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| CodegenError::Internal("str-len void".into()))?
            .into_int_value();
        self.rt_str_release(s)?;
        Ok(n)
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
            // For `def` constants: store a static cstr; users convert via from_cstr.
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

fn llvm_array_type<'ctx>(
    ctx: &'ctx Context,
    elem: &Type,
    len: u32,
) -> inkwell::types::ArrayType<'ctx> {
    match elem {
        Type::I32 => ctx.i32_type().array_type(len),
        Type::I64 => ctx.i64_type().array_type(len),
        Type::F32 => ctx.f32_type().array_type(len),
        Type::F64 => ctx.f64_type().array_type(len),
        Type::Bool => ctx.bool_type().array_type(len),
        other => panic!("unsupported array element type {other}"),
    }
}
