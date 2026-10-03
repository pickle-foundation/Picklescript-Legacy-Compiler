use cranelift_codegen::binemit::Reloc;
use cranelift_codegen::control::ControlPlane;
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::immediates::{Imm64, Offset32};
use cranelift_codegen::ir::{
    types, AbiParam, Block, BlockArg, ExternalName, Function, GlobalValue, GlobalValueData,
    InstBuilder, MemFlags, SigRef, Signature, StackSlot, StackSlotData, StackSlotKind, TrapCode,
    UserFuncName, Value,
};
use cranelift_codegen::isa::{CallConv, OwnedTargetIsa};
use cranelift_codegen::settings::{builder as settings_builder, Configurable, Flags};
use cranelift_codegen::{FinalizedMachReloc, FinalizedRelocTarget};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

use std::collections::HashMap;

use anyhow::{anyhow, bail, Context, Result};
use pickle_compiler::ir::{
    BinOp, Callee, FuncId, IrConst, IrFunc, IrInstr, IrModule, IrTerm, IrTy, Slot, Temp, UnOp,
};

use pickle_runtime::runtime_addr as abi_runtime_addr;

/// Bytes of the fixed `ShadowFrame` header: `prev`(8) + `slot_count: u32` +
/// padding `u32`.
const SHADOW_HEADER_BYTES: u32 = 16;
const CELL_BYTES: u32 = 8;

/// Alignment of compiled code chunks inside the executable mapping.
const CODE_ALIGN: usize = 16;

/// Slot width for a top-level `var` cell in the JIT mapping. Every global is
/// padded to this so a load/store of any supported scalar type stays in bounds
/// even when its initializer is shorter.
const DATA_ALIGN: usize = 8;

fn align_up(n: usize, a: usize) -> usize {
    (n + a - 1) & !(a - 1)
}

/// Native calling convention of PickleScript values that map onto machine
/// registers. On x64 Windows this matches the runtime's `extern "C"`.
#[cfg(windows)]
pub(crate) fn host_cc() -> CallConv {
    CallConv::WindowsFastcall
}

#[cfg(not(windows))]
pub(crate) fn host_cc() -> CallConv {
    CallConv::SystemV
}

fn is_managed(ty: IrTy) -> bool {
    matches!(ty, IrTy::Str | IrTy::Ptr)
}

/// Machine type a `IrTy` is carried in. `Bool` is a byte, `Char` an `i32`,
/// `Int`/`Str`/`Ptr` a word, `Float` an `f64`, `Unit` a zero word.
fn clif_ty(ty: IrTy) -> cranelift_codegen::ir::Type {
    match ty {
        IrTy::Bool => types::I8,
        IrTy::Char => types::I32,
        IrTy::Int => types::I64,
        IrTy::Float => types::F64,
        IrTy::Str | IrTy::Ptr | IrTy::Unit => types::I64,
    }
}

fn slot_bytes(ty: IrTy) -> u8 {
    match ty {
        IrTy::Bool => 1,
        IrTy::Char => 4,
        _ => 8,
    }
}

fn align_shift(ty: IrTy) -> u8 {
    match ty {
        IrTy::Bool => 0,
        IrTy::Char => 2,
        _ => 3,
    }
}

/// Cell byte offset inside a shadow frame.
fn cell_offset(cell: u32) -> Offset32 {
    Offset32::new((SHADOW_HEADER_BYTES + cell * CELL_BYTES) as i32)
}

pub(crate) fn signature_for(func: &IrFunc) -> Signature {
    let mut sig = Signature::new(host_cc());
    for p in &func.params {
        sig.params.push(AbiParam::new(clif_ty(p.ty)));
    }
    if !func.ret.is_unit() {
        sig.returns.push(AbiParam::new(clif_ty(func.ret)));
    }
    sig
}

fn extern_signature(params: &[IrTy], ret: IrTy) -> Signature {
    let mut sig = Signature::new(host_cc());
    for p in params {
        sig.params.push(AbiParam::new(clif_ty(*p)));
    }
    if !ret.is_unit() {
        sig.returns.push(AbiParam::new(clif_ty(ret)));
    }
    sig
}

/// `pickle_fmod`: the runtime does not link the C `fmod` symbol, so the JIT
/// supplies a tiny binding built from the `%` operator.
#[no_mangle]
pub extern "C" fn pickle_fmod(a: f64, b: f64) -> f64 {
    a % b
}

/// Executable page allocations for machine code. Uses `VirtualAlloc` on
/// Windows and `mmap` on Unix; both return write+execute pages.
#[cfg(windows)]
mod mem {
    use super::align_up;
    use std::io;

    const MEM_COMMIT: u32 = 0x1000;
    const MEM_RESERVE: u32 = 0x2000;
    const PAGE_EXECUTE_READWRITE: u32 = 0x40;
    const MEM_RELEASE: u32 = 0x8000;

    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualAlloc(
            lp_address: *mut u8,
            dw_size: usize,
            fl_allocation_type: u32,
            fl_protect: u32,
        ) -> *mut u8;
        fn VirtualFree(lp_address: *mut u8, dw_size: usize, dw_free_type: u32) -> i32;
    }

    pub fn alloc(len: usize) -> io::Result<*mut u8> {
        if len == 0 {
            return Ok(std::ptr::null_mut());
        }
        let round = align_up(len, 0x1000);
        // SAFETY: kernel32 always available; size is page-multiple.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null_mut(),
                round,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_EXECUTE_READWRITE,
            )
        };
        if p.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(p)
        }
    }

    /// # Safety
    ///
    /// `ptr` must come from [`alloc`]; `len` must match the original request.
    pub unsafe fn free(ptr: *mut u8, _len: usize) {
        if !ptr.is_null() {
            unsafe {
                VirtualFree(ptr, 0, MEM_RELEASE);
            }
        }
    }
}

/// Executable page allocations for machine code on Unix.
#[cfg(unix)]
mod mem {
    use super::align_up;
    use std::io;

    fn page_size() -> usize {
        unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
    }

    pub fn alloc(len: usize) -> io::Result<*mut u8> {
        if len == 0 {
            return Ok(std::ptr::null_mut());
        }
        let page = page_size();
        let round = align_up(len, page);
        // SAFETY: standard mmap with anonymous private read-write-exec pages.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                round,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            Err(io::Error::last_os_error())
        } else {
            Ok(p as *mut u8)
        }
    }

    /// # Safety
    ///
    /// `ptr` must come from [`alloc`]; `len` must match the original request.
    pub unsafe fn free(ptr: *mut u8, len: usize) {
        if !ptr.is_null() {
            let page = page_size();
            let round = align_up(len, page);
            unsafe {
                libc::munmap(ptr as *mut libc::c_void, round);
            }
        }
    }
}

/// Per-function lowering plan: which slots and temps are managed (live heap
/// references) and which frame cells they own.
struct Plan {
    /// ir slot index -> frame cell index.
    cell: HashMap<u32, u32>,
    /// temp id -> frame cell index (managed temps only).
    temp_cell: HashMap<u32, u32>,
    /// Total number of cells; zero means no shadow frame is needed.
    ncells: u32,
    /// Type of every temp (used to pick signed/float instruction variants).
    types: HashMap<u32, IrTy>,
}

