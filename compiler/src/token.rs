use crate::diag::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum StrSeg {
    Text { text: String },
    Expr { tokens: Vec<LexedToken> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct StrLit {
    pub segments: Vec<StrSeg>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Ident(String),

    // Literals
    Number,
    Char(char),
    Str(StrLit),

    // Keywords
    Module,
    Import,
    Class,
    Struct,
    Enum,
    Interface,
    Fn,
    Constructor,
    Property,
    Get,
    Set,
    Static,
    Override,
    Let,
    Var,
    Const,
    Public,
    Private,
    Protected,
    If,
    Else,
    While,
    For,
    In,
    Break,
    Continue,
    Return,
    Match,
    Case,
    True,
    False,
    None,
    Async,
    Await,
    Task,
    Channel,
    Unsafe,
    Extends,
    Implements,
    Is,
    As,
    From,
    This,
    Super,
    Init,
    Deinit,
    Operator,
    Try,
    Catch,
    Finally,
    Throw,

    // Punctuation & operators
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Colon,
    ColonColon,
    Semicolon,
    Arrow,
    FatArrow,
    Assign,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    ShlEq,
    ShrEq,
    AndEq,
    OrEq,
    XorEq,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    StarStar,
    Bang,
    Tilde,
    Amp,
    Pipe,
    Caret,
    Shl,
    Shr,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    AndAnd,
    OrOr,
    Question,
    QuestionDot,
    QuestionQuestion,
    QuestionColon,
    Range,
    RangeIncl,
    SendOp,
    Ellipsis,
    At,
    Hash,

    Newline,
    Eof,
}

impl Tok {
    pub fn describe(&self) -> String {
        match self {
            Tok::Ident(s) => format!("identifier `{s}`"),
            Tok::Number => "number".to_string(),
            Tok::Char(c) => format!("character literal `{c}`"),
            Tok::Str(_) => "string literal".to_string(),
            Tok::Newline => "end of line".to_string(),
            Tok::Eof => "end of file".to_string(),
            other => format!("`{}`", other.punct()),
        }
    }

    pub fn punct(&self) -> &'static str {
        use Tok::*;
        match self {
            LParen => "(",
            RParen => ")",
            LBrace => "{",
            RBrace => "}",
            LBracket => "[",
            RBracket => "]",
            Comma => ",",
            Dot => ".",
            Colon => ":",
            ColonColon => "::",
            Arrow => "->",
            FatArrow => "=>",
            Assign => "=",
            PlusEq => "+=",
            MinusEq => "-=",
            StarEq => "*=",
            SlashEq => "/=",
            PercentEq => "%=",
            ShlEq => "<<=",
            ShrEq => ">>=",
            AndEq => "&=",
            OrEq => "|=",
            XorEq => "^=",
            Plus => "+",
            Minus => "-",
            Star => "*",
            Slash => "/",
            Percent => "%",
            StarStar => "**",
            Bang => "!",
            Tilde => "~",
            Amp => "&",
            Pipe => "|",
            Caret => "^",
            Shl => "<<",
            Shr => ">>",
            EqEq => "==",
            NotEq => "!=",
            Lt => "<",
            Le => "<=",
            Gt => ">",
            Ge => ">=",
            AndAnd => "&&",
            OrOr => "||",
            Question => "?",
            QuestionDot => "?.",
            QuestionQuestion => "??",
            QuestionColon => "?:",
            Range => "..",
            RangeIncl => "..=",
            SendOp => "<-",
            Ellipsis => "...",
            At => "@",
            Hash => "#",

            // Keywords spell themselves, so a diagnostic can name the token it
            // wanted. Without these arms every keyword fell through to the `?`
            // catch-all and read as "`?`".
            Module => "module",
            Import => "import",
            Class => "class",
            Struct => "struct",
            Enum => "enum",
            Interface => "interface",
            Fn => "fn",
            Constructor => "constructor",
            Property => "property",
            Get => "get",
            Set => "set",
            Static => "static",
            Override => "override",
            Let => "let",
            Var => "var",
            Const => "const",
            Public => "public",
            Private => "private",
            Protected => "protected",
            If => "if",
            Else => "else",
            While => "while",
            For => "for",
            In => "in",
            Break => "break",
            Continue => "continue",
            Return => "return",
            Match => "match",
            Case => "case",
            True => "true",
            False => "false",
            None => "none",
            Async => "async",
            Await => "await",
            Task => "task",
            Channel => "channel",
            Unsafe => "unsafe",
            Extends => "extends",
            Implements => "implements",
            Is => "is",
            As => "as",
            From => "from",
            This => "this",
            Super => "super",
            Init => "init",
            Deinit => "deinit",
            Operator => "operator",
            Try => "try",
            Catch => "catch",
            Finally => "finally",
            Throw => "throw",

            _ => "?",
        }
    }

    /// Stable kind name for the canonical `pickle lex` dump. The self-hosted
    /// lexer must emit the same names, byte for byte, for differential testing.
    pub fn canonical(&self) -> &'static str {
        use Tok::*;
        match self {
            Ident(_) => "Ident",
            Number => "Number",
            Char(_) => "Char",
            Str(_) => "Str",
            Module => "Module",
            Import => "Import",
            Class => "Class",
            Struct => "Struct",
            Enum => "Enum",
            Interface => "Interface",
            Fn => "Fn",
            Constructor => "Constructor",
            Property => "Property",
            Get => "Get",
            Set => "Set",
            Static => "Static",
            Override => "Override",
            Let => "Let",
            Var => "Var",
            Const => "Const",
            Public => "Public",
            Private => "Private",
            Protected => "Protected",
            If => "If",
            Else => "Else",
            While => "While",
            For => "For",
            In => "In",
            Break => "Break",
            Continue => "Continue",
            Return => "Return",
            Match => "Match",
            Case => "Case",
            True => "True",
            False => "False",
            None => "None",
            Async => "Async",
            Await => "Await",
            Task => "Task",
            Channel => "Channel",
            Unsafe => "Unsafe",
            Extends => "Extends",
            Implements => "Implements",
            Is => "Is",
            As => "As",
            From => "From",
            This => "This",
            Super => "Super",
            Init => "Init",
            Deinit => "Deinit",
            Operator => "Operator",
            Try => "Try",
            Catch => "Catch",
            Finally => "Finally",
            Throw => "Throw",
            LParen => "LParen",
            RParen => "RParen",
            LBrace => "LBrace",
            RBrace => "RBrace",
            LBracket => "LBracket",
            RBracket => "RBracket",
            Comma => "Comma",
            Dot => "Dot",
            Colon => "Colon",
            ColonColon => "ColonColon",
            Semicolon => "Semicolon",
            Arrow => "Arrow",
            FatArrow => "FatArrow",
            Assign => "Assign",
            PlusEq => "PlusEq",
            MinusEq => "MinusEq",
            StarEq => "StarEq",
            SlashEq => "SlashEq",
            PercentEq => "PercentEq",
            ShlEq => "ShlEq",
            ShrEq => "ShrEq",
            AndEq => "AndEq",
            OrEq => "OrEq",
            XorEq => "XorEq",
            Plus => "Plus",
            Minus => "Minus",
            Star => "Star",
            Slash => "Slash",
            Percent => "Percent",
            StarStar => "StarStar",
            Bang => "Bang",
            Tilde => "Tilde",
            Amp => "Amp",
            Pipe => "Pipe",
            Caret => "Caret",
            Shl => "Shl",
            Shr => "Shr",
            EqEq => "EqEq",
            NotEq => "NotEq",
            Lt => "Lt",
            Le => "Le",
            Gt => "Gt",
            Ge => "Ge",
            AndAnd => "AndAnd",
            OrOr => "OrOr",
            Question => "Question",
            QuestionDot => "QuestionDot",
            QuestionQuestion => "QuestionQuestion",
            QuestionColon => "QuestionColon",
            Range => "Range",
            RangeIncl => "RangeIncl",
            SendOp => "SendOp",
            Ellipsis => "Ellipsis",
            At => "At",
            Hash => "Hash",
            Newline => "Newline",
            Eof => "Eof",
        }
    }

    /// Look up a keyword by its source text. Compiles to a jump table rather
    /// than a linear scan.
    pub fn keyword(s: &str) -> Option<Tok> {
        use Tok::*;
        match s {
            "module" => Some(Module),
            "import" => Some(Import),
            "class" => Some(Class),
            "struct" => Some(Struct),
            "enum" => Some(Enum),
            "interface" => Some(Interface),
            "fn" => Some(Fn),
            "constructor" => Some(Constructor),
            "property" => Some(Property),
            "get" => Some(Get),
            "set" => Some(Set),
            "static" => Some(Static),
            "override" => Some(Override),
            "let" => Some(Let),
            "var" => Some(Var),
            "const" => Some(Const),
            "public" => Some(Public),
            "private" => Some(Private),
            "protected" => Some(Protected),
            "if" => Some(If),
            "else" => Some(Else),
            "while" => Some(While),
            "for" => Some(For),
            "in" => Some(In),
            "break" => Some(Break),
            "continue" => Some(Continue),
            "return" => Some(Return),
            "match" => Some(Match),
            "case" => Some(Case),
            "true" => Some(True),
            "false" => Some(False),
            "none" => Some(None),
            "async" => Some(Async),
            "await" => Some(Await),
            "task" => Some(Task),
            "channel" => Some(Channel),
            "unsafe" => Some(Unsafe),
            "extends" => Some(Extends),
            "implements" => Some(Implements),
            "is" => Some(Is),
            "as" => Some(As),
            "from" => Some(From),
            "this" => Some(This),
            "super" => Some(Super),
            "init" => Some(Init),
            "deinit" => Some(Deinit),
            "operator" => Some(Operator),
            "try" => Some(Try),
            "catch" => Some(Catch),
            "finally" => Some(Finally),
            "throw" => Some(Throw),
            _ => Option::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: Tok,
    pub span: Span,
}

impl Token {
    pub fn new(kind: Tok, span: Span) -> Token {
        Token { kind, span }
    }
}

/// Value payloads carried on some tokens: number text/radix, char, ident kind.
#[derive(Debug, Clone, Default)]
pub struct TokenData {
    pub text: String,
    pub int: Option<i128>,
    pub float: Option<f64>,
    pub is_float: bool,
    pub suffix: Option<String>,
    pub doc: Option<String>,
}

impl PartialEq for TokenData {
    fn eq(&self, other: &Self) -> bool {
        self.text == other.text
            && self.int == other.int
            && self.is_float == other.is_float
            && self.suffix == other.suffix
            && self.doc == other.doc
            && match (self.float, other.float) {
                (Some(a), Some(b)) => a.to_bits() == b.to_bits(),
                (None, None) => true,
                _ => false,
            }
    }
}

/// A token plus the payload needed by the parser.
#[derive(Debug, Clone, PartialEq)]
pub struct LexedToken {
    pub token: Token,
    pub data: TokenData,
}

impl LexedToken {
    pub fn new(kind: Tok, span: Span) -> LexedToken {
        LexedToken {
            token: Token::new(kind, span),
            data: TokenData::default(),
        }
    }

    pub fn number(text: String, span: Span) -> LexedToken {
        let mut t = LexedToken::new(Tok::Number, span);
        t.data.text = text.clone();
        t.data.int = parse_int_literal(&text);
        if t.data.int.is_none() {
            t.data.float = text.replace('_', "").parse::<f64>().ok();
            t.data.is_float = text.contains('.') || text.contains('e') || text.contains('E');
        }
        t
    }

    /// Canonical single-line dump for `pickle lex`. The self-hosted lexer
    /// reproduces this string byte-for-byte; the differential test diffs the
    /// output of `pickle lex` on both compilers, so the format is the
    /// serialized contract that `pickle/compiler/lexer/` must match.
    ///
    /// Format: `Kind start:end field=value ...`, one line per token.
    /// - `Ident`   -> `text="..."` (escaped)
    /// - `Number`  -> `text="..." suf="..."|"-" isf=0|1 int=-|<i128>`
    /// - `Char`    -> `ch='x' int=-|<i128>`
    /// - `Str`     -> space-separated segments: `T("...")` text,
    ///   `E[tok; tok; ...]` interpolated expression token list
    /// - any kind  -> trailing `doc="..."` when the token carries a doc comment
    pub fn canonical(&self) -> String {
        let mut out = String::new();
        out.push_str(self.token.kind.canonical());
        out.push(' ');
        out.push_str(&self.token.span.start.to_string());
        out.push(':');
        out.push_str(&self.token.span.end.to_string());
        match &self.token.kind {
            Tok::Ident(name) => {
                out.push_str(" text=");
                out.push_str(&canonical_escape(name));
            }
            Tok::Number => {
                out.push_str(" text=");
                out.push_str(&canonical_escape(&self.data.text));
                out.push_str(" suf=");
                match &self.data.suffix {
                    Some(s) => out.push_str(&canonical_escape(s)),
                    None => out.push('-'),
                }
                out.push_str(" isf=");
                out.push(if self.data.is_float { '1' } else { '0' });
                out.push_str(" int=");
                match self.data.int {
                    Some(v) => out.push_str(&v.to_string()),
                    None => out.push('-'),
                }
            }
            Tok::Char(c) => {
                out.push_str(" ch=");
                out.push_str(&canonical_escape_char(*c));
                out.push_str(" int=");
                match self.data.int {
                    Some(v) => out.push_str(&v.to_string()),
                    None => out.push('-'),
                }
            }
            Tok::Str(lit) => {
                for seg in &lit.segments {
                    out.push(' ');
                    canonical_strseg(&mut out, seg);
                }
            }
            _ => {}
        }
        if let Some(doc) = &self.data.doc {
            out.push_str(" doc=");
            out.push_str(&canonical_escape(doc));
        }
        out
    }
}

/// Quote `s` with `"`, escaping the characters that would break the
/// one-token-per-line canonical format. The self-hosted lexer reimplements
/// the same table so dumps diff cleanly.
fn canonical_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

fn canonical_escape_char(c: char) -> String {
    let mut out = String::with_capacity(6);
    out.push('\'');
    match c {
        '\\' => out.push_str("\\\\"),
        '\'' => out.push_str("\\'"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\0' => out.push_str("\\0"),
        _ => out.push(c),
    }
    out.push('\'');
    out
}

/// Render one string-literal segment. `Text` segments quote their content;
/// `Expr` segments embed the canonical one-line dumps of their nested tokens.
fn canonical_strseg(out: &mut String, seg: &StrSeg) {
    match seg {
        StrSeg::Text { text } => {
            out.push_str("T(");
            out.push_str(&canonical_escape(text));
            out.push(')');
        }
        StrSeg::Expr { tokens } => {
            out.push_str("E[");
            for (i, t) in tokens.iter().enumerate() {
                if i > 0 {
                    out.push_str("; ");
                }
                out.push_str(&t.canonical());
            }
            out.push(']');
        }
    }
}

fn parse_int_literal(text: &str) -> Option<i128> {
    let clean = text.replace('_', "");
    let (sign, body) = match clean.strip_prefix('-') {
        Some(_) => ("-", &clean[1..]),
        None => ("", clean.as_str()),
    };
    let (radix, digits) =
        if let Some(h) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
            (16, h)
        } else if let Some(h) = body.strip_prefix("0b").or_else(|| body.strip_prefix("0B")) {
            (2, h)
        } else if let Some(h) = body.strip_prefix("0o").or_else(|| body.strip_prefix("0O")) {
            (8, h)
        } else {
            (10, body)
        };
    if digits.is_empty() {
        return None;
    }
    let v = i128::from_str_radix(digits, radix).ok()?;
    if sign == "-" {
        Some(-v)
    } else {
        Some(v)
    }
}
