//! Compiler history: the public API surface of a module, as a snapshot the
//! CLI can diff across git commits.
//!
//! The philosophy behind the feature is that the compiler remembers how a
//! project evolved: `pickle history` shows `User.name: string -> string?`
//! with the commit that did it, `pickle builds` shows pass/fail per commit,
//! and `pickle explain E0308 --history` finds the commit where a code first
//! fired. This module only knows how to *read a program* and turn it into a
//! comparable surface. The git walking, caching under `.pickle/history/`,
//! and rendering live in the CLI.
//!
//! The surface is deliberately flat: every field, method, constructor,
//! property, enum variant, top-level function, const, and type becomes its
//! own `PublicItem`, keyed by `(kind, name)` (`User.name`, `Color.Red`).
//! Diffing two surfaces is then a plain set comparison that yields small,
//! honest "Before/After" changes instead of one opaque "class User changed".

use crate::ast::{Item, Program, TypeExpr, TypeExprKind};
use json::Json;

/// One comparable element of a module's surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicItem {
    /// Grouping bucket, e.g. `fn`, `class`, `field`, `method`, `ctor`,
    /// `prop`, `variant`, `imethod`, `iprop`, `const`.
    pub kind: &'static str,
    /// Stable dotted name, e.g. `User.name`, `Color.Red`, `render`.
    pub name: String,
    /// Source visibility: `pub`, `prot`, `priv`, or `def`.
    pub vis: &'static str,
    /// Canonical signature: parametrs, return type, and any flags that are
    /// part of the call contract (`static`, `override`, extends/implements).
    pub sig: String,
}

