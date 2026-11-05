//! Abstract syntax tree for HSPL.
//!
//! Programs have nine types: eight primitive types plus named struct types.
//! Annotations are written after names:
//! `function() : t`, `parameter : t`, `local : t`, `global : t`. A missing
//! value-type annotation means [`Type::Word`]; a missing return-type
//! annotation means [`Type::Void`]. `void` is only a return type.
//!
//! Structs are named, never anonymous, never declared inline, and there are
//! no generics or `impl` blocks. Structs are declared at the top level:
//! `point : struct { x : word, y : word };`. Values of a struct type are
//! created by declaring a variable or global of that type (zero-initialised)
//! and then assigning fields with dotted paths (`p.x = 5;`). Field access uses
//! postfix `.` (`p.x`). There are no operators yet for everything that is not
//! an arithmetic, bitwise, or reference operation: comparisons, logical
//! operators, and casts are still to come. Arithmetic (`+ - * / %`), bitwise
//! (`& | ^ ~ << >>`) and reference (`&` address-of, `*`) operations are
//! expressions.

/// A primitive type, or a named struct type with typed fields.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    /// A named struct type with typed fields, such as `point` with fields
    /// `x : word` and `y : word`. Structs are named and never anonymous; there
    /// are no generics and no `impl` blocks.
    Struct {
        /// The name of the struct type.
        name: String,
        /// The fields, in order, each with its own type.
        fields: Vec<(String, Type)>,
    },
}

impl Type {
    /// The source spelling of this type. Struct types keep their name.
    pub fn name(&self) -> String {
        match self {
            Type::Void => "void".to_string(),
            Type::Word => "word".to_string(),
            Type::Int => "int".to_string(),
            Type::Short => "short".to_string(),
            Type::Byte => "byte".to_string(),
            Type::Bool => "bool".to_string(),
            Type::Float => "float".to_string(),
            Type::Double => "double".to_string(),
            Type::Struct { name, .. } => name.clone(),
        }
    }

    /// Look up a primitive type by its source spelling. Struct types are
    /// resolved by the parser from its registry of declared structs; a bare
    /// `struct { ... }` is never a type (structs are never anonymous).
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
    /// Struct types are aggregates, so they are 0 as well.
    pub fn bits(&self) -> u32 {
        match self {
            Type::Void | Type::Struct { .. } => 0,
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
    pub fn mantissa_bits(&self) -> u32 {
        match self {
            Type::Float => 24,
            Type::Double => 53,
            _ => 0,
        }
    }

    /// Whether this is the void type.
    pub fn is_void(&self) -> bool {
        matches!(self, Type::Void)
    }

    /// Whether this is the boolean type.
    pub fn is_bool(&self) -> bool {
        matches!(self, Type::Bool)
    }

    /// Whether this is a floating-point type.
    pub fn is_float(&self) -> bool {
        matches!(self, Type::Float | Type::Double)
    }

    /// Whether this is a named struct type.
    pub fn is_struct(&self) -> bool {
        matches!(self, Type::Struct { .. })
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
    /// nothing narrows to anything implicitly. Struct values never widen.
    pub fn can_widen_to(&self, target: &Type) -> bool {
        if self.is_void() || target.is_void() || self.is_struct() || target.is_struct() {
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

/// A unary operation: negation, bitwise not, address-of, or dereference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// Unary minus (`-x`).
    Neg,
    /// Unary bitwise not (`~x`).
    BitNot,
    /// Address-of (`&x`), which yields a `word` holding the address.
    Ref,
    /// Dereference (`*x`), which loads through an address held in a `word`.
    Deref,
}

/// A binary operation: an arithmetic or bitwise combination of two operands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// Addition (`+`).
    Add,
    /// Subtraction (`-`).
    Sub,
    /// Multiplication (`*`).
    Mul,
    /// Division (`/`).
    Div,
    /// Remainder (`%`).
    Rem,
    /// Bitwise AND (`&`).
    And,
    /// Bitwise OR (`|`).
    Or,
    /// Bitwise XOR (`^`).
    Xor,
    /// Left shift (`<<`).
    Shl,
    /// Right shift (`>>`).
    Shr,
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
    /// `point : struct { x : word, y : word };` — a struct type declaration.
    /// Structs are named and never anonymous.
    StructDecl(StructDecl),
}

/// A struct type declaration: a name and its fields.
#[derive(Debug, Clone, PartialEq)]
pub struct StructDecl {
    pub name: String,
    pub fields: Vec<Binding>,
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

/// An expression. Operator nodes wrap operands: [`Expr::Unary`] for prefix
/// operations (negate, bitwise not, address-of, dereference) and
/// [`Expr::Binary`] for infix operations (arithmetic and bitwise combinations).
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
    /// `point.x` — access to the field `x` of the value in `base`. The base
    /// must be a variable or another field access (an addressable value).
    Field {
        base: Box<Expr>,
        field: String,
    },
    /// Raw assembly body from an `asm { ... }` block.
    Asm(String),
    /// A unary operation such as negation, bitwise not, address-of, or
    /// dereference.
    Unary {
        op: UnaryOp,
        operand: Box<Expr>,
    },
    /// A binary operation such as an arithmetic or bitwise combination.
    Binary {
        op: BinaryOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
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
