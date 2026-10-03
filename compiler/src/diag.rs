use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub file: FileId,
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(file: FileId, start: usize, end: usize) -> Span {
        Span { file, start, end }
    }

    pub fn join(self, other: Span) -> Span {
        debug_assert!(self.file == other.file);
        Span {
            file: self.file,
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    pub fn to(self, other: Span) -> Span {
        self.join(other)
    }

    pub fn point(self) -> Span {
        Span {
            file: self.file,
            start: self.start,
            end: self.start,
        }
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}-{}", self.file.0, self.start, self.end)
    }
}

pub struct SourceFile {
    pub name: String,
    pub text: Rc<str>,
    line_starts: Vec<usize>,
}

impl SourceFile {
    fn new(name: String, text: String) -> SourceFile {
        let mut line_starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        let text: Rc<str> = Rc::from(text);
        SourceFile {
            name,
            text,
            line_starts,
        }
    }

    pub fn line_of(&self, byte: usize) -> usize {
        match self.line_starts.binary_search(&byte) {
            Ok(i) => i,
            Err(i) => i - 1,
        }
    }

    /// Byte offset of the start of a line (0-based line index).
    pub fn line_start(&self, line: usize) -> usize {
        self.line_starts[line]
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    pub fn line_text(&self, line: usize) -> &str {
        let start = self.line_starts[line];
        let end = if line + 1 < self.line_starts.len() {
            self.line_starts[line + 1]
        } else {
            self.text.len()
        };
        self.text[start..end]
            .trim_end_matches(['\n', '\r'])
            .trim_end()
    }
}

#[derive(Default)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

impl SourceMap {
    pub fn add(&mut self, name: String, text: String) -> FileId {
        let id = FileId(self.files.len());
        self.files.push(SourceFile::new(name, text));
        id
    }

    pub fn get(&self, id: FileId) -> &SourceFile {
        &self.files[id.0]
    }

    /// Every file the frontend read, in load order.
    ///
    /// A build cache keys its entry on these: the lowered program depends on
    /// the contents of all of them, and the entry file's own text is only
    /// known to the frontend once it has read it.
    pub fn files(&self) -> &[SourceFile] {
        &self.files
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub span: Option<Span>,
    pub notes: Vec<(Option<Span>, String)>,
    /// Stable error code, when the emitting phase assigned one. `None`
    /// renders without a `[E....]` prefix.
    pub code: Option<crate::error::ErrorCode>,
    /// Concrete, executable fixes. Kept machine-friendly (no markdown).
    pub suggestions: Vec<String>,
}

impl Diagnostic {
    pub fn error(message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            severity: Severity::Error,
            message: message.into(),
            span: None,
            notes: Vec::new(),
            code: None,
            suggestions: Vec::new(),
        }
    }

    pub fn error_at(span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            severity: Severity::Error,
            message: message.into(),
            span: Some(span),
            notes: Vec::new(),
            code: None,
            suggestions: Vec::new(),
        }
    }

    pub fn warn_at(span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            severity: Severity::Warning,
            message: message.into(),
            span: Some(span),
            notes: Vec::new(),
            code: None,
            suggestions: Vec::new(),
        }
    }

    pub fn note(mut self, message: impl Into<String>) -> Diagnostic {
        self.notes.push((None, message.into()));
        self
    }

    pub fn note_at(mut self, span: Span, message: impl Into<String>) -> Diagnostic {
        self.notes.push((Some(span), message.into()));
        self
    }

    pub fn with_code(mut self, code: crate::error::ErrorCode) -> Diagnostic {
        self.code = Some(code);
        self
    }

    pub fn suggestion(mut self, suggestion: impl Into<String>) -> Diagnostic {
        self.suggestions.push(suggestion.into());
        self
    }
}

/// A session-wide error sink. Collects diagnostics for one compile.
#[derive(Default)]
pub struct DiagnosticSink {
    pub diagnostics: Rc<RefCell<Vec<Diagnostic>>>,
    pub errors_abort: usize,
}

impl DiagnosticSink {
    pub fn new() -> DiagnosticSink {
        DiagnosticSink {
            diagnostics: Rc::new(RefCell::new(Vec::new())),
            errors_abort: 50,
        }
    }

    pub fn emit(&self, d: Diagnostic) {
        let abort = d.severity == Severity::Error;
        self.diagnostics.borrow_mut().push(d);
        if abort && self.any_error() {
            // count handled by has_errors
        }
        let _ = abort;
    }

    pub fn any_error(&self) -> bool {
        self.diagnostics
            .borrow()
            .iter()
            .any(|d| d.severity == Severity::Error)
    }

    pub fn error_count(&self) -> usize {
        self.diagnostics
            .borrow()
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .count()
    }

    pub fn too_many_errors(&self) -> bool {
        self.error_count() >= self.errors_abort
    }

    pub fn render_all(&self, map: &SourceMap, colored: bool) -> String {
        self.render_all_grouped(map, colored)
    }

