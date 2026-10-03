use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::ast::*;
use crate::check::check_program;
use crate::diag::{Diagnostic, DiagnosticSink, FileId, SourceMap, Span};
use crate::error::ErrorCode;
use crate::parser::parse;
use crate::resolve::{ResolvedProgram, Resolver};

pub struct FrontOutput {
    pub program: Program,
    pub resolved: ResolvedProgram,
}

/// Lex, parse, resolve, and type-check a single source module. The caller owns
/// the `SourceMap` and `DiagnosticSink` (checked after this returns).
///
/// Any `import`/`use` declarations are loaded module-by-module from disk and
/// merged into one flat program; qualified references are rewritten before
/// resolution. The `file_name` is used as a filesystem path for import
/// resolution, so callers that want on-disk modules must pass a full path
/// (the CLI passes the file's display path).
pub fn frontend(
    file_name: &str,
    source: &str,
    map: &mut SourceMap,
    diags: &DiagnosticSink,
) -> Option<FrontOutput> {
    let loaded = Loader::new(map, diags).load_root(file_name, source)?;
    let mut program = loaded.merged_program;
    Rewriter {
        ctx: &loaded.imports_for,
        collided: &loaded.collided,
        diags,
    }
    .rewrite_program(&mut program);
    let resolved = Resolver::with_files(diags, loaded.file_roots).resolve(&program);
    Some(FrontOutput { program, resolved })
}

/// Lex, parse, resolve, and type-check a source module, returning only whether
/// the whole front end passed. Bodies are checked after name resolution so the
/// checker sees the final tables.
pub fn frontend_checked(
    file_name: &str,
    source: &str,
    map: &mut SourceMap,
    diags: &DiagnosticSink,
) -> Option<FrontOutput> {
    let out = frontend(file_name, source, map, diags)?;
    check_program(&out.program, &out.resolved, diags);
    Some(out)
}

fn render_path(path: &[String]) -> String {
    path.join(".")
}

/// `A`, `B`, `C`, ... for the alias suggestions in ambiguity notes.
fn alias_suffix(i: usize) -> String {
    let mut n = i;
    let mut s = String::new();
    loop {
        s.push((b'A' + (n % 26) as u8) as char);
        n /= 26;
        if n == 0 {
            break;
        }
        n -= 1;
    }
    s
}

fn top_level_name(item: &Item) -> Option<&str> {
    match &item.kind {
        ItemKind::Fn(f) => Some(&f.name),
        ItemKind::Class(c) => Some(&c.name),
        ItemKind::Struct(s) => Some(&s.name),
        ItemKind::Enum(e) => Some(&e.name),
        ItemKind::Interface(i) => Some(&i.name),
        ItemKind::Const(c) => Some(&c.name),
        ItemKind::Global(g) => Some(&g.name),
        ItemKind::Test(f) => Some(&f.name),
    }
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
        | Stmt::Try { span, .. }
        | Stmt::Throw { span, .. }
        | Stmt::Expr(Expr { span, .. })
        | Stmt::Empty(span) => *span,
    }
}

/// What a single source file is allowed to see through its own imports.
#[derive(Default)]
struct FileImports {
    /// Importable module paths (full `a.b.c` paths from `import`/`use`), with
    /// the exported names of each resolved module.
    modules: HashMap<Vec<String>, ModuleBinding>,
    /// `import a.b as x` binds `x` to module path `a.b`.
    aliases: HashMap<String, Vec<String>>,
    /// `use a.b.item as x` binds local name `x` to exported item (`item`,
    /// provider module path). Aliased imports introduce the item to this file
    /// under the alias only.
    renames: HashMap<String, (String, Vec<String>)>,
    /// Public names this file may refer to bare (its own exports plus every
    /// `use a.b.item` / `use a.b.*` target), each with the provider module
    /// path(s). When a name resolves to two or more providers the reference
    /// is ambiguous and must be disambiguated with an alias.
    bindings: HashMap<String, Vec<String>>,
    /// The module path this file declares, when it declares one.
    own_module: Option<String>,
}

struct ModuleBinding {
    exports: Rc<HashSet<String>>,
    /// Module path owning the exported declarations; `None` means the binding
    /// key itself is the owning module (used to canonicalize collided flat
    /// names for qualified references).
    provider: Option<Vec<String>>,
}

/// Where a module path resolves on disk: a single `.pkl` module file, or a
/// package directory treated as a module for import purposes.
enum ModuleLoc {
    File(PathBuf),
    Package(PathBuf),
}

/// A workspace manifest (`picklescript.toml`): crate names mapped to the
/// directories that hold that crate's modules. Only the `[workspace]`
/// `members` array is read; everything else (package metadata, paths,
/// registry, dependencies) is reserved for the future package manager.
#[derive(Default)]
struct Workspace {
    crates: HashMap<String, PathBuf>,
}

impl Workspace {
    /// Find the nearest `picklescript.toml` at or above `start` and read its
    /// `[workspace] members` (paths resolved relative to the manifest file).
    /// The walk matches the module base search — up to (and including) `cwd`.
    fn discover(start: &Path) -> Workspace {
        let cwd = std::env::current_dir().ok();
        let mut dir = Some(if start.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            start.to_path_buf()
        });
        let mut seen = HashSet::new();
        while let Some(d) = dir {
            let canon = d.canonicalize().unwrap_or_else(|_| d.clone());
            if !seen.insert(canon.clone()) {
                break;
            }
            let manifest = d.join("picklescript.toml");
            if manifest.is_file() {
                if let Some(crates) = read_workspace_members(&manifest) {
                    return Workspace { crates };
                }
            }
            if let Some(c) = &cwd {
                if canon == c.canonicalize().unwrap_or_else(|_| c.clone()) {
                    break;
                }
            }
            dir = d.parent().map(Path::to_path_buf);
        }
        Workspace::default()
    }

    fn crate_dir(&self, name: &str) -> Option<&Path> {
        self.crates.get(name).map(|p| p.as_path())
    }
}

/// Read a `picklescript.toml` and return the `[workspace] members` mapping of
/// crate name to directory, resolved relative to the manifest's own directory.
/// Returns `None` when the file carries no usable `members` array.
fn read_workspace_members(manifest: &Path) -> Option<HashMap<String, PathBuf>> {
    let text = std::fs::read_to_string(manifest).ok()?;
    let base = manifest.parent().unwrap_or_else(|| Path::new("."));
    let raw = parse_workspace_members(&text);
    if raw.is_empty() {
        return None;
    }
    Some(
        raw.into_iter()
            .map(|(name, dir)| (name, base.join(dir)))
            .collect(),
    )
}

