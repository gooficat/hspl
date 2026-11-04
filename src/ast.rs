//! Abstract syntax tree for HSPL.
//!
//! HSPL has eight primitive types. Annotations are written after names:
//! `function() : t`, `parameter : t`, `local : t`, `global : t`. A missing
//! value-type annotation means [`Type::Word`]; a missing return-type
//! annotation means [`Type::Void`]. `void` is only a return type.
//!
//! There are no operators. The only expressions are literals (integer and
//! floating-point), variables, calls, inline assembly blocks, Rust-style
//! statement expressions (blocks), and conditional expressions whose branches
//! are expressions.

/// A primitive type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Type {
    /// No value. The default return type; usable only as a function or
    /// `extern` return type.
    Void,
    /// 64-bit integer; the default for value annotations.
    Word,
    /// 32-bit integer.
    Int,
    /// 16-bit integer.
    Short,
    /// 8-bit integer.
    Byte,
    /// Boolean, represented as `i1` in LLVM.
    Bool,
    /// 32-bit IEEE-754 floating-point value.
    Float,
    /// 64-bit IEEE-754 floating-point value.
    Double,
}

impl Type {
    /// The source spelling of this type.
    pub fn name(self) -> &'static str {
        match self {
            Type::Void => "void",
            Type::Word => "word",
            Type::Int => "int",
            Type::Short => "short",
            Type::Byte => "byte",
            Type::Bool => "bool",
            Type::Float => "float",
            Type::Double => "double",
        }
    }

    /// Look up a type by its source spelling.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "void" => Some(Type::Void),
            "word" => Some(Type::Word),
            "int" => Some(Type::Int),
            "short" => Some(Type::Short),
            "byte" => Some(Type::Byte),
            "bool" => Some(Type::Bool),
            "float" => Some(Type::Float),
            "double" => Some(Type::Double),
            _ => None,
        }
    }

    /// The width of this type in bits. `void` has no representation, so it is 0.
    pub fn bits(self) -> u32 {
        match self {
            Type::Void => 0,
            Type::Word => 64,
            Type::Int => 32,
            Type::Short => 16,
            Type::Byte => 8,
            Type::Bool => 1,
            Type::Float => 32,
            Type::Double => 64,
        }
    }

    /// The number of mantissa bits of the floating-point types; `0` otherwise.
    /// Every integer of at most this many bits is exactly representable.
    pub fn mantissa_bits(self) -> u32 {
        match self {
            Type::Float => 24,
            Type::Double => 53,
            _ => 0,
        }
    }

    /// Whether this is the void type.
    pub fn is_void(self) -> bool {
        matches!(self, Type::Void)
    }

    /// Whether this is the boolean type.
    pub fn is_bool(self) -> bool {
        matches!(self, Type::Bool)
    }

    /// Whether this is a floating-point type.
    pub fn is_float(self) -> bool {
        matches!(self, Type::Float | Type::Double)
    }

    /// Whether a value of this type can be implicitly widened to `target`.
    ///
    /// Widening is kind-aware. Integers widen strictly by width: `bool`
    /// zero-extends, and every other integer widens with a sign-extension.
    /// Floating-point types widen strictly by width (`float` → `double`).
    /// An integer widens to a floating-point type exactly when every value of
    /// the integer type is representable (at most `mantissa_bits` bits): the
    /// narrower integers widen to `float`, and up to 32 bits widen to
    /// `double`. A floating-point value never widens to an integer type, and
    /// nothing narrows to anything implicitly.
    pub fn can_widen_to(self, target: Type) -> bool {
        if self.is_void() || target.is_void() {
            return false;
        }
        match (self.is_float(), target.is_float()) {
            (true, true) => self.bits() < target.bits(),
            (true, false) => false,
            (false, true) => target.mantissa_bits() >= self.bits(),
            (false, false) => self.bits() < target.bits(),
        }
    }
}

/// A name introduced with a type annotation: a parameter or a local.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub name: String,
    pub ty: Type,
}

/// A literal initializer: `42`, `true`, `false`, or a floating-point literal
/// such as `1.5`. Used for global definitions; [`Expr`] has its own literal
/// variants.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Literal {
    /// An integer literal, or `true`/`false` stored as `1`/`0`.
    Int(i64),
    /// A floating-point literal.
    Float(f64),
}

/// A whole translation unit.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Program {
    pub items: Vec<Item>,
}

/// A top-level item.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// `extern print(word) : word;` — a symbol defined outside this unit.
    Extern(ExternDecl),
    Global(Global),
    Function(Function),
}

/// An imported function's signature. Parameter names are not part of an extern
/// declaration, only their types.
#[derive(Debug, Clone, PartialEq)]
pub struct ExternDecl {
    pub name: String,
    pub params: Vec<Type>,
    pub ret: Type,
}

/// A global definition: `name : type 42;`, `name : type;`, or `name 42;`.
#[derive(Debug, Clone, PartialEq)]
pub struct Global {
    pub name: String,
    pub ty: Type,
    /// Initializer as written. It is converted and range-checked against `ty`
    /// during codegen and defaults to zero when omitted.
    pub init: Literal,
}

/// A function definition.
///
/// The body is exactly one statement; the function's result is that statement's
/// value. There are no `return` statements.
#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    pub name: String,
    pub params: Vec<Binding>,
    pub ret: Type,
    pub body: Stmt,
}

/// A statement. Every statement produces a value, so blocks can yield values.
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// `auto a : int, b;` — locals are zero-initialised.
    Auto(Vec<Binding>),
    /// `name = expr;` — assignment is a statement, not an operator expression.
    Assign(String, Expr),
    /// An expression evaluated as a statement.
    Expr(Expr),
    /// `jump name;` — unconditional transfer to a label in this function.
    Jump(String),
    /// `name:` or `label name;` — a jump target.
    Label(String),
    /// A stray `;`.
    Empty,
}

/// A Rust-style statement expression.
///
/// The value of the block is the value of its trailing expression, or zero of
/// the expected type when there is no trailing expression.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub tail: Option<Box<Expr>>,
}

/// An expression. Note the complete absence of operator nodes.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// An integer literal; its type is taken from context and range-checked.
    Int(i64),
    /// A floating-point literal; its type is taken from context (`float` or
    /// `double`, defaulting to `double`) and range-checked.
    Float(f64),
    /// `true` or `false`.
    Bool(bool),
    Var(String),
    Call(String, Vec<Expr>),
    /// Raw assembly body from an `asm { ... }` block.
    Asm(String),
    /// A nested statement expression.
    Block(Block),
    /// `if condition expression else expression` — an expression yielding the
    /// value of the taken branch. Braces are optional because `{ ... }` is
    /// itself a statement expression. Without `else`, the false path yields
    /// zero.
    If {
        cond: Box<Expr>,
        then_branch: Box<Expr>,
        else_branch: Option<Box<Expr>>,
    },
}
