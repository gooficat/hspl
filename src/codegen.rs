//! LLVM IR generation for HSPL.
//!
//! Every value has one of six types: five value types (`word`, `int`, `short`,
//! `byte`, `bool`) plus `void`, which is only valid as a function or `extern`
//! return type and produces no value. They are represented as LLVM `i64`,
//! `i32`, `i16`, `i8`, `i1`, and `void`. There are no operators; computation
//! happens through function calls, inline assembly, and a small set of
//! control-flow constructs.
//!
//! - Globals become module-level globals of their declared type.
//! - Functions use their annotated parameter and return types. The result is the
//!   value of the body's single statement.
//! - `extern name(types...) : ret;` declares a typed symbol defined elsewhere
//!   (for example the standard library's `print`).
//! - Implicit conversion is widening only: `bool` zero-extends, narrower
//!   integers sign-extend, and narrowing is rejected.
//! - Integer literals take their type from context and are range-checked.
//! - `if` becomes a conditional branch and a phi joining the branch values.
//!   Non-boolean conditions are compared against zero.
//! - `jump` branches to a function-scoped label block. All allocas live in the
//!   entry block so loops and jumps into scopes cannot reallocate or see an
//!   uninitialised slot.
//! - A user `main` is emitted as `main.hspl` plus an `i32 main()` wrapper so the
//!   C runtime links and exits correctly.
//!
//! Inline assembly blocks:
//!
//! ```text
//! add(a : word, b : word) : word asm {
//!     movq $a, $out
//!     addq $b, $out
//! }
//! ```
//!
//! `$out` is the block's output operand (constraint `=r`); each distinct `$name`
//! is an input operand (`r`) loaded from a variable. Operands are numbered
//! output-first, so `$a` becomes `$1` and `$out` becomes `$0`. `$$` escapes a
//! literal dollar for immediates (`movq $$42, $out`). The result type comes from
//! context (the return type or parameter type) and defaults to `word`; inputs
//! keep their variables' types.

use std::collections::{HashMap, HashSet};

use inkwell::basic_block::BasicBlock;
use inkwell::builder::{Builder, BuilderError};
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::targets::TargetMachine;
use inkwell::types::{BasicMetadataTypeEnum, BasicTypeEnum, IntType};
use inkwell::values::{BasicValueEnum, FunctionValue, IntValue, PointerValue};
use inkwell::IntPredicate;

use crate::ast::{Binding, Block, Expr, Function, Item, Program, Stmt, Type};

/// The standard library defines `print`, so a program cannot also define it.
const RESERVED_PRINT: &str =
    "`print` is provided by the standard library; import it with `extern print(word) : word;`";

/// Compile `program` to textual LLVM IR.
pub fn generate(program: &Program, module_name: &str) -> Result<String, String> {
    let context = Context::create();
    let module = context.create_module(module_name);
    let builder = context.create_builder();

    let mut cg = Codegen {
        context: &context,
        module,
        builder,
        alloca_builder: context.create_builder(),
        globals: HashMap::new(),
        functions: HashMap::new(),
        externs: HashMap::new(),
        scopes: Vec::new(),
        label_blocks: HashMap::new(),
        defined_labels: HashSet::new(),
        jumped_labels: HashSet::new(),
    };

    // Tag the module for the host so downstream tools do not warn.
    cg.module.set_triple(&TargetMachine::get_default_triple());

    cg.declare(program)?;

    for item in &program.items {
        if let Item::Function(function) = item {
            cg.gen_function(function)?;
        }
    }

    cg.gen_main_wrapper()?;

    cg.module
        .verify()
        .map_err(|err| format!("generated module failed verification: {err}"))?;

    Ok(cg.module.print_to_string().to_string())
}

/// The signature of a callable symbol, whether defined here or imported.
#[derive(Clone)]
struct FnSig<'ctx> {
    value: FunctionValue<'ctx>,
    params: Vec<Type>,
    ret: Type,
}

/// A value together with the HSPL type it carries. When `ty` is [`Type::Void`],
/// `value` is `None` because `void` produces no value.
#[derive(Clone, Copy)]
struct TypedValue<'ctx> {
    value: Option<BasicValueEnum<'ctx>>,
    ty: Type,
}