/// Parse the raw `[workspace] members = [ { name = "x", path = "y" }, ... ]`
/// list out of a manifest's text (a tight TOML subset; inline tables and
/// bare-string entries are accepted, everything else is ignored).
fn parse_workspace_members(text: &str) -> HashMap<String, String> {
    let mut members = HashMap::new();
    let mut section = String::new();
    let mut pending: Option<String> = None;
    for raw in text.lines() {
        let clean = strip_toml_comment(raw).trim();
        if clean.is_empty() {
            continue;
        }
        if let Some(buf) = pending.as_mut() {
            buf.push_str(clean);
            buf.push(' ');
            if array_is_closed(buf) {
                let full = std::mem::take(&mut pending).unwrap();
                parse_members_array(&full, &mut members);
            }
            continue;
        }
        if clean.starts_with('[') && clean.ends_with(']') {
            section = clean[1..clean.len() - 1].trim().to_string();
            continue;
        }
        if section == "workspace" {
            if let Some(eq) = clean.find('=') {
                if clean[..eq].trim() == "members" {
                    let value = clean[eq + 1..].trim();
                    if value.starts_with('[') {
                        if array_is_closed(value) {
                            parse_members_array(value, &mut members);
                        } else {
                            pending = Some(value.to_string());
                        }
                    }
                }
            }
        }
    }
    members
}

/// Whether a `[ ... ]` buffer has reached balanced brackets (its array is
/// complete), ignoring brackets inside quoted strings.
fn array_is_closed(s: &str) -> bool {
    let mut depth = 0i64;
    let mut in_str: Option<char> = None;
    let mut prev = '\0';
    for c in s.chars() {
        if let Some(q) = in_str {
            if c == q && prev != '\\' {
                in_str = None;
            }
        } else {
            match c {
                '"' | '\'' => in_str = Some(c),
                '[' => depth += 1,
                ']' => depth -= 1,
                _ => {}
            }
        }
        prev = c;
    }
    depth <= 0
}

/// Extract `{ name = "...", path = "..." }` tables (and bare string entries)
/// from an array body, writing each `name -> path` into `out`.
fn parse_members_array(array: &str, out: &mut HashMap<String, String>) {
    let mut rest = array;
    while let Some(start) = rest.find(['{', '"', '\'']) {
        let Some(c) = rest.chars().nth(start) else {
            break;
        };
        if c == '{' {
            let after = &rest[start + 1..];
            let Some(close) = matching_brace(after) else {
                break;
            };
            let table = &after[..close];
            if let (Some(name), Some(path)) = (extract_kv(table, "name"), extract_kv(table, "path"))
            {
                out.insert(name, path);
            }
            rest = &after[close + 1..];
        } else {
            let after = &rest[start + 1..];
            let Some(end) = after.find(c) else {
                break;
            };
            let value = after[..end].trim();
            if !value.is_empty() {
                out.insert(value.to_string(), value.to_string());
            }
            rest = &after[end + 1..];
        }
    }
}

/// Index just past the `}` closing the top-level `{ ... }` starting at the
/// beginning of `table`, ignoring braces inside quoted strings.
fn matching_brace(table: &str) -> Option<usize> {
    let mut depth = 1i64;
    let mut in_str: Option<char> = None;
    let mut prev = '\0';
    for (i, c) in table.char_indices() {
        if let Some(q) = in_str {
            if c == q && prev != '\\' {
                in_str = None;
            }
        } else {
            match c {
                '"' | '\'' => in_str = Some(c),
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
        }
        prev = c;
    }
    None
}

/// Pull `key = value` out of an inline table body, with the value given
/// bare, single-, or double-quoted.
fn extract_kv(table: &str, want: &str) -> Option<String> {
    let mut rest = table;
    loop {
        let idx = rest.find(want)?;
        let before = idx
            .checked_sub(1)
            .and_then(|i| rest.as_bytes().get(i))
            .copied();
        if matches!(before, Some(b) if b.is_ascii_alphanumeric() || b == b'_') {
            rest = &rest[idx + want.len()..];
            continue;
        }
        let after = rest[idx + want.len()..].trim_start();
        let Some(eq) = after.strip_prefix('=') else {
            rest = &rest[idx + want.len()..];
            continue;
        };
        let value = eq.trim_start();
        if value.starts_with('"') || value.starts_with('\'') {
            let q = value.chars().next().unwrap();
            let end = value[1..].find(q)?;
            return Some(value[1..1 + end].to_string());
        }
        let end = value.find([',', '}']).unwrap_or(value.len());
        let bare = value[..end].trim();
        return if bare.is_empty() {
            None
        } else {
            Some(bare.to_string())
        };
    }
}

/// Drop a trailing `#` comment that is not inside a quoted string.
fn strip_toml_comment(line: &str) -> &str {
    let mut in_str: Option<char> = None;
    let mut prev = '\0';
    for (i, c) in line.char_indices() {
        if let Some(q) = in_str {
            if c == q && prev != '\\' {
                in_str = None;
            }
        } else {
            match c {
                '#' => return &line[..i],
                '"' | '\'' => in_str = Some(c),
                _ => {}
            }
        }
        prev = c;
    }
    line
}

/// What a `from`-import (an `ImportItem`) resolved to.
enum ItemTarget {
    /// The imported path names a whole module file; its public items become
    /// available and its name binds for qualified access.
    ModuleFile { idx: usize, local: String },
    /// The imported path names one exported symbol of its provider module.
    Symbol {
        idx: usize,
        /// The exported item's own name within its module.
        name: String,
        /// The local binding (alias or last path segment).
        local: String,
    },
}

/// A namespace re-export of the form `pub import a.b` / `pub import a.b as x`:
/// consumers that load this module get a qualified `module.name.<export>`
/// binding pointing at the provider module's exports.
#[derive(Clone)]
struct PubNs {
    name: String,
    /// Module path owning the re-exported declarations.
    provider: Vec<String>,
    exports: Rc<HashSet<String>>,
}

impl FileImports {
    fn exports_of(&self, path: &[String]) -> Option<&HashSet<String>> {
        self.modules.get(path).map(|m| m.exports.as_ref())
    }

    /// The module path owning a bound module (used to canonicalize collided
    /// flat names); falls back to the bound path itself.
    fn provider_of(&self, path: &[String]) -> Vec<String> {
        self.modules
            .get(path)
            .and_then(|m| m.provider.clone())
            .unwrap_or_else(|| path.to_vec())
    }
}

struct LoadedModules {
    merged_program: Program,
    imports_for: HashMap<FileId, Rc<FileImports>>,
    file_roots: HashMap<FileId, String>,
    /// Item names exported by two or more loaded modules: references to them
    /// must be disambiguated before flat resolution.
    collided: HashSet<String>,
}

struct ModuleFile {
    file_path: PathBuf,
    fid: FileId,
    module_path: Vec<String>,
    exports: HashSet<String>,
    /// Package virtual modules list member exports as their own; such names
    /// must not count toward collision (the members themselves do).
    is_package: bool,
    pub_ns: Vec<PubNs>,
    /// Flattened symbol re-exports (`pub import { x, y } from A` /
    /// `pub import * from A`): local name as exported by this module, with
    /// the index of the module that actually defines it.
    pub_items: Vec<(String, usize)>,
    imports: Vec<ImportDecl>,
    program: Program,
}

struct Loader<'a> {
    map: &'a mut SourceMap,
    diags: &'a DiagnosticSink,
    loaded: HashMap<PathBuf, usize>,
    files: Vec<ModuleFile>,
    imports_for: HashMap<FileId, Rc<FileImports>>,
    file_roots: HashMap<FileId, String>,
    workspace: Workspace,
}

