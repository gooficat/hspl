//! Recursive-descent parser for HSPL.
//!
//! Grammar (informal):
//!
//! ```text
//! program   := item*
//! item      := 'extern' ident '(' [type (',' type)*] ')' [':' type] ';'  // import
//!            | ident '(' [binding (',' binding)*] ')' [':' type] statement
//!            | ident [':' type] [integer | 'true' | 'false'] ';'      // global
//!
//! binding   := ident [':' type]
//!
//! statement := 'auto' binding (',' binding)* ';'
//!            | ident '=' expression ';'
//!            | 'jump' ident ';'                             // goto
//!            | ident ':'                                    // label
//!            | 'label' ident [':'] ';'                      // label, keyword form
//!            | expression ';'
//!            | expression                                   // block-like, no ';'
//!            | ';'
//!
//! expression := integer
//!             | 'true' | 'false'
//!             | ident
//!             | ident '(' [expression (',' expression)*] ')'
//!             | asm '{' raw assembly '}'
//!             | 'if' ['('] expression [')'] expression ['else' expression]
//!             | '{' statement* [expression] '}'              // statement expression
//! ```
//!
//! A missing value-type annotation (parameters, locals, globals) means
//! `word`; a missing return-type annotation means `void`. There is no
//! operator precedence table because there are no operators.

use crate::ast::{Binding, Block, ExternDecl, Expr, Function, Global, Item, Program, Stmt, Type};
use crate::lexer::{Token, TokenKind};

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
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    fn program(mut self) -> Result<Program, String> {
        let mut items = Vec::new();
        while !self.check(&TokenKind::Eof) {
            items.push(self.item()?);
        }
        Ok(Program { items })
    }

    // ----- token helpers -------------------------------------------------

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
        Type::from_name(&name).ok_or_else(|| {
            format!(
                "line {line}: unknown type `{name}` (expected word, int, short, byte, bool, or void)"
            )
        })
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
            let ty = self.optional_type()?;
            let init = match self.peek().clone() {
                TokenKind::Int(value) => {
                    self.advance();
                    value
                }
                TokenKind::True => {
                    self.advance();
                    1
                }
                TokenKind::False => {
                    self.advance();
                    0
                }
                _ => 0,
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
            TokenKind::Ident(_) if matches!(self.peek_at(1), TokenKind::Assign) => {
                let name = self.ident("in an assignment")?;
                self.expect(&TokenKind::Assign, "in an assignment")?;
                let value = self.expr()?;
                self.expect(&TokenKind::Semi, "after the assigned value")?;
                Ok(Parsed::Stmt(Stmt::Assign(name, value)))
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

    fn expr(&mut self) -> Result<Expr, String> {
        match self.peek().clone() {
            TokenKind::Int(value) => {
                self.advance();
                Ok(Expr::Int(value))
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
                    init: 42,
                }),
                Item::Global(Global {
                    name: "zero".into(),
                    ty: Type::Byte,
                    init: 0,
                }),
                Item::Global(Global {
                    name: "counter".into(),
                    ty: Type::Int,
                    init: 7,
                }),
                Item::Global(Global {
                    name: "flag".into(),
                    ty: Type::Bool,
                    init: 1,
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
}
