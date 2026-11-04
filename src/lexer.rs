//! Tokenizer for HSPL.
//!
//! The language has no operators, so the token set is deliberately small. An
//! `asm { ... }` block is captured as a single [`TokenKind::Asm`] token because
//! its contents are raw target assembly, not HSPL tokens.

use std::fmt;

/// A token kind. Identifiers and integers carry their value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    Ident(String),
    Int(i64),
    /// Raw body of an `asm { ... }` block (the braces are not included).
    Asm(String),
    Auto,
    Extern,
    If,
    Else,
    Jump,
    Label,
    True,
    False,
    LParen,
    RParen,
    LBrace,
    RBrace,
    Comma,
    Colon,
    Semi,
    Assign,
    Eof,
}

impl fmt::Display for TokenKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenKind::Ident(name) => write!(f, "identifier `{name}`"),
            TokenKind::Int(value) => write!(f, "integer `{value}`"),
            TokenKind::Asm(_) => write!(f, "an `asm` block"),
            TokenKind::Auto => write!(f, "`auto`"),
            TokenKind::Extern => write!(f, "`extern`"),
            TokenKind::If => write!(f, "`if`"),
            TokenKind::Else => write!(f, "`else`"),
            TokenKind::Jump => write!(f, "`jump`"),
            TokenKind::Label => write!(f, "`label`"),
            TokenKind::True => write!(f, "`true`"),
            TokenKind::False => write!(f, "`false`"),
            TokenKind::LParen => write!(f, "`(`"),
            TokenKind::RParen => write!(f, "`)`"),
            TokenKind::LBrace => write!(f, "`{{`"),
            TokenKind::RBrace => write!(f, "`}}`"),
            TokenKind::Comma => write!(f, "`,`"),
            TokenKind::Colon => write!(f, "`:`"),
            TokenKind::Semi => write!(f, "`;`"),
            TokenKind::Assign => write!(f, "`=`"),
            TokenKind::Eof => write!(f, "end of file"),
        }
    }
}

/// A token plus the line it started on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub line: usize,
}

/// Tokenize `source`, ending with [`TokenKind::Eof`].
pub fn tokenize(source: &str) -> Result<Vec<Token>, String> {
    Lexer::new(source).run()
}

struct Lexer {
    chars: Vec<char>,
    pos: usize,
    line: usize,
}

impl Lexer {
    fn new(source: &str) -> Self {
        Self {
            chars: source.chars().collect(),
            pos: 0,
            line: 1,
        }
    }

    fn run(mut self) -> Result<Vec<Token>, String> {
        let mut tokens = Vec::new();

        loop {
            self.skip_trivia()?;
            let line = self.line;
            let Some(c) = self.peek() else {
                tokens.push(Token {
                    kind: TokenKind::Eof,
                    line,
                });
                return Ok(tokens);
            };

            let kind = if c.is_ascii_alphabetic() || c == '_' {
                let word = self.word();
                match word.as_str() {
                    "auto" => TokenKind::Auto,
                    "extern" => TokenKind::Extern,
                    "if" => TokenKind::If,
                    "else" => TokenKind::Else,
                    "jump" => TokenKind::Jump,
                    "label" => TokenKind::Label,
                    "true" => TokenKind::True,
                    "false" => TokenKind::False,
                    "asm" => {
                        let body = self.asm_block(line)?;
                        tokens.push(Token {
                            kind: TokenKind::Asm(body),
                            line,
                        });
                        continue;
                    }
                    _ => TokenKind::Ident(word),
                }
            } else if c.is_ascii_digit() {
                self.number(line)?
            } else {
                self.symbol(line)?
            };

            tokens.push(Token { kind, line });
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.pos + offset).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }

    /// Skip whitespace, `//` line comments and `/* block */` comments.
    fn skip_trivia(&mut self) -> Result<(), String> {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.bump();
                }
                Some('/') if self.peek_at(1) == Some('/') => {
                    while let Some(c) = self.bump() {
                        if c == '\n' {
                            break;
                        }
                    }
                }
                Some('/') if self.peek_at(1) == Some('*') => {
                    let start = self.line;
                    self.bump();
                    self.bump();
                    loop {
                        match (self.peek(), self.peek_at(1)) {
                            (None, _) => {
                                return Err(format!("line {start}: unterminated block comment"));
                            }
                            (Some('*'), Some('/')) => {
                                self.bump();
                                self.bump();
                                break;
                            }
                            _ => {
                                self.bump();
                            }
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn word(&mut self) -> String {
        let mut text = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == '_' {
                text.push(c);
                self.bump();
            } else {
                break;
            }
        }
        text
    }

    /// Read the raw body of `asm { ... }`, allowing nested braces.
    fn asm_block(&mut self, line: usize) -> Result<String, String> {
        self.skip_trivia()?;

        if self.peek() != Some('{') {
            return Err(format!("line {line}: expected `{{` after `asm`"));
        }
        self.bump();

        let start = self.line;
        let mut depth = 1usize;
        let mut body = String::new();

        loop {
            let Some(c) = self.bump() else {
                return Err(format!("line {start}: unterminated `asm` block"));
            };

            match c {
                '{' => {
                    depth += 1;
                    body.push(c);
                }
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    body.push(c);
                }
                _ => body.push(c),
            }
        }

        Ok(body)
    }

    fn number(&mut self, line: usize) -> Result<TokenKind, String> {
        // Hexadecimal literals are parsed as unsigned so they can be used as
        // two's-complement words, e.g. `0xFFFFFFFFFFFFFFFF` for -1.
        if self.peek() == Some('0') && matches!(self.peek_at(1), Some('x' | 'X')) {
            self.bump();
            self.bump();

            let mut digits = String::new();
            while let Some(c) = self.peek() {
                if c.is_ascii_hexdigit() {
                    digits.push(c);
                    self.bump();
                } else {
                    break;
                }
            }

            if digits.is_empty() {
                return Err(format!("line {line}: malformed hexadecimal literal"));
            }

            return u64::from_str_radix(&digits, 16)
                .map(|value| TokenKind::Int(value as i64))
                .map_err(|_| format!("line {line}: integer literal `0x{digits}` is out of range"));
        }

        let mut digits = String::new();
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                digits.push(c);
                self.bump();
            } else {
                break;
            }
        }

