//! The module system: `import`/`from` loading, qualified references,
//! namespace/item/wildcard forms, aliases, pub re-exports, and the ambiguity
//! diagnostic for cross-module name collisions.

use pickle_compiler::diag::{DiagnosticSink, SourceMap};
use pickle_compiler::error::ErrorCode;
use pickle_compiler::front::frontend_checked;

fn fixture(name: &str) -> String {
    format!(
        "{}/tests/fixtures/modules/{name}",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn ws_fixture(name: &str) -> String {
    format!("{}/tests/fixtures/ws/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn check_file(path: &str) -> (Option<bool>, DiagnosticSink, SourceMap) {
    let source = std::fs::read_to_string(path).expect("fixture exists");
    let diags = DiagnosticSink::new();
    let mut map = SourceMap::default();
    let ok = frontend_checked(path, &source, &mut map, &diags);
    (ok.map(|_| true), diags, map)
}

fn codes(diags: &DiagnosticSink) -> Vec<ErrorCode> {
    diags
        .diagnostics
        .borrow()
        .iter()
        .filter_map(|d| d.code)
        .collect()
}

fn has_code(diags: &DiagnosticSink, code: ErrorCode) -> bool {
    codes(diags).contains(&code)
}

// ---- the four import forms + `as` aliases ---------------------------------

#[test]
fn imports_and_aliases_compile_clean() {
    let (ok, diags, _map) = check_file(&fixture("main.pkl"));
    assert!(ok.is_some(), "main.pkl should frontend-clean");
    assert_eq!(codes(&diags), Vec::new(), "no diagnostics expected");
}

// ---- qualified names inside `try` and `throw` ------------------------------
//
// `rewrite_stmt` had a catch-all arm that returned a statement unvisited, and
// `Stmt::Try` and `Stmt::Throw` both fell into it. A qualified call inside a
// `try` body, a `catch`, a `finally`, or a thrown expression therefore kept its
// module prefix and the checker reported it as an undeclared name.
//
// A bare imported name does not reach the rewriter, so the other tests in this
// file pass whether or not this is fixed; only an `import mod` reference does.

#[test]
fn qualified_names_inside_try_and_throw_resolve() {
    let (ok, diags, _map) = check_file(&fixture("try_qualified_main.pkl"));
    assert!(
        ok.is_some(),
        "a qualified call inside try/catch/finally or a throw operand must \
         resolve; got {:?}",
        codes(&diags)
    );
    assert_eq!(codes(&diags), Vec::new(), "no diagnostics expected");
}

// ---- module path mismatch -------------------------------------------------

#[test]
fn module_path_mismatch_is_reported() {
    let (ok, diags, _map) = check_file(&fixture("mismatch_main.pkl"));
    assert!(ok.is_some(), "loader continues after module errors");
    assert!(
        has_code(&diags, ErrorCode::ModuleMismatch),
        "expected a ModuleMismatch diagnostic, got {:?}",
        codes(&diags)
    );
}

// ---- a bare collided name must be disambiguated with an alias -------------

#[test]
fn ambiguous_import_names_both_providers() {
    let (ok, diags, map) = check_file(&fixture("ambiguous_main.pkl"));
    assert!(ok.is_some(), "loader continues after collision errors");
    assert!(
        has_code(&diags, ErrorCode::AmbiguousImport),
        "expected an AmbiguousImport diagnostic, got {:?}",
        codes(&diags)
    );
    let rendered = diags.render_all(&map, false);
    assert!(
        rendered.contains("provided by module `math.ops` and module `text.format`"),
        "provider note missing in:\n{rendered}"
    );
    assert!(
        rendered.contains("import helper from `math.ops` as helperA"),
        "alias suggestion missing in:\n{rendered}"
    );
}

// ---- aliasing resolves the same collision --------------------------------

#[test]
fn aliased_collision_compiles_clean() {
    let (ok, diags, _map) = check_file(&fixture("aliased_main.pkl"));
    assert!(ok.is_some(), "aliases should disambiguate providers");
    assert_eq!(codes(&diags), Vec::new(), "no diagnostics expected");
}

// ---- a missing module path is reported under ModuleNotFound --------------

#[test]
fn missing_module_is_reported() {
    let path = fixture("missing_main.pkl");
    std::fs::write(&path, "import does.not.exist\n\nfn main() {}\n").expect("write fixture");
    let (ok, diags, _map) = check_file(&path);
    let _ = std::fs::remove_file(&path);
    assert!(ok.is_some(), "loader continues after module errors");
    assert!(
        has_code(&diags, ErrorCode::ModuleNotFound),
        "expected a ModuleNotFound diagnostic, got {:?}",
        codes(&diags)
    );
}

// ---- `from`-importing a name the module does not export --------------------

#[test]
fn unexported_item_is_reported() {
    let path = fixture("unexported_main.pkl");
    std::fs::write(&path, "import nope from math.ops\n\nfn main() {}\n").expect("write fixture");
    let (ok, diags, _map) = check_file(&path);
    let _ = std::fs::remove_file(&path);
    assert!(ok.is_some(), "loader continues after module errors");
    assert!(
        has_code(&diags, ErrorCode::NotExported),
        "expected a NotExported diagnostic, got {:?}",
        codes(&diags)
    );
}

// ---- brace list with aliases, whole nested module files --------------------

#[test]
fn brace_items_and_nested_module_files_compile_clean() {
    let path = fixture("items_main.pkl");
    std::fs::write(
        &path,
        "import { add as plus } from math.ops\n\
         import Token from nested\n\
         fn main() -> int {\n\
         \x20   return plus(2, 3)\n\
         }\n",
    )
    .expect("write fixture");
    // `nested/token.pkl` is a nested module file (no `module` decl in source).
    let nested_dir = format!(
        "{}/tests/fixtures/modules/nested",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::create_dir_all(&nested_dir).ok();
    std::fs::write(
        format!("{nested_dir}/token.pkl"),
        "class Token { var text: string }\n",
    )
    .expect("write nested fixture");
    let (ok, diags, _map) = check_file(&path);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{nested_dir}/token.pkl"));
    let _ = std::fs::remove_dir(&nested_dir);
    assert!(ok.is_some(), "items form should frontend-clean");
    assert_eq!(
        codes(&diags),
        Vec::new(),
        "no diagnostics expected, got {:?} and:\n{}",
        codes(&diags),
        diags.render_all(&_map, false)
    );
}

// ---- modules declared without a matching file still work as plain sources --

#[test]
fn module_decl_single_file_still_compiles() {
    let source = "module hello.world\n\nfn greet() -> int { return 5 }\n";
    let diags = DiagnosticSink::new();
    let mut map = SourceMap::default();
    let out = frontend_checked("hello_world.pkl", source, &mut map, &diags);
    assert!(out.is_some());
    assert_eq!(codes(&diags), Vec::new());
}

// ---- `pub import` re-exports a namespace and flattened items ----------------

#[test]
fn pub_import_reexports_namespace_and_items() {
    let base = format!("{}/tests/fixtures/modules/std", env!("CARGO_MANIFEST_DIR"));
    std::fs::create_dir_all(&base).ok();
    std::fs::write(
        format!("{base}/json.pkl"),
        "module std.json\n\
         fn encode(v: int) -> int { return v }\n\
         fn decode(v: int) -> int { return v }\n",
    )
    .expect("write json.pkl");
    std::fs::write(
        format!("{base}/http.pkl"),
        "module std.http\n\
         public import std.json\n\
         public import { encode } from std.json\n",
    )
    .expect("write http.pkl");
    let path = fixture("reexport_main.pkl");
    std::fs::write(
        &path,
        "import std.http\n\
         import * from std.http\n\
         fn main() -> int {\n\
         \x20   return encode(7) + std.http.json.decode(1)\n\
         }\n",
    )
    .expect("write reexport_main.pkl");
    let (ok, diags, map) = check_file(&path);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{base}/http.pkl"));
    let _ = std::fs::remove_file(format!("{base}/json.pkl"));
    let _ = std::fs::remove_dir(&base);
    assert!(ok.is_some(), "pub re-export should frontend-clean");
    assert_eq!(
        codes(&diags),
        Vec::new(),
        "no diagnostics expected, got {:?} and:\n{}",
        codes(&diags),
        diags.render_all(&map, false)
    );
}

// ---- package directory imports through the union of its members ------------

#[test]
fn package_dir_imports_union_of_members() {
    let base = format!("{}/tests/fixtures/modules/pkg", env!("CARGO_MANIFEST_DIR"));
    std::fs::create_dir_all(&base).ok();
    std::fs::write(
        format!("{base}/vec.pkl"),
        "module pkg.vec\n\
         class Vec2 {\n\
         \x20   var x: int\n\
         \x20   var y: int\n\
         \x20   constructor(x: int, y: int) {\n\
         \x20       this.x = x\n\
         \x20       this.y = y\n\
         \x20   }\n\
         }\n\
         fn mkVec(x: int, y: int) -> int { return x + y }\n",
    )
    .expect("write vec.pkl");
    let path = fixture("pkg_main.pkl");
    std::fs::write(
        &path,
        "import Vec2 from pkg\n\
         import { mkVec } from pkg\n\
         fn main() -> int {\n\
         \x20   let v = Vec2(1, 2)\n\
         \x20   return mkVec(v.x, v.y) + v.x\n\
         }\n",
    )
    .expect("write pkg_main.pkl");
    let (ok, diags, map) = check_file(&path);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{base}/vec.pkl"));
    let _ = std::fs::remove_dir(&base);
    assert!(ok.is_some(), "package import should frontend-clean");
    assert_eq!(
        codes(&diags),
        Vec::new(),
        "no diagnostics expected, got {:?} and:\n{}",
        codes(&diags),
        diags.render_all(&map, false)
    );
}

// ---- workspace manifest: crate-qualified imports resolve across crates -------

#[test]
fn workspace_crate_imports_resolve() {
    let (ok, diags, map) = check_file(&ws_fixture("beta/main.pkl"));
    assert!(ok.is_some(), "cross-crate import should frontend-clean");
    assert_eq!(
        codes(&diags),
        Vec::new(),
        "no diagnostics expected, got {:?} and:\n{}",
        codes(&diags),
        diags.render_all(&map, false)
    );
}

// ---- a missing module inside a known crate reports ModuleNotFound ------------

#[test]
fn missing_crate_module_is_reported() {
    let path = ws_fixture("beta/missing_main.pkl");
    std::fs::write(
        &path,
        "import ghost from alpha.does.not.exist\n\nfn main() {}\n",
    )
    .expect("write fixture");
    let (ok, diags, map) = check_file(&path);
    let _ = std::fs::remove_file(&path);
    assert!(ok.is_some(), "loader continues after module errors");
    assert!(
        has_code(&diags, ErrorCode::ModuleNotFound),
        "expected a ModuleNotFound diagnostic, got {:?} and:\n{}",
        codes(&diags),
        diags.render_all(&map, false)
    );
}