/// Recover temp types and classify managed slots/temps in a pre-pass.
fn analyze(func: &IrFunc, module: &IrModule) -> Plan {
    let mut tt: HashMap<u32, IrTy> = HashMap::new();
    for b in &func.blocks {
        for ins in &b.instrs {
            match ins {
                IrInstr::Const { dst, c } => {
                    let ty = match c {
                        IrConst::Int(_) => IrTy::Int,
                        IrConst::Float(_) => IrTy::Float,
                        IrConst::Bool(_) => IrTy::Bool,
                        IrConst::Char(_) => IrTy::Char,
                        IrConst::Null => IrTy::Ptr,
                        IrConst::Str(_) => IrTy::Str,
                        // Tag `Int` on purpose: raw data addresses must never
                        // be put into the shadow frame as managed pointers.
                        IrConst::StrAddr(_) => IrTy::Int,
                        // Likewise for raw function addresses (finalizers).
                        IrConst::FuncAddr(_) => IrTy::Int,
                    };
                    tt.insert(dst.0, ty);
                }
                IrInstr::UnOp { dst, op, v } => {
                    let t = tt.get(&v.0).copied().unwrap_or(IrTy::Ptr);
                    let ty = match (op, t) {
                        // Logical `not` is Bool-typed; arithmetic/bitwise keep
                        // the operand type.
                        (UnOp::Not, _) => IrTy::Bool,
                        (_, t) => t,
                    };
                    tt.insert(dst.0, ty);
                }
                IrInstr::BinOp { dst, op, a, .. } => {
                    let ty = match op {
                        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                            IrTy::Bool
                        }
                        _ => tt.get(&a.0).copied().unwrap_or(IrTy::Ptr),
                    };
                    tt.insert(dst.0, ty);
                }
                IrInstr::Copy { dst, v } => {
                    let t = tt.get(&v.0).copied().unwrap_or(IrTy::Ptr);
                    tt.insert(dst.0, t);
                }
                IrInstr::Itof { dst, .. } => {
                    tt.insert(dst.0, IrTy::Float);
                }
                IrInstr::Ftoi { dst, .. } => {
                    tt.insert(dst.0, IrTy::Int);
                }
                IrInstr::Chartoi { dst, .. } => {
                    tt.insert(dst.0, IrTy::Int);
                }
                IrInstr::Itochar { dst, .. } => {
                    tt.insert(dst.0, IrTy::Char);
                }
                IrInstr::LoadSlot { dst, slot } => {
                    let t = func
                        .slots
                        .get(slot.0 as usize)
                        .copied()
                        .unwrap_or(IrTy::Ptr);
                    tt.insert(dst.0, t);
                }
                IrInstr::StoreSlot { .. } => {}
                IrInstr::LocalAddr { dst, .. } => {
                    // Raw address: tagged `Int` so it never enters the shadow
                    // frame (it is not a managed pointer).
                    tt.insert(dst.0, IrTy::Int);
                }
                IrInstr::LoadRaw { dst, ty, .. } => {
                    tt.insert(dst.0, *ty);
                }
                // The address of a global is a raw code/data pointer, so it is
                // tagged `Int` deliberately: a data symbol must never land in
                // the shadow frame as a managed pointer.
                IrInstr::GlobalAddr { dst, .. } => {
                    tt.insert(dst.0, IrTy::Int);
                }
                IrInstr::LoadGlobal { dst, ty, .. } => {
                    tt.insert(dst.0, *ty);
                }
                IrInstr::StoreGlobal { .. } => {}
                IrInstr::StoreRaw { .. } => {}
                IrInstr::Call { dst, callee, .. } => {
                    let ret = match callee {
                        Callee::Func(fid) => module.funcs[fid.0].ret,
                        Callee::Extern(eid) => module.externs[eid.0].ret,
                    };
                    if let Some(d) = dst {
                        tt.insert(d.0, ret);
                    }
                }
                IrInstr::CallInd { dst, ret, .. } => {
                    if let Some(d) = dst {
                        tt.insert(d.0, *ret);
                    }
                }
            }
        }
    }

    let mut cell = HashMap::new();
    let mut next = 0u32;
    for (i, ty) in func.slots.iter().enumerate() {
        if is_managed(*ty) {
            cell.insert(i as u32, next);
            next += 1;
        }
    }
    let mut temp_cell = HashMap::new();
    for (t, ty) in &tt {
        if is_managed(*ty) {
            temp_cell.insert(*t, next);
            next += 1;
        }
    }
    Plan {
        cell,
        temp_cell,
        ncells: next,
        types: tt,
    }
}

/// Get (or create) the `SigRef` + symbol `GlobalValue` pair for an extern or
/// user function, keyed by its linkable symbol name. The cached CLIF
/// signature lets AOT object emission declare the import without re-deriving
/// the parameter types.
fn extern_pair(
    builder: &mut FunctionBuilder,
    cache: &mut HashMap<String, (SigRef, GlobalValue, Signature)>,
    symbol: &str,
    params: &[IrTy],
    ret: IrTy,
) -> (SigRef, GlobalValue) {
    // Key on the full signature: the same runtime symbol (e.g. `pickle_list_new`)
    // may be referenced with different arities from different IR externs.
    let mut key = String::with_capacity(symbol.len() + params.len() * 4 + 8);
    key.push_str(symbol);
    for p in params {
        key.push_str(&format!(",{p:?}"));
    }
    key.push_str(&format!("->{ret:?}"));
    if let Some(p) = cache.get(&key) {
        return (p.0, p.1);
    }
    let sig = extern_signature(params, ret);
    let sr = builder.import_signature(sig.clone());
    let gv = builder.create_global_value(GlobalValueData::Symbol {
        name: ExternalName::testcase(symbol),
        offset: Imm64::new(0),
        colocated: false,
        tls: false,
    });
    let p = (sr, gv, sig);
    cache.insert(key, p);
    (sr, gv)
}

/// A `GlobalValue` for a module data symbol (a `pkl_strdata_*` or
/// `pkl_global_*` block). Separate from [`extern_pair`] because the cache key
/// is the symbol alone: data has no signature, and reusing a function
/// signature here would be meaningless.
fn data_pair(
    builder: &mut FunctionBuilder,
    cache: &mut HashMap<String, (SigRef, GlobalValue, Signature)>,
    symbol: &str,
) -> GlobalValue {
    if let Some(p) = cache.get(symbol) {
        return p.1;
    }
    let sig = extern_signature(&[], IrTy::Unit);
    let sr = builder.import_signature(sig.clone());
    let gv = builder.create_global_value(GlobalValueData::Symbol {
        name: ExternalName::testcase(symbol),
        offset: Imm64::new(0),
        colocated: false,
        tls: false,
    });
    cache.insert(symbol.to_string(), (sr, gv, sig));
    gv
}

/// Store `v` into the shadow frame cell of managed temp `dst`.
fn write_through(
    builder: &mut FunctionBuilder,
    plan: &Plan,
    frame: Option<StackSlot>,
    dst: u32,
    v: Value,
) {
    if let Some(&cell) = plan.temp_cell.get(&dst) {
        let fr = frame.expect("managed temp implies shadow frame");
        builder.ins().stack_store(v, fr, cell_offset(cell));
    }
}

/// `PKL_GC_CHECK` call-chain hook installed by [`Jit::link`].
///
/// # Safety
/// Called by the runtime from inside a JIT'd frame with a live stack pointer.
unsafe extern "C" fn gc_check_caller_hook(sp: usize, tag_ptr: usize, tag_len: usize) {
    crate::iced::print_call_chain(sp, tag_ptr, tag_len);
}

/// A single compiled function: its bytes and the absolute relocations against
/// the module's linkable symbols.
struct CompiledFunc {
    symbol: String,
    bytes: Vec<u8>,
    relocs: Vec<FinalizedMachReloc>,
}

/// The host JIT: a `TargetIsa`, created from the native backend.
pub struct Jit {
    isa: OwnedTargetIsa,
}

impl Default for Jit {
    fn default() -> Self {
        Self::new().expect("host JIT target")
    }
}

impl Jit {
    /// Build the host ISA. PIC is disabled so that `symbol_value` loads use a
    /// plain absolute `movabs` + `Reloc::Abs8` rather than GOT-relative
    /// addressing (there is no PIC runtime here to fill a GOT).
    pub fn new() -> Result<Self> {
        let mut flag_builder = settings_builder();
        flag_builder
            .set("is_pic", "false")
            .with_context(|| "cannot set is_pic=false")?;
        // `opt_level=speed` was tried and measured against `irxonly.pkl` on
        // `pickle/compiler/check.pkl` with a warm lowering cache, three runs
        // each: JIT 0.92-0.96s at the default `none` versus 1.68s at `speed`,
        // and the guest phase was no better either (5.75-5.89s versus
        // 6.56-6.91s). The extra passes cost more to run than they save, so the
        // default stays `none`. `PKL_JIT_OPT=speed` re-runs the experiment, and
        // is also the knob to bisect a miscompile against optimized code.
        let opt_level = std::env::var("PKL_JIT_OPT").unwrap_or_else(|_| "none".to_string());
        flag_builder
            .set("opt_level", &opt_level)
            .with_context(|| "cannot set opt_level")?;
        // Compile speed matters more than catching backend bugs we have
        // already exercised: the verifier re-checks every CLIF function and
        // adds noticeable fixed cost per function for large modules (e.g.
        // JIT-compiling the self-host compiler's ~3000 functions).
        flag_builder
            .set("enable_verifier", "false")
            .with_context(|| "cannot set enable_verifier=false")?;
        let flags = Flags::new(flag_builder);
        let isa_builder = cranelift_native::builder().map_err(|e| anyhow!("host ISA: {e}"))?;
        let isa = isa_builder.finish(flags).map_err(anyhow::Error::msg)?;
        Ok(Jit { isa })
    }

    /// Lower and compile one PickleIR function to machine code.
    fn compile_function(&self, module: &IrModule, fid: FuncId) -> Result<CompiledFunc> {
        let func = &module.funcs[fid.0];
        let (clif, _externs) = lower_func(module, fid)?;
        let mut ctx = cranelift_codegen::Context::new();
        ctx.func = clif;

        let compiled = ctx
            .compile(&*self.isa, &mut ControlPlane::default())
            .map_err(|e| anyhow!("compile of `{}`: {:?}", func.name, e))?;
        let relocs = compiled.buffer.relocs().to_vec();
        let bytes = compiled.buffer.data().to_vec();
        Ok(CompiledFunc {
            symbol: func.symbol.clone(),
            bytes,
            relocs,
        })
    }
}