    /// Render diagnostics grouped under their error code.
    ///
    /// Diagnostics without a `code` fall into an `(undiagnosed)` group that
    /// lists them plainly, so the output never hides an error.
    pub fn render_all_grouped(&self, map: &SourceMap, colored: bool) -> String {
        use std::collections::BTreeMap;
        let diags = self.diagnostics.borrow();
        let mut groups: BTreeMap<Option<crate::error::ErrorCode>, Vec<Diagnostic>> =
            BTreeMap::new();
        for d in diags.iter() {
            groups.entry(d.code).or_default().push(d.clone());
        }
        drop(diags);

        let (sg, eg) = ("\x1b[1;31m", "\x1b[0m");
        let (cinder, einder) = if colored { (sg, eg) } else { ("", "") };
        let mut out = String::new();
        for (code, diags) in groups {
            match code {
                Some(code) => {
                    out.push_str(&format!(
                        "{cinder}error[{code}]{einder} -- {}\n",
                        code_title(code)
                    ));
                }
                None => {
                    out.push_str(&format!("{cinder}error{einder} -- (uncoded)\n"));
                }
            }
            for d in &diags {
                let mut plain = String::new();
                render_diagnostic(d, map, colored, &mut plain);
                for line in plain.lines() {
                    out.push_str("    ");
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
        out
    }

    /// Render diagnostics as one JSON array. `diagnostics` arrays and their
    /// `notes`/`suggestions` are exactly the same length and position as the
    /// compiler's internal vectors; a machine consumer can align them 1:1.
    pub fn render_all_json(&self, map: &SourceMap) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        out.push_str("[\n");
        for (i, d) in self.diagnostics.borrow().iter().enumerate() {
            out.push_str("  {\n");
            let _ = writeln!(out, "    \"message\": {},", json_string(&d.message));
            let _ = writeln!(
                out,
                "    \"severity\": {},",
                json_string(match d.severity {
                    Severity::Error => "error",
                    Severity::Warning => "warning",
                })
            );
            let _ = writeln!(
                out,
                "    \"code\": {},",
                match d.code {
                    Some(code) => json_string(code.id()),
                    None => "null".to_string(),
                }
            );
            match d.span {
                Some(span) => {
                    let line = map.get(span.file).line_of(span.start);
                    let col = span.start - map.get(span.file).line_start(line);
                    let _ = write!(
                        out,
                        "    \"span\": {{\n      \"file\": {},\n      \"line\": {},\n      \"column\": {},\n      \"end_column\": {}\n    }},\n",
                        json_string(&map.get(span.file).name),
                        line + 1,
                        col + 1,
                        col + span.end.saturating_sub(span.start).max(1)
                    );
                }
                None => {
                    out.push_str("    \"span\": null,\n");
                }
            }
            out.push_str("    \"notes\": [");
            for (j, (_, note)) in d.notes.iter().enumerate() {
                if j > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "{}", json_string(note));
            }
            out.push_str("],\n");
            out.push_str("    \"suggestions\": [");
            for (j, s) in d.suggestions.iter().enumerate() {
                if j > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "{}", json_string(s));
            }
            out.push_str("]\n  }");
            if i + 1 < self.diagnostics.borrow().len() {
                out.push(',');
            }
            out.push('\n');
        }
        out.push_str("]\n");
        out
    }
}

fn code_title(code: crate::error::ErrorCode) -> &'static str {
    crate::error::explain(code.id())
        .map(|e| e.title)
        .unwrap_or("unknown code")
}

/// Minimal RFC 8259 JSON string encoder. The compiler has no `serde`
/// dependency, and diagnostics only ever carry strings, numbers, and null.
pub fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn render_diagnostic(d: &Diagnostic, map: &SourceMap, colored: bool, out: &mut String) {
    use std::fmt::Write;
    let (severity, sg, eg) = match d.severity {
        Severity::Error => ("error", "\x1b[1;31m", "\x1b[0m"),
        Severity::Warning => ("warning", "\x1b[1;33m", "\x1b[0m"),
    };
    let code_prefix = match d.code {
        Some(code) => format!("[{}] ", code.id()),
        None => String::new(),
    };

    match d.span {
        Some(span) => {
            let file = map.get(span.file);
            let line = file.line_of(span.start);
            let lline = line + 1;
            let col = span.start - file.line_start(line);
            if colored {
                let _ = write!(out, "{sg}{severity}{eg}: ");
            } else {
                let _ = write!(out, "{severity}: ");
            }
            let _ = write!(out, "{}:{}:{}: {code_prefix}", file.name, lline, col + 1,);
            let _ = writeln!(out, "{}", d.message);

            let text = file.line_text(line);
            let _ = writeln!(out, "  {lline} | {text}");
            // Align the `^` gutter with the line-number gutter above, so the
            // caret always lands under the exact byte column, however many
            // digits the line number has.
            let gutter = format!("{}| ", " ".repeat(lline.to_string().len() + 3));
            out.push_str(&gutter);
            for _ in 0..col {
                out.push(' ');
            }
            let width = span.end.saturating_sub(span.start).clamp(1, 80);
            for _ in 0..width {
                out.push('^');
            }
            for (nspan, note) in &d.notes {
                match nspan {
                    Some(nspan) => {
                        let line = file.line_of(nspan.start);
                        let col = nspan.start - file.line_start(line);
                        let ntext = file.line_text(line);
                        let nline = line + 1;
                        let ngutter = format!("{}| ", " ".repeat(nline.to_string().len() + 3));
                        let _ = write!(out, "\n  {nline} | {ntext}");
                        out.push('\n');
                        out.push_str(&ngutter);
                        for _ in 0..col {
                            out.push(' ');
                        }
                        let _ = write!(out, "^ note: {note}");
                    }
                    None => {
                        let _ = write!(out, "\n  = note: {note}");
                    }
                }
            }
        }
        None => {
            if colored {
                let _ = write!(out, "{sg}{severity}{eg}: ");
            } else {
                let _ = write!(out, "{severity}: ");
            }
            let _ = write!(out, "{code_prefix}");
            let _ = write!(out, "{}", d.message);
            for (_, note) in &d.notes {
                let _ = write!(out, "\n  = note: {note}");
            }
        }
    }
    for suggestion in &d.suggestions {
        let _ = write!(out, "\n  = help: {suggestion}");
    }
}
