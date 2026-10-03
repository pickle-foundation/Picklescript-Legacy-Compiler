//! A build cache for the lowering step, keyed on the sources it consumed.
//!
//! `pickle run <file>` lowers the program with the Rust compiler first, and for
//! a self-hosted tool run that means parsing, resolving, and type-checking every
//! module the entry file imports -- for `tools/diff/port/irxonly.pkl` that is
//! roughly a megabyte of Pickle source (the self-hosted front end, emitter, IR,
//! and standard library), which costs seconds on *every* invocation and changes
//! only when one of those files changes.
//!
//! So: remember which files the last lowering of an entry point read, hash
//! their contents, and keep the lowered module as IRX text next to that hash.
//! A hit skips the frontend and the emitter entirely; the remaining cost is
//! parsing the IRX back and JIT-compiling it.
//!
//! Two rules keep this sound:
//!
//! * The cache *filename* is only a hint. Correctness comes from comparing the
//!   stored fingerprint -- the full list of paths, sizes, and content hashes --
//!   against a freshly computed one, so a hash collision costs a re-compile,
//!   never a wrong program.
//! * The compiler's own bytes are part of the fingerprint. Any change to the
//!   toolchain, including one that does not touch this file, misses the cache.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use pickle_compiler::diag::SourceMap;

/// `PKL_NO_CACHE=1` turns the cache off; `PKL_CACHE_DIR` moves it.
fn enabled() -> bool {
    std::env::var_os("PKL_NO_CACHE").is_none()
}

fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os("PKL_CACHE_DIR") {
        return PathBuf::from(d);
    }
    // Beside the build output, so `cargo clean` drops it with everything else.
    PathBuf::from("target").join("pickle-cache")
}

/// A stable hex digest. Used only to name files and to fingerprint inputs, so
/// the standard library's SipHash is plenty.
fn digest(bytes: &[u8]) -> String {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Identity of the toolchain that produced a cached module. Hashing the
/// executable is blunt but exact: a rebuilt compiler must not serve modules
/// lowered by its predecessor, and the exe's size dwarfs the few milliseconds
/// this costs next to a multi-second lowering.
fn toolchain_stamp() -> String {
    static STAMP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    STAMP
        .get_or_init(|| {
            let exe = std::env::current_exe().ok();
            let (len, digest) = match exe.as_ref().and_then(|p| fs::read(p).ok()) {
                Some(bytes) => (bytes.len(), digest(&bytes)),
                // No readable exe (a stripped or relocated binary): fall back to
                // the version, which still invalidates across releases.
                None => (0, env!("CARGO_PKG_VERSION").to_string()),
            };
            format!("pickle-cli {} {len} {digest}", env!("CARGO_PKG_VERSION"))
        })
        .clone()
}

/// The fingerprint of one source file: path, byte length, and content hash.
fn file_stamp(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    Some(format!(
        "{} {} {}",
        path.display(),
        text.len(),
        digest(text.as_bytes())
    ))
}

/// The fingerprint of a lowering: the toolchain plus every input file.
///
/// The stamps are sorted and deduplicated: the frontend may load imports in a
/// different order on each run, and an order-sensitive key would then miss on
/// every invocation.
fn fingerprint(deps: &[PathBuf]) -> Option<String> {
    let mut stamps = Vec::with_capacity(deps.len());
    for dep in deps {
        stamps.push(file_stamp(dep)?);
    }
    stamps.sort();
    stamps.dedup();
    // Newline-*separated* rather than newline-terminated, so the fingerprint is
    // exactly the header block a cache entry stores before its blank line.
    let mut key = toolchain_stamp();
    for stamp in stamps {
        key.push('\n');
        key.push_str(&stamp);
    }
    Some(key)
}

/// Where the dependency list for `entry` is remembered.
fn dep_list_path(entry: &Path) -> PathBuf {
    dir().join(format!(
        "{}.deps",
        digest(entry.to_string_lossy().as_bytes())
    ))
}

/// Where the IRX for a given fingerprint lives.
fn module_path(fingerprint: &str) -> PathBuf {
    dir().join(format!("{}.irx", digest(fingerprint.as_bytes())))
}

/// The files the last lowering of `entry` read, if it is still recorded.
///
/// This is what makes the cache work without lowering first: the fingerprint
/// needs the dependency list, and the dependency list is a record of the
/// previous run.
pub fn recorded_deps(entry: &Path) -> Option<Vec<PathBuf>> {
    if !enabled() {
        return None;
    }
    let text = fs::read_to_string(dep_list_path(entry)).ok()?;
    let mut deps = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        deps.push(PathBuf::from(line));
    }
    if deps.is_empty() {
        None
    } else {
        Some(deps)
    }
}

