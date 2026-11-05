//! Recursive-descent parser for HSPL.
//!
//! Grammar (informal):
//!
//! ```text
//! program   := item*
//! item      := 'extern' ident '(' [type (',' type)*] ')' [':' type] ';'  // import
//!            | ident '(' [binding (',' binding)*] ')' [':' type] statement
//!            | ident ':' type [ ('struct' | integer | 'true' | 'false') ]
//!            | ident [':' type] [integer | 'true' | 'false'] ';'      // global
//!            | ident ':' 'struct' '{' [ident ':' type (',' ident ':' type)*] '}' ';'
//!
//! binding   := ident [':' type]
//!
//! statement := 'auto' binding (',' binding)* ';'
//!            | path '=' expression ';'            // path is ident ('.' ident)*
//!            | 'jump' ident ';'                   // goto
//!            | ident ':'                          // label
//!            | 'label' ident [':'] ';'            // label, keyword form
//!            | expression ';'
//!            | expression                          // block-like, no ';'
//!            | ';'
//!
//! expression := integer | float
//!             | 'true' | 'false'
//!             | ident
//!             | ident '(' [expression (',' expression)*] ')'
//!             | asm '{' raw assembly '}'
//!             | 'if' ['('] expression [')'] expression ['else' expression]
//!             | '{' statement* [expression] '}'              // statement expression
//!             | unary_op expression
//!             | expression binary_op expression
//!             | expression '.' ident                         // field access
//! ```
//!
//! unary_op  := '-' | '~' | '&' | '*'
//! binary_op := '|' | '^' | '&' | '<<' | '>>' | '+' | '-' | '*' | '/' | '%'
//!
//! Precedence, loosest to tightest: `|` < `^` < `&` < shifts < `+`/`-`
//! < `*`/`/`/`%` < unary. Binary operators are left-associative.
//!
//! A missing value-type annotation (parameters, locals, globals) means
//! `word`; a missing return-type annotation means `void`. There are no
//! comparison or logical operators yet; arithmetic, bitwise, and reference
//! operations are expressions.

use crate::ast::{
    Binding, Block, BinaryOp, ExternDecl, Expr, Function, Global, Item, Literal, Program, Stmt,
    StructDecl, Type, UnaryOp,
};
use crate::lexer::{Token, TokenKind};
use std::collections::HashMap;

/// Parse a token stream into a [`Program`].
pub fn parse(tokens: Vec<Token>) -> Result<Program, String> {
    Parser::new(tokens).program()
}

