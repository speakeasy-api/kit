use crate::{Diagnostic, Phase, Span};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TokenKind {
    Ident(String),
    Integer(String),
    Number(String),
    String(String),
    Return,
    For,
    In,
    Boundary,
    Fold,
    Skip,
    Assert,
    Fail,
    Retry,
    Catch,
    If,
    Else,
    And,
    Or,
    Not,
    Null,
    True,
    False,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Dot,
    Comma,
    Colon,
    Semicolon,
    Newline,
    Equal,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    EqualEqual,
    BangEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Eof,
}

impl TokenKind {
    fn continues_line(&self) -> bool {
        matches!(
            self,
            Self::Equal
                | Self::Comma
                | Self::Colon
                | Self::Plus
                | Self::Minus
                | Self::Star
                | Self::Slash
                | Self::Percent
                | Self::EqualEqual
                | Self::BangEqual
                | Self::Less
                | Self::LessEqual
                | Self::Greater
                | Self::GreaterEqual
                | Self::And
                | Self::Or
                | Self::Not
                | Self::In
                | Self::If
                | Self::Else
        )
    }
}

pub(crate) fn lex(source: &str) -> Result<Vec<Token>, Vec<Diagnostic>> {
    let mut lx = Lexer {
        source,
        pos: 0,
        tokens: vec![],
        diagnostics: vec![],
        paren: 0,
        bracket: 0,
        frames: Vec::new(),
    };
    while lx.pos < source.len() {
        lx.one();
    }
    lx.tokens.push(Token {
        kind: TokenKind::Eof,
        span: Span::new(source.len(), source.len()),
    });
    if lx.diagnostics.is_empty() {
        Ok(lx.tokens)
    } else {
        Err(lx.diagnostics)
    }
}

struct Lexer<'a> {
    source: &'a str,
    pos: usize,
    tokens: Vec<Token>,
    diagnostics: Vec<Diagnostic>,
    paren: usize,
    bracket: usize,
    /// Grouping counts saved at each `{`: newlines are statement
    /// terminators inside braces (blocks and object literals) even when the
    /// brace group itself sits inside parentheses or brackets.
    frames: Vec<(usize, usize)>,
}