/// The lowered module for `entry`, if the recorded inputs are unchanged.
///
/// Every failure here -- no entry, a damaged file, a fingerprint that does not
/// match -- is a miss, never an error: the caller lowers from source instead.
pub fn lookup(deps: &[PathBuf]) -> Option<String> {
    if !enabled() {
        return None;
    }
    let want = fingerprint(deps)?;
    let text = fs::read_to_string(module_path(&want)).ok()?;
    // The file name is a digest of the fingerprint, so it is only a hint; the
    // fingerprint block is what makes a hit trustworthy. A blank line ends it,
    // which no stamp can contain, so the split is unambiguous even when the
    // IRX payload starts with a line of its own.
    let (head, irx) = text.split_once("\n\n")?;
    let (len, stored) = head.split_once('\n')?;
    if stored != want {
        return None;
    }
    // The length is what catches an entry cut short. A truncated payload still
    // carries a matching fingerprint, so without this a half-written file would
    // be served as a hit and fail somewhere further downstream.
    if len.parse::<usize>().ok()? != irx.len() {
        return None;
    }
    Some(irx.to_string())
}

/// Record `entry`'s dependency list and the module lowered from it.
///
/// The dependency list is replaced, not merged: a lowering that reached a
/// different set of files (an import removed) must not leave the old set behind
/// to be consulted next time.
pub fn store(entry: &Path, map: &SourceMap, irx: &str) {
    if !enabled() {
        return;
    }
    let deps: Vec<PathBuf> = map.files().iter().map(|f| PathBuf::from(&f.name)).collect();
    let Some(want) = fingerprint(&deps) else {
        return;
    };
    let dir = dir();
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    write_atomic(
        &module_path(&want),
        format!("{}\n{want}\n\n{irx}", irx.len()).as_bytes(),
    );
    // The dependency list goes last: until it lands, a reader falls back to a
    // full lowering rather than trusting a half-recorded input set.
    let mut list = String::new();
    for dep in &deps {
        list.push_str(&dep.to_string_lossy());
        list.push('\n');
    }
    write_atomic(&dep_list_path(entry), list.as_bytes());
}

