use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;

use cranelift_codegen::ir::{
    types, AbiParam, ExternalName, Function, GlobalValueData, InstBuilder, Signature,
    UserExternalName, UserFuncName,
};
use cranelift_codegen::isa::OwnedTargetIsa;
use cranelift_codegen::settings::{builder as settings_builder, Configurable, Flags};
use cranelift_codegen::Context as ClifContext;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_module::{
    default_libcall_names, DataDescription, DataId as ModDataId, FuncId as ModFuncId, Linkage,
    Module,
};
use cranelift_object::{ObjectBuilder, ObjectModule};
use pickle_compiler::ir::{FuncId as IrFuncId, IrModule};

use crate::jit;

/// Stack reserve, in bytes, for every executable we link.
///
/// The linker's own default is 1 MiB, which is far too small: Pickle's lowering
/// is recursive and its per-function frames are large (a managed value costs a
/// GC root cell, so a big function carries a big frame). Compiling even a
/// three-line program can nest `stmt` -> `expr` -> `call` deep enough to run
/// the 1 MiB stack out, which shows up as an access violation in the *next*
/// function's prologue rather than as a stack-overflow report.
///
/// This is address space only; pages are committed on demand, so a large
/// reserve costs nothing at startup. `pelink.pkl` writes the same value for the
/// self-hosted path so both linkers agree.
pub const STACK_RESERVE: u64 = 256 * 1024 * 1024;
/// Initial stack commit, in bytes. Small on purpose: the loader grows the
/// stack on demand inside `STACK_RESERVE`.
pub const STACK_COMMIT: u64 = 1024 * 1024;

/// The native backend, PIC off so every `symbol_value` becomes a plain
/// absolute relocation (matches the JIT path; the linker fixes these up).
pub fn isa() -> Result<OwnedTargetIsa> {
    let mut flag_builder = settings_builder();
    flag_builder
        .set("is_pic", "false")
        .context("cannot set is_pic=false")?;
    let flags = Flags::new(flag_builder);
    let isa_builder = cranelift_native::builder().map_err(|e| anyhow!("host ISA: {e}"))?;
    isa_builder.finish(flags).map_err(anyhow::Error::msg)
}

/// Emit one relocatable object (COFF on Windows, ELF elsewhere) for the whole
/// module: the compiled `pickle_*` functions, the string data as read-only
/// symbols, and undefined imports for the runtime helpers.
pub fn emit_object(module: &IrModule) -> Result<Vec<u8>> {
    let builder = ObjectBuilder::new(isa()?, "pickle_program", default_libcall_names())?;
    let mut obj = ObjectModule::new(builder);

    // Lower every function; each one also reports the externs it calls with
    // their CLIF signatures.
    let mut lowered: Vec<(String, Function)> = Vec::with_capacity(module.funcs.len());
    let mut externs: HashMap<String, Signature> = HashMap::new();
    for (i, f) in module.funcs.iter().enumerate() {
        let (clif, fx) = jit::lower_func(module, IrFuncId(i))?;
        externs.extend(fx);
        lowered.push((f.symbol.clone(), clif));
    }

    // `pickle_fmod` is supplied by the process for the JIT; for AOT define it
    // here in the object so we do not need a glue library. Register it in the
    // symbol map too, or callers keep a `TestCase` reference to it and the
    // object backend panics on the relocation.
    let mut func_ids: HashMap<String, (ModFuncId, bool)> = HashMap::new();
    if let Some(sig) = externs.remove("pickle_fmod") {
        let id = obj.declare_function("pickle_fmod", Linkage::Export, &sig)?;
        func_ids.insert("pickle_fmod".to_string(), (id, true));
        let mut ctx = ClifContext::new();
        ctx.func = fmod_binding();
        obj.define_function(id, &mut ctx)?;
    }

    // Declare the runtime imports as functions. The `pkl_strdata_*` names in
    // the extern list are actually string data, declared below.
    for (sym, sig) in &externs {
        // `pkl_strdata_*` are string blobs and `pkl_global_*` are top-level
        // `var` slots; both are data, not functions. Declaring them here as
        // imports makes Cranelift reject the module with
        // "Incompatible declaration of identifier" when the data symbols are
        // declared further down.
        if sym.starts_with("pkl_strdata_") || sym.starts_with("pkl_global_") {
            continue;
        }
        let id = obj.declare_function(sym, Linkage::Import, sig)?;
        func_ids.insert(sym.clone(), (id, false));
    }

    // Declare the compiled pickle functions as exports.
    for f in &module.funcs {
        let sig = jit::signature_for(f);
        let id = obj.declare_function(&f.symbol, Linkage::Export, &sig)?;
        func_ids.insert(f.symbol.clone(), (id, true));
    }

    // String constants live in a read-only data section as `pkl_strdata_*`.
    let mut data_ids: HashMap<String, (ModDataId, bool)> = HashMap::new();
    for (i, bytes) in module.strings.iter().enumerate() {
        let name = format!("pkl_strdata_{i}");
        let id = obj.declare_data(&name, Linkage::Export, false, false)?;
        data_ids.insert(name, (id, true));
        let mut desc = DataDescription::new();
        desc.set_align(16);
        desc.define(bytes.clone().into_boxed_slice());
        obj.define_data(id, &desc)?;
    }

    // Top-level `var` slots become read/write data symbols, one per global.
      // `pkl_global_*` is deliberately *not* prefixed as read-only like the
      // string blobs: these are written by `storeglobal`.
      for g in &module.globals {
          let bytes = match &g.init {
              Some(b) => b.clone(),
              // Zero-init: an empty definition still gets a symbol with a
              // real address, which is what `loadglobal`/`gaddr` need. Cranelift
              // routes an empty definition to `.bss`, so these never cost file
              // space.
              None => Vec::new(),
          };
          // `writable = true` is load-bearing, not cosmetic: cranelift-object
          // picks `StandardSection::Data` (a writable `.data`) only for
          // writable data symbols and puts everything else in the read-only
          // `.rdata`. With `writable = false` the symbol still loads correctly
          // but every `storeglobal` faults with an access violation, because
          // the PE page holding it is mapped read-only.
          let id = obj.declare_data(&g.symbol, Linkage::Export, true, false)?;
          data_ids.insert(g.symbol.clone(), (id, true));
          let mut desc = DataDescription::new();
          desc.set_align(8);
          desc.define(bytes.into_boxed_slice());
          obj.define_data(id, &desc)?;
      }

      // Remap the `TestCase` symbol references to module user names (the object
      // backend refuses test-case names), then define each function.
    while let Some((symbol, mut clif)) = lowered.pop() {
        rewrite_symbol_names(&mut clif, &func_ids, &data_ids);
        let id = func_ids[&symbol].0;
        let mut ctx = ClifContext::new();
        ctx.func = clif;
obj.define_function(id, &mut ctx)?;
    }

    Ok(obj.finish().emit()?)
}

