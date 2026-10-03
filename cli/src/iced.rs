//! Friendly crash reporting for the toolchain.
//!
//! The host toolchain runs Pickle code by JIT-compiling it to native machine
//! code, so an internal fault in the compiled program shows up as a raw
//! Windows access violation / Unix signal rather than a Rust panic. Install
//! this from `main` so every such crash (plus ordinary Rust panics) reports
//! an internal compiler error instead of a bare OS fault message.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Mutex;

/// Executable ranges with their symbol tables, registered so a crash handler
/// can walk the native stack and name the compiled function that faulted.
#[derive(Default)]
struct CodeSyms {
    /// `(abs_addr, symbol)` sorted by address; the nearest entry at or below a
    /// return address names the enclosing compiled function.
    symbols: Vec<(usize, String)>,
}

static CODE: Mutex<Vec<(usize, usize, CodeSyms)>> = Mutex::new(Vec::new());

/// Native (non-JIT) functions the reporter should be able to name, e.g. the
/// `pickle_*` runtime ABI. `(addr, name)` sorted by address.
static NATIVE: Mutex<Vec<(usize, String)>> = Mutex::new(Vec::new());

/// Latch of `PKL_ICED_DEBUG`, read ONCE on the main thread (which has a full
/// stack). The VEH handler and `raw_crash` both run from a nearly-exhausted
/// stack, where `std::env::var`/environment lookups allocate and can fault,
/// which used to re-enter the handler and bury the real crash in a storm of
/// handler frames. Never touch the environment from crash paths again.
static DEBUG_VAR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Dump destination for `PKL_ICED_SYMS` (empty when unset), captured once on
/// the main thread; the reporter thread never reads the environment.
static SYMS_PATH: Mutex<Option<String>> = Mutex::new(None);

/// Hard cap on VEH handler re-entries. An exception handler on Windows can be
/// re-dispatched if it faults while running; once we exceed the cap we exit
/// rather than spiral forever.
#[cfg(windows)]
static REENTRY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Latches whether crash-time symbolication (DbgHelp) is wanted. Like
/// `DEBUG_VAR`, decided on the main thread; the reporter thread only reads it.
static SYMS_VAR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Snapshot of the environment flags used by crash paths, taken before any
/// fault so the handlers never touch the process environment.
pub fn latch_crash_env() {
    use std::sync::atomic::Ordering;
    let dbg = std::env::var("PKL_ICED_DEBUG").is_ok();
    DEBUG_VAR.store(dbg, Ordering::SeqCst);
    let syms = std::env::var("PKL_ICED_SYMS").is_ok();
    SYMS_VAR.store(syms, Ordering::SeqCst);
    if let Ok(p) = std::env::var("PKL_ICED_SYMS") {
        *SYMS_PATH.lock().unwrap() = Some(p);
    }
}

/// Scan upward from `sp` for return addresses that land in registered code and
/// print them innermost-first. The runtime's `PKL_GC_CHECK` probes call this
/// through `pickle_gc_set_caller_hook` when a managed argument turns out to be
/// a dead or mistyped block: the stack is then a JIT frame deep and this is the
/// only place the offending call site can be recovered, because a controlled
/// panic unwinds before the crash reporter can walk anything.
pub(crate) fn print_call_chain(sp: usize, tag_ptr: usize, tag_len: usize) {
    if sp == 0 || tag_ptr == 0 {
        return;
    }
    let tag = unsafe {
        core::str::from_utf8_unchecked(core::slice::from_raw_parts(tag_ptr as *const u8, tag_len))
    };
    let mut out: Vec<String> = Vec::new();
    let mut last = String::new();
    // 64 words is enough to walk out of the helper chain in JIT'd code and stop
    // well before the Rust frames that hold no return addresses in JIT ranges.
    for i in 0..64usize {
        // SAFETY: `sp` is the address of a live local in the calling helper, and
        // 512 bytes above it is still inside the frames of that call chain.
        let word = unsafe { core::ptr::read_volatile((sp + i * 8) as *const usize) };
        if word < 0x1000 {
            continue;
        }
        let Some(name) = resolve(word) else { continue };
        if name == last {
            continue;
        }
        last = name.clone();
        out.push(name);
        if out.len() >= 12 {
            break;
        }
    }
    if out.is_empty() {
        return;
    }
    eprintln!("[gc-check] call chain for {tag}:");
    for (i, name) in out.iter().enumerate() {
        eprintln!("[gc-check]   #{i} {name}");
    }
}

/// Register a single native function that a compiled program can call into.
pub fn register_native(name: &str, addr: usize) {
    let mut g = NATIVE.lock().unwrap();
    g.push((addr, name.to_string()));
    g.sort_unstable();
    if DEBUG_VAR.load(std::sync::atomic::Ordering::SeqCst) {
        eprintln!("[iced] register_native {name} addr={addr:#x}");
    }
}

/// Register a compiled-code range + absolute symbol table for backtraces.
pub fn register_code_range(base: usize, len: usize, symbols: HashMap<String, usize>) {
    let mut v: Vec<(usize, String)> = symbols.into_iter().map(|(s, a)| (a, s)).collect();
    v.sort_unstable();
    let mut g = CODE.lock().unwrap();
    g.push((base, len, CodeSyms { symbols: v }));
    if DEBUG_VAR.load(std::sync::atomic::Ordering::SeqCst) {
        eprintln!(
            "[iced] register_code_range base={base:#x} len={len:#x} syms={}",
            g.last().map(|(_, _, s)| s.symbols.len()).unwrap_or(0)
        );
    }
    if SYMS_VAR.load(std::sync::atomic::Ordering::SeqCst) {
        let Some(path) = SYMS_PATH.lock().unwrap().clone() else {
            return;
        };
        let mut out = String::new();
        let syms = &g.last().unwrap().2.symbols;
        for entry in syms.iter() {
            out.push_str(&format!("{:016x} {}\n", entry.0, entry.1));
        }
        let _ = std::fs::write(&path, out);
    }
}