/// Replace `path` in one step.
///
/// Windows `rename` fails if the target exists, and a crash between write and
/// rename would otherwise leave a truncated entry behind that later runs would
/// read as a miss forever.
fn write_atomic(path: &Path, bytes: &[u8]) {
    let tmp = path.with_extension("tmp");
    if fs::write(&tmp, bytes).is_err() {
        return;
    }
    if fs::rename(&tmp, path).is_ok() {
        return;
    }
    // Either Windows refused because the target is there, or the rename failed
    // for another reason. Dropping the old target and trying once more covers
    // the first case; the second leaves the previous entry in place, which is
    // still a valid entry for its own fingerprint.
    let _ = fs::remove_file(path);
    if fs::rename(&tmp, path).is_err() {
        let _ = fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These tests drive the cache through its environment variables, and the
    /// test harness runs them in parallel, so they take turns.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Held for the duration of a test that reads or writes `PKL_*`.
    fn env() -> std::sync::MutexGuard<'static, ()> {
        // A poisoned lock just means another test panicked; the environment is
        // still this test's to fix up.
        ENV.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The cache is only sound if these hold, so they are checked rather than
    /// assumed.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pickle-cache-test-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn source(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn fingerprint_ignores_dependency_order() {
        let dir = scratch("order");
        let a = source(&dir, "a.pkl", "a");
        let b = source(&dir, "b.pkl", "b");
        // The frontend may load imports in any order; an order-sensitive key
        // would miss on every run.
        assert_eq!(fingerprint(&[a.clone(), b.clone()]), fingerprint(&[b, a]));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fingerprint_tracks_content_not_metadata() {
        let dir = scratch("content");
        let a = source(&dir, "a.pkl", "one");
        let before = fingerprint(std::slice::from_ref(&a)).unwrap();
        source(&dir, "a.pkl", "two");
        assert_ne!(before, fingerprint(std::slice::from_ref(&a)).unwrap());
        // The same bytes written again (new mtime) must still hit.
        let after = fingerprint(std::slice::from_ref(&a)).unwrap();
        source(&dir, "a.pkl", "two");
        assert_eq!(after, fingerprint(&[a]).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fingerprint_requires_every_file_to_exist() {
        let dir = scratch("missing");
        let a = source(&dir, "a.pkl", "a");
        assert!(fingerprint(std::slice::from_ref(&a)).is_some());
        fs::remove_file(&a).unwrap();
        assert!(fingerprint(&[a]).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn lookup_rejects_a_changed_source() {
        let _env = env();
        let dir = scratch("stale");
        let a = source(&dir, "a.pkl", "one");
        std::env::set_var("PKL_CACHE_DIR", &dir);
        std::env::remove_var("PKL_NO_CACHE");
        let deps = vec![a.clone()];
        let want = fingerprint(&deps).unwrap();
        let payload = "irx payload\nsecond line\n";
        fs::write(
            module_path(&want),
            format!("{}\n{want}\n\n{payload}", payload.len()).as_bytes(),
        )
        .unwrap();
        assert_eq!(lookup(&deps).as_deref(), Some(payload));
        source(&dir, "a.pkl", "two");
        assert!(lookup(&deps).is_none());
        std::env::remove_var("PKL_CACHE_DIR");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn lookup_ignores_an_entry_for_other_inputs() {
        let _env = env();
        let dir = scratch("foreign");
        let a = source(&dir, "a.pkl", "one");
        std::env::set_var("PKL_CACHE_DIR", &dir);
        std::env::remove_var("PKL_NO_CACHE");
        let other = fingerprint(&[source(&dir, "b.pkl", "b")]).unwrap();
        fs::write(
            module_path(&other),
            format!("7\n{other}\n\npayload").as_bytes(),
        )
        .unwrap();
        assert!(lookup(&[a]).is_none());
        std::env::remove_var("PKL_CACHE_DIR");
        let _ = fs::remove_dir_all(&dir);
    }

    /// An entry cut short by a crash still carries a matching fingerprint, so the
    /// length in the header is the only thing that catches it. Serving the short
    /// payload would hand the JIT a truncated module.
    #[test]
    fn lookup_rejects_a_truncated_entry() {
        let _env = env();
        let dir = scratch("torn");
        let a = source(&dir, "a.pkl", "one");
        std::env::set_var("PKL_CACHE_DIR", &dir);
        std::env::remove_var("PKL_NO_CACHE");
        let deps = vec![a.clone()];
        let want = fingerprint(&deps).unwrap();
        // The header claims 40 bytes of payload; only 9 arrived.
        fs::write(
            module_path(&want),
            format!("40\n{want}\n\nirx modul").as_bytes(),
        )
        .unwrap();
        assert!(lookup(&deps).is_none());
        // The full payload is accepted, so the guard is not simply rejecting
        // every entry it sees.
        fs::write(
            module_path(&want),
            format!("9\n{want}\n\nirx modul").as_bytes(),
        )
        .unwrap();
        assert_eq!(lookup(&deps).as_deref(), Some("irx modul"));
        std::env::remove_var("PKL_CACHE_DIR");
        let _ = fs::remove_dir_all(&dir);
    }

    /// A payload with a blank line in it must survive the header split, and the
    /// length must count bytes rather than characters.
    #[test]
    fn lookup_keeps_a_multiline_payload_intact() {
        let _env = env();
        let dir = scratch("multiline");
        let a = source(&dir, "a.pkl", "one");
        std::env::set_var("PKL_CACHE_DIR", &dir);
        std::env::remove_var("PKL_NO_CACHE");
        let deps = vec![a.clone()];
        let want = fingerprint(&deps).unwrap();
        let payload = "irx line one\n\nirx line two\n";
        fs::write(
            module_path(&want),
            format!("{}\n{want}\n\n{payload}", payload.len()).as_bytes(),
        )
        .unwrap();
        assert_eq!(lookup(&deps).as_deref(), Some(payload));
        std::env::remove_var("PKL_CACHE_DIR");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn disabled_cache_never_hits() {
        let _env = env();
        let dir = scratch("disabled");
        let a = source(&dir, "a.pkl", "one");
        std::env::set_var("PKL_CACHE_DIR", &dir);
        std::env::set_var("PKL_NO_CACHE", "1");
        assert!(recorded_deps(&dir.join("entry.pkl")).is_none());
        assert!(lookup(&[a]).is_none());
        std::env::remove_var("PKL_NO_CACHE");
        std::env::remove_var("PKL_CACHE_DIR");
        let _ = fs::remove_dir_all(&dir);
    }
}