/// Render a written type expression back to PickleScript text.
pub fn type_expr_to_string(ty: &TypeExpr) -> String {
    match &ty.kind {
        TypeExprKind::Path(parts) => parts.join("."),
        TypeExprKind::Generic(inner, args) => {
            let inner = type_expr_to_string(inner);
            let args = args
                .iter()
                .map(type_expr_to_string)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{inner}<{args}>")
        }
        TypeExprKind::Pointer(inner) => format!("*{}", type_expr_to_string(inner)),
        TypeExprKind::Ref(inner) => format!("&{}", type_expr_to_string(inner)),
        TypeExprKind::Option(inner) => format!("{}?", type_expr_to_string(inner)),
        TypeExprKind::Tuple(items) => {
            let items = items
                .iter()
                .map(type_expr_to_string)
                .collect::<Vec<_>>()
                .join(", ");
            format!("({items})")
        }
        TypeExprKind::Fn {
            params,
            ret,
            is_async,
        } => {
            let params = params
                .iter()
                .map(type_expr_to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let prefix = if *is_async { "async " } else { "" };
            match ret {
                Some(ret) => format!("{prefix}({params}) -> {}", type_expr_to_string(ret)),
                None => format!("{prefix}({params})"),
            }
        }
        TypeExprKind::Infer => "_".to_string(),
    }
}

fn generics_of(generics: &[crate::ast::GenericParam]) -> String {
    if generics.is_empty() {
        String::new()
    } else {
        let names = generics
            .iter()
            .map(|g| g.name.clone())
            .collect::<Vec<_>>()
            .join(", ");
        format!("<{names}>")
    }
}

/// One parameter, in call-contract text: `name: T`, or `T` when nameless.
fn param_to_string(p: &crate::ast::Param) -> String {
    let ty = match &p.ty {
        Some(ty) => type_expr_to_string(ty),
        None => "_".to_string(),
    };
    if p.rest {
        format!("...{ty}")
    } else if p.name.is_empty() {
        ty
    } else {
        format!("{}: {ty}", p.name)
    }
}

fn params_to_string(params: &[crate::ast::Param]) -> String {
    let ps = params
        .iter()
        .map(param_to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!("({ps})")
}

/// Full call-contract for a function-like: `max<T>(a: T, b: T) -> T`.
fn fn_sig(
    name: &str,
    generics: &[crate::ast::GenericParam],
    params: &[crate::ast::Param],
    ret: &Option<TypeExpr>,
) -> String {
    let mut s = name.to_string();
    s.push_str(&generics_of(generics));
    s.push_str(&params_to_string(params));
    if let Some(ret) = ret {
        s.push_str(" -> ");
        s.push_str(&type_expr_to_string(ret));
    }
    s
}

fn vis_of(v: crate::ast::Visibility) -> &'static str {
    use crate::ast::Visibility::*;
    match v {
        Public => "pub",
        Protected => "prot",
        Private => "priv",
        Default => "def",
    }
}

/// Extract the surface of a parsed module. Only the AST is needed; bodies are
/// irrelevant to the public contract. Items are sorted by `(kind, name)` so
/// diffs are stable regardless of source order.
pub fn extract_surface(program: &Program) -> Vec<PublicItem> {
    let mut out = Vec::new();
    for item in &program.items {
        surface_item(&mut out, item);
    }
    out.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
    out
}

fn push(out: &mut Vec<PublicItem>, item: PublicItem) {
    // The flat key must be unique; a duplicate (same kind + dotted name)
    // would corrupt set-diffs. Keep the first and note duplicates would have
    // been rejected by the resolver earlier.
    if !out
        .iter()
        .any(|i| i.kind == item.kind && i.name == item.name)
    {
        out.push(item);
    }
}

fn surface_item(out: &mut Vec<PublicItem>, item: &Item) {
    use crate::ast::ItemKind::*;
    match &item.kind {
        Fn(f) => {
            let mut flags = String::new();
            if f.is_async {
                flags.push_str("async ");
            }
            push(
                out,
                PublicItem {
                    kind: "fn",
                    name: f.name.clone(),
                    vis: vis_of(f.visibility),
                    sig: format!(
                        "{flags}{}",
                        fn_sig(&f.name, &f.generics, &f.params, &f.return_ty)
                    ),
                },
            );
        }
        Test(f) => {
            push(
                out,
                PublicItem {
                    kind: "fn",
                    name: f.name.clone(),
                    vis: vis_of(f.visibility),
                    sig: format!(
                        "test {}",
                        fn_sig(&f.name, &f.generics, &f.params, &f.return_ty)
                    ),
                },
            );
        }
        Class(c) => {
            push(
                out,
                PublicItem {
                    kind: "class",
                    name: c.name.clone(),
                    vis: vis_of(c.visibility),
                    sig: type_head(&c.name, &c.generics, &c.extends, &c.implements),
                },
            );
            for m in &c.members {
                surface_member(out, &c.name, m);
            }
        }
        Struct(s) => {
            push(
                out,
                PublicItem {
                    kind: "struct",
                    name: s.name.clone(),
                    vis: vis_of(s.visibility),
                    sig: type_head(&s.name, &s.generics, &None, &s.implements),
                },
            );
            for m in &s.members {
                surface_member(out, &s.name, m);
            }
        }
        Enum(e) => {
            push(
                out,
                PublicItem {
                    kind: "enum",
                    name: e.name.clone(),
                    vis: vis_of(e.visibility),
                    sig: type_head(&e.name, &e.generics, &None, &e.implements),
                },
            );
            for v in &e.variants {
                let fields = v
                    .fields
                    .iter()
                    .map(|f| format!("{}: {}", f.name, type_expr_to_string(&f.ty)))
                    .collect::<Vec<_>>()
                    .join(", ");
                push(
                    out,
                    PublicItem {
                        kind: "variant",
                        name: format!("{}.{}", e.name, v.name),
                        vis: "def",
                        sig: if fields.is_empty() {
                            String::new()
                        } else {
                            format!("({fields})")
                        },
                    },
                );
            }
        }
        Interface(i) => {
            push(
                out,
                PublicItem {
                    kind: "interface",
                    name: i.name.clone(),
                    vis: vis_of(i.visibility),
                    sig: type_head(&i.name, &i.generics, &None, &i.extends),
                },
            );
            use crate::ast::InterfaceMember::*;
            for m in &i.members {
                match m {
                    Method(md) => push(
                        out,
                        PublicItem {
                            kind: "imethod",
                            name: format!("{}.{}", i.name, md.name),
                            vis: vis_of(md.visibility),
                            sig: fn_sig(&md.name, &md.generics, &md.params, &md.return_ty),
                        },
                    ),
                    Property { name, ty, .. } => push(
                        out,
                        PublicItem {
                            kind: "iprop",
                            name: format!("{}.{name}", i.name),
                            vis: "def",
                            sig: format!("{name}: {}", type_expr_to_string(ty)),
                        },
                    ),
                    Const { name, .. } => push(
                        out,
                        PublicItem {
                            kind: "iconst",
                            name: format!("{}.{name}", i.name),
                            vis: "def",
                            sig: name.clone(),
                        },
                    ),
                }
            }
        }
        Const(c) => push(
            out,
            PublicItem {
                kind: "const",
                name: c.name.clone(),
                vis: vis_of(c.visibility),
                sig: match &c.ty {
                    Some(ty) => format!("{}: {}", c.name, type_expr_to_string(ty)),
                    None => c.name.clone(),
                },
            },
        ),
        Global(g) => {
            push(
                out,
                PublicItem {
                    kind: "var",
                    name: g.name.clone(),
                    vis: vis_of(g.visibility),
                    sig: match &g.ty {
                        Some(ty) => format!("{}: {}", g.name, type_expr_to_string(ty)),
                        None => g.name.clone(),
                    },
                },
            );
        }
    }
}

/// `User<T> extends Person implements Runnable` (or just `User<T>`).
fn type_head(
    name: &str,
    generics: &[crate::ast::GenericParam],
    extends: &Option<TypeExpr>,
    implements: &[TypeExpr],
) -> String {
    let mut s = name.to_string();
    s.push_str(&generics_of(generics));
    if let Some(ex) = extends {
        s.push_str(&format!(" extends {}", type_expr_to_string(ex)));
    }
    if !implements.is_empty() {
        let ifaces = implements
            .iter()
            .map(type_expr_to_string)
            .collect::<Vec<_>>()
            .join(", ");
        s.push_str(&format!(" implements {ifaces}"));
    }
    s
}

fn surface_member(out: &mut Vec<PublicItem>, owner: &str, m: &crate::ast::ClassMember) {
    use crate::ast::ClassMember::*;
    match m {
        Field {
            name,
            ty,
            visibility,
            is_static,
            const_,
            ..
        } => {
            let mut flags = String::new();
            if *is_static {
                flags.push_str("static ");
            }
            if *const_ {
                flags.push_str("const ");
            }
            let tytext = match ty {
                Some(ty) => format!(": {}", type_expr_to_string(ty)),
                None => String::new(),
            };
            push(
                out,
                PublicItem {
                    kind: "field",
                    name: format!("{owner}.{name}"),
                    vis: vis_of(*visibility),
                    sig: format!("{flags}{name}{tytext}"),
                },
            );
        }
        Method(md) => {
            let mut flags = String::new();
            if md.is_static {
                flags.push_str("static ");
            }
            if md.is_override {
                flags.push_str("override ");
            }
            if md.is_async {
                flags.push_str("async ");
            }
            push(
                out,
                PublicItem {
                    kind: "method",
                    name: format!("{owner}.{}", md.name),
                    vis: vis_of(md.visibility),
                    sig: format!(
                        "{flags}{}",
                        fn_sig(&md.name, &md.generics, &md.params, &md.return_ty)
                    ),
                },
            );
        }
        Constructor(c) => push(
            out,
            PublicItem {
                kind: "ctor",
                name: format!("{owner}.<init>"),
                vis: vis_of(c.visibility),
                sig: params_to_string(&c.params),
            },
        ),
        Property(p) => {
            let tytext = match &p.ty {
                Some(ty) => format!(": {}", type_expr_to_string(ty)),
                None => String::new(),
            };
            let mut flags = String::new();
            if p.is_static {
                flags.push_str("static ");
            }
            let mut accessors = String::new();
            if p.get.is_some() {
                accessors.push_str(" get");
            }
            if p.set.is_some() {
                accessors.push_str(" set");
            }
            push(
                out,
                PublicItem {
                    kind: "prop",
                    name: format!("{owner}.{}", p.name),
                    vis: vis_of(p.visibility),
                    sig: format!("{flags}{}{}{accessors}", p.name, tytext),
                },
            );
        }
        Const {
            name,
            visibility,
            ty,
            ..
        } => push(
            out,
            PublicItem {
                kind: "const",
                name: format!("{owner}.{name}"),
                vis: vis_of(*visibility),
                sig: match ty {
                    Some(ty) => format!("{name}: {}", type_expr_to_string(ty)),
                    None => name.clone(),
                },
            },
        ),
        Init(_) | Deinit(_) => {}
    }
}

/// Render every import in a module's dependency list (text form, as written).
pub fn deps_of(program: &Program) -> Vec<String> {
    use crate::ast::ImportKind::*;
    let mut out = Vec::new();
    for imp in &program.imports {
        let prefix = if imp.is_public { "pub " } else { "" };
        let text = match &imp.kind {
            Module { alias } => match alias {
                Some(a) => format!("{prefix}import {} as {a}", imp.source.join(".")),
                None => format!("{prefix}import {}", imp.source.join(".")),
            },
            Items { items } if items.len() == 1 => {
                let it = &items[0];
                match &it.alias {
                    Some(a) => format!(
                        "{prefix}import {} from {} as {a}",
                        it.path.join("."),
                        imp.source.join(".")
                    ),
                    None => format!(
                        "{prefix}import {} from {}",
                        it.path.join("."),
                        imp.source.join(".")
                    ),
                }
            }
            Items { items } => {
                let body = items
                    .iter()
                    .map(|it| match &it.alias {
                        Some(a) => format!("{} as {a}", it.path.join(".")),
                        None => it.path.join("."),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{prefix}import {{ {body} }} from {}", imp.source.join("."))
            }
            Wildcard => format!("{prefix}import * from {}", imp.source.join(".")),
        };
        out.push(text);
    }
    out.sort();
    out
}

/// A change between two surfaces of the same module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceChange {
    pub status: &'static str, // "+", "-", or "~"
    pub kind: &'static str,
    pub name: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// Guarded diff of two surfaces: every item that differs, keyed by
/// `(kind, name)` and sorted for stable rendering.
pub fn diff_surfaces(before: &[PublicItem], after: &[PublicItem]) -> Vec<SurfaceChange> {
    fn find_item<'a>(items: &'a [PublicItem], k: &str, n: &str) -> Option<&'a PublicItem> {
        items.iter().find(|i| i.kind == k && i.name == n)
    }
    let mut changes = Vec::new();
    let mut keys: Vec<(&str, &str)> = Vec::new();
    for i in before.iter().chain(after.iter()) {
        if !keys.iter().any(|(k, n)| *k == i.kind && *n == i.name) {
            keys.push((i.kind, i.name.as_str()));
        }
    }
    keys.sort();
    for (kind, name) in keys {
        let b = find_item(before, kind, name);
        let a = find_item(after, kind, name);
        match (b, a) {
            (None, Some(a)) => changes.push(SurfaceChange {
                status: "+",
                kind,
                name: name.to_string(),
                before: None,
                after: Some(a.sig.clone()),
            }),
            (Some(b), None) => changes.push(SurfaceChange {
                status: "-",
                kind,
                name: name.to_string(),
                before: Some(b.sig.clone()),
                after: None,
            }),
            (Some(b), Some(a)) if b.sig != a.sig => changes.push(SurfaceChange {
                status: "~",
                kind,
                name: name.to_string(),
                before: Some(b.sig.clone()),
                after: Some(a.sig.clone()),
            }),
            _ => {}
        }
    }
    changes
}

/// A plain-text label for a changed signature, when the difference is
/// structural enough to name (`string -> string?` is "made nullable").
pub fn transit_label(before: &str, after: &str) -> Option<&'static str> {
    if !before.contains('?') && after.contains('?') {
        let stripped = after.replace('?', "");
        // The only difference is the option: honest to call it nullable.
        if before == stripped {
            return Some("made nullable");
        }
    }
    if before.contains('?') && !after.contains('?') && before.replace('?', "") == after {
        return Some("no longer optional");
    }
    None
}