/// The object backend emits relocations keyed off `ExternalName::User`
/// references. Rewrite every `Symbol` global with a `TestCase` name into a
/// user name (namespace 0 = function, 1 = data) and register it in the
/// function's user-name table.
fn rewrite_symbol_names(
    f: &mut Function,
    func_ids: &HashMap<String, (ModFuncId, bool)>,
    data_ids: &HashMap<String, (ModDataId, bool)>,
) {
    let gvs: Vec<(cranelift_codegen::ir::GlobalValue, GlobalValueData)> = f
        .global_values
        .iter()
        .map(|(k, v)| (k, v.clone()))
        .collect();
    let mut user_refs: HashMap<String, cranelift_codegen::ir::UserExternalNameRef> = HashMap::new();
    for (key, data) in gvs {
        let GlobalValueData::Symbol {
            name,
            offset,
            colocated: _,
            tls,
        } = data
        else {
            continue;
        };
        let ExternalName::TestCase(tc) = name else {
            continue;
        };
        let sym = String::from_utf8_lossy(tc.raw()).into_owned();
        let (ns, idx, col) = if let Some((id, c)) = func_ids.get(&sym) {
            (0u32, id.as_u32(), *c)
        } else if let Some((id, c)) = data_ids.get(&sym) {
            (1, id.as_u32(), *c)
        } else {
            continue;
        };
        let r = user_refs.entry(sym.clone()).or_insert_with(|| {
            f.declare_imported_user_function(UserExternalName {
                namespace: ns,
                index: idx,
            })
        });
        f.global_values[key] = GlobalValueData::Symbol {
            name: ExternalName::user(*r),
            offset,
            colocated: col,
            tls,
        };
    }
}

/// A little `pickle_fmod(a, b)` binding in CLIF, computed as
/// `a - floor(a / b) * b`.
fn fmod_binding() -> Function {
    let mut sig = Signature::new(jit::host_cc());
    sig.params.push(AbiParam::new(types::F64));
    sig.params.push(AbiParam::new(types::F64));
    sig.returns.push(AbiParam::new(types::F64));
    let mut f = Function::with_name_signature(UserFuncName::testcase(&b"pickle_fmod"[..]), sig);
    let mut fctx = FunctionBuilderContext::new();
    let mut b = FunctionBuilder::new(&mut f, &mut fctx);
    let block = b.create_block();
    b.switch_to_block(block);
    let a = b.append_block_param(block, types::F64);
    let c = b.append_block_param(block, types::F64);
    let q = b.ins().fdiv(a, c);
    let qf = b.ins().floor(q);
    let m = b.ins().fmul(qf, c);
    let r = b.ins().fsub(a, m);
    b.ins().return_(&[r]);
    b.seal_all_blocks();
    drop(fctx);
    f
}

/// Compile the runtime as a Rust rlib, and return the path to that rlib.
pub fn build_runtime_rlib(target_dir: &std::path::Path) -> Result<std::path::PathBuf> {
    let status = std::process::Command::new("cargo")
        .arg("build")
        .arg("-p")
        .arg("pickle-runtime")
        .arg("--target-dir")
        .arg(target_dir)
        .status()
        .context("cannot run cargo for the AOT runtime")?;
    if !status.success() {
        bail!("cargo build of the AOT runtime failed");
    }
    let deps = target_dir.join("debug").join("deps");
    for entry in std::fs::read_dir(&deps).context("runtime deps dir")? {
        let p = entry?.path();
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name.starts_with("libpickle_runtime-") && name.ends_with(".rlib") {
            return Ok(p);
        }
    }
    bail!("runtime rlib not found in the AOT build dir")
}