        digits
            .parse::<i64>()
            .map(TokenKind::Int)
            .map_err(|_| format!("line {line}: integer literal `{digits}` is out of range"))
    }

    fn symbol(&mut self, line: usize) -> Result<TokenKind, String> {
        let kind = match self.bump() {
            Some('(') => TokenKind::LParen,
            Some(')') => TokenKind::RParen,
            Some('{') => TokenKind::LBrace,
            Some('}') => TokenKind::RBrace,
            Some(',') => TokenKind::Comma,
            Some(':') => TokenKind::Colon,
            Some(';') => TokenKind::Semi,
            Some('=') => TokenKind::Assign,
            Some(c) => {
                return Err(format!(
                    "line {line}: unexpected character `{c}` (HSPL has no operators)"
                ));
            }
            None => TokenKind::Eof,
        };

        Ok(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(source: &str) -> Vec<TokenKind> {
        tokenize(source)
            .expect("source should tokenize")
            .into_iter()
            .map(|token| token.kind)
            .collect()
    }

    #[test]
    fn tokenizes_a_function() {
        assert_eq!(
            kinds("add(a, b) asm { movq $a, $out }"),
            vec![
                TokenKind::Ident("add".into()),
                TokenKind::LParen,
                TokenKind::Ident("a".into()),
                TokenKind::Comma,
                TokenKind::Ident("b".into()),
                TokenKind::RParen,
                TokenKind::Asm(" movq $a, $out ".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn tokenizes_statements() {
        assert_eq!(
            kinds("auto x, y; x = y;"),
            vec![
                TokenKind::Auto,
                TokenKind::Ident("x".into()),
                TokenKind::Comma,
                TokenKind::Ident("y".into()),
                TokenKind::Semi,
                TokenKind::Ident("x".into()),
                TokenKind::Assign,
                TokenKind::Ident("y".into()),
                TokenKind::Semi,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn tokenizes_type_annotations_and_booleans() {
        assert_eq!(
            kinds("f(x : byte) : bool true;"),
            vec![
                TokenKind::Ident("f".into()),
                TokenKind::LParen,
                TokenKind::Ident("x".into()),
                TokenKind::Colon,
                TokenKind::Ident("byte".into()),
                TokenKind::RParen,
                TokenKind::Colon,
                TokenKind::Ident("bool".into()),
                TokenKind::True,
                TokenKind::Semi,
                TokenKind::Eof,
            ]
        );
        assert_eq!(
            kinds("false"),
            vec![TokenKind::False, TokenKind::Eof]
        );
    }

    #[test]
    fn tokenizes_control_flow_keywords() {
        assert_eq!(
            kinds("if else jump label"),
            vec![
                TokenKind::If,
                TokenKind::Else,
                TokenKind::Jump,
                TokenKind::Label,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn captures_asm_raw() {
        let tokens = tokenize("asm { movq $$42, $out\n  addq $x, $out }").unwrap();
        assert_eq!(
            tokens[0].kind,
            TokenKind::Asm(" movq $$42, $out\n  addq $x, $out ".into())
        );
    }

    #[test]
    fn skips_comments() {
        assert_eq!(
            kinds("// note\nx /* inline */ = 1;"),
            vec![
                TokenKind::Ident("x".into()),
                TokenKind::Assign,
                TokenKind::Int(1),
                TokenKind::Semi,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn parses_hex_literals() {
        assert_eq!(
            kinds("0x10 0xFFFFFFFFFFFFFFFF"),
            vec![TokenKind::Int(16), TokenKind::Int(-1), TokenKind::Eof]
        );
    }

    #[test]
    fn rejects_operators() {
        let error = tokenize("x = 1 + 2;").unwrap_err();
        assert!(error.contains("no operators"), "{error}");
    }

    #[test]
    fn rejects_unterminated_asm() {
        assert!(tokenize("asm { movq x").is_err());
    }
}
