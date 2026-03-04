use super::ast::{Expr, FnBody, Stmt};
use super::registry::{
    ToyField, ToyFieldType, ToyFunction, ToyParam, ToyStruct, ToylangRegistry,
};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Clone)]
enum Token {
    Ident(String),
    LBrace,
    RBrace,
    LParen,
    RParen,
    LAngle,
    RAngle,
    Colon,
    DoubleColon, // ::
    Comma,
    Ampersand,
    Star,
    Arrow,     // ->
    Dot,       // .
    Semicolon, // ;
    Equals,    // =
    IntLit(i64),
    Eof,
}

fn tokenize(src: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // Skip whitespace
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }

        // Skip line comments
        if chars[i] == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }

        // Arrow ->
        if chars[i] == '-' && i + 1 < chars.len() && chars[i + 1] == '>' {
            tokens.push(Token::Arrow);
            i += 2;
            continue;
        }

        // DoubleColon ::
        if chars[i] == ':' && i + 1 < chars.len() && chars[i + 1] == ':' {
            tokens.push(Token::DoubleColon);
            i += 2;
            continue;
        }

        // Digit sequences
        if chars[i].is_ascii_digit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let s: String = chars[start..i].iter().collect();
            tokens.push(Token::IntLit(s.parse::<i64>().unwrap()));
            continue;
        }

        // Single-char tokens
        match chars[i] {
            '{' => { tokens.push(Token::LBrace); i += 1; }
            '}' => { tokens.push(Token::RBrace); i += 1; }
            '(' => { tokens.push(Token::LParen); i += 1; }
            ')' => { tokens.push(Token::RParen); i += 1; }
            '<' => { tokens.push(Token::LAngle); i += 1; }
            '>' => { tokens.push(Token::RAngle); i += 1; }
            ':' => { tokens.push(Token::Colon); i += 1; }
            ',' => { tokens.push(Token::Comma); i += 1; }
            '&' => { tokens.push(Token::Ampersand); i += 1; }
            '*' => { tokens.push(Token::Star); i += 1; }
            '.' => { tokens.push(Token::Dot); i += 1; }
            ';' => { tokens.push(Token::Semicolon); i += 1; }
            '=' => { tokens.push(Token::Equals); i += 1; }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                tokens.push(Token::Ident(chars[start..i].iter().collect()));
            }
            _ => { i += 1; } // skip unknown chars
        }
    }

    tokens.push(Token::Eof);
    tokens
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn peek2(&self) -> Option<&Token> {
        self.tokens.get(self.pos + 1)
    }

    fn consume(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        t
    }

    fn expect_ident(&mut self) -> Result<String, String> {
        match self.consume() {
            Token::Ident(s) => Ok(s),
            t => Err(format!("expected identifier, got {:?}", t)),
        }
    }

    fn expect(&mut self, expected: Token) -> Result<(), String> {
        let t = self.consume();
        if t == expected {
            Ok(())
        } else {
            Err(format!("expected {:?}, got {:?}", expected, t))
        }
    }

    fn parse_program(&mut self) -> Result<ToylangRegistry, String> {
        let mut structs: HashMap<String, ToyStruct> = HashMap::new();
        let mut functions: HashMap<String, ToyFunction> = HashMap::new();

        loop {
            match self.peek() {
                Token::Ident(s) if s == "struct" => {
                    let (name, s) = self.parse_struct()?;
                    structs.insert(name, s);
                }
                Token::Ident(s) if s == "fn" => {
                    let (name, f) = self.parse_fn()?;
                    functions.insert(name, f);
                }
                Token::Eof => break,
                t => return Err(format!("unexpected token {:?} at top level", t)),
            }
        }

        Ok(ToylangRegistry { structs, functions })
    }

    fn parse_struct(&mut self) -> Result<(String, ToyStruct), String> {
        // consume "struct"
        self.consume();
        let name = self.expect_ident()?;
        self.expect(Token::LBrace)?;

        let mut fields = Vec::new();
        while self.peek() != &Token::RBrace && self.peek() != &Token::Eof {
            fields.push(self.parse_field()?);
            // optional trailing comma
            if self.peek() == &Token::Comma {
                self.consume();
            }
        }
        self.expect(Token::RBrace)?;

        Ok((name.clone(), ToyStruct { name, fields }))
    }

    fn parse_field(&mut self) -> Result<ToyField, String> {
        let name = self.expect_ident()?;
        self.expect(Token::Colon)?;
        let rust_type = self.parse_primitive_type()?;
        Ok(ToyField { name, rust_type })
    }

    fn parse_primitive_type(&mut self) -> Result<ToyFieldType, String> {
        let s = self.expect_ident()?;
        match s.as_str() {
            "i32" => Ok(ToyFieldType::I32),
            "i64" => Ok(ToyFieldType::I64),
            "f64" => Ok(ToyFieldType::F64),
            "bool" => Ok(ToyFieldType::Bool),
            other => Err(format!(
                "unsupported field type '{}'; only i32, i64, f64, bool are allowed",
                other
            )),
        }
    }

    fn parse_fn(&mut self) -> Result<(String, ToyFunction), String> {
        // consume "fn"
        self.consume();
        let name = self.expect_ident()?;
        self.expect(Token::LParen)?;
        let params = self.parse_params()?;
        self.expect(Token::RParen)?;

        let return_ty = if self.peek() == &Token::Arrow {
            self.consume();
            Some(self.parse_type_str()?)
        } else {
            None
        };

        // parse function body
        self.expect(Token::LBrace)?;
        let body = self.parse_fn_body()?;
        // parse_fn_body consumes everything up to and including the closing RBrace

        Ok((name.clone(), ToyFunction { name, params, return_ty, body: Some(body) }))
    }

    fn parse_fn_body(&mut self) -> Result<FnBody, String> {
        let mut stmts = Vec::new();

        loop {
            // End of body
            if self.peek() == &Token::RBrace || self.peek() == &Token::Eof {
                self.consume(); // consume '}'
                return Ok(FnBody { stmts, ret: None });
            }

            // "let" statement
            if let Token::Ident(s) = self.peek() {
                if s == "let" {
                    self.consume(); // consume "let"
                    let var_name = self.expect_ident()?;
                    self.expect(Token::Equals)?;
                    let expr = self.parse_expr()?;
                    self.expect(Token::Semicolon)?;
                    stmts.push(Stmt::Let { name: var_name, expr });
                    continue;
                }
            }

            // Expression — either trailing return or stmt followed by ';'
            let expr = self.parse_expr()?;
            if self.peek() == &Token::Semicolon {
                self.consume(); // consume ';'
                stmts.push(Stmt::ExprStmt(expr));
            } else {
                // trailing expression — return value
                self.expect(Token::RBrace)?;
                return Ok(FnBody { stmts, ret: Some(expr) });
            }
        }
    }

    fn parse_expr(&mut self) -> Result<Expr, String> {
        let mut expr = self.parse_primary()?;

        // method call chaining: expr.method(args)
        loop {
            if self.peek() == &Token::Dot {
                self.consume(); // consume '.'
                let method = self.expect_ident()?;
                self.expect(Token::LParen)?;
                let args = self.parse_args()?;
                self.expect(Token::RParen)?;
                expr = Expr::MethodCall {
                    receiver: Box::new(expr),
                    method,
                    args,
                };
            } else {
                break;
            }
        }

        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<Expr, String> {
        match self.peek().clone() {
            Token::IntLit(n) => {
                self.consume();
                Ok(Expr::IntLit(n))
            }
            Token::Ident(name) => {
                // peek ahead to distinguish:
                //   IDENT "::" IDENT "(" -> StaticCall
                //   IDENT "{" -> StructLit (only when next non-ambiguous)
                //   IDENT otherwise -> Var
                let name = name.clone();
                self.consume(); // consume the ident

                if self.peek() == &Token::DoubleColon {
                    // StaticCall: Ty::method(args)
                    self.consume(); // consume '::'
                    let method = self.expect_ident()?;
                    self.expect(Token::LParen)?;
                    let args = self.parse_args()?;
                    self.expect(Token::RParen)?;
                    Ok(Expr::StaticCall { ty: name, method, args })
                } else if self.peek() == &Token::LBrace {
                    // StructLit: Name { field: expr, ... }
                    self.consume(); // consume '{'
                    let mut fields = Vec::new();
                    while self.peek() != &Token::RBrace && self.peek() != &Token::Eof {
                        let field_name = self.expect_ident()?;
                        self.expect(Token::Colon)?;
                        let field_expr = self.parse_expr()?;
                        fields.push((field_name, field_expr));
                        if self.peek() == &Token::Comma {
                            self.consume();
                        }
                    }
                    self.expect(Token::RBrace)?;
                    Ok(Expr::StructLit { name, fields })
                } else {
                    Ok(Expr::Var(name))
                }
            }
            t => Err(format!("expected expression, got {:?}", t)),
        }
    }

    fn parse_args(&mut self) -> Result<Vec<Expr>, String> {
        let mut args = Vec::new();
        while self.peek() != &Token::RParen && self.peek() != &Token::Eof {
            args.push(self.parse_expr()?);
            if self.peek() == &Token::Comma {
                self.consume();
            }
        }
        Ok(args)
    }

    fn parse_params(&mut self) -> Result<Vec<ToyParam>, String> {
        let mut params = Vec::new();
        while self.peek() != &Token::RParen && self.peek() != &Token::Eof {
            let name = self.expect_ident()?;
            self.expect(Token::Colon)?;
            let ty = self.parse_type_str()?;
            params.push(ToyParam { name, ty });
            if self.peek() == &Token::Comma {
                self.consume();
            }
        }
        Ok(params)
    }

    /// Parse a type expression and return it as a string.
    fn parse_type_str(&mut self) -> Result<String, String> {
        match self.peek().clone() {
            Token::Ampersand => {
                self.consume();
                // optional "mut"
                let prefix = if let Token::Ident(s) = self.peek() {
                    if s == "mut" {
                        self.consume();
                        "&mut ".to_string()
                    } else {
                        "&".to_string()
                    }
                } else {
                    "&".to_string()
                };
                let inner = self.parse_type_str()?;
                Ok(format!("{}{}", prefix, inner))
            }
            Token::Star => {
                self.consume();
                let qualifier = self.expect_ident()?;
                if qualifier != "const" && qualifier != "mut" {
                    return Err(format!("expected 'const' or 'mut' after '*', got '{}'", qualifier));
                }
                let inner = self.parse_type_str()?;
                Ok(format!("*{} {}", qualifier, inner))
            }
            Token::Ident(s) => {
                let s = s.clone();
                self.consume();
                if self.peek() == &Token::LAngle {
                    self.consume();
                    let mut args = Vec::new();
                    while self.peek() != &Token::RAngle && self.peek() != &Token::Eof {
                        args.push(self.parse_type_str()?);
                        if self.peek() == &Token::Comma {
                            self.consume();
                        }
                    }
                    self.expect(Token::RAngle)?;
                    Ok(format!("{}<{}>", s, args.join(", ")))
                } else {
                    Ok(s)
                }
            }
            t => Err(format!("expected type, got {:?}", t)),
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn parse(src: &str) -> Result<ToylangRegistry, String> {
    Parser::new(tokenize(src)).parse_program()
}