/// Build the CLIF for one PickleIR function, following the runtime's shadow
/// frame convention. Returns the function plus the sorted set of extern
/// symbols it references, so AOT object emission can declare them.
pub(crate) fn lower_func(
    module: &IrModule,
    fid: FuncId,
) -> Result<(Function, Vec<(String, Signature)>)> {
    let func = &module.funcs[fid.0];
    let plan = analyze(func, module);
    let needs_frame = plan.ncells > 0;

    let mut ctx = cranelift_codegen::Context::new();
    ctx.func = Function::with_name_signature(
        UserFuncName::testcase(func.symbol.as_bytes()),
        signature_for(func),
    );

    let mut fctx = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fctx);
    let mut call_cache: HashMap<String, (SigRef, GlobalValue, Signature)> = HashMap::new();

    // Shadow frame for all managed values of this function.
    let frame = if needs_frame {
        Some(builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            SHADOW_HEADER_BYTES + CELL_BYTES * plan.ncells,
            3,
        )))
    } else {
        None
    };

    // Non-managed slots become ordinary stack slots.
    let mut slot_ss: HashMap<u32, StackSlot> = HashMap::new();
    for (i, ty) in func.slots.iter().enumerate() {
        if is_managed(*ty) {
            continue;
        }
        let ss = builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            slot_bytes(*ty) as u32,
            align_shift(*ty),
        ));
        slot_ss.insert(i as u32, ss);
    }

    // Create every IR block up front; branches reference them by id.
    let mut blocks: HashMap<u32, Block> = HashMap::new();
    for b in &func.blocks {
        blocks.insert(b.id.0, builder.create_block());
    }

    let entry = blocks[&func.entry.0];
    builder.switch_to_block(entry);
    builder.append_block_params_for_function_params(entry);
    let params: Vec<Value> = builder.block_params(entry).to_vec();

    let mut values: HashMap<u32, Value> = HashMap::new();
    for (i, v) in params.iter().enumerate() {
        values.insert(i as u32, *v);
    }

    // ---- prologue ----
    // Zero every frame cell, then write slot_count.
    if let Some(fr) = frame {
        for c in 0..plan.ncells {
            let zero = builder.ins().iconst(types::I64, 0);
            builder.ins().stack_store(zero, fr, cell_offset(c));
        }
        let n = builder.ins().iconst(types::I32, plan.ncells as i64);
        builder.ins().stack_store(n, fr, Offset32::new(8));
    }

    // Params already live in slots 0..n in the IR; seed those slots.
    for (i, v) in params.iter().enumerate() {
        let ty = *func.slots.get(i).context("param slot")?;
        if is_managed(ty) {
            let &cell = plan.cell.get(&(i as u32)).context("managed param cell")?;
            let fr = frame.context("managed param implies frame")?;
            builder.ins().stack_store(*v, fr, cell_offset(cell));
        } else if let Some(ss) = slot_ss.get(&(i as u32)) {
            builder.ins().stack_store(*v, *ss, Offset32::new(0));
        }
    }

    // Push the shadow frame before any potential safepoint.
    if let Some(fr) = frame {
        let (s, gv) = extern_pair(
            &mut builder,
            &mut call_cache,
            "pickle_shadow_push",
            &[IrTy::Ptr],
            IrTy::Unit,
        );
        let fp = builder.ins().stack_addr(types::I64, fr, Offset32::new(0));
        let addr = builder.ins().symbol_value(types::I64, gv);
        builder.ins().call_indirect(s, addr, &[fp]);
    }

    // ---- body ----
    let mut current = Some(entry);
    for b in &func.blocks {
        let blk = blocks[&b.id.0];
        if current != Some(blk) {
            builder.switch_to_block(blk);
            current = Some(blk);
        }
        for ins in &b.instrs {
            lower_instr(
                module,
                func,
                &plan,
                &mut builder,
                frame,
                &slot_ss,
                &mut values,
                &mut call_cache,
                ins,
            )?;
        }
        lower_term(
            func,
            &mut builder,
            frame,
            &blocks,
            &values,
            &mut call_cache,
            &b.term,
        )?;
    }
    builder.seal_all_blocks();

    let mut externs: Vec<(String, Signature)> = call_cache
        .into_iter()
        .map(|(key, (_sr, _gv, sig))| {
            // The runtime symbol a relocation references is the bare
            // `symbol` argument of `extern_pair` (e.g. `pickle_str_from_bytes`),
            // while the cache key carries a signature suffix: parameters are
            // comma-joined and the return type is `->ret` (so a zero-param
            // extern has no comma at all: `pickle_panic_no_match->Unit`).
            // Report the bare symbol so the object backend imports the name
            // that every `symbol_value` relocation actually references.
            // Relocations carry only the name, so two signatures sharing a
            // bare symbol cannot both be imported; key on the bare name.
            let bare = key
                .split("->")
                .next()
                .unwrap_or(&key)
                .split(',')
                .next()
                .unwrap_or(&key)
                .to_string();
            (bare, sig)
        })
        .collect();
    externs.sort_by(|a, b| a.0.cmp(&b.0));
    externs.dedup_by(|a, b| a.0 == b.0);
    Ok((ctx.func, externs))
}

fn unit_value(builder: &mut FunctionBuilder) -> Value {
    builder.ins().iconst(types::I64, 0)
}

#[allow(clippy::too_many_arguments)]
/// Emit a call to a runtime native and return its single result value (or the
/// unit sentinel for `()`-returning calls). Shared by the generic extern-call
/// path and the fallible slow paths of [`try_inline_extern_call`].
fn call_extern_value(
    builder: &mut FunctionBuilder,
    call_cache: &mut HashMap<String, (SigRef, GlobalValue, Signature)>,
    symbol: &str,
    params: &[IrTy],
    ret: IrTy,
    iargs: &[Value],
) -> Value {
    let (s, gv) = extern_pair(builder, call_cache, symbol, params, ret);
    let addr = builder.ins().symbol_value(types::I64, gv);
    let inst = builder.ins().call_indirect(s, addr, iargs);
    builder
        .inst_results(inst)
        .first()
        .copied()
        .unwrap_or_else(|| unit_value(builder))
}

/// Managed-object header/payload offsets, shared with `runtime/src/object.rs`
/// and `runtime/src/layout.rs`. `Enums`/`Str`/etc. keep `size` at +16 because
/// the 24-byte header is `{ next: *mut, class_id: u32, flags: u32, size: u32,
/// _pad: u32 }`; every payload begins at +24.
const OBJ_SIZE_OFF: i64 = 16;
const OBJ_PAYLOAD_OFF: i64 = 24;
const SLOT_BYTES: i64 = 8;