impl<'a> Loader<'a> {
    fn new(map: &'a mut SourceMap, diags: &'a DiagnosticSink) -> Loader<'a> {
        Loader {
            map,
            diags,
            loaded: HashMap::new(),
            files: Vec::new(),
            imports_for: HashMap::new(),
            file_roots: HashMap::new(),
            workspace: Workspace::default(),
        }
    }

    /// Parse the root module (source supplied by the caller) and all modules
    /// reachable from its imports, recursively. Returns the merged flat
    /// program plus the per-file import tables used by the rewrite pass.
    fn load_root(mut self, file_name: &str, source: &str) -> Option<LoadedModules> {
        let fid = self.map.add(file_name.to_string(), source.to_string());
        let program = self.parse_file(fid, source)?;
        let declared = program.module.as_ref().map(|m| m.path.clone());
        let module_path = declared.unwrap_or_default();
        let mut exports = HashSet::new();
        for item in &program.items {
            if let Some(n) = top_level_name(item) {
                exports.insert(n.to_string());
            }
        }
        let root_path = PathBuf::from(file_name);
        self.workspace = Workspace::discover(root_path.parent().unwrap_or_else(|| Path::new(".")));
        self.files.push(ModuleFile {
            file_path: root_path.clone(),
            fid,
            module_path,
            exports,
            is_package: false,
            pub_ns: Vec::new(),
            pub_items: Vec::new(),
            imports: program.imports.clone(),
            program,
        });
        self.loaded.insert(
            root_path
                .canonicalize()
                .unwrap_or_else(|_| root_path.clone()),
            0,
        );
        if !self.files[0].module_path.is_empty() {
            self.file_roots
                .insert(fid, self.files[0].module_path.join("."));
        }
        self.process_imports(0);

        let mut item_counts: HashMap<&str, usize> = HashMap::new();
        for f in &self.files {
            if f.module_path.is_empty() || f.is_package {
                continue;
            }
            for n in &f.exports {
                *item_counts.entry(n.as_str()).or_default() += 1;
            }
        }
        let collided = item_counts
            .into_iter()
            .filter(|(_, count)| *count > 1)
            .map(|(n, _)| n.to_string())
            .collect();

        let mut items = Vec::new();
        for f in &self.files {
            items.extend(f.program.items.iter().cloned());
        }
        let merged_program = Program {
            module: self.files[0].program.module.clone(),
            imports: Vec::new(),
            items,
        };
        Some(LoadedModules {
            merged_program,
            imports_for: self.imports_for,
            file_roots: self.file_roots,
            collided,
        })
    }

    fn parse_file(&mut self, fid: FileId, source: &str) -> Option<Program> {
        let tokens = crate::lexer::lex(fid, source, self.diags);
        parse(tokens, self.diags).ok()
    }

    /// Where a module path points: a single module file, or a package
    /// directory whose members are module files (`x/y.pkl` for the module
    /// path `pkg.x.y` imported from that package).
    fn resolve_module_loc(&self, importer: &Path, segments: &[String]) -> Option<ModuleLoc> {
        if segments.is_empty() {
            return None;
        }
        let mut rel = PathBuf::new();
        for s in segments {
            rel.push(s);
        }
        let rel_file = rel.with_extension("pkl");

        // A leading workspace crate name resolves inside that crate's own
        // directory: the crate root maps to `lib.pkl`/`main.pkl` and each
        // deeper segment to a module file or package dir within the crate.
        // A crate name is authoritative, so no other base is searched.
        if let Some(crate_dir) = self.workspace.crate_dir(&segments[0]) {
            if segments.len() == 1 {
                for stem in ["lib", "main"] {
                    let f = crate_dir.join(format!("{stem}.pkl"));
                    if f.is_file() {
                        return Some(ModuleLoc::File(f));
                    }
                }
                return None;
            }
            let mut sub = PathBuf::new();
            for s in &segments[1..] {
                sub.push(s);
            }
            let f = crate_dir.join(sub.with_extension("pkl"));
            if f.is_file() {
                return Some(ModuleLoc::File(f));
            }
            let p = crate_dir.join(&sub);
            if p.is_dir() {
                return Some(ModuleLoc::Package(p));
            }
            return None;
        }

        // Bases searched per location: the importing file's directory, then
        // each ancestor up to (and including) the process working directory,
        // then finally `<cwd>/src/`. A directory base is a candidate for a
        // file (segments joined with `.pkl`) or a package (a real directory).
        let mut bases: Vec<PathBuf> = Vec::new();
        let cwd = std::env::current_dir().ok();
        let mut dir = match importer.parent() {
            Some(p) if !p.as_os_str().is_empty() => Some(p.to_path_buf()),
            _ => Some(PathBuf::from(".")),
        };
        let mut seen = HashSet::new();
        while let Some(d) = dir {
            let canon = d.canonicalize().unwrap_or_else(|_| d.clone());
            if !seen.insert(canon) {
                break;
            }
            bases.push(d.clone());
            if let Some(c) = &cwd {
                if d == *c {
                    break;
                }
            }
            dir = d.parent().map(Path::to_path_buf);
        }
        if let Some(c) = &cwd {
            if c.parent().is_none() {
                // cwd is a filesystem root; nothing above.
            }
            bases.push(c.join("src"));
        }

        for base in bases {
            let f = base.join(&rel_file);
            if f.is_file() {
                return Some(ModuleLoc::File(f));
            }
            let p = base.join(&rel);
            if p.is_dir() {
                return Some(ModuleLoc::Package(p));
            }
        }
        None
    }

    /// Load the module `segments` (if not already loaded) and return its
    /// index. Emits `ModuleNotFound` when there is no matching file or
    /// package directory.
    fn ensure_module(&mut self, importer: &Path, segments: &[String], span: Span) -> Option<usize> {
        if segments.is_empty() {
            self.diags.emit(
                Diagnostic::error_at(span, "module path must not be empty")
                    .with_code(ErrorCode::ModuleNotFound),
            );
            return None;
        }
        match self.resolve_module_loc(importer, segments) {
            Some(ModuleLoc::File(file)) => self.load_file(&file, Some((segments, span))),
            Some(ModuleLoc::Package(dir)) => self.load_package(&dir, Some((segments, span))),
            None => {
                let mut note = "searched up the importing file's directory tree, then `<cwd>/src/`"
                    .to_string();
                if let Some(dir) = self.workspace.crate_dir(&segments[0]) {
                    note.push_str(&format!(
                        "; then inside workspace crate `{}` at `{}`",
                        segments[0],
                        dir.display()
                    ));
                }
                self.diags.emit(
                    Diagnostic::error_at(
                        span,
                        format!("cannot find module `{}`", render_path(segments)),
                    )
                    .with_code(ErrorCode::ModuleNotFound)
                    .note(note),
                );
                None
            }
        }
    }

    fn load_file(&mut self, path: &Path, imported_as: Option<(&[String], Span)>) -> Option<usize> {
        let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Some(&i) = self.loaded.get(&canon) {
            return Some(i);
        }
        let source = std::fs::read_to_string(path).ok()?;
        let fid = self.map.add(path.display().to_string(), source.clone());
        let program = self.parse_file(fid, &source)?;

        let declared = program.module.as_ref().map(|m| m.path.clone());
        if let (Some(decl_path), Some((import_path, ispan))) = (declared.as_ref(), imported_as) {
            if decl_path != import_path {
                self.diags.emit(
                    Diagnostic::error_at(
                        ispan,
                        format!(
                            "file `{}` declares module `{}`, not `{}`",
                            path.display(),
                            render_path(decl_path),
                            render_path(import_path),
                        ),
                    )
                    .with_code(ErrorCode::ModuleMismatch),
                );
            }
        }
        let module_path = declared
            .clone()
            .or_else(|| imported_as.map(|(ip, _)| ip.to_vec()))
            .unwrap_or_default();

        let mut exports = HashSet::new();
        for item in &program.items {
            if let Some(n) = top_level_name(item) {
                exports.insert(n.to_string());
            }
        }
        let idx = self.files.len();
        self.files.push(ModuleFile {
            file_path: path.to_path_buf(),
            fid,
            module_path,
            exports,
            is_package: false,
            pub_ns: Vec::new(),
            pub_items: Vec::new(),
            imports: program.imports.clone(),
            program,
        });
        self.loaded.insert(canon, idx);
        if !self.files[idx].module_path.is_empty() {
            self.file_roots
                .insert(fid, self.files[idx].module_path.join("."));
        }
        self.process_imports(idx);
        self.finish_exports(idx);
        Some(idx)
    }

    /// After imports are processed, public re-exports contribute names to the
    /// module's export surface (namespaces stay qualified-only; flattened
    /// items are visible to wildcard/item imports of this module).
    fn finish_exports(&mut self, idx: usize) {
        let ns: Vec<String> = self.files[idx]
            .pub_ns
            .iter()
            .map(|n| n.name.clone())
            .collect();
        let items: Vec<String> = self.files[idx]
            .pub_items
            .iter()
            .map(|(n, _)| n.clone())
            .collect();
        self.files[idx].exports.extend(ns);
        self.files[idx].exports.extend(items);
    }

    /// Materialize a package directory as a module node: load every member
    /// module file (`*.pkl`, non-recursive) as its own module and expose the
    /// union of their exports as the package's exports.
    fn load_package(
        &mut self,
        dir: &Path,
        imported_as: Option<(&[String], Span)>,
    ) -> Option<usize> {
        let segments = imported_as.map(|(s, _)| s.to_vec()).unwrap_or_default();
        let span = imported_as.map(|(_, s)| s);
        let mut exports = HashSet::new();
        let entries = std::fs::read_dir(dir).ok()?;
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().map(|e| e == "pkl").unwrap_or(false) && p.is_file() {
                let stem = p
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let mut mpath = segments.clone();
                mpath.push(stem);
                if let Some(idx) = self.load_file(&p, span.map(|s| (mpath.as_slice(), s))) {
                    exports.extend(self.files[idx].exports.clone());
                }
            }
        }
        let fid = self.map.add(dir.display().to_string(), String::new());
        let idx = self.files.len();
        self.files.push(ModuleFile {
            file_path: dir.to_path_buf(),
            fid,
            module_path: segments.clone(),
            exports,
            is_package: true,
            pub_ns: Vec::new(),
            pub_items: Vec::new(),
            imports: Vec::new(),
            program: Program {
                module: None,
                imports: Vec::new(),
                items: Vec::new(),
            },
        });
        let canon = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        self.loaded.insert(canon, idx);
        let mut fi = FileImports::default();
        if !segments.is_empty() {
            let own_provider = segments.join(".");
            for name in self.files[idx].exports.clone() {
                fi.bindings
                    .entry(name)
                    .or_default()
                    .push(own_provider.clone());
            }
            fi.own_module = Some(own_provider);
        }
        self.imports_for.insert(fid, Rc::new(fi));
        Some(idx)
    }

    /// Build the import tables for one module and load its dependencies.
    fn process_imports(&mut self, idx: usize) {
        let imports = self.files[idx].imports.clone();
        let importer = self.files[idx].file_path.clone();
        let mut fi = FileImports::default();
        let mut bound_aliases = HashSet::new();
        let mut bound_symbols = HashSet::new();
        for imp in &imports {
            match &imp.kind {
                ImportKind::Module { alias } => {
                    let target = self.ensure_module(&importer, &imp.source, imp.span);
                    let exports = target
                        .map(|t| self.files[t].exports.clone())
                        .unwrap_or_default();
                    let alias_name = alias
                        .clone()
                        .unwrap_or_else(|| imp.source.last().cloned().unwrap_or_default());
                    if !bound_aliases.insert(alias_name.clone())
                        || fi.aliases.contains_key(&alias_name)
                    {
                        self.diags.emit(
                            Diagnostic::error_at(
                                imp.span,
                                format!("duplicate import alias `{alias_name}`"),
                            )
                            .with_code(ErrorCode::DuplicateImportAlias),
                        );
                    }
                    if imp.is_public {
                        if let Some(t) = target {
                            let texports = self.files[t].exports.clone();
                            let tpath = self.files[t].module_path.clone();
                            self.files[idx].pub_ns.push(PubNs {
                                name: alias_name.clone(),
                                provider: tpath,
                                exports: Rc::new(texports),
                            });
                        }
                    }
                    fi.aliases.insert(alias_name, imp.source.clone());
                    fi.modules.insert(
                        imp.source.clone(),
                        ModuleBinding {
                            exports: Rc::new(exports),
                            provider: None,
                        },
                    );
                    if let Some(t) = target {
                        self.splice_ns_reexports(t, &mut fi);
                    }
                }
                ImportKind::Items { items } => {
                    for item in items {
                        match self.load_item(&importer, &imp.source, item) {
                            None => {}
                            Some(ItemTarget::ModuleFile { idx: tid, local }) => {
                                if !bound_aliases.insert(local.clone())
                                    || fi.aliases.contains_key(&local)
                                {
                                    self.diags.emit(
                                        Diagnostic::error_at(
                                            item.span,
                                            format!("duplicate import alias `{local}`"),
                                        )
                                        .with_code(ErrorCode::DuplicateImportAlias),
                                    );
                                }
                                let target_path = self.files[tid].module_path.clone();
                                let exports = self.files[tid].exports.clone();
                                if imp.is_public {
                                    self.files[idx].pub_ns.push(PubNs {
                                        name: local.clone(),
                                        provider: target_path.clone(),
                                        exports: Rc::new(exports.clone()),
                                    });
                                }
                                fi.aliases.insert(local, target_path.clone());
                                fi.modules.insert(
                                    target_path,
                                    ModuleBinding {
                                        exports: Rc::new(exports),
                                        provider: None,
                                    },
                                );
                                self.splice_ns_reexports(tid, &mut fi);
                            }
                            Some(ItemTarget::Symbol {
                                idx: tid,
                                name,
                                local,
                            }) => {
                                let provider = self.files[tid].module_path.clone();
                                if imp.is_public {
                                    self.files[idx].pub_items.push((name.clone(), tid));
                                }
                                fi.modules.entry(provider.clone()).or_insert(ModuleBinding {
                                    exports: Rc::new(self.files[tid].exports.clone()),
                                    provider: None,
                                });
                                match &item.alias {
                                    Some(_) => {
                                        if fi.renames.contains_key(&local)
                                            || bound_symbols.contains(&local)
                                        {
                                            self.diags.emit(
                                                Diagnostic::error_at(
                                                    item.span,
                                                    format!("duplicate import alias `{local}`"),
                                                )
                                                .with_code(ErrorCode::DuplicateImportAlias),
                                            );
                                        }
                                        bound_symbols.insert(local.clone());
                                        fi.renames.insert(local, (name, provider));
                                    }
                                    None => {
                                        fi.bindings
                                            .entry(name.clone())
                                            .or_default()
                                            .push(render_path(&provider));
                                    }
                                }
                            }
                        }
                    }
                    if let Some(t) = self.loaded_for(&importer, &imp.source) {
                        self.splice_ns_reexports(t, &mut fi);
                    }
                }
                ImportKind::Wildcard => {
                    let target = self.ensure_module(&importer, &imp.source, imp.span);
                    let exports = target
                        .map(|t| self.files[t].exports.clone())
                        .unwrap_or_default();
                    for name in &exports {
                        let def = target.and_then(|t| {
                            self.files[t]
                                .pub_items
                                .iter()
                                .find(|(n, _)| n == name)
                                .map(|(_, i)| *i)
                        });
                        let provider = match def {
                            Some(d) => render_path(&self.files[d].module_path),
                            None => render_path(&imp.source),
                        };
                        fi.bindings.entry(name.clone()).or_default().push(provider);
                        if imp.is_public {
                            if let Some(t) = target {
                                self.files[idx]
                                    .pub_items
                                    .push((name.clone(), def.unwrap_or(t)));
                            }
                        }
                    }
                    fi.modules
                        .entry(imp.source.clone())
                        .or_insert(ModuleBinding {
                            exports: Rc::new(exports),
                            provider: None,
                        });
                    if let Some(t) = target {
                        self.splice_ns_reexports(t, &mut fi);
                    }
                }
            }
        }
        let own = self.files[idx].module_path.clone();
        if !own.is_empty() {
            let own_provider = own.join(".");
            for name in self.files[idx].exports.clone() {
                fi.bindings
                    .entry(name)
                    .or_default()
                    .push(own_provider.clone());
            }
            fi.own_module = Some(own_provider);
        }
        self.imports_for.insert(self.files[idx].fid, Rc::new(fi));
    }

    /// Resolve one `from`-import item inside its source module.
    ///
    /// The item path is resolved as the deepest module-file chain inside the
    /// source's tree first (`source/item1/.../itemT.pkl`), so `import Token
    /// from lexer` binds the module file `lexer/Token.pkl` and
    /// `import math.Vector.add from engine` (when `engine/math/Vector.pkl`
    /// exists) binds the final export `add`. Otherwise the path is looked up
    /// as a direct exported symbol of the source module itself.
    fn load_item(
        &mut self,
        importer: &Path,
        source: &[String],
        item: &ImportItem,
    ) -> Option<ItemTarget> {
        let n = item.path.len();
        if n == 0 {
            self.diags.emit(
                Diagnostic::error_at(item.span, "imported name must not be empty")
                    .with_code(ErrorCode::ModuleNotFound),
            );
            return None;
        }
        for t in (1..=n).rev() {
            let rem = &item.path[t..];
            if rem.len() > 1 {
                continue;
            }
            let mut segs = source.to_vec();
            segs.extend_from_slice(&item.path[..t]);
            if let Some(ModuleLoc::File(f)) = self.resolve_module_loc(importer, &segs) {
                if let Some(idx) = self.load_file(&f, Some((&segs, item.span))) {
                    return match rem.first() {
                        Some(name) => {
                            if !self.files[idx].exports.contains(name) {
                                self.diags.emit(
                                    Diagnostic::error_at(
                                        item.span,
                                        format!(
                                            "`{name}` is not exported by module `{}`",
                                            render_path(&segs)
                                        ),
                                    )
                                    .with_code(ErrorCode::NotExported),
                                );
                                return None;
                            }
                            let local = item.alias.clone().unwrap_or_else(|| name.clone());
                            Some(ItemTarget::Symbol {
                                idx,
                                name: name.clone(),
                                local,
                            })
                        }
                        None => {
                            let local = item
                                .alias
                                .clone()
                                .unwrap_or_else(|| item.path.last().cloned().unwrap_or_default());
                            Some(ItemTarget::ModuleFile { idx, local })
                        }
                    };
                }
            }
        }
        if let Some(idx) = self.ensure_module(importer, source, item.span) {
            if n == 1 {
                let name = &item.path[0];
                if self.files[idx].exports.contains(name) {
                    // A re-exported symbol resolves to its defining module so
                    // ambiguity/canonicalization sees the real provider.
                    let def = self.files[idx]
                        .pub_items
                        .iter()
                        .find(|(n, _)| n == name)
                        .map(|(_, i)| *i)
                        .unwrap_or(idx);
                    let local = item.alias.clone().unwrap_or_else(|| name.clone());
                    return Some(ItemTarget::Symbol {
                        idx: def,
                        name: name.clone(),
                        local,
                    });
                }
            }
            self.diags.emit(
                Diagnostic::error_at(
                    item.span,
                    format!(
                        "`{}` is not exported by module `{}`",
                        item.path.join("."),
                        render_path(source)
                    ),
                )
                .with_code(ErrorCode::NotExported),
            );
        }
        None
    }

    /// Whether the source has already been loaded (its index), without
    /// emitting a `ModuleNotFound` for an unresolvable path.
    fn loaded_for(&self, importer: &Path, segments: &[String]) -> Option<usize> {
        match self.resolve_module_loc(importer, segments) {
            Some(ModuleLoc::File(f)) => {
                let c = f.canonicalize().unwrap_or(f);
                self.loaded.get(&c).copied()
            }
            Some(ModuleLoc::Package(d)) => {
                let c = d.canonicalize().unwrap_or(d);
                self.loaded.get(&c).copied()
            }
            None => None,
        }
    }

    /// Copy `pub import a.b` namespace re-exports of a loaded module into the
    /// importing file's scope, so `module.reexport.symbol` resolves.
    fn splice_ns_reexports(&mut self, tidx: usize, fi: &mut FileImports) {
        let ns = self.files[tidx].pub_ns.clone();
        let tpath = self.files[tidx].module_path.clone();
        for p in ns {
            let mut full = tpath.clone();
            full.push(p.name.clone());
            if fi.modules.contains_key(&full) {
                continue;
            }
            fi.modules.insert(
                full,
                ModuleBinding {
                    exports: p.exports,
                    provider: Some(p.provider),
                },
            );
        }
    }
}