struct Codegen<'ctx> {
    context: &'ctx Context,
    module: Module<'ctx>,
    builder: Builder<'ctx>,
    /// Builder positioned in the function's entry block for allocas. Keeping
    /// every alloca there makes backward jumps and loops safe: a slot is
    /// allocated once, not on each iteration.
    alloca_builder: Builder<'ctx>,
    /// Addresses and types of module-level globals.
    globals: HashMap<String, (PointerValue<'ctx>, Type)>,
    /// HSPL function name -> LLVM function.
    functions: HashMap<String, FnSig<'ctx>>,
    /// Names imported with `extern` -> typed declaration.
    externs: HashMap<String, FnSig<'ctx>>,
    /// Lexical scopes; the last entry is the innermost block.
    scopes: Vec<HashMap<String, (PointerValue<'ctx>, Type)>>,
    /// Jump targets in the current function. Labels are function-scoped.
    label_blocks: HashMap<String, BasicBlock<'ctx>>,
    /// Labels defined in the current function (for duplicate checks).
    defined_labels: HashSet<String>,
    /// Labels referenced by `jump` in the current function.
    jumped_labels: HashSet<String>,
}

/// Shorthand for the builder's `Result<T, BuilderError>`.
fn build<T>(result: Result<T, BuilderError>) -> Result<T, String> {
    result.map_err(|err| err.to_string())
}

/// Check that `value` can be represented by `ty` when written as a literal.
///
/// The accepted range covers both the signed and unsigned interpretations of
/// the type's width, so `byte 255` (stored as -1) and `int 0xFFFFFFFF` work.
fn check_literal(value: i64, ty: Type, context: &str) -> Result<(), String> {
    let (min, max): (i128, i128) = match ty {
        // Every `i64` is a valid word; hex literals wrap to `i64` in the lexer.
        Type::Word => return Ok(()),
        Type::Int => (-(1i128 << 31), (1i128 << 32) - 1),
        Type::Short => (-(1i128 << 15), (1i128 << 16) - 1),
        Type::Byte => (-(1i128 << 7), (1i128 << 8) - 1),
        Type::Bool => (0, 1),
        Type::Void => return Ok(()),
    };

    let value = value as i128;
    if value < min || value > max {
        return Err(format!(
            "{context} {value} is out of range for `{}` ({min}..={max})",
            ty.name()
        ));
    }
    Ok(())
}

impl<'ctx> Codegen<'ctx> {
    // ----- types ---------------------------------------------------------

    fn int_type(&self, ty: Type) -> IntType<'ctx> {
        match ty {
            Type::Word => self.context.i64_type(),
            Type::Int => self.context.i32_type(),
            Type::Short => self.context.i16_type(),
            Type::Byte => self.context.i8_type(),
            Type::Bool => self.context.bool_type(),
            Type::Void => panic!("internal error: `void` has no integer representation"),
        }
    }

    fn llvm_type(&self, ty: Type) -> BasicTypeEnum<'ctx> {
        match ty {
            Type::Void => panic!("internal error: `void` is not a first-class LLVM type"),
            _ => self.int_type(ty).into(),
        }
    }

    /// A zero of `ty`, or no value when `ty` is `void`.
    fn zero(&self, ty: Type) -> TypedValue<'ctx> {
        TypedValue {
            value: if ty.is_void() {
                None
            } else {
                Some(self.llvm_type(ty).const_zero())
            },
            ty,
        }
    }

    /// An integer constant of `ty` with the bit pattern of `value`. `ty` must
    /// be a value type, not `void`.
    fn constant(&self, ty: Type, value: i64) -> TypedValue<'ctx> {
        TypedValue {
            value: Some(self.int_type(ty).const_int(value as u64, false).into()),
            ty,
        }
    }

    /// Convert `value` to `target`, allowing only implicit widening. Converting
    /// anything to `void` discards the value; converting `void` to anything
    /// else is an error.
    fn coerce(&self, value: TypedValue<'ctx>, target: Type) -> Result<TypedValue<'ctx>, String> {
        // Anything may be discarded into `void`.
        if target.is_void() {
            return Ok(TypedValue { value: None, ty: Type::Void });
        }
        if value.ty.is_void() {
            return Err(format!(
                "cannot use `void` as a value of type `{}`",
                target.name()
            ));
        }
        if value.ty == target {
            return Ok(TypedValue { value: value.value, ty: target });
        }

        if !value.ty.can_widen_to(target) {
            return Err(format!(
                "cannot implicitly convert `{}` to `{}` (only widening conversions are allowed)",
                value.ty.name(),
                target.name()
            ));
        }

        let input = value.value.expect("value type has a value").into_int_value();
        let target_type = self.int_type(target);
        let extended = if value.ty.is_bool() {
            build(self.builder.build_int_z_extend(input, target_type, "zext"))?
        } else {
            build(self.builder.build_int_s_extend(input, target_type, "sext"))?
        };

        Ok(TypedValue {
            value: Some(extended.into()),
            ty: target,
        })
    }

    // ----- declarations --------------------------------------------------

    fn declare(&mut self, program: &Program) -> Result<(), String> {
        let mut seen = HashSet::new();

        for item in &program.items {
            match item {
                Item::Extern(decl) => {
                    if !seen.insert(decl.name.clone()) {
                        return Err(format!("`{}` is declared more than once", decl.name));
                    }
                }
                Item::Global(global) => {
                    if global.name == "print" {
                        return Err(RESERVED_PRINT.to_string());
                    }
                    if global.ty.is_void() {
                        return Err(format!("`{}` cannot have type `void`", global.name));
                    }
                    if !seen.insert(global.name.clone()) {
                        return Err(format!("`{}` is defined more than once", global.name));
                    }
                }
                Item::Function(function) => {
                    if function.name == "print" {
                        return Err(RESERVED_PRINT.to_string());
                    }
                    if !seen.insert(function.name.clone()) {
                        return Err(format!("`{}` is defined more than once", function.name));
                    }
                }
            }
        }

        for item in &program.items {
            if let Item::Global(global) = item {
                check_literal(
                    global.init,
                    global.ty,
                    &format!("global `{}` initializer", global.name),
                )?;
                let value = self
                    .module
                    .add_global(self.int_type(global.ty), None, &global.name);
                value.set_initializer(&self.int_type(global.ty).const_int(global.init as u64, false));
                self.globals.insert(
                    global.name.clone(),
                    (value.as_pointer_value(), global.ty),
                );
            }
        }

        for item in &program.items {
            if let Item::Extern(decl) = item {
                let param_types: Vec<BasicMetadataTypeEnum<'ctx>> = decl
                    .params
                    .iter()
                    .map(|ty| self.int_type(*ty).into())
                    .collect();
                let fn_type = self.int_type(decl.ret).fn_type(&param_types, false);
                let value = self.module.add_function(&decl.name, fn_type, None);
                self.externs.insert(
                    decl.name.clone(),
                    FnSig {
                        value,
                        params: decl.params.clone(),
                        ret: decl.ret,
                    },
                );
            }
        }

        for item in &program.items {
            if let Item::Function(function) = item {
                self.declare_function(function)?;
            }
        }

        Ok(())
    }

    fn declare_function(&mut self, function: &Function) -> Result<(), String> {
        if function.name == "main" && !function.params.is_empty() {
            return Err("`main` cannot take parameters".to_string());
        }

        let mut params = HashSet::new();
        for param in &function.params {
            if param.ty.is_void() {
                return Err(format!(
                    "parameter `{}` of `{}` cannot have type `void`",
                    param.name, function.name
                ));
            }
            if !params.insert(param.name.clone()) {
                return Err(format!(
                    "parameter `{}` is declared twice in `{}`",
                    param.name, function.name
                ));
            }
        }

        let param_types: Vec<BasicMetadataTypeEnum<'ctx>> = function
            .params
            .iter()
            .map(|param| self.int_type(param.ty).into())
            .collect();
        // `void` is only a return type; it maps to LLVM `void`, not an integer
        // type, so build the function type with the right return kind directly.
        let fn_type = match function.ret {
            Type::Void => self.context.void_type().fn_type(&param_types, false),
            _ => self.int_type(function.ret).fn_type(&param_types, false),
        };

        let llvm_name = Self::llvm_function_name(&function.name);
        let value = self.module.add_function(&llvm_name, fn_type, None);
        self.functions.insert(
            function.name.clone(),
            FnSig {
                value,
                params: function.params.iter().map(|param| param.ty).collect(),
                ret: function.ret,
            },
        );
        Ok(())
    }

    /// A user `main` keeps its declared return type, so it is emitted under a
    /// distinct symbol and wrapped by a C-ABI `i32 main`.
    fn llvm_function_name(name: &str) -> String {
        if name == "main" {
            "main.hspl".to_string()
        } else {
            name.to_string()
        }
    }

    // ----- function bodies -----------------------------------------------

    fn gen_function(&mut self, function: &Function) -> Result<(), String> {
        let sig = self
            .functions
            .get(&function.name)
            .cloned()
            .expect("function was declared");

        // `entry` holds every alloca and parameter store; `body` is the actual
        // statement. Entry is terminated only after the body is generated, so
        // locals discovered later can still be appended.
        let entry = self.context.append_basic_block(sig.value, "entry");
        let body = self.context.append_basic_block(sig.value, "body");
        self.alloca_builder.position_at_end(entry);
        self.builder.position_at_end(body);

        self.scopes.clear();
        self.scopes.push(HashMap::new());
        self.label_blocks.clear();
        self.defined_labels.clear();
        self.jumped_labels.clear();

        for (index, param) in function.params.iter().enumerate() {
            let slot = build(
                self.alloca_builder
                    .build_alloca(self.int_type(param.ty), &param.name),
            )?;
            let arg = sig
                .value
                .get_nth_param(index as u32)
                .ok_or_else(|| format!("internal error: missing parameter {index}"))?;
            build(self.alloca_builder.build_store(slot, arg))?;
            self.scopes
                .last_mut()
                .expect("scope exists")
                .insert(param.name.clone(), (slot, param.ty));
        }

        // The body's value is the function's result. A `void` function yields
        // no value and ends with `ret void`.
        let result = self.gen_stmt(&function.body, Some(function.ret))?;

        if function.ret.is_void() {
            if self.current_block()?.get_terminator().is_none() {
                build(self.builder.build_return(None))?;
            }
        } else {
            let result = self.coerce(result, function.ret)?;

            // A body ending in `jump` already terminates its block. Otherwise
            // return the value it produced.
            if self.current_block()?.get_terminator().is_none() {
                let value = result.value.expect("non-void result has a value");
                build(self.builder.build_return(Some(&value)))?;
            }
        }

        if let Some(name) = self
            .jumped_labels
            .iter()
            .find(|name| !self.defined_labels.contains(*name))
        {
            return Err(format!("jump to undefined label `{name}`"));
        }

        // Entry runs all allocas and parameter stores, then falls through.
        build(self.alloca_builder.build_unconditional_branch(body))?;

        self.scopes.clear();
        Ok(())
    }

    /// Emit `i32 @main()` calling the user's `main.hspl` and converting its
    /// result to the C `int` exit status.
    fn gen_main_wrapper(&mut self) -> Result<(), String> {
        let Some(sig) = self.functions.get("main").cloned() else {
            return Ok(());
        };

        let i32_type = self.context.i32_type();
        let wrapper_type = i32_type.fn_type(&[], false);
        let wrapper = self.module.add_function("main", wrapper_type, None);
        let entry = self.context.append_basic_block(wrapper, "entry");
        self.builder.position_at_end(entry);

        let call = build(self.builder.build_call(sig.value, &[], "call"))?;

        if sig.ret.is_void() {
            // A `void` main returns no value; exit with status 0.
            let zero: BasicValueEnum = i32_type.const_zero().into();
            build(self.builder.build_return(Some(&zero)))?;
            return Ok(());
        }

        let value = call
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| "internal error: `main` did not return a value".to_string())?
            .into_int_value();

        let converted = match sig.ret {
            Type::Word => build(self.builder.build_int_truncate(value, i32_type, "trunc"))?,
            Type::Int => value,
            Type::Short | Type::Byte => {
                build(self.builder.build_int_s_extend(value, i32_type, "sext"))?
            }
            Type::Bool => build(self.builder.build_int_z_extend(value, i32_type, "zext"))?,
            Type::Void => unreachable!("handled above"),
        };

        let returned: BasicValueEnum = converted.into();
        build(self.builder.build_return(Some(&returned)))?;

        Ok(())
    }

    // ----- statements ----------------------------------------------------

    /// Generate a statement, converting its value to `expected` when given.
    fn gen_stmt(
        &mut self,
        stmt: &Stmt,
        expected: Option<Type>,
    ) -> Result<TypedValue<'ctx>, String> {
        // A label may be reachable even when the preceding statement jumped
        // away, so handle it before opening a fresh (unreachable) block.
        if let Stmt::Label(name) = stmt {
            return self.gen_label(name, expected);
        }

        self.ensure_open_block()?;

        let value = match stmt {
            Stmt::Auto(bindings) => {
                for binding in bindings {
                    self.declare_local(binding)?;
                }
                self.zero(expected.unwrap_or(Type::Word))
            }
            Stmt::Assign(name, expr) => {
                let (slot, ty) = self
                    .lookup(name)
                    .ok_or_else(|| format!("assignment to undefined variable `{name}`"))?;
                let value = self.gen_expr(expr, Some(ty))?;
                let value = self.coerce(value, ty)?;
                build(self.builder.build_store(
                    slot,
                    *value.value.as_ref().expect("assigned value"),
                ))?;
                value
            }
            Stmt::Expr(expr) => self.gen_expr(expr, expected)?,
            Stmt::Jump(name) => self.gen_jump(name, expected)?,
            Stmt::Label(_) => unreachable!("labels are handled above"),
            Stmt::Empty => self.zero(expected.unwrap_or(Type::Word)),
        };

        match expected {
            Some(target) => self.coerce(value, target),
            None => Ok(value),
        }
    }

    /// Generate a statement expression: a new scope, then its trailing value.
    fn gen_block(
        &mut self,
        block: &Block,
        expected: Option<Type>,
    ) -> Result<TypedValue<'ctx>, String> {
        self.scopes.push(HashMap::new());

        for stmt in &block.stmts {
            self.gen_stmt(stmt, None)?;
        }

        // If the last statement jumped away, the tail (or the zero this block
        // yields) lives in a fresh unreachable block, which keeps the IR valid
        // and gives any enclosing `if` a block to branch from.
        self.ensure_open_block()?;

        let value = match &block.tail {
            Some(expr) => self.gen_expr(expr, expected)?,
            None => self.zero(expected.unwrap_or(Type::Word)),
        };

        self.scopes.pop();
        Ok(value)
    }

    // ----- control flow ---------------------------------------------------

    fn current_block(&self) -> Result<BasicBlock<'ctx>, String> {
        self.builder
            .get_insert_block()
            .ok_or_else(|| "internal error: no current basic block".to_string())
    }

    fn current_function(&self) -> Result<FunctionValue<'ctx>, String> {
        self.current_block()?
            .get_parent()
            .ok_or_else(|| "internal error: current block has no function".to_string())
    }

    /// If the current block already has a terminator (for example after a
    /// `jump`), continue in a fresh block so instructions are still valid.
    fn ensure_open_block(&mut self) -> Result<(), String> {
        if self.current_block()?.get_terminator().is_some() {
            let function = self.current_function()?;
            let next = self.context.append_basic_block(function, "unreachable");
            self.builder.position_at_end(next);
        }
        Ok(())
    }

    fn label_block(&mut self, name: &str) -> Result<BasicBlock<'ctx>, String> {
        if let Some(block) = self.label_blocks.get(name) {
            return Ok(*block);
        }

        let function = self.current_function()?;
        let block = self
            .context
            .append_basic_block(function, &format!("label.{name}"));
        self.label_blocks.insert(name.to_string(), block);
        Ok(block)
    }

    fn gen_label(
        &mut self,
        name: &str,
        expected: Option<Type>,
    ) -> Result<TypedValue<'ctx>, String> {
        if !self.defined_labels.insert(name.to_string()) {
            return Err(format!("label `{name}` is defined more than once"));
        }

        let target = self.label_block(name)?;
        if self.current_block()?.get_terminator().is_none() {
            build(self.builder.build_unconditional_branch(target))?;
        }
        self.builder.position_at_end(target);

        Ok(self.zero(expected.unwrap_or(Type::Word)))
    }

    fn gen_jump(
        &mut self,
        name: &str,
        expected: Option<Type>,
    ) -> Result<TypedValue<'ctx>, String> {
        self.jumped_labels.insert(name.to_string());
        let target = self.label_block(name)?;
        build(self.builder.build_unconditional_branch(target))?;
        Ok(self.zero(expected.unwrap_or(Type::Word)))
    }

    fn gen_if(
        &mut self,
        cond: &Expr,
        then_branch: &Expr,
        else_branch: Option<&Expr>,
        expected: Option<Type>,
    ) -> Result<TypedValue<'ctx>, String> {
        let result_ty = expected.unwrap_or(Type::Word);

        let condition = self.gen_expr(cond, None)?;
        let condition = self.as_condition(condition)?;

        // A `void` if discards its branch values; branches are still emitted
        // for their side effects.
        let branch_expected = if result_ty.is_void() {
            None
        } else {
            Some(result_ty)
        };

        let function = self.current_function()?;
        let then_block = self.context.append_basic_block(function, "if.then");
        let else_block = self.context.append_basic_block(function, "if.else");
        let merge_block = self.context.append_basic_block(function, "if.end");
        build(
            self.builder
                .build_conditional_branch(condition, then_block, else_block),
        )?;

        self.builder.position_at_end(then_block);
        let then_value = self.gen_expr(then_branch, branch_expected)?;
        let then_value = if result_ty.is_void() {
            then_value
        } else {
            self.coerce(then_value, result_ty)?
        };
        self.ensure_open_block()?;
        let then_end = self.current_block()?;
        build(self.builder.build_unconditional_branch(merge_block))?;

        self.builder.position_at_end(else_block);
        let else_value = match else_branch {
            Some(expr) => self.gen_expr(expr, branch_expected)?,
            None => self.zero(result_ty),
        };
        let else_value = if result_ty.is_void() {
            else_value
        } else {
            self.coerce(else_value, result_ty)?
        };
        self.ensure_open_block()?;
        let else_end = self.current_block()?;
        build(self.builder.build_unconditional_branch(merge_block))?;

        self.builder.position_at_end(merge_block);
        if result_ty.is_void() {
            return Ok(TypedValue { value: None, ty: Type::Void });
        }

        let phi = build(self.builder.build_phi(self.llvm_type(result_ty), "if"))?;
        phi.add_incoming(&[
            (
                &then_value.value.expect("then branch has a value"),
                then_end,
            ),
            (
                &else_value.value.expect("else branch has a value"),
                else_end,
            ),
        ]);

        Ok(TypedValue {
            value: Some(phi.as_basic_value()),
            ty: result_ty,
        })
    }

    /// Turn a condition value into an `i1`: booleans are used directly, and
    /// integers are true when non-zero.
    fn as_condition(&mut self, value: TypedValue<'ctx>) -> Result<IntValue<'ctx>, String> {
        if value.ty.is_void() {
            return Err("cannot use `void` as a condition".to_string());
        }
        let int_value = value.value.expect("condition is a value").into_int_value();
        if value.ty.is_bool() {
            Ok(int_value)
        } else {
            let zero = self.int_type(value.ty).const_zero();
            build(
                self.builder
                    .build_int_compare(IntPredicate::NE, int_value, zero, "cond"),
            )
        }
    }

    fn declare_local(&mut self, binding: &Binding) -> Result<(), String> {
        if binding.ty.is_void() {
            return Err(format!(
                "local `{}` cannot have type `void`",
                binding.name
            ));
        }
        if self
            .scopes
            .last()
            .expect("scope exists")
            .contains_key(&binding.name)
        {
            return Err(format!(
                "`{}` is declared twice in the same block",
                binding.name
            ));
        }

        // The slot lives in the entry block so jumps and loops never allocate
        // again. Zero it both at entry (for jumps into the scope) and at the
        // declaration (so a declaration inside a loop resets each iteration).
        let slot = build(
            self.alloca_builder
                .build_alloca(self.int_type(binding.ty), &binding.name),
        )?;
        // `binding.ty` is never `void` (validated in `declare_function`), so
        // the zero value always exists.
        let zero = self.zero(binding.ty).value.expect("local zero value");
        build(self.alloca_builder.build_store(slot, zero))?;
        build(self.builder.build_store(slot, zero))?;
        self.scopes
            .last_mut()
            .expect("scope exists")
            .insert(binding.name.clone(), (slot, binding.ty));
        Ok(())
    }

    // ----- expressions ---------------------------------------------------

    fn gen_expr(
        &mut self,
        expr: &Expr,
        expected: Option<Type>,
    ) -> Result<TypedValue<'ctx>, String> {
        self.ensure_open_block()?;

        // A `void` context discards the value but still validates and emits the
        // expression for its side effects (for example `void_fn();` as a
        // statement).
        if expected.is_some_and(Type::is_void) {
            return self
                .gen_expr(expr, None)
                .map(|_| TypedValue { value: None, ty: Type::Void });
        }

        match expr {
            Expr::Int(value) => {
                let ty = expected.unwrap_or(Type::Word);
                check_literal(*value, ty, "integer literal")?;
                Ok(self.constant(ty, *value))
            }
            // A boolean literal is `bool` on its own, but adopts an expected
            // integer type directly (equivalent to zero-extension).
            Expr::Bool(value) => {
                let ty = expected.unwrap_or(Type::Bool);
                Ok(self.constant(ty, i64::from(*value)))
            }
            Expr::Var(name) => {
                let (slot, ty) = self
                    .lookup(name)
                    .ok_or_else(|| format!("use of undefined variable `{name}`"))?;
                let loaded = build(self.builder.build_load(self.int_type(ty), slot, name))?;
                let value = TypedValue { value: Some(loaded), ty };
                match expected {
                    Some(target) => self.coerce(value, target),
                    None => Ok(value),
                }
            }
            Expr::Call(name, args) => self.gen_call(name, args, expected),
            Expr::Asm(source) => self.gen_asm(source, expected.unwrap_or(Type::Word)),
            Expr::Block(block) => self.gen_block(block, expected),
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => self.gen_if(cond, then_branch, else_branch.as_deref(), expected),
        }
    }

    fn gen_call(
        &mut self,
        name: &str,
        args: &[Expr],
        expected: Option<Type>,
    ) -> Result<TypedValue<'ctx>, String> {
        let sig = self
            .functions
            .get(name)
            .cloned()
            .or_else(|| self.externs.get(name).cloned())
            .ok_or_else(|| {
                format!("call to undefined function `{name}` (missing an `extern` declaration)")
            })?;

        if args.len() != sig.params.len() {
            return Err(format!(
                "`{name}` expects {} argument(s), found {}",
                sig.params.len(),
                args.len()
            ));
        }

        let mut values = Vec::with_capacity(args.len());
        for (arg, &param) in args.iter().zip(&sig.params) {
            let value = self.gen_expr(arg, Some(param))?;
            let value = self.coerce(value, param)?;
            values.push(value.value.expect("argument value").into());
        }

        let call = build(self.builder.build_call(sig.value, &values, "call"))?;

        // A call to a `void` function produces no value: it can only be used as
        // a statement, never as an expression.
        if sig.ret.is_void() {
            let void_value = TypedValue { value: None, ty: Type::Void };
            return match expected {
                Some(target) if !target.is_void() => Err(format!(
                    "cannot use a `void` call to `{name}` as a value of type `{}`",
                    target.name()
                )),
                _ => Ok(void_value),
            };
        }

        let value = call
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| format!("`{name}` did not produce a value"))?;
        let value = TypedValue { value: Some(value), ty: sig.ret };

        match expected {
            Some(target) => self.coerce(value, target),
            None => Ok(value),
        }
    }

    // ----- inline assembly -----------------------------------------------

    fn gen_asm(
        &mut self,
        source: &str,
        result_ty: Type,
    ) -> Result<TypedValue<'ctx>, String> {
        if result_ty.is_void() {
            return Err("asm block cannot have `void` result type".to_string());
        }
        let (template, inputs) = rewrite_asm(source);

        let mut args = Vec::with_capacity(inputs.len());
        let mut param_types: Vec<BasicMetadataTypeEnum<'ctx>> = Vec::with_capacity(inputs.len());
        for name in &inputs {
            let (slot, ty) = self
                .lookup(name)
                .ok_or_else(|| format!("asm block references undefined variable `${name}`"))?;
            let value = build(self.builder.build_load(self.int_type(ty), slot, name))?;
            args.push(value.into());
            param_types.push(self.int_type(ty).into());
        }

        let fn_type = self.int_type(result_ty).fn_type(&param_types, false);

        // One output (`=r`) followed by one input register per variable.
        let mut constraints = String::from("=r");
        for _ in &inputs {
            constraints.push_str(",r");
        }

        let asm = self.context.create_inline_asm(
            fn_type,
            template,
            constraints,
            true,  // side effects
            false, // alignstack
            None,  // default dialect (AT&T)
            false, // can throw
        );

        let call = build(self
            .builder
            .build_indirect_call(fn_type, asm, &args, "asm"))?;

        let value = call
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| "asm block did not produce a value".to_string())?;

        Ok(TypedValue {
            value: Some(value),
            ty: result_ty,
        })
    }

    // ----- variables -----------------------------------------------------

    fn lookup(&self, name: &str) -> Option<(PointerValue<'ctx>, Type)> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
            .or_else(|| self.globals.get(name).copied())
    }
}