/// Inline a few runtime natives whose semantics are plain memory reads/writes
/// at fixed offsets (class fields, enum tags/payloads, boxed-int payloads).
///
/// Every fallible edge (null object, out-of-bounds slot) takes a cold branch
/// that calls the *original* native, so null panics, bounds `null`/no-op
/// behaviour and `PKL_GC_TRACE` taps stay byte-identical to the runtime. The
/// emitted IR/IRX text is untouched; only generated machine code changes.
///
/// Returns `Ok(true)` when the call was lowered inline (including `dst`
/// bookkeeping), `Ok(false)` when the caller should fall through to the
/// generic indirect-call path.
#[allow(clippy::too_many_arguments)]
fn try_inline_extern_call(
    plan: &Plan,
    builder: &mut FunctionBuilder,
    frame: Option<StackSlot>,
    symbol: &str,
    params: &[IrTy],
    ret: IrTy,
    iargs: &[Value],
    values: &mut HashMap<u32, Value>,
    call_cache: &mut HashMap<String, (SigRef, GlobalValue, Signature)>,
    dst: Option<Temp>,
) -> Result<bool> {
    if iargs.is_empty() {
        return Ok(false);
    }
    let obj = iargs[0];
    let ret_managed = is_managed(ret);
    let finish = |builder: &mut FunctionBuilder, values: &mut HashMap<u32, Value>, v: Value| {
        if let Some(d) = dst {
            if ret_managed {
                write_through(builder, plan, frame, d.0, v);
            }
            values.insert(d.0, v);
        }
    };
    match symbol {
        // `pickle_obj_slot_get(obj, slot) -> ptr`: raw load of a class field.
        // Runtime (`class_slot`): null/negative/`slot > slots` yield null,
        // where `slots = (size - 24 - 8) / 8` and `size < 32` is rejected.
        "pickle_obj_slot_get" => {
            let slot = iargs[1];
            let zero = builder.ins().iconst(types::I64, 0);
            let not_null = builder.ins().icmp(IntCC::NotEqual, obj, zero);
            let nonneg = builder
                .ins()
                .icmp(IntCC::SignedGreaterThanOrEqual, slot, zero);
            let sz32 = builder.ins().load(
                types::I32,
                MemFlags::trusted(),
                obj,
                Offset32::new(OBJ_SIZE_OFF as i32),
            );
            let sz = builder.ins().uextend(types::I64, sz32);
            let c32 = builder.ins().iconst(types::I64, 32);
            let big_enough = builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThanOrEqual, sz, c32);
            let sz_m32 = builder.ins().isub(sz, c32);
            let c3 = builder.ins().iconst(types::I64, 3);
            let slots = builder.ins().ushr(sz_m32, c3);
            let in_bounds = builder
                .ins()
                .icmp(IntCC::UnsignedLessThanOrEqual, slot, slots);
            let nn = builder.ins().band(not_null, nonneg);
            let be = builder.ins().band(big_enough, in_bounds);
            let valid = builder.ins().band(nn, be);
            let off = builder.ins().iconst(types::I64, OBJ_PAYLOAD_OFF);
            let sh = builder.ins().ishl(slot, c3);
            let inner = builder.ins().iadd(off, sh);
            let addr = builder.ins().iadd(obj, inner);
            let merge = emit_guarded_load(
                builder,
                call_cache,
                symbol,
                params,
                ret,
                iargs,
                valid,
                |b| b.ins().load(types::I64, MemFlags::trusted(), addr, 0),
            );
            finish(builder, values, builder.block_params(merge)[0]);
            Ok(true)
        }
        // `pickle_obj_slot_set(obj, slot, value)`: raw store of a class field.
        // Same guards; out-of-range writes are silently ignored by the runtime.
        "pickle_obj_slot_set" => {
            let slot = iargs[1];
            let zero = builder.ins().iconst(types::I64, 0);
            let not_null = builder.ins().icmp(IntCC::NotEqual, obj, zero);
            let nonneg = builder
                .ins()
                .icmp(IntCC::SignedGreaterThanOrEqual, slot, zero);
            let sz32 = builder.ins().load(
                types::I32,
                MemFlags::trusted(),
                obj,
                Offset32::new(OBJ_SIZE_OFF as i32),
            );
            let sz = builder.ins().uextend(types::I64, sz32);
            let c32 = builder.ins().iconst(types::I64, 32);
            let big_enough = builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThanOrEqual, sz, c32);
            let sz_m32 = builder.ins().isub(sz, c32);
            let c3 = builder.ins().iconst(types::I64, 3);
            let slots = builder.ins().ushr(sz_m32, c3);
            let in_bounds = builder
                .ins()
                .icmp(IntCC::UnsignedLessThanOrEqual, slot, slots);
            let nn = builder.ins().band(not_null, nonneg);
            let be = builder.ins().band(big_enough, in_bounds);
            let valid = builder.ins().band(nn, be);
            let off = builder.ins().iconst(types::I64, OBJ_PAYLOAD_OFF);
            let sh = builder.ins().ishl(slot, c3);
            let inner = builder.ins().iadd(off, sh);
            let addr = builder.ins().iadd(obj, inner);
            emit_guarded_store(
                builder, call_cache, symbol, params, ret, iargs, valid, addr, iargs[2],
            );
            Ok(true)
        }
        // `pickle_enum_tag(obj) -> int`: unguarded load of the tag i64 at +24.
        "pickle_enum_tag" => {
            let off = builder.ins().iconst(types::I64, OBJ_PAYLOAD_OFF);
            let addr = builder.ins().iadd(obj, off);
            let v = builder.ins().load(types::I64, MemFlags::trusted(), addr, 0);
            finish(builder, values, v);
            Ok(true)
        }
        // `pickle_enum_field(obj, idx) -> ptr`: payload field load, null when
        // out of bounds. `field_count = (size - 24 - 8).max(0) / 8` (layout.rs).
        "pickle_enum_field" => {
            let idx = iargs[1];
            let zero = builder.ins().iconst(types::I64, 0);
            let not_null = builder.ins().icmp(IntCC::NotEqual, obj, zero);
            let nonneg = builder
                .ins()
                .icmp(IntCC::SignedGreaterThanOrEqual, idx, zero);
            let sz32 = builder.ins().load(
                types::I32,
                MemFlags::trusted(),
                obj,
                Offset32::new(OBJ_SIZE_OFF as i32),
            );
            let sz = builder.ins().uextend(types::I64, sz32);
            let c32 = builder.ins().iconst(types::I64, 32);
            let sz_ge_32 = builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThanOrEqual, sz, c32);
            let sz_m32 = builder.ins().isub(sz, c32);
            let sat = builder.ins().select(sz_ge_32, sz_m32, zero);
            let c3 = builder.ins().iconst(types::I64, 3);
            let count = builder.ins().ushr(sat, c3);
            let in_bounds = builder.ins().icmp(IntCC::UnsignedLessThan, idx, count);
            let nn = builder.ins().band(not_null, nonneg);
            let valid = builder.ins().band(nn, in_bounds);
            let off = builder
                .ins()
                .iconst(types::I64, OBJ_PAYLOAD_OFF + SLOT_BYTES);
            let sh = builder.ins().ishl(idx, c3);
            let inner = builder.ins().iadd(off, sh);
            let addr = builder.ins().iadd(obj, inner);
            let merge = emit_guarded_load(
                builder,
                call_cache,
                symbol,
                params,
                ret,
                iargs,
                valid,
                |b| b.ins().load(types::I64, MemFlags::trusted(), addr, 0),
            );
            finish(builder, values, builder.block_params(merge)[0]);
            Ok(true)
        }
        // `pickle_unbox_i64(obj) -> int`: load of the i64 payload at +24; a
        // null object takes the slow path so the runtime's panic fires.
        "pickle_unbox_i64" => {
            let zero = builder.ins().iconst(types::I64, 0);
            let not_null = builder.ins().icmp(IntCC::NotEqual, obj, zero);
            let off = builder.ins().iconst(types::I64, OBJ_PAYLOAD_OFF);
            let addr = builder.ins().iadd(obj, off);
            let fast = builder.ins().load(types::I64, MemFlags::trusted(), addr, 0);
            let slow = builder.create_block();
            let merge = builder.create_block();
            builder.append_block_param(merge, types::I64);
            builder.ins().brif(
                not_null,
                merge,
                &[BlockArg::Value(fast)],
                slow,
                &[] as &[BlockArg],
            );
            builder.switch_to_block(slow);
            let slow_v = call_extern_value(builder, call_cache, symbol, params, ret, iargs);
            builder.ins().jump(merge, &[BlockArg::Value(slow_v)]);
            builder.switch_to_block(merge);
            finish(builder, values, builder.block_params(merge)[0]);
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Shared tail of the guarded-load inline paths: branch on `valid`; the fast
/// case loads via `addr` and the cold case calls the original native. Returns
/// the merge block whose first parameter is the result.
#[allow(clippy::too_many_arguments)]
fn emit_guarded_load(
    builder: &mut FunctionBuilder,
    call_cache: &mut HashMap<String, (SigRef, GlobalValue, Signature)>,
    symbol: &str,
    params: &[IrTy],
    ret: IrTy,
    iargs: &[Value],
    valid: Value,
    fast_load: impl FnOnce(&mut FunctionBuilder) -> Value,
) -> Block {
    let slow = builder.create_block();
    let merge = builder.create_block();
    builder.append_block_param(merge, types::I64);
    let fast = fast_load(builder);
    builder.ins().brif(
        valid,
        merge,
        &[BlockArg::Value(fast)],
        slow,
        &[] as &[BlockArg],
    );
    builder.switch_to_block(slow);
    let slow_v = call_extern_value(builder, call_cache, symbol, params, ret, iargs);
    builder.ins().jump(merge, &[BlockArg::Value(slow_v)]);
    builder.switch_to_block(merge);
    merge
}

/// Shared tail of the guarded-store inline paths: store through `addr` on the
/// fast path, call the original native on the cold path.
#[allow(clippy::too_many_arguments)]
fn emit_guarded_store(
    builder: &mut FunctionBuilder,
    call_cache: &mut HashMap<String, (SigRef, GlobalValue, Signature)>,
    symbol: &str,
    params: &[IrTy],
    ret: IrTy,
    iargs: &[Value],
    valid: Value,
    addr: Value,
    value: Value,
) {
    let fast = builder.create_block();
    let slow = builder.create_block();
    let done = builder.create_block();
    builder
        .ins()
        .brif(valid, fast, &[] as &[BlockArg], slow, &[] as &[BlockArg]);
    builder.switch_to_block(fast);
    builder.ins().store(MemFlags::trusted(), value, addr, 0);
    builder.ins().jump(done, &[] as &[BlockArg]);
    builder.switch_to_block(slow);
    call_extern_value(builder, call_cache, symbol, params, ret, iargs);
    builder.ins().jump(done, &[] as &[BlockArg]);
    builder.switch_to_block(done);
}

#[allow(clippy::too_many_arguments)]
fn lower_instr(
    module: &IrModule,
    func: &IrFunc,
    plan: &Plan,
    builder: &mut FunctionBuilder,
    frame: Option<StackSlot>,
    slot_ss: &HashMap<u32, StackSlot>,
    values: &mut HashMap<u32, Value>,
    call_cache: &mut HashMap<String, (SigRef, GlobalValue, Signature)>,
    ins: &IrInstr,
) -> Result<()> {
    match ins {
        IrInstr::Const { dst, c } => {
            let v = match c {
                IrConst::Int(x) => builder.ins().iconst(types::I64, *x),
                IrConst::Char(x) => builder.ins().iconst(types::I32, *x as i64),
                IrConst::Bool(x) => builder.ins().iconst(types::I8, if *x { 1 } else { 0 }),
                IrConst::Float(bits) => {
                    let int = builder.ins().iconst(types::I64, *bits as i64);
                    builder.ins().bitcast(types::F64, MemFlags::new(), int)
                }
                IrConst::Null => builder.ins().iconst(types::I64, 0),
                IrConst::Str(sid) => {
                    let (_s, gv) = extern_pair(
                        builder,
                        call_cache,
                        &format!("pkl_strdata_{}", sid.0),
                        &[IrTy::Ptr, IrTy::Int],
                        IrTy::Ptr,
                    );
                    let data = builder.ins().symbol_value(types::I64, gv);
                    let len = builder.ins().iconst(
                        types::I64,
                        module.strings.get(sid.0).map_or(0, Vec::len) as i64,
                    );
                    let (fs, fgv) = extern_pair(
                        builder,
                        call_cache,
                        "pickle_str_from_bytes",
                        &[IrTy::Ptr, IrTy::Int],
                        IrTy::Ptr,
                    );
                    let addr = builder.ins().symbol_value(types::I64, fgv);
                    let inst = builder.ins().call_indirect(fs, addr, &[data, len]);
                    builder
                        .inst_results(inst)
                        .first()
                        .copied()
                        .unwrap_or_else(|| unit_value(builder))
                }
                IrConst::StrAddr(sid) => {
                    let (_s, gv) = extern_pair(
                        builder,
                        call_cache,
                        &format!("pkl_strdata_{}", sid.0),
                        &[IrTy::Ptr, IrTy::Int],
                        IrTy::Ptr,
                    );
                    builder.ins().symbol_value(types::I64, gv)
                }
                IrConst::FuncAddr(fid) => {
                    let f = &module.funcs[fid.0];
                    let params: Vec<IrTy> = f.params.iter().map(|p| p.ty).collect();
                    let (_s, gv) = extern_pair(builder, call_cache, &f.symbol, &params, f.ret);
                    builder.ins().symbol_value(types::I64, gv)
                }
            };
            values.insert(dst.0, v);
            write_through(builder, plan, frame, dst.0, v);
        }
        IrInstr::UnOp { dst, op, v } => {
            let x = *values.get(&v.0).context("unop operand")?;
            let vv = match op {
                UnOp::Neg => {
                    if plan.types.get(&v.0).copied() == Some(IrTy::Float) {
                        builder.ins().fneg(x)
                    } else {
                        builder.ins().ineg(x)
                    }
                }
                // Logical `not` on a byte-sized bool.
                UnOp::Not => {
                    let zero = builder.ins().iconst(types::I8, 0);
                    builder.ins().icmp(IntCC::Equal, x, zero)
                }
                UnOp::BitNot => builder.ins().bnot(x),
            };
            values.insert(dst.0, vv);
            write_through(builder, plan, frame, dst.0, vv);
        }
        IrInstr::BinOp { dst, op, a, b } => {
            let av = *values.get(&a.0).context("binop lhs")?;
            let bv = *values.get(&b.0).context("binop rhs")?;
            let is_float = plan.types.get(&a.0).copied() == Some(IrTy::Float);
            let vv = match op {
                BinOp::Add => {
                    if is_float {
                        builder.ins().fadd(av, bv)
                    } else {
                        builder.ins().iadd(av, bv)
                    }
                }
                BinOp::Sub => {
                    if is_float {
                        builder.ins().fsub(av, bv)
                    } else {
                        builder.ins().isub(av, bv)
                    }
                }
                BinOp::Mul => {
                    if is_float {
                        builder.ins().fmul(av, bv)
                    } else {
                        builder.ins().imul(av, bv)
                    }
                }
                BinOp::Div => {
                    if is_float {
                        builder.ins().fdiv(av, bv)
                    } else {
                        builder.ins().sdiv(av, bv)
                    }
                }
                BinOp::Mod => {
                    if is_float {
                        let (s, gv) = extern_pair(
                            builder,
                            call_cache,
                            "pickle_fmod",
                            &[IrTy::Float, IrTy::Float],
                            IrTy::Float,
                        );
                        let addr = builder.ins().symbol_value(types::I64, gv);
                        let inst = builder.ins().call_indirect(s, addr, &[av, bv]);
                        builder
                            .inst_results(inst)
                            .first()
                            .copied()
                            .unwrap_or_else(|| unit_value(builder))
                    } else {
                        builder.ins().srem(av, bv)
                    }
                }
                BinOp::Shl => builder.ins().ishl(av, bv),
                BinOp::Shr => builder.ins().sshr(av, bv),
                BinOp::BitAnd => builder.ins().band(av, bv),
                BinOp::BitOr => builder.ins().bor(av, bv),
                BinOp::BitXor => builder.ins().bxor(av, bv),
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                    if is_float {
                        let cc = match op {
                            BinOp::Eq => FloatCC::Equal,
                            BinOp::Ne => FloatCC::NotEqual,
                            BinOp::Lt => FloatCC::LessThan,
                            BinOp::Le => FloatCC::LessThanOrEqual,
                            BinOp::Gt => FloatCC::GreaterThan,
                            BinOp::Ge => FloatCC::GreaterThanOrEqual,
                            _ => unreachable!(),
                        };
                        builder.ins().fcmp(cc, av, bv)
                    } else {
                        let cc = match op {
                            BinOp::Eq => IntCC::Equal,
                            BinOp::Ne => IntCC::NotEqual,
                            BinOp::Lt => IntCC::SignedLessThan,
                            BinOp::Le => IntCC::SignedLessThanOrEqual,
                            BinOp::Gt => IntCC::SignedGreaterThan,
                            BinOp::Ge => IntCC::SignedGreaterThanOrEqual,
                            _ => unreachable!(),
                        };
                        builder.ins().icmp(cc, av, bv)
                    }
                }
            };
            values.insert(dst.0, vv);
            write_through(builder, plan, frame, dst.0, vv);
        }
        IrInstr::Copy { dst, v } => {
            let x = *values.get(&v.0).context("copy operand")?;
            values.insert(dst.0, x);
            // A copied managed value must also root the destination cell.
            write_through(builder, plan, frame, dst.0, x);
        }
        IrInstr::Itof { dst, v } => {
            let x = *values.get(&v.0).context("itof operand")?;
            let vv = builder.ins().fcvt_from_sint(types::F64, x);
            values.insert(dst.0, vv);
        }
        IrInstr::Ftoi { dst, v } => {
            let x = *values.get(&v.0).context("ftoi operand")?;
            let vv = builder.ins().fcvt_to_sint_sat(types::I64, x);
            values.insert(dst.0, vv);
        }
        IrInstr::Chartoi { dst, v } => {
            let x = *values.get(&v.0).context("chartoi operand")?;
            let vv = builder.ins().uextend(types::I64, x);
            values.insert(dst.0, vv);
        }
        IrInstr::Itochar { dst, v } => {
            let x = *values.get(&v.0).context("itochar operand")?;
            let vv = builder.ins().ireduce(types::I32, x);
            values.insert(dst.0, vv);
        }
        IrInstr::LoadSlot { dst, slot } => {
            let v = load_slot(builder, func, plan, frame, slot_ss, *slot)?;
            values.insert(dst.0, v);
            // A loaded managed value is the same object already stashed in the
            // source slot's cell, but that cell can be overwritten by a later
            // `StoreSlot` *before* this temp's next use. If the temp survives
            // across a subsequent allocating call, a collection can then free
            // the object while only its register copy remains live: root it in
            // its own cell too so the value pins until this temp is superseded.
            // (Was the missing `write_through` here that made the selfhost emit
            // non-deterministic under auto-GC: a slot-reuse pattern left a map
            // key/holder only in a CLIF register at a safe point.)
            if is_managed(
                func.slots
                    .get(slot.0 as usize)
                    .copied()
                    .unwrap_or(IrTy::Ptr),
            ) {
                write_through(builder, plan, frame, dst.0, v);
            }
        }
        IrInstr::StoreSlot { slot, v } => {
            let x = *values.get(&v.0).context("store operand")?;
            store_slot(builder, func, plan, frame, slot_ss, *slot, x);
        }
        IrInstr::LocalAddr { dst, slot } => {
            let ss = slot_ss
                .get(&slot.0)
                .context("raw address of a managed or absent slot")?;
            let a = builder.ins().stack_addr(types::I64, *ss, Offset32::new(0));
            values.insert(dst.0, a);
        }
        IrInstr::LoadRaw { dst, addr, ty } => {
            let a = *values.get(&addr.0).context("loadraw address")?;
            let v = builder.ins().load(clif_ty(*ty), MemFlags::trusted(), a, 0);
            values.insert(dst.0, v);
            // A managed value loaded out of a raw buffer (list element, enum
            // payload, class field) must be rooted in its shadow cell too, or a
            // collection during a later allocating call can free it while the
            // SSA value still references it.
            if is_managed(*ty) {
                write_through(builder, plan, frame, dst.0, v);
            }
        }
        IrInstr::StoreRaw { addr, v, ty } => {
            let a = *values.get(&addr.0).context("storeraw address")?;
            let x = *values.get(&v.0).context("storeraw value")?;
            builder.ins().store(MemFlags::trusted(), x, a, 0);
            let _ = ty;
        }
        IrInstr::GlobalAddr { dst, id } => {
            let gv = data_pair(builder, call_cache, &module.globals[*id].symbol);
            let v = builder.ins().symbol_value(types::I64, gv);
            values.insert(dst.0, v);
        }
        IrInstr::LoadGlobal { dst, id, ty } => {
            let gv = data_pair(builder, call_cache, &module.globals[*id].symbol);
            let a = builder.ins().symbol_value(types::I64, gv);
            let v = builder.ins().load(clif_ty(*ty), MemFlags::trusted(), a, 0);
            values.insert(dst.0, v);
            if is_managed(*ty) {
                write_through(builder, plan, frame, dst.0, v);
            }
        }
        IrInstr::StoreGlobal { id, v, ty } => {
            let gv = data_pair(builder, call_cache, &module.globals[*id].symbol);
            let a = builder.ins().symbol_value(types::I64, gv);
            let x = *values.get(&v.0).context("storeglobal value")?;
            builder.ins().store(MemFlags::trusted(), x, a, 0);
            let _ = ty;
        }
        IrInstr::Call { dst, callee, args } => {
            let mut iargs: Vec<Value> = Vec::with_capacity(args.len());
            for a in args {
                iargs.push(*values.get(&a.0).context("call arg")?);
            }
            let (name, params, ret) = match callee {
                Callee::Func(fid) => {
                    let f = &module.funcs[fid.0];
                    let params = f.params.iter().map(|p| p.ty).collect::<Vec<_>>();
                    (f.symbol.clone(), params, f.ret)
                }
                Callee::Extern(eid) => {
                    let e = &module.externs[eid.0];
                    (e.symbol.clone(), e.params.clone(), e.ret)
                }
            };
            if matches!(callee, Callee::Extern(_)) {
                for (ai, a) in args.iter().enumerate() {
                    if ai < params.len() && is_managed(params[ai]) {
                        let ty = plan.types.get(&a.0).copied().unwrap_or(IrTy::Ptr);
                        if !is_managed(ty) {
                            eprintln!(
                                "UNROOTED-ARG fn={} extern={} arg#{} temp={} typed={ty:?} (managed param)",
                                func.name, name, ai, a.0
                            );
                        }
                    }
                }
            }
            if let Callee::Extern(_) = callee {
                if try_inline_extern_call(
                    plan, builder, frame, &name, &params, ret, &iargs, values, call_cache, *dst,
                )? {
                    return Ok(());
                }
            }
            let (s, gv) = extern_pair(builder, call_cache, &name, &params, ret);
            let addr = builder.ins().symbol_value(types::I64, gv);
            let inst = builder.ins().call_indirect(s, addr, &iargs);
            if let Some(d) = dst {
                let v = builder
                    .inst_results(inst)
                    .first()
                    .copied()
                    .unwrap_or_else(|| unit_value(builder));
                values.insert(d.0, v);
                if is_managed(ret) {
                    write_through(builder, plan, frame, d.0, v);
                }
            }
        }
        IrInstr::CallInd {
            dst,
            fn_addr,
            params,
            ret,
            args,
        } => {
            let mut iargs: Vec<Value> = Vec::with_capacity(args.len());
            for a in args {
                iargs.push(*values.get(&a.0).context("callind arg")?);
            }
            let addr = *values.get(&fn_addr.0).context("callind address")?;
            let s = builder.import_signature(extern_signature(params.as_slice(), *ret));
            let inst = builder.ins().call_indirect(s, addr, &iargs);
            if let Some(d) = dst {
                let v = builder
                    .inst_results(inst)
                    .first()
                    .copied()
                    .unwrap_or_else(|| unit_value(builder));
                values.insert(d.0, v);
                if is_managed(*ret) {
                    write_through(builder, plan, frame, d.0, v);
                }
            }
        }
    }
    Ok(())
}