/// Result of parsing inside a block: a normal statement or the trailing
/// (value-producing) expression of a statement expression.
enum Parsed {
    Stmt(Stmt),
    Tail(Expr),
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// Struct declarations seen so far in this file, keyed by name. A struct
    /// type may only be referenced with a declared name; bodies are never
    /// written inline.
    structs: HashMap<String, Vec<(String, Type)>>,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            pos: 0,
            structs: HashMap::new(),
        }
    }

    fn program(mut self) -> Result<Program, String> {
        let mut items = Vec::new();
        while !self.check(&TokenKind::Eof) {
            items.push(self.item()?);
        }
        Ok(Program { items })
    }

    // ----- token helpers -------------------------------------------------

    /// Whether the upcoming tokens look like an assignment path:
    /// `ident ('.' ident)* '='`.
    fn at_assignment_path(&self) -> bool {
        let mut index = self.pos;
        if !matches!(self.tokens.get(index).map(|token| &token.kind), Some(TokenKind::Ident(_))) {
            return false;
        }
        index += 1;
        loop {
            match self.tokens.get(index).map(|token| &token.kind) {
                Some(TokenKind::Dot) => {
                    index += 1;
                    match self.tokens.get(index).map(|token| &token.kind) {
                        Some(TokenKind::Ident(_)) => index += 1,
                        _ => return false,
                    }
                }
                Some(TokenKind::Assign) => return true,
                _ => return false,
            }
        }
    }

    /// Parse `ident ('.' ident)*` and return the components joined with `.`.
    fn assignment_path(&mut self) -> Result<String, String> {
        let mut path = self.ident("in an assignment")?;
        while self.eat(&TokenKind::Dot) {
            let field = self.ident("after `.`")?;
            path.push('.');
            path.push_str(&field);
        }
        Ok(path)
    }

    fn peek(&self) -> &TokenKind {
        &self.tokens[self.pos].kind
    }

    fn peek_at(&self, offset: usize) -> &TokenKind {
        let index = (self.pos + offset).min(self.tokens.len() - 1);
        &self.tokens[index].kind
    }

    fn line(&self) -> usize {
        self.tokens[self.pos].line
    }

    fn check(&self, kind: &TokenKind) -> bool {
        std::mem::discriminant(self.peek()) == std::mem::discriminant(kind)
    }

    fn advance(&mut self) -> TokenKind {
        let kind = self.tokens[self.pos].kind.clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        kind
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.check(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: &TokenKind, context: &str) -> Result<(), String> {
        if self.eat(kind) {
            Ok(())
        } else {
            Err(format!(
                "line {}: expected {kind} {context}, found {}",
                self.line(),
                self.peek()
            ))
        }
    }

    fn ident(&mut self, context: &str) -> Result<String, String> {
        match self.peek().clone() {
            TokenKind::Ident(name) => {
                self.advance();
                Ok(name)
            }
            other => Err(format!(
                "line {}: expected an identifier {context}, found {other}",
                self.line()
            )),
        }
    }

    // ----- types ---------------------------------------------------------

    fn ty(&mut self, context: &str) -> Result<Type, String> {
        let line = self.line();
        let name = self.ident(context)?;
        if let Some(ty) = Type::from_name(&name) {
            return Ok(ty);
        }
        if name == "struct" {
            return Err(format!(
                "line {line}: `struct` is not a type on its own; structs are declared at the top level as `name : struct {{ ... }};`"
            ));
        }
        if let Some(fields) = self.structs.get(&name) {
            return Ok(Type::Struct { name, fields: fields.clone() });
        }
        Err(format!(
            "line {line}: unknown type `{name}` (expected word, int, short, byte, bool, float, double, void, or a declared struct)"
        ))
    }

    /// Parse an optional `: type` in a value position (parameters, locals,
    /// globals); a missing annotation means `word`.
    fn optional_type(&mut self) -> Result<Type, String> {
        if self.eat(&TokenKind::Colon) {
            self.ty("after `:`")
        } else {
            Ok(Type::Word)
        }
    }

    /// Parse an optional `: type` in a return position (function or `extern`
    /// return type); a missing annotation means `void`.
    fn optional_return_type(&mut self) -> Result<Type, String> {
        if self.eat(&TokenKind::Colon) {
            self.ty("after `:`")
        } else {
            Ok(Type::Void)
        }
    }

    fn binding(&mut self, context: &str) -> Result<Binding, String> {
        let name = self.ident(context)?;
        let ty = self.optional_type()?;
        Ok(Binding { name, ty })
    }

    // ----- top-level items -----------------------------------------------

    fn item(&mut self) -> Result<Item, String> {
        if self.eat(&TokenKind::Extern) {
            let name = self.ident("after `extern`")?;
            self.expect(&TokenKind::LParen, "to open the extern parameter list")?;

            let mut params = Vec::new();
            if !self.check(&TokenKind::RParen) {
                loop {
                    params.push(self.ty("for an extern parameter")?);
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
            }
            self.expect(&TokenKind::RParen, "to close the extern parameter list")?;

            let ret = self.optional_return_type()?;
            self.expect(&TokenKind::Semi, "after an `extern` declaration")?;
            return Ok(Item::Extern(ExternDecl { name, params, ret }));
        }

        let name = self.ident("at the top level")?;

        if self.eat(&TokenKind::LParen) {
            let mut params = Vec::new();
            if !self.check(&TokenKind::RParen) {
                loop {
                    params.push(self.binding("for a parameter name")?);
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
            }
            self.expect(&TokenKind::RParen, "to close the parameter list")?;

            let ret = self.optional_return_type()?;

            // Exactly one statement, which may itself be a statement expression.
            let body = self.stmt()?;
            Ok(Item::Function(Function {
                name,
                params,
                ret,
                body,
            }))
        } else {
            // `name : struct { field : type, ... };` — a struct type
            // declaration. Structs are named, never anonymous, and never
            // declared inline.
            if self.check(&TokenKind::Colon)
                && matches!(self.peek_at(1), TokenKind::Ident(name) if name == "struct")
            {
                self.advance(); // `:`
                self.advance(); // `struct`
                self.expect(&TokenKind::LBrace, "to open the struct body")?;

                let mut fields = Vec::new();
                let mut seen = std::collections::HashSet::new();
                if !self.check(&TokenKind::RBrace) {
                    loop {
                        let field = self.binding("for a field name")?;
                        if !seen.insert(field.name.clone()) {
                            return Err(format!(
                                "line {}: struct `{name}` has a duplicate field `{}`",
                                self.line(),
                                field.name
                            ));
                        }
                        if field.ty.is_void() {
                            return Err(format!(
                                "line {}: field `{}` of struct `{name}` cannot have type `void`",
                                self.line(),
                                field.name
                            ));
                        }
                        fields.push(field);
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                        // A trailing comma before `}` is allowed.
                        if self.check(&TokenKind::RBrace) {
                            break;
                        }
                    }
                }
                self.expect(&TokenKind::RBrace, "to close the struct body")?;

                if fields.is_empty() {
                    return Err(format!(
                        "line {}: struct `{name}` has no fields",
                        self.line()
                    ));
                }
                if Type::from_name(&name).is_some() || name == "print" {
                    return Err(format!(
                        "line {}: struct `{name}` cannot use a reserved or builtin type name",
                        self.line()
                    ));
                }
                if self.structs.contains_key(&name) {
                    return Err(format!(
                        "line {}: struct `{name}` is defined more than once",
                        self.line()
                    ));
                }

                self.expect(&TokenKind::Semi, "after the struct definition")?;

                let field_types: Vec<(String, Type)> = fields
                    .iter()
                    .map(|field| (field.name.clone(), field.ty.clone()))
                    .collect();
                self.structs.insert(name.clone(), field_types);
                return Ok(Item::StructDecl(StructDecl { name, fields }));
            }

            let ty = self.optional_type()?;
            let init = match self.peek().clone() {
                TokenKind::Int(value) => {
                    self.advance();
                    Literal::Int(value)
                }
                TokenKind::Float(value) => {
                    self.advance();
                    Literal::Float(value)
                }
                TokenKind::True => {
                    self.advance();
                    Literal::Int(1)
                }
                TokenKind::False => {
                    self.advance();
                    Literal::Int(0)
                }
                _ => Literal::Int(0),
            };
            self.expect(&TokenKind::Semi, "after the global definition")?;
            Ok(Item::Global(Global { name, ty, init }))
        }
    }

    // ----- statements ----------------------------------------------------

    fn stmt(&mut self) -> Result<Stmt, String> {
        match self.stmt_or_tail()? {
            Parsed::Stmt(stmt) => Ok(stmt),
            // A function body is a statement; a bare trailing expression is
            // accepted as an expression statement.
            Parsed::Tail(expr) => Ok(Stmt::Expr(expr)),
        }
    }

    fn stmt_or_tail(&mut self) -> Result<Parsed, String> {
        match self.peek() {
            TokenKind::Auto => {
                self.advance();
                let mut bindings = vec![self.binding("after `auto`")?];
                while self.eat(&TokenKind::Comma) {
                    bindings.push(self.binding("after `,`")?);
                }
                self.expect(&TokenKind::Semi, "after an `auto` declaration")?;
                Ok(Parsed::Stmt(Stmt::Auto(bindings)))
            }
            TokenKind::Semi => {
                self.advance();
                Ok(Parsed::Stmt(Stmt::Empty))
            }
            TokenKind::Jump => {
                self.advance();
                let name = self.ident("after `jump`")?;
                self.expect(&TokenKind::Semi, "after a `jump` target")?;
                Ok(Parsed::Stmt(Stmt::Jump(name)))
            }
            // `label done;` or `label done:`.
            TokenKind::Label => {
                self.advance();
                let name = self.ident("after `label`")?;
                if !self.eat(&TokenKind::Colon) {
                    self.expect(&TokenKind::Semi, "after a label")?;
                }
                Ok(Parsed::Stmt(Stmt::Label(name)))
            }
            // `done:` — a label written without the keyword.
            TokenKind::Ident(_) if matches!(self.peek_at(1), TokenKind::Colon) => {
                let name = self.ident("for a label")?;
                self.expect(&TokenKind::Colon, "after a label name")?;
                Ok(Parsed::Stmt(Stmt::Label(name)))
            }
            // `path = expression;` — assignment, where the path is one or
            // more identifiers separated by `.` (a variable or a chain of
            // struct fields, e.g. `p.x = 5;`).
            TokenKind::Ident(_) if self.at_assignment_path() => {
                let path = self.assignment_path()?;
                self.expect(&TokenKind::Assign, "in an assignment")?;
                let value = self.expr()?;
                self.expect(&TokenKind::Semi, "after the assigned value")?;
                Ok(Parsed::Stmt(Stmt::Assign(path, value)))
            }
            _ => {
                let expr = self.expr()?;

                if self.eat(&TokenKind::Semi) {
                    Ok(Parsed::Stmt(Stmt::Expr(expr)))
                } else if self.check(&TokenKind::RBrace) {
                    // Trailing expression: this block yields its value.
                    Ok(Parsed::Tail(expr))
                } else if is_block_like(&expr) {
                    // Blocks and `asm` blocks are statements without a `;`.
                    Ok(Parsed::Stmt(Stmt::Expr(expr)))
                } else {
                    Err(format!(
                        "line {}: expected `;` or `}}`, found {}",
                        self.line(),
                        self.peek()
                    ))
                }
            }
        }
    }

    fn block(&mut self) -> Result<Block, String> {
        self.expect(&TokenKind::LBrace, "to open a block")?;

        let mut stmts = Vec::new();
        let mut tail = None;

        while !self.check(&TokenKind::RBrace) && !self.check(&TokenKind::Eof) {
            match self.stmt_or_tail()? {
                Parsed::Stmt(stmt) => stmts.push(stmt),
                Parsed::Tail(expr) => {
                    tail = Some(Box::new(expr));
                    break;
                }
            }
        }

        self.expect(&TokenKind::RBrace, "to close the block")?;
        Ok(Block { stmts, tail })
    }

    // ----- expressions ---------------------------------------------------

    /// Parse a full expression: a sequence of binary operations, resolved by
    /// precedence climbing (the loosest level first).
    fn expr(&mut self) -> Result<Expr, String> {
        self.binary_expr(0)
    }

    /// Parse an expression of at least `min_precedence`. Precedence climbing:
    /// operands looser than the running minimum terminate the loop, which is
    /// what builds the precedence table without a flat list of levels.
    fn binary_expr(&mut self, min_precedence: u8) -> Result<Expr, String> {
        let mut lhs = self.unary_expr()?;

        while let Some((precedence, op)) = self.binary_operator()? {
            if precedence < min_precedence {
                break;
            }
            self.advance();
            // Left-associative: the right operand parses at `precedence + 1`.
            let rhs = self.binary_expr(precedence + 1)?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }

        Ok(lhs)
    }

    /// The next infix operator, if any, with its precedence (lower binds
    /// looser).
    fn binary_operator(&mut self) -> Result<Option<(u8, BinaryOp)>, String> {
        let (precedence, op) = match self.peek().clone() {
            TokenKind::Pipe => (1, BinaryOp::Or),
            TokenKind::Caret => (2, BinaryOp::Xor),
            TokenKind::Amp => (3, BinaryOp::And),
            TokenKind::LShift => (4, BinaryOp::Shl),
            TokenKind::RShift => (4, BinaryOp::Shr),
            TokenKind::Plus => (5, BinaryOp::Add),
            TokenKind::Minus => (5, BinaryOp::Sub),
            TokenKind::Star => (6, BinaryOp::Mul),
            TokenKind::Slash => (6, BinaryOp::Div),
            TokenKind::Percent => (6, BinaryOp::Rem),
            _ => return Ok(None),
        };
        Ok(Some((precedence, op)))
    }

    /// Parse a prefix (unary) expression: `-`, `~`, `&` or `*`, applied to
    /// the unary expression that follows.
    fn unary_expr(&mut self) -> Result<Expr, String> {
        match self.peek().clone() {
            TokenKind::Minus => {
                self.advance();
                let operand = self.unary_expr()?;
                Ok(Expr::Unary {
                    op: UnaryOp::Neg,
                    operand: Box::new(operand),
                })
            }
            TokenKind::Tilde => {
                self.advance();
                let operand = self.unary_expr()?;
                Ok(Expr::Unary {
                    op: UnaryOp::BitNot,
                    operand: Box::new(operand),
                })
            }
            TokenKind::Amp => {
                self.advance();
                let operand = self.unary_expr()?;
                Ok(Expr::Unary {
                    op: UnaryOp::Ref,
                    operand: Box::new(operand),
                })
            }
            TokenKind::Star => {
                self.advance();
                let operand = self.unary_expr()?;
                Ok(Expr::Unary {
                    op: UnaryOp::Deref,
                    operand: Box::new(operand),
                })
            }
            _ => self.postfix(),
        }
    }

    /// Parse a primary expression followed by zero or more `.field` suffixes,
    /// e.g. `p.x`, `p.inner.y`, or `f().x`.
    fn postfix(&mut self) -> Result<Expr, String> {
        let mut expr = self.primary()?;
        while self.eat(&TokenKind::Dot) {
            let field = self.ident("after `.`")?;
            expr = Expr::Field {
                base: Box::new(expr),
                field,
            };
        }
        Ok(expr)
    }

    fn primary(&mut self) -> Result<Expr, String> {
        match self.peek().clone() {
            TokenKind::Int(value) => {
                self.advance();
                Ok(Expr::Int(value))
            }
            TokenKind::Float(value) => {
                self.advance();
                Ok(Expr::Float(value))
            }
            TokenKind::True => {
                self.advance();
                Ok(Expr::Bool(true))
            }
            TokenKind::False => {
                self.advance();
                Ok(Expr::Bool(false))
            }
            TokenKind::Asm(body) => {
                self.advance();
                Ok(Expr::Asm(body))
            }
            TokenKind::If => self.if_expr(),
            TokenKind::Ident(name) => {
                self.advance();
                if self.eat(&TokenKind::LParen) {
                    let mut args = Vec::new();
                    if !self.check(&TokenKind::RParen) {
                        loop {
                            args.push(self.expr()?);
                            if !self.eat(&TokenKind::Comma) {
                                break;
                            }
                        }
                    }
                    self.expect(&TokenKind::RParen, "to close the argument list")?;
                    Ok(Expr::Call(name, args))
                } else {
                    Ok(Expr::Var(name))
                }
            }
            TokenKind::LBrace => Ok(Expr::Block(self.block()?)),
            other => Err(format!(
                "line {}: expected an expression, found {other}",
                self.line()
            )),
        }
    }

    fn if_expr(&mut self) -> Result<Expr, String> {
        self.expect(&TokenKind::If, "to start a conditional")?;

        // Accept both `if cond ...` and `if (cond) ...`. Only one level of
        // parentheses is used for the condition; the expression language has no
        // general grouping operator.
        let cond = if self.eat(&TokenKind::LParen) {
            let cond = self.expr()?;
            self.expect(&TokenKind::RParen, "after the `if` condition")?;
            cond
        } else {
            self.expr()?
        };

        let then_branch = self.if_branch()?;
        let else_branch = if self.eat(&TokenKind::Else) {
            Some(Box::new(self.if_branch()?))
        } else {
            None
        };

        Ok(Expr::If {
            cond: Box::new(cond),
            then_branch: Box::new(then_branch),
            else_branch,
        })
    }

    /// A branch is an expression; `{ ... }` is just a statement expression, so
    /// braces are optional. A bare `jump label;` is also accepted B-style.
    fn if_branch(&mut self) -> Result<Expr, String> {
        if self.check(&TokenKind::Jump) {
            let stmt = self.stmt()?;
            Ok(Expr::Block(Block {
                stmts: vec![stmt],
                tail: None,
            }))
        } else {
            self.expr()
        }
    }
}

fn is_block_like(expr: &Expr) -> bool {
    matches!(expr, Expr::Block(_) | Expr::Asm(_) | Expr::If { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;

    fn parse_source(source: &str) -> Program {
        parse(tokenize(source).expect("tokenizes")).expect("parses")
    }

    #[test]
    fn parses_a_single_expression_body() {
        let program = parse_source("zero() 0;");
        let Item::Function(function) = &program.items[0] else {
            panic!("expected a function");
        };
        assert_eq!(function.name, "zero");
        assert!(function.params.is_empty());
        assert_eq!(function.ret, Type::Void);
        assert_eq!(function.body, Stmt::Expr(Expr::Int(0)));
    }

    #[test]
    fn parses_type_annotations_after_names() {
        let program = parse_source("f(x : byte, y) : int x;");
        let Item::Function(function) = &program.items[0] else {
            panic!("expected a function");
        };
        assert_eq!(function.ret, Type::Int);
        assert_eq!(
            function.params,
            vec![
                Binding {
                    name: "x".into(),
                    ty: Type::Byte,
                },
                Binding {
                    name: "y".into(),
                    ty: Type::Word,
                },
            ]
        );
    }

    #[test]
    fn parses_extern_signatures() {
        let program = parse_source("extern print(word) : word;\nextern tick() : int;");
        assert_eq!(
            program.items,
            vec![
                Item::Extern(ExternDecl {
                    name: "print".into(),
                    params: vec![Type::Word],
                    ret: Type::Word,
                }),
                Item::Extern(ExternDecl {
                    name: "tick".into(),
                    params: vec![],
                    ret: Type::Int,
                }),
            ]
        );
    }

    #[test]
    fn extern_return_types_default_to_void() {
        let program = parse_source("extern put(byte, bool);");
        let Item::Extern(decl) = &program.items[0] else {
            panic!("expected an extern");
        };
        assert_eq!(decl.params, vec![Type::Byte, Type::Bool]);
        assert_eq!(decl.ret, Type::Void);
    }

    #[test]
    fn function_return_types_default_to_void() {
        let program = parse_source("f(x : byte, y) { }");
        let Item::Function(function) = &program.items[0] else {
            panic!("expected a function");
        };
        // Value annotations still default to `word`; return types default to `void`.
        assert_eq!(function.ret, Type::Void);
        assert_eq!(function.params[1].ty, Type::Word);
    }

    #[test]
    fn parses_asm_as_body_without_semicolon() {
        let program = parse_source("double(x) asm { movq $x, $out }");
        let Item::Function(function) = &program.items[0] else {
            panic!("expected a function");
        };
        assert_eq!(
            function.body,
            Stmt::Expr(Expr::Asm(" movq $x, $out ".into()))
        );
    }

    #[test]
    fn parses_rust_style_statement_expression() {
        let program = parse_source("f() { auto x; x = 7; x }");
        let Item::Function(function) = &program.items[0] else {
            panic!("expected a function");
        };
        let Stmt::Expr(Expr::Block(block)) = &function.body else {
            panic!("expected a block body");
        };
        assert_eq!(block.stmts.len(), 2);
        assert_eq!(block.tail.as_deref(), Some(&Expr::Var("x".into())));
    }

    #[test]
    fn parses_a_statement_expression_as_an_argument() {
        let program = parse_source("main() print({ auto n; n = 1; n });");
        let Item::Function(function) = &program.items[0] else {
            panic!("expected a function");
        };
        let Stmt::Expr(Expr::Call(name, args)) = &function.body else {
            panic!("expected a call body");
        };
        assert_eq!(name, "print");
        assert!(matches!(args[0], Expr::Block(_)));
    }

    #[test]
    fn parses_globals() {
        let program = parse_source("answer 42; zero : byte; counter : int 7; flag : bool true;");
        assert_eq!(
            program.items,
            vec![
                Item::Global(Global {
                    name: "answer".into(),
                    ty: Type::Word,
                    init: Literal::Int(42),
                }),
                Item::Global(Global {
                    name: "zero".into(),
                    ty: Type::Byte,
                    init: Literal::Int(0),
                }),
                Item::Global(Global {
                    name: "counter".into(),
                    ty: Type::Int,
                    init: Literal::Int(7),
                }),
                Item::Global(Global {
                    name: "flag".into(),
                    ty: Type::Bool,
                    init: Literal::Int(1),
                }),
            ]
        );
    }

    #[test]
    fn parses_auto_with_types() {
        let program = parse_source("f() : word { auto a : byte, b; }");
        let Item::Function(function) = &program.items[0] else {
            panic!("expected a function");
        };
        let Stmt::Expr(Expr::Block(block)) = &function.body else {
            panic!("expected a block body");
        };
        assert_eq!(
            block.stmts,
            vec![Stmt::Auto(vec![
                Binding {
                    name: "a".into(),
                    ty: Type::Byte,
                },
                Binding {
                    name: "b".into(),
                    ty: Type::Word,
                },
            ])]
        );
    }

    #[test]
    fn parses_bool_literals() {
        let program = parse_source("yes() : bool true;\nno() : bool false;");
        let bodies: Vec<_> = program
            .items
            .iter()
            .map(|item| {
                let Item::Function(function) = item else {
                    panic!("expected a function");
                };
                function.body.clone()
            })
            .collect();
        assert_eq!(
            bodies,
            vec![Stmt::Expr(Expr::Bool(true)), Stmt::Expr(Expr::Bool(false))]
        );
    }

    #[test]
    fn rejects_unknown_types() {
        let error = parse(tokenize("f(x : quadruple) x;").unwrap()).unwrap_err();
        assert!(error.contains("unknown type `quadruple`"), "{error}");
    }

    #[test]
    fn parses_void_type() {
        // `void` is a recognized type name.
        let program = parse_source("f() : void { }\ng : void;");
        let Item::Function(function) = &program.items[0] else {
            panic!("expected a function");
        };
        assert_eq!(function.ret, Type::Void);
        let Item::Global(global) = &program.items[1] else {
            panic!("expected a global");
        };
        assert_eq!(global.ty, Type::Void);
    }

    #[test]
    fn rejects_missing_semicolon() {
        let error = parse(tokenize("f() { x = 1 }").unwrap()).unwrap_err();
        assert!(error.contains("expected `;`"), "{error}");
    }

    #[test]
    fn function_bodies_are_exactly_one_statement() {
        // The second statement is not part of `f`; it is invalid at top level.
        let error = parse(tokenize("f() 0;\n1;").unwrap()).unwrap_err();
        assert!(error.contains("expected an identifier"), "{error}");
    }

    #[test]
    fn assignment_is_not_an_expression() {
        let error = parse(tokenize("f() print(x = 1);").unwrap()).unwrap_err();
        assert!(error.contains("expected `)`"), "{error}");
    }

    #[test]
    fn rejects_unsupported_keywords() {
        // `return` and `while` are not keywords, so they fail as unparseable
        // statements rather than being silently accepted.
        for source in ["f() return 0;", "f() while x { 1 }"] {
            assert!(
                parse(tokenize(source).unwrap()).is_err(),
                "`{source}` should not parse"
            );
        }
    }

    #[test]
    fn parses_if_expressions() {
        let program = parse_source("choose(flag : bool) : int if flag { 1 } else { 2 };");
        let Item::Function(function) = &program.items[0] else {
            panic!("expected a function");
        };
        let Stmt::Expr(Expr::If {
            cond,
            then_branch,
            else_branch,
        }) = &function.body
        else {
            panic!("expected an if body");
        };
        assert_eq!(**cond, Expr::Var("flag".into()));
        assert!(matches!(**then_branch, Expr::Block(_)));
        assert!(matches!(
            else_branch.as_deref(),
            Some(Expr::Block(_))
        ));
    }

    #[test]
    fn parses_if_with_unbraced_expression_branches() {
        // Braces are optional: `{ ... }` is just a statement expression.
        let program = parse_source("f(flag : bool) : int if flag 1 else 2;");
        let Item::Function(function) = &program.items[0] else {
            panic!("expected a function");
        };
        let Stmt::Expr(Expr::If {
            then_branch,
            else_branch,
            ..
        }) = &function.body
        else {
            panic!("expected an if body");
        };
        assert_eq!(**then_branch, Expr::Int(1));
        assert_eq!(else_branch.as_deref(), Some(&Expr::Int(2)));
    }

    #[test]
    fn parses_if_with_parenthesised_condition_and_no_else() {
        let program = parse_source("f(flag : int) : int if (flag) { 1 };");
        let Item::Function(function) = &program.items[0] else {
            panic!("expected a function");
        };
        let Stmt::Expr(Expr::If {
            cond, else_branch, ..
        }) = &function.body
        else {
            panic!("expected an if body");
        };
        assert_eq!(**cond, Expr::Var("flag".into()));
        assert!(else_branch.is_none());
    }

    #[test]
    fn parses_bare_jump_as_an_if_branch() {
        let program = parse_source("f(flag : bool) : word { if flag jump done; done: 0 }");
        let Item::Function(function) = &program.items[0] else {
            panic!("expected a function");
        };
        let Stmt::Expr(Expr::Block(block)) = &function.body else {
            panic!("expected a block body");
        };
        let Stmt::Expr(Expr::If { then_branch, .. }) = &block.stmts[0] else {
            panic!("expected an if");
        };
        let Expr::Block(then_block) = then_branch.as_ref() else {
            panic!("expected a branch block");
        };
        assert_eq!(then_block.stmts, vec![Stmt::Jump("done".into())]);
    }

    #[test]
    fn parses_jump_and_both_label_spellings() {
        let program = parse_source(
            "f() : word { top: jump top; label done; jump done; label end: jump end; }",
        );
        let Item::Function(function) = &program.items[0] else {
            panic!("expected a function");
        };
        let Stmt::Expr(Expr::Block(block)) = &function.body else {
            panic!("expected a block body");
        };
        assert_eq!(
            block.stmts,
            vec![
                Stmt::Label("top".into()),
                Stmt::Jump("top".into()),
                Stmt::Label("done".into()),
                Stmt::Jump("done".into()),
                Stmt::Label("end".into()),
                Stmt::Jump("end".into()),
            ]
        );
    }

    #[test]
    fn parses_float_literals_and_types() {
        let program = parse_source("f() : double 1.5;");
        let Item::Function(function) = &program.items[0] else {
            panic!("expected a function");
        };
        assert_eq!(function.ret, Type::Double);
        assert_eq!(function.body, Stmt::Expr(Expr::Float(1.5)));
    }

    #[test]
    fn parses_float_globals() {
        let program = parse_source("pi : double 3.14159; plain : float; temp : double 2;");
        assert_eq!(
            program.items,
            vec![
                Item::Global(Global {
                    name: "pi".into(),
                    ty: Type::Double,
                    init: Literal::Float(3.14159),
                }),
                Item::Global(Global {
                    name: "plain".into(),
                    ty: Type::Float,
                    init: Literal::Int(0),
                }),
                Item::Global(Global {
                    name: "temp".into(),
                    ty: Type::Double,
                    init: Literal::Int(2),
                }),
            ]
        );
    }

    #[test]
    fn parses_float_extern_signatures() {
        let program = parse_source("extern convert(float) : double;");
        let Item::Extern(decl) = &program.items[0] else {
            panic!("expected an extern");
        };
        assert_eq!(decl.params, vec![Type::Float]);
        assert_eq!(decl.ret, Type::Double);
    }

    #[test]
    fn labels_can_precede_a_tail_expression() {
        let program = parse_source("f() : int { done: 7 }");
        let Item::Function(function) = &program.items[0] else {
            panic!("expected a function");
        };
        let Stmt::Expr(Expr::Block(block)) = &function.body else {
            panic!("expected a block body");
        };
        assert_eq!(block.stmts, vec![Stmt::Label("done".into())]);
        assert_eq!(block.tail.as_deref(), Some(&Expr::Int(7)));
    }

    #[test]
    fn parses_binary_operations() {
        let program = parse_source("f(a : word, b : word) : word a - b;");
        let Item::Function(function) = &program.items[0] else {
            panic!("expected a function");
        };
        assert_eq!(
            function.body,
            Stmt::Expr(Expr::Binary {
                op: BinaryOp::Sub,
                lhs: Box::new(Expr::Var("a".into())),
                rhs: Box::new(Expr::Var("b".into())),
            })
        );
    }

    #[test]
    fn parses_unary_operations() {
        let program = parse_source("f(x : word) : word -x;");
        let Item::Function(function) = &program.items[0] else {
            panic!("expected a function");
        };
        assert_eq!(
            function.body,
            Stmt::Expr(Expr::Unary {
                op: UnaryOp::Neg,
                operand: Box::new(Expr::Var("x".into())),
            })
        );

        let program = parse_source("f(x : word) : word ~x;");
        let Item::Function(function) = &program.items[0] else {
            panic!("expected a function");
        };
        assert_eq!(
            function.body,
            Stmt::Expr(Expr::Unary {
                op: UnaryOp::BitNot,
                operand: Box::new(Expr::Var("x".into())),
            })
        );
    }

    #[test]
    fn parses_left_associative_binary_operations() {
        // `a - b - c` is `(a - b) - c`.
        let program = parse_source("f(a : word, b : word, c : word) : word a - b - c;");
        let Item::Function(function) = &program.items[0] else {
            panic!("expected a function");
        };
        assert_eq!(
            function.body,
            Stmt::Expr(Expr::Binary {
                op: BinaryOp::Sub,
                lhs: Box::new(Expr::Binary {
                    op: BinaryOp::Sub,
                    lhs: Box::new(Expr::Var("a".into())),
                    rhs: Box::new(Expr::Var("b".into())),
                }),
                rhs: Box::new(Expr::Var("c".into())),
            })
        );
    }

    #[test]
    fn parses_precedence_of_binary_operations() {
        // `a + b * c` is `a + (b * c)`.
        let program = parse_source("f(a : word, b : word, c : word) : word a + b * c;");
        let Item::Function(function) = &program.items[0] else {
            panic!("expected a function");
        };
        assert_eq!(
            function.body,
            Stmt::Expr(Expr::Binary {
                op: BinaryOp::Add,
                lhs: Box::new(Expr::Var("a".into())),
                rhs: Box::new(Expr::Binary {
                    op: BinaryOp::Mul,
                    lhs: Box::new(Expr::Var("b".into())),
                    rhs: Box::new(Expr::Var("c".into())),
                }),
            })
        );
    }
}

#[cfg(test)]
mod struct_tests {
    use super::*;
    use crate::lexer::tokenize;

    fn parse_source(source: &str) -> Program {
        parse(tokenize(source).expect("tokenizes")).expect("parses")
    }

    #[test]
    fn types_have_eight_primitives_plus_named_structs() {
        let program = parse_source("point : struct { x : word, y : word };");
        let Item::StructDecl(decl) = &program.items[0] else {
            panic!("expected a struct declaration");
        };
        assert_eq!(decl.name, "point");
        assert_eq!(
            decl.fields,
            vec![
                Binding { name: "x".into(), ty: Type::Word },
                Binding { name: "y".into(), ty: Type::Word },
            ]
        );
    }

    #[test]
    fn struct_fields_can_be_of_any_declared_type() {
        let program = parse_source(
            "point : struct { x : word, y : word };\n\
             offset : struct { base : point, dx : word, on : bool, temp : float };",
        );
        let Item::StructDecl(decl) = &program.items[1] else {
            panic!("expected a struct declaration");
        };
        assert_eq!(
            decl.fields,
            vec![
                Binding { name: "base".into(), ty: Type::Struct {
                    name: "point".into(),
                    fields: vec![
                        ("x".into(), Type::Word),
                        ("y".into(), Type::Word),
                    ],
                } },
                Binding { name: "dx".into(), ty: Type::Word },
                Binding { name: "on".into(), ty: Type::Bool },
                Binding { name: "temp".into(), ty: Type::Float },
            ]
        );
    }

    #[test]
    fn an_unknown_struct_name_is_rejected() {
        let error = parse(tokenize("f() : point { auto p : point; p }").unwrap()).unwrap_err();
        assert!(error.contains("unknown type `point`"), "{error}");
    }

    #[test]
    fn a_struct_field_cannot_reference_a_later_struct() {
        let error = parse(tokenize(
            "outer : struct { inner : inner };\ninner : struct { n : word };",
        ).unwrap()).unwrap_err();
        assert!(error.contains("unknown type `inner`"), "{error}");
    }

    #[test]
    fn struct_declarations_reject_duplicate_fields() {
        let error = parse(tokenize("point : struct { x : word, x : word };").unwrap()).unwrap_err();
        assert!(error.contains("duplicate field `x`"), "{error}");
    }

    #[test]
    fn struct_declarations_reject_empty_bodies() {
        let error = parse(tokenize("point : struct { };").unwrap()).unwrap_err();
        assert!(error.contains("has no fields"), "{error}");
    }

    #[test]
    fn struct_declarations_reject_void_fields() {
        let error = parse(tokenize("point : struct { x : void };").unwrap()).unwrap_err();
        assert!(error.contains("cannot have type `void`"), "{error}");
    }

    #[test]
    fn struct_declarations_cannot_use_builtin_type_names() {
        let error = parse(tokenize("word : struct { x : word };").unwrap()).unwrap_err();
        assert!(error.contains("builtin type name"), "{error}");
    }

    #[test]
    fn struct_declarations_cannot_be_named_print() {
        let error = parse(tokenize("print : struct { x : word };").unwrap()).unwrap_err();
        assert!(error.contains("standard library") || error.contains("reserved"), "{error}");
    }

    #[test]
    fn duplicate_struct_declarations_are_rejected() {
        let error = parse(tokenize(
            "point : struct { x : word };\npoint : struct { y : word };",
        ).unwrap()).unwrap_err();
        assert!(error.contains("defined more than once"), "{error}");
    }

    #[test]
    fn struct_declarations_are_never_anonymous() {
        // A name is always required for a struct type.
        let error = parse(tokenize("main() : word { auto p : struct { x : word }; 0 }").unwrap()).unwrap_err();
        assert!(error.contains("`struct` is not a type on its own"), "{error}");
    }

    #[test]
    fn field_access_parses_as_nested_field_expressions() {
        let program = parse_source(
            "namespace : struct { dx : word };\n\
             point : struct { inner : namespace, n : word };\n\
             main() : word { auto p : point; p.inner.dx }",
        );
        let Item::Function(function) = &program.items[2] else {
            panic!("expected a function");
        };
        let Stmt::Expr(Expr::Block(block)) = &function.body else {
            panic!("expected a block body");
        };
        let Some(Expr::Field { base, field }) = block.tail.as_deref() else {
            panic!("expected a field access, found {block:?}");
        };
        assert_eq!(field, "dx");
        let Expr::Field { base, field } = base.as_ref() else {
            panic!("expected a nested field access");
        };
        assert_eq!(field, "inner");
        assert!(matches!(base.as_ref(), Expr::Var(name) if name == "p"));
    }

    #[test]
    fn assignment_paths_parse_with_dotted_names() {
        let program = parse_source(
            "point : struct { x : word };\nmain() : word { auto p : point; p.x = 5; p.x }",
        );
        let Item::Function(function) = &program.items[1] else {
            panic!("expected a function");
        };
        let Stmt::Expr(Expr::Block(block)) = &function.body else {
            panic!("expected a block body");
        };
        match &block.stmts[1] {
            Stmt::Assign(path, Expr::Int(5)) => assert_eq!(path, "p.x"),
            other => panic!("expected an assignment to `p.x`, found {other:?}"),
        }
        match &block.tail.as_deref() {
            Some(Expr::Field { base, field }) => {
                assert!(matches!(base.as_ref(), Expr::Var(name) if name == "p"));
                assert_eq!(field, "x");
            }
            other => panic!("expected a field access, found {other:?}"),
        }
    }

    #[test]
    fn plain_variables_still_parse_as_assignment_paths() {
        let program = parse_source("main() : word { auto v; v = 3; v }");
        let Item::Function(function) = &program.items[0] else {
            panic!("expected a function");
        };
        let Stmt::Expr(Expr::Block(block)) = &function.body else {
            panic!("expected a block body");
        };
        match &block.stmts[1] {
            Stmt::Assign(name, Expr::Int(3)) => assert_eq!(name, "v"),
            other => panic!("expected assignment to `v`, found {other:?}"),
        }
    }

    #[test]
    fn field_access_works_on_calls_and_groups_too() {
        // `p.inner.x` chains through any primary, not just variables.
        let program = parse_source(
            "point : struct { x : word };\nidentity(p : point) : point p;\nmain() : word { auto p : point; identity(p).x }",
        );
        let Item::Function(function) = &program.items[2] else {
            panic!("expected a function");
        };
        let Stmt::Expr(Expr::Block(block)) = &function.body else {
            panic!("expected a block body");
        };
        let Some(Expr::Field { base, field }) = block.tail.as_deref() else {
            panic!("expected a field access, found {block:?}");
        };
        assert_eq!(field, "x");
        assert!(matches!(base.as_ref(), Expr::Call(name, _) if name == "identity"));
    }
}