/// Rewrite module-qualified references (`compiler.lexer.Lexer(...)`, `lex.X`,
/// `use a.b.item as x`) into the plain names the flat resolver understands.
/// The rewrite is driven per source file by that file's own imports.
struct Rewriter<'a> {
    ctx: &'a HashMap<FileId, Rc<FileImports>>,
    /// Item names exported by more than one loaded module. Such items are
    /// flattened under their module path (`math.ops.helper`), and any bare
    /// reference with several in-scope providers is an error until the
    /// programmer aliases it.
    collided: &'a HashSet<String>,
    diags: &'a DiagnosticSink,
}

impl<'a> Rewriter<'a> {
    fn rewrite_program(&self, program: &mut Program) {
        for item in &mut program.items {
            self.rewrite_item(item);
        }
    }

    fn ctx_for(&self, fid: FileId) -> Option<&FileImports> {
        self.ctx.get(&fid).map(|c| c.as_ref())
    }

    /// The flat name of `item` declared by module `provider`: the plain item
    /// name when it is unique, otherwise the module-qualified name.
    fn canon(&self, item: &str, provider: &str) -> String {
        if self.collided.contains(item) {
            format!("{provider}.{item}")
        } else {
            item.to_string()
        }
    }

    /// Resolve a bare in-scope name. A single provider canonicalizes; two or
    /// more is the ambiguity the aliasing syntax exists to resolve.
    fn canonical_or_ambiguous(&self, name: &str, providers: &[String], span: Span) -> Option<Expr> {
        let mut uniq: Vec<&String> = providers.iter().collect();
        uniq.sort_unstable();
        uniq.dedup();
        match uniq.len() {
            0 => None,
            1 => Some(qualified_expr(span, &[self.canon(name, uniq[0])])),
            _ => {
                let mut diag =
                    Diagnostic::error_at(span, format!("ambiguous imported name `{name}`"))
                        .with_code(ErrorCode::AmbiguousImport);
                let mut note = format!("`{name}` is provided by module `{}`", uniq[0]);
                for p in &uniq[1..] {
                    note.push_str(&format!(" and module `{p}`"));
                }
                diag = diag.note(note);
                let mut suggestion = String::new();
                for (i, p) in uniq.iter().enumerate() {
                    if i > 0 {
                        suggestion.push_str(" / ");
                    }
                    suggestion.push_str(&format!(
                        "import {name} from `{p}` as {name}{}",
                        alias_suffix(i)
                    ));
                }
                diag = diag.note(suggestion);
                self.diags.emit(diag);
                Some(qualified_expr(span, &[name.to_string()]))
            }
        }
    }