/// Read an IR slot through its backing storage (shadow cell or stack slot).
fn load_slot(
    builder: &mut FunctionBuilder,
    func: &IrFunc,
    plan: &Plan,
    frame: Option<StackSlot>,
    slot_ss: &HashMap<u32, StackSlot>,
    slot: Slot,
) -> Result<Value> {
    let ty = *func.slots.get(slot.0 as usize).context("slot type")?;
    if is_managed(ty) {
        let &cell = plan.cell.get(&slot.0).context("managed slot cell")?;
        let fr = frame.context("managed slot implies frame")?;
        Ok(builder.ins().stack_load(types::I64, fr, cell_offset(cell)))
    } else if let Some(ss) = slot_ss.get(&slot.0) {
        Ok(builder.ins().stack_load(clif_ty(ty), *ss, Offset32::new(0)))
    } else {
        Ok(unit_value(builder))
    }
}

/// Write an IR slot through its backing storage.
fn store_slot(
    builder: &mut FunctionBuilder,
    func: &IrFunc,
    plan: &Plan,
    frame: Option<StackSlot>,
    slot_ss: &HashMap<u32, StackSlot>,
    slot: Slot,
    v: Value,
) {
    let ty = func.slots[slot.0 as usize];
    if is_managed(ty) {
        let &cell = plan.cell.get(&slot.0).expect("managed slot cell");
        let fr = frame.expect("managed slot implies frame");
        builder.ins().stack_store(v, fr, cell_offset(cell));
    } else if let Some(ss) = slot_ss.get(&slot.0) {
        builder.ins().stack_store(v, *ss, Offset32::new(0));
    }
}