/// Rewrite `$name` placeholders in an asm template into LLVM operand numbers.
///
/// Returns the rewritten template and the distinct input variable names in
/// first-use order. `$out` maps to operand 0; `$$` and `$<digit>` pass through.
fn rewrite_asm(source: &str) -> (String, Vec<String>) {
    let mut template = String::with_capacity(source.len());
    let mut inputs: Vec<String> = Vec::new();
    let mut chars = source.chars().peekable();

    while let Some(c) = chars.next() {
        if c != '$' {
            template.push(c);
            continue;
        }

        match chars.peek().copied() {
            // `$$` is an escaped dollar for literals such as AT&T immediates.
            Some('$') => {
                chars.next();
                template.push_str("$$");
            }
            // Explicit LLVM operand references pass through unchanged.
            Some(digit) if digit.is_ascii_digit() => {
                template.push('$');
                template.push(digit);
                chars.next();
            }
            // `$name` -> `$0` for the output, `$1..` for inputs.
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                let mut name = String::new();
                while let Some(&next) = chars.peek() {
                    if next.is_ascii_alphanumeric() || next == '_' {
                        name.push(next);
                        chars.next();
                    } else {
                        break;
                    }
                }

                if name == "out" {
                    template.push_str("$0");
                } else {
                    let index = inputs
                        .iter()
                        .position(|input| input == &name)
                        .unwrap_or_else(|| {
                            inputs.push(name);
                            inputs.len() - 1
                        });
                    template.push_str(&format!("${}", index + 1));
                }
            }
            Some(other) => {
                template.push('$');
                template.push(other);
                chars.next();
            }
            None => template.push('$'),
        }
    }

    (template, inputs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;
    use crate::parser::parse;

    fn ir(source: &str) -> String {
        let program = parse(tokenize(source).expect("tokenizes")).expect("parses");
        generate(&program, "test").expect("generates")
    }

    fn ir_err(source: &str) -> String {
        let program = parse(tokenize(source).expect("tokenizes")).expect("parses");
        generate(&program, "test").unwrap_err()
    }

    #[test]
    fn emits_typed_globals() {
        let text = ir("answer 42;\nzero : word;\ncounter : int 7;\nsmall : byte 200;");
        assert!(text.contains("@answer = global i64 42"), "{text}");
        assert!(text.contains("@zero = global i64 0"), "{text}");
        assert!(text.contains("@counter = global i32 7"), "{text}");
        assert!(text.contains("@small = global i8 -56"), "{text}");
    }

    #[test]
    fn declarations_use_types_after_names() {
        let text = ir("f(x : byte, y : short) : int x;");
        assert!(text.contains("define i32 @f(i8 %0, i16 %1)"), "{text}");
    }

    #[test]
    fn bool_globals_and_literals() {
        let text = ir("flag : bool true;\nyes() : bool true;\nno() : bool false;");
        assert!(text.contains("@flag = global i1 true"), "{text}");
        assert!(text.contains("define i1 @yes()"), "{text}");
        assert!(text.contains("ret i1 true"), "{text}");
        assert!(text.contains("ret i1 false"), "{text}");
    }

    #[test]
    fn declares_and_calls_typed_externs() {
        let text = ir("extern print(word) : word;\nextern put(byte) : int;\nmain() print(42);");
        assert!(text.contains("declare i64 @print(i64)"), "{text}");
        assert!(text.contains("declare i32 @put(i8)"), "{text}");
        assert!(text.contains("call i64 @print(i64 42)"), "{text}");
    }

    #[test]
    fn externs_with_no_arguments() {
        let text = ir("extern tick() : word;\nmain() tick();");
        assert!(text.contains("declare i64 @tick()"), "{text}");
        assert!(text.contains("call i64 @tick()"), "{text}");
    }

    #[test]
    fn argument_literals_adopt_the_parameter_type() {
        let text = ir("extern put(byte) : word;\nmain() put(200);");
        assert!(text.contains("@put(i8 -56)"), "{text}");
    }

    #[test]
    fn extern_arguments_are_widened() {
        let text = ir(
            "extern take(word, word, word) : word;\n\
             f(flag : bool, small : byte, n : int) take(flag, small, n);",
        );
        // bool zero-extends, narrower integers sign-extend.
        assert!(text.contains("zext i1"), "{text}");
        assert!(text.contains("sext i8"), "{text}");
        assert!(text.contains("sext i32"), "{text}");
    }

    #[test]
    fn externs_check_arity() {
        let error = ir_err("extern f(word, word) : word;\nmain() f(1);");
        assert!(error.contains("expects 2 argument(s)"), "{error}");
    }

    #[test]
    fn rejects_out_of_range_global_initializers() {
        let error = ir_err("small : byte 300;");
        assert!(error.contains("out of range for `byte`"), "{error}");
        assert!(error.contains("global `small` initializer"), "{error}");
    }

    #[test]
    fn print_is_not_a_builtin() {
        let error = ir_err("main() print(1);");
        assert!(error.contains("undefined function `print`"), "{error}");
        assert!(error.contains("extern"), "{error}");
    }

    #[test]
    fn print_cannot_be_redefined() {
        let error = ir_err("print(x) x;");
        assert!(error.contains("standard library"), "{error}");

        let error = ir_err("print 1;");
        assert!(error.contains("standard library"), "{error}");
    }

    #[test]
    fn function_returns_body_value() {
        let text = ir("id(x) : word x;");
        assert!(text.contains("define i64 @id(i64 %0)"), "{text}");
        assert!(text.contains("ret i64"), "{text}");
    }

    #[test]
    fn return_types_default_to_void() {
        let text = ir("f() { }");
        assert!(text.contains("define void @f()"), "{text}");
        assert!(text.contains("ret void"), "{text}");
    }

    #[test]
    fn block_tail_is_the_value() {
        let text = ir("seven() : word { auto x; x = 7; x }");
        assert!(text.contains("store i64 7"), "{text}");
        assert!(text.contains("ret i64"), "{text}");
    }

    #[test]
    fn typed_auto_allocates_the_declared_type() {
        let text = ir("f() : byte { auto x : byte; x }");
        assert!(text.contains("alloca i8"), "{text}");
        assert!(text.contains("store i8 0"), "{text}");
        assert!(text.contains("ret i8"), "{text}");
    }

    #[test]
    fn assignment_uses_the_variable_type() {
        let text = ir("f() : int { auto x : short; x = 1; x }");
        assert!(text.contains("store i16 1"), "{text}");
        assert!(text.contains("sext i16"), "{text}");
        assert!(text.contains("ret i32"), "{text}");
    }

    #[test]
    fn empty_blocks_yield_zero_of_the_expected_type() {
        let text = ir("f() : byte { }");
        assert!(text.contains("ret i8 0"), "{text}");
    }

    #[test]
    fn widens_bool_with_zero_extend() {
        let text = ir("f(flag : bool) : int flag;");
        assert!(text.contains("zext i1"), "{text}");
        assert!(text.contains("ret i32"), "{text}");
    }

    #[test]
    fn widens_narrow_integers_with_sign_extend() {
        let text = ir("f(x : byte) : word x;");
        assert!(text.contains("sext i8"), "{text}");
        assert!(text.contains("ret i64"), "{text}");
    }

    #[test]
    fn rejects_narrowing() {
        let error = ir_err("f(x : word) : byte x;");
        assert!(
            error.contains("cannot implicitly convert `word` to `byte`"),
            "{error}"
        );
    }

    #[test]
    fn rejects_out_of_range_literals() {
        assert!(
            ir_err("f() : byte 300;").contains("out of range for `byte`"),
            "byte literal"
        );
        assert!(
            ir_err("f() : bool 2;").contains("out of range for `bool`"),
            "bool literal"
        );
        assert!(
            ir_err("f() : short 70000;").contains("out of range for `short`"),
            "short literal"
        );
        // Unsigned-looking values are accepted for narrower types.
        let text = ir("f() : byte 255;");
        assert!(text.contains("ret i8 -1"), "{text}");
    }

    #[test]
    fn wraps_word_main_for_the_c_abi() {
        let text = ir("main() : word 0;");
        assert!(text.contains("define i64 @main.hspl()"), "{text}");
        assert!(text.contains("define i32 @main()"), "{text}");
        assert!(text.contains("trunc i64"), "{text}");
    }

    #[test]
    fn unannotated_main_is_void_and_exits_zero() {
        let text = ir("main() { }");
        assert!(text.contains("define void @main.hspl()"), "{text}");
        assert!(text.contains("define i32 @main()"), "{text}");
        assert!(text.contains("ret i32 0"), "{text}");
    }

    #[test]
    fn wraps_int_main_without_truncation() {
        let text = ir("main() : int 0;");
        assert!(text.contains("define i32 @main.hspl()"), "{text}");
        assert!(text.contains("define i32 @main()"), "{text}");
        assert!(!text.contains("trunc"), "{text}");
    }

    #[test]
    fn wraps_narrow_main_with_extension() {
        let text = ir("main() : byte 0;");
        assert!(text.contains("sext i8"), "{text}");
    }

    #[test]
    fn emits_inline_asm_operands() {
        let text = ir("double(x) : word asm {\n  movq $x, $out\n  addq $x, $out\n}");
        assert!(text.contains("asm sideeffect"), "{text}");
        assert!(text.contains("movq $1, $0"), "{text}");
        assert!(text.contains("addq $1, $0"), "{text}");
        assert!(text.contains("\"=r,r\""), "{text}");
    }

    #[test]
    fn emits_asm_with_only_an_output() {
        let text = ir("answer() : word asm { movq $$42, $out }");
        assert!(text.contains("movq $$42, $0"), "{text}");
        assert!(text.contains("\"=r\""), "{text}");
    }

    #[test]
    fn asm_uses_the_expected_result_type() {
        let text = ir("f(x : byte) : byte asm {\n  movb $x, $out\n}");
        assert!(text.contains("call i8 asm"), "{text}");
        assert!(text.contains("movb $1, $0"), "{text}");
    }

    #[test]
    fn asm_defaults_to_word() {
        let text = ir("f() asm { movq $$42, $out }");
        assert!(text.contains("call i64 asm"), "{text}");
    }

    #[test]
    fn emits_if_with_a_phi() {
        let text = ir("choose(flag : bool) : int if flag { 1 } else { 2 };");
        assert!(text.contains("br i1"), "{text}");
        assert!(text.contains("phi i32"), "{text}");
        assert!(text.contains("ret i32"), "{text}");
    }

    #[test]
    fn integer_conditions_compare_with_zero() {
        let text = ir("f(n : int) : int if n { 1 } else { 2 };");
        assert!(text.contains("icmp ne i32"), "{text}");
    }

    #[test]
    fn if_without_else_yields_zero() {
        let text = ir("f(flag : bool) : byte if flag { 7 };");
        assert!(text.contains("phi i8"), "{text}");
    }

    #[test]
    fn emits_jump_and_label() {
        let text = ir("f() : int { jump done; done: 7 }");
        assert!(text.contains("label.done"), "{text}");
        assert!(text.contains("br label %label.done"), "{text}");
        assert!(text.contains("ret i32 7"), "{text}");
    }

    #[test]
    fn if_can_jump_out_and_still_produce_a_value() {
        let text = ir("f(flag : bool) : int { if flag { jump done; } done: 5 }");
        assert!(text.contains("br label %label.done"), "{text}");
        assert!(text.contains("ret i32 5"), "{text}");
    }

    #[test]
    fn accepts_a_bare_jump_branch() {
        let text = ir("f(flag : bool) : word { if flag jump done; done: 7 }");
        assert!(text.contains("br label %label.done"), "{text}");
        assert!(text.contains("ret i64 7"), "{text}");
    }

    #[test]
    fn unreachable_statements_after_a_jump_are_still_valid() {
        let text = ir("f() : int { jump done; 1; done: 2 }");
        assert!(text.contains("unreachable"), "{text}");
        assert!(text.contains("ret i32 2"), "{text}");
    }

    #[test]
    fn allocas_are_hoisted_to_the_entry_block() {
        let text = ir("f(n : int) : int { auto i : int; top: i = n; jump top; }");
        let entry = text.find("entry:").expect("entry block");
        let body = text.find("body:").expect("body block");
        let alloca = text.find("alloca i32").expect("alloca");
        assert!(entry < alloca && alloca < body, "{text}");
        assert!(text.contains("br label %label.top"), "{text}");
    }

    #[test]
    fn rejects_undefined_labels() {
        let error = ir_err("f() { jump nope; }");
        assert!(error.contains("jump to undefined label `nope`"), "{error}");
    }

    #[test]
    fn rejects_duplicate_labels() {
        let error = ir_err("f() { a: a: }");
        assert!(error.contains("label `a` is defined more than once"), "{error}");
    }

    #[test]
    fn emits_void_function_with_ret_void() {
        let text = ir("f() : void { }");
        assert!(text.contains("define void @f()"), "{text}");
        assert!(text.contains("ret void"), "{text}");
    }

    #[test]
    fn emits_void_function_with_side_effects() {
        let text = ir(
            "extern print(word) : word;\nf() : void { print(42); }",
        );
        assert!(text.contains("define void @f()"), "{text}");
        assert!(text.contains("call i64 @print(i64 42)"), "{text}");
        assert!(text.contains("ret void"), "{text}");
    }

    #[test]
    fn void_main_wraps_to_exit_zero() {
        let text = ir("main() : void { }");
        assert!(text.contains("define i32 @main()"), "{text}");
        assert!(text.contains("call void @main.hspl()"), "{text}");
        assert!(text.contains("ret i32 0"), "{text}");
    }

    #[test]
    fn rejects_void_local() {
        let error = ir_err("f() : void { auto x : void; }");
        assert!(error.contains("local `x` cannot have type `void`"), "{error}");
    }

    #[test]
    fn rejects_void_parameter() {
        let error = ir_err("f(x : void) : void { }");
        assert!(error.contains("parameter `x` of `f` cannot have type `void`"), "{error}");
    }

    #[test]
    fn rejects_void_global() {
        let error = ir_err("g : void 42;");
        assert!(error.contains("`g` cannot have type `void`"), "{error}");
    }

    #[test]
    fn rejects_void_call_as_value() {
        let error = ir_err("foo() : void { }\nmain() : word foo();");
        assert!(error.contains("cannot use a `void` call to `foo` as a value of type `word`"), "{error}");
    }

    #[test]
    fn rejects_void_condition() {
        let error = ir_err("foo() : void { }\nf() : void { if foo() { } }");
        assert!(error.contains("cannot use `void` as a condition"), "{error}");
    }

    #[test]
    fn rejects_void_if_branch_value() {
        let error = ir_err("foo() : void { }\nf(flag : bool) : word if flag foo() else 2;");
        assert!(
            error.contains("cannot use a `void` call to `foo` as a value"),
            "{error}"
        );
    }

    #[test]
    fn rejects_undefined_variables() {
        let error = ir_err("main() nope;");
        assert!(error.contains("undefined variable `nope`"), "{error}");
    }

    #[test]
    fn rejects_wrong_arity() {
        let error = ir_err("f(x) x;\nmain() f();");
        assert!(error.contains("expects 1 argument"), "{error}");
    }

    #[test]
    fn rejects_duplicate_definitions() {
        let error = ir_err("x 1;\nx 2;");
        assert!(error.contains("defined more than once"), "{error}");
    }

    #[test]
    fn rejects_duplicate_externs() {
        let error = ir_err("extern print(word) : word;\nextern print(word) : word;");
        assert!(error.contains("declared more than once"), "{error}");
    }

    #[test]
    fn rejects_undefined_asm_operand() {
        let error = ir_err("f() asm { movq $nope, $out }");
        assert!(error.contains("undefined variable `$nope`"), "{error}");
    }

    #[test]
    fn rejects_main_with_parameters() {
        let error = ir_err("main(argc) argc;");
        assert!(error.contains("`main` cannot take parameters"), "{error}");
    }

    #[test]
    fn rewrites_asm_placeholders() {
        assert_eq!(
            rewrite_asm("movq $x, $out\naddq $x, $out"),
            (
                "movq $1, $0\naddq $1, $0".to_string(),
                vec!["x".to_string()]
            )
        );
        assert_eq!(
            rewrite_asm("movq $$42, $out"),
            ("movq $$42, $0".to_string(), Vec::<String>::new())
        );
        assert_eq!(
            rewrite_asm("movq $a, $out\naddq $b, $out"),
            (
                "movq $1, $0\naddq $2, $0".to_string(),
                vec!["a".to_string(), "b".to_string()]
            )
        );
    }
}