/// A stable FNV-1a 64-bit hash of snapshot content (hex). The compiler has no
/// hash crate and std's DefaultHasher is not guaranteed stable across
/// releases, so for the history cache — where the hash keys cache entries —
/// this deterministic hash is preferred.
pub fn fnv1a64(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// A full per-file snapshot of one commit: the surface, its deps, and the
/// build outcome. Rendered by the CLI from `.pickle/history/symbols.db`,
/// `dependencies.db`, and `builds.db`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub commit: String,
    pub short: String,
    pub date: String,
    pub message: String,
    pub file: String,
    pub hash: String,
    pub ok: bool,
    pub codes: Vec<String>,
    pub deps: Vec<String>,
    pub items: Vec<PublicItem>,
}

impl Snapshot {
    /// Full-snapshot JSON (the `symbols.db` line). Numbers are only inside
    /// `codes` item rendering, which is string-only today, so the writer only
    /// needs strings, booleans, and arrays.
    pub fn to_json(&self) -> String {
        let mut o = String::from("{");
        let mut field = |name: &str, val: &str| {
            o.push('"');
            o.push_str(name);
            o.push_str("\":");
            o.push_str(&crate::diag::json_string(val));
            o.push(',');
        };
        field("commit", &self.commit);
        field("short", &self.short);
        field("date", &self.date);
        field("message", &self.message);
        field("file", &self.file);
        field("hash", &self.hash);
        o.push_str("\"ok\":");
        o.push_str(if self.ok { "true" } else { "false" });
        o.push(',');
        o.push_str("\"codes\":");
        push_string_array(&mut o, &self.codes);
        o.push(',');
        o.push_str("\"deps\":");
        push_string_array(&mut o, &self.deps);
        o.push(',');
        o.push_str("\"items\":");
        o.push('[');
        for (i, item) in self.items.iter().enumerate() {
            if i > 0 {
                o.push(',');
            }
            o.push_str("{\"kind\":");
            o.push_str(&crate::diag::json_string(item.kind));
            o.push_str(",\"name\":");
            o.push_str(&crate::diag::json_string(&item.name));
            o.push_str(",\"vis\":");
            o.push_str(&crate::diag::json_string(item.vis));
            o.push_str(",\"sig\":");
            o.push_str(&crate::diag::json_string(&item.sig));
            o.push('}');
        }
        o.push(']');
        o.push('}');
        o
    }

