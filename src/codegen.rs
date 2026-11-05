//! LLVM IR generation for HSPL.
//!
//! Every value has one of the nine types: five integer types (`word`,
//! `int`, `short`, `byte`, `bool`), two IEEE-754 floating-point types (`float`,
//! `double`), named struct types (`point : struct { x : word, y : word };`),
//! and `void`, which is only valid as a function or `extern`
//! return type and produces no value. They are represented as LLVM `i64`,
//! `i32`, `i16`, `i8`, `i1`, `f32`, `f64`, `%name = type { ... }`, and
//! `void`. There are no comparison
//! or logical operators yet; arithmetic (`+ - * / %`), bitwise (`& | ^ ~ <<
//! >>`) and reference (`&` address-of, `*` dereference) operators are
//! expressions, and computation happens through those operations, function
//! calls, inline assembly, and a small set of control-flow constructs.
//!
//! Struct values are zero-initialised, pass by value between functions, and
//! are addressed field-by-field through GEPs (the base pointer itself is
//! folded by LLVM's zero-index fold when it is the first field).
//!
//! - Globals become module-level globals of their declared type.
//! - Functions use their annotated parameter and return types. The result is the
//!   value of the body's single statement.
//! - `extern name(types...) : ret;` declares a typed symbol defined elsewhere
//!   (for example the standard library's `print`).
//! - Implicit conversion is widening only: `bool` zero-extends, narrower
//!   integers sign-extend, `float` widens to `double`, and an integer widens
//!   to a floating-point type only when exactly representable (`byte` and
//!   `short` to `float`; up to `int` to `double`). Narrowing is rejected.
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
//! `$out` is the block's output operand; each distinct `$name` is an input
//! operand loaded from a variable. Operands are numbered output-first, so
//! `$a` becomes `$1` and `$out` becomes `$0`. `$$` escapes a literal dollar
//! for immediates (`movq $$42, $out`). The result type comes from context
//! (the return type or parameter type) and defaults to `word`; inputs keep
//! their variables' types. Integer operands use the `r` constraint;
//! floating-point operands use the `x` constraint (an SSE register on
//! x86-64).

use std::collections::{HashMap, HashSet};

use inkwell::basic_block::BasicBlock;
use inkwell::builder::{Builder, BuilderError};
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::targets::TargetMachine;
use inkwell::types::{BasicMetadataTypeEnum, BasicTypeEnum, IntType, StructType};
use inkwell::values::{BasicValueEnum, FunctionValue, IntValue, PointerValue};
use inkwell::{AddressSpace, FloatPredicate, IntPredicate};

use crate::ast::{
    Binding, Block, BinaryOp, Expr, Function, Item, Literal, Program, Stmt, Type, UnaryOp,
};

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
        structs: HashMap::new(),
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
#[derive(Clone)]
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
    /// Named LLVM struct types for each declared HSPL struct, keyed by name.
    structs: HashMap<String, StructType<'ctx>>,
}

/// Shorthand for the builder's `Result<T, BuilderError>`.
fn build<T>(result: Result<T, BuilderError>) -> Result<T, String> {
    result.map_err(|err| err.to_string())
}

/// The source spelling of a binary operation, for diagnostics.
fn op_symbol(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "add",
        BinaryOp::Sub => "sub",
        BinaryOp::Mul => "mul",
        BinaryOp::Div => "div",
        BinaryOp::Rem => "rem",
        BinaryOp::And => "and",
        BinaryOp::Or => "or",
        BinaryOp::Xor => "xor",
        BinaryOp::Shl => "shl",
        BinaryOp::Shr => "shr",
    }
}

/// Whether a binary operation is a bitwise (or shift) operation, which only
/// make sense for integers.
fn is_bitwise(op: BinaryOp) -> bool {
    matches!(
        op,
        BinaryOp::And | BinaryOp::Or | BinaryOp::Xor | BinaryOp::Shl | BinaryOp::Shr
    )
}