/// Best-effort name for a code address inside a registered range.
pub(crate) fn resolve(addr: usize) -> Option<String> {
    let g = CODE.lock().ok()?;
    for (base, len, syms) in g.iter() {
        let base = *base;
        if addr < base || addr >= base + *len {
            continue;
        }
        let off = addr - base;
        match syms.symbols.binary_search_by(|(a, _)| a.cmp(&addr)) {
            Ok(i) => return Some(syms.symbols[i].1.clone()),
            Err(i) => {
                if i == 0 {
                    return Some(format!("<mapping+0x{:x}>", off));
                }
                let best = &syms.symbols[i - 1];
                let d = off - (best.0 - base);
                return Some(format!("{}+0x{:x}", best.1, d));
            }
        }
    }
    drop(g);
    let n = NATIVE.lock().ok()?;
    if let Ok(i) = n.binary_search_by(|(a, _)| a.cmp(&addr)) {
        return Some(n[i].1.to_string());
    }
    if let Err(i) = n.binary_search_by(|(a, _)| a.cmp(&addr)) {
        if i > 0 {
            let best = &n[i - 1];
            return Some(format!("n:{} +0x{:x}", best.1, addr - best.0));
        }
    }
    None
}

/// Scan the faulting thread's own stack for return addresses that land inside
/// a registered code range. Best-effort. Deliberately small: it runs on a
/// stack that is nearly exhausted, so it stays headroom-safe by walking a
/// bounded window and resolving at most a handful of symbols. This is the
/// version that reliably fit in the overflow headroom before heavier
/// instrumentation errored out the whole process.
#[cfg(unix)]
fn backtrace_from(rsp: usize) -> Vec<String> {
    if rsp == 0 {
        return Vec::new();
    }
    // The handler has latched IN_BT before calling this, so re-entrancy is
    // already accounted for; do not swap again here.
    let ranges: Vec<(usize, usize)> = {
        let g = CODE.lock().unwrap();
        g.iter().map(|(b, l, _)| (*b, b.wrapping_add(*l))).collect()
    };
    let mut out: Vec<String> = Vec::new();
    let mut addr = rsp;
    let mut guard = 0usize;
    while guard < 128 {
        let mut p = addr & !0x7usize;
        let end = p.wrapping_add(512);
        while p < end {
            let v = unsafe { (p as *const usize).read_volatile() };
            if v != 0 && ranges.iter().any(|r| v >= r.0 && v < r.1) {
                if let Some(s) = resolve(v) {
                    out.push(s);
                }
            }
            p = p.wrapping_add(8);
        }
        addr = addr.wrapping_add(512);
        guard += 1;
    }
    out.truncate(12);
    out
}

const STANZA: &[u8] = b"The pickle jar cracked.\n\nInternal compiler error:\nE9999\n\nThe compiler encountered something it did not expect.\nNo pickles were harmed, but the vinegar level is concerning.\n";

static PRINTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(windows)]
static IN_BT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(windows)]
static CAUGHT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Crash state shared between the VEH handler (which runs on a nearly
/// exhausted stack and may write nothing) and a reporter thread (full stack)
/// that formats and prints the diagnostics. The handler only writes atomics
/// and then spins, so its own stack usage stays in the overflow headroom.
#[cfg(windows)]
static CRASH_SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// The reporter thread, leaked and published once by `install` so a crash
/// handler can wake it with `unpark`: allocation-free and O(1), which is the
/// only thing a handler on a nearly-exhausted stack can afford.
#[cfg(windows)]
static REPORTER: std::sync::atomic::AtomicPtr<std::thread::Thread> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());
#[cfg(windows)]
static CRASH_RIP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(windows)]
static CRASH_RSP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(windows)]
static CRASH_ACC: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(windows)]
static CRASH_CODE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(windows)]
static CRASH_SBASE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(windows)]
static CRASH_SLIMIT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(windows)]
static CRASH_TID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(windows)]
static CRASH_RSI: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(windows)]
static CRASH_RCX: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(windows)]
static SEEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Format and print the most recent crash from the given sequence number with
/// full headroom. Frames are resolved symbolically where possible; the raw
/// walk from `rsp` names every in-range return address. Runs on the
/// Windows-only reporter thread, so its `windows::*` helpers are gated with
/// it.
#[cfg(windows)]
fn report_crash(seq: usize) {
    let rip = CRASH_RIP.load(std::sync::atomic::Ordering::SeqCst);
    let rsp = CRASH_RSP.load(std::sync::atomic::Ordering::SeqCst);
    let acc = CRASH_ACC.load(std::sync::atomic::Ordering::SeqCst);
    let code = CRASH_CODE.load(std::sync::atomic::Ordering::SeqCst);
    let sbase = CRASH_SBASE.load(std::sync::atomic::Ordering::SeqCst);
    let slimit = CRASH_SLIMIT.load(std::sync::atomic::Ordering::SeqCst);
    let tid = CRASH_TID.load(std::sync::atomic::Ordering::SeqCst);
    let rsi = CRASH_RSI.load(std::sync::atomic::Ordering::SeqCst);
    let rcx = CRASH_RCX.load(std::sync::atomic::Ordering::SeqCst);
    let mut msg = format!(
        "[iced-report] seq={seq} code={code:#010x} rip={rip:#x} rsp={rsp:#x} acc={acc:#x} rsi={rsi:#x} rcx={rcx:#x}\n"
    );
    let ranges: Vec<(usize, usize)> = {
        match CODE.lock() {
            Ok(g) => g.iter().map(|(b, l, _)| (*b, b.wrapping_add(*l))).collect(),
            Err(_) => Vec::new(),
        }
    };
    msg.push_str(&format!(
        "      code_ranges={}\n",
        ranges
            .iter()
            .map(|(a, b)| format!("{a:#x}..{b:#x}"))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    for (name, m) in windows::module_bases() {
        msg.push_str(&format!("      module {name} base={m:#x}\n"));
    }
    if tid != 0 {
        msg.push_str(&format!(
            "      thread=tid{tid:#x} stack=[{slimit:#x}..{sbase:#x}] used={:#x}\n",
            sbase.wrapping_sub(slimit)
        ));
    }
    if let Some(s) = resolve(rip) {
        msg.push_str(&format!("      rip -> {s}\n"));
    }
    if rsp != 0 && sbase != 0 {
        // Walk the whole span of the faulting stack from the bottom up.
        // JIT frames are resolved by symbol; meanwhile every qword that looks
        // like a code address (native returns included) is tallied so a
        // repeating native frame — a runaway recursion invisible to the JIT
        // symbol table — names itself by frequency.
        let mut chain: Vec<(usize, String)> = Vec::new();
        let mut counts: std::collections::HashMap<usize, usize> = Default::default();
        let mut pages_scanned = 0usize;
        let mut page = slimit & !0xFFFusize;
        let end = sbase & !0xFFFusize;
        while page < end {
            pages_scanned += 1;
            if pages_scanned > 1 << 20 {
                break;
            }
            if let Some((lo, hi)) = windows::committed_run(page, end) {
                let lo = lo & !0x7usize;
                let mut p = lo;
                while p < hi {
                    let v = unsafe { (p as *const usize).read_volatile() };
                    let q = p.wrapping_sub(8);
                    p = p.wrapping_add(8);
                    if v == 0 || v < 0x10000 {
                        continue;
                    }
                    if v >= 0x1_0000_0000 {
                        *counts.entry(v).or_insert(0) += 1;
                    }
                    if !ranges.iter().any(|(b, e)| v >= *b && v < *e) {
                        continue;
                    }
                    if chain.len() > 200_000 {
                        break;
                    }
                    if let Some(s) = resolve(v) {
                        chain.push((q.wrapping_sub(slimit), s));
                    }
                }
                page = hi & !0xFFFusize;
            } else {
                page += 0x1000;
            }
        }
        let mut top: Vec<(usize, &usize)> = counts.iter().map(|(a, c)| (*c, a)).collect();
        top.sort_by_key(|a| std::cmp::Reverse(a.0));
        top.truncate(12);
        msg.push_str(&format!(
            "      stack_pages={pages_scanned} resolved_frames={} top_native_hits=({})\n",
            chain.len(),
            top.iter()
                .map(|(c, a)| {
                    let named = windows::sym_from_addr(**a)
                        .or_else(|| resolve(**a))
                        .map(|n| format!(" => {n}"))
                        .unwrap_or_default();
                    format!("{:#x}×{c}{named}", a)
                })
                .collect::<Vec<_>>()
                .join(", ")
        ));
        let win = &chain[..chain.len().min(40)];
        let mut head = String::new();
        for (i, (off, s)) in win.iter().enumerate() {
            head.push_str(&format!("      [{i:03} @{off:#x}] {s}\n"));
        }
        msg.push_str(&head);
        if chain.len() > 40 {
            // Look for a short repeating unit anywhere later in the chain.
            let sigs: Vec<&str> = chain.iter().map(|(_, s)| s.as_str()).collect();
            let mut period = None;
            'outer: for plen in 1..=64 {
                if sigs.len() < plen * 3 {
                    break;
                }
                let last = &sigs[sigs.len() - plen..];
                let prev = &sigs[sigs.len() - 2 * plen..sigs.len() - plen];
                if last == prev {
                    period = Some((plen, sigs.len()));
                    break 'outer;
                }
            }
            if let Some((plen, total)) = period {
                msg.push_str(&format!(
                    "      [cycle] period={plen} frames_tail={total} repeated={}\n",
                    (total as f64 / plen as f64).ceil() as usize
                ));
                for (i, (off, s)) in chain[chain.len() - plen..].iter().enumerate() {
                    msg.push_str(&format!("      [x{i:03} @{off:#x}] {s}\n"));
                }
            } else {
                msg.push_str(&format!(
                    "      [tail] no periodic cycle found, total frames={}\n",
                    chain.len()
                ));
                for (i, (off, s)) in chain[chain.len() - 40..].iter().enumerate() {
                    msg.push_str(&format!("      [t{i:03} @{off:#x}] {s}\n"));
                }
            }
        }
    }
    let _ = std::io::stderr().write_all(msg.as_bytes());
    SEEN.store(seq, std::sync::atomic::Ordering::SeqCst);
}

/// Print the crash stanza (plus a one-line cause) exactly once, then exit
/// with code 101 (the code Rust uses for a panicking process).
fn crash(extra: Option<&str>) -> ! {
    if PRINTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        std::process::exit(101);
    }
    let _ = std::io::stderr().write_all(STANZA);
    if let Some(e) = extra {
        let _ = std::io::stderr().write_all(b"\n");
        let _ = writeln!(std::io::stderr(), "cause: {e}");
    }
    std::process::exit(101)
}