    fn rewrite_item(&self, item: &mut Item) {
        match &mut item.kind {
            ItemKind::Fn(f) => {
                self.rename_decl(&mut f.name, item.span.file);
                self.rewrite_fn(f);
            }
            ItemKind::Test(f) => {
                self.rename_decl(&mut f.name, item.span.file);
                self.rewrite_fn(f);
            }
            ItemKind::Class(c) => {
                if let Some(te) = &mut c.extends {
                    self.rewrite_type(te);
                }
                for te in &mut c.implements {
                    self.rewrite_type(te);
                }
                for m in &mut c.members {
                    self.rewrite_class_member(m);
                }
            }
            ItemKind::Struct(s) => {
                for te in &mut s.implements {
                    self.rewrite_type(te);
                }
                for m in &mut s.members {
                    self.rewrite_class_member(m);
                }
            }
            ItemKind::Enum(e) => {
                for te in &mut e.implements {
                    self.rewrite_type(te);
                }
                for v in &mut e.variants {
                    for f in &mut v.fields {
                        self.rewrite_type(&mut f.ty);
                    }
                }
            }
            ItemKind::Interface(i) => {
                for te in &mut i.extends {
                    self.rewrite_type(te);
                }
                for m in &mut i.members {
                    match m {
                        InterfaceMember::Method(m) => self.rewrite_method(m),
                        InterfaceMember::Property { ty, .. } => self.rewrite_type(ty),
                        InterfaceMember::Const { value, .. } => self.rewrite_expr(value),
                    }
                }
            }
            ItemKind::Const(c) => {
                self.rename_decl(&mut c.name, item.span.file);
                if let Some(ty) = &mut c.ty {
                    self.rewrite_type(ty);
                }
                self.rewrite_expr(&mut c.value);
            }
            ItemKind::Global(g) => {
                self.rename_decl(&mut g.name, item.span.file);
                if let Some(ty) = &mut g.ty {
                    self.rewrite_type(ty);
                }
                self.rewrite_expr(&mut g.value);
            }
        }
    }

