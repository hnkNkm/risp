//! LLVM IR codegen using inkwell.

use crate::ast::*;
use crate::typeck::{TypeCk, VariantInfo, mangle_method};
use inkwell::AddressSpace;
use inkwell::FloatPredicate;
use inkwell::IntPredicate;
use inkwell::basic_block::BasicBlock;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum, FunctionType, StructType};
use inkwell::values::{BasicMetadataValueEnum, BasicValueEnum, FunctionValue, IntValue, PointerValue};
use std::collections::HashMap;
use thiserror::Error;

/// Whether loading a local consumes it (move) or only observes it (place).
#[derive(Clone, Copy, PartialEq, Eq)]
enum ValueUse {
    /// Consuming use: take move-types; retain Rc/str.
    Move,
    /// Non-consuming: load without taking / without retain.
    Place,
}

fn is_move_type(ty: &Type) -> bool {
    matches!(ty, Type::Named(_) | Type::Box(_) | Type::Vec { .. })
}

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
    externs: HashMap<String, crate::typeck::FnSig>,
    /// Per-named-type drop glue (`void (T)`). Breaks compile-time recursion for
    /// recursive ADTs like `(enum List Nil Cons (Box List))`.
    drop_fns: HashMap<String, FunctionValue<'ctx>>,
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
            externs: HashMap::new(),
            drop_fns: HashMap::new(),
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
        self.externs = tyck.externs.clone();
        self.drop_fns.clear();

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
        let i64_ty = self.context.i64_type();
        let box_alloc_ty = i8ptr_ty.fn_type(&[i64_ty.into()], false);
        self.fns.insert(
            "risp_box_alloc".into(),
            self.module.add_function("risp_box_alloc", box_alloc_ty, None),
        );
        let box_free_ty = void_ty.fn_type(&[i8ptr_ty.into()], false);
        self.fns.insert(
            "risp_box_free".into(),
            self.module.add_function("risp_box_free", box_free_ty, None),
        );

        // Vec i32
        let vec_new_ty = i8ptr_ty.fn_type(&[], false);
        self.fns.insert(
            "risp_vec_i32_new".into(),
            self.module.add_function("risp_vec_i32_new", vec_new_ty, None),
        );
        let vec_push_ty = void_ty.fn_type(&[i8ptr_ty.into(), i32_ty.into()], false);
        self.fns.insert(
            "risp_vec_i32_push".into(),
            self.module.add_function("risp_vec_i32_push", vec_push_ty, None),
        );
        let vec_get_ty = i32_ty.fn_type(&[i8ptr_ty.into(), i32_ty.into()], false);
        self.fns.insert(
            "risp_vec_i32_get".into(),
            self.module.add_function("risp_vec_i32_get", vec_get_ty, None),
        );
        let vec_len_ty = i32_ty.fn_type(&[i8ptr_ty.into()], false);
        self.fns.insert(
            "risp_vec_i32_len".into(),
            self.module.add_function("risp_vec_i32_len", vec_len_ty, None),
        );
        let vec_free_ty = void_ty.fn_type(&[i8ptr_ty.into()], false);
        self.fns.insert(
            "risp_vec_i32_free".into(),
            self.module.add_function("risp_vec_i32_free", vec_free_ty, None),
        );

        // Generic Rc / Weak
        let rc_alloc_ty = i8ptr_ty.fn_type(&[i64_ty.into()], false);
        self.fns.insert(
            "risp_rc_alloc".into(),
            self.module.add_function("risp_rc_alloc", rc_alloc_ty, None),
        );
        self.fns.insert(
            "risp_rc_retain".into(),
            self.module.add_function("risp_rc_retain", retain_ty, None),
        );
        let rc_rel_ty = i32_ty.fn_type(&[i8ptr_ty.into()], false);
        self.fns.insert(
            "risp_rc_release_strong".into(),
            self.module
                .add_function("risp_rc_release_strong", rc_rel_ty, None),
        );
        self.fns.insert(
            "risp_rc_after_payload_drop".into(),
            self.module
                .add_function("risp_rc_after_payload_drop", release_ty, None),
        );
        self.fns.insert(
            "risp_weak_from".into(),
            self.module.add_function("risp_weak_from", retain_ty, None),
        );
        self.fns.insert(
            "risp_weak_upgrade".into(),
            self.module.add_function("risp_weak_upgrade", retain_ty, None),
        );
        self.fns.insert(
            "risp_weak_release".into(),
            self.module
                .add_function("risp_weak_release", release_ty, None),
        );

        // Declare + emit drop glue for recursive ADTs before user code.
        self.declare_drop_fns();
        self.emit_drop_fn_bodies()?;

        // declare all user / extern / impl methods first (allow forward refs)
        for it in &prog.items {
            match it {
                TopLevel::Function(f) => {
                    let params: Vec<Type> = f.params.iter().map(|p| p.ty.clone()).collect();
                    let fn_ty = self.fn_type_for_decl(&params, &f.ret, false);
                    let fv = self.module.add_function(&f.name, fn_ty, None);
                    self.fns.insert(f.name.clone(), fv);
                    self.fn_types.insert(f.name.clone(), (params, f.ret.clone()));
                }
                TopLevel::Extern(e) => {
                    let params: Vec<Type> = e.params.iter().map(|p| p.ty.clone()).collect();
                    let fn_ty = self.fn_type_for_decl(&params, &e.ret, true);
                    let fv = self.module.add_function(&e.name, fn_ty, None);
                    self.fns.insert(e.name.clone(), fv);
                    self.fn_types
                        .insert(e.name.clone(), (params, e.ret.clone()));
                }
                TopLevel::Impl(ib) => {
                    for m in &ib.methods {
                        let mangled = mangle_method(&ib.trait_name, &ib.for_ty, &m.name);
                        let params: Vec<Type> = m.params.iter().map(|p| p.ty.clone()).collect();
                        let fn_ty = self.fn_type_for_decl(&params, &m.ret, false);
                        let fv = self.module.add_function(&mangled, fn_ty, None);
                        self.fns.insert(mangled.clone(), fv);
                        self.fn_types
                            .insert(mangled, (params, m.ret.clone()));
                    }
                }
                _ => {}
            }
        }

        // emit consts as globals
        for it in &prog.items {
            if let TopLevel::Const(c) = it {
                let v = self.const_eval(&c.value, &c.ty)?;
                self.consts.insert(c.name.clone(), (c.ty.clone(), v));
            }
        }

        // emit function / impl method bodies
        for it in &prog.items {
            match it {
                TopLevel::Function(f) => {
                    self.emit_function(f)?;
                }
                TopLevel::Impl(ib) => {
                    for m in &ib.methods {
                        let mangled = mangle_method(&ib.trait_name, &ib.for_ty, &m.name);
                        let synthetic = Function {
                            name: mangled,
                            params: m.params.clone(),
                            ret: m.ret.clone(),
                            body: m.body.clone(),
                            span: m.span,
                        };
                        self.emit_function(&synthetic)?;
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// `extern_c`: `str` parameters/returns lower to `i8*` (C string).
    fn fn_type_for_decl(&self, params: &[Type], ret: &Type, extern_c: bool) -> FunctionType<'ctx> {
        let param_tys: Vec<BasicMetadataTypeEnum> = params
            .iter()
            .map(|t| {
                if extern_c && *t == Type::Str {
                    self.context.ptr_type(AddressSpace::default()).into()
                } else {
                    self.basic_metadata(t)
                }
            })
            .collect();
        match ret {
            Type::Unit => self.context.void_type().fn_type(&param_tys, false),
            Type::Str if extern_c => self
                .context
                .ptr_type(AddressSpace::default())
                .fn_type(&param_tys, false),
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
            Type::Str
            | Type::Array { .. }
            | Type::Box(_)
            | Type::Vec { .. }
            | Type::Rc(_)
            | Type::Weak(_) => self.context.ptr_type(AddressSpace::default()).into(),
            Type::Named(n) if self.structs.contains_key(n) => self.llvm_struct_ty(n).into(),
            Type::Named(n) if self.enums.contains_key(n) => self.llvm_enum_ty().into(),
            Type::Named(n) => panic!("unknown named type {n}"),
            // Shared borrow: LLVM pointer to the pointee.
            Type::Ref(_) => self.context.ptr_type(AddressSpace::default()).into(),
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
                    if let (Some(dty), Some(sv)) = (ex.ty.as_ref(), v) {
                        if self.needs_drop(dty) {
                            self.emit_drop(sv, dty)?;
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
        // Drop owned locals before returning. The return value (if needs_drop) is
        // a separately owned reference produced by `emit_expr`.
        self.release_drop_locals()?;
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
        // Release non-parameter owned locals before overwriting params / looping.
        for (name, (ptr, ty)) in self.locals.clone() {
            if self.needs_drop(&ty) && !self.param_locals.contains_key(&name) {
                let bt = self.llvm_basic(&ty);
                let old = self
                    .builder
                    .build_load(bt, ptr, "tco.drop")
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                self.emit_drop(old, &ty)?;
            }
        }
        for (name, v) in self.param_names.iter().zip(values) {
            let (ptr, ty) = self.param_locals[name].clone();
            self.store_owned(ptr, v, &ty)?;
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
        // Droppable slots start zeroed so release-before-store is safe.
        if self.needs_drop(ty) {
            tmp_builder
                .build_store(alloca, self.zero_owned(ty))
                .unwrap();
        }
        alloca
    }

    /// Types whose values own heap resources and must be dropped.
    fn needs_drop(&self, ty: &Type) -> bool {
        match ty {
            Type::Str | Type::Box(_) | Type::Vec { .. } | Type::Rc(_) | Type::Weak(_) => true,
            Type::Named(n) if self.structs.contains_key(n) => self.structs[n]
                .fields
                .iter()
                .any(|f| self.needs_drop(&f.ty)),
            Type::Named(n) if self.enums.contains_key(n) => self.enums[n]
                .variants
                .iter()
                .any(|v| v.payload.as_ref().is_some_and(|p| self.needs_drop(p))),
            _ => false,
        }
    }

    fn zero_owned(&self, ty: &Type) -> BasicValueEnum<'ctx> {
        match ty {
            Type::Str | Type::Box(_) | Type::Vec { .. } | Type::Rc(_) | Type::Weak(_) => self
                .context
                .ptr_type(AddressSpace::default())
                .const_null()
                .into(),
            Type::Named(n) if self.structs.contains_key(n) => {
                self.llvm_struct_ty(n).const_zero().into()
            }
            Type::Named(n) if self.enums.contains_key(n) => self.llvm_enum_ty().const_zero().into(),
            other => panic!("zero_owned on non-drop type {other}"),
        }
    }

    fn declare_drop_fns(&mut self) {
        let void_ty = self.context.void_type();
        let mut names: Vec<String> = self
            .structs
            .keys()
            .chain(self.enums.keys())
            .cloned()
            .collect();
        names.sort();
        for name in names {
            let ty = Type::Named(name.clone());
            if !self.needs_drop(&ty) {
                continue;
            }
            let bt = self.llvm_basic(&ty);
            let ft = void_ty.fn_type(&[bt.into()], false);
            let fv = self
                .module
                .add_function(&format!("__risp_drop_{name}"), ft, None);
            self.drop_fns.insert(name, fv);
        }
    }

    fn emit_drop_fn_bodies(&self) -> Result<(), CodegenError> {
        for (name, fv) in &self.drop_fns {
            let entry = self.context.append_basic_block(*fv, "entry");
            // Re-enter via a temporary builder position; restore later.
            let saved = self.builder.get_insert_block();
            self.builder.position_at_end(entry);
            let arg = fv
                .get_nth_param(0)
                .ok_or_else(|| CodegenError::Internal("drop fn arg".into()))?;
            if self.structs.contains_key(name) {
                self.emit_drop_struct_fields(arg.into_struct_value(), name)?;
            } else {
                self.emit_drop_enum(arg.into_struct_value(), name)?;
            }
            self.builder
                .build_return(None)
                .map_err(|e| CodegenError::Llvm(e.to_string()))?;
            if let Some(bb) = saved {
                self.builder.position_at_end(bb);
            }
        }
        Ok(())
    }

    fn emit_drop_struct_fields(
        &self,
        sv: inkwell::values::StructValue<'ctx>,
        name: &str,
    ) -> Result<(), CodegenError> {
        let sdef = self.structs[name].clone();
        for (i, f) in sdef.fields.iter().enumerate() {
            if self.needs_drop(&f.ty) {
                let fv = self
                    .builder
                    .build_extract_value(sv, i as u32, &f.name)
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                self.emit_drop(fv, &f.ty)?;
            }
        }
        Ok(())
    }

    fn emit_drop(&self, v: BasicValueEnum<'ctx>, ty: &Type) -> Result<(), CodegenError> {
        match ty {
            Type::Str => self.rt_str_release(v.into_pointer_value()),
            Type::Box(inner) => self.emit_drop_box(v.into_pointer_value(), inner),
            Type::Vec { .. } => self.emit_drop_vec(v.into_pointer_value()),
            Type::Rc(inner) => self.emit_drop_rc(v.into_pointer_value(), inner),
            Type::Weak(_) => self.emit_drop_weak(v.into_pointer_value()),
            Type::Named(n) if self.drop_fns.contains_key(n) => {
                let f = self.drop_fns[n];
                self.builder
                    .build_call(f, &[v.into()], &format!("drop.{n}"))
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                Ok(())
            }
            Type::Named(n) if self.structs.contains_key(n) => {
                self.emit_drop_struct_fields(v.into_struct_value(), n)
            }
            Type::Named(n) if self.enums.contains_key(n) => {
                self.emit_drop_enum(v.into_struct_value(), n)
            }
            _ => Ok(()),
        }
    }

    fn emit_drop_vec(&self, p: PointerValue<'ctx>) -> Result<(), CodegenError> {
        let f = self.fns["risp_vec_i32_free"];
        self.builder
            .build_call(f, &[p.into()], "vec.free")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        Ok(())
    }

    fn emit_drop_weak(&self, p: PointerValue<'ctx>) -> Result<(), CodegenError> {
        let f = self.fns["risp_weak_release"];
        self.builder
            .build_call(f, &[p.into()], "weak.release")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        Ok(())
    }

    fn emit_drop_rc(
        &self,
        p: PointerValue<'ctx>,
        inner: &Type,
    ) -> Result<(), CodegenError> {
        let fv = self
            .builder
            .get_insert_block()
            .unwrap()
            .get_parent()
            .unwrap();
        let nonnull_bb = self.context.append_basic_block(fv, "drop.rc.body");
        let drop_pay_bb = self.context.append_basic_block(fv, "drop.rc.payload");
        let merge_bb = self.context.append_basic_block(fv, "drop.rc.end");

        let is_null = self
            .builder
            .build_is_null(p, "rc.isnull")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        self.builder
            .build_conditional_branch(is_null, merge_bb, nonnull_bb)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;

        self.builder.position_at_end(nonnull_bb);
        let rel = self.fns["risp_rc_release_strong"];
        let call = self
            .builder
            .build_call(rel, &[p.into()], "rc.release")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        let new_strong = call
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| CodegenError::Internal("rc_release_strong".into()))?
            .into_int_value();
        let is_zero = self
            .builder
            .build_int_compare(
                IntPredicate::EQ,
                new_strong,
                self.context.i32_type().const_zero(),
                "rc.strong0",
            )
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        self.builder
            .build_conditional_branch(is_zero, drop_pay_bb, merge_bb)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;

        self.builder.position_at_end(drop_pay_bb);
        let bt = self.llvm_basic(inner);
        let inner_v = self
            .builder
            .build_load(bt, p, "rc.payload")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        self.emit_drop(inner_v, inner)?;
        let after = self.fns["risp_rc_after_payload_drop"];
        self.builder
            .build_call(after, &[p.into()], "rc.after")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        self.builder
            .build_unconditional_branch(merge_bb)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;

        self.builder.position_at_end(merge_bb);
        Ok(())
    }

    fn emit_drop_box(
        &self,
        p: PointerValue<'ctx>,
        inner: &Type,
    ) -> Result<(), CodegenError> {
        let fv = self
            .builder
            .get_insert_block()
            .unwrap()
            .get_parent()
            .unwrap();
        let nonnull_bb = self.context.append_basic_block(fv, "drop.box.body");
        let merge_bb = self.context.append_basic_block(fv, "drop.box.end");
        let is_null = self
            .builder
            .build_is_null(p, "box.isnull")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        self.builder
            .build_conditional_branch(is_null, merge_bb, nonnull_bb)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;

        self.builder.position_at_end(nonnull_bb);
        let bt = self.llvm_basic(inner);
        let inner_v = self
            .builder
            .build_load(bt, p, "box.inner")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        self.emit_drop(inner_v, inner)?;
        let free_fn = self.fns["risp_box_free"];
        self.builder
            .build_call(free_fn, &[p.into()], "box.free")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        self.builder
            .build_unconditional_branch(merge_bb)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;

        self.builder.position_at_end(merge_bb);
        Ok(())
    }

    fn emit_drop_enum(
        &self,
        ev: inkwell::values::StructValue<'ctx>,
        enum_name: &str,
    ) -> Result<(), CodegenError> {
        let edef = self.enums[enum_name].clone();
        let drop_variants: Vec<_> = edef
            .variants
            .iter()
            .enumerate()
            .filter(|(_, v)| v.payload.as_ref().is_some_and(|p| self.needs_drop(p)))
            .map(|(i, v)| (i as u32, v.payload.clone().unwrap(), v.name.clone()))
            .collect();
        if drop_variants.is_empty() {
            return Ok(());
        }

        let tag = self
            .builder
            .build_extract_value(ev, 0, "drop.tag")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?
            .into_int_value();
        let payload = self
            .builder
            .build_extract_value(ev, 1, "drop.payload")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?
            .into_int_value();

        let fv = self
            .builder
            .get_insert_block()
            .unwrap()
            .get_parent()
            .unwrap();
        let merge_bb = self.context.append_basic_block(fv, "drop.enum.end");
        let else_bb = self.context.append_basic_block(fv, "drop.enum.skip");

        let mut cases = Vec::with_capacity(drop_variants.len());
        let mut arm_bbs = Vec::with_capacity(drop_variants.len());
        for (tag_i, _, vname) in &drop_variants {
            let bb = self
                .context
                .append_basic_block(fv, &format!("drop.enum.{vname}"));
            let tag_c = self.context.i32_type().const_int(*tag_i as u64, false);
            cases.push((tag_c, bb));
            arm_bbs.push(bb);
        }

        self.builder
            .build_switch(tag, else_bb, &cases)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;

        for ((_, pty, _), bb) in drop_variants.iter().zip(arm_bbs.into_iter()) {
            self.builder.position_at_end(bb);
            let unpacked = self.unpack_payload(payload, pty)?;
            self.emit_drop(unpacked, pty)?;
            self.builder
                .build_unconditional_branch(merge_bb)
                .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        }

        self.builder.position_at_end(else_bb);
        self.builder
            .build_unconditional_branch(merge_bb)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        self.builder.position_at_end(merge_bb);
        Ok(())
    }

    fn emit_retain(
        &self,
        v: BasicValueEnum<'ctx>,
        ty: &Type,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        match ty {
            Type::Str => Ok(self.rt_str_retain(v.into_pointer_value())?.into()),
            Type::Rc(_) => Ok(self
                .rt_call_ptr1("risp_rc_retain", v.into_pointer_value())?
                .into()),
            Type::Weak(_) => Ok(self
                .rt_call_ptr1("risp_weak_from", v.into_pointer_value())?
                .into()),
            // Unique ownership: retain is invalid; callers must take instead.
            Type::Box(_) | Type::Vec { .. } => Ok(v),
            Type::Named(n) if self.structs.contains_key(n) => {
                let sdef = self.structs[n].clone();
                let mut agg = v.into_struct_value();
                for (i, f) in sdef.fields.iter().enumerate() {
                    if self.needs_drop(&f.ty) && !is_move_type(&f.ty) {
                        let fv = self
                            .builder
                            .build_extract_value(agg, i as u32, &f.name)
                            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                        let retained = self.emit_retain(fv, &f.ty)?;
                        agg = self
                            .builder
                            .build_insert_value(agg, retained, i as u32, &f.name)
                            .map_err(|e| CodegenError::Llvm(e.to_string()))?
                            .into_struct_value();
                    }
                }
                Ok(agg.into())
            }
            Type::Named(n) if self.enums.contains_key(n) => self.emit_retain_enum(v, n),
            _ => Ok(v),
        }
    }

    fn emit_retain_enum(
        &self,
        v: BasicValueEnum<'ctx>,
        enum_name: &str,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let edef = self.enums[enum_name].clone();
        let retain_variants: Vec<_> = edef
            .variants
            .iter()
            .enumerate()
            .filter(|(_, v)| {
                v.payload
                    .as_ref()
                    .is_some_and(|p| self.needs_drop(p) && !is_move_type(p))
            })
            .map(|(i, v)| (i as u32, v.payload.clone().unwrap(), v.name.clone()))
            .collect();
        if retain_variants.is_empty() {
            return Ok(v);
        }

        let ev = v.into_struct_value();
        let tag = self
            .builder
            .build_extract_value(ev, 0, "retain.tag")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?
            .into_int_value();
        let payload = self
            .builder
            .build_extract_value(ev, 1, "retain.payload")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?
            .into_int_value();

        let fv = self
            .builder
            .get_insert_block()
            .unwrap()
            .get_parent()
            .unwrap();
        let merge_bb = self.context.append_basic_block(fv, "retain.enum.end");
        let else_bb = self.context.append_basic_block(fv, "retain.enum.skip");
        let et = self.llvm_enum_ty();

        let mut cases = Vec::with_capacity(retain_variants.len());
        let mut arm_bbs = Vec::with_capacity(retain_variants.len());
        for (tag_i, _, vname) in &retain_variants {
            let bb = self
                .context
                .append_basic_block(fv, &format!("retain.enum.{vname}"));
            let tag_c = self.context.i32_type().const_int(*tag_i as u64, false);
            cases.push((tag_c, bb));
            arm_bbs.push(bb);
        }

        self.builder
            .build_switch(tag, else_bb, &cases)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;

        let mut incoming: Vec<(BasicValueEnum<'ctx>, BasicBlock<'ctx>)> = Vec::new();
        for ((_, pty, _), bb) in retain_variants.iter().zip(arm_bbs.into_iter()) {
            self.builder.position_at_end(bb);
            let unpacked = self.unpack_payload(payload, pty)?;
            let retained = self.emit_retain(unpacked, pty)?;
            let bits = self.pack_payload(retained, pty)?;
            let mut agg = et.get_undef();
            agg = self
                .builder
                .build_insert_value(agg, tag, 0, "tag")
                .map_err(|e| CodegenError::Llvm(e.to_string()))?
                .into_struct_value();
            agg = self
                .builder
                .build_insert_value(agg, bits, 1, "payload")
                .map_err(|e| CodegenError::Llvm(e.to_string()))?
                .into_struct_value();
            let end_bb = self.builder.get_insert_block().unwrap();
            self.builder
                .build_unconditional_branch(merge_bb)
                .map_err(|e| CodegenError::Llvm(e.to_string()))?;
            incoming.push((agg.into(), end_bb));
        }

        self.builder.position_at_end(else_bb);
        let else_end = self.builder.get_insert_block().unwrap();
        self.builder
            .build_unconditional_branch(merge_bb)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        incoming.push((v, else_end));

        self.builder.position_at_end(merge_bb);
        let phi = self
            .builder
            .build_phi(et, "retain.enum")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        for (val, bb) in &incoming {
            phi.add_incoming(&[(val, *bb)]);
        }
        Ok(phi.as_basic_value())
    }

    /// Store an owned value into an alloca, dropping the previous contents when needed.
    fn store_owned(
        &self,
        alloca: PointerValue<'ctx>,
        new_owned: BasicValueEnum<'ctx>,
        ty: &Type,
    ) -> Result<(), CodegenError> {
        if self.needs_drop(ty) {
            let bt = self.llvm_basic(ty);
            let old = self
                .builder
                .build_load(bt, alloca, "old.owned")
                .map_err(|e| CodegenError::Llvm(e.to_string()))?;
            self.emit_drop(old, ty)?;
        }
        self.builder
            .build_store(alloca, new_owned)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        Ok(())
    }

    /// Load from alloca according to `mode`.
    /// - Move + move-type: take (load + zero) so scope-end drop is a no-op.
    /// - Move + Rc/str/Weak: retain (shared ownership).
    /// - Place: plain load (no take, no retain).
    fn load_owned(
        &self,
        alloca: PointerValue<'ctx>,
        ty: &Type,
        mode: ValueUse,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let bt = self.llvm_basic(ty);
        let v = self
            .builder
            .build_load(bt, alloca, "load.owned")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        match mode {
            ValueUse::Place => Ok(v),
            ValueUse::Move if is_move_type(ty) => {
                if self.needs_drop(ty) {
                    self.builder
                        .build_store(alloca, self.zero_owned(ty))
                        .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                }
                Ok(v)
            }
            ValueUse::Move if self.needs_drop(ty) => self.emit_retain(v, ty),
            ValueUse::Move => Ok(v),
        }
    }

    /// Release all locals that need drop (function exit).
    fn release_drop_locals(&self) -> Result<(), CodegenError> {
        for (ptr, ty) in self.locals.values() {
            if self.needs_drop(ty) {
                let bt = self.llvm_basic(ty);
                let v = self
                    .builder
                    .build_load(bt, *ptr, "drop.local")
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                self.emit_drop(v, ty)?;
            }
        }
        Ok(())
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

    fn expr_ty(e: &Expr) -> Result<&Type, CodegenError> {
        e.ty.as_ref()
            .ok_or_else(|| CodegenError::Internal("expression missing type info (typeck not run?)".into()))
    }

    fn emit_expr(&mut self, e: &Expr) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        self.emit_expr_mode(e, ValueUse::Move)
    }

    fn emit_expr_mode(
        &mut self,
        e: &Expr,
        mode: ValueUse,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
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
                    } else if self.needs_drop(&vty) || is_move_type(&vty) {
                        Ok(Some(self.load_owned(ptr, &vty, mode)?))
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
                // Drop this scope's owned bindings when the body did not exit via break.
                if !self.block_terminated() {
                    self.release_let_drop_bindings(bindings)?;
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
                        if let (Some(dty), Some(sv)) = (ex.ty.as_ref(), v) {
                            if self.needs_drop(dty) {
                                self.emit_drop(sv, dty)?;
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
                } else {
                    self.store_owned(ptr, v, &vty)?;
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
            ExprKind::BoxOf { expr } => Ok(Some(self.emit_box_of(expr)?)),
            ExprKind::VecNew { .. } => Ok(Some(self.emit_vec_new()?)),
            ExprKind::Field { base, field } => Ok(Some(self.emit_field(base, field)?)),
            ExprKind::Match { scrutinee, arms } => self.emit_match(scrutinee, arms, &ty),
            ExprKind::Call { callee, args } => self.emit_call(callee, args, &ty),
        }
    }

    fn emit_vec_new(&self) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let f = self.fns["risp_vec_i32_new"];
        let call = self
            .builder
            .build_call(f, &[], "vec.new")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        Ok(call
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| CodegenError::Internal("vec_new".into()))?)
    }

    /// Emit a place load for `e` when it is a local Var; otherwise a consuming temp.
    /// Returns `(value, owned_temp)` — caller must drop when `owned_temp`.
    fn emit_place_arg(
        &mut self,
        e: &Expr,
    ) -> Result<(BasicValueEnum<'ctx>, bool), CodegenError> {
        if matches!(e.kind, ExprKind::Var(_)) {
            let v = self
                .emit_expr_mode(e, ValueUse::Place)?
                .ok_or_else(|| CodegenError::Internal("place arg".into()))?;
            Ok((v, false))
        } else {
            let v = self
                .emit_expr(e)?
                .ok_or_else(|| CodegenError::Internal("temp arg".into()))?;
            Ok((v, true))
        }
    }

    fn emit_vpush(&mut self, args: &[Expr]) -> Result<(), CodegenError> {
        let (vec_v, owned) = self.emit_place_arg(&args[0])?;
        let x = self
            .emit_expr(&args[1])?
            .ok_or_else(|| CodegenError::Internal("vpush val".into()))?
            .into_int_value();
        let f = self.fns["risp_vec_i32_push"];
        self.builder
            .build_call(f, &[vec_v.into_pointer_value().into(), x.into()], "vpush")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        if owned {
            let ty = Self::expr_ty(&args[0])?.clone();
            self.emit_drop(vec_v, &ty)?;
        }
        Ok(())
    }

    fn emit_vget(&mut self, args: &[Expr]) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let (vec_v, owned) = self.emit_place_arg(&args[0])?;
        let idx = self
            .emit_expr(&args[1])?
            .ok_or_else(|| CodegenError::Internal("vget idx".into()))?
            .into_int_value();
        let f = self.fns["risp_vec_i32_get"];
        let call = self
            .builder
            .build_call(
                f,
                &[vec_v.into_pointer_value().into(), idx.into()],
                "vget",
            )
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        if owned {
            let ty = Self::expr_ty(&args[0])?.clone();
            self.emit_drop(vec_v, &ty)?;
        }
        Ok(call
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| CodegenError::Internal("vget".into()))?)
    }

    fn emit_vlen(
        &mut self,
        args: &[Expr],
    ) -> Result<inkwell::values::IntValue<'ctx>, CodegenError> {
        let (vec_v, owned) = self.emit_place_arg(&args[0])?;
        let f = self.fns["risp_vec_i32_len"];
        let call = self
            .builder
            .build_call(f, &[vec_v.into_pointer_value().into()], "vlen")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        if owned {
            let ty = Self::expr_ty(&args[0])?.clone();
            self.emit_drop(vec_v, &ty)?;
        }
        Ok(call
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| CodegenError::Internal("vlen".into()))?
            .into_int_value())
    }

    fn emit_rc_of(&mut self, expr: &Expr) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let inner_ty = Self::expr_ty(expr)?.clone();
        let v = self
            .emit_expr(expr)?
            .ok_or_else(|| CodegenError::Internal("rc inner".into()))?;
        let bt = self.llvm_basic(&inner_ty);
        let size = bt
            .size_of()
            .ok_or_else(|| CodegenError::Internal("rc size_of".into()))?;
        let alloc_fn = self.fns["risp_rc_alloc"];
        let call = self
            .builder
            .build_call(alloc_fn, &[size.into()], "rc.alloc")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        let p = call
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| CodegenError::Internal("rc_alloc".into()))?
            .into_pointer_value();
        self.builder
            .build_store(p, v)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        Ok(p.into())
    }

    fn emit_rc_clone(&mut self, expr: &Expr) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let (v, owned) = self.emit_place_arg(expr)?;
        let retained = self
            .rt_call_ptr1("risp_rc_retain", v.into_pointer_value())?
            .into();
        if owned {
            let ty = Self::expr_ty(expr)?.clone();
            self.emit_drop(v, &ty)?;
        }
        Ok(retained)
    }

    fn emit_downgrade(&mut self, expr: &Expr) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let (v, owned) = self.emit_place_arg(expr)?;
        let w = self
            .rt_call_ptr1("risp_weak_from", v.into_pointer_value())?
            .into();
        if owned {
            let ty = Self::expr_ty(expr)?.clone();
            self.emit_drop(v, &ty)?;
        }
        Ok(w)
    }

    fn emit_upgrade(&mut self, expr: &Expr) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let (v, owned) = self.emit_place_arg(expr)?;
        let up = self
            .rt_call_ptr1("risp_weak_upgrade", v.into_pointer_value())?
            .into();
        if owned {
            let ty = Self::expr_ty(expr)?.clone();
            self.emit_drop(v, &ty)?;
        }
        Ok(up)
    }

    fn emit_rc_is_null(
        &mut self,
        expr: &Expr,
    ) -> Result<inkwell::values::IntValue<'ctx>, CodegenError> {
        let (v, owned) = self.emit_place_arg(expr)?;
        let is_null = self
            .builder
            .build_is_null(v.into_pointer_value(), "rc.isnull")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        if owned {
            let ty = Self::expr_ty(expr)?.clone();
            self.emit_drop(v, &ty)?;
        }
        Ok(is_null)
    }

    fn emit_box_of(&mut self, expr: &Expr) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let inner_ty = Self::expr_ty(expr)?.clone();
        let v = self
            .emit_expr(expr)?
            .ok_or_else(|| CodegenError::Internal("box inner".into()))?;
        let bt = self.llvm_basic(&inner_ty);
        let size = bt
            .size_of()
            .ok_or_else(|| CodegenError::Internal("box size_of".into()))?;
        let alloc_fn = self.fns["risp_box_alloc"];
        let call = self
            .builder
            .build_call(alloc_fn, &[size.into()], "box.alloc")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        let p = call
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| CodegenError::Internal("box.alloc void".into()))?
            .into_pointer_value();
        self.builder
            .build_store(p, v)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        Ok(p.into())
    }

    fn emit_unbox(&mut self, args: &[Expr]) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let box_ty = Self::expr_ty(&args[0])?.clone();
        let Type::Box(inner) = box_ty else {
            return Err(CodegenError::Internal("unbox non-box".into()));
        };
        let p = self
            .emit_expr(&args[0])?
            .ok_or_else(|| CodegenError::Internal("unbox arg".into()))?
            .into_pointer_value();
        let bt = self.llvm_basic(&inner);
        let v = self
            .builder
            .build_load(bt, p, "unbox.val")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        // Move inner out; free the box shell without dropping the payload.
        let free_fn = self.fns["risp_box_free"];
        self.builder
            .build_call(free_fn, &[p.into()], "unbox.free")
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
        Ok(v)
    }

    /// `(borrow x)` — pointer to a local alloca (must be a `Var` place).
    fn emit_borrow(
        &self,
        place: &Expr,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        let ExprKind::Var(name) = &place.kind else {
            return Err(CodegenError::Internal(
                "borrow requires a local variable place".into(),
            ));
        };
        let (ptr, _) = self
            .locals
            .get(name)
            .cloned()
            .ok_or_else(|| CodegenError::Internal(format!("borrow unresolved {name}")))?;
        Ok(Some(ptr.into()))
    }

    fn emit_field(&mut self, base: &Expr, field: &str) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let raw_base_ty = Self::expr_ty(base)?.clone();
        let (base_ty, via_ref) = match &raw_base_ty {
            Type::Ref(inner) => (inner.as_ref().clone(), true),
            other => (other.clone(), false),
        };
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
        let field_ty = sdef.fields[idx].ty.clone();

        // Local / Ref place: GEP so we do not take the whole struct.
        let place_ptr = if via_ref {
            Some(
                self.emit_expr_mode(base, ValueUse::Place)?
                    .ok_or_else(|| CodegenError::Internal("field ref base".into()))?
                    .into_pointer_value(),
            )
        } else if let ExprKind::Var(name) = &base.kind {
            Some(
                self.locals
                    .get(name)
                    .map(|(p, _)| *p)
                    .ok_or_else(|| CodegenError::Internal(format!("field unresolved {name}")))?,
            )
        } else {
            None
        };

        if let Some(base_ptr) = place_ptr {
            let st = self.llvm_struct_ty(n);
            let field_ptr = self
                .builder
                .build_struct_gep(st, base_ptr, idx as u32, field)
                .map_err(|e| CodegenError::Llvm(e.to_string()))?;
            let ft = self.llvm_basic(&field_ty);
            let extracted = self
                .builder
                .build_load(ft, field_ptr, field)
                .map_err(|e| CodegenError::Llvm(e.to_string()))?;
            if is_move_type(&field_ty) && self.needs_drop(&field_ty) {
                self.builder
                    .build_store(field_ptr, self.zero_owned(&field_ty))
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                return Ok(extracted);
            }
            if self.needs_drop(&field_ty) {
                return self.emit_retain(extracted, &field_ty);
            }
            return Ok(extracted);
        }

        let base_v = self
            .emit_expr(base)?
            .ok_or_else(|| CodegenError::Internal("field base".into()))?;
        let extracted = self
            .builder
            .build_extract_value(base_v.into_struct_value(), idx as u32, field)
            .map_err(|e| CodegenError::Llvm(e.to_string()))?;

        let result = if is_move_type(&field_ty) {
            extracted
        } else if self.needs_drop(&field_ty) {
            self.emit_retain(extracted, &field_ty)?
        } else {
            extracted
        };
        if self.needs_drop(&base_ty) {
            if is_move_type(&field_ty) && self.needs_drop(&field_ty) {
                let zeroed = self
                    .builder
                    .build_insert_value(
                        base_v.into_struct_value(),
                        self.zero_owned(&field_ty),
                        idx as u32,
                        "field.taken",
                    )
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?
                    .into_struct_value();
                self.emit_drop(zeroed.into(), &base_ty)?;
            } else {
                self.emit_drop(base_v, &base_ty)?;
            }
        }
        Ok(result)
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
            Type::Str
            | Type::Box(_)
            | Type::Vec { .. }
            | Type::Rc(_)
            | Type::Weak(_) => Ok(self
                .builder
                .build_ptr_to_int(v.into_pointer_value(), i64_ty, "ptrpayload")
                .map_err(|e| CodegenError::Llvm(e.to_string()))?),
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
            Type::Str
            | Type::Box(_)
            | Type::Vec { .. }
            | Type::Rc(_)
            | Type::Weak(_) => {
                let ptr_ty = self.context.ptr_type(AddressSpace::default());
                Ok(self
                    .builder
                    .build_int_to_ptr(bits, ptr_ty, "ptrunpack")
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?
                    .into())
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
        let scrut_ty = Self::expr_ty(scrutinee)?.clone();
        let ev_val = self
            .emit_expr(scrutinee)?
            .ok_or_else(|| CodegenError::Internal("match scrutinee".into()))?;
        let ev = ev_val.into_struct_value();
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
            let mut payload_taken = false;
            let prev = if let (Some(pty), Some(bname)) = (&info.payload, &arm.binding) {
                let mut unpacked = self.unpack_payload(payload, pty)?;
                if is_move_type(pty) {
                    // Move payload into the binding; do not drop it with the scrutinee.
                    payload_taken = true;
                } else if self.needs_drop(pty) {
                    unpacked = self.emit_retain(unpacked, pty)?;
                }
                let alloca = self.create_entry_alloca(fv, bname, pty);
                self.builder
                    .build_store(alloca, unpacked)
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                self.locals.insert(bname.clone(), (alloca, pty.clone()))
            } else {
                None
            };

            // Drop scrutinee temporary (skip when move payload was taken into binding).
            if self.needs_drop(&scrut_ty) {
                if payload_taken {
                    self.emit_drop(self.zero_owned(&scrut_ty), &scrut_ty)?;
                } else {
                    self.emit_drop(ev_val, &scrut_ty)?;
                }
            }

            let body_v = self.emit_expr(&arm.body)?;

            // Drop match binding before leaving the arm.
            if let (Some(pty), Some(bname)) = (&info.payload, &arm.binding) {
                if self.needs_drop(pty) && !self.block_terminated() {
                    if let Some((ptr, _)) = self.locals.get(bname).cloned() {
                        let bt = self.llvm_basic(pty);
                        let bv = self
                            .builder
                            .build_load(bt, ptr, "match.drop.bind")
                            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                        self.emit_drop(bv, pty)?;
                        self.builder
                            .build_store(ptr, self.zero_owned(pty))
                            .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                    }
                }
            }

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

    fn release_let_drop_bindings(&self, bindings: &[Binding]) -> Result<(), CodegenError> {
        for b in bindings.iter().rev() {
            if !self.needs_drop(&b.ty) {
                continue;
            }
            let Some((ptr, _)) = self.locals.get(&b.name).cloned() else {
                continue;
            };
            let bt = self.llvm_basic(&b.ty);
            let v = self
                .builder
                .build_load(bt, ptr, "letscope.drop")
                .map_err(|e| CodegenError::Llvm(e.to_string()))?;
            self.emit_drop(v, &b.ty)?;
            // Zero the slot so a later function-exit drop is a no-op.
            self.builder
                .build_store(ptr, self.zero_owned(&b.ty))
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
        } else {
            let alloca = self.create_entry_alloca(fv, &b.name, &b.ty);
            self.store_owned(alloca, v, &b.ty)?;
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
            "unbox" => Ok(Some(self.emit_unbox(args)?)),
            "borrow" => self.emit_borrow(&args[0]),
            "vpush!" => {
                self.emit_vpush(args)?;
                Ok(None)
            }
            "vget" => Ok(Some(self.emit_vget(args)?)),
            "vlen" => Ok(Some(self.emit_vlen(args)?.into())),
            "rc" => Ok(Some(self.emit_rc_of(&args[0])?)),
            "rc-clone" => Ok(Some(self.emit_rc_clone(&args[0])?)),
            "downgrade" => Ok(Some(self.emit_downgrade(&args[0])?)),
            "upgrade" => Ok(Some(self.emit_upgrade(&args[0])?)),
            "rc-is-null" => Ok(Some(self.emit_rc_is_null(&args[0])?.into())),
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
                let (fn_params, fn_ret_ty) = self
                    .fn_types
                    .get(callee)
                    .cloned()
                    .ok_or_else(|| CodegenError::Internal(format!("no sig {callee}")))?;
                let is_extern = self.externs.contains_key(callee);
                let mut argv: Vec<BasicMetadataValueEnum> = Vec::with_capacity(args.len());
                let mut owned_strs: Vec<PointerValue<'ctx>> = Vec::new();
                for (i, a) in args.iter().enumerate() {
                    let v = self.emit_expr(a)?.unwrap();
                    if is_extern && fn_params.get(i) == Some(&Type::Str) {
                        let owned = v.into_pointer_value();
                        let cstr = self.rt_call_ptr1("risp_str_cstr", owned)?;
                        argv.push(cstr.into());
                        owned_strs.push(owned);
                    } else {
                        argv.push(v.into());
                    }
                }
                let call = self
                    .builder
                    .build_call(fv, &argv, "calltmp")
                    .map_err(|e| CodegenError::Llvm(e.to_string()))?;
                for p in owned_strs {
                    self.emit_drop(p.into(), &Type::Str)?;
                }
                if fn_ret_ty == Type::Unit {
                    Ok(None)
                } else if is_extern && fn_ret_ty == Type::Str {
                    let cstr = call
                        .try_as_basic_value()
                        .basic()
                        .ok_or_else(|| CodegenError::Internal("extern str ret".into()))?
                        .into_pointer_value();
                    Ok(Some(self.rt_str_from_cstr(cstr)?.into()))
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
            self.emit_drop(owned.into(), &Type::Str)?;
            return Ok(());
        }

        let printf = self.fns["__printf"];
        let (fmt, arg_v): (&str, BasicMetadataValueEnum) = match &ty {
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
            Type::Str
            | Type::Unit
            | Type::Array { .. }
            | Type::Named(_)
            | Type::Ref(_)
            | Type::Box(_)
            | Type::Vec { .. }
            | Type::Rc(_)
            | Type::Weak(_) => {
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
        self.emit_drop(a.into(), &Type::Str)?;
        self.emit_drop(b.into(), &Type::Str)?;
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
        self.emit_drop(s.into(), &Type::Str)?;
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