    /// Parse a `symbols.db` line back into a snapshot.
    pub fn from_json(line: &str) -> Option<Snapshot> {
        fn static_kind(s: &str) -> Option<&'static str> {
            Some(match s {
                "fn" => "fn",
                "class" => "class",
                "struct" => "struct",
                "enum" => "enum",
                "interface" => "interface",
                "field" => "field",
                "method" => "method",
                "ctor" => "ctor",
                "prop" => "prop",
                "const" => "const",
                "variant" => "variant",
                "imethod" => "imethod",
                "iprop" => "iprop",
                "iconst" => "iconst",
                _ => return None,
            })
        }
        fn static_vis(s: &str) -> Option<&'static str> {
            Some(match s {
                "pub" => "pub",
                "prot" => "prot",
                "priv" => "priv",
                "def" => "def",
                _ => return None,
            })
        }
        let obj = json::parse(line).ok()?;
        let obj = obj.as_obj()?;
        let mut items = Vec::new();
        if let Some(arr) = obj.get("items") {
            for v in arr.as_arr() {
                if let Some(o) = v.as_obj() {
                    if let (Some(kind), Some(name), Some(vis), Some(sig)) = (
                        o.get("kind")
                            .and_then(Json::as_str_opt)
                            .and_then(static_kind),
                        o.get("name").and_then(Json::as_str_owned),
                        o.get("vis").and_then(Json::as_str_opt).and_then(static_vis),
                        o.get("sig").and_then(Json::as_str_owned),
                    ) {
                        items.push(PublicItem {
                            kind,
                            name,
                            vis,
                            sig,
                        });
                    }
                }
            }
        }
        Some(Snapshot {
            commit: obj.get("commit")?.as_str_owned()?,
            short: obj.get("short")?.as_str_owned()?,
            date: obj.get("date")?.as_str_owned()?,
            message: obj.get("message")?.as_str_owned()?,
            file: obj.get("file")?.as_str_owned()?,
            hash: obj.get("hash")?.as_str_owned()?,
            ok: obj.get("ok")?.as_bool()?,
            codes: obj.get("codes")?.as_strings(),
            deps: obj.get("deps")?.as_strings(),
            items,
        })
    }
}