/// Lower a block terminator. The shadow frame is popped before any exit.
fn lower_term(
    func: &IrFunc,
    builder: &mut FunctionBuilder,
    frame: Option<StackSlot>,
    blocks: &HashMap<u32, Block>,
    values: &HashMap<u32, Value>,
    call_cache: &mut HashMap<String, (SigRef, GlobalValue, Signature)>,
    term: &IrTerm,
) -> Result<()> {
    match term {
        IrTerm::Branch { target } => {
            builder.ins().jump(blocks[&target.0], &[] as &[BlockArg]);
        }
        IrTerm::BranchIf { cond, then, else_ } => {
            let c = *values.get(&cond.0).context("branch cond")?;
            builder.ins().brif(
                c,
                blocks[&then.0],
                &[] as &[BlockArg],
                blocks[&else_.0],
                &[] as &[BlockArg],
            );
        }
        IrTerm::Return { v } => {
            // The shadow frame is pushed once in the prologue, so it is popped
            // exactly once, at each exit (branches between blocks must not
            // pop, or a multi-block function would pop more often than it
            // pushed and trip the runtime's LIFO assert).
            if let Some(fr) = frame {
                let (s, gv) = extern_pair(
                    builder,
                    call_cache,
                    "pickle_shadow_pop",
                    &[IrTy::Ptr],
                    IrTy::Unit,
                );
                let fp = builder.ins().stack_addr(types::I64, fr, Offset32::new(0));
                let addr = builder.ins().symbol_value(types::I64, gv);
                builder.ins().call_indirect(s, addr, &[fp]);
            }
            match v {
                Some(t) if !func.ret.is_unit() => {
                    let x = *values.get(&t.0).context("return value")?;
                    builder.ins().return_(&[x]);
                }
                _ => {
                    builder.ins().return_(&[] as &[Value]);
                }
            }
        }
        IrTerm::Unreachable => {
            let code = TrapCode::user(1).expect("user trap code 1 is valid");
            builder.ins().trap(code);
        }
    }
    Ok(())
}

/// Runtime addresses the linker knows about, by `pickle_*` symbol name. The
/// runtime owns that list (see `pickle_runtime::declare_abi!`); the host adds
/// exactly one symbol of its own, `pickle_fmod`, which the toolchain provides
/// in place of the runtime.
fn runtime_addr(name: &str) -> Option<usize> {
    match name {
        "pickle_fmod" => Some(pickle_fmod as *const () as usize),
        _ => abi_runtime_addr(name),
    }
}

/// Register every runtime ABI function into the crash reporter so native
/// pickle_* frames get names in backtraces. Call once at startup.
pub fn register_runtime_symbols() {
    for name in pickle_runtime::ALL_ABI_NAMES
        .iter()
        .copied()
        .chain(["pickle_fmod"])
    {
        if let Some(a) = runtime_addr(name) {
            crate::iced::register_native(name, a);
        }
    }
}

fn external_name_of(name: &ExternalName) -> Option<String> {
    match name {
        ExternalName::TestCase(t) => Some(String::from_utf8_lossy(t.raw()).into_owned()),
        _ => None,
    }
}

/// Patch a single relocation inside the executable mapping.
fn patch_reloc(
    code: &mut [u8],
    func_off: usize,
    reloc: &FinalizedMachReloc,
    target_abs: usize,
) -> Result<()> {
    let p = reloc.offset as usize;
    let abs = target_abs as i128 + reloc.addend as i128;
    let p_abs = func_off + p;
    let region_base = 0usize;
    let _ = region_base;
    let size = match reloc.kind {
        Reloc::Abs8 => 8,
        Reloc::Abs4 => 4,
        Reloc::X86PCRel4 | Reloc::X86CallPCRel4 | Reloc::X86CallPLTRel4 => 4,
        other => bail!("unsupported relocation kind {other:?}"),
    };
    if p + size > code.len() {
        bail!(
            "relocation at offset {p} (kind {:?}) overflows function of {} bytes",
            reloc.kind,
            code.len()
        );
    }
    match reloc.kind {
        Reloc::Abs8 => {
            let v = (abs & 0xFFFF_FFFF_FFFF_FFFF) as u64;
            code[p..p + 8].copy_from_slice(&v.to_le_bytes());
        }
        Reloc::Abs4 => {
            let v = (abs & 0xFFFF_FFFF) as u32;
            code[p..p + 4].copy_from_slice(&v.to_le_bytes());
        }
        Reloc::X86PCRel4 | Reloc::X86CallPCRel4 | Reloc::X86CallPLTRel4 => {
            // PC-relative displacement is measured from the end of the
            // instruction, i.e. `target - (P + 4)`; the provided addend
            // encodes that -4 (or the -4/leaq adjustment) so reuse it.
            let disp = (abs - p_abs as i128) as i32;
            code[p..p + 4].copy_from_slice(&disp.to_le_bytes());
        }
        _ => unreachable!("handled above"),
    }
    Ok(())
}