/// Link the emitted object against the runtime rlib using `rustc` as the
/// linker driver. A tiny shim crate provides the process `main`, which boots
/// the runtime and calls the compiled `pickle_main`.
pub fn link_object(
    object: &[u8],
    object_dir: &std::path::Path,
    rlib: &std::path::Path,
    out: &std::path::Path,
) -> Result<()> {
    let ext = if cfg!(target_os = "windows") {
        "obj"
    } else {
        "o"
    };
    // Both scratch paths have to be unique per invocation. Two `build`s running
    // at once (cargo's test harness runs integration tests in parallel, and a
    // user may script concurrent builds) would otherwise write over each
    // other's object and shim, so each could link the other's program.
    let stamp = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );
    let obj_path = object_dir.join(format!("pickle_program-{stamp}.{ext}"));
    std::fs::write(&obj_path, object).context("cannot write object file")?;

    let shim = object_dir.join(format!("main_shim-{stamp}.rs"));
    std::fs::write(
        &shim,
        "unsafe extern \"C\" {\n".to_string()
            + "    fn pickle_main();\n"
            + "    fn pickle_args_set(argc: usize, argv: *const *const u8);\n"
            + "    fn pickle_runtime_init() -> u32;\n"
            + "    fn pickle_runtime_shutdown();\n"
            + "    fn pickle_flush();\n"
            + "}\n"
            + "\n"
            + "#[allow(unused_extern_crates)]\n"
            + "extern crate pickle_runtime;\n"
            + "\n"
            + "fn main() {\n"
            + "    let args: Vec<std::ffi::CString> = {\n"
            + "        let mut v = vec![std::ffi::CString::new(\"pickle\").unwrap()];\n"
            + "        let mut skip_dashdash = true;\n"
            + "        for a in std::env::args_os().skip(1) {\n"
            + "            if skip_dashdash && a == \"--\" {\n"
            + "                skip_dashdash = false;\n"
            + "                continue;\n"
            + "            }\n"
            + "            skip_dashdash = false;\n"
            + "            match a.into_string() {\n"
            + "                Ok(s) => match std::ffi::CString::new(s) {\n"
            + "                    Ok(c) => v.push(c),\n"
            + "                    Err(_) => v.push(std::ffi::CString::new(\"\").unwrap()),\n"
            + "                },\n"
            + "                Err(_) => v.push(std::ffi::CString::new(\"\").unwrap()),\n"
            + "            }\n"
            + "        }\n"
            + "        v\n"
            + "    };\n"
            + "    let ptrs: Vec<*const u8> =\n"
            + "        args.iter().map(|c| c.as_ptr() as *const u8).collect();\n"
            + "    unsafe {\n"
            + "        pickle_args_set(ptrs.len(), ptrs.as_ptr());\n"
            + "        pickle_runtime_init();\n"
            + "        pickle_main();\n"
            + "        pickle_flush();\n"
            // Propagation is cooperative, so a `throw` with no handler left
            // makes `pickle_main` return early like any other exit. Only the
            // host can tell that apart from a normal return.
            + "        if let Some(payload) = pickle_runtime::take_escaped_exception() {\n"
            + "            eprintln!(\"uncaught exception: {}\", payload);\n"
            + "            pickle_runtime_shutdown();\n"
            + "            std::process::exit(1);\n"
            + "        }\n"
            + "        pickle_runtime_shutdown();\n"
            + "    }\n"
            + "}\n",
    )
    .context("cannot write the AOT main shim")?;

    let mut cmd = std::process::Command::new("rustc");
    cmd.arg(&shim)
        .arg("--edition")
        .arg("2021")
        .arg("-C")
        .arg("opt-level=2")
        .arg("-C")
        .arg("panic=abort")
        .arg(format!("--extern=pickle_runtime={}", rlib.display()))
        .arg("-C")
        .arg(format!("link-arg={}", obj_path.display()));

    // The default 1 MiB stack is not enough for Pickle's recursive lowering;
    // see `STACK_RESERVE`. Only the PE/COFF linker understands /STACK:.
    if cfg!(target_os = "windows") {
        cmd.arg("-C").arg(format!(
            "link-arg=/STACK:{},{}",
            STACK_RESERVE, STACK_COMMIT
        ));
    }

    let status = cmd
        .arg("-o")
        .arg(out)
        .status()
        .context("cannot run rustc to link the program")?;
    // Clean up both scratch files, including on the failure path, so a failed
    // build does not leave the object behind for the next one to pick up.
    let _ = std::fs::remove_file(&obj_path);
    let _ = std::fs::remove_file(&shim);
    if !status.success() {
        bail!("linking {out:?} failed");
    }
    Ok(())
}