    /// A collided top-level function, test, or const declared by a module file
    /// is flattened under its module-qualified name so the merged program
    /// keeps distinct table entries and emitted symbols per provider.
    fn rename_decl(&self, name: &mut String, fid: FileId) {
        let Some(provider) = self.ctx_for(fid).and_then(|c| c.own_module.clone()) else {
            return;
        };
        if self.collided.contains(name.as_str()) {
            *name = format!("{provider}.{name}");
        }
    }

    fn rewrite_class_member(&self, m: &mut ClassMember) {
        match m {
            ClassMember::Field {
                ty, init, attrs, ..
            } => {
                if let Some(ty) = ty {
                    self.rewrite_type(ty);
                }
                if let Some(init) = init {
                    self.rewrite_expr(init);
                }
                for a in attrs {
                    for e in &mut a.args {
                        self.rewrite_expr(e);
                    }
                }
            }
            ClassMember::Method(m) => self.rewrite_method(m),
            ClassMember::Constructor(ctor) => {
                for p in &mut ctor.params {
                    self.rewrite_param(p);
                }
                self.rewrite_block(&mut ctor.body);
            }
            ClassMember::Property(p) => {
                if let Some(ty) = &mut p.ty {
                    self.rewrite_type(ty);
                }
                if let Some(get) = &mut p.get {
                    self.rewrite_accessor(get);
                }
                if let Some(set) = &mut p.set {
                    self.rewrite_accessor(set);
                }
            }
            ClassMember::Init(b) => self.rewrite_block(b),
            ClassMember::Deinit(b) => self.rewrite_block(b),
            ClassMember::Const { ty, value, .. } => {
                if let Some(ty) = ty {
                    self.rewrite_type(ty);
                }
                self.rewrite_expr(value);
            }
        }
    }

    fn rewrite_accessor(&self, acc: &mut PropertyAccessor) {
        match acc {
            PropertyAccessor::Expr(e) => self.rewrite_expr(e),
            PropertyAccessor::Block(b) => self.rewrite_block(b),
        }
    }

    fn rewrite_fn(&self, f: &mut FnDecl) {
        for p in &mut f.params {
            self.rewrite_param(p);
        }
        if let Some(ty) = &mut f.return_ty {
            self.rewrite_type(ty);
        }
        if let Some(body) = &mut f.body {
            self.rewrite_body(body);
        }
    }