fn push_string_array(out: &mut String, vals: &[String]) {
    out.push('[');
    for (i, v) in vals.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&crate::diag::json_string(v));
    }
    out.push(']');
}

/// Minimal JSON reader for the history cache. The compiler has no serde; the
/// writer (`json_string`) only ever emits the escapes handled here.
mod json {
    use std::collections::HashMap;

    #[derive(Debug, Clone)]
    pub enum Json {
        Null,
        Bool(bool),
        #[allow(dead_code)]
        Num(i64),
        Str(String),
        Arr(Vec<Json>),
        Obj(HashMap<String, Json>),
    }

    impl Json {
        pub fn as_obj(&self) -> Option<&HashMap<String, Json>> {
            match self {
                Json::Obj(m) => Some(m),
                _ => None,
            }
        }
        pub fn as_str_opt(&self) -> Option<&str> {
            match self {
                Json::Str(s) => Some(s),
                _ => None,
            }
        }
        pub fn as_str_owned(&self) -> Option<String> {
            self.as_str_opt().map(|s| s.to_string())
        }
        pub fn as_bool(&self) -> Option<bool> {
            match self {
                Json::Bool(b) => Some(*b),
                _ => None,
            }
        }
        pub fn as_arr(&self) -> &[Json] {
            match self {
                Json::Arr(a) => a,
                _ => &[],
            }
        }
        pub fn as_strings(&self) -> Vec<String> {
            self.as_arr()
                .iter()
                .filter_map(|v| match v {
                    Json::Str(s) => Some(s.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    pub fn parse(input: &str) -> Result<Json, String> {
        let mut p = Parser {
            bytes: input.as_bytes(),
            i: 0,
        };
        p.ws();
        let v = p.value()?;
        p.ws();
        if p.i != p.bytes.len() {
            return Err("trailing characters".into());
        }
        Ok(v)
    }

    struct Parser<'a> {
        bytes: &'a [u8],
        i: usize,
    }

    impl<'a> Parser<'a> {
        fn ws(&mut self) {
            while self.i < self.bytes.len()
                && matches!(self.bytes[self.i], b' ' | b'\t' | b'\r' | b'\n')
            {
                self.i += 1;
            }
        }
        fn peek(&self) -> Option<u8> {
            self.bytes.get(self.i).copied()
        }
        fn value(&mut self) -> Result<Json, String> {
            match self.peek() {
                Some(b'{') => self.object(),
                Some(b'[') => self.array(),
                Some(b'"') => Ok(Json::Str(self.string()?)),
                Some(b't') => self.keyword("true", Json::Bool(true)),
                Some(b'f') => self.keyword("false", Json::Bool(false)),
                Some(b'n') => self.keyword("null", Json::Null),
                Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
                other => Err(format!("unexpected token `{other:?}` at byte {}", self.i)),
            }
        }
        fn keyword(&mut self, kw: &str, v: Json) -> Result<Json, String> {
            if self.bytes[self.i..].starts_with(kw.as_bytes()) {
                self.i += kw.len();
                Ok(v)
            } else {
                Err("bad literal".into())
            }
        }
        fn number(&mut self) -> Result<Json, String> {
            let start = self.i;
            if self.peek() == Some(b'-') {
                self.i += 1;
            }
            while self.i < self.bytes.len() && self.bytes[self.i].is_ascii_digit() {
                self.i += 1;
            }
            let text = std::str::from_utf8(&self.bytes[start..self.i]).map_err(|_| "bad number")?;
            let n: i64 = text
                .parse()
                .map_err(|_| format!("number too large: `{text}`"))?;
            Ok(Json::Num(n))
        }
        fn object(&mut self) -> Result<Json, String> {
            self.i += 1; // '{'
            self.ws();
            let mut map = HashMap::new();
            if self.peek() == Some(b'}') {
                self.i += 1;
                return Ok(Json::Obj(map));
            }
            loop {
                self.ws();
                let key = self.string()?;
                self.ws();
                if self.peek() != Some(b':') {
                    return Err("expected `:`".into());
                }
                self.i += 1;
                self.ws();
                let val = self.value()?;
                map.insert(key, val);
                self.ws();
                match self.peek() {
                    Some(b',') => {
                        self.i += 1;
                    }
                    Some(b'}') => {
                        self.i += 1;
                        return Ok(Json::Obj(map));
                    }
                    _ => return Err("expected `,` or `}`".into()),
                }
            }
        }
        fn array(&mut self) -> Result<Json, String> {
            self.i += 1; // '['
            self.ws();
            let mut arr = Vec::new();
            if self.peek() == Some(b']') {
                self.i += 1;
                return Ok(Json::Arr(arr));
            }
            loop {
                self.ws();
                arr.push(self.value()?);
                self.ws();
                match self.peek() {
                    Some(b',') => {
                        self.i += 1;
                    }
                    Some(b']') => {
                        self.i += 1;
                        return Ok(Json::Arr(arr));
                    }
                    _ => return Err("expected `,` or `]`".into()),
                }
            }
        }
        fn string(&mut self) -> Result<String, String> {
            if self.peek() != Some(b'"') {
                return Err("expected string".into());
            }
            self.i += 1;
            let mut out = String::new();
            while let Some(c) = self.peek() {
                self.i += 1;
                match c {
                    b'"' => return Ok(out),
                    b'\\' => {
                        let esc = self.peek().ok_or("unterminated escape")?;
                        self.i += 1;
                        match esc {
                            b'"' => out.push('"'),
                            b'\\' => out.push('\\'),
                            b'/' => out.push('/'),
                            b'b' => out.push('\u{08}'),
                            b'f' => out.push('\u{0c}'),
                            b'n' => out.push('\n'),
                            b'r' => out.push('\r'),
                            b't' => out.push('\t'),
                            b'u' => {
                                let hex = self
                                    .bytes
                                    .get(self.i..self.i + 4)
                                    .ok_or("short \\u escape")?;
                                let s = std::str::from_utf8(hex).map_err(|_| "bad \\u")?;
                                let cp = u32::from_str_radix(s, 16).map_err(|_| "bad \\u hex")?;
                                self.i += 4;
                                out.push(char::from_u32(cp).ok_or("invalid \\u code point")?);
                            }
                            other => return Err(format!("unknown escape `\\{}`", other as char)),
                        }
                    }
                    // Multi-byte UTF-8: json_string passes non-ASCII through
                    // raw, so decode the whole sequence, not one byte.
                    other if other >= 0x80 => {
                        let len = if other >= 0xF0 {
                            4
                        } else if other >= 0xE0 {
                            3
                        } else {
                            2
                        };
                        let end = self.i + len - 1;
                        let raw = self.bytes.get(self.i - 1..end).ok_or("truncated UTF-8")?;
                        let s = std::str::from_utf8(raw).map_err(|_| "bad UTF-8")?;
                        out.push_str(s);
                        self.i = end;
                    }
                    other => out.push(other as char),
                }
            }
            Err("unterminated string".into())
        }
    }
}
