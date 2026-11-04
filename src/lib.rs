//! HSPL: Hasan's Simple Programming Language.
//!
//! A small B-like language that compiles to LLVM IR. Values have one of six
//! types: five value types (`word`, `int`, `short`, `byte`, `bool`) plus
//! `void`, which is only valid as a function or `extern` return type and is
//! the default when no return type is given. Types are
//! written after names as in `f(x : int) : byte`. There are no operators and no
//! `return` statements: every function body is a single statement whose value
//! is the function's result. Control flow is `if` expressions plus `jump` and
//! labels. Inline assembly blocks provide the escape hatch for real machine
//! operations, and `extern` imports typed symbols such as the standard library's
//! `print`.
//!
//! ```text
//! source -> lexer -> parser -> AST -> codegen -> LLVM IR -> (clang) -> native binary
//! ```
//!
//! ```
//! use hspl::compile_to_ir;
//!
//! let ir = compile_to_ir("main() : word 0;").unwrap();
//! assert!(ir.contains("define i32 @main()"));
//! ```

pub mod ast;
pub mod codegen;
pub mod lexer;
pub mod parser;

use ast::Program;

/// Parse source text into an AST.
pub fn parse_source(source: &str) -> Result<Program, String> {
    let tokens = lexer::tokenize(source)?;
    parser::parse(tokens)
}

/// Compile source text to textual LLVM IR.
pub fn compile_to_ir(source: &str) -> Result<String, String> {
    compile_to_ir_named(source, "hspl")
}

/// Compile source text to textual LLVM IR, using `module_name` as the module id.
pub fn compile_to_ir_named(source: &str, module_name: &str) -> Result<String, String> {
    let program = parse_source(source)?;
    codegen::generate(&program, module_name)
}