    fn rewrite_method(&self, m: &mut MethodDecl) {
        for p in &mut m.params {
            self.rewrite_param(p);
        }
        if let Some(ty) = &mut m.return_ty {
            self.rewrite_type(ty);
        }
        if let Some(body) = &mut m.body {
            self.rewrite_body(body);
        }
    }

    fn rewrite_param(&self, p: &mut Param) {
        if let Some(ty) = &mut p.ty {
            self.rewrite_type(ty);
        }
        if let Some(def) = &mut p.default {
            self.rewrite_expr(def);
        }
        for a in &mut p.attrs {
            for e in &mut a.args {
                self.rewrite_expr(e);
            }
        }
    }

    fn rewrite_body(&self, body: &mut FnBody) {
        match body {
            FnBody::Block(b) => self.rewrite_block(b),
            FnBody::Expr(e) => self.rewrite_expr(e),
        }
    }

    fn rewrite_block(&self, block: &mut Block) {
        for s in &mut block.stmts {
            self.rewrite_stmt(s);
        }
        if let Some(e) = &mut block.expr {
            self.rewrite_expr(e);
        }
    }

    fn rewrite_stmt(&self, stmt: &mut Stmt) {
        let fid = stmt_span(stmt).file;
        match stmt {
            Stmt::Let {
                pattern,
                ty,
                init,
                attrs,
                ..
            } => {
                self.rewrite_pattern(pattern, fid);
                if let Some(ty) = ty {
                    self.rewrite_type(ty);
                }
                if let Some(init) = init {
                    self.rewrite_expr(init);
                }
                for a in attrs {
                    for e in &mut a.args {
                        self.rewrite_expr(e);
                    }
                }
            }
            Stmt::Const { ty, value, .. } => {
                if let Some(ty) = ty {
                    self.rewrite_type(ty);
                }
                self.rewrite_expr(value);
            }
            Stmt::Return { value: Some(v), .. } => {
                self.rewrite_expr(v);
            }
            Stmt::Return { value: None, .. } => {}
            Stmt::While { cond, body, .. } => {
                self.rewrite_expr(cond);
                self.rewrite_block(body);
            }
            Stmt::For { header, body, .. } => {
                match header {
                    ForHeader::In { pattern, sequence } => {
                        self.rewrite_pattern(pattern, fid);
                        self.rewrite_expr(sequence);
                    }
                    ForHeader::Range { init, cond, step } => {
                        self.rewrite_stmt(init);
                        self.rewrite_expr(cond);
                        self.rewrite_expr(step);
                    }
                }
                self.rewrite_block(body);
            }
            Stmt::Expr(e) => self.rewrite_expr(e),
            Stmt::Try {
                body,
                handler,
                finally,
                ..
            } => {
                // All three blocks carry expressions that can name an imported
                // item, so a qualified call inside a `try` body, its `catch`, or
                // its `finally` has to be rewritten like any other. The binder
                // is a plain name, not an expression, so there is nothing to
                // visit.
                self.rewrite_block(body);
                if let Some(h) = handler {
                    self.rewrite_block(h);
                }
                if let Some(f) = finally {
                    self.rewrite_block(f);
                }
            }
            Stmt::Throw { value, .. } => self.rewrite_expr(value),
            // `Break`, `Continue` and `Empty` carry no subexpressions, so
            // leaving them alone is correct rather than an oversight.
            _ => {}
        }
    }