/// The executable mapping produced by [`Jit::compile`]. Freed on drop.
pub struct JitProgram {
    base: *mut u8,
    len: usize,
    entry: usize,
    symbols: HashMap<String, usize>,
}

// SAFETY: the mapping is fully initialised by `compile` and read-only
// afterwards (`run` never mutates it), so ownership may move to a single
// worker thread; drop of the final owner frees the pages exactly once.
unsafe impl Send for JitProgram {}

impl Drop for JitProgram {
    fn drop(&mut self) {
        // SAFETY: `base`/`len` come from `mem::alloc`; matched by `mem::free`.
        unsafe {
            mem::free(self.base, self.len);
        }
    }
}

impl JitProgram {
    /// Execute the compiled `pickle_main`.
    ///
    /// Returns a `throw` payload that escaped `main` uncaught. Propagation is
    /// cooperative, so such a throw makes the entry function return early just
    /// like a normal return; only the host can tell the difference.
    ///
    /// # Safety
    ///
    /// The process must have run `pickle_runtime_init` first.
    pub unsafe fn run(&self) -> Option<String> {
        // SAFETY: `entry` points at the compiled pickle_main, a no-arg
        // void-returning function with the host C calling convention.
        let f: unsafe extern "C" fn() =
            unsafe { std::mem::transmute::<usize, unsafe extern "C" fn()>(self.entry) };
        unsafe {
            f();
        }
        pickle_runtime::take_escaped_exception()
    }

    /// Absolute address of a compiled symbol (a `pickle_*` function or a
    /// string data blob), if present in this module.
    pub fn symbol_addr(&self, name: &str) -> Option<usize> {
        self.symbols.get(name).copied()
    }
}

impl Jit {
    /// Compile the whole module, lay out all code plus string data into one
    /// executable mapping, and patch every relocation.
    pub fn compile(&self, module: &IrModule) -> Result<JitProgram> {
        self.compile_expect_main(module, true)
    }

    /// Compile with no `main` required (test modules). The returned program
    /// exposes `symbol_addr` for the `pickle_test_<name>` descriptors.
    pub fn compile_no_main(&self, module: &IrModule) -> Result<JitProgram> {
        self.compile_expect_main(module, false)
    }