/// Check that `value` can be represented by `ty` when written as a literal.
///
/// The accepted range covers both the signed and unsigned interpretations of
/// the type's width, so `byte 255` (stored as -1) and `int 0xFFFFFFFF` work.
fn check_literal(value: i64, ty: &Type, context: &str) -> Result<(), String> {
    let (min, max): (i128, i128) = match ty {
        // Every `i64` is a valid word; hex literals wrap to `i64` in the lexer.
        Type::Word => return Ok(()),
        Type::Int => (-(1i128 << 31), (1i128 << 32) - 1),
        Type::Short => (-(1i128 << 15), (1i128 << 16) - 1),
        Type::Byte => (-(1i128 << 7), (1i128 << 8) - 1),
        Type::Bool => (0, 1),
        Type::Float | Type::Double => {
            unreachable!("literal_value routes floating-point targets before check_literal")
        }
        Type::Void => return Ok(()),
        Type::Struct { .. } => {
            return Err(format!(
                "{context} {value} cannot be used as a `{}` value",
                ty.name()
            ))
        }
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

/// Check that `value` can be represented by `ty` when written as a
/// floating-point literal. Overflow to infinity (or an already-infinite
/// value) is rejected.
fn check_float_literal(value: f64, ty: &Type, context: &str) -> Result<(), String> {
    let limit = match ty {
        Type::Float => f32::MAX as f64,
        Type::Double => f64::MAX,
        _ => unreachable!("literal_value routes floating-point targets before check_float_literal"),
    };
    if !value.is_finite() || value.abs() > limit {
        return Err(format!(
            "{context} {value} is out of range for `{}`",
            ty.name()
        ));
    }
    Ok(())
}

/// The largest integer magnitude exactly representable in `ty`. `i64` literals
/// beyond this cannot be converted to a float constant without rounding.
fn exact_integer_limit(ty: &Type) -> i128 {
    match ty {
        Type::Float => 1i128 << 24,
        Type::Double => 1i128 << 53,
        _ => i64::MAX as i128,
    }
}

impl<'ctx> Codegen<'ctx> {
    // ----- types ---------------------------------------------------------

    /// Whether `ty` is an integer type that the built-in operations run on:
    /// `word` (64-bit), `int` (32-bit), `short` (16-bit), or `byte` (8-bit).
    /// The boolean type is not an arithmetic type.
    fn is_plain_int(&self, ty: &Type) -> bool {
        matches!(ty, Type::Word | Type::Int | Type::Short | Type::Byte)
    }

    fn int_type(&self, ty: Type) -> IntType<'ctx> {
        match &ty {
            Type::Word => return self.context.i64_type(),
            Type::Int => return self.context.i32_type(),
            Type::Short => return self.context.i16_type(),
            Type::Byte => return self.context.i8_type(),
            Type::Bool => return self.context.bool_type(),
            _ => {}
        }
        panic!("internal error: `{}` has no integer representation", ty.name())
    }

    fn float_type(&self, ty: Type) -> inkwell::types::FloatType<'ctx> {
        match &ty {
            Type::Float => return self.context.f32_type(),
            Type::Double => return self.context.f64_type(),
            _ => {}
        }
        panic!("internal error: `{}` has no floating-point representation", ty.name())
    }

    /// The named LLVM struct type for a declared HSPL struct.
    fn struct_llvm_type(&self, name: &str) -> StructType<'ctx> {
        self.structs
            .get(name)
            .copied()
            .unwrap_or_else(|| panic!("internal error: struct `{name}` was not declared"))
    }

    /// The LLVM type for any first-class HSPL type.
    fn llvm_type(&self, ty: Type) -> BasicTypeEnum<'ctx> {
        match &ty {
            Type::Void => panic!("internal error: `void` is not a first-class LLVM type"),
            Type::Float => self.context.f32_type().into(),
            Type::Double => self.context.f64_type().into(),
            Type::Struct { name, .. } => self.struct_llvm_type(name).into(),
            _ => self.int_type(ty).into(),
        }
    }

    /// The LLVM function type for a signature. `void` returns LLVM `void`;
    /// floating-point returns use the matching float type; everything else is
    /// an integer. Struct types use their named aggregate.
    fn fn_type(
        &self,
        ret: Type,
        params: &[BasicMetadataTypeEnum<'ctx>],
    ) -> inkwell::types::FunctionType<'ctx> {
        match &ret {
            Type::Void => self.context.void_type().fn_type(params, false),
            Type::Float | Type::Double => self.float_type(ret.clone()).fn_type(params, false),
            Type::Struct { name, .. } => self
                .struct_llvm_type(name)
                .fn_type(params, false),
            _ => self.int_type(ret.clone()).fn_type(params, false),
        }
    }

    /// A zero of `ty`, or no value when `ty` is `void`.
    fn zero(&self, ty: Type) -> TypedValue<'ctx> {
        let value = if ty.is_void() {
            None
        } else if let Type::Struct { name, .. } = &ty {
            Some(self.struct_llvm_type(name).const_zero().into())
        } else {
            Some(self.llvm_type(ty.clone()).const_zero())
        };
        TypedValue { value, ty }
    }

    /// An integer constant of `ty` with the bit pattern of `value`. `ty` must
    /// be a value type, not `void` and not floating-point.
    fn constant(&self, ty: Type, value: i64) -> TypedValue<'ctx> {
        TypedValue {
            value: Some(self.int_type(ty.clone()).const_int(value as u64, false).into()),
            ty,
        }
    }

    /// A floating-point constant of `ty` (`float` or `double`).
    fn float_constant(&self, ty: Type, value: f64) -> TypedValue<'ctx> {
        TypedValue {
            value: Some(self.float_type(ty.clone()).const_float(value).into()),
            ty,
        }
    }

    /// Build a typed constant from a written literal, checking it against
    /// `ty`. Integer literals adopt a floating-point `ty` only when exactly
    /// representable; floating-point literals are only valid for floating-point
    /// `ty`.
    fn literal_value(
        &self,
        ty: Type,
        literal: &Literal,
        context: &str,
    ) -> Result<TypedValue<'ctx>, String> {
        // A struct global or local is zero-initialised; there is no struct
        // literal syntax, so the synthetic zero from the parser is the only
        // literal a struct is ever paired with.
        if let Type::Struct { .. } = ty {
            return match literal {
                Literal::Int(0) => Ok(self.zero(ty)),
                _ => Err(format!(
                    "{context} cannot be used as a `{}` initializer; struct values are zero-initialised and their fields are assigned one at a time",
                    ty.name()
                )),
            };
        }
        match (literal, &ty) {
            (Literal::Int(value), Type::Float | Type::Double) => {
                if (*value as i128).abs() <= exact_integer_limit(&ty) {
                    Ok(self.float_constant(ty, *value as f64))
                } else {
                    Err(format!(
                        "{context} {value} is out of range for `{}` (must be exactly representable)",
                        ty.name()
                    ))
                }
            }
            (Literal::Int(value), _) => {
                check_literal(*value, &ty, context)?;
                Ok(self.constant(ty, *value))
            }
            (Literal::Float(value), Type::Float | Type::Double) => {
                check_float_literal(*value, &ty, context)?;
                Ok(self.float_constant(ty, *value))
            }
            (Literal::Float(value), _) => Err(format!(
                "{context} {value} cannot be used as `{}`",
                ty.name()
            )),
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

        if !value.ty.can_widen_to(&target) {
            return Err(format!(
                "cannot implicitly convert `{}` to `{}` (only widening conversions are allowed)",
                value.ty.name(),
                target.name()
            ));
        }

        let input = value.value.expect("value type has a value");

        // Floating-point targets: widening between floats, or an exact
        // integer (or bool) to float.
        if target.is_float() {
            let target_type = self.float_type(target.clone());
            if value.ty.is_float() {
                let extended = build(
                    self.builder.build_float_ext(input.into_float_value(), target_type, "fpext"),
                )?;
                return Ok(TypedValue {
                    value: Some(extended.into()),
                    ty: target,
                });
            }
            if value.ty.is_bool() {
                let extended = build(
                    self.builder
                        .build_unsigned_int_to_float(input.into_int_value(), target_type, "uitofp"),
                )?;
                return Ok(TypedValue {
                    value: Some(extended.into()),
                    ty: target,
                });
            }
            let extended = build(
                self.builder
                    .build_signed_int_to_float(input.into_int_value(), target_type, "sitofp"),
            )?;
            return Ok(TypedValue {
                value: Some(extended.into()),
                ty: target,
            });
        }

        // Integer targets can only come from integer sources: a float source
        // would have been rejected by `can_widen_to`.
        let input = input.into_int_value();
        let target_type = self.int_type(target.clone());
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
        let mut seen_structs = HashSet::new();

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
                Item::StructDecl(decl) => {
                    if decl.name == "print" {
                        return Err(RESERVED_PRINT.to_string());
                    }
                    if Type::from_name(&decl.name).is_some() {
                        return Err(format!(
                            "struct `{}` cannot use a builtin type name",
                            decl.name
                        ));
                    }
                    if !seen_structs.insert(decl.name.clone()) {
                        return Err(format!("struct `{}` is defined more than once", decl.name));
                    }
                    if !seen.insert(decl.name.clone()) {
                        return Err(format!("`{}` is defined more than once", decl.name));
                    }
                }
            }
        }

        // Create the struct types in two passes: every named type exists
        // before any body is emitted.
        for item in &program.items {
            if let Item::StructDecl(decl) = item {
                let struct_ty = self.context.opaque_struct_type(&decl.name);
                self.structs.insert(decl.name.clone(), struct_ty);
            }
        }
        for item in &program.items {
            if let Item::StructDecl(decl) = item {
                let field_types: Vec<BasicTypeEnum<'ctx>> = decl
                    .fields
                    .iter()
                    .map(|field| self.llvm_type(field.ty.clone()))
                    .collect();
                self.struct_llvm_type(&decl.name)
                    .set_body(&field_types, false);
            }
        }

        for item in &program.items {
            if let Item::Global(global) = item {
                let context = format!("global `{}` initializer", global.name);
                let constant = self.literal_value(global.ty.clone(), &global.init, &context)?;
                let value = self
                    .module
                    .add_global(self.llvm_type(global.ty.clone()), None, &global.name);
                let basic = constant.value.expect("global initializer has a value");
                value.set_initializer(&basic);
                self.globals.insert(
                    global.name.clone(),
                    (value.as_pointer_value(), global.ty.clone()),
                );
            }
        }

        for item in &program.items {
            if let Item::Extern(decl) = item {
                for ty in decl.params.iter().chain(std::iter::once(&decl.ret)) {
                    if ty.is_struct() {
                        return Err(format!(
                            "`extern` signatures cannot use struct types (struct `{}`); struct values are passed by value between HSPL functions only",
                            ty.name()
                        ));
                    }
                }
                let param_types: Vec<BasicMetadataTypeEnum<'ctx>> = decl
                    .params
                    .iter()
                    .map(|ty| self.llvm_type(ty.clone()).into())
                    .collect();
                let fn_type = self.fn_type(decl.ret.clone(), &param_types);
                let value = self.module.add_function(&decl.name, fn_type, None);
                self.externs.insert(
                    decl.name.clone(),
                    FnSig {
                        value,
                        params: decl.params.clone(),
                        ret: decl.ret.clone(),
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
        if function.name == "main" {
            if !function.params.is_empty() {
                return Err("`main` cannot take parameters".to_string());
            }
            if function.ret.is_float() {
                return Err(
                    "`main` cannot return `float` or `double`; use an integer type (or omit the annotation for `void`)"
                        .to_string(),
                );
            }
            if function.ret.is_struct() {
                return Err(
                    "`main` cannot return a struct type; return an integer type (or omit the annotation for `void`)"
                        .to_string(),
                );
            }
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
            .map(|param| self.llvm_type(param.ty.clone()).into())
            .collect();
        let fn_type = self.fn_type(function.ret.clone(), &param_types);

        let llvm_name = Self::llvm_function_name(&function.name);
        let value = self.module.add_function(&llvm_name, fn_type, None);
        self.functions.insert(
            function.name.clone(),
            FnSig {
                value,
                params: function.params.iter().map(|param| param.ty.clone()).collect(),
                ret: function.ret.clone(),
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
                    .build_alloca(self.llvm_type(param.ty.clone()), &param.name),
            )?;
            let arg = sig
                .value
                .get_nth_param(index as u32)
                .ok_or_else(|| format!("internal error: missing parameter {index}"))?;
            build(self.alloca_builder.build_store(slot, arg))?;
            self.scopes
                .last_mut()
                .expect("scope exists")
                .insert(param.name.clone(), (slot, param.ty.clone()));
        }

        // The body's value is the function's result. A `void` function yields
        // no value and ends with `ret void`.
        let result = self.gen_stmt(&function.body, Some(function.ret.clone()))?;

        if function.ret.is_void() {
            if self.current_block()?.get_terminator().is_none() {
                build(self.builder.build_return(None))?;
            }
        } else {
            let result = self.coerce(result, function.ret.clone())?;

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
            Type::Float | Type::Double => {
                unreachable!("`main` cannot return a floating-point type")
            }
            Type::Struct { .. } => {
                unreachable!("`main` cannot return a struct type")
            }
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
                self.zero(expected.clone().unwrap_or(Type::Word))
            }
            Stmt::Assign(path, expr) => {
                let mut components = path.split('.');
                let base = components.next().expect("assignment path has a base");
                let (mut slot, mut ty) = self
                    .lookup(base)
                    .ok_or_else(|| format!("assignment to undefined variable `{base}`"))?;
                for field in components {
                    (slot, ty) = self.field_pointer(slot, &ty, field)?;
                }
                let value = self.gen_expr(expr, Some(ty.clone()))?;
                let value = self.coerce(value, ty.clone())?;
                build(self.builder.build_store(
                    slot,
                    *value.value.as_ref().expect("assigned value"),
                ))?;
                value
            }
            Stmt::Expr(expr) => self.gen_expr(expr, expected.clone())?,
            Stmt::Jump(name) => self.gen_jump(name, expected.clone())?,
            Stmt::Label(_) => unreachable!("labels are handled above"),
            Stmt::Empty => self.zero(expected.clone().unwrap_or(Type::Word)),
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
            Some(result_ty.clone())
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
        let then_value = self.gen_expr(then_branch, branch_expected.clone())?;
        let then_value = if result_ty.is_void() {
            then_value
        } else {
            self.coerce(then_value, result_ty.clone())?
        };
        self.ensure_open_block()?;
        let then_end = self.current_block()?;
        build(self.builder.build_unconditional_branch(merge_block))?;

        self.builder.position_at_end(else_block);
        let else_value = match else_branch {
            Some(expr) => self.gen_expr(expr, branch_expected.clone())?,
            None => self.zero(result_ty.clone()),
        };
        let else_value = if result_ty.is_void() {
            else_value
        } else {
            self.coerce(else_value, result_ty.clone())?
        };
        self.ensure_open_block()?;
        let else_end = self.current_block()?;
        build(self.builder.build_unconditional_branch(merge_block))?;

        self.builder.position_at_end(merge_block);
        if result_ty.is_void() {
            return Ok(TypedValue { value: None, ty: Type::Void });
        }

        let phi = build(self.builder.build_phi(self.llvm_type(result_ty.clone()), "if"))?;
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

    /// Turn a condition value into an `i1`. Booleans are used directly,
    /// integers are true when non-zero, and floating-point values are true
    /// when not equal to zero (NaN is false, as in C).
    fn as_condition(&mut self, value: TypedValue<'ctx>) -> Result<IntValue<'ctx>, String> {
        if value.ty.is_void() {
            return Err("cannot use `void` as a condition".to_string());
        }
        if value.ty.is_bool() {
            return Ok(value.value.expect("condition is a value").into_int_value());
        }
        if value.ty.is_struct() {
            return Err(format!(
                "cannot use `{}` as a condition",
                value.ty.name()
            ));
        }
        if value.ty.is_float() {
            let float_value = value.value.expect("condition is a value").into_float_value();
            let zero = self.float_type(value.ty).const_zero();
            return build(
                self.builder
                    .build_float_compare(FloatPredicate::ONE, float_value, zero, "cond"),
            );
        }
        let int_value = value.value.expect("condition is a value").into_int_value();
        let zero = self.int_type(value.ty).const_zero();
        build(
            self.builder
                .build_int_compare(IntPredicate::NE, int_value, zero, "cond"),
        )
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
                .build_alloca(self.llvm_type(binding.ty.clone()), &binding.name),
        )?;
        // `binding.ty` is never `void` (validated in `declare_function`), so
        // the zero value always exists.
        let zero = self.zero(binding.ty.clone()).value.expect("local zero value");
        build(self.alloca_builder.build_store(slot, zero))?;
        build(self.builder.build_store(slot, zero))?;
        self.scopes
            .last_mut()
            .expect("scope exists")
            .insert(binding.name.clone(), (slot, binding.ty.clone()));
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
        if expected.as_ref().is_some_and(|ty| ty.is_void()) {
            return self
                .gen_expr(expr, None)
                .map(|_| TypedValue { value: None, ty: Type::Void });
        }

        match expr {
            Expr::Int(value) => {
                let ty = expected.unwrap_or(Type::Word);
                self.literal_value(ty, &Literal::Int(*value), "integer literal")
            }
            Expr::Float(value) => {
                let ty = expected.unwrap_or(Type::Double);
                self.literal_value(ty, &Literal::Float(*value), "floating-point literal")
            }
            // A boolean literal is `bool` on its own, but adopts an expected
            // integer type directly (equivalent to zero-extension), and an
            // expected floating-point type exactly (0.0 or 1.0).
            Expr::Bool(value) => {
                let ty = expected.unwrap_or(Type::Bool);
                self.literal_value(ty, &Literal::Int(i64::from(*value)), "boolean literal")
            }
            Expr::Var(name) => {
                let (slot, ty) = self
                    .lookup(name)
                    .ok_or_else(|| format!("use of undefined variable `{name}`"))?;
                let loaded = build(self.builder.build_load(self.llvm_type(ty.clone()), slot, name))?;
                let value = TypedValue { value: Some(loaded), ty };
                match expected {
                    Some(target) => self.coerce(value, target),
                    None => Ok(value),
                }
            }
            Expr::Field { base, field } => {
                let (slot, fty) = self.field_address(base, field)?;
                let loaded =
                    build(self.builder.build_load(self.llvm_type(fty.clone()), slot, field))?;
                let value = TypedValue { value: Some(loaded), ty: fty };
                match expected {
                    Some(target) => self.coerce(value, target),
                    None => Ok(value),
                }
            }
            Expr::Call(name, args) => self.gen_call(name, args, expected),
            Expr::Asm(source) => self.gen_asm(source, expected.unwrap_or(Type::Word)),
            Expr::Block(block) => self.gen_block(block, expected),
            Expr::Unary { op, operand } => self.gen_unary(*op, operand, expected),
            Expr::Binary { op, lhs, rhs } => self.gen_binary(*op, lhs, rhs, expected),
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => self.gen_if(cond, then_branch, else_branch.as_deref(), expected),
        }
    }

    /// Lower a unary operation. `-`, `~`, `&` (address-of) and `*` (deref)
    /// are the prefix operators; `&` and `*` also exist infix, where they are
    /// handled by [`Codegen::gen_binary`].
    fn gen_unary(
        &mut self,
        op: UnaryOp,
        operand: &Expr,
        expected: Option<Type>,
    ) -> Result<TypedValue<'ctx>, String> {
        let value = self.gen_expr(operand, expected.clone())?;

        if value.ty.is_void() {
            return Err("cannot apply a unary operation to `void`".to_string());
        }

        match op {
            UnaryOp::Neg => {
                if value.ty.is_float() {
                    let result = build(
                        self.builder
                            .build_float_neg(value.value.expect("float operand").into_float_value(), "neg"),
                    )?;
                    return Ok(TypedValue {
                        value: Some(result.into()),
                        ty: value.ty,
                    });
                }
                if self.is_plain_int(&value.ty) {
                    let result = build(
                        self.builder
                            .build_int_neg(value.value.expect("integer operand").into_int_value(), "neg"),
                    )?;
                    return Ok(TypedValue {
                        value: Some(result.into()),
                        ty: value.ty,
                    });
                }
                Err(format!(
                    "the unary `-` operator cannot be applied to `{}`",
                    value.ty.name()
                ))
            }
            UnaryOp::BitNot => {
                if !self.is_plain_int(&value.ty) {
                    return Err(format!(
                        "the unary `~` operator cannot be applied to `{}`",
                        value.ty.name()
                    ));
                }
                let result = build(
                    self.builder
                        .build_not(value.value.expect("integer operand").into_int_value(), "not"),
                )?;
                Ok(TypedValue {
                    value: Some(result.into()),
                    ty: value.ty,
                })
            }
            UnaryOp::Ref => self.gen_ref(operand, expected),
            UnaryOp::Deref => self.gen_deref(operand, expected),
        }
    }

    /// Compute the address of a named field inside a struct value living at
    /// `parent_slot`. Returns the field's pointer and its type.
    fn field_pointer(
        &self,
        parent_slot: PointerValue<'ctx>,
        parent: &Type,
        field: &str,
    ) -> Result<(PointerValue<'ctx>, Type), String> {
        let Type::Struct { name, fields } = parent else {
            return Err(format!(
                "`{}` is not a struct type, so it has no field `{field}`",
                parent.name()
            ));
        };
        let index = fields
            .iter()
            .position(|(field_name, _)| field_name == field)
            .ok_or_else(|| format!("struct `{name}` has no field `{field}`"))?;
        let struct_ty = self.struct_llvm_type(name);
        let pointer = build(self.builder.build_struct_gep(struct_ty, parent_slot, index as u32, field))?;
        Ok((pointer, fields[index].1.clone()))
    }

    /// The address of the field `field` inside `base`. `base` must be a
    /// variable or another field access — field access only makes sense on an
    /// addressable (place) expression.
    fn field_address(
        &mut self,
        base: &Expr,
        field: &str,
    ) -> Result<(PointerValue<'ctx>, Type), String> {
        match base {
            Expr::Var(name) => {
                let (slot, ty) = self
                    .lookup(name)
                    .ok_or_else(|| format!("use of undefined variable `{name}`"))?;
                self.field_pointer(slot, &ty, field)
            }
            Expr::Field { base, field: inner_field } => {
                let (slot, ty) = self.field_address(base, inner_field)?;
                self.field_pointer(slot, &ty, field)
            }
            _ => Err(
                "field access is only supported on a variable or another field access, for example `p.x` or `p.inner.x`"
                    .to_string(),
            ),
        }
    }

    /// Take the address of a variable: the result is a `word` holding the
    /// address, which a later dereference (`*`) can load through. Only
    /// `word`-typed variables are addressable for now; `short` and `byte`
    /// variables are skipped.
    fn gen_ref(
        &mut self,
        operand: &Expr,
        _expected: Option<Type>,
    ) -> Result<TypedValue<'ctx>, String> {
        let Expr::Var(name) = operand else {
            return Err("`&` (address-of) can only be applied to a variable".to_string());
        };
        let (slot, ty) = self
            .lookup(name)
            .ok_or_else(|| format!("cannot take the address of undefined variable `{name}`"))?;
        if ty != Type::Word {
            return Err(format!(
                "`&` (address-of) is only available for `word` values right now (variable `{name}` is `{}`)",
                ty.name()
            ));
        }
        let address = build(self.builder.build_ptr_to_int(slot, self.context.i64_type(), "ref"))?;
        Ok(TypedValue {
            value: Some(address.into()),
            ty: Type::Word,
        })
    }

    /// Dereference an address held in a `word`: load through it and yield the
    /// referenced value as a `word`.
    fn gen_deref(
        &mut self,
        operand: &Expr,
        _expected: Option<Type>,
    ) -> Result<TypedValue<'ctx>, String> {
        let value = self.gen_expr(operand, Some(Type::Word))?;
        if value.ty != Type::Word {
            return Err(format!(
                "cannot dereference a value of type `{}` (an address is a `word`)",
                value.ty.name()
            ));
        }
        let int = value.value.expect("deref operand has a value");
        let int_value = int.into_int_value();
        let pointer = build(
            self.builder
                .build_int_to_ptr(int_value, self.context.ptr_type(AddressSpace::default()), "deref"),
        )?;
        let loaded = build(self.builder.build_load(self.llvm_type(Type::Word), pointer, "deref.load"))?;
        Ok(TypedValue {
            value: Some(loaded.into_int_value().into()),
            ty: Type::Word,
        })
    }

    /// Lower a binary operation: arithmetic or bitwise, on integers and
    /// floating-point values.
    fn gen_binary(
        &mut self,
        op: BinaryOp,
        lhs: &Expr,
        rhs: &Expr,
        expected: Option<Type>,
    ) -> Result<TypedValue<'ctx>, String> {
        let lhs_value = self.gen_expr(lhs, expected.clone())?;
        if lhs_value.ty.is_void() {
            return Err("cannot apply a binary operation to `void`".to_string());
        }
        let rhs_value = self.gen_expr(rhs, expected)?;
        if rhs_value.ty.is_void() {
            return Err("cannot apply a binary operation to `void`".to_string());
        }
        if lhs_value.ty.is_struct() || rhs_value.ty.is_struct() {
            return Err(format!(
                "the `{}` operator cannot be applied to a struct value (operate on its fields instead)",
                op_symbol(op)
            ));
        }

        let (result_ty, lhs_value, rhs_value) = if lhs_value.ty.is_float() || rhs_value.ty.is_float() {
            // Floating-point arithmetic (possibly after widening an integer).
            let result_ty = if lhs_value.ty.is_float() {
                lhs_value.ty.clone()
            } else {
                rhs_value.ty.clone()
            };
            let lhs = if lhs_value.ty == result_ty {
                lhs_value
            } else {
                self.coerce(lhs_value, result_ty.clone())?
            };
            let rhs = if rhs_value.ty == result_ty {
                rhs_value
            } else {
                self.coerce(rhs_value, result_ty.clone())?
            };
            (result_ty, lhs, rhs)
        } else {
            // Integer arithmetic or bitwise operation. Pick the wider type;
            // widening is the language's only conversion.
            let result_ty = if lhs_value.ty == rhs_value.ty {
                lhs_value.ty.clone()
            } else if rhs_value.ty.can_widen_to(&lhs_value.ty) {
                rhs_value.ty.clone()
            } else if lhs_value.ty.can_widen_to(&rhs_value.ty) {
                lhs_value.ty.clone()
            } else {
                return Err(format!(
                    "cannot combine `{}` and `{}`",
                    lhs_value.ty.name(),
                    rhs_value.ty.name()
                ));
            };
            let lhs = if lhs_value.ty == result_ty {
                lhs_value
            } else {
                self.coerce(lhs_value, result_ty.clone())?
            };
            let rhs = if rhs_value.ty == result_ty {
                rhs_value
            } else {
                self.coerce(rhs_value, result_ty.clone())?
            };
            (result_ty, lhs, rhs)
        };

        if result_ty == Type::Bool {
            return Err(format!(
                "the `{}` operator is not available for `{}` yet (widen the operands first)",
                op_symbol(op),
                result_ty.name()
            ));
        }

        if is_bitwise(op) && result_ty.is_float() {
            return Err(format!(
                "the `{}` operator cannot be applied to floating-point type `{}`",
                op_symbol(op),
                result_ty.name()
            ));
        }

        let value = if result_ty.is_float() {
            let lhs = lhs_value.value.expect("operand has a value");
            let rhs = rhs_value.value.expect("operand has a value");
            let lhs = lhs.into_float_value();
            let rhs = rhs.into_float_value();
            let result = match op {
                BinaryOp::Add => build(self.builder.build_float_add(lhs, rhs, "add"))?,
                BinaryOp::Sub => build(self.builder.build_float_sub(lhs, rhs, "sub"))?,
                BinaryOp::Mul => build(self.builder.build_float_mul(lhs, rhs, "mul"))?,
                BinaryOp::Div => build(self.builder.build_float_div(lhs, rhs, "div"))?,
                BinaryOp::Rem => build(self.builder.build_float_rem(lhs, rhs, "rem"))?,
                _ => unreachable!("the result of a floating-point operation is never a bitwise one"),
            };
            TypedValue {
                value: Some(result.into()),
                ty: result_ty,
            }
        } else {
            let lhs = lhs_value.value.expect("operand has a value");
            let rhs = rhs_value.value.expect("operand has a value");
            let lhs = lhs.into_int_value();
            let rhs = rhs.into_int_value();
            let result = match op {
                BinaryOp::Add => build(self.builder.build_int_add(lhs, rhs, "add"))?,
                BinaryOp::Sub => build(self.builder.build_int_sub(lhs, rhs, "sub"))?,
                BinaryOp::Mul => build(self.builder.build_int_mul(lhs, rhs, "mul"))?,
                BinaryOp::Div => build(self.builder.build_int_signed_div(lhs, rhs, "div"))?,
                BinaryOp::Rem => build(self.builder.build_int_signed_rem(lhs, rhs, "rem"))?,
                BinaryOp::And => build(self.builder.build_and(lhs, rhs, "and"))?,
                BinaryOp::Or => build(self.builder.build_or(lhs, rhs, "or"))?,
                BinaryOp::Xor => build(self.builder.build_xor(lhs, rhs, "xor"))?,
                BinaryOp::Shl => build(self.builder.build_left_shift(lhs, rhs, "shl"))?,
                BinaryOp::Shr => build(self.builder.build_right_shift(lhs, rhs, true, "shr"))?,
            };
            TypedValue {
                value: Some(result.into()),
                ty: result_ty,
            }
        };

        Ok(value)
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
        for (arg, param) in args.iter().zip(&sig.params) {
            let value = self.gen_expr(arg, Some(param.clone()))?;
            let value = self.coerce(value, param.clone())?;
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
        if result_ty.is_struct() {
            return Err(
                format!("asm block cannot produce a `{}` value (struct values have no register constraint)", result_ty.name())
            );
        }
        let (template, inputs) = rewrite_asm(source);

        let mut args = Vec::with_capacity(inputs.len());
        let mut param_types: Vec<BasicMetadataTypeEnum<'ctx>> = Vec::with_capacity(inputs.len());
        let mut input_types: Vec<Type> = Vec::with_capacity(inputs.len());
        for name in &inputs {
            let (slot, ty) = self
                .lookup(name)
                .ok_or_else(|| format!("asm block references undefined variable `${name}`"))?;
            let value = build(self.builder.build_load(self.llvm_type(ty.clone()), slot, name))?;
            args.push(value.into());
            param_types.push(self.llvm_type(ty.clone()).into());
            input_types.push(ty);
        }

        let fn_type = self.fn_type(result_ty.clone(), &param_types);

        // One output followed by one input register per variable. Floating-point
        // operands use the `x` constraint (SSE registers), integers use `r`;
        // struct values have no constraint and are rejected.
        let constraint_for = |ty: &Type| -> Result<&'static str, String> {
            if ty.is_float() {
                Ok("x")
            } else if self.is_plain_int(ty) {
                Ok("r")
            } else {
                Err(format!(
                    "asm blocks cannot take `{}` operands (only integers and floating-point values have register constraints)",
                    ty.name()
                ))
            }
        };
        let mut constraints = format!("={}", constraint_for(&result_ty)?);
        for ty in &input_types {
            constraints.push(',');
            constraints.push_str(constraint_for(ty)?);
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
            .find_map(|scope| scope.get(name).cloned())
            .or_else(|| self.globals.get(name).cloned())
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

    #[test]
    fn floats_are_first_class_types() {
        let text = ir("f(x : float, y : double) : double y;");
        assert!(text.contains("define double @f(float %0, double %1)"), "{text}");
        let text = ir("f() : float 1.0;");
        assert!(text.contains("define float @f()"), "{text}");
    }

    #[test]
    fn float_globals_are_emitted() {
        let text = ir("pi : double 3.25;\nzero : float;\nplain 7;");
        assert!(text.contains("@pi = global double 3.250000e+00"), "{text}");
        assert!(text.contains("@zero = global float 0.000000e+00"), "{text}");
        assert!(text.contains("@plain = global i64 7"), "{text}");
    }

    #[test]
    fn float_widens_to_double() {
        let text = ir("f(x : float) : double { auto d : double; d = x; d }");
        assert!(text.contains("fpext float"), "{text}");
        assert!(text.contains("ret double"), "{text}");
    }

    #[test]
    fn narrow_integers_widen_into_float() {
        let text = ir("f(x : byte) : float { auto d : float; d = x; d }");
        assert!(text.contains("sitofp i8"), "{text}");
        let text = ir("f(x : short) : float { auto d : float; d = x; d }");
        assert!(text.contains("sitofp i16"), "{text}");
    }

    #[test]
    fn int_widens_to_double_but_not_float() {
        let text = ir("f(x : int) : double x;");
        assert!(text.contains("sitofp i32"), "{text}");
        let error = ir_err("f(x : int) : float x;");
        assert!(
            error.contains("cannot implicitly convert `int` to `float`"),
            "{error}"
        );
    }

    #[test]
    fn word_does_not_widen_to_double() {
        let error = ir_err("f(x : word) : double x;");
        assert!(
            error.contains("cannot implicitly convert `word` to `double`"),
            "{error}"
        );
    }

    #[test]
    fn floats_do_not_widen_to_integers() {
        let error = ir_err("f(x : double) : word x;");
        assert!(
            error.contains("cannot implicitly convert `double` to `word`"),
            "{error}"
        );
        let error = ir_err("f(x : float) : int { auto d : int; d = x; d }");
        assert!(
            error.contains("cannot implicitly convert `float` to `int`"),
            "{error}"
        );
    }

    #[test]
    fn bool_zero_extends_into_floats() {
        // A boolean literal adopts the float type directly (exact 0.0/1.0).
        let text = ir("f() : double { auto d : double; d = true; d }");
        assert!(text.contains("1.000000e+00"), "{text}");
        // A boolean variable zero-extends into the float.
        let text = ir("f(b : bool) : double { auto d : double; d = b; d }");
        assert!(text.contains("uitofp i1"), "{text}");
        let text = ir("f(b : bool) : float { auto d : float; d = b; d }");
        assert!(text.contains("uitofp i1"), "{text}");
    }

    #[test]
    fn float_literals_adopt_float_context() {
        let text = ir("f() : double 1.5;");
        assert!(text.contains("define double @f()"), "{text}");
        assert!(text.contains("1.500000e+00"), "{text}");
        let text = ir("f() : float 1.5;");
        assert!(text.contains("define float @f()"), "{text}");
        assert!(text.contains("1.500000e+00"), "{text}");
    }

    #[test]
    fn integer_literals_adopt_float_context() {
        let text = ir("f() : double 2;");
        assert!(text.contains("2.000000e+00"), "{text}");
    }

    #[test]
    fn emits_addition_for_words() {
        let text = ir("f(a : word, b : word) : word a + b;");
        assert!(text.contains("add i64"), "{text}");
        assert!(text.contains("ret i64"), "{text}");
    }

    #[test]
    fn emits_subtraction_multiplication_and_division() {
        let text = ir("f(a : word, b : word) : word a - b * a / b;");
        assert!(text.contains("sub i64"), "{text}");
        assert!(text.contains("mul i64"), "{text}");
        assert!(text.contains("sdiv i64"), "{text}");
    }

    #[test]
    fn emits_remainder() {
        let text = ir("f(a : word, b : word) : word a % b;");
        assert!(text.contains("srem i64"), "{text}");
    }

    #[test]
    fn emits_float_arithmetic() {
        let text = ir("f(x : float, y : float) : float x + y;");
        assert!(text.contains("fadd float"), "{text}");
        let text = ir("f(x : double, y : double) : double x * y;");
        assert!(text.contains("fmul double"), "{text}");
    }

    #[test]
    fn emits_bitwise_operations() {
        let text = ir("f(a : word, b : word) : word a & b;");
        assert!(text.contains("and i64"), "{text}");
        let text = ir("f(a : word, b : word) : word a | b;");
        assert!(text.contains("or i64"), "{text}");
        let text = ir("f(a : word, b : word) : word a ^ b;");
        assert!(text.contains("xor i64"), "{text}");
        let text = ir("f(a : word) : word ~a;");
        assert!(text.contains("xor i64"), "{text}");
    }

    #[test]
    fn emits_shifts() {
        let text = ir("f(a : word, b : word) : word a << b;");
        assert!(text.contains("shl i64"), "{text}");
        let text = ir("f(a : word, b : word) : word a >> b;");
        assert!(text.contains("ashr i64"), "{text}");
    }

    #[test]
    fn emits_unary_negation() {
        let text = ir("f(x : word) : word -x;");
        assert!(text.contains("ret i64"), "{text}");
    }

    #[test]
    fn emits_ref_and_deref_for_words() {
        let text = ir("f(x : word) : word { auto y : word; y = &x; *y }");
        assert!(text.contains("ptrtoint"), "{text}");
        assert!(text.contains("inttoptr"), "{text}");
        assert!(text.contains("load i64"), "{text}");
    }

    #[test]
    fn emits_arithmetic_on_short() {
        let text = ir("f(x : short) : short { auto y : short; y = x + x; y }");
        assert!(text.contains("add i16"), "{text}");
    }

    #[test]
    fn emits_arithmetic_on_short_two_operands() {
        let text = ir("f(a : short, b : short) : short a + b;");
        assert!(text.contains("add i16"), "{text}");
        let text = ir("f(a : short, b : short) : short a * b;");
        assert!(text.contains("mul i16"), "{text}");
    }

    #[test]
    fn rejects_ref_on_a_narrow_integer() {
        let error = ir_err("f(x : byte) : word { auto y : word; y = &x; }");
        assert!(error.contains("only available for `word`"), "{error}");
    }

    #[test]
    fn emits_arithmetic_on_byte() {
        let text = ir("f(a : byte, b : byte) : byte a + b;");
        assert!(text.contains("add i8"), "{text}");
        let text = ir("f(a : byte, b : byte) : byte a - b;");
        assert!(text.contains("sub i8"), "{text}");
    }

    #[test]
    fn emits_bitwise_operations_on_short_and_byte() {
        let text = ir("f(a : byte, b : byte) : byte a & b;");
        assert!(text.contains("and i8"), "{text}");
        let text = ir("f(a : short, b : short) : short a << b;");
        assert!(text.contains("shl i16"), "{text}");
    }

    #[test]
    fn emits_unary_negation_on_byte() {
        let text = ir("f(x : byte) : byte -x;");
        assert!(text.contains("ret i8"), "{text}");
    }

    #[test]
    fn rejects_arithmetic_on_bool() {
        let error = ir_err("f(x : bool) : bool x + x;");
        assert!(error.contains("not available for `bool`"), "{error}");
    }

    #[test]
    fn inexact_integer_literal_for_float_is_rejected() {
        // 2^24 + 1 is not representable in f32.
        let error = ir_err("f() : float 16777217;");
        assert!(error.contains("out of range for `float`"), "{error}");
        // 2^53 + 1 is not representable in f64.
        let error = ir_err("f() : double 9007199254740993;");
        assert!(error.contains("out of range for `double`"), "{error}");
    }

    #[test]
    fn float_literal_overflow_is_rejected() {
        let error = ir_err("f() : double 1e999;");
        assert!(error.contains("out of range for `double`"), "{error}");
        let error = ir_err("f() : float 1e999;");
        assert!(error.contains("out of range for `float`"), "{error}");
        let error = ir_err("big : double 1e999;");
        assert!(error.contains("global `big` initializer"), "{error}");
    }

    #[test]
    fn float_literal_in_integer_context_is_rejected() {
        let error = ir_err("f() : word 1.5;");
        assert!(error.contains("cannot be used as `word`"), "{error}");
        let error = ir_err("f() : bool 1.5;");
        assert!(error.contains("cannot be used as `bool`"), "{error}");
    }

    #[test]
    fn float_conditions_compare_with_zero() {
        let text = ir("f(d : double) : word if d { 1 } else { 2 };");
        assert!(text.contains("fcmp one double"), "{text}");
        let text = ir("f(x : float) : word if x { 1 } else { 2 };");
        assert!(text.contains("fcmp one float"), "{text}");
    }

    #[test]
    fn float_asm_uses_the_x_constraint() {
        let text = ir("f(x : double) : double asm { movsd $x, $out }");
        assert!(text.contains("call double asm"), "{text}");
        assert!(text.contains("=x,x"), "{text}");
    }

    #[test]
    fn extern_float_signatures() {
        let text = ir("extern convert(float, double) : double;\nmain() convert(1.5, 2.5);");
        assert!(text.contains("declare double @convert(float, double)"), "{text}");
    }

    #[test]
    fn main_cannot_return_a_float() {
        let error = ir_err("main() : float 1.0;");
        assert!(error.contains("`main` cannot return `float`"), "{error}");
    }

    #[test]
    fn float_global_initializers() {
        let text = ir("pi : double 3.25;\nf() : double pi;");
        assert!(text.contains("@pi = global double 3.250000e+00"), "{text}");
    }

    #[test]
    fn int_literal_adopts_float_global_initializer() {
        let text = ir("two : double 2;");
        assert!(text.contains("@two = global double 2.000000e+00"), "{text}");
    }
}

#[cfg(test)]
mod struct_tests {
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
    fn emits_named_struct_types_and_zeroed_globals() {
        let text = ir("point : struct { x : word, y : word };\norigin : point;\nmain() : word { 0 }");
        assert!(text.contains("%point = type { i64, i64 }"), "{text}");
        assert!(text.contains("@origin = global %point zeroinitializer"), "{text}");
    }

    #[test]
    fn struct_locals_are_zero_initialised() {
        let text = ir(
            "point : struct { x : int, y : short };\nf() : word { auto p : point; 0 }",
        );
        assert!(text.contains("%point = type { i32, i16 }"), "{text}");
        assert!(text.contains("%p = alloca %point"), "{text}");
        assert!(text.contains("store %point zeroinitializer"), "{text}");
    }

    #[test]
    fn structs_are_never_anonymous_in_the_ir() {
        let text = ir("point : struct { x : word };\nmain() : word { auto p : point; 0 }");
        assert!(!text.contains("%struct."), "{text}");
        assert!(text.contains("%point = type { i64 }"), "{text}");
    }

    #[test]
    fn field_read_and_assignment_emit_geps() {
        let text = ir(
            "point : struct { x : word, y : word };\nq(p : point) : word { p.x + p.y }",
        );
        assert!(text.contains("getelementptr inbounds"), "{text}");
        assert!(text.contains("load i64"), "{text}");
        assert!(text.contains("add i64"), "{text}");
    }

    #[test]
    fn field_assignment_stores_through_a_gep() {
        let text = ir(
            "point : struct { x : word, y : word };\n\
             main() : word { auto p : point; p.x = 5; p.y = 7; 0 }",
        );
        assert!(text.contains("store i64 5"), "{text}");
        assert!(text.contains("store i64 7"), "{text}");
    }

    #[test]
    fn nested_struct_fields_chain_geps() {
        let text = ir(
            "inner : struct { n : word };\n\
             outer : struct { inner : inner, m : word };\n\
             main() : word { auto s : outer; s.inner.n = 42; s.inner.n }",
        );
        assert!(text.contains("%outer = type { %inner, i64 }"), "{text}");
        assert!(text.contains("%inner = type { i64 }"), "{text}");
        assert!(text.contains("getelementptr"), "{text}");
    }

    #[test]
    fn struct_arguments_and_returns_are_passed_by_value() {
        let text = ir(
            "point : struct { x : word, y : word };\n\
             add(p : point, q : point) : point { auto out : point; out.x = p.x + q.x; out }\n\
             main() : word { 0 }",
        );
        assert!(text.contains("@add(%point %0, %point %1)"), "{text}");
        assert!(text.contains("define %point @add("), "{text}");
    }

    #[test]
    fn struct_whole_value_assignment_between_same_type_copy() {
        let text = ir(
            "point : struct { x : word };\n\
             main() : word { auto a : point; auto b : point; a = b; 0 }",
        );
        assert!(text.contains("load %point"), "{text}");
        assert!(text.contains("store %point"), "{text}");
    }

    #[test]
    fn struct_globals_can_be_read_and_written() {
        let text = ir(
            "point : struct { x : word };\n\
             origin : point;\n\
             main() : word { origin.x = 1; origin.x }",
        );
        assert!(text.contains("@origin = global %point zeroinitializer"), "{text}");
        assert!(text.contains("store i64 1"), "{text}");
    }

    fn parse_err(source: &str) -> String {
        parse(tokenize(source).expect("tokenizes")).unwrap_err()
    }

    #[test]
    fn a_struct_field_cannot_reference_a_later_struct() {
        let error = parse_err(
            "outer : struct { inner : inner };\ninner : struct { n : word };\nmain() : word { 0 }",
        );
        assert!(error.contains("unknown type `inner`"), "{error}");
    }

    #[test]
    fn assigning_a_struct_to_a_field_of_another_struct_is_supported() {
        let text = ir(
            "inner : struct { n : word };\n\
             outer : struct { inner : inner, m : word };\n\
             main() : word { auto s : outer; s.inner.n = 3; s.inner.n + 41 }",
        );
        assert!(text.contains("add i64"), "{text}");
    }

    #[test]
    fn a_struct_cannot_be_combined_with_an_integer() {
        let error = ir_err(
            "point : struct { x : word };\nmain() : word { auto p : point; p }",
        );
        assert!(error.contains("cannot implicitly convert `point` to `word`"), "{error}");
    }

    #[test]
    fn arithmetic_on_a_struct_is_rejected() {
        // The struct operand reaches the binary-operator check when the whole
        // expression has no expected type (a condition position).
        let error = ir_err(
            "point : struct { x : word };\nmain() : word { auto p : point; auto q : point; if p + q { 1 } else { 0 }; 0 }",
        );
        assert!(error.contains("cannot be applied to a struct value"), "{error}");
    }

    #[test]
    fn a_struct_cannot_be_an_if_condition() {
        let error = ir_err(
            "point : struct { x : word };\nf(p : point) : word if p { 1 } else { 2 };",
        );
        assert!(error.contains("cannot use `point` as a condition"), "{error}");
    }

    #[test]
    fn struct_conditions_through_fields_work() {
        let text = ir(
            "point : struct { x : word };\n\
             f(p : point) : word if p.x { 1 } else { 2 };",
        );
        assert!(text.contains("icmp ne"), "{text}");
    }

    #[test]
    fn zero_operand_struct_field_of_same_type_assigns() {
        let text = ir(
            "point : struct { x : word, y : word };\n\
             q(p : point) : word { p.x = p.y; p.x }",
        );
        assert!(text.contains("store i64"), "{text}");
    }

    #[test]
    fn rejects_assigning_to_a_non_struct_field() {
        let error = ir_err(
            "point : struct { x : word };\nf(p : point) : word { p.nope = 1; 3 }",
        );
        assert!(error.contains("struct `point` has no field `nope`"), "{error}");
    }

    #[test]
    fn rejects_field_access_on_a_primitive() {
        let error = ir_err("f(x : word) : word { x.nope }");
        assert!(error.contains("`word` is not a struct type"), "{error}");
    }

    #[test]
    fn print_cannot_be_named_as_a_struct() {
        let error = parse_err("print : struct { x : word };\nmain() : word { 0 }");
        assert!(error.contains("reserved"), "{error}");
    }

    #[test]
    fn extern_signatures_cannot_mention_struct_types() {
        let error = ir_err(
            "point : struct { x : word };\nextern tick(point) : word;\nmain() : word { 0 }",
        );
        assert!(error.contains("`extern` signatures cannot use struct types"), "{error}");
    }

    #[test]
    fn main_cannot_return_a_struct() {
        let error = ir_err("point : struct { x : word };\nmain() : point { auto p : point; p }");
        assert!(error.contains("`main` cannot return a struct type"), "{error}");
    }

    #[test]
    fn duplicate_struct_definitions_are_rejected() {
        let error = parse_err(
            "point : struct { x : word };\npoint : struct { y : word };\nmain() : word { 0 }",
        );
        assert!(error.contains("defined more than once"), "{error}");
    }
}