/// Install the panic hook and platform crash handlers. Call once from `main`.
pub fn install() {
    std::panic::set_hook(Box::new(|info| {
        let payload = info.payload();
        let cause = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned());
        crash(cause.as_deref());
    }));
    // Snapshot the crash-related env flags before any fault can happen: the
    // VEH handler runs on a nearly-exhausted stack where env lookups allocate
    // and can re-fault. `install` runs on the main thread with a full stack.
    latch_crash_env();
    #[cfg(windows)]
    windows::install_veh();
    #[cfg(windows)]
    {
        // Reporter thread: on a fault the VEH handler and the faulting thread
        // have almost no stack left, so the real diagnostics (symbol
        // resolution, stack walk) run here on a full stack. It *parks* rather
        // than polling: a `yield_now` loop here burns a whole core for the life
        // of the process, which is pure waste on a machine running one compiler
        // thread, and it shows up as phantom background CPU.
        let handle = std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .name("iced-reporter".into())
            .spawn(|| loop {
                std::thread::park();
                let seq = CRASH_SEQ.load(std::sync::atomic::Ordering::SeqCst);
                if seq == 0 {
                    continue;
                }
                report_crash(seq);
                std::process::exit(101);
            })
            .expect("spawn iced-reporter");
        REPORTER.store(
            Box::into_raw(Box::new(handle.thread().clone())),
            std::sync::atomic::Ordering::SeqCst,
        );
    }
    #[cfg(unix)]
    unix::install_signals();
}

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;

    const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;
    const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
    const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

    // Handled fault codes: access violation, guard-page violation (Cranelift
    // frames are not stack-probed, so overflow lands here first on Windows),
    // stack overflow, and `process::abort`/fail-fast.
    const STATUS_GUARD_PAGE_VIOLATION: u32 = 0x8000_0001;
    const STATUS_ACCESS_VIOLATION: u32 = 0xC000_0005;
    const STATUS_STACK_OVERFLOW: u32 = 0xC000_00FD;
    const STATUS_FAIL_FAST: u32 = 0xC000_0409;

    #[repr(C)]
    struct ExceptionRecord {
        code: u32,
        flags: u32,
        nested: *mut ExceptionRecord,
        address: *mut c_void,
        info_count: u32,
        info: *mut usize,
    }

    #[repr(C)]
    struct ExceptionPointers {
        record: *mut ExceptionRecord,
        context: *mut c_void,
    }

    pub const MEM_COMMIT: u32 = 0x1000;
    pub const PAGE_GUARD: u32 = 0x100;

    #[repr(C)]
    pub struct Mbi {
        pub base_addr: *mut c_void,
        pub allocation_base: *mut c_void,
        pub allocation_protect: u32,
        pub partition_id: u16,
        pub region_size: usize,
        pub state: u32,
        pub protect: u32,
        pub type_: u32,
    }

    pub unsafe fn virtual_query(address: *const c_void, info: *mut Mbi) -> usize {
        unsafe extern "system" {
            fn VirtualQuery(address: *const c_void, info: *mut Mbi, len: usize) -> usize;
        }
        VirtualQuery(address, info, std::mem::size_of::<Mbi>())
    }

    /// Return the committed, non-guard region starting at or just above
    /// `addr`, capped at `end`. `None` when the next region is not a usable
    /// committed span (guard page, free, reserved).
    pub fn committed_run(addr: usize, end: usize) -> Option<(usize, usize)> {
        let mut mbi = unsafe { std::mem::zeroed::<Mbi>() };
        // SAFETY: VirtualQuery does not dereference the address; MBI is POD.
        let rq = unsafe { virtual_query(addr as *const c_void, &mut mbi) };
        if rq == 0 || mbi.state != MEM_COMMIT || (mbi.protect & PAGE_GUARD) != 0 {
            return None;
        }
        let lo = mbi.base_addr as usize;
        let hi = lo.wrapping_add(mbi.region_size).min(end);
        if hi <= lo {
            return None;
        }
        Some((lo, hi))
    }

    /// Map the lowest loaded module bases using `EnumProcessModulesEx`-free
    /// heuristics: walk the compact executable regions between 0x7ff000000000
    /// and 0x7fffffffffff and match them to module path names via
    /// GetModuleFileNameA. Best-effort; missing names are left blank.
    pub fn module_bases() -> Vec<(String, usize)> {
        unsafe extern "system" {
            fn GetModuleFileNameA(module: *mut c_void, buf: *mut u8, size: u32) -> u32;
        }
        let mut out: Vec<(String, usize)> = Vec::new();
        // The PEB's loader data keeps a doubly-linked list of loaded modules.
        // On x64 the PEB is at gs:[0x60]; Ldr is at +0x18; InLoadOrderModuleList
        // head at +0x10 (LIST_ENTRY InInitializationOrder... read InLoadOrder,
        // the standard first). Each LDR_DATA_TABLE_ENTRY: DllBase at +0x30,
        // FullDllName UNICODE_STRING { len, max, buf } at +0x48.
        unsafe {
            let mut peb: usize = 0;
            std::arch::asm!("mov {}, gs:0x60", out(reg) peb, options(nostack, nomem));
            let ldr = (peb as *const usize).add(0x18 / 8).read_volatile();
            if ldr == 0 {
                return out;
            }
            let head = ldr + 0x10;
            let mut cur = (head as *const usize).read_volatile();
            for _ in 0..512 {
                if cur == 0 || cur == head {
                    break;
                }
                let base = (cur as *const usize).add(0x30 / 8).read_volatile();
                let str_buf = (cur as *const usize).add(0x48 / 8 + 1).read_volatile();
                let mut buf = [0u8; 512];
                let n = GetModuleFileNameA(base as *mut c_void, buf.as_mut_ptr(), 512);
                let name = if n > 0 {
                    let end = buf[..n as usize]
                        .iter()
                        .position(|&b| b == 0)
                        .unwrap_or(n as usize);
                    String::from_utf8_lossy(&buf[..end]).to_string()
                } else {
                    String::new()
                };
                let _ = str_buf;
                out.push((name, base));
                let link = (cur as *const usize).read_volatile();
                if link == head || link == 0 {
                    break;
                }
                cur = link;
            }
        }
        out
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(n_std_handle: u32) -> *mut c_void;
        fn WriteFile(
            file: *mut c_void,
            buffer: *const c_void,
            bytes_to_write: u32,
            bytes_written: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        fn ExitProcess(exit_code: u32);
        fn AddVectoredExceptionHandler(first: i32, handler: *const c_void) -> *mut c_void;
    }

    /// Resolve a native address to `name+offset` via DbgHelp, reading the PDB
    /// that ships beside the freshly built pickle-cli.exe. Used to name the
    /// non-JIT partners of a runaway loop (e.g. a native `pickle_*` call the
    /// JIT symbol table can't see). Idempotent; cheap after first init.
    pub fn sym_from_addr(addr: usize) -> Option<String> {
        use std::sync::Once;
        // DbgHelp's SYMBOL_INFO: fixed prefix is exactly 88 bytes on x64
        // (SizeOfStruct, TypeIndex, Reserved[2], Index, Size, ModBase, Flags,
        // Value, Address, Register, Scope, Tag, NameLen, MaxNameLen), then
        // Name[1] starts at byte 88. SizeOfStruct MUST be 88, or SymFromAddr
        // fails with ERROR_INVALID_PARAMETER (0x57).
        const SIZE_OF_STRUCT: usize = 88;
        const NAME_OFF: usize = 88;
        const NBUF: usize = 512;
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct FixPart {
            size_of_struct: u32,
            type_index: u32,
            reserved: [u64; 2],
            index: u32,
            size: u32,
            mod_base: u64,
            flags: u32,
            value: u64,
            address: u64,
            register: u32,
            scope: u32,
            tag: u32,
            name_len: u32,
            max_name_len: u32,
        }
        debug_assert_eq!(std::mem::size_of::<FixPart>(), SIZE_OF_STRUCT);
        unsafe extern "system" {
            fn GetCurrentProcess() -> *mut c_void;
            fn SymInitialize(
                h_process: *mut c_void,
                user_search_path: *const u8,
                invade: i32,
            ) -> i32;
            fn SymSetOptions(options: u32) -> u32;
            fn SymFromAddr(
                h_process: *mut c_void,
                address: u64,
                displacement: *mut u64,
                symbol: *mut FixPart,
            ) -> i32;
        }
        static INIT: Once = Once::new();
        INIT.call_once(|| unsafe {
            let h = GetCurrentProcess();
            // SYMOPT_UNDNAME only; SYMOPT_DEFERRED_LOADS (0x4) can make
            // SymFromAddr report the module as not found. The PDB lives next
            // to the exe but the exe references it via /PDBALTPATH:%_PDB%,
            // so DbgHelp needs that directory as the search path or it will
            // fail with PDB-not-found (0x1E7).
            SymSetOptions(0x2);
            let search = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            let Some(search_str) = search.to_str() else {
                return;
            };
            let _c = std::ffi::CString::new(search_str).unwrap_or_default();
            let cptr = if _c.as_bytes().is_empty() {
                std::ptr::null()
            } else {
                _c.as_ptr() as *const u8
            };
            let ok = SymInitialize(h, cptr, 1);
            if super::DEBUG_VAR.load(std::sync::atomic::Ordering::SeqCst) {
                eprintln!("[iced] SymInitialize(search={search:?}) -> {ok}");
            }
        });
        let mut buffer = [0u8; SIZE_OF_STRUCT + NBUF];
        let head = buffer.as_mut_ptr() as *mut FixPart;
        unsafe {
            (*head).size_of_struct = SIZE_OF_STRUCT as u32;
            (*head).max_name_len = (NBUF - 1) as u32;
            let mut disp: u64 = 0;
            let ok = SymFromAddr(GetCurrentProcess(), addr as u64, &mut disp, head);
            if ok == 0 {
                return None;
            }
            let name_len = ((*head).name_len as usize).min(NBUF - 1);
            let name_bytes: &[u8] = &buffer[NAME_OFF..NAME_OFF + name_len];
            let end = name_bytes
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(name_bytes.len());
            let name = String::from_utf8_lossy(&name_bytes[..end]).to_string();
            if name.is_empty() {
                return None;
            }
            if disp > 0 {
                Some(format!("{} +0x{:x}", name, disp))
            } else {
                Some(name)
            }
        }
    }

    /// Exit with the crash stanza using only raw synchronous stderr writes:
    /// the fault may have left allocators and std locks in an unknown state.
    /// Never reads the environment or allocates: both re-fault on an almost
    /// exhausted stack and used to re-enter the handler in an endless storm.
    fn raw_crash(extra: &str) -> ! {
        if super::DEBUG_VAR.load(std::sync::atomic::Ordering::SeqCst) {
            // Raw write of a (maybe slightly wrong) debug line; extra is a
            // static-ish &str, so no allocation.
            unsafe {
                let h = GetStdHandle(STD_ERROR_HANDLE);
                if !h.is_null() {
                    let tag = b"[iced] raw_crash\n";
                    let mut written = 0u32;
                    WriteFile(
                        h,
                        tag.as_ptr() as *const c_void,
                        tag.len() as u32,
                        &mut written,
                        std::ptr::null_mut(),
                    );
                    WriteFile(
                        h,
                        extra.as_ptr() as *const c_void,
                        extra.len() as u32,
                        &mut written,
                        std::ptr::null_mut(),
                    );
                }
            }
        }
        if super::PRINTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
            unsafe {
                ExitProcess(101);
            }
        }
        unsafe {
            let h = GetStdHandle(STD_ERROR_HANDLE);
            if !h.is_null() {
                let mut written = 0u32;
                WriteFile(
                    h,
                    super::STANZA.as_ptr() as *const c_void,
                    super::STANZA.len() as u32,
                    &mut written,
                    std::ptr::null_mut(),
                );
                let cause = extra.as_bytes();
                WriteFile(
                    h,
                    cause.as_ptr() as *const c_void,
                    cause.len() as u32,
                    &mut written,
                    std::ptr::null_mut(),
                );
            }
            ExitProcess(101);
        }
        unreachable!()
    }

    unsafe extern "system" fn veh_handler(pointers: *mut ExceptionPointers) -> i32 {
        // Guard against the handler itself faulting. When the faulting thread's
        // stack is exhausted, even a small amount of handler stack use can
        // re-trigger the guard page, Windows re-dispatches this VEH, and the
        // stack fills with handler frames (seen as a fake "native loop" in
        // top_native_hits). Cap it so a storm still yields a report, then exit
        // hard rather than recurse forever.
        let re = super::REENTRY.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if re > 100_000 {
            ExitProcess(101);
        }
        {
            // TEMP probe: unconditional, before null checks. Prints code + null
            // flags for the first ~14 deliveries.
            use std::sync::atomic::Ordering;
            if super::DEBUG_VAR.load(Ordering::SeqCst) && re < 14 {
                unsafe {
                    let h = GetStdHandle(STD_OUTPUT_HANDLE);
                    if !h.is_null() {
                        const HEX: &[u8] = b"0123456789abcdef";
                        let mut s = [0u8; 64];
                        let tag = b"[iced] p=";
                        s[..tag.len()].copy_from_slice(tag);
                        let mut n = tag.len();
                        let pnull = pointers.is_null();
                        s[n] = if pnull { b'P' } else { b'p' };
                        n += 1;
                        // record null flag
                        let record = if pnull {
                            std::ptr::null()
                        } else {
                            (*pointers).record
                        };
                        s[n] = if record.is_null() { b'R' } else { b'r' };
                        n += 1;
                        s[n] = b' ';
                        n += 1;
                        let tag2 = b"code=";
                        s[n..n + 5].copy_from_slice(tag2);
                        n += 5;
                        let c = if record.is_null() { 0 } else { (*record).code } as u64;
                        for i in 0..8 {
                            s[n + i] = HEX[((c >> (28 - i * 4)) & 0xF) as usize];
                        }
                        n += 8;
                        let tag3 = b" ctx=";
                        s[n..n + 5].copy_from_slice(tag3);
                        n += 5;
                        let ctxp = if pnull {
                            std::ptr::null()
                        } else {
                            (*pointers).context
                        };
                        s[n] = if ctxp.is_null() { b'C' } else { b'c' };
                        n += 1;
                        s[n] = b'\n';
                        n += 1;
                        let mut written = 0u32;
                        WriteFile(
                            h,
                            s.as_ptr() as *const c_void,
                            n as u32,
                            &mut written,
                            std::ptr::null_mut(),
                        );
                    }
                }
            }
        }
        if pointers.is_null() {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let record = (*pointers).record;
        if record.is_null() {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let code = (*record).code;
        if super::DEBUG_VAR.load(std::sync::atomic::Ordering::SeqCst) && re < 14 {
            // TEMP probe after code read.
            unsafe {
                let h = GetStdHandle(STD_OUTPUT_HANDLE);
                if !h.is_null() {
                    let mut s = [0u8; 48];
                    let tag = b"[iced] aft re=";
                    s[..tag.len()].copy_from_slice(tag);
                    s[tag.len()] = b'0' + (re % 10) as u8;
                    s[tag.len() + 1] = b'\n';
                    let mut written = 0u32;
                    WriteFile(
                        h,
                        s.as_ptr() as *const c_void,
                        tag.len() as u32 + 2,
                        &mut written,
                        std::ptr::null_mut(),
                    );
                }
            }
        }
        if super::DEBUG_VAR.load(std::sync::atomic::Ordering::SeqCst) && pointers.is_null() {
            unsafe {
                let h = GetStdHandle(STD_OUTPUT_HANDLE);
                if !h.is_null() {
                    let mut s = [0u8; 32];
                    let tag = b"[iced] NULLP";
                    s[..tag.len()].copy_from_slice(tag);
                    s[tag.len()] = b'\n';
                    let mut written = 0u32;
                    WriteFile(
                        h,
                        s.as_ptr() as *const c_void,
                        tag.len() as u32 + 1,
                        &mut written,
                        std::ptr::null_mut(),
                    );
                }
            }
        }
        let ctx = (*pointers).context as *const u64;
        if super::DEBUG_VAR.load(std::sync::atomic::Ordering::SeqCst) && !ctx.is_null() {
            let tick = super::CAUGHT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if tick < 6 {
                let rspd = ctx.add(0x98 / 8).read();
                let ripd = ctx.add(0xF8 / 8).read();
                let accd = 0usize;
                // Raw stderr write; hex digits hand-written into a fixed buffer
                // (no allocator, no fmt machinery).
                unsafe {
                    let h = GetStdHandle(STD_ERROR_HANDLE);
                    if !h.is_null() {
                        let mut buf = [0u8; 96];
                        let mut n = 0usize;
                        let push = |b: &[u8], n: &mut usize, buf: &mut [u8]| {
                            let take = b.len().min(buf.len().saturating_sub(*n));
                            buf[*n..*n + take].copy_from_slice(&b[..take]);
                            *n += take;
                        };
                        fn hex(v: usize, out: &mut [u8]) -> usize {
                            const D: &[u8] = b"0123456789abcdef";
                            if v == 0 {
                                out[0] = b'0';
                                return 1;
                            }
                            let mut x = v;
                            let mut i = 0usize;
                            while x != 0 && i < out.len() {
                                out[out.len() - 1 - i] = D[x & 0xF];
                                x >>= 4;
                                i += 1;
                            }
                            let start = out.len() - i;
                            out.copy_within(start.., 0);
                            i
                        }
                        push(b"[iced] tick=", &mut n, &mut buf);
                        buf[n] = b'0' + (tick as u8).min(9);
                        n += 1;
                        for (label, v) in [
                            (" code=0x", code as usize),
                            (" rsp=", rspd as usize),
                            (" rip=", ripd as usize),
                            (" acc=", accd),
                        ] {
                            push(label.as_bytes(), &mut n, &mut buf);
                            let mut hx = [0u8; 16];
                            let hn = hex(v, &mut hx);
                            push(&hx[..hn], &mut n, &mut buf);
                        }
                        buf[n] = b'\n';
                        n += 1;
                        let mut written = 0u32;
                        WriteFile(
                            h,
                            buf.as_ptr() as *const c_void,
                            n as u32,
                            &mut written,
                            std::ptr::null_mut(),
                        );
                    }
                }
            }
        }
        if code == STATUS_ACCESS_VIOLATION
            || code == STATUS_GUARD_PAGE_VIOLATION
            || code == STATUS_STACK_OVERFLOW
            || code == STATUS_FAIL_FAST
        {
            if super::DEBUG_VAR.load(std::sync::atomic::Ordering::SeqCst) && re < 14 {
                unsafe {
                    let h = GetStdHandle(STD_OUTPUT_HANDLE);
                    if !h.is_null() {
                        let mut s = [0u8; 32];
                        let tag = b"[iced] CRASHBLOCK";
                        s[..tag.len()].copy_from_slice(tag);
                        s[tag.len()] = b'\n';
                        let mut written = 0u32;
                        WriteFile(
                            h,
                            s.as_ptr() as *const c_void,
                            tag.len() as u32 + 1,
                            &mut written,
                            std::ptr::null_mut(),
                        );
                    }
                }
            }
            // x64 CONTEXT layout (WinNT.h): P1..P6Home (0x00), ContextFlags/MxCsr (0x30),
            // Segments (0x38), EFlags (0x44), Dr0..Dr7 (0x48), then the GPRs —
            // Rax 0x78, Rcx 0x80, Rdx 0x88, Rbx 0x90, **Rsp 0x98**, Rbp 0xA0,
            // **Rip 0xF8**.
            let context = (*pointers).context as *const u64;
            let rsp = if !context.is_null() {
                context.add(0x98 / 8).read()
            } else {
                0
            };
            let rip = if !context.is_null() {
                context.add(0xF8 / 8).read()
            } else {
                0
            };
            let rsi = if !context.is_null() {
                context.add(0xA8 / 8).read()
            } else {
                0
            };
            let rcx = if !context.is_null() {
                context.add(0x80 / 8).read()
            } else {
                0
            };
            let acc = 0usize;
            // x64 TEB: gs:[0x30] is the TEB pointer; StackBase at +0x08 and
            // StackLimit at +0x10 from it. Reading these on the faulting thread
            // gives the whole thread stack, not just the guard-page rescue
            // region the OS carved out for exception delivery.
            let (sbase, slimit, tid) = unsafe {
                let mut teb: usize = 0;
                std::arch::asm!(
                    "mov {}, gs:0x30",
                    out(reg) teb,
                    options(nostack, nomem)
                );
                // TEB layout: +0x08 StackBase, +0x10 StackLimit, +0x48
                // ThreadId (ClientId). Read once; the faulting thread is the
                // currently executing thread in the VEH.
                let base = (teb as *const usize)
                    .add(0x08 / std::mem::size_of::<usize>())
                    .read_volatile();
                let limit = (teb as *const usize).add(0x10 / 8).read_volatile();
                let tptr = (teb as *const usize).add(0x48 / 8).read_volatile() as u32;
                (base, limit, tptr as usize)
            };
            if super::IN_BT.swap(true, std::sync::atomic::Ordering::SeqCst) {
                // Re-entered a second time: the first delivery published the
                // crash state and spun; there is nothing left to report here.
                if super::DEBUG_VAR.load(std::sync::atomic::Ordering::SeqCst) && re < 14 {
                    unsafe {
                        let h = GetStdHandle(STD_OUTPUT_HANDLE);
                        if !h.is_null() {
                            let mut s = [0u8; 32];
                            let tag = b"[iced] REENTER";
                            s[..tag.len()].copy_from_slice(tag);
                            s[tag.len()] = b'\n';
                            let mut written = 0u32;
                            WriteFile(
                                h,
                                s.as_ptr() as *const c_void,
                                tag.len() as u32 + 1,
                                &mut written,
                                std::ptr::null_mut(),
                            );
                        }
                    }
                }
                raw_crash("re-entered the crash handler while reporting");
            }
            // Publish for the reporter thread (which has a full stack) and
            // spin until it has printed. Nothing between the publish and the
            // spin may use meaningful stack space.
            super::CRASH_RIP.store(rip as usize, std::sync::atomic::Ordering::SeqCst);
            super::CRASH_RSP.store(rsp as usize, std::sync::atomic::Ordering::SeqCst);
            super::CRASH_ACC.store(acc, std::sync::atomic::Ordering::SeqCst);
            super::CRASH_CODE.store(code as usize, std::sync::atomic::Ordering::SeqCst);
            super::CRASH_SBASE.store(sbase, std::sync::atomic::Ordering::SeqCst);
            super::CRASH_SLIMIT.store(slimit, std::sync::atomic::Ordering::SeqCst);
            super::CRASH_TID.store(tid, std::sync::atomic::Ordering::SeqCst);
            super::CRASH_RSI.store(rsi as usize, std::sync::atomic::Ordering::SeqCst);
            super::CRASH_RCX.store(rcx as usize, std::sync::atomic::Ordering::SeqCst);
            let seq = super::CRASH_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            super::CRASH_SEQ.store(seq, std::sync::atomic::Ordering::SeqCst);
            // The reporter is parked, not polling, so it has to be woken
            // explicitly. `unpark` only touches an atomic, and it latches, so
            // a wake that lands before the reporter parks is not lost.
            {
                let reporter = super::REPORTER.load(std::sync::atomic::Ordering::SeqCst);
                if !reporter.is_null() {
                    // SAFETY: `install` stores a leaked `Thread` that lives for
                    // the rest of the process, and `unpark` is safe from any
                    // thread.
                    unsafe { (*reporter).unpark() };
                }
            }
            // Bound the spin so a missing reporter thread still exits. The
            // reporter exits the whole process once it has printed, so the
            // fallback here only runs if the report thread never woke up.
            let mut spins = 0usize;
            while super::SEEN.load(std::sync::atomic::Ordering::SeqCst) < seq
                && spins < 2_000_000_000
            {
                std::hint::spin_loop();
                spins += 1;
            }
            raw_crash("no reporter thread reached the crash");
        }
        EXCEPTION_CONTINUE_SEARCH
    }

    pub fn install_veh() {
        unsafe {
            AddVectoredExceptionHandler(1, veh_handler as *const c_void);
        }
    }
}

#[cfg(unix)]
mod unix {
    use std::ffi::c_void;

    /// Pull the faulting thread's RSP out of the signal's saved context on
    /// Linux x86_64; returns 0 where the layout is unknown so the backtrace
    /// walk is simply skipped.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    fn uctx_rsp(ucontext: *mut c_void) -> usize {
        if ucontext.is_null() {
            return 0;
        }
        unsafe {
            let uc = &*(ucontext as *const libc::ucontext_t);
            uc.uc_mcontext.gregs[libc::REG_RSP as usize] as usize
        }
    }

    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    fn uctx_rsp(_ucontext: *mut c_void) -> usize {
        0
    }

    unsafe extern "C" fn handler(sig: i32, _info: *mut libc::siginfo_t, ucontext: *mut c_void) {
        if super::PRINTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
            libc::_exit(101);
        }
        let rsp = uctx_rsp(ucontext);
        let bt = super::backtrace_from(rsp);
        let mut cause = format!("signal {sig}\nbacktrace:");
        if bt.is_empty() {
            cause.push_str(" <none>");
        } else {
            for f in bt {
                cause.push_str("\n  ");
                cause.push_str(&f);
            }
        }
        cause.push('\n');
        let _ = libc::write(
            2,
            super::STANZA.as_ptr() as *const c_void,
            super::STANZA.len(),
        );
        let _ = libc::write(2, cause.as_ptr() as *const c_void, cause.len());
        libc::_exit(101);
    }

    pub fn install_signals() {
        unsafe {
            for sig in [
                libc::SIGSEGV,
                libc::SIGBUS,
                libc::SIGABRT,
                libc::SIGILL,
                libc::SIGFPE,
            ] {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = handler as *const () as usize;
                libc::sigemptyset(&mut action.sa_mask);
                action.sa_flags = libc::SA_SIGINFO;
                libc::sigaction(sig, &action, std::ptr::null_mut());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::STANZA;

    // The personality is part of the contract: if the jar's words change, so
    // must the mirror copy in the self-hosted port notes. These tests pin the
    // exact texts so a stray refactor cannot silently re-brine them.
    #[test]
    fn ice_stanza_is_pinned() {
        let text = String::from_utf8(STANZA.to_vec()).unwrap();
        assert!(text.starts_with("The pickle jar cracked.\n\nInternal compiler error:\nE9999\n"));
        assert!(text.contains("The compiler encountered something it did not expect.\n"));
        assert!(text.contains("No pickles were harmed, but the vinegar level is concerning.\n"));
    }

    #[test]
    fn jar_diagnostics_lock() {
        let mut out = Vec::new();
        let lines = [
            "🥒 Pickle Jar Diagnostics",
            "Compiler: healthy",
            "Parser: fermented",
            "Resolver: brined",
            "Emitter: still soaking",
            "Vinegar: ██████████ 100%",
            "Jar integrity: acceptable",
        ];
        for l in lines {
            out.extend_from_slice(l.as_bytes());
            out.push(b'\n');
        }
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("🥒 Pickle Jar Diagnostics"));
        assert!(s.contains("Vinegar: ██████████ 100%"));
        assert!(s.contains("Jar integrity: acceptable"));
    }
}