    /// Shared implementation of [`Jit::compile`], optionally requiring a
    /// `pickle_main` entry.
    fn compile_expect_main(&self, module: &IrModule, need_main: bool) -> Result<JitProgram> {
        let mut cf: Vec<CompiledFunc> = Vec::with_capacity(module.funcs.len());
        for fid in 0..module.funcs.len() {
            cf.push(self.compile_function(module, FuncId(fid))?);
        }
        if cf.is_empty() {
            bail!("module has no functions to compile");
        }
        if need_main && !cf.iter().any(|f| f.symbol == "pickle_main") {
            bail!("module has no `main`; nothing to run");
        }

        // Lay out: functions first, then string blobs, all 16-aligned.
        let mut cursor = 0usize;
        let mut func_offs = Vec::with_capacity(cf.len());
        for f in &cf {
            cursor = align_up(cursor, CODE_ALIGN);
            func_offs.push(cursor);
            cursor += f.bytes.len();
        }
        let mut str_offs = Vec::with_capacity(module.strings.len());
        for s in &module.strings {
            cursor = align_up(cursor, CODE_ALIGN);
            str_offs.push(cursor);
            cursor += s.len();
        }
        // Top-level `var` slots. Unlike the string blobs these are *written*,
        // so each gets a full 8-byte cell regardless of how small the declared
        // type looks; the init bytes are copied over the zeroed cell.
        let mut glob_offs = Vec::with_capacity(module.globals.len());
        for _g in &module.globals {
            cursor = align_up(cursor, DATA_ALIGN);
            glob_offs.push(cursor);
            cursor += DATA_ALIGN;
        }
        let total = align_up(cursor, CODE_ALIGN);
        let base = mem::alloc(total).with_context(|| "cannot allocate executable memory")?;

        // Copy in the machine code and the string blobs.
        for (i, f) in cf.iter().enumerate() {
            // SAFETY: `base + func_offs[i]` is inside the mapping.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    f.bytes.as_ptr(),
                    base.add(func_offs[i]),
                    f.bytes.len(),
                );
            }
        }
        for (i, s) in module.strings.iter().enumerate() {
            // SAFETY: `base + str_offs[i]` is inside the mapping.
            unsafe {
                std::ptr::copy_nonoverlapping(s.as_ptr(), base.add(str_offs[i]), s.len());
            }
        }
        // The mapping is already zeroed by `mem::alloc`, so a global with no
        // initializer needs no copy -- only an explicit one must be written,
        // and it must never exceed its cell.
        for (i, g) in module.globals.iter().enumerate() {
            let Some(bytes) = &g.init else { continue };
            assert!(
                bytes.len() <= DATA_ALIGN,
                "global `{}` initializer is {} bytes, over the {DATA_ALIGN}-byte cell",
                g.symbol,
                bytes.len()
            );
            // SAFETY: `bytes.len() <= DATA_ALIGN` and the cell is inside the
            // mapping; the cell is distinct per global so no overlap.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    base.add(glob_offs[i]),
                    bytes.len(),
                );
            }
        }

        // Symbol table over absolute addresses.
        let mut symbols: HashMap<String, usize> = HashMap::new();
        for (i, f) in cf.iter().enumerate() {
            symbols.insert(f.symbol.clone(), base as usize + func_offs[i]);
        }
        for (i, _s) in module.strings.iter().enumerate() {
            symbols.insert(format!("pkl_strdata_{i}"), base as usize + str_offs[i]);
        }
        for (i, g) in module.globals.iter().enumerate() {
            symbols.insert(g.symbol.clone(), base as usize + glob_offs[i]);
        }
        for (i, f) in cf.iter().enumerate() {
            for r in &f.relocs {
                let target = match &r.target {
                    FinalizedRelocTarget::Func(off) => base as usize + func_offs[i] + *off as usize,
                    FinalizedRelocTarget::ExternalName(name) => {
                        let sym = external_name_of(name).with_context(|| {
                            format!("relocation to unsupported name {:?}", name)
                        })?;
                        if let Some(&a) = symbols.get(&sym) {
                            a
                        } else if let Some(a) = runtime_addr(&sym) {
                            a
                        } else {
                            bail!("unresolved symbol `{sym}` in `{}`", cf[i].symbol);
                        }
                    }
                };
                // SAFETY: relocated range stays inside the function's bytes.
                let code = unsafe {
                    std::slice::from_raw_parts_mut(base.add(func_offs[i]), cf[i].bytes.len())
                };
                patch_reloc(code, base as usize + func_offs[i], r, target)?;
            }
        }

        // The entry symbol is required for `main`-centric use but not for the
        // test runner, which addresses each test directly by symbol.
        let entry = symbols.get("pickle_main").copied().unwrap_or(0);
        crate::iced::register_code_range(base as usize, total, symbols.clone());
        // `PKL_GC_CHECK` probes report the JIT call chain of a dead/mistyped
        // managed argument; this is the only symbol resolver they can reach.
        pickle_runtime::abi::pickle_gc_set_caller_hook(gc_check_caller_hook as *const () as usize);
        Ok(JitProgram {
            base,
            len: total,
            entry,
            symbols,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pickle_compiler::diag::{DiagnosticSink, SourceMap};
    use pickle_compiler::front::frontend;
    use pickle_compiler::ir::{BlockId, ExternId, IrBlock, IrExtern, StrId, Temp};
    use pickle_runtime::abi;
    use std::sync::Once;

    /// The test binary spins tests up on several threads, and the runtime is a
    /// process global -- so bring it up exactly once and leave it up.
    static RUNTIME_ONCE: Once = Once::new();

    fn bring_up_runtime() {
        RUNTIME_ONCE.call_once(|| {
            abi::pickle_runtime_init();
        });
    }

    /// Compile a source module (front-end + check + lower) and run it through
    /// the JIT with a live runtime. Returns the module so tests can assert on
    /// the lowered shape after the fact.
    fn run_source(src: &str) -> IrModule {
        let mut map = SourceMap::default();
        let diags = DiagnosticSink::new();
        let module = frontend("test.pkl", src, &mut map, &diags)
            .and_then(|out| pickle_compiler::emit::emit_ir(&out.program, &out.resolved, &diags))
            .unwrap_or_else(|| panic!("front-end failed:\n{}", diags.render_all(&map, false)));
        bring_up_runtime();
        let jit = Jit::new().expect("jit");
        let prog = jit.compile(&module).expect("jit compile");
        unsafe {
            prog.run();
        }
        module
    }

    fn trivial_module() -> IrModule {
        let mut module = IrModule::default();
        module.funcs.push(IrFunc {
            name: "main".to_string(),
            symbol: "pickle_main".to_string(),
            params: Vec::new(),
            ret: IrTy::Unit,
            slots: Vec::new(),
            entry: BlockId(0),
            blocks: vec![IrBlock {
                id: BlockId(0),
                instrs: Vec::new(),
                term: IrTerm::Return { v: None },
            }],
            is_main: true,
            is_test: false,
            tags: Vec::new(),
        });
        module
    }

    #[test]
    fn compile_and_run_trivial_main() {
        let jit = Jit::new().expect("jit");
        let module = trivial_module();
        let prog = jit.compile(&module).expect("compile");
        // Empty body: only a `return`; safe without runtime bring-up.
        unsafe { prog.run() };
    }

    /// Exercises the whole shadow-frame path: a string temp is constructed via
    /// `pickle_str_from_bytes` (write-through), printed, and the frame is
    /// popped on return.
    #[test]
    fn run_with_shadow_frame_and_string() {
        let jit = Jit::new().expect("jit");
        let mut module = IrModule::default();
        module.strings.push(b"hello".to_vec());
        module.externs.push(IrExtern {
            symbol: "pickle_print_obj".to_string(),
            params: vec![IrTy::Ptr],
            ret: IrTy::Unit,
        });
        module.externs.push(IrExtern {
            symbol: "pickle_print_newline".to_string(),
            params: Vec::new(),
            ret: IrTy::Unit,
        });
        module.funcs.push(IrFunc {
            name: "main".to_string(),
            symbol: "pickle_main".to_string(),
            params: Vec::new(),
            ret: IrTy::Unit,
            slots: Vec::new(),
            entry: BlockId(0),
            blocks: vec![IrBlock {
                id: BlockId(0),
                instrs: vec![
                    IrInstr::Const {
                        dst: Temp(0),
                        c: IrConst::Str(StrId(0)),
                    },
                    IrInstr::Call {
                        dst: None,
                        callee: Callee::Extern(ExternId(0)),
                        args: vec![Temp(0)],
                    },
                    IrInstr::Call {
                        dst: None,
                        callee: Callee::Extern(ExternId(1)),
                        args: Vec::new(),
                    },
                ],
                term: IrTerm::Return { v: None },
            }],
            is_main: true,
            is_test: false,
            tags: Vec::new(),
        });

        // The collectable runner does its own bring-up like the binary.
        bring_up_runtime();
        let prog = jit.compile(&module).expect("compile");
        unsafe { prog.run() };
    }

    /// End-to-end: a program using zero-capture lambdas, capturing closures,
    /// returned closures, and a module function used as a value runs correctly
    /// through the whole supply chain (front-end, checker, emitter, JIT,
    /// runtime). Expected prints appear on stdout (canonical output is asserted
    /// manually/at the integration level); here we assert the lowered shape
    /// that makes the closed-over captures and function values dispatch
    /// dynamically.
    #[test]
    fn run_closures_end_to_end() {
        let m = run_source(
            r#"fn base(x: int) -> int {
                x * 10
            }

            fn wrap(f: fn (int) -> int, n: int) -> int {
                return f(n) + 1
            }

            fn main() {
                let same = (x: int) => x
                println(same(21))
                let by = base
                println(by(4))
                let k = 3
                let mul = (n: int) => n * k
                println(mul(6))
                let b = 100
                let add = (n: int) => n + b
                println(wrap(add, 1))
                println(same(21))
            }"#,
        );
        let dump = format!("{m}");
        let callinds = m
            .funcs
            .iter()
            .filter(|f| f.name == "main")
            .flat_map(|f| &f.blocks)
            .flat_map(|b| &b.instrs)
            .filter(|i| matches!(i, IrInstr::CallInd { .. }))
            .count();
        assert_eq!(
            callinds, 4,
            "4 dynamic calls (same, by, mul, add via wrap):\n{dump}"
        );
        let tramps = m
            .funcs
            .iter()
            .filter(|f| f.symbol.starts_with("pkl_tramp_"))
            .count();
        assert_eq!(tramps, 1, "one fn-value trampoline for `base`:\n{dump}");
    }

    /// `op=` every target end-to-end: string plural fields/statics/lists/maps
    /// concatenate (exercising the runtime's null-safe `pickle_str_concat` for
    /// still-null statics), instance/static properties round through their
    /// getters and setters, and a scalar raw-pointer deref read-modify-writes.
    #[test]
    fn run_compound_assignment_across_targets() {
        run_source(
            r#"class Bag {
                var name: string
                static var counter: int
                static var label: string
                property doubled: int {
                    get => Bag.counter * 2
                    set { Bag.counter = value / 2 }
                }
                static property dp: int {
                    get => Bag.counter * 2
                    set { Bag.counter = value / 2 }
                }
                constructor() {
                    this.name = "a"
                }
            }

            fn main() {
                var b = Bag()
                b.name += "b"
                println(b.name)
                Bag.counter += 1
                Bag.label += "s"
                println(Bag.label)
                b.doubled += 4
                println(Bag.counter)
                Bag.dp += 2
                println(Bag.counter)
                var xs: List<string> = ["p"]
                xs[0] += "q"
                println(xs[0])
                var m: Map<string, string> = { "k": "v" }
                m["k"] += "w"
                m["u"] += "v"
                println(m["k"])
                println(m["u"])
                unsafe {
                    var n = 1
                    let p: *int = &n
                    (*p) += 5
                    println(n)
                }
            }"#,
        );
    }

    #[test]
    fn run_let_destructuring_and_for_in_tuples() {
        run_source(
            r#"fn main() {
                var t = (1, 2)
                let (a, b) = t
                println(a + b)
                var (x, y) = (3, 4)
                println(x + y)
                let (l, (inner, _)) = (5, (6, 7))
                println(l + inner)
                for ((i, j) in [(10, 20), (30, 40)]) {
                    println(i * j)
                }
                var pairs: List<(int, int)> = [(7, 3), (5, 2)]
                for ((p, q) in pairs) {
                    println(p - q)
                }
            }"#,
        );
    }

    /// A range used as a *value*: `var r = 2..6` materializes a `List<int>` via
    /// `pickle_range`, so `len(r)`, `r[i]`, and `for (x in r)` all work on the
    /// stored range (`..=` is end-inclusive).
    #[test]
    fn run_range_as_value() {
        run_source(
            r#"fn main() {
                var r = 2..6
                println(len(r))
                println(r[1])
                println(r[len(r) - 1])
                var t = 0
                for (x in r) {
                    t = t + x
                }
                println(t)
                var ri = 2..=5
                println(len(ri))
                println(ri[0])
                println(ri[len(ri) - 1])
            }"#,
        );
    }

    /// `&T` beyond parameters: references can be stored in `let`/`var` bindings,
    /// returned from functions, kept in `List<&T>`, and dereferenced — scalar
    /// referents by address, managed referents by identity.
    #[test]
    fn run_ref_values_across_positions() {
        run_source(
            r#"class Point {
                x: int = 0
                constructor(x: int) {
                    this.x = x
                }
            }

            fn fwd(p: &int) -> &int {
                return p
            }

            fn sum2(a: &int, b: &int) -> int {
                return *a + *b
            }

            fn collect(p: &int) -> int {
                var refs: List<&int> = [p]
                var total = 0
                for (r in refs) {
                    total = total + *r
                }
                return total
            }

            fn main() {
                var t = 7
                var s = 13
                let r1: &int = fwd(t)
                var q: &int = r1
                println(*r1)
                println(sum2(t, s))
                println(sum2(r1, q))
                println(collect(t))
                var o = Point(5)
                let pc: &Point = o
                var objs: List<&Point> = [pc]
                for (pp in objs) {
                    println(pp.x)
                }
                println(objs[0].x)
            }"#,
        );
    }

    /// Spread arguments (`f(...xs)`) across every static call target: a plain
    /// function (whole-list and prefix + spread), a struct-like constructor, a
    /// static and instance method (including a chained `Ctor(...xs).m(...ys)`
    /// receiver), a generic function, and variadic `println`/`print` where the
    /// spread unrolls element-by-element. The runtime arity guard is exercised
    /// only implicitly here (a separate panic test covers the mismatch path).
    #[test]
    fn run_spread_arguments() {
        run_source(
            r#"class Point {
                x: int = 0
                y: int = 0
                constructor(x: int, y: int) {
                    this.x = x
                    this.y = y
                }
                fn sum() -> int {
                    return this.x + this.y
                }
            }

            class Counter {
                var x: int
                var y: int
                constructor(x: int, y: int) {
                    this.x = x
                    this.y = y
                }
                static fn add(a: int, b: int) -> int {
                    return a + b
                }
                fn bump(dx: int, dy: int) -> int {
                    return this.x + this.y + dx + dy
                }
            }

            fn add3(a: int, b: int, c: int) -> int {
                return a + b + c
            }

            fn generic3<T>(a: T, b: T, c: T) -> T {
                return b
            }

            fn main() {
                let th: List<int> = [10, 20, 30]
                let tw: List<int> = [40, 50]
                let ys: List<string> = ["a", "b"]
                println(add3(...th))
                println(add3(th[0], ...tw))
                let p = Point(...tw)
                println(p.x, p.y, p.sum())
                println(Counter.add(...tw))
                println(generic3<int>(...th))
                println("spread-print:", ...th)
                print("inline", 9, 7, "\n", ...tw)
                println("strings:", ...ys)
                println(Counter(...tw).bump(...tw))
                println("done")
            }"#,
        );
    }
}
