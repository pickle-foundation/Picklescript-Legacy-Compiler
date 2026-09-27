use crate::ast::*;
use crate::diag::{Diagnostic, DiagnosticSink, Span};
use crate::token::{LexedToken, StrLit, StrSeg, Tok};

type PResult<T> = Result<T, ()>;

/// How the upcoming `name<Type,...>` bracket resolves: a call `name<T>(args)`,
/// a standalone generic value `name<T>`, or no generic bracket at all (None).
#[derive(PartialEq, Clone, Copy)]
enum GenericCallFlavor {
    Call,
    Value,
    None,
}

#[allow(clippy::result_unit_err)]
pub fn parse(tokens: Vec<LexedToken>, diags: &DiagnosticSink) -> PResult<Program> {
    Parser::new(tokens, diags).parse_program()
}

fn is_wildcard_ident(s: &str) -> bool {
    s == "_"
}

struct Parser<'a> {
    tokens: Vec<LexedToken>,
    pos: usize,
    diags: &'a DiagnosticSink,
}

impl<'a> Parser<'a> {
    fn new(tokens: Vec<LexedToken>, diags: &'a DiagnosticSink) -> Parser<'a> {
        Parser {
            tokens,
            pos: 0,
            diags,
        }
    }

    // ---------- token helpers ----------

    fn peek(&self) -> &LexedToken {
        &self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    fn peek_kind(&self) -> &Tok {
        &self.peek().token.kind
    }

    fn peek_at(&self, ahead: usize) -> &Tok {
        &self.tokens[(self.pos + ahead).min(self.tokens.len() - 1)]
            .token
            .kind
    }

    fn span(&self) -> Span {
        self.peek().token.span
    }

    fn prev_span(&self) -> Span {
        if self.pos == 0 {
            self.span()
        } else {
            self.tokens[self.pos - 1].token.span
        }
    }

    fn advance(&mut self) -> LexedToken {
        let t = self.peek().clone();
        if !matches!(t.token.kind, Tok::Eof) {
            self.pos += 1;
        }
        t
    }

    fn kind(&self) -> &Tok {
        self.peek_kind()
    }

    fn bump(&mut self) -> Tok {
        self.advance().token.kind
    }

    fn at(&self, kind: &Tok) -> bool {
        self.peek_kind() == kind
    }

    fn eat(&mut self, kind: &Tok) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: &Tok) -> PResult<()> {
        if self.at(kind) {
            self.bump();
            Ok(())
        } else {
            self.err_expected(kind);
            Err(())
        }
    }

    fn err_expected(&mut self, kind: &Tok) {
        let span = self.span();
        let expected = match kind {
            Tok::Newline => "end of line".to_string(),
            other => format!("`{}`", other.punct()),
        };
        self.err_at(
            span,
            format!("expected {expected}, found {}", self.kind().describe()),
        );
    }

    fn err_at(&mut self, span: Span, msg: impl Into<String>) {
        self.diags
            .emit(Diagnostic::error_at(span, msg).with_code(crate::error::ErrorCode::Syntax));
    }

    fn err_here(&mut self, msg: impl Into<String>) {
        let span = self.span();
        self.err_at(span, msg);
    }

    fn newlines(&mut self) {
        while self.at(&Tok::Newline) {
            self.bump();
        }
    }

    #[allow(dead_code)]
    fn start_kw(&self, kw: &Tok) -> bool {
        self.peek_kind() == kw
    }

    /// Skip newlines if the preceding token implies expression continuation.
    fn continuation_after(&mut self, prev: &Tok) {
        let continues = matches!(
            prev,
            Tok::Plus
                | Tok::Minus
                | Tok::Star
                | Tok::Slash
                | Tok::Percent
                | Tok::StarStar
                | Tok::Shl
                | Tok::Shr
                | Tok::Amp
                | Tok::Pipe
                | Tok::Caret
                | Tok::AndAnd
                | Tok::OrOr
                | Tok::EqEq
                | Tok::NotEq
                | Tok::Lt
                | Tok::Le
                | Tok::Gt
                | Tok::Ge
                | Tok::Comma
                | Tok::Dot
                | Tok::QuestionDot
                | Tok::Arrow
                | Tok::FatArrow
                | Tok::Range
                | Tok::RangeIncl
                | Tok::QuestionQuestion
                | Tok::Question
                | Tok::QuestionColon
                | Tok::SendOp
                | Tok::LParen
                | Tok::LBracket
                | Tok::Assign
                | Tok::PlusEq
                | Tok::MinusEq
                | Tok::StarEq
                | Tok::SlashEq
                | Tok::PercentEq
                | Tok::ShlEq
                | Tok::ShrEq
                | Tok::AndEq
                | Tok::OrEq
                | Tok::XorEq
        );
        if continues {
            self.newlines();
        }
    }

    /// True when an operator-prefixed next line should be glued onto the
    /// expression currently being parsed.
    ///
    /// Glue is refused when the last significant token was a closing brace:
    /// a block-ended construct (`if` without `else`, `match`, `unsafe`,
    /// lambda, bare block) is complete on its own line, so a following
    /// operator-prefixed line is a NEW statement (`-1` after `}` is unary
    /// negation, not `(if-expr) - 1`). Otherwise chains and multi-line
    /// arithmetic still continue (`a\n+ b`, `f()\n.bar()`).
    fn next_begins_continuation(&self) -> bool {
        if self.last_sig_was_rbrace() {
            return false;
        }
        match self.peek_non_nl(1) {
            Some(t) => matches!(
                t,
                Tok::Dot
                    | Tok::QuestionDot
                    | Tok::Plus
                    | Tok::Minus
                    | Tok::Star
                    | Tok::Slash
                    | Tok::Percent
                    | Tok::StarStar
                    | Tok::AndAnd
                    | Tok::OrOr
                    | Tok::EqEq
                    | Tok::NotEq
                    | Tok::Lt
                    | Tok::Le
                    | Tok::Gt
                    | Tok::Ge
                    | Tok::Shl
                    | Tok::Shr
                    | Tok::Amp
                    | Tok::Pipe
                    | Tok::Caret
                    | Tok::Range
                    | Tok::RangeIncl
                    | Tok::Question
                    | Tok::QuestionQuestion
                    | Tok::QuestionColon
                    | Tok::SendOp
            ),
            None => false,
        }
    }

    /// Whether the closest token behind the cursor (past any newlines) is a
    /// closing brace.
    fn last_sig_was_rbrace(&self) -> bool {
        let mut i = self.pos;
        while i > 0 {
            i -= 1;
            match &self.tokens[i].token.kind {
                Tok::Newline => continue,
                k => return matches!(k, Tok::RBrace),
            }
        }
        false
    }

    fn peek_non_nl(&self, mut ahead: usize) -> Option<Tok> {
        loop {
            let t = self.peek_at(ahead).clone();
            match t {
                Tok::Newline => ahead += 1,
                Tok::Eof => return None,
                _ => return Some(t),
            }
        }
    }

    fn expect_stmt_end(&mut self) -> PResult<()> {
        // Optional `;` is tolerated as a statement separator.
        while self.at(&Tok::Semicolon) {
            self.bump();
        }
        if self.at(&Tok::Newline) {
            self.newlines();
            Ok(())
        } else if self.at(&Tok::RBrace) || self.at(&Tok::Eof) {
            Ok(())
        } else {
            let found = self.kind().clone();
            self.err_here(format!("expected end of line, found {}", found.describe()));
            Err(())
        }
    }

    fn expect_ident(&mut self, what: &str) -> PResult<String> {
        match self.kind().clone() {
            Tok::Ident(s) => {
                self.bump();
                Ok(s)
            }
            other => {
                if let Some(kw) = contextual_ident(&other) {
                    self.bump();
                    return Ok(kw.to_string());
                }
                self.err_at(
                    self.span(),
                    format!("expected identifier for {what}, found {}", other.describe()),
                );
                Err(())
            }
        }
    }

    // ---------- program ----------

    fn parse_program(&mut self) -> PResult<Program> {
        let mut module = None;
        let mut imports = Vec::new();
        let mut items = Vec::new();
        self.newlines();
        loop {
            if self.at(&Tok::Eof) {
                break;
            }
            match self.kind().clone() {
                Tok::Module => {
                    module = Some(self.parse_module_decl()?);
                    self.newlines();
                }
                Tok::Import => {
                    let imp = self.parse_import()?;
                    imports.push(imp);
                    self.newlines();
                }
                _ => {
                    if self.at(&Tok::Public) && self.peek_at(1) == &Tok::Import {
                        let imp = self.parse_import()?;
                        imports.push(imp);
                        self.newlines();
                        continue;
                    }
                    match self.parse_item() {
                        Ok(more) => items.extend(more),
                        Err(()) => self.recover_to_item(),
                    }
                    self.newlines();
                }
            }
        }
        Ok(Program {
            module,
            imports,
            items,
        })
    }

    fn parse_module_decl(&mut self) -> PResult<ModuleDecl> {
        let start = self.span();
        self.expect(&Tok::Module)?;
        let mut path = Vec::new();
        loop {
            path.push(self.expect_ident("module path")?);
            if self.at(&Tok::Dot) {
                self.bump();
            } else {
                break;
            }
        }
        let span = start.to(self.prev_span());
        self.expect_stmt_end()?;
        Ok(ModuleDecl { path, span })
    }

    fn parse_import(&mut self) -> PResult<ImportDecl> {
        let start = self.span();
        let is_public = self.eat(&Tok::Public);
        self.expect(&Tok::Import)?;

        let (source, kind) = if self.at(&Tok::LBrace) {
            self.bump();
            let mut items = Vec::new();
            if self.at(&Tok::RBrace) {
                self.err_here("import list must not be empty");
                self.bump();
            } else {
                loop {
                    let istart = self.span();
                    let itm_path = self.parse_dotted_path("import path")?;
                    let alias = if self.eat(&Tok::As) {
                        Some(self.expect_ident("import alias")?)
                    } else {
                        None
                    };
                    items.push(ImportItem {
                        path: itm_path,
                        alias,
                        span: istart.to(self.prev_span()),
                    });
                    if self.eat(&Tok::Comma) {
                        self.newlines();
                        continue;
                    }
                    break;
                }
                self.expect(&Tok::RBrace)?;
            }
            self.expect(&Tok::From)?;
            let source = self.parse_dotted_path("import source")?;
            (source, ImportKind::Items { items })
        } else if self.at(&Tok::Star) {
            self.bump();
            self.expect(&Tok::From)?;
            let source = self.parse_dotted_path("import source")?;
            (source, ImportKind::Wildcard)
        } else {
            let first = self.parse_dotted_path("import path")?;
            if self.eat(&Tok::From) {
                let source = self.parse_dotted_path("import source")?;
                let alias = if self.eat(&Tok::As) {
                    Some(self.expect_ident("import alias")?)
                } else {
                    None
                };
                let span = start.to(self.prev_span());
                (
                    source,
                    ImportKind::Items {
                        items: vec![ImportItem {
                            path: first,
                            alias,
                            span,
                        }],
                    },
                )
            } else {
                let alias = if self.eat(&Tok::As) {
                    Some(self.expect_ident("import alias")?)
                } else {
                    None
                };
                (first, ImportKind::Module { alias })
            }
        };

        let span = start.to(self.prev_span());
        self.expect_stmt_end()?;
        Ok(ImportDecl {
            is_public,
            source,
            kind,
            span,
        })
    }

    fn parse_dotted_path(&mut self, what: &str) -> PResult<Vec<String>> {
        let mut segs = Vec::new();
        loop {
            segs.push(self.expect_ident(what)?);
            if self.at(&Tok::Dot) {
                self.bump();
            } else {
                break;
            }
        }
        Ok(segs)
    }

    fn recover_to_item(&mut self) {
        let mut depth = 0usize;
        loop {
            match self.kind().clone() {
                Tok::Eof => break,
                Tok::LBrace => {
                    depth += 1;
                    self.bump();
                }
                Tok::RBrace => {
                    if depth == 0 {
                        // Stray closing brace at top level: consume it so we
                        // make forward progress rather than looping forever.
                        self.bump();
                        break;
                    }
                    depth -= 1;
                    self.bump();
                }
                Tok::Newline if depth == 0 => {
                    self.bump();
                    while self.at(&Tok::Newline) {
                        self.bump();
                    }
                    if self.starts_item() {
                        break;
                    }
                }
                Tok::Fn
                | Tok::Class
                | Tok::Struct
                | Tok::Enum
                | Tok::Interface
                | Tok::Const
                | Tok::Hash
                    if depth == 0 =>
                {
                    break;
                }
                _ => {
                    self.bump();
                }
            }
        }
    }

    fn starts_item(&self) -> bool {
        matches!(
            self.kind(),
            Tok::Fn
                | Tok::Class
                | Tok::Struct
                | Tok::Enum
                | Tok::Interface
                | Tok::Const
                | Tok::Public
                | Tok::Private
                | Tok::Protected
                | Tok::Hash
        ) || self.is_test_fn_ahead()
    }

    fn is_test_fn_ahead(&self) -> bool {
        matches!(self.kind(), Tok::Ident(n) if n == "test")
            && matches!(self.peek_non_nl(1), Some(Tok::Fn))
    }

    /// `test(...)` / `it(...)` followed by a call: the callable test form.
    fn is_test_call_ahead(&self) -> bool {
        matches!(self.kind(), Tok::Ident(n) if n == "test" || n == "it")
            && matches!(self.peek_non_nl(1), Some(Tok::LParen))
    }

    /// `describe("group", { ... })`: a group of tests/hooks.
    fn is_describe_call_ahead(&self) -> bool {
        matches!(self.kind(), Tok::Ident(n) if n == "describe")
            && matches!(self.peek_non_nl(1), Some(Tok::LParen))
    }

    /// `beforeAll({ ... })` / `afterAll({ ... })` / `beforeEach` / `afterEach`
    /// as a top-level (root-group) hook declaration.
    fn is_hook_call_ahead(&self) -> bool {
        matches!(
            self.kind(),
            Tok::Ident(n) if n == "beforeAll" || n == "afterAll" || n == "beforeEach" || n == "afterEach"
        ) && matches!(self.peek_non_nl(1), Some(Tok::LParen))
    }

    /// Parse `("description", { body })` for the `test`/`it` item. The body is
    /// a block expression which becomes the test's statement block (any
    /// trailing value is treated as a discarded statement).
    fn parse_test_call_item(&mut self) -> PResult<ItemKind> {
        let start = self.span();
        self.bump(); // `test` / `it`
        self.expect(&Tok::LParen)?;
        let desc = self.parse_test_description()?;
        self.expect(&Tok::Comma)?;
        self.newlines();
        let mut block = self.parse_block()?;
        self.newlines();
        self.expect(&Tok::RParen)?;
        if let Some(tail) = block.expr.take() {
            block.stmts.push(Stmt::Expr(*tail));
        }
        let span = start.to(self.prev_span());
        Ok(ItemKind::Test(FnDecl {
            name: desc,
            span,
            visibility: Visibility::Default,
            is_async: false,
            generics: Vec::new(),
            params: Vec::new(),
            return_ty: None,
            body: Some(FnBody::Block(Box::new(block))),
        }))
    }

    fn parse_test_description(&mut self) -> PResult<String> {
        let start = self.span();
        let lit = match self.kind().clone() {
            Tok::Str(lit) => {
                self.bump();
                lit
            }
            other => {
                self.err_at(
                    start,
                    format!(
                        "test description must be a string literal, found {}",
                        other.describe()
                    ),
                );
                return Err(());
            }
        };
        let mut out = String::new();
        let mut interpolated = false;
        for seg in lit.segments {
            match seg {
                StrSeg::Text { text } => out.push_str(&text),
                StrSeg::Expr { tokens } => {
                    interpolated = true;
                    let _ = tokens;
                }
            }
        }
        if interpolated {
            self.err_at(start, "interpolation is not allowed in a test description");
        }
        Ok(out)
    }

    // ---------- items ----------

    /// Parse zero or more `#[name]` / `#[name(args)]` attributes. Attribute
    /// arguments are full expressions; newlines inside the brackets are
    /// suppressed by the lexer's nesting depth.
    fn parse_attributes(&mut self) -> PResult<Vec<Attribute>> {
        let mut out = Vec::new();
        while self.at(&Tok::Hash) {
            let start = self.span();
            self.bump();
            self.expect(&Tok::LBracket)?;
            let name = self.expect_ident("attribute name")?;
            let mut args = Vec::new();
            if self.eat(&Tok::LParen) {
                if !self.at(&Tok::RParen) {
                    loop {
                        args.push(self.parse_expr()?);
                        if self.eat(&Tok::Comma) {
                            if self.at(&Tok::RParen) {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                }
                self.expect(&Tok::RParen)?;
            }
            self.expect(&Tok::RBracket)?;
            out.push(Attribute {
                name,
                args,
                span: start.to(self.prev_span()),
            });
            // Attributes are conventionally one per line; any newline here
            // separates the attribute from the declaration it annotates.
            self.newlines();
        }
        Ok(out)
    }

    fn parse_item(&mut self) -> PResult<Vec<Item>> {
        let doc = Vec::new();
        let start = self.span();
        let attrs = self.parse_attributes()?;

        // `test("description", { ... })` / `it("description", { ... })` as a
        // top-level test declaration (the primary testing-framework form).
        if self.is_test_call_ahead() {
            let kind = self.parse_test_call_item()?;
            let span = start.to(self.prev_span());
            return Ok(vec![Item {
                doc,
                attrs,
                span,
                kind,
            }]);
        }

        // `describe("group", { ... })` desugars into the flat set of
        // prefixed test items and hidden hook functions it contains.
        if self.is_describe_call_ahead() {
            let items = self.parse_describe_item()?;
            return Ok(items);
        }

        // `beforeEach({ ... })` etc. at top level attach to the root group.
        if self.is_hook_call_ahead() {
            let kind = self.parse_hook_item()?;
            let span = start.to(self.prev_span());
            return Ok(vec![Item {
                doc,
                attrs,
                span,
                kind,
            }]);
        }

        let mut visibility = Visibility::Default;
        let mut is_test = false;

        loop {
            match self.kind().clone() {
                Tok::Public => {
                    self.bump();
                    visibility = Visibility::Public;
                }
                Tok::Private => {
                    self.bump();
                    visibility = Visibility::Private;
                }
                Tok::Protected => {
                    self.bump();
                    visibility = Visibility::Protected;
                }
                _ => break,
            }
        }

        if self.is_test_fn_ahead() {
            self.bump(); // `test`
            is_test = true;
        }

        let kind = match self.kind().clone() {
            Tok::Fn => {
                let mut f = self.parse_fn()?;
                f.visibility = visibility;
                if is_test {
                    ItemKind::Test(f)
                } else {
                    ItemKind::Fn(f)
                }
            }
            Tok::Class => {
                let mut c = self.parse_type_decl(true)?;
                c.visibility = visibility;
                ItemKind::Class(c)
            }
            Tok::Struct => {
                let mut c = self.parse_type_decl(false)?;
                c.visibility = visibility;
                let (extends, implements) = (c.extends, c.implements);
                if implements.is_empty() {
                    if let Some(ext) = extends {
                        self.err_at(
                            ext.span,
                            "structs cannot inherit from other types; use composition",
                        );
                    }
                }
                let s = StructDecl {
                    name: c.name,
                    span: c.span,
                    visibility: c.visibility,
                    generics: c.generics,
                    implements,
                    members: c.members,
                };
                let _ = &s;
                ItemKind::Struct(s)
            }
            Tok::Enum => {
                let mut e = self.parse_enum()?;
                e.visibility = visibility;
                ItemKind::Enum(e)
            }
            Tok::Interface => {
                let mut i = self.parse_interface()?;
                i.visibility = visibility;
                ItemKind::Interface(i)
            }
            Tok::Const => {
                let c = self.parse_const()?;
                ItemKind::Const(c)
            }
            _ => {
                self.err_here(format!(
                    "expected a declaration (class, fn, struct, enum, interface, const), found {}",
                    self.kind().describe()
                ));
                return Err(());
            }
        };
        let span = start.to(self.prev_span());
        Ok(vec![Item {
            doc,
            attrs,
            span,
            kind,
        }])
    }

    /// `describe("name", { items })`: parse the header, then desugar the body.
    /// Returns every test/hook/group item the block expands to.
    fn parse_describe_item(&mut self) -> PResult<Vec<Item>> {
        let start = self.span();
        self.bump(); // `describe`
        self.expect(&Tok::LParen)?;
        let desc = self.parse_test_description()?;
        self.expect(&Tok::Comma)?;
        self.newlines();
        let mut block = self.parse_block()?;
        self.newlines();
        self.expect(&Tok::RParen)?;
        if let Some(tail) = block.expr.take() {
            block.stmts.push(Stmt::Expr(*tail));
        }
        let span = start.to(self.prev_span());
        let mut out = Vec::new();
        self.desugar_describe_block(0, &[desc], &block, &mut out)?;
        let _ = span;
        Ok(out)
    }

    /// `beforeEach({ ... })` (or `beforeAll`/`afterEach`/`afterAll`) at the
    /// top level: a hidden hook function attached to the root group.
    fn parse_hook_item(&mut self) -> PResult<ItemKind> {
        let start = self.span();
        let hook = self.expect_ident("hook name")?;
        self.expect(&Tok::LParen)?;
        self.newlines();
        let mut block = self.parse_block()?;
        self.newlines();
        self.expect(&Tok::RParen)?;
        if let Some(tail) = block.expr.take() {
            block.stmts.push(Stmt::Expr(*tail));
        }
        let span = start.to(self.prev_span());
        let name = format!("__pkl_hook_{}_", hook_kind(&hook).unwrap_or("?"),);
        Ok(ItemKind::Fn(FnDecl {
            name,
            span,
            visibility: Visibility::Default,
            is_async: false,
            generics: Vec::new(),
            params: Vec::new(),
            return_ty: None,
            body: Some(FnBody::Block(Box::new(block))),
        }))
    }

    /// Expand a `describe` body into flat items. `depth` is the current nest
    /// level (each nested describe adds one); `path` is the group name path so
    /// far, `out` receives tests/hooks in group order.
    fn desugar_describe_block(
        &mut self,
        _depth: usize,
        path: &[String],
        block: &Block,
        out: &mut Vec<Item>,
    ) -> PResult<()> {
        for stmt in &block.stmts {
            let Stmt::Expr(e) = stmt else {
                self.err_at(
                    stmt_span(stmt),
                    "expected `test(...)`, `it(...)`, `describe(...)` or a hook in a describe body",
                );
                return Err(());
            };
            let Some((callee, args)) = call_parts(e) else {
                self.err_at(
                    e.span,
                    "expected `test(...)`, `it(...)`, `describe(...)` or a hook in a describe body",
                );
                return Err(());
            };
            match callee {
                "test" | "it" => {
                    let desc = self.describe_call_str(e, args, 0)?;
                    let b = match args.get(1).map(|a| block_of(&a.value)) {
                        Some(Some(b)) => b.clone(),
                        _ => {
                            self.err_at(
                                e.span,
                                format!("`{callee}()` needs a `(\"description\", {{ ... }})` body"),
                            );
                            return Err(());
                        }
                    };
                    let prefix = if path.is_empty() {
                        String::new()
                    } else {
                        format!("{} > ", path.join(" > "))
                    };
                    let full = format!("{prefix}{desc}");
                    out.push(Item {
                        doc: Vec::new(),
                        attrs: Vec::new(),
                        span: e.span,
                        kind: ItemKind::Test(FnDecl {
                            name: full,
                            span: e.span,
                            visibility: Visibility::Default,
                            is_async: false,
                            generics: Vec::new(),
                            params: Vec::new(),
                            return_ty: None,
                            body: Some(FnBody::Block(Box::new(b))),
                        }),
                    });
                }
                "describe" => {
                    let desc = self.describe_call_str(e, args, 0)?;
                    let mut b = match args.get(1).map(|a| block_of(&a.value)) {
                        Some(Some(b)) => b.clone(),
                        _ => {
                            self.err_at(
                                e.span,
                                "`describe()` needs a `(\"name\", {{ ... }})` body",
                            );
                            return Err(());
                        }
                    };
                    // The block's final statement is parsed as its tail
                    // expression; flatten it back so it is desugared too.
                    if let Some(tail) = b.expr.take() {
                        b.stmts.push(Stmt::Expr(*tail));
                    }
                    let mut nested = Vec::new();
                    let mut path = path.to_vec();
                    path.push(desc);
                    self.desugar_describe_block(0, &path, &b, &mut nested)?;
                    out.extend(nested);
                    continue;
                }
                "beforeAll" | "beforeEach" | "afterEach" | "afterAll" => {
                    self.desugar_hook(e, args, path, out)?;
                }
                _ => {
                    self.err_at(
                        e.span,
                        format!(
                            "unsupported statement in a describe body: `{callee}(...)` (expected test/it/describe or a hook)"
                        ),
                    );
                    return Err(());
                }
            }
        }
        Ok(())
    }

    /// Extract a hook call inside a group into a hidden function item whose
    /// mangled name encodes `(kind, group path)` for the test runner.
    fn desugar_hook(
        &mut self,
        e: &Expr,
        args: &[CallArg],
        path: &[String],
        out: &mut Vec<Item>,
    ) -> PResult<()> {
        let hook = match &e.kind {
            ExprKind::Call { callee, .. } => match &callee.kind {
                ExprKind::Ident(n) => n.clone(),
                _ => unreachable!(),
            },
            _ => unreachable!(),
        };
        let Some(kind) = hook_kind(&hook) else {
            self.err_at(e.span, format!("unknown hook `{hook}`"));
            return Err(());
        };
        let b = match args.first().map(|a| block_of(&a.value)) {
            Some(Some(b)) => b.clone(),
            _ => {
                self.err_at(e.span, format!("`{hook}()` needs a `{{ ... }}` body block"));
                return Err(());
            }
        };
        let mut name = format!("__pkl_hook_{kind}_",);
        name.push_str(&path.join(" > "));
        out.push(Item {
            doc: Vec::new(),
            attrs: Vec::new(),
            span: e.span,
            kind: ItemKind::Fn(FnDecl {
                name,
                span: e.span,
                visibility: Visibility::Default,
                is_async: false,
                generics: Vec::new(),
                params: Vec::new(),
                return_ty: None,
                body: Some(FnBody::Block(Box::new(b))),
            }),
        });
        Ok(())
    }

    /// Read the description string literal argument of a nested
    /// `test`/`it`/`describe` call, rejecting interpolated literals.
    fn describe_call_str(&mut self, e: &Expr, args: &[CallArg], index: usize) -> PResult<String> {
        let a = args.get(index);
        match a.and_then(|a| str_literal_of(&a.value)) {
            Some(segments) => {
                let mut out = String::new();
                for seg in segments {
                    match seg {
                        StrPart::Text(text) => out.push_str(text),
                        StrPart::Expr(_) => {
                            self.err_at(
                                e.span,
                                "interpolation is not allowed in a test description",
                            );
                            return Err(());
                        }
                    }
                }
                Ok(out)
            }
            None => {
                self.err_at(e.span, "test description must be a string literal");
                Err(())
            }
        }
    }

    fn parse_fn(&mut self) -> PResult<FnDecl> {
        let start = self.span();
        self.expect(&Tok::Fn)?;
        let name = self.expect_fn_name()?;
        let generics = self.parse_generic_params()?;
        let params = self.parse_param_list()?;
        let return_ty = self.parse_optional_return_type()?;
        let body = self.parse_fn_body()?;
        let span = start.to(self.prev_span());
        Ok(FnDecl {
            name,
            span,
            visibility: Visibility::Default,
            is_async: false,
            generics,
            params,
            return_ty,
            body: Some(body),
        })
    }

    fn parse_optional_return_type(&mut self) -> PResult<Option<TypeExpr>> {
        if self.eat(&Tok::Arrow) {
            return Ok(Some(self.parse_type()?));
        }
        // Legacy `: Type` form is tolerated for interfaces written that way.
        if self.eat(&Tok::Colon) {
            return Ok(Some(self.parse_type()?));
        }
        Ok(None)
    }

    fn expect_fn_name(&mut self) -> PResult<String> {
        match self.kind().clone() {
            Tok::Ident(s) => {
                self.bump();
                Ok(s)
            }
            other => {
                if let Some(kw) = contextual_ident(&other) {
                    self.bump();
                    return Ok(kw.to_string());
                }
                self.err_at(
                    self.span(),
                    format!("expected a function name, found {}", other.describe()),
                );
                Err(())
            }
        }
    }

    fn parse_generic_params(&mut self) -> PResult<Vec<GenericParam>> {
        if !self.at(&Tok::Lt) {
            return Ok(Vec::new());
        }
        self.bump();
        let mut out = Vec::new();
        loop {
            let name = self.expect_ident("type parameter")?;
            let span = self.prev_span();
            out.push(GenericParam { name, span });
            if self.at(&Tok::Comma) {
                self.bump();
                self.newlines();
            } else {
                break;
            }
        }
        self.consume_gt(1)?;
        Ok(out)
    }

    fn parse_param_list(&mut self) -> PResult<Vec<Param>> {
        self.expect(&Tok::LParen)?;
        let mut params = Vec::new();
        if self.at(&Tok::RParen) {
            self.bump();
            return Ok(params);
        }
        loop {
            let start = self.span();
            let attrs = self.parse_attributes()?;
            let rest = self.eat(&Tok::Ellipsis);
            let name = self.expect_ident("parameter")?;
            let ty = if self.eat(&Tok::Colon) {
                Some(self.parse_type()?)
            } else {
                None
            };
            let default = if self.eat(&Tok::Assign) {
                Some(self.parse_expr()?)
            } else {
                None
            };
            params.push(Param {
                name,
                ty,
                default,
                rest,
                attrs,
                span: start.to(self.prev_span()),
            });
            if self.eat(&Tok::Comma) {
                self.continuation_after(&Tok::Comma);
                if self.at(&Tok::RParen) {
                    break;
                }
            } else {
                break;
            }
        }
        self.expect(&Tok::RParen)?;
        Ok(params)
    }

    fn parse_fn_body(&mut self) -> PResult<FnBody> {
        if self.eat(&Tok::FatArrow) {
            let e = self.parse_expr()?;
            return Ok(FnBody::Expr(Box::new(e)));
        }
        if self.at(&Tok::LBrace) {
            let b = self.parse_block()?;
            return Ok(FnBody::Block(Box::new(b)));
        }
        // No body: abstract declaration (interfaces, abstract classes).
        Ok(FnBody::Block(Box::new(Block {
            stmts: Vec::new(),
            expr: None,
            span: self.span(),
        })))
    }

    fn parse_block(&mut self) -> PResult<Block> {
        let start = self.span();
        self.expect(&Tok::LBrace)?;
        let mut stmts = Vec::new();
        let mut expr = None;
        self.newlines();
        loop {
            if self.at(&Tok::RBrace) || self.at(&Tok::Eof) {
                break;
            }
            let s = self.parse_stmt()?;
            // The block tail is the final expression before the closing brace.
            // It may sit directly at `}` or be separated from it by a newline.
            // Peek past newlines (without consuming) so a trailing `expr\n}`
            // still counts as the block's value instead of being dropped as a
            // discarded statement.
            let is_tail = self.at(&Tok::RBrace)
                || self.at(&Tok::Eof)
                || matches!(self.peek_non_nl(1), None | Some(Tok::RBrace));
            if is_tail {
                self.newlines();
                match s {
                    Stmt::Expr(e) => {
                        expr = Some(Box::new(e));
                    }
                    s => stmts.push(s),
                }
                break;
            }
            self.expect_stmt_end()?;
            stmts.push(s);
        }
        self.expect(&Tok::RBrace)?;
        let span = start.to(self.prev_span());
        Ok(Block { stmts, expr, span })
    }

    fn parse_stmt(&mut self) -> PResult<Stmt> {
        let attrs = self.parse_attributes()?;
        let mut stmt = self.parse_stmt_inner()?;
        if !attrs.is_empty() {
            match &mut stmt {
                Stmt::Let { attrs: slot, .. } => *slot = attrs,
                _ => self.err_here("attributes are only supported on `let`/`var` bindings"),
            }
        }
        Ok(stmt)
    }

    fn parse_stmt_inner(&mut self) -> PResult<Stmt> {
        match self.kind().clone() {
            Tok::Let | Tok::Var => {
                let mutable = self.at(&Tok::Var);
                let span_start = self.span();
                self.bump();
                let pat = self.parse_pattern()?;
                let ty = if self.eat(&Tok::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                let init = if self.eat(&Tok::Assign) {
                    self.continuation_after(&Tok::Assign);
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                let span = span_start.to(self.prev_span());
                Ok(Stmt::Let {
                    pattern: pat,
                    ty,
                    init,
                    mutable,
                    attrs: Vec::new(),
                    span,
                })
            }
            Tok::Const => {
                let start = self.span();
                self.bump();
                let name = self.expect_ident("constant name")?;
                let ty = if self.eat(&Tok::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                self.expect(&Tok::Assign)?;
                self.continuation_after(&Tok::Assign);
                let value = self.parse_expr()?;
                let span = start.to(self.prev_span());
                Ok(Stmt::Const {
                    name,
                    ty,
                    value,
                    span,
                })
            }
            Tok::Return => {
                let start = self.span();
                self.bump();
                let value = if self.at(&Tok::Newline) || self.at(&Tok::RBrace) || self.at(&Tok::Eof)
                {
                    None
                } else {
                    Some(self.parse_expr()?)
                };
                let span = start.to(self.prev_span());
                Ok(Stmt::Return { value, span })
            }
            Tok::Break => {
                let start = self.span();
                self.bump();
                let span = start.to(self.prev_span());
                Ok(Stmt::Break { span })
            }
            Tok::Continue => {
                let start = self.span();
                self.bump();
                let span = start.to(self.prev_span());
                Ok(Stmt::Continue { span })
            }
            Tok::While => self.parse_while_stmt(),
            Tok::For => self.parse_for_stmt(),
            Tok::Semicolon => {
                let span = self.span();
                self.bump();
                Ok(Stmt::Empty(span))
            }
            Tok::Newline => {
                let span = self.span();
                self.bump();
                Ok(Stmt::Empty(span))
            }
            _ => {
                let e = self.parse_expr()?;
                Ok(Stmt::Expr(e))
            }
        }
    }

    fn parse_while_stmt(&mut self) -> PResult<Stmt> {
        let start = self.span();
        self.expect(&Tok::While)?;
        self.expect(&Tok::LParen)?;
        let cond = self.parse_expr()?;
        self.expect(&Tok::RParen)?;
        let body = self.parse_block()?;
        let span = start.to(self.prev_span());
        Ok(Stmt::While {
            cond: Box::new(cond),
            body: Box::new(body),
            span,
        })
    }

    fn parse_for_stmt(&mut self) -> PResult<Stmt> {
        let start = self.span();
        self.expect(&Tok::For)?;
        self.expect(&Tok::LParen)?;

        let header = if self.scan_for_header_is_iteration() {
            let pattern = self.parse_pattern()?;
            self.expect(&Tok::In)?;
            let sequence = self.parse_expr()?;
            ForHeader::In { pattern, sequence }
        } else {
            let init = self.parse_stmt()?;
            self.expect(&Tok::Semicolon)?;
            if self.at(&Tok::Newline) {
                self.newlines();
            }
            let cond = self.parse_expr()?;
            self.expect(&Tok::Semicolon)?;
            if self.at(&Tok::Newline) {
                self.newlines();
            }
            let step = self.parse_expr()?;
            ForHeader::Range {
                init: Box::new(init),
                cond: Box::new(cond),
                step: Box::new(step),
            }
        };
        self.expect(&Tok::RParen)?;
        let body = self.parse_block()?;
        let span = start.to(self.prev_span());
        Ok(Stmt::For {
            header,
            body: Box::new(body),
            span,
        })
    }

    fn scan_for_header_is_iteration(&self) -> bool {
        // Look for a top-level `in` token inside the for header. By the time
        // this runs, `for (` are consumed, so the first remaining token may
        // open a tuple-pattern group `((k, v) in m)` — that group's parens
        // nest inward and must NOT be mistaken for the header closer.
        let mut depth = 0usize;
        let mut i = self.pos;
        let toks = &self.tokens;
        loop {
            let t = &toks[i.min(toks.len() - 1)];
            match &t.token.kind {
                Tok::LParen | Tok::LBracket | Tok::LBrace => depth += 1,
                Tok::RParen | Tok::RBracket | Tok::RBrace => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                Tok::In => return true,
                Tok::Newline if depth == 0 => return false,
                Tok::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        false
    }

    fn parse_const(&mut self) -> PResult<ConstDecl> {
        let start = self.span();
        self.expect(&Tok::Const)?;
        let name = self.expect_ident("constant name")?;
        let ty = if self.eat(&Tok::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        self.expect(&Tok::Assign)?;
        self.continuation_after(&Tok::Assign);
        let value = self.parse_expr()?;
        let span = start.to(self.prev_span());
        self.expect_stmt_end()?;
        Ok(ConstDecl {
            name,
            visibility: Visibility::Default,
            ty,
            value,
            span,
        })
    }

    // ---------- types ----------

    fn parse_type(&mut self) -> PResult<TypeExpr> {
        let start = self.span();
        let mut ty = self.parse_type_prefix()?;
        while self.at(&Tok::Question) {
            self.bump();
            let span = start.to(self.prev_span());
            ty = TypeExpr {
                span,
                kind: TypeExprKind::Option(Box::new(ty)),
            };
        }
        Ok(ty)
    }

    fn parse_type_prefix(&mut self) -> PResult<TypeExpr> {
        let start = self.span();
        match self.kind().clone() {
            Tok::Star => {
                self.bump();
                let inner = self.parse_type()?;
                let span = start.to(inner.span);
                Ok(TypeExpr {
                    span,
                    kind: TypeExprKind::Pointer(Box::new(inner)),
                })
            }
            Tok::Amp => {
                self.bump();
                let inner = self.parse_type()?;
                let span = start.to(inner.span);
                Ok(TypeExpr {
                    span,
                    kind: TypeExprKind::Ref(Box::new(inner)),
                })
            }
            Tok::Fn => {
                self.bump();
                self.expect(&Tok::LParen)?;
                let mut pts = Vec::new();
                if !self.at(&Tok::RParen) {
                    loop {
                        pts.push(self.parse_type()?);
                        if self.eat(&Tok::Comma) {
                            self.continuation_after(&Tok::Comma);
                        } else {
                            break;
                        }
                    }
                }
                self.expect(&Tok::RParen)?;
                let is_async = false;
                let ret = if self.eat(&Tok::Arrow) || self.eat(&Tok::Colon) {
                    Some(Box::new(self.parse_type()?))
                } else {
                    None
                };
                let span = start.to(self.prev_span());
                Ok(TypeExpr {
                    span,
                    kind: TypeExprKind::Fn {
                        params: pts,
                        ret,
                        is_async,
                    },
                })
            }
            Tok::LParen => {
                self.bump();
                let mut parts = Vec::new();
                if !self.at(&Tok::RParen) {
                    loop {
                        parts.push(self.parse_type()?);
                        if self.eat(&Tok::Comma) {
                            self.continuation_after(&Tok::Comma);
                            if self.at(&Tok::RParen) {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                }
                self.expect(&Tok::RParen)?;
                if self.at(&Tok::Arrow) {
                    self.bump();
                    let ret = self.parse_type()?;
                    let span = start.to(self.prev_span());
                    return Ok(TypeExpr {
                        span,
                        kind: TypeExprKind::Fn {
                            params: parts,
                            ret: Some(Box::new(ret)),
                            is_async: false,
                        },
                    });
                }
                if parts.len() == 1 {
                    let span = start.to(self.prev_span());
                    let k = parts.pop().unwrap().kind;
                    Ok(TypeExpr { span, kind: k })
                } else {
                    let span = start.to(self.prev_span());
                    Ok(TypeExpr {
                        span,
                        kind: TypeExprKind::Tuple(parts),
                    })
                }
            }
            Tok::Ident(s) if is_wildcard_ident(&s) => {
                self.bump();
                Ok(TypeExpr {
                    span: start,
                    kind: TypeExprKind::Infer,
                })
            }
            Tok::Ident(_) => {
                let mut path = Vec::new();
                let span0 = start;
                loop {
                    path.push(self.expect_ident("type name")?);
                    if self.at(&Tok::Dot) {
                        self.bump();
                    } else {
                        break;
                    }
                }
                let span = span0.to(self.prev_span());
                if self.at(&Tok::Lt) {
                    let args = self.parse_type_args()?;
                    let base = TypeExpr {
                        span,
                        kind: TypeExprKind::Path(path),
                    };
                    let span2 = span0.to(self.prev_span());
                    Ok(TypeExpr {
                        span: span2,
                        kind: TypeExprKind::Generic(Box::new(base), args),
                    })
                } else {
                    Ok(TypeExpr {
                        span,
                        kind: TypeExprKind::Path(path),
                    })
                }
            }
            other => {
                self.err_at(
                    start,
                    format!("expected a type, found {}", other.describe()),
                );
                Err(())
            }
        }
    }

    fn parse_type_args(&mut self) -> PResult<Vec<TypeExpr>> {
        self.expect(&Tok::Lt)?;
        let mut args = Vec::new();
        if self.at(&Tok::Gt) {
            self.bump();
            return Ok(args);
        }
        loop {
            args.push(self.parse_type()?);
            if self.at(&Tok::Comma) {
                self.bump();
                self.continuation_after(&Tok::Comma);
                if self.at(&Tok::Gt) {
                    self.bump();
                    break;
                }
                if self.at(&Tok::Shr) {
                    // `List<Map<K, V>,>` : `>>` lexes as one token.
                    self.split_shift_as_two_gt();
                    self.bump();
                    break;
                }
            } else if self.at(&Tok::Gt) {
                self.bump();
                break;
            } else if self.at(&Tok::Shr) {
                self.split_shift_as_two_gt();
                self.bump();
                break;
            } else {
                self.err_here(format!(
                    "expected `>` or `,` in type arguments, found {}",
                    self.kind().describe()
                ));
                return Err(());
            }
        }
        Ok(args)
    }

    /// In type-argument position a right shift `>>` means *two* closings
    /// (`List<Map<K, V>>`). Replace the current `Shr` token with a single
    /// `>` and inject a second `>` right after it, so each `parse_type_args`
    /// invocation (one nesting level) consumes exactly one closing and the
    /// leftover `>` is what the outer invocation sees next.
    fn split_shift_as_two_gt(&mut self) {
        if !matches!(self.kind(), Tok::Shr) {
            return;
        }
        let span = self.span();
        let t = &mut self.tokens[self.pos];
        t.token = crate::token::Token::new(Tok::Gt, span);
        self.tokens
            .insert(self.pos + 1, LexedToken::new(Tok::Gt, span));
    }

    fn consume_gt(&mut self, n: u32) -> PResult<()> {
        let mut remaining = n;
        while remaining > 0 {
            match self.kind().clone() {
                Tok::Gt => {
                    self.bump();
                    remaining -= 1;
                }
                Tok::Shr => self.split_shift_as_two_gt(),
                other => {
                    self.err_here(format!("expected `>` or `>>`, found {}", other.describe()));
                    return Err(());
                }
            }
        }
        Ok(())
    }

    // ---------- patterns ----------
    fn parse_pattern(&mut self) -> PResult<Pattern> {
        let p = self.parse_pattern_inner()?;
        if self.at(&Tok::Pipe) {
            let mut alts = vec![p];
            while self.eat(&Tok::Pipe) {
                alts.push(self.parse_pattern_inner()?);
            }
            return Ok(Pattern::Or(alts));
        }
        Ok(p)
    }

    fn parse_pattern_inner(&mut self) -> PResult<Pattern> {
        match self.kind().clone() {
            Tok::Ident(s) if is_wildcard_ident(&s) => {
                self.bump();
                Ok(Pattern::Wildcard)
            }
            Tok::Number => {
                let t = self.advance();
                let kind = if t.data.is_float {
                    Lit::Float {
                        value: t.data.float.unwrap_or(0.0),
                    }
                } else {
                    Lit::Int {
                        value: t.data.int.unwrap_or(0),
                    }
                };
                Ok(Pattern::Literal(kind))
            }
            Tok::Str(_) => {
                let t = self.advance();
                match t.token.kind {
                    Tok::Str(lit) => Ok(Pattern::Literal(Lit::String(self.string_parts(lit)?))),
                    _ => unreachable!(),
                }
            }
            Tok::Char(_) => {
                let t = self.advance();
                match t.token.kind {
                    Tok::Char(c) => Ok(Pattern::Literal(Lit::Char(c))),
                    _ => unreachable!(),
                }
            }
            Tok::True => {
                self.bump();
                Ok(Pattern::Literal(Lit::Bool(true)))
            }
            Tok::False => {
                self.bump();
                Ok(Pattern::Literal(Lit::Bool(false)))
            }
            Tok::None => {
                self.bump();
                Ok(Pattern::Literal(Lit::None))
            }
            Tok::Minus => {
                self.bump();
                let t = self.advance();
                let kind = if t.data.is_float {
                    Lit::Float {
                        value: -t.data.float.unwrap_or(0.0),
                    }
                } else {
                    Lit::Int {
                        value: -t.data.int.unwrap_or(0),
                    }
                };
                Ok(Pattern::Literal(kind))
            }
            Tok::LParen => {
                self.bump();
                let mut parts = Vec::new();
                if !self.at(&Tok::RParen) {
                    loop {
                        parts.push(self.parse_pattern()?);
                        if self.eat(&Tok::Comma) {
                            self.continuation_after(&Tok::Comma);
                            if self.at(&Tok::RParen) {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                }
                self.expect(&Tok::RParen)?;
                if parts.len() == 1 {
                    Ok(parts.pop().unwrap())
                } else {
                    Ok(Pattern::Tuple(parts))
                }
            }
            other if matches!(&other, Tok::Ident(_)) || contextual_ident(&other).is_some() => {
                let mut path = Vec::new();
                path.push(self.expect_ident("pattern")?);
                while self.at(&Tok::Dot) {
                    self.bump();
                    path.push(self.expect_ident("pattern path")?);
                }
                if self.at(&Tok::LParen) {
                    self.bump();
                    let mut payloads = Vec::new();
                    if !self.at(&Tok::RParen) {
                        loop {
                            payloads.push(self.parse_pattern()?);
                            if self.eat(&Tok::Comma) {
                                self.continuation_after(&Tok::Comma);
                                if self.at(&Tok::RParen) {
                                    break;
                                }
                            } else {
                                break;
                            }
                        }
                    }
                    self.expect(&Tok::RParen)?;
                    Ok(Pattern::Variant { path, payloads })
                } else if self.at(&Tok::Colon) {
                    self.bump();
                    let ty = self.parse_type()?;
                    Ok(Pattern::Binding {
                        name: path[0].clone(),
                        ty: Some(ty),
                    })
                } else if path.len() > 1 {
                    Ok(Pattern::Variant {
                        path,
                        payloads: Vec::new(),
                    })
                } else {
                    Ok(Pattern::Binding {
                        name: path[0].clone(),
                        ty: None,
                    })
                }
            }
            other => {
                self.err_at(
                    self.span(),
                    format!("expected a pattern, found {}", other.describe()),
                );
                Err(())
            }
        }
    }

    // ---------- expressions ----------

    /// Convert a lexed string literal into parsed parts, re-parsing each
    /// interpolation segment with a nested parser bound to this source's
    /// diagnostics.
    fn string_parts(&mut self, lit: StrLit) -> PResult<Vec<StrPart>> {
        let mut parts = Vec::new();
        for seg in lit.segments {
            match seg {
                StrSeg::Text { text } => parts.push(StrPart::Text(text)),
                StrSeg::Expr { tokens } => {
                    let mut sub = Parser::new(tokens, self.diags);
                    parts.push(StrPart::Expr(sub.parse_expr()?));
                }
            }
        }
        Ok(parts)
    }

    fn parse_expr(&mut self) -> PResult<Expr> {
        let lhs = self.parse_binary(0)?;
        let op = match self.kind().clone() {
            Tok::Assign => AssignOp::Assign,
            Tok::PlusEq => AssignOp::Add,
            Tok::MinusEq => AssignOp::Sub,
            Tok::StarEq => AssignOp::Mul,
            Tok::SlashEq => AssignOp::Div,
            Tok::PercentEq => AssignOp::Mod,
            Tok::ShlEq => AssignOp::Shl,
            Tok::ShrEq => AssignOp::Shr,
            Tok::AndEq => AssignOp::BitAnd,
            Tok::OrEq => AssignOp::BitOr,
            Tok::XorEq => AssignOp::BitXor,
            _ => return Ok(lhs),
        };
        let op_tok = AssignOp::as_tok(&op);
        self.bump();
        self.continuation_after(&op_tok);
        let value = self.parse_expr()?;
        let span = lhs.span.to(value.span);
        Ok(Expr {
            span,
            kind: ExprKind::Assign {
                target: Box::new(lhs),
                op,
                value: Box::new(value),
            },
        })
    }

    fn parse_binary(&mut self, min_prec: u8) -> PResult<Expr> {
        let mut lhs = self.parse_unary()?;
        loop {
            if self.at(&Tok::Newline) && self.next_begins_continuation() {
                self.newlines();
            }
            // Casts are binary but parse RHS as a type.
            if matches!(self.kind(), Tok::As | Tok::Is) {
                let is_as = self.at(&Tok::As);
                let op_span = self.span();
                self.bump();
                let cast_kind = if is_as && self.at(&Tok::Question) {
                    self.bump();
                    CastKind::TryAs
                } else if is_as {
                    CastKind::As
                } else {
                    CastKind::Is
                };
                let ty = self.parse_type()?;
                let span = lhs.span.to(op_span).to(ty.span);
                lhs = Expr {
                    span,
                    kind: ExprKind::Cast {
                        expr: Box::new(lhs),
                        ty,
                        kind: cast_kind,
                    },
                };
                continue;
            }
            let t = self.kind().clone();
            let Some((prec, op)) = binop_of(&t) else {
                break;
            };
            if prec < min_prec {
                break;
            }
            let opkind = t.clone();
            self.bump();
            self.continuation_after(&opkind);
            let right_min = if op == BinOp::Pow { prec } else { prec + 1 };
            let rhs = self.parse_binary(right_min)?;
            lhs = Expr {
                span: lhs.span.to(rhs.span),
                kind: ExprKind::Binary {
                    op,
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
            };
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> PResult<Expr> {
        let start = self.span();
        let op = match self.kind().clone() {
            Tok::Minus => Some(UnOp::Neg),
            Tok::Bang => Some(UnOp::Not),
            Tok::Tilde => Some(UnOp::BitNot),
            Tok::Star => Some(UnOp::Deref),
            Tok::Amp => Some(UnOp::AddrOf),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let operand = self.parse_unary()?;
            let span = start.to(operand.span);
            return Ok(Expr {
                span,
                kind: ExprKind::Unary {
                    op,
                    operand: Box::new(operand),
                },
            });
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> PResult<Expr> {
        let mut e = self.parse_primary()?;
        loop {
            if self.at(&Tok::Newline) && self.next_begins_continuation() {
                self.newlines();
            }
            let span_start = e.span;
            match self.kind().clone() {
                Tok::Dot => {
                    self.bump();
                    let name = self.expect_ident("member name")?;
                    let span = span_start.to(self.prev_span());
                    e = Expr {
                        span,
                        kind: ExprKind::Member {
                            object: Box::new(e),
                            name,
                        },
                    };
                }
                Tok::QuestionDot => {
                    self.bump();
                    let name = self.expect_ident("member name")?;
                    let span = span_start.to(self.prev_span());
                    e = Expr {
                        span,
                        kind: ExprKind::OptAccess {
                            object: Box::new(e),
                            name,
                        },
                    };
                }
                Tok::Question => {
                    self.bump();
                    let span = span_start.to(self.prev_span());
                    e = Expr {
                        span,
                        kind: ExprKind::OptUnwrap(Box::new(e)),
                    };
                }
                Tok::LParen => {
                    let args = self.parse_call_args_internal()?;
                    let span = span_start.to(self.prev_span());
                    e = Expr {
                        span,
                        kind: ExprKind::Call {
                            callee: Box::new(e),
                            args,
                        },
                    };
                }
                Tok::LBracket => {
                    self.bump();
                    let idx = self.parse_expr()?;
                    self.expect(&Tok::RBracket)?;
                    let span = span_start.to(self.prev_span());
                    e = Expr {
                        span,
                        kind: ExprKind::Index {
                            object: Box::new(e),
                            index: Box::new(idx),
                        },
                    };
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn parse_call_args_internal(&mut self) -> PResult<Vec<CallArg>> {
        self.expect(&Tok::LParen)?;
        let mut args = Vec::new();
        if self.at(&Tok::RParen) {
            self.bump();
            return Ok(args);
        }
        loop {
            let start = self.span();
            let spread = self.eat(&Tok::Ellipsis);
            let name = if self.peek_is_named_arg() {
                let n = self.expect_ident("argument name")?;
                self.expect(&Tok::Colon)?;
                Some(n)
            } else {
                None
            };
            let value = self.parse_expr()?;
            args.push(CallArg {
                name,
                value,
                spread,
                span: start.to(self.prev_span()),
            });
            if self.eat(&Tok::Comma) {
                self.continuation_after(&Tok::Comma);
                if self.at(&Tok::RParen) {
                    break;
                }
            } else {
                break;
            }
        }
        self.expect(&Tok::RParen)?;
        Ok(args)
    }

    fn peek_is_named_arg(&self) -> bool {
        if !matches!(self.kind(), Tok::Ident(_)) {
            return false;
        }
        matches!(self.peek_at(1), Tok::Colon)
    }

    fn parse_primary(&mut self) -> PResult<Expr> {
        let start = self.span();
        match self.kind().clone() {
            Tok::Number => {
                let t = self.advance();
                let kind = if t.data.is_float
                    || t.data
                        .suffix
                        .as_deref()
                        .map(|s| s.starts_with('f'))
                        .unwrap_or(false)
                {
                    ExprKind::Lit(Lit::Float {
                        value: t.data.float.unwrap_or(0.0),
                    })
                } else {
                    ExprKind::Lit(Lit::Int {
                        value: t.data.int.unwrap_or(0),
                    })
                };
                Ok(Expr {
                    span: t.token.span,
                    kind,
                })
            }
            Tok::Str(_) => {
                let t = self.advance();
                match t.token.kind {
                    Tok::Str(lit) => Ok(Expr {
                        span: t.token.span,
                        kind: ExprKind::Lit(Lit::String(self.string_parts(lit)?)),
                    }),
                    _ => unreachable!(),
                }
            }
            Tok::Char(_) => {
                let t = self.advance();
                match t.token.kind {
                    Tok::Char(c) => Ok(Expr {
                        span: t.token.span,
                        kind: ExprKind::Lit(Lit::Char(c)),
                    }),
                    _ => unreachable!(),
                }
            }
            Tok::True => {
                self.bump();
                Ok(Expr {
                    span: start,
                    kind: ExprKind::Lit(Lit::Bool(true)),
                })
            }
            Tok::False => {
                self.bump();
                Ok(Expr {
                    span: start,
                    kind: ExprKind::Lit(Lit::Bool(false)),
                })
            }
            Tok::None => {
                self.bump();
                Ok(Expr {
                    span: start,
                    kind: ExprKind::Lit(Lit::None),
                })
            }
            Tok::This => {
                self.bump();
                Ok(Expr {
                    span: start,
                    kind: ExprKind::This,
                })
            }
            Tok::Super => {
                self.bump();
                Ok(Expr {
                    span: start,
                    kind: ExprKind::Super,
                })
            }
            Tok::If => self.parse_if(),
            Tok::Match => self.parse_match(),
            Tok::Unsafe => {
                self.bump();
                let body = self.parse_block()?;
                let span = start.to(self.prev_span());
                Ok(Expr {
                    span,
                    kind: ExprKind::Unsafe(Box::new(body)),
                })
            }
            Tok::Await => {
                self.bump();
                let inner = self.parse_unary()?;
                let span = start.to(inner.span);
                Ok(Expr {
                    span,
                    kind: ExprKind::Await(Box::new(inner)),
                })
            }
            Tok::Task => {
                // `task { body }` — sugar for `thread_spawn(fn() { body })`,
                // desugared here so check and emit see an ordinary spawn call
                // and the task carries its state exactly as a hand-written
                // closure does (the block becomes the closure body, captures
                // and all). The `{` may sit on the next line, but a bare
                // `task` that is not followed by a block stays the contextual
                // keyword used as a plain identifier.
                if self.peek_non_nl(1) == Some(Tok::LBrace) {
                    self.bump();
                    self.newlines();
                    let body = self.parse_block()?;
                    let end = self.prev_span();
                    // Distinct spans so the checker's per-span type map keeps
                    // the closure's `Fn` entry separate from the call's:
                    // the lambda spans its own `{...}` block, the callee the
                    // bare `task` token, and the call the whole expression.
                    let closure = Expr {
                        span: body.span,
                        kind: ExprKind::Lambda {
                            params: Vec::new(),
                            return_ty: None,
                            is_async: false,
                            body: FnBody::Block(Box::new(body)),
                        },
                    };
                    return Ok(Expr {
                        span: start.to(end),
                        kind: ExprKind::Call {
                            callee: Box::new(Expr {
                                span: start,
                                kind: ExprKind::Ident("thread_spawn".to_string()),
                            }),
                            args: vec![CallArg {
                                name: None,
                                value: closure,
                                spread: false,
                                span: end,
                            }],
                        },
                    });
                }
                self.bump();
                Ok(Expr {
                    span: start.to(self.prev_span()),
                    kind: ExprKind::Ident("task".to_string()),
                })
            }
            Tok::Ident(_) => {
                if matches!(
                    self.generic_call_flavor(),
                    Some(GenericCallFlavor::Call | GenericCallFlavor::Value)
                ) {
                    return self.parse_generic_call(start);
                }
                let name = self.expect_ident("expression")?;
                Ok(Expr {
                    span: start.to(self.prev_span()),
                    kind: ExprKind::Ident(name),
                })
            }
            other if contextual_ident(&other).is_some() => {
                let name = contextual_ident(&other).unwrap().to_string();
                self.bump();
                Ok(Expr {
                    span: start.to(self.prev_span()),
                    kind: ExprKind::Ident(name),
                })
            }
            Tok::Fn => self.parse_lambda(start),
            Tok::LParen => self.parse_paren_expr(),
            Tok::LBracket => {
                self.bump();
                let mut elems = Vec::new();
                if !self.at(&Tok::RBracket) {
                    loop {
                        elems.push(self.parse_expr()?);
                        if self.eat(&Tok::Comma) {
                            self.continuation_after(&Tok::Comma);
                            if self.at(&Tok::RBracket) {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                }
                self.expect(&Tok::RBracket)?;
                let span = start.to(self.prev_span());
                Ok(Expr {
                    span,
                    kind: ExprKind::Array(elems),
                })
            }
            Tok::LBrace => {
                if self.looks_like_map_literal() {
                    return self.parse_map_literal(start);
                }
                let b = self.parse_block()?;
                let span = start.to(self.prev_span());
                Ok(Expr {
                    span,
                    kind: ExprKind::Block(Box::new(b)),
                })
            }
            other => {
                self.err_at(
                    start,
                    format!("expected an expression, found {}", other.describe()),
                );
                Err(())
            }
        }
    }

    fn looks_like_map_literal(&self) -> bool {
        let mut depth = 0usize;
        let mut i = self.pos + 1;
        let toks = &self.tokens;
        loop {
            let t = &toks[i.min(toks.len() - 1)];
            match &t.token.kind {
                Tok::LBrace | Tok::LBracket | Tok::LParen => depth += 1,
                Tok::RBrace if depth == 0 => break,
                Tok::RBrace | Tok::RBracket | Tok::RParen => {
                    depth = depth.saturating_sub(1);
                }
                Tok::Eof => break,
                Tok::Newline if depth == 0 => return false,
                Tok::Colon if depth == 0 => return true,
                _ => {}
            }
            i += 1;
        }
        false
    }

    fn parse_map_literal(&mut self, start: Span) -> PResult<Expr> {
        self.expect(&Tok::LBrace)?;
        let mut entries = Vec::new();
        self.newlines();
        if !self.at(&Tok::RBrace) {
            loop {
                let k = self.parse_expr()?;
                self.expect(&Tok::Colon)?;
                self.continuation_after(&Tok::Colon);
                let v = self.parse_expr()?;
                entries.push((k, v));
                if self.eat(&Tok::Comma) {
                    self.continuation_after(&Tok::Comma);
                    self.newlines();
                    if self.at(&Tok::RBrace) {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        self.expect(&Tok::RBrace)?;
        let span = start.to(self.prev_span());
        Ok(Expr {
            span,
            kind: ExprKind::Map(entries),
        })
    }

    fn parse_paren_expr(&mut self) -> PResult<Expr> {
        let start = self.span();
        if let Some(e) = self.try_parse_lambda_parens(start)? {
            return Ok(e);
        }
        self.expect(&Tok::LParen)?;
        let first = self.parse_expr()?;
        if self.at(&Tok::Comma) {
            let mut items = vec![first];
            while self.eat(&Tok::Comma) {
                self.continuation_after(&Tok::Comma);
                if self.at(&Tok::RParen) {
                    break;
                }
                items.push(self.parse_expr()?);
            }
            self.expect(&Tok::RParen)?;
            let span = start.to(self.prev_span());
            return Ok(Expr {
                span,
                kind: ExprKind::Tuple(items),
            });
        }
        self.expect(&Tok::RParen)?;
        let span = start.to(self.prev_span());
        Ok(Expr {
            span,
            kind: first.kind,
        })
    }

    fn try_parse_lambda_parens(&mut self, start: Span) -> PResult<Option<Expr>> {
        let saved = self.pos;
        if !self.at(&Tok::LParen) {
            return Ok(None);
        }
        let mut depth = 0usize;
        let mut i = self.pos;
        let toks = &self.tokens;
        loop {
            let t = &toks[i.min(toks.len() - 1)];
            match &t.token.kind {
                Tok::LParen | Tok::LBracket | Tok::LBrace => depth += 1,
                Tok::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        let mut j = i + 1;
                        while matches!(&toks[j.min(toks.len() - 1)].token.kind, Tok::Newline) {
                            j += 1;
                        }
                        if matches!(
                            &toks[j.min(toks.len() - 1)].token.kind,
                            Tok::Arrow | Tok::FatArrow
                        ) {
                            break;
                        }
                        return Ok(None);
                    }
                }
                Tok::Eof => return Ok(None),
                _ => {}
            }
            i += 1;
        }
        // It is a lambda inline form.
        self.pos = saved;
        self.bump(); // LParen
        let mut params = Vec::new();
        if !self.at(&Tok::RParen) {
            loop {
                let ps = self.span();
                let rest = self.eat(&Tok::Ellipsis);
                let name = self.expect_ident("lambda parameter")?;
                let ty = if self.eat(&Tok::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                params.push(Param {
                    name,
                    ty,
                    default: None,
                    rest,
                    attrs: Vec::new(),
                    span: ps.to(self.prev_span()),
                });
                if self.eat(&Tok::Comma) {
                    self.continuation_after(&Tok::Comma);
                    if self.at(&Tok::RParen) {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        self.expect(&Tok::RParen)?;
        self.newlines();
        let arrow = if self.at(&Tok::FatArrow) {
            Tok::FatArrow
        } else {
            Tok::Arrow
        };
        self.bump();
        self.continuation_after(&arrow);
        let (body, return_ty) = if arrow == Tok::FatArrow {
            let e = self.parse_expr()?;
            (FnBody::Expr(Box::new(e)), None)
        } else {
            // `(x) -> Type { body }`
            let ret = self.parse_type()?;
            let body = self.parse_block()?;
            (FnBody::Block(Box::new(body)), Some(ret))
        };
        let span = start.to(self.prev_span());
        Ok(Some(Expr {
            span,
            kind: ExprKind::Lambda {
                params,
                return_ty,
                is_async: false,
                body,
            },
        }))
    }

    fn parse_lambda(&mut self, start: Span) -> PResult<Expr> {
        self.expect(&Tok::Fn)?;
        let params = self.parse_param_list()?;
        let return_ty = self.parse_optional_return_type()?;
        let body = self.parse_fn_body()?;
        let span = start.to(self.prev_span());
        Ok(Expr {
            span,
            kind: ExprKind::Lambda {
                params,
                return_ty,
                is_async: false,
                body,
            },
        })
    }

    /// Whether the upcoming `name<Type,...>` is a generic call `name<T>(...)`,
    /// a generic value `name<T>` (a function value over explicit type
    /// arguments), or not a generic bracket at all (a chained comparison such
    /// as `a < b > c`, which keeps parsing as an ordinary expression).
    fn generic_call_flavor(&self) -> Option<GenericCallFlavor> {
        let mut ix = self.pos;
        let toks = &self.tokens;
        if !matches!(toks[ix.min(toks.len() - 1)].token.kind, Tok::Ident(_)) {
            return None;
        }
        ix += 1;
        if !matches!(toks[ix.min(toks.len() - 1)].token.kind, Tok::Lt) {
            return None;
        }
        let mut depth = 0usize;
        loop {
            let t = &toks[ix.min(toks.len() - 1)];
            match &t.token.kind {
                Tok::Lt => depth += 1,
                Tok::Gt | Tok::Shr => {
                    let closings = if matches!(t.token.kind, Tok::Shr) {
                        2
                    } else {
                        1
                    };
                    if depth < closings {
                        // `>>` closes more levels than are open: this is not a
                        // generic call (comparison/shift), not a nested type.
                        return None;
                    }
                    depth -= closings;
                    if depth == 0 {
                        return Some(self.generic_flavor_after_close(toks, ix));
                    }
                }
                Tok::Eof | Tok::Newline => return None,
                _ => {}
            }
            ix += 1;
        }
    }

    fn generic_flavor_after_close(&self, toks: &[LexedToken], ix: usize) -> GenericCallFlavor {
        // A `(` after the closing `>` is a call `name<T>(args)`.
        let mut j = ix + 1;
        while matches!(&toks[j.min(toks.len() - 1)].token.kind, Tok::Newline) {
            j += 1;
        }
        if matches!(&toks[j.min(toks.len() - 1)].token.kind, Tok::LParen) {
            return GenericCallFlavor::Call;
        }
        // Otherwise a value `name<T>` -- but only when the expression clearly
        // ends there. A bare name, `>`, `>=`, or `>>` right after the close is
        // a chained comparison (`a < b > c`), NOT a generic value; everything
        // else (statement end, `,`, `)`, `]`, `}`, `;`) means the bracket
        // stands alone as a value.
        match toks[(ix + 1).min(toks.len() - 1)].token.kind {
            Tok::Ident(_) | Tok::Gt | Tok::Ge | Tok::Shr | Tok::Eof => GenericCallFlavor::None,
            _ => GenericCallFlavor::Value,
        }
    }

    fn parse_generic_call(&mut self, start: Span) -> PResult<Expr> {
        let name = self.expect_ident("function name")?;
        let type_args = self.parse_type_args()?;
        // Without a `(` this is a generic function used as a VALUE
        // (`let f = id_fn<int>` or `apply(id_fn<int>, 3)`): an expression of
        // that concrete instantiation's signature.
        if !self.at(&Tok::LParen) {
            let span = start.to(self.prev_span());
            return Ok(Expr {
                span,
                kind: ExprKind::GenericCall { name, type_args },
            });
        }
        let call_args = self.parse_call_args_internal()?;
        let callee_span = start
            .to(self.prev_span())
            .to(call_args.last().map(|a| a.span).unwrap_or(start));
        let span = start.to(self.prev_span());
        let callee = Expr {
            span: callee_span,
            kind: ExprKind::GenericCall { name, type_args },
        };
        Ok(Expr {
            span,
            kind: ExprKind::Call {
                callee: Box::new(callee),
                args: call_args,
            },
        })
    }

    fn parse_if(&mut self) -> PResult<Expr> {
        let start = self.span();
        self.expect(&Tok::If)?;
        self.expect(&Tok::LParen)?;
        let cond = if self.at(&Tok::Let) || self.at(&Tok::Var) {
            self.bump();
            let pat = self.parse_pattern()?;
            self.expect(&Tok::Assign)?;
            self.continuation_after(&Tok::Assign);
            let value = self.parse_expr()?;
            self.expect(&Tok::RParen)?;
            IfCond::Binding {
                pattern: pat,
                value: Box::new(value),
            }
        } else {
            let c = self.parse_expr()?;
            self.expect(&Tok::RParen)?;
            IfCond::Cond(Box::new(c))
        };
        let then = self.parse_block()?;
        let else_else = if self.at(&Tok::Newline) {
            if matches!(self.peek_non_nl(1), Some(Tok::Else)) {
                self.newlines();
                Some(self.parse_else()?)
            } else {
                None
            }
        } else if self.at(&Tok::Else) {
            Some(self.parse_else()?)
        } else {
            None
        };
        let span = start.to(self.prev_span());
        Ok(Expr {
            span,
            kind: ExprKind::If {
                cond,
                then: Box::new(then),
                else_else: else_else.map(Box::new),
            },
        })
    }

    fn parse_else(&mut self) -> PResult<Expr> {
        self.expect(&Tok::Else)?;
        if self.at(&Tok::If) {
            self.parse_if()
        } else {
            let b = self.parse_block()?;
            let span = b.span;
            Ok(Expr {
                span,
                kind: ExprKind::Block(Box::new(b)),
            })
        }
    }

    fn parse_match(&mut self) -> PResult<Expr> {
        let start = self.span();
        self.expect(&Tok::Match)?;
        self.expect(&Tok::LParen)?;
        let scrutinee = self.parse_expr()?;
        self.expect(&Tok::RParen)?;
        self.expect(&Tok::LBrace)?;
        let mut arms = Vec::new();
        self.newlines();
        loop {
            if self.at(&Tok::RBrace) || self.at(&Tok::Eof) {
                break;
            }
            let arm_start = self.span();
            let catch_all = if self.eat(&Tok::Else) {
                true
            } else {
                self.expect(&Tok::Case)?;
                false
            };
            let pattern = if catch_all || self.at(&Tok::Arrow) {
                Pattern::Wildcard
            } else {
                let first = self.parse_pattern()?;
                if self.eat(&Tok::Comma) {
                    let mut pats = vec![first, self.parse_pattern()?];
                    while self.eat(&Tok::Comma) {
                        pats.push(self.parse_pattern()?);
                    }
                    Pattern::Or(pats)
                } else {
                    first
                }
            };
            let guard = if self.at(&Tok::If) {
                self.bump();
                Some(self.parse_expr()?)
            } else {
                None
            };
            self.expect(&Tok::Arrow)?;
            let body = self.parse_expr()?;
            let arm_span = arm_start.to(body.span);
            arms.push(MatchArm {
                pattern,
                guard,
                body,
                span: arm_span,
            });
            if self.at(&Tok::Newline) {
                self.newlines();
            } else if self.at(&Tok::RBrace) || self.at(&Tok::Eof) {
                break;
            } else {
                self.err_here(format!(
                    "expected end of match arm, found {}",
                    self.kind().describe()
                ));
                return Err(());
            }
        }
        self.expect(&Tok::RBrace)?;
        let span = start.to(self.prev_span());
        Ok(Expr {
            span,
            kind: ExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms,
            },
        })
    }

    // ---------- type declarations ----------

    fn parse_type_decl(&mut self, is_class: bool) -> PResult<ClassDecl> {
        let start = self.span();
        self.bump(); // class | struct
        let name = self.expect_ident(if is_class {
            "class name"
        } else {
            "struct name"
        })?;
        let generics = self.parse_generic_params()?;
        let mut extends = None;
        let mut implements = Vec::new();
        if is_class && self.at(&Tok::Extends) {
            self.bump();
            extends = Some(self.parse_type()?);
        }
        if self.at(&Tok::Implements) {
            self.bump();
            loop {
                implements.push(self.parse_type()?);
                if self.eat(&Tok::Comma) {
                    self.continuation_after(&Tok::Comma);
                    self.newlines();
                } else {
                    break;
                }
            }
        }
        self.expect(&Tok::LBrace)?;
        let members = self.parse_class_members()?;
        self.expect(&Tok::RBrace)?;
        let span = start.to(self.prev_span());
        Ok(ClassDecl {
            name,
            span,
            visibility: Visibility::Default,
            generics,
            extends,
            implements,
            members,
        })
    }

    fn parse_enum(&mut self) -> PResult<EnumDecl> {
        let start = self.span();
        self.bump();
        let name = self.expect_ident("enum name")?;
        let generics = self.parse_generic_params()?;
        let mut implements = Vec::new();
        if self.at(&Tok::Implements) {
            self.bump();
            loop {
                implements.push(self.parse_type()?);
                if self.eat(&Tok::Comma) {
                    self.continuation_after(&Tok::Comma);
                } else {
                    break;
                }
            }
        }
        self.expect(&Tok::LBrace)?;
        let mut variants = Vec::new();
        self.newlines();
        loop {
            if self.at(&Tok::RBrace) || self.at(&Tok::Eof) {
                break;
            }
            let vstart = self.span();
            if self.at(&Tok::Case) {
                self.bump();
            }
            let vname = self.expect_ident("enum variant")?;
            let mut fields = Vec::new();
            let mut discriminant = None;
            if self.at(&Tok::LParen) {
                self.bump();
                if !self.at(&Tok::RParen) {
                    loop {
                        let fname = self.expect_ident("variant field")?;
                        self.expect(&Tok::Colon)?;
                        let ty = self.parse_type()?;
                        fields.push(EnumField { name: fname, ty });
                        if self.eat(&Tok::Comma) {
                            self.continuation_after(&Tok::Comma);
                            if self.at(&Tok::RParen) {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                }
                self.expect(&Tok::RParen)?;
            } else if self.at(&Tok::Assign) {
                self.bump();
                let t = self.advance();
                discriminant = t.data.int;
                if discriminant.is_none() {
                    self.err_at(t.token.span, "enum discriminants must be integer literals");
                }
            }
            variants.push(EnumVariant {
                name: vname,
                fields,
                discriminant,
                span: vstart.to(self.prev_span()),
            });
            if self.at(&Tok::Newline) {
                self.newlines();
            } else if self.at(&Tok::RBrace) || self.at(&Tok::Eof) {
                break;
            } else {
                self.err_here("expected newline or `,` between enum variants");
                return Err(());
            }
        }
        self.expect(&Tok::RBrace)?;
        let span = start.to(self.prev_span());
        Ok(EnumDecl {
            name,
            span,
            visibility: Visibility::Default,
            generics,
            variants,
            implements,
        })
    }

    fn parse_interface(&mut self) -> PResult<InterfaceDecl> {
        let start = self.span();
        self.bump();
        let name = self.expect_ident("interface name")?;
        let generics = self.parse_generic_params()?;
        let mut extends = Vec::new();
        if self.at(&Tok::Extends) {
            self.bump();
            loop {
                extends.push(self.parse_type()?);
                if self.eat(&Tok::Comma) {
                    self.continuation_after(&Tok::Comma);
                } else {
                    break;
                }
            }
        }
        self.expect(&Tok::LBrace)?;
        let mut members = Vec::new();
        self.newlines();
        loop {
            if self.at(&Tok::RBrace) || self.at(&Tok::Eof) {
                break;
            }
            match self.kind().clone() {
                Tok::Fn => {
                    let m = self.parse_method(false)?;
                    members.push(InterfaceMember::Method(m));
                }
                Tok::Property => {
                    let pstart = self.span();
                    self.bump();
                    let name = self.expect_ident("property name")?;
                    let ty = if self.eat(&Tok::Colon) {
                        Some(self.parse_type()?)
                    } else {
                        None
                    };
                    let span = pstart.to(self.prev_span());
                    members.push(InterfaceMember::Property {
                        name,
                        ty: ty.unwrap_or(TypeExpr {
                            span,
                            kind: TypeExprKind::Infer,
                        }),
                        span,
                    });
                }
                Tok::Const => {
                    self.bump();
                    let cname = self.expect_ident("constant name")?;
                    let ty = if self.eat(&Tok::Colon) {
                        Some(self.parse_type()?)
                    } else {
                        None
                    };
                    self.expect(&Tok::Assign)?;
                    let value = self.parse_expr()?;
                    let span = start.to(self.prev_span());
                    members.push(InterfaceMember::Const {
                        name: cname,
                        value,
                        span,
                    });
                    let _ = ty;
                }
                Tok::Newline => {
                    self.bump();
                }
                _ => {
                    self.err_here("expected `fn`, `property`, or `const` in interface");
                    return Err(());
                }
            }
            if self.at(&Tok::Newline) {
                self.newlines();
            }
        }
        self.expect(&Tok::RBrace)?;
        let span = start.to(self.prev_span());
        Ok(InterfaceDecl {
            name,
            span,
            visibility: Visibility::Default,
            generics,
            extends,
            members,
        })
    }

    fn parse_class_members(&mut self) -> PResult<Vec<ClassMember>> {
        let mut members = Vec::new();
        self.newlines();
        loop {
            if self.at(&Tok::RBrace) || self.at(&Tok::Eof) {
                break;
            }
            members.push(self.parse_class_member()?);
            if self.at(&Tok::Newline) {
                self.newlines();
            }
            // A member ending in `}` may be followed by the next member on the
            // same line; field members enforce their own end-of-line via
            // expect_stmt_end.
        }
        Ok(members)
    }

    fn parse_class_member(&mut self) -> PResult<ClassMember> {
        let start = self.span();
        let attrs = self.parse_attributes()?;
        let mut visibility = Visibility::Default;
        let mut is_static = false;
        let mut is_override = false;
        let mut is_const = false;

        loop {
            match self.kind().clone() {
                Tok::Public => {
                    self.bump();
                    visibility = Visibility::Public;
                }
                Tok::Private => {
                    self.bump();
                    visibility = Visibility::Private;
                }
                Tok::Protected => {
                    self.bump();
                    visibility = Visibility::Protected;
                }
                Tok::Static => {
                    self.bump();
                    is_static = true;
                }
                Tok::Override => {
                    self.bump();
                    is_override = true;
                }
                Tok::Const => {
                    self.bump();
                    is_const = true;
                }
                _ => break,
            }
        }

        let field_syntax = !is_const && matches!(self.kind(), Tok::Let | Tok::Var | Tok::Ident(_));
        if !attrs.is_empty() && !field_syntax {
            for a in &attrs {
                self.err_at(
                    a.span,
                    format!(
                        "attribute `#[{}]` is only supported on class/struct fields",
                        a.name
                    ),
                );
            }
        }

        match (is_const, self.kind().clone()) {
            (true, _) => {
                let name = self.expect_ident("constant name")?;
                let ty = if self.eat(&Tok::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                self.expect(&Tok::Assign)?;
                self.continuation_after(&Tok::Assign);
                let value = self.parse_expr()?;
                let span = start.to(self.prev_span());
                self.expect_stmt_end()?;
                Ok(ClassMember::Const {
                    name,
                    visibility,
                    ty,
                    value,
                    span,
                })
            }
            (false, k)
                if matches!(&k, Tok::Constructor)
                    && matches!(self.peek_at(1), Tok::LParen | Tok::Dot) =>
            {
                self.bump();
                let name = if self.at(&Tok::Dot) {
                    self.bump();
                    Some(self.expect_ident("constructor name")?)
                } else {
                    None
                };
                let params = self.parse_param_list()?;
                let body = self.parse_block()?;
                let span = start.to(self.prev_span());
                Ok(ClassMember::Constructor(ConstructorDecl {
                    name,
                    span,
                    visibility,
                    params,
                    body,
                }))
            }
            (false, Tok::Property) => {
                let p = self.parse_property_with(start, visibility, is_static)?;
                Ok(ClassMember::Property(p))
            }
            (false, Tok::Fn) => {
                let mut m = self.parse_method(is_override)?;
                m.is_static = is_static;
                m.visibility = visibility;
                Ok(ClassMember::Method(m))
            }
            (false, k) if matches!(&k, Tok::Operator) && is_operator_symbol(self.peek_at(1)) => {
                let m = self.parse_operator_method(start, is_override)?;
                Ok(ClassMember::Method(m))
            }
            (false, k)
                if matches!(&k, Tok::Init | Tok::Deinit) && self.peek_at(1) == &Tok::LBrace =>
            {
                self.bump();
                let body = self.parse_block()?;
                if matches!(k, Tok::Init) {
                    Ok(ClassMember::Init(body))
                } else {
                    Ok(ClassMember::Deinit(body))
                }
            }
            (false, Tok::Let) | (false, Tok::Var) => {
                self.bump();
                let name = self.expect_ident("field name")?;
                let ty = if self.eat(&Tok::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                let init = if self.eat(&Tok::Assign) {
                    self.continuation_after(&Tok::Assign);
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                let span = start.to(self.prev_span());
                self.expect_stmt_end()?;
                Ok(ClassMember::Field {
                    name,
                    ty,
                    init,
                    visibility,
                    is_static,
                    const_: false,
                    attrs: attrs.clone(),
                    span,
                })
            }
            (false, k) if member_field_name(&k) => {
                // field: `name: Type` or `name = expr` (`= expr` may infer).
                // `member_field_name` also admits contextual keywords so fields
                // like `use`, `get`, `set`, or `init` stay legal names.
                let name = self.expect_ident("field name")?;
                let ty = if self.eat(&Tok::Colon) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                let init = if self.eat(&Tok::Assign) {
                    self.continuation_after(&Tok::Assign);
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                let span = start.to(self.prev_span());
                self.expect_stmt_end()?;
                Ok(ClassMember::Field {
                    name,
                    ty,
                    init,
                    visibility,
                    is_static,
                    const_: false,
                    attrs: attrs.clone(),
                    span,
                })
            }
            _ => {
                self.err_here(format!(
                    "expected a class member, found {}",
                    self.kind().describe()
                ));
                Err(())
            }
        }
    }

    fn parse_property_with(
        &mut self,
        start: Span,
        visibility: Visibility,
        is_static: bool,
    ) -> PResult<PropertyDecl> {
        self.expect(&Tok::Property)?;
        let name = self.expect_ident("property name")?;
        let ty = if self.eat(&Tok::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        self.parse_property_body_parts(start, visibility, is_static, name, ty)
    }

    fn parse_property_body_parts(
        &mut self,
        start: Span,
        visibility: Visibility,
        is_static: bool,
        name: String,
        ty: Option<TypeExpr>,
    ) -> PResult<PropertyDecl> {
        self.expect(&Tok::LBrace)?;
        let mut get = None;
        let mut set = None;
        self.newlines();
        loop {
            if self.at(&Tok::RBrace) || self.at(&Tok::Eof) {
                break;
            }
            match self.kind().clone() {
                Tok::Get => {
                    self.bump();
                    get = Some(self.parse_property_accessor()?);
                }
                Tok::Set => {
                    self.bump();
                    set = Some(self.parse_property_accessor()?);
                }
                _ => {
                    self.err_here("expected `get` or `set` in property");
                    return Err(());
                }
            }
            if self.at(&Tok::Newline) {
                self.newlines();
            } else if self.at(&Tok::RBrace) || self.at(&Tok::Eof) {
                break;
            } else {
                self.err_here("expected newline between property accessors");
                return Err(());
            }
        }
        self.expect(&Tok::RBrace)?;
        let span = start.to(self.prev_span());
        Ok(PropertyDecl {
            name,
            span,
            visibility,
            ty,
            get,
            set,
            is_static,
        })
    }

    fn parse_property_accessor(&mut self) -> PResult<PropertyAccessor> {
        if self.at(&Tok::FatArrow) {
            self.bump();
            let e = self.parse_expr()?;
            Ok(PropertyAccessor::Expr(e))
        } else if self.at(&Tok::LBrace) {
            let b = self.parse_block()?;
            Ok(PropertyAccessor::Block(b))
        } else {
            self.err_here("expected `=> expr` or `{ ... }` in property accessor");
            Err(())
        }
    }

    fn parse_method(&mut self, is_override: bool) -> PResult<MethodDecl> {
        let start = self.span();
        self.expect(&Tok::Fn)?;
        let name = self.expect_fn_name()?;
        let generics = self.parse_generic_params()?;
        let params = self.parse_param_list()?;
        let return_ty = self.parse_optional_return_type()?;
        let body = self.parse_optional_body()?;
        let span = start.to(self.prev_span());
        Ok(MethodDecl {
            name,
            span,
            visibility: Visibility::Default,
            is_static: false,
            is_override,
            is_async: false,
            is_test: false,
            operator: None,
            generics,
            params,
            return_ty,
            body,
        })
    }

    /// Parse a method body if present; `None` means an abstract declaration.
    fn parse_optional_body(&mut self) -> PResult<Option<FnBody>> {
        if self.eat(&Tok::FatArrow) {
            let e = self.parse_expr()?;
            return Ok(Some(FnBody::Expr(Box::new(e))));
        }
        if self.at(&Tok::LBrace) {
            let b = self.parse_block()?;
            return Ok(Some(FnBody::Block(Box::new(b))));
        }
        Ok(None)
    }

    fn parse_operator_method(&mut self, start: Span, is_override: bool) -> PResult<MethodDecl> {
        self.expect(&Tok::Operator)?;
        let op = match self.kind().clone() {
            Tok::Plus => "+",
            Tok::Minus => "-",
            Tok::Star => "*",
            Tok::Slash => "/",
            Tok::Percent => "%",
            Tok::StarStar => "**",
            Tok::EqEq => "==",
            Tok::NotEq => "!=",
            Tok::Lt => "<",
            Tok::Le => "<=",
            Tok::Gt => ">",
            Tok::Ge => ">=",
            Tok::Shl => "<<",
            Tok::Shr => ">>",
            Tok::Amp => "&",
            Tok::Pipe => "|",
            Tok::Caret => "^",
            Tok::LBracket => {
                self.bump();
                if self.eat(&Tok::RBracket) {
                    "[]"
                } else {
                    self.err_here("expected `]` after `[` in index operator");
                    return Err(());
                }
            }
            other => {
                self.err_here(format!(
                    "expected an operator symbol, found {}",
                    other.describe()
                ));
                return Err(());
            }
        };
        self.bump();
        let generics = self.parse_generic_params()?;
        let params = self.parse_param_list()?;
        let return_ty = self.parse_optional_return_type()?;
        let body = self.parse_optional_body()?;
        let span = start.to(self.prev_span());
        Ok(MethodDecl {
            name: op.to_string(),
            span,
            visibility: Visibility::Default,
            is_static: false,
            is_override,
            is_async: false,
            is_test: false,
            operator: Some(op.to_string()),
            generics,
            params,
            return_ty,
            body,
        })
    }
}

fn binop_of(t: &Tok) -> Option<(u8, BinOp)> {
    use Tok::*;
    match t {
        OrOr => Some((1, BinOp::Or)),
        QuestionQuestion => Some((1, BinOp::NullCoalesce)),
        AndAnd => Some((2, BinOp::And)),
        Pipe => Some((3, BinOp::BitOr)),
        Caret => Some((4, BinOp::BitXor)),
        Amp => Some((5, BinOp::BitAnd)),
        EqEq => Some((6, BinOp::Eq)),
        NotEq => Some((6, BinOp::Ne)),
        Lt => Some((6, BinOp::Lt)),
        Le => Some((6, BinOp::Le)),
        Gt => Some((6, BinOp::Gt)),
        Ge => Some((6, BinOp::Ge)),
        In => Some((6, BinOp::In)),
        Shl => Some((7, BinOp::Shl)),
        Shr => Some((7, BinOp::Shr)),
        Plus => Some((8, BinOp::Add)),
        Minus => Some((8, BinOp::Sub)),
        Star => Some((9, BinOp::Mul)),
        Slash => Some((9, BinOp::Div)),
        Percent => Some((9, BinOp::Mod)),
        StarStar => Some((10, BinOp::Pow)),
        Range => Some((0, BinOp::Range)),
        RangeIncl => Some((0, BinOp::RangeIncl)),
        SendOp => Some((0, BinOp::Send)),
        _ => Option::None,
    }
}

impl AssignOp {
    fn as_tok(&self) -> Tok {
        match self {
            AssignOp::Assign => Tok::Assign,
            AssignOp::Add => Tok::PlusEq,
            AssignOp::Sub => Tok::MinusEq,
            AssignOp::Mul => Tok::StarEq,
            AssignOp::Div => Tok::SlashEq,
            AssignOp::Mod => Tok::PercentEq,
            AssignOp::Shl => Tok::ShlEq,
            AssignOp::Shr => Tok::ShrEq,
            AssignOp::BitAnd => Tok::AndEq,
            AssignOp::BitOr => Tok::OrEq,
            AssignOp::BitXor => Tok::XorEq,
        }
    }
}
// ---- describe / hook desugar helpers ------------------------------------

fn hook_kind(name: &str) -> Option<&'static str> {
    match name {
        "beforeAll" => Some("ba"),
        "beforeEach" => Some("be"),
        "afterEach" => Some("ae"),
        "afterAll" => Some("aa"),
        _ => None,
    }
}

fn call_parts(e: &Expr) -> Option<(&str, &[CallArg])> {
    if let ExprKind::Call { callee, args } = &e.kind {
        if let ExprKind::Ident(n) = &callee.kind {
            return Some((n, args));
        }
    }
    None
}

fn str_literal_of(e: &Expr) -> Option<&Vec<StrPart>> {
    if let ExprKind::Lit(Lit::String(parts)) = &e.kind {
        return Some(parts);
    }
    None
}

fn block_of(e: &Expr) -> Option<&Block> {
    if let ExprKind::Block(b) = &e.kind {
        return Some(b);
    }
    None
}

fn stmt_span(s: &Stmt) -> Span {
    match s {
        Stmt::Let { span, .. }
        | Stmt::Const { span, .. }
        | Stmt::Return { span, .. }
        | Stmt::Break { span }
        | Stmt::Continue { span }
        | Stmt::While { span, .. }
        | Stmt::For { span, .. }
        | Stmt::Empty(span) => *span,
        Stmt::Expr(e) => e.span,
    }
}

/// The "contextual" keyword set: words that bind only at their grammar
/// position (a `get` accessor inside a `property`, `init` before a body, a
/// `static` member modifier, ...) and are ordinary identifiers everywhere
/// else. Reserved words — `fn`, `let`, `if`, `match`, ... — are not here.
fn contextual_ident(t: &Tok) -> Option<&'static str> {
    Some(match t {
        Tok::Get => "get",
        Tok::Set => "set",
        Tok::Init => "init",
        Tok::Deinit => "deinit",
        Tok::Task => "task",
        Tok::Channel => "channel",
        Tok::Async => "async",
        Tok::Constructor => "constructor",
        Tok::Operator => "operator",
        Tok::Property => "property",
        Tok::Static => "static",
        Tok::Override => "override",
        Tok::Public => "public",
        Tok::Private => "private",
        Tok::Protected => "protected",
        Tok::Extends => "extends",
        Tok::Implements => "implements",
        _ => return None,
    })
}

/// Whether a class/struct member slot should be read as a field name: plain
/// identifiers plus the contextual words that rarely need their grammar
/// meaning there (`get`/`set`/`init`/... as variable-safe names). Declaration
/// keywords (`constructor`, `operator`, `property`) still bind at a member
/// slot, exactly like `fn` does.
fn member_field_name(t: &Tok) -> bool {
    matches!(
        t,
        Tok::Ident(_)
            | Tok::Get
            | Tok::Set
            | Tok::Init
            | Tok::Deinit
            | Tok::Task
            | Tok::Channel
            | Tok::Async
    )
}

/// The operator-symbol tokens an `operator` member may overload.
fn is_operator_symbol(t: &Tok) -> bool {
    matches!(
        t,
        Tok::Plus
            | Tok::Minus
            | Tok::Star
            | Tok::Slash
            | Tok::Percent
            | Tok::StarStar
            | Tok::EqEq
            | Tok::NotEq
            | Tok::Lt
            | Tok::Le
            | Tok::Gt
            | Tok::Ge
            | Tok::Shl
            | Tok::Shr
            | Tok::Amp
            | Tok::Pipe
            | Tok::Caret
            | Tok::LBracket
    )
}