    fn rewrite_expr(&self, e: &mut Expr) {
        let ctx = self.ctx_for(e.span.file);
        let fid = e.span.file;
        if let Some(segs) = flatten_member_spine(e) {
            if let Some(rewritten) = self.match_chain(&segs, e.span, ctx) {
                *e = rewritten;
                return;
            }
        }
        match &mut e.kind {
            ExprKind::Call { callee, args } => {
                self.rewrite_expr(callee);
                for a in args {
                    self.rewrite_expr(&mut a.value);
                }
            }
            ExprKind::Member { object, .. } => self.rewrite_expr(object),
            ExprKind::OptAccess { object, .. } => self.rewrite_expr(object),
            ExprKind::Index { object, index } => {
                self.rewrite_expr(object);
                self.rewrite_expr(index);
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.rewrite_expr(lhs);
                self.rewrite_expr(rhs);
            }
            ExprKind::Unary { operand, .. } => self.rewrite_expr(operand),
            ExprKind::Assign { target, value, .. } => {
                self.rewrite_expr(target);
                self.rewrite_expr(value);
            }
            ExprKind::Lambda {
                params,
                return_ty,
                body,
                ..
            } => {
                for p in params {
                    self.rewrite_param(p);
                }
                if let Some(ty) = return_ty {
                    self.rewrite_type(ty);
                }
                self.rewrite_body(body);
            }
            ExprKind::If {
                cond,
                then,
                else_else,
            } => {
                match cond {
                    IfCond::Cond(e) => self.rewrite_expr(e),
                    IfCond::Binding { pattern, value } => {
                        self.rewrite_pattern(pattern, fid);
                        self.rewrite_expr(value);
                    }
                }
                self.rewrite_block(then);
                if let Some(e) = else_else {
                    self.rewrite_expr(e);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.rewrite_expr(scrutinee);
                for arm in arms {
                    self.rewrite_pattern(&mut arm.pattern, arm.span.file);
                    if let Some(g) = &mut arm.guard {
                        self.rewrite_expr(g);
                    }
                    self.rewrite_expr(&mut arm.body);
                }
            }
            ExprKind::Await(e) => self.rewrite_expr(e),
            ExprKind::GenericCall { type_args, .. } => {
                for t in type_args {
                    self.rewrite_type(t);
                }
            }
            ExprKind::Cast { expr, ty, .. } => {
                self.rewrite_expr(expr);
                self.rewrite_type(ty);
            }
            ExprKind::Unsafe(b) => self.rewrite_block(b),
            ExprKind::Block(b) => self.rewrite_block(b),
            ExprKind::Tuple(es) | ExprKind::Array(es) => {
                for x in es {
                    self.rewrite_expr(x);
                }
            }
            ExprKind::Map(kvs) => {
                for (k, v) in kvs {
                    self.rewrite_expr(k);
                    self.rewrite_expr(v);
                }
            }
            ExprKind::OptUnwrap(e) => self.rewrite_expr(e),
            ExprKind::Range { start, end, .. } => {
                self.rewrite_expr(start);
                self.rewrite_expr(end);
            }
            _ => {}
        }
    }

    fn rewrite_type(&self, te: &mut TypeExpr) {
        let ctx = self.ctx_for(te.span.file);
        match &mut te.kind {
            TypeExprKind::Path(parts) => {
                if let Some(mut new) = self.match_type_path(parts, ctx) {
                    if new.len() == 1 {
                        if let Some(c) = ctx {
                            if let Some(item) = c.renames.get(&new[0]) {
                                new[0] = item.0.clone();
                            }
                        }
                    }
                    *parts = new;
                }
            }
            TypeExprKind::Generic(inner, args) => {
                self.rewrite_type(inner);
                for a in args {
                    self.rewrite_type(a);
                }
            }
            TypeExprKind::Pointer(t) | TypeExprKind::Ref(t) | TypeExprKind::Option(t) => {
                self.rewrite_type(t);
            }
            TypeExprKind::Tuple(ts) => {
                for t in ts {
                    self.rewrite_type(t);
                }
            }
            TypeExprKind::Fn { params, ret, .. } => {
                for t in params {
                    self.rewrite_type(t);
                }
                if let Some(r) = ret {
                    self.rewrite_type(r);
                }
            }
            _ => {}
        }
    }

    fn rewrite_pattern(&self, p: &mut Pattern, fid: FileId) {
        let ctx = self.ctx_for(fid);
        match p {
            Pattern::Binding { ty: Some(ty), .. } => {
                self.rewrite_type(ty);
            }
            Pattern::Binding { ty: None, .. } => {}
            Pattern::Tuple(ps) => {
                for q in ps {
                    self.rewrite_pattern(q, fid);
                }
            }
            Pattern::Variant { path, payloads } => {
                if let Some(c) = ctx {
                    if path.len() >= 2 {
                        if let Some(k) = longest_prefix(path, c) {
                            *path = path[k..].to_vec();
                        } else if c.aliases.contains_key(&path[0]) {
                            *path = path[1..].to_vec();
                        }
                    }
                    if !path.is_empty() {
                        if let Some(item) = c.renames.get(&path[0]) {
                            path[0] = item.0.clone();
                        }
                    }
                }
                for q in payloads {
                    self.rewrite_pattern(q, fid);
                }
            }
            Pattern::Or(ps) => {
                for q in ps {
                    self.rewrite_pattern(q, fid);
                }
            }
            _ => {}
        }
    }

    /// Resolve a dotted or aliased chain to a plain name when the prefix is a
    /// module path (or module alias) in scope, and the next segment is one of
    /// that module's exports. Returns `None` when the chain is not a module
    /// reference, in which case it is left to the checker as an ordinary
    /// member access.
    fn match_chain(&self, segs: &[String], span: Span, ctx: Option<&FileImports>) -> Option<Expr> {
        let ctx = ctx?;
        if segs.len() == 1 {
            let name = &segs[0];
            if let Some((item, provider)) = ctx.renames.get(name) {
                return Some(qualified_expr(
                    span,
                    &[self.canon(item, &render_path(provider))],
                ));
            }
            if let Some(providers) = ctx.bindings.get(name) {
                return self.canonical_or_ambiguous(name, providers, span);
            }
            return None;
        }
        if let Some(k) = longest_prefix(segs, ctx) {
            let prefix = &segs[..k];
            let remaining = &segs[k..];
            if ctx.exports_of(prefix)?.contains(&remaining[0]) {
                if remaining.len() == 1 && self.collided.contains(&remaining[0]) {
                    return Some(qualified_expr(
                        span,
                        &[self.canon(&remaining[0], &render_path(&ctx.provider_of(prefix)))],
                    ));
                }
                return Some(qualified_expr(span, remaining));
            }
            return None;
        }
        if let Some(mod_path) = ctx.aliases.get(&segs[0]) {
            let remaining = &segs[1..];
            if !remaining.is_empty() && ctx.exports_of(mod_path)?.contains(&remaining[0]) {
                if remaining.len() == 1 && self.collided.contains(&remaining[0]) {
                    return Some(qualified_expr(
                        span,
                        &[self.canon(&remaining[0], &render_path(&ctx.provider_of(mod_path)))],
                    ));
                }
                return Some(qualified_expr(span, remaining));
            }
            return None;
        }
        None
    }

    /// For `TypeExprKind::Path`: drop a matched module prefix, or return the
    /// path unchanged for a bare single segment. Renames for single segments
    /// are applied by the caller.
    fn match_type_path(&self, parts: &[String], ctx: Option<&FileImports>) -> Option<Vec<String>> {
        if parts.len() <= 1 {
            return None;
        }
        let ctx = ctx?;
        if let Some(k) = longest_prefix(parts, ctx) {
            return Some(parts[k..].to_vec());
        }
        if ctx.aliases.contains_key(&parts[0]) {
            return Some(parts[1..].to_vec());
        }
        None
    }
}

/// Longest `k` (1 <= k < segs.len()) with `segs[..k]` an importable module path.
fn longest_prefix(segs: &[String], ctx: &FileImports) -> Option<usize> {
    let mut k = 0;
    for i in 1..segs.len() {
        if ctx.modules.contains_key(&segs[..i]) {
            k = i;
        }
    }
    (k > 0).then_some(k)
}

fn flatten_member_spine(e: &Expr) -> Option<Vec<String>> {
    match &e.kind {
        ExprKind::Ident(n) => Some(vec![n.clone()]),
        ExprKind::Member { object, name } => {
            let mut segs = flatten_member_spine(object)?;
            segs.push(name.clone());
            Some(segs)
        }
        _ => None,
    }
}

fn qualified_expr(span: Span, remaining: &[String]) -> Expr {
    let mut e = Expr {
        span,
        kind: ExprKind::Ident(remaining[0].clone()),
    };
    for seg in &remaining[1..] {
        e = Expr {
            span,
            kind: ExprKind::Member {
                object: Box::new(e),
                name: seg.clone(),
            },
        };
    }
    e
}

#[cfg(test)]
mod workspace_parse_tests {
    use super::parse_workspace_members;

    #[test]
    fn parses_inline_table_members() {
        let toml = "[workspace]
members = [
    { name = \"alpha\", path = \"alpha\" },
    { name = \"beta\", path = \"beta\" },
]
";
        let m = parse_workspace_members(toml);
        assert_eq!(m.get("alpha").map(|s| s.as_str()), Some("alpha"));
        assert_eq!(m.get("beta").map(|s| s.as_str()), Some("beta"));
    }

    #[test]
    fn parses_single_line_members() {
        let toml = "[workspace]
members = [{ name = \"compiler\", path = \"pickle/compiler\" }]
";
        let m = parse_workspace_members(toml);
        assert_eq!(
            m.get("compiler").map(|s| s.as_str()),
            Some("pickle/compiler")
        );
    }

    #[test]
    fn bare_string_entries_map_name_to_path() {
        let toml = "[workspace]
members = [\"alpha\", \"beta\"]
";
        let m = parse_workspace_members(toml);
        assert_eq!(m.get("alpha").map(|s| s.as_str()), Some("alpha"));
        assert_eq!(m.get("beta").map(|s| s.as_str()), Some("beta"));
    }

    #[test]
    fn ignores_other_sections() {
        let toml = "[package]
name = \"picklescript\"
[workspace]
members = [{ name = \"compiler\", path = \"pickle/compiler\" }]
[registry]
default = \"pickle\"
";
        let m = parse_workspace_members(toml);
        assert_eq!(
            m.get("compiler").map(|s| s.as_str()),
            Some("pickle/compiler")
        );
        assert_eq!(m.len(), 1);
    }
}
