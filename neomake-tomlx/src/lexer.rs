//! Expression-level lexer for TOMLX.
//!
//! The lexer operates on a *single TOMLX expression slice*: the
//! right-hand side of `$var = <expr>`, of a `key = <expr>` whose value
//! the document scanner (see `scanner.rs`) found not to be plain TOML,
//! or a statement such as `git(...)`. Such a slice may span several
//! lines when it holds a multi-line string, array, inline table or `if`.
//!
//! Strings follow TOML: `"…"` and `"""…"""` are basic strings with TOML's
//! escapes plus `\$`, and are templates in which `${expr}` is
//! interpolated; `'…'` and `'''…'''` are literal strings, taken as
//! written.
//!
//! Every token carries a [`Span`] with file, line and column, so that
//! diagnostics point at the original source even though the lexer runs
//! on a sub-slice of it.

use std::path::{Path, PathBuf};

use crate::error::TomlxError;

/// A location in the source, used for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// Source file path.
    pub file: PathBuf,
    /// 1-indexed line number.
    pub line: usize,
    /// 1-indexed column number.
    pub col: usize,
}

impl Span {
    /// Construct a new span.
    pub fn new(file: impl Into<PathBuf>, line: usize, col: usize) -> Self {
        Self {
            file: file.into(),
            line,
            col,
        }
    }

    /// An error located at this span.
    pub fn error(&self, message: impl Into<String>) -> TomlxError {
        TomlxError::new(&self.file, self.line, self.col, message)
    }
}

/// Tokens emitted by the TOMLX expression lexer.
#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    /// A bare identifier (function name, keyword context).
    Ident(String),
    /// Integer literal.
    Int(i64),
    /// Floating-point literal.
    Float(f64),
    /// Boolean literal.
    Bool(bool),
    /// Basic (template) string, single- or multi-line, possibly
    /// containing `${...}` interpolations. Each segment is either a
    /// literal chunk or a raw source slice of an expression.
    TemplateStr(Vec<StrSegment>),
    /// Literal string, single- or multi-line: no interpolation, no escapes.
    LitStr(String),
    /// `$IDENT` or `${IDENT}`: a bare variable reference.
    VarRef(String),
    /// `if` keyword.
    If,
    /// `else` keyword.
    Else,
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// `,`
    Comma,
    /// `.` (separates the parts of a dotted key in an inline table)
    Dot,
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `==`
    EqEq,
    /// `!=`
    NotEq,
    /// `&&`
    AndAnd,
    /// `||`
    OrOr,
    /// `!`
    Bang,
    /// `=` (keyword arguments and inline-table entries)
    Eq,
}

/// A segment inside a template (basic) string.
#[derive(Debug, Clone, PartialEq)]
pub enum StrSegment {
    /// A literal, fully-unescaped character sequence.
    Lit(String),
    /// Raw source of an expression found between `${` and the matching `}`.
    /// The parser re-lexes and re-parses this slice.
    Interp(String),
}

/// A token paired with its source location.
#[derive(Debug, Clone)]
pub struct SpannedToken {
    /// Token payload.
    pub token: Token,
    /// Location where the token starts.
    pub span: Span,
}

/// Tokenize `src` as a single TOMLX expression.
///
/// `base` is the location of `src[0]` in the original file; the lexer
/// tracks newlines and columns relative to that base so that reported
/// diagnostics match the user's source.
pub fn tokenize(src: &str, base: &Span) -> Result<Vec<SpannedToken>, TomlxError> {
    let mut lex = Lexer::new(src, base);
    let mut out = Vec::new();
    while let Some(tok) = lex.next_token()? {
        out.push(tok);
    }
    Ok(out)
}

struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    file: PathBuf,
    line: usize,
    col: usize,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str, base: &Span) -> Self {
        Self {
            src: src.as_bytes(),
            pos: 0,
            file: base.file.clone(),
            line: base.line,
            col: base.col,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<u8> {
        self.src.get(self.pos + off).copied()
    }

    fn starts_with(&self, s: &[u8]) -> bool {
        self.src[self.pos..].starts_with(s)
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        if b == b'\n' {
            self.line += 1;
            self.col = 1;
        } else if b & 0xC0 != 0x80 {
            // Count characters, not the continuation bytes of UTF-8.
            self.col += 1;
        }
        Some(b)
    }

    fn bump_n(&mut self, n: usize) {
        for _ in 0..n {
            self.bump();
        }
    }

    fn span(&self) -> Span {
        Span::new(self.file.clone(), self.line, self.col)
    }

    fn err(&self, msg: impl Into<String>) -> TomlxError {
        TomlxError::new(&self.file, self.line, self.col, msg)
    }

    fn skip_ws_and_comments(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n' | b'\r') => {
                    self.bump();
                }
                Some(b'#') => {
                    while let Some(b) = self.peek() {
                        if b == b'\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                _ => break,
            }
        }
    }

    fn single(&mut self, token: Token, span: Span) -> Result<Option<SpannedToken>, TomlxError> {
        self.bump();
        Ok(Some(SpannedToken { token, span }))
    }

    fn double(&mut self, token: Token, span: Span) -> Result<Option<SpannedToken>, TomlxError> {
        self.bump_n(2);
        Ok(Some(SpannedToken { token, span }))
    }

    fn next_token(&mut self) -> Result<Option<SpannedToken>, TomlxError> {
        self.skip_ws_and_comments();
        let span = self.span();
        let b = match self.peek() {
            Some(b) => b,
            None => return Ok(None),
        };

        match b {
            b'(' => return self.single(Token::LParen, span),
            b')' => return self.single(Token::RParen, span),
            b'{' => return self.single(Token::LBrace, span),
            b'}' => return self.single(Token::RBrace, span),
            b'[' => return self.single(Token::LBracket, span),
            b']' => return self.single(Token::RBracket, span),
            b',' => return self.single(Token::Comma, span),
            b'.' => return self.single(Token::Dot, span),
            b'+' => return self.single(Token::Plus, span),
            // Unary or binary minus; the parser decides.
            b'-' => return self.single(Token::Minus, span),
            b'=' if self.peek_at(1) == Some(b'=') => return self.double(Token::EqEq, span),
            b'=' => return self.single(Token::Eq, span),
            b'!' if self.peek_at(1) == Some(b'=') => return self.double(Token::NotEq, span),
            b'!' => return self.single(Token::Bang, span),
            b'&' if self.peek_at(1) == Some(b'&') => return self.double(Token::AndAnd, span),
            b'&' => return Err(self.err("unexpected `&` (did you mean `&&`?)")),
            b'|' if self.peek_at(1) == Some(b'|') => return self.double(Token::OrOr, span),
            b'|' => return Err(self.err("unexpected `|` (did you mean `||`?)")),
            _ => {}
        }

        if b == b'$' {
            self.bump();
            let braced = self.peek() == Some(b'{');
            if braced {
                self.bump();
            }
            let name = self.read_ident()?;
            if braced {
                if self.peek() != Some(b'}') {
                    return Err(self.err("expected `}` to close `${...}`"));
                }
                self.bump();
            }
            return Ok(Some(SpannedToken {
                token: Token::VarRef(name),
                span,
            }));
        }

        if b == b'"' {
            let multiline = self.starts_with(b"\"\"\"");
            let segments = self.read_basic_string(multiline)?;
            return Ok(Some(SpannedToken {
                token: Token::TemplateStr(segments),
                span,
            }));
        }
        if b == b'\'' {
            let multiline = self.starts_with(b"'''");
            let s = self.read_literal_string(multiline)?;
            return Ok(Some(SpannedToken {
                token: Token::LitStr(s),
                span,
            }));
        }

        if b.is_ascii_digit() {
            return self.read_number(span).map(Some);
        }

        if is_ident_start(b) {
            let ident = self.read_ident()?;
            let tok = match ident.as_str() {
                "true" => Token::Bool(true),
                "false" => Token::Bool(false),
                "inf" => Token::Float(f64::INFINITY),
                "nan" => Token::Float(f64::NAN),
                "if" => Token::If,
                "else" => Token::Else,
                _ => Token::Ident(ident),
            };
            return Ok(Some(SpannedToken { token: tok, span }));
        }

        let ch = std::str::from_utf8(&self.src[self.pos..])
            .ok()
            .and_then(|s| s.chars().next())
            .unwrap_or(b as char);
        Err(self.err(format!("unexpected character `{ch}`")))
    }

    fn read_ident(&mut self) -> Result<String, TomlxError> {
        let start = self.pos;
        if !self.peek().map(is_ident_start).unwrap_or(false) {
            return Err(self.err("expected identifier"));
        }
        self.bump();
        while let Some(b) = self.peek() {
            if is_ident_continue(b) {
                self.bump();
            } else {
                break;
            }
        }
        Ok(String::from_utf8_lossy(&self.src[start..self.pos]).into_owned())
    }

    /// TOML numbers in decimal: digits with `_` separators, an optional
    /// fraction and an optional exponent.
    fn read_number(&mut self, span: Span) -> Result<SpannedToken, TomlxError> {
        let mut text = String::new();
        let mut is_float = false;
        self.read_digits(&mut text)?;
        if self.peek() == Some(b'.') && self.peek_at(1).is_some_and(|b| b.is_ascii_digit()) {
            is_float = true;
            text.push('.');
            self.bump();
            self.read_digits(&mut text)?;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            text.push('e');
            self.bump();
            if let Some(sign @ (b'+' | b'-')) = self.peek() {
                text.push(sign as char);
                self.bump();
            }
            self.read_digits(&mut text)?;
        }
        let token = if is_float {
            Token::Float(
                text.parse::<f64>()
                    .map_err(|_| span.error(format!("invalid float `{text}`")))?,
            )
        } else {
            Token::Int(
                text.parse::<i64>()
                    .map_err(|_| span.error(format!("invalid integer `{text}`")))?,
            )
        };
        Ok(SpannedToken { token, span })
    }

    fn read_digits(&mut self, out: &mut String) -> Result<(), TomlxError> {
        let start = out.len();
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() {
                out.push(b as char);
                self.bump();
            } else if b == b'_'
                && out.len() > start
                && self.peek_at(1).is_some_and(|c| c.is_ascii_digit())
            {
                self.bump();
            } else {
                break;
            }
        }
        if out.len() == start {
            return Err(self.err("expected digits"));
        }
        Ok(())
    }

    /// A `"…"` or `"""…"""` string, split at `${…}` interpolations.
    fn read_basic_string(&mut self, multiline: bool) -> Result<Vec<StrSegment>, TomlxError> {
        let start = self.span();
        if multiline {
            self.bump_n(3);
            // A newline right after the opening delimiter is not content.
            if self.starts_with(b"\r\n") {
                self.bump_n(2);
            } else if self.peek() == Some(b'\n') {
                self.bump();
            }
        } else {
            self.bump();
        }

        let mut segments: Vec<StrSegment> = Vec::new();
        let mut buf: Vec<u8> = Vec::new();
        let flush = |buf: &mut Vec<u8>, segments: &mut Vec<StrSegment>| {
            if !buf.is_empty() {
                let text = String::from_utf8(std::mem::take(buf))
                    .expect("bytes copied from a str in whole characters");
                segments.push(StrSegment::Lit(text));
            }
        };

        loop {
            let b = match self.peek() {
                Some(b) => b,
                None => return Err(start.error("unterminated string")),
            };
            match b {
                b'"' if !multiline => {
                    self.bump();
                    break;
                }
                b'"' if self.starts_with(b"\"\"\"") => {
                    // Up to two quotes right before the closing delimiter
                    // belong to the content: `""""` ends with a `"`.
                    let mut extra = 0;
                    while extra < 2 && self.peek_at(3 + extra) == Some(b'"') {
                        extra += 1;
                    }
                    buf.resize(buf.len() + extra, b'"');
                    self.bump_n(extra + 3);
                    break;
                }
                b'\n' if !multiline => {
                    return Err(start.error(
                        "unterminated string (a string that spans lines is written \"\"\"…\"\"\")",
                    ))
                }
                b'\\' => {
                    self.bump();
                    self.read_escape(&mut buf, multiline)?;
                }
                b'$' if self.peek_at(1) == Some(b'{') => {
                    flush(&mut buf, &mut segments);
                    self.bump_n(2);
                    segments.push(StrSegment::Interp(self.read_interpolation()?));
                }
                _ => {
                    buf.push(b);
                    self.bump();
                }
            }
        }
        flush(&mut buf, &mut segments);
        if segments.is_empty() {
            segments.push(StrSegment::Lit(String::new()));
        }
        Ok(segments)
    }

    /// The escape after a backslash, appended to `buf`.
    fn read_escape(&mut self, buf: &mut Vec<u8>, multiline: bool) -> Result<(), TomlxError> {
        let esc = self.peek().ok_or_else(|| self.err("unterminated escape"))?;
        if multiline && matches!(esc, b' ' | b'\t' | b'\r' | b'\n') {
            // A line-ending backslash swallows the newline and all the
            // whitespace that follows it.
            let mut i = 0;
            while matches!(self.peek_at(i), Some(b' ' | b'\t')) {
                i += 1;
            }
            if !matches!(self.peek_at(i), Some(b'\r' | b'\n')) {
                return Err(self.err("a `\\` followed by whitespace must end its line"));
            }
            while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
                self.bump();
            }
            return Ok(());
        }
        self.bump();
        let simple = match esc {
            b'n' => Some(b'\n'),
            b't' => Some(b'\t'),
            b'r' => Some(b'\r'),
            b'b' => Some(0x08),
            b'f' => Some(0x0c),
            b'e' => Some(0x1b),
            b'0' => Some(0),
            b'\\' => Some(b'\\'),
            b'"' => Some(b'"'),
            b'$' => Some(b'$'),
            _ => None,
        };
        if let Some(byte) = simple {
            buf.push(byte);
            return Ok(());
        }
        let digits = match esc {
            b'x' => 2,
            b'u' => 4,
            b'U' => 8,
            other => {
                return Err(self.err(format!(
                    "unknown escape `\\{}`",
                    char::from(other).escape_default()
                )))
            }
        };
        let end = self.pos + digits;
        let hex = self
            .src
            .get(self.pos..end)
            .and_then(|h| std::str::from_utf8(h).ok())
            .ok_or_else(|| self.err("truncated escape"))?;
        let code = u32::from_str_radix(hex, 16)
            .map_err(|_| self.err(format!("invalid escape `{hex}`")))?;
        let ch = char::from_u32(code)
            .ok_or_else(|| self.err(format!("`\\{}{hex}` is not a character", esc as char)))?;
        self.bump_n(digits);
        let mut utf8 = [0u8; 4];
        buf.extend_from_slice(ch.encode_utf8(&mut utf8).as_bytes());
        Ok(())
    }

    /// The raw source between `${` (already consumed) and its `}`.
    fn read_interpolation(&mut self) -> Result<String, TomlxError> {
        let start = self.span();
        let begin = self.pos;
        let mut depth = 1usize;
        loop {
            let c = self
                .peek()
                .ok_or_else(|| start.error("unterminated `${...}` interpolation in string"))?;
            match c {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                quote @ (b'"' | b'\'') => {
                    // A string inside the interpolation, skipped whole.
                    self.bump();
                    while let Some(c2) = self.peek() {
                        if c2 == b'\\' && quote == b'"' {
                            self.bump();
                        } else if c2 == quote {
                            break;
                        }
                        self.bump();
                    }
                }
                _ => {}
            }
            self.bump();
        }
        let raw = String::from_utf8_lossy(&self.src[begin..self.pos]).into_owned();
        self.bump(); // the closing `}`
        Ok(raw)
    }

    /// A `'…'` or `'''…'''` string, taken as written.
    fn read_literal_string(&mut self, multiline: bool) -> Result<String, TomlxError> {
        let start = self.span();
        if multiline {
            self.bump_n(3);
            if self.starts_with(b"\r\n") {
                self.bump_n(2);
            } else if self.peek() == Some(b'\n') {
                self.bump();
            }
        } else {
            self.bump();
        }
        let begin = self.pos;
        loop {
            let b = match self.peek() {
                Some(b) => b,
                None => return Err(start.error("unterminated literal string")),
            };
            if !multiline && b == b'\'' {
                let s = String::from_utf8_lossy(&self.src[begin..self.pos]).into_owned();
                self.bump();
                return Ok(s);
            }
            if multiline && self.starts_with(b"'''") {
                let mut extra = 0;
                while extra < 2 && self.peek_at(3 + extra) == Some(b'\'') {
                    extra += 1;
                }
                let end = self.pos + extra;
                let s = String::from_utf8_lossy(&self.src[begin..end]).into_owned();
                self.bump_n(extra + 3);
                return Ok(s);
            }
            if !multiline && b == b'\n' {
                return Err(start.error(
                    "unterminated literal string (a string that spans lines is written '''…''')",
                ));
            }
            self.bump();
        }
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Convenience: construct a span rooted at the top of a file.
pub fn span_for(file: &Path) -> Span {
    Span::new(file, 1, 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn base() -> Span {
        Span::new(PathBuf::from("test"), 1, 1)
    }

    fn tokens(src: &str) -> Vec<Token> {
        tokenize(src, &base())
            .unwrap()
            .into_iter()
            .map(|st| st.token)
            .collect()
    }

    fn one_string(src: &str) -> Vec<StrSegment> {
        match tokens(src).remove(0) {
            Token::TemplateStr(segs) => segs,
            other => panic!("expected TemplateStr, got {other:?}"),
        }
    }

    #[test]
    fn lexes_literals_and_operators() {
        let toks = tokens("1 + 2 == 3 && true || false");
        assert_eq!(
            toks,
            vec![
                Token::Int(1),
                Token::Plus,
                Token::Int(2),
                Token::EqEq,
                Token::Int(3),
                Token::AndAnd,
                Token::Bool(true),
                Token::OrOr,
                Token::Bool(false),
            ]
        );
    }

    #[test]
    fn lexes_var_refs() {
        let toks = tokens("$foo ${bar_baz}");
        assert_eq!(
            toks,
            vec![Token::VarRef("foo".into()), Token::VarRef("bar_baz".into()),]
        );
    }

    #[test]
    fn lexes_template_string_with_interp() {
        let segs = one_string(r#""hello ${name} x""#);
        assert_eq!(
            segs,
            vec![
                StrSegment::Lit("hello ".into()),
                StrSegment::Interp("name".into()),
                StrSegment::Lit(" x".into()),
            ]
        );
    }

    #[test]
    fn lexes_literal_string() {
        let toks = tokens("'hi ${not interpolated}'");
        assert_eq!(toks, vec![Token::LitStr("hi ${not interpolated}".into())]);
    }

    #[test]
    fn lexes_if_else_and_call() {
        let toks = tokens("if env(\"X\") == \"1\" { 1 } else { 2 }");
        assert!(toks.contains(&Token::If));
        assert!(toks.contains(&Token::Else));
        assert!(toks.contains(&Token::EqEq));
        assert!(toks
            .iter()
            .any(|t| matches!(t, Token::Ident(s) if s == "env")));
    }

    #[test]
    fn lexes_bare_eq_for_kwargs() {
        let toks = tokens("depth = 1");
        assert_eq!(
            toks,
            vec![Token::Ident("depth".into()), Token::Eq, Token::Int(1)]
        );
    }

    #[test]
    fn eq_eq_still_works() {
        let toks = tokens("a == b");
        assert!(toks.contains(&Token::EqEq));
    }

    #[test]
    fn keeps_non_ascii_text_intact() {
        assert_eq!(
            one_string("\"héllo — ✓ ${x}\""),
            vec![
                StrSegment::Lit("héllo — ✓ ".into()),
                StrSegment::Interp("x".into())
            ]
        );
        assert_eq!(tokens("'naïve'"), vec![Token::LitStr("naïve".into())]);
    }

    #[test]
    fn understands_toml_escapes() {
        assert_eq!(
            one_string(r#""a\tbé\U0001F600\x41\$\\\"""#),
            vec![StrSegment::Lit("a\tbé😀A$\\\"".into())]
        );
        assert!(tokenize(r#""\q""#, &base()).is_err());
    }

    #[test]
    fn lexes_multiline_basic_strings() {
        let segs = one_string("\"\"\"\nline one ${x}\n  two \"quoted\"\n\"\"\"");
        assert_eq!(
            segs,
            vec![
                StrSegment::Lit("line one ".into()),
                StrSegment::Interp("x".into()),
                StrSegment::Lit("\n  two \"quoted\"\n".into()),
            ]
        );
        // A line-ending backslash joins lines.
        assert_eq!(
            one_string("\"\"\"one \\\n    two\"\"\""),
            vec![StrSegment::Lit("one two".into())]
        );
        // Quotes right before the delimiter are content.
        assert_eq!(
            one_string("\"\"\"say \"hi\"\"\"\"\""),
            vec![StrSegment::Lit("say \"hi\"\"".into())]
        );
    }

    #[test]
    fn lexes_multiline_literal_strings() {
        let src = "'''\nif [ \"${VAR}\" ]; then\n  echo $? 'x'\nfi\n'''";
        assert_eq!(
            tokens(src),
            vec![Token::LitStr(
                "if [ \"${VAR}\" ]; then\n  echo $? 'x'\nfi\n".into()
            )]
        );
    }

    #[test]
    fn tracks_lines_through_multiline_strings() {
        let toks = tokenize("'''a\nb\nc''' + x", &base()).unwrap();
        assert_eq!(toks[2].span.line, 3);
    }

    #[test]
    fn lexes_toml_numbers() {
        assert_eq!(
            tokens("1_000 3.5 1e3 2.5E-1 inf"),
            vec![
                Token::Int(1000),
                Token::Float(3.5),
                Token::Float(1000.0),
                Token::Float(0.25),
                Token::Float(f64::INFINITY),
            ]
        );
    }

    #[test]
    fn lexes_dotted_keys() {
        assert_eq!(
            tokens("a.b = 1"),
            vec![
                Token::Ident("a".into()),
                Token::Dot,
                Token::Ident("b".into()),
                Token::Eq,
                Token::Int(1)
            ]
        );
    }
}