impl Lexer<'_> {
    fn one(&mut self) {
        let start = self.pos;
        let ch = self.bump().unwrap();
        match ch {
            ' ' | '\t' | '\r' => {}
            '\n' => {
                if self.paren == 0
                    && self.bracket == 0
                    && !self.tokens.last().is_some_and(|t| t.kind.continues_line())
                    && !self.tokens.last().is_some_and(|t| {
                        matches!(t.kind, TokenKind::Newline | TokenKind::Semicolon)
                    })
                {
                    self.push(TokenKind::Newline, start);
                }
            }
            '#' => self.comment(),
            '/' if self.peek() == Some('/') => {
                self.bump();
                self.comment();
            }
            '/' => self.push(TokenKind::Slash, start),
            '(' => {
                self.paren += 1;
                self.push(TokenKind::LParen, start);
            }
            ')' => {
                self.paren = self.paren.saturating_sub(1);
                self.push(TokenKind::RParen, start);
            }
            '[' => {
                self.bracket += 1;
                self.push(TokenKind::LBracket, start);
            }
            ']' => {
                self.bracket = self.bracket.saturating_sub(1);
                self.push(TokenKind::RBracket, start);
            }
            '{' => {
                self.frames.push((self.paren, self.bracket));
                self.paren = 0;
                self.bracket = 0;
                self.push(TokenKind::LBrace, start);
            }
            '}' => {
                if let Some((paren, bracket)) = self.frames.pop() {
                    self.paren = paren;
                    self.bracket = bracket;
                }
                self.push(TokenKind::RBrace, start);
            }
            '.' => self.push(TokenKind::Dot, start),
            ',' => self.push(TokenKind::Comma, start),
            ':' => self.push(TokenKind::Colon, start),
            ';' => self.push(TokenKind::Semicolon, start),
            '+' => self.push(TokenKind::Plus, start),
            '-' => self.push(TokenKind::Minus, start),
            '*' => self.push(TokenKind::Star, start),
            '%' => self.push(TokenKind::Percent, start),
            '=' if self.peek() == Some('=') => {
                self.bump();
                self.push(TokenKind::EqualEqual, start);
            }
            '=' => self.push(TokenKind::Equal, start),
            '!' if self.peek() == Some('=') => {
                self.bump();
                self.push(TokenKind::BangEqual, start);
            }
            '<' if self.peek() == Some('=') => {
                self.bump();
                self.push(TokenKind::LessEqual, start);
            }
            '<' => self.push(TokenKind::Less, start),
            '>' if self.peek() == Some('=') => {
                self.bump();
                self.push(TokenKind::GreaterEqual, start);
            }
            '>' => self.push(TokenKind::Greater, start),
            '"' => self.string(start),
            c if c.is_ascii_digit() => self.number(start),
            c if c == '_' || unicode_ident::is_xid_start(c) => self.ident(start),
            _ => self.diagnostics.push(
                Diagnostic::error(
                    "RL1001",
                    Phase::Parse,
                    Span::new(start, self.pos),
                    "invalid character",
                    format!("`{ch}` is not valid Runlet syntax"),
                )
                .with_fix(Span::new(start, self.pos), "", "remove this character"),
            ),
        }
    }

    fn comment(&mut self) {
        while self.peek().is_some_and(|c| c != '\n') {
            self.bump();
        }
    }
    fn string(&mut self, start: usize) {
        let content_start = self.pos;
        let mut escaped = false;
        while let Some(c) = self.bump() {
            if c == '"' && !escaped {
                let raw = &self.source[content_start..self.pos - 1];
                match serde_json::from_str::<String>(&format!("\"{raw}\"")) {
                    Ok(s) => self.push(TokenKind::String(s), start),
                    Err(e) => self.diagnostics.push(
                        Diagnostic::error(
                            "RL1003",
                            Phase::Parse,
                            Span::new(start, self.pos),
                            "invalid string escape",
                            e.to_string(),
                        )
                        .with_fix(
                            Span::new(start, self.pos),
                            "\"\"",
                            "replace with a valid JSON string",
                        ),
                    ),
                }
                return;
            }
            escaped = c == '\\' && !escaped;
            if c != '\\' {
                escaped = false;
            }
        }
        self.diagnostics.push(
            Diagnostic::error(
                "RL1002",
                Phase::Parse,
                Span::new(start, self.pos),
                "unterminated string",
                "add a closing double quote",
            )
            .with_fix(Span::new(self.pos, self.pos), "\"", "close the string"),
        );
    }
    fn number(&mut self, start: usize) {
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.bump();
        }
        let mut number = false;
        if self.peek() == Some('.') && self.peek_second().is_some_and(|c| c.is_ascii_digit()) {
            number = true;
            self.bump();
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.bump();
            }
        }
        if self.peek().is_some_and(|c| c == 'e' || c == 'E') {
            number = true;
            self.bump();
            if self.peek().is_some_and(|c| c == '+' || c == '-') {
                self.bump();
            }
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.bump();
            }
        }
        let raw = self.source[start..self.pos].to_owned();
        self.push(
            if number {
                TokenKind::Number(raw)
            } else {
                TokenKind::Integer(raw)
            },
            start,
        );
    }
    fn ident(&mut self, start: usize) {
        while self
            .peek()
            .is_some_and(|c| c == '_' || unicode_ident::is_xid_continue(c))
        {
            self.bump();
        }
        let s = &self.source[start..self.pos];
        let kind = match s {
            "return" => TokenKind::Return,
            "for" => TokenKind::For,
            "in" => TokenKind::In,
            "boundary" => TokenKind::Boundary,
            "fold" => TokenKind::Fold,
            "skip" => TokenKind::Skip,
            "assert" => TokenKind::Assert,
            "fail" => TokenKind::Fail,
            "retry" => TokenKind::Retry,
            "catch" => TokenKind::Catch,
            "if" => TokenKind::If,
            "else" => TokenKind::Else,
            "and" => TokenKind::And,
            "or" => TokenKind::Or,
            "not" => TokenKind::Not,
            "null" => TokenKind::Null,
            "true" => TokenKind::True,
            "false" => TokenKind::False,
            _ => TokenKind::Ident(s.to_owned()),
        };
        self.push(kind, start);
    }
    fn push(&mut self, kind: TokenKind, start: usize) {
        self.tokens.push(Token {
            kind,
            span: Span::new(start, self.pos),
        });
    }
    fn peek(&self) -> Option<char> {
        self.source[self.pos..].chars().next()
    }
    fn peek_second(&self) -> Option<char> {
        let mut i = self.source[self.pos..].chars();
        i.next();
        i.next()
    }
    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }
}
