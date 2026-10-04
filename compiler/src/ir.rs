//! PickleIR: the typed, lowering-oriented intermediate representation produced
//! from the checked AST (Milestone 3, first slice).
//!
//! Model: each function is a small CFG of basic blocks over
//! procedure-local *slots* (memory, like `alloca` slots / Wasm locals) and
//! *temps* (fresh values, valid only inside the block that defined them).
//! Values that must survive a branch go through slots, so no phis are needed;
//! this maps 1:1 onto Cranelift stack locals and block-local virtual values.

use std::collections::HashMap;
use std::fmt;

/// The IR type lattice. Ptr is any heap reference (string, list, map,
/// class/struct instance, option). Strings are heap objects in this model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrTy {
    Bool,
    Char,
    Int,
    Float,
    Str,
    Unit,
    Ptr,
}

impl IrTy {
    pub fn is_unit(self) -> bool {
        self == IrTy::Unit
    }

    /// Storage width of a value of this type, in bits. Mirrors `tyWidth` in
    /// `pickle/compiler/x64.pkl` so both backends agree on symbol sizes.
    pub fn bit_width(self) -> u32 {
        match self {
            IrTy::Bool => 8,
            IrTy::Char => 32,
            IrTy::Int | IrTy::Float | IrTy::Str | IrTy::Unit | IrTy::Ptr => 64,
        }
    }

    /// Storage width in bytes: what a `.data`/`.bss` slot for this type needs.
    pub fn byte_width(self) -> usize {
        (self.bit_width() / 8) as usize
    }
}

impl fmt::Display for IrTy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            IrTy::Bool => "bool",
            IrTy::Char => "char",
            IrTy::Int => "int64",
            IrTy::Float => "float64",
            IrTy::Str => "string",
            IrTy::Unit => "unit",
            IrTy::Ptr => "ptr",
        };
        f.write_str(s)
    }
}

/// A value produced by an instruction; only valid in its defining block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Temp(pub u32);

impl fmt::Display for Temp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A procedure-local storage cell; valid function-wide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Slot(pub u32);

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A basic block id. `func.entry` is the entry block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockId(pub u32);

impl fmt::Display for BlockId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A module-level string constant (into `IrModule::strings`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StrId(pub usize);

impl fmt::Display for StrId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FuncId(pub usize);

impl fmt::Display for FuncId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExternId(pub usize);

impl fmt::Display for ExternId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrConst {
    Int(i64),
    Float(u64),
    Bool(bool),
    Char(u32),
    Null,
    Str(StrId),
    /// Tag `Int`: raw address of a `pkl_strdata_` block (module string data).
    /// Never used as a managed pointer; used to hand string bytes to the
    /// runtime (e.g. `pickle_class_register`). Typed `Int` so the value never
    /// enters the GC trace frame.
    StrAddr(StrId),
    /// Tag `Int`: the link-time address of a module function's symbol. Used to
    /// hand a finalizer (`pkl_<T>_deinit`) to `pickle_class_register`. Typed
    /// `Int` so the raw code pointer never enters the GC trace frame.
    FuncAddr(FuncId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    BitNot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Shl,
    Shr,
    BitAnd,
    BitOr,
    BitXor,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone)]
pub struct IrParam {
    pub name: String,
    pub ty: IrTy,
}

#[derive(Debug, Clone)]
pub struct IrFunc {
    /// Unmangled source name.
    pub name: String,
    /// Linkable symbol (compiler-owned namespace, `pkl_*`).
    pub symbol: String,
    pub params: Vec<IrParam>,
    pub ret: IrTy,
    /// Slot index -> type (params occupy slots 0..params.len()).
    pub slots: Vec<IrTy>,
    pub entry: BlockId,
    pub blocks: Vec<IrBlock>,
    /// `true` for `main`.
    pub is_main: bool,
    /// `true` for tests.
    pub is_test: bool,
    /// `#[tag("…")]` tags carried by test items (empty for non-tests).
    /// `#[tag("…")]` filters, hex-encoded by `irx_serialize` and whitespace
    /// delimited on the `tags` line. A tag must therefore be non-empty: an
    /// empty one serializes to a bare trailing space that the reader tokenizes
    /// away, and the round trip would fail the tag count. The emitter drops
    /// empty tags rather than writing one.
    pub tags: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct IrBlock {
    pub id: BlockId,
    pub instrs: Vec<IrInstr>,
    pub term: IrTerm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Callee {
    Func(FuncId),
    Extern(ExternId),
}

/// A reference to a runtime `pickle_*` symbol the codegen/link phase binds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IrExtern {
    pub symbol: String,
    pub params: Vec<IrTy>,
    pub ret: IrTy,
}

#[derive(Debug, Clone)]
pub enum IrInstr {
    Const {
        dst: Temp,
        c: IrConst,
    },
    /// Integer or float negation; logical/bool `not`; bitwise `not`.
    UnOp {
        dst: Temp,
        op: UnOp,
        v: Temp,
    },
    /// Arith/shift/bitwise/comparison. Comparison results are `bool`.
    BinOp {
        dst: Temp,
        op: BinOp,
        a: Temp,
        b: Temp,
    },
    Copy {
        dst: Temp,
        v: Temp,
    },
    /// Signed int -> float conversion (numeric promotion at mixed operands).
    Itof {
        dst: Temp,
        v: Temp,
    },
    /// Float -> int conversion (saturating cast).
    Ftoi {
        dst: Temp,
        v: Temp,
    },
    /// Char (code point, i32) -> int (i64): scalar reinterpreting cast.
    Chartoi {
        dst: Temp,
        v: Temp,
    },
    /// Int (i64) -> char (i32): scalar reinterpreting cast (narrowing).
    Itochar {
        dst: Temp,
        v: Temp,
    },
    LoadSlot {
        dst: Temp,
        slot: Slot,
    },
    StoreSlot {
        slot: Slot,
        v: Temp,
    },
    /// Raw address (`Int`) of a scalar local's stack storage. Only valid for
    /// non-managed slots; the address is never GC-tracked.
    LocalAddr {
        dst: Temp,
        slot: Slot,
    },
    /// Raw address (`Int`) of the data symbol backing global `id`. The escape
    /// route to `&g`, and what makes a global reachable by the GC root scan.
    GlobalAddr {
        dst: Temp,
        id: usize,
    },
    /// Read global `id` as `ty`.
    LoadGlobal {
        dst: Temp,
        id: usize,
        ty: IrTy,
    },
    /// Write `v` into global `id` as `ty`.
    StoreGlobal {
        id: usize,
        v: Temp,
        ty: IrTy,
    },
    /// Load a scalar of `ty` through a raw address (`Int`) temp.
    LoadRaw {
        dst: Temp,
        addr: Temp,
        ty: IrTy,
    },
    /// Store a scalar of `ty` through a raw address (`Int`) temp.
    StoreRaw {
        addr: Temp,
        v: Temp,
        ty: IrTy,
    },
    /// Call a user function or runtime symbol. `dst` absent for void calls.
    Call {
        dst: Option<Temp>,
        callee: Callee,
        args: Vec<Temp>,
    },
    /// Call a closure value: a function pointer (`Int`) over an explicit
    /// signature. `fn_addr` is the raw code address of the closure's body
    /// entry or of a top-level function used as a value.
    CallInd {
        dst: Option<Temp>,
        fn_addr: Temp,
        params: Vec<IrTy>,
        ret: IrTy,
        args: Vec<Temp>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrTerm {
    Branch {
        target: BlockId,
    },
    BranchIf {
        cond: Temp,
        then: BlockId,
        else_: BlockId,
    },
    Return {
        v: Option<Temp>,
    },
    Unreachable,
}

/// A top-level mutable slot: one data symbol with an optional initializer.
///
/// `init` is a *constant* expression (never a load from another global)
/// because it runs before `main`, in program order, with no frame to evaluate
/// in. That restriction keeps the image a pure data blob and makes ordering
/// cycles impossible rather than merely diagnosed.
#[derive(Debug, Clone, PartialEq)]
pub struct IrGlobal {
   /// Linker-facing symbol, e.g. `pkl_global_<module>_<name>`.
   pub symbol: String,
   pub ty: IrTy,
   /// Whether `ty` came from an explicit annotation (drives width of the slot).
   pub ty_explicit: bool,
   /// Little-endian initial bytes. `Some(empty)` means zero-init (`.bss`).
   pub init: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Default)]
pub struct IrModule {
    pub funcs: Vec<IrFunc>,
    pub externs: Vec<IrExtern>,
    /// Source function name -> id (slice-1: at most one non-generic entry).
    pub funcs_by_name: HashMap<String, FuncId>,
    /// String literal data.
    pub strings: Vec<Vec<u8>>,
    /// Top-level mutable slots, emitted into `.data`/`.bss`.
    pub globals: Vec<IrGlobal>,
}

impl IrModule {
    pub fn extern_id(&mut self, ex: IrExtern) -> ExternId {
        for (i, e) in self.externs.iter().enumerate() {
            if *e == ex {
                return ExternId(i);
            }
        }
        self.externs.push(ex);
        ExternId(self.externs.len() - 1)
    }
}

impl fmt::Display for IrModule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for func in &self.funcs {
            writeln!(f, "{func}")?;
        }
        Ok(())
    }
}

impl fmt::Display for IrFunc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = if self.is_main {
            "main "
        } else if self.is_test {
            "test "
        } else {
            ""
        };
        write!(
            f,
            "{kind}fn {}({}) -> {}{}",
            self.name,
            self.params
                .iter()
                .map(|p| format!("{}: {}", p.name, p.ty))
                .collect::<Vec<_>>()
                .join(", "),
            self.ret,
            if self.symbol.is_empty() {
                String::new()
            } else {
                format!(" @{}", self.symbol)
            }
        )?;
        for block in &self.blocks {
            writeln!(f)?;
            write!(f, "  {block}")?;
        }
        Ok(())
    }
}

impl fmt::Display for IrBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let _ = self.id;
        if self.id.0 == 0 {
            writeln!(f, "@entry:")?;
        } else {
            writeln!(f, "@{}:", self.id.0)?;
        }
        for instr in &self.instrs {
            writeln!(f, "    {instr}")?;
        }
        writeln!(f, "    {}", self.term)
    }
}

fn temp_name(t: Temp) -> String {
    format!("t{}", t.0)
}

impl fmt::Display for IrInstr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IrInstr::Const { dst, c } => {
                write!(f, "{} = const {}", temp_name(*dst), irconst_repr(*c))
            }
            IrInstr::UnOp { dst, op, v } => {
                let op = match op {
                    UnOp::Neg => "neg",
                    UnOp::Not => "not",
                    UnOp::BitNot => "bitnot",
                };
                write!(f, "{} = {} {}", temp_name(*dst), op, temp_name(*v))
            }
            IrInstr::BinOp { dst, op, a, b } => write!(
                f,
                "{} = binop.{} {}, {}",
                temp_name(*dst),
                binop_repr(*op),
                temp_name(*a),
                temp_name(*b)
            ),
            IrInstr::Copy { dst, v } => write!(f, "{} = copy {}", temp_name(*dst), temp_name(*v)),
            IrInstr::Itof { dst, v } => write!(f, "{} = itof {}", temp_name(*dst), temp_name(*v)),
            IrInstr::Ftoi { dst, v } => write!(f, "{} = ftoi {}", temp_name(*dst), temp_name(*v)),
            IrInstr::Chartoi { dst, v } => {
                write!(f, "{} = chartoi {}", temp_name(*dst), temp_name(*v))
            }
            IrInstr::Itochar { dst, v } => {
                write!(f, "{} = itochar {}", temp_name(*dst), temp_name(*v))
            }
            IrInstr::LoadSlot { dst, slot } => {
                write!(f, "{} = load slot s{}", temp_name(*dst), slot.0)
            }
            IrInstr::StoreSlot { slot, v } => {
                write!(f, "store {} -> slot s{}", temp_name(*v), slot.0)
            }
            IrInstr::LocalAddr { dst, slot } => {
                write!(f, "{} = addr slot s{}", temp_name(*dst), slot.0)
            }
            IrInstr::LoadRaw { dst, addr, ty } => {
                write!(
                    f,
                    "{} = loadraw.{} [{}]",
                    temp_name(*dst),
                    ty,
                    temp_name(*addr)
                )
            }
            IrInstr::StoreRaw { addr, v, ty } => {
                write!(
                    f,
                    "storeraw.{} {} -> [{}]",
                    ty,
                    temp_name(*v),
                    temp_name(*addr)
                )
            }
            IrInstr::GlobalAddr { dst, id } => {
                write!(f, "gaddr {} -> global#{}", temp_name(*dst), id)
            }
            IrInstr::LoadGlobal { dst, id, ty } => {
                write!(
                    f,
                    "loadglobal.{} global#{} -> {}",
                    ty,
                    id,
                    temp_name(*dst)
                )
            }
            IrInstr::StoreGlobal { id, v, ty } => {
                write!(f, "storeglobal.{} {} -> global#{}", ty, temp_name(*v), id)
            }
            IrInstr::Call { dst, callee, args } => {
                let callee = match callee {
                    Callee::Func(id) => format!("fn#{}", id.0),
                    Callee::Extern(id) => format!("extern#{}", id.0),
                };
                let args = args
                    .iter()
                    .map(|a| temp_name(*a))
                    .collect::<Vec<_>>()
                    .join(", ");
                match dst {
                    Some(d) => write!(f, "{} = call {}({})", temp_name(*d), callee, args),
                    None => write!(f, "call {}({})", callee, args),
                }
            }
            IrInstr::CallInd {
                dst,
                fn_addr,
                params,
                ret,
                args,
            } => {
                let sig = params
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                let args = args
                    .iter()
                    .map(|a| temp_name(*a))
                    .collect::<Vec<_>>()
                    .join(", ");
                match dst {
                    Some(d) => write!(
                        f,
                        "{} = callind [{}] -> {} ({}) via {}",
                        temp_name(*d),
                        sig,
                        ret,
                        args,
                        temp_name(*fn_addr)
                    ),
                    None => write!(
                        f,
                        "callind [{}] -> {} ({}) via {}",
                        sig,
                        ret,
                        args,
                        temp_name(*fn_addr)
                    ),
                }
            }
        }
    }
}

impl fmt::Display for IrTerm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IrTerm::Branch { target } => write!(f, "branch @{}", target.0),
            IrTerm::BranchIf { cond, then, else_ } => {
                write!(f, "branchif {} @{} @{}", temp_name(*cond), then.0, else_.0)
            }
            IrTerm::Return { v } => match v {
                Some(t) => write!(f, "return {}", temp_name(*t)),
                None => write!(f, "return"),
            },
            IrTerm::Unreachable => write!(f, "unreachable"),
        }
    }
}

fn binop_repr(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "add",
        BinOp::Sub => "sub",
        BinOp::Mul => "mul",
        BinOp::Div => "div",
        BinOp::Mod => "mod",
        BinOp::Shl => "shl",
        BinOp::Shr => "shr",
        BinOp::BitAnd => "band",
        BinOp::BitOr => "bor",
        BinOp::BitXor => "bxor",
        BinOp::Eq => "eq",
        BinOp::Ne => "ne",
        BinOp::Lt => "lt",
        BinOp::Le => "le",
        BinOp::Gt => "gt",
        BinOp::Ge => "ge",
    }
}

pub fn irconst_repr(c: IrConst) -> String {
    match c {
        IrConst::Int(v) => v.to_string(),
        IrConst::Float(bits) => format!("{}f", f64::from_bits(bits)),
        IrConst::Bool(v) => v.to_string(),
        IrConst::Char(v) => format!("'{}'", char::from_u32(v).unwrap_or('\u{FFFD}')),
        IrConst::Null => "null".to_string(),
        IrConst::Str(id) => format!("str#{}", id.0),
        IrConst::StrAddr(id) => format!("addrof str#{}", id.0),
        IrConst::FuncAddr(id) => format!("addrof fn#{}", id.0),
    }
}

/// Hex-encode a string-literal byte blob (module string table). Round-trips
/// through `unhex`. One of the pieces the lossy `.ir` text omits, so the
/// round-trippable serialization carries string bytes here.
pub fn hex(data: &[u8]) -> String {
    const HEXD: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(data.len() * 2);
    for b in data {
        out.push(HEXD[(b >> 4) as usize] as char);
        out.push(HEXD[(b & 0xf) as usize] as char);
    }
    out
}

/// Inverse of [`hex`].
pub fn unhex(s: &str) -> Option<Vec<u8>> {
    let chars: Vec<u8> = s.as_bytes().to_vec();
    if !chars.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(chars.len() / 2);
    let mut i = 0;
    while i < chars.len() {
        let hi = hexnib(chars[i])?;
        let lo = hexnib(chars[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

fn hexnib(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// A temp/slot/block id as a bare integer token. IRX uses bare ints for all
/// references (temps, slots, blocks, extern/string/func ids) so lines are
/// whitespace-delimited and trivially mirrorable by the self-hosted emitter.
fn tok_temp(t: Temp) -> String {
    t.0.to_string()
}

fn tok_slot(s: Slot) -> String {
    s.0.to_string()
}

/// Hex-encode a free-form string (func name/symbol, param name, tag) so
/// whitespace inside it (e.g. test names like `ordering > greater and less`)
/// cannot break line tokenising.
fn tok_text(s: &str) -> String {
    hex(s.as_bytes())
}

/// Serialize an `IrModule` into the lossless, round-trippable IRX text
/// format. Unlike the human-facing `.ir` dump this includes everything the
/// backend needs (extern and string tables, func slots/tags/entry), so the
/// text can be fed back through [`IrModule::from_irx`] and JIT-compiled.
///
/// Section markers and line syntax must match the self-hosted emitter's
/// mirror (the `_self` IRX files are byte-compared in the battery).
pub fn irx_serialize(m: &IrModule) -> String {
    let mut out = String::new();
    out.push_str(&format!("irx externs {}\n", m.externs.len()));
    for (i, e) in m.externs.iter().enumerate() {
        out.push_str(&format!("extern {i} {}\n", tok_text(&e.symbol)));
        out.push_str(&format!(
            "esig {} {}\n",
            e.params
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join(" "),
            e.ret
        ));
    }
    out.push_str(&format!("irx strings {}\n", m.strings.len()));
    for (i, s) in m.strings.iter().enumerate() {
        out.push_str(&format!("string {i} {}\n", hex(s)));
    }
    out.push_str(&format!("irx globals {}\n", m.globals.len()));
    for (i, g) in m.globals.iter().enumerate() {
        out.push_str(&format!(
            "global {i} {} {} {}\n",
            tok_text(&g.symbol),
            usize::from(g.ty_explicit),
            g.ty
        ));
        match &g.init {
            // `-` means zero-init (emitted into .bss); anything else is
            // little-endian init bytes (.data).
            None => out.push_str("ginit -\n"),
            Some(b) => out.push_str(&format!("ginit {}\n", hex(b))),
        }
    }
    out.push_str(&format!("irx funcs {}\n", m.funcs.len()));
    for (i, f) in m.funcs.iter().enumerate() {
        out.push_str(&format!(
            "func {i} {} {} {} {} {} {}\n",
            tok_text(&f.name),
            tok_text(&f.symbol),
            usize::from(f.is_main),
            usize::from(f.is_test),
            f.entry.0,
            f.ret
        ));
        out.push_str(&format!(
            "params {} {}\n",
            f.params.len(),
            f.params
                .iter()
                .map(|p| format!("{} {}", tok_text(&p.name), p.ty))
                .collect::<Vec<_>>()
                .join(" ")
        ));
        out.push_str(&format!(
            "slots {} {}\n",
            f.slots.len(),
            f.slots
                .iter()
                .map(|t| t.to_string())
                .collect::<Vec<_>>()
                .join(" ")
        ));
        out.push_str(&format!(
            "tags {} {}\n",
            f.tags.len(),
            f.tags
                .iter()
                .map(|t| tok_text(t))
                .collect::<Vec<_>>()
                .join(" ")
        ));
        out.push_str(&format!("blocks {}\n", f.blocks.len()));
        for b in &f.blocks {
            out.push_str(&format!("block {}\n", b.id.0));
            for instr in &b.instrs {
                out.push_str(&irx_instr_str(instr.clone()));
                out.push('\n');
            }
            out.push_str(&irx_term_str(&b.term));
            out.push('\n');
        }
    }
    out
}

fn irx_instr_str(i: IrInstr) -> String {
    match i {
        IrInstr::Const { dst, c } => format!(
            "const {} {}",
            tok_temp(dst),
            match c {
                IrConst::Int(v) => format!("int {v}"),
                IrConst::Float(bits) => format!("float {}", f64::from_bits(bits)),
                IrConst::Bool(v) => format!("bool {v}"),
                IrConst::Char(v) => format!("char {v}"),
                IrConst::Null => "null".to_string(),
                IrConst::Str(id) => format!("str {id}"),
                IrConst::StrAddr(id) => format!("straddr {id}"),
                IrConst::FuncAddr(id) => format!("fnaddr {id}"),
            }
        ),
        IrInstr::UnOp { dst, op, v } => format!(
            "unop {} {} {}",
            tok_temp(dst),
            match op {
                UnOp::Neg => "neg",
                UnOp::Not => "not",
                UnOp::BitNot => "bitnot",
            },
            tok_temp(v)
        ),
        IrInstr::BinOp { dst, op, a, b } => format!(
            "binop {} {} {} {}",
            tok_temp(dst),
            binop_repr(op),
            tok_temp(a),
            tok_temp(b)
        ),
        IrInstr::Copy { dst, v } => format!("copy {} {}", tok_temp(dst), tok_temp(v)),
        IrInstr::Itof { dst, v } => format!("itof {} {}", tok_temp(dst), tok_temp(v)),
        IrInstr::Ftoi { dst, v } => format!("ftoi {} {}", tok_temp(dst), tok_temp(v)),
        IrInstr::Chartoi { dst, v } => format!("chartoi {} {}", tok_temp(dst), tok_temp(v)),
        IrInstr::Itochar { dst, v } => format!("itochar {} {}", tok_temp(dst), tok_temp(v)),
        IrInstr::LoadSlot { dst, slot } => format!("load {} {}", tok_temp(dst), tok_slot(slot)),
        IrInstr::StoreSlot { slot, v } => format!("store {} {}", tok_slot(slot), tok_temp(v)),
        IrInstr::LocalAddr { dst, slot } => format!("addr {} {}", tok_temp(dst), tok_slot(slot)),
        IrInstr::LoadRaw { dst, addr, ty } => {
            format!("loadraw {} {} {}", tok_temp(dst), ty, tok_temp(addr))
        }
IrInstr::StoreRaw { addr, v, ty } => {
              format!("storeraw {} {} {}", ty, tok_temp(v), tok_temp(addr))
          }
          IrInstr::GlobalAddr { dst, id } => format!("gaddr {} {}", tok_temp(dst), id),
          IrInstr::LoadGlobal { dst, id, ty } => {
              format!("loadglobal {} {} {}", tok_temp(dst), id, ty)
          }
          IrInstr::StoreGlobal { id, v, ty } => {
              format!("storeglobal {} {} {}", id, ty, tok_temp(v))
          }
        IrInstr::Call { dst, callee, args } => {
            let cal = match callee {
                Callee::Func(id) => format!("fn {id}"),
                Callee::Extern(id) => format!("extern {id}"),
            };
            let ar = args
                .iter()
                .map(|a| tok_temp(*a))
                .collect::<Vec<_>>()
                .join(" ");
            match dst {
                Some(d) => format!("call {} {} {}", tok_temp(d), cal, ar),
                None => format!("call - {} {}", cal, ar),
            }
        }
        IrInstr::CallInd {
            dst,
            fn_addr,
            params,
            ret,
            args,
        } => {
            // `callind <dst|-> <fn_addr> <ret> <nparams> <pty>* <nargs> <t>*`
            // Both lists are count-prefixed so parsing is unambiguous.
            let mut parts = format!(
                "callind {} {} {} {}",
                match dst {
                    Some(d) => tok_temp(d),
                    None => "-".to_string(),
                },
                tok_temp(fn_addr),
                ret,
                params.len(),
            );
            for p in &params {
                parts.push(' ');
                parts.push_str(&p.to_string());
            }
            parts.push(' ');
            parts.push_str(&args.len().to_string());
            for a in &args {
                parts.push(' ');
                parts.push_str(&tok_temp(*a));
            }
            parts
        }
    }
}

fn irx_term_str(t: &IrTerm) -> String {
    match t {
        IrTerm::Branch { target } => format!("branch {target}"),
        IrTerm::BranchIf { cond, then, else_ } => {
            format!("branchif {} {} {}", tok_temp(*cond), then.0, else_.0)
        }
        IrTerm::Return { v } => match v {
            Some(t) => format!("return {}", tok_temp(*t)),
            None => "return -".to_string(),
        },
        IrTerm::Unreachable => "unreachable".to_string(),
    }
}

impl IrModule {
    /// Parse IRX text (produced by [`irx_serialize`], or by the self-hosted
    /// emitter's mirror) back into a module. `funcs_by_name` is rebuilt
    /// index-ordered.
    pub fn from_irx(text: &str) -> Result<IrModule, String> {
        let mut module = IrModule {
            funcs: Vec::new(),
            externs: Vec::new(),
            funcs_by_name: HashMap::new(),
            strings: Vec::new(),
            globals: Vec::new(),
        };
        let mut cur_func: Option<usize> = None;
        for (lineno, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            let err = |msg: &str| format!("irx line {}: {msg}: {raw:?}", lineno + 1);
            let toks: Vec<&str> = line.split_whitespace().collect();
            match toks[0] {
                "irx" => {
                    if toks.len() != 3 {
                        return Err(err("bad irx header"));
                    }
                    match toks[1] {
                        "externs" => {
                            let n: usize = toks[2].parse().map_err(|_| err("bad externs count"))?;
                            module.externs.reserve(n);
                        }
                        "strings" => {
                            let n: usize = toks[2].parse().map_err(|_| err("bad strings count"))?;
                            module.strings.reserve(n);
                        }
                        "funcs" => {
                            let n: usize = toks[2].parse().map_err(|_| err("bad funcs count"))?;
                            module.funcs.reserve(n);
                        }
                        "globals" => {
                            let n: usize = toks[2].parse().map_err(|_| err("bad globals count"))?;
                            module.globals.reserve(n);
                        }
                        v => return Err(err(&format!("unknown irx section {v}"))),
                    }
                }
                // `extern <i> <hexsymbol>` then `esig <p1> <p2> ... <ret>`.
                "extern" => {
                    let _id: usize = toks
                        .get(1)
                        .ok_or_else(|| err("bad extern id"))?
                        .parse()
                        .map_err(|_| err("bad extern id"))?;
                    let sym = unhex(toks.get(2).copied().unwrap_or(""))
                        .and_then(|b| String::from_utf8(b).ok())
                        .ok_or_else(|| err("bad extern symbol"))?;
                    module.externs.push(IrExtern {
                        symbol: sym,
                        params: Vec::new(),
                        ret: IrTy::Unit,
                    });
                }
                "esig" => {
                    if toks.len() < 2 {
                        return Err(err("esig needs a ret ty"));
                    }
                    let ret = parse_irx_ty(toks.last().copied().unwrap_or(""))
                        .ok_or_else(|| err("bad esig ret"))?;
                    let mut params = Vec::new();
                    for t in &toks[1..toks.len() - 1] {
                        params.push(parse_irx_ty(t).ok_or_else(|| err("bad esig param"))?);
                    }
                    let ex = module
                        .externs
                        .last_mut()
                        .ok_or_else(|| err("esig before extern"))?;
                    ex.params = params;
                    ex.ret = ret;
                }
                "string" => {
                    let _id: usize = toks
                        .get(1)
                        .ok_or_else(|| err("bad string id"))?
                        .parse()
                        .map_err(|_| err("bad string id"))?;
                    let bytes = unhex(toks.get(2).copied().unwrap_or(""))
                        .ok_or_else(|| err("bad string bytes"))?;
                    module.strings.push(bytes);
                }
                // `global <i> <hexsymbol> <typedeclared> <ty>` then `ginit -` or `ginit <hex>`.
                "global" => {
                    let _id: usize = toks
                        .get(1)
                        .ok_or_else(|| err("bad global id"))?
                        .parse()
                        .map_err(|_| err("bad global id"))?;
                    let symbol = unhex(toks.get(2).copied().unwrap_or(""))
                        .and_then(|b| String::from_utf8(b).ok())
                        .ok_or_else(|| err("bad global symbol"))?;
                    let explicit = toks.get(3).ok_or_else(|| err("bad global flag"))?;
                    let ty = parse_irx_ty(toks.get(4).copied().unwrap_or(""))
                        .ok_or_else(|| err("bad global ty"))?;
                    module.globals.push(IrGlobal {
                        symbol,
                        ty,
                        ty_explicit: *explicit == "1",
                        init: None,
                    });
                }
                "ginit" => {
                    // Strict on token count: silently ignoring trailing bytes
                    // would apply a truncated initializer and hand the global
                    // the wrong value.
                    if toks.len() != 2 {
                        return Err(err("ginit takes exactly one value"));
                    }
                    let payload = toks[1];
                    let g = module
                        .globals
                        .last_mut()
                        .ok_or_else(|| err("ginit before global"))?;
                    g.init = if payload == "-" {
                        None
                    } else {
                        Some(unhex(payload).ok_or_else(|| err("bad ginit bytes"))?)
                    };
                }
                "func" => {
                    // `func <i> <hexname> <hexsymbol> <main01> <test01> <entry> <ret>`
                    let _id: usize = toks
                        .get(1)
                        .ok_or_else(|| err("bad func id"))?
                        .parse()
                        .map_err(|_| err("bad func id"))?;
                    let name = unhex(toks.get(2).copied().unwrap_or(""))
                        .and_then(|b| String::from_utf8(b).ok())
                        .ok_or_else(|| err("bad func name"))?;
                    let symbol = unhex(toks.get(3).copied().unwrap_or(""))
                        .and_then(|b| String::from_utf8(b).ok())
                        .ok_or_else(|| err("bad func symbol"))?;
                    // The two flags are `== "1"`, so anything else reads as false;
                    // a short line is an error rather than a defaulted func.
                    let main_flag = toks.get(4).ok_or_else(|| err("bad func main flag"))?;
                    let test_flag = toks.get(5).ok_or_else(|| err("bad func test flag"))?;
                    let entry = toks.get(6).ok_or_else(|| err("bad entry"))?;
                    let ret_tok = toks.get(7).ok_or_else(|| err("bad func ret"))?;
                    let is_main: bool = *main_flag == "1";
                    let is_test: bool = *test_flag == "1";
                    let entry: u32 = entry.parse().map_err(|_| err("bad entry"))?;
                    let ret = parse_irx_ty(ret_tok).ok_or_else(|| err("bad func ret"))?;
                    module.funcs.push(IrFunc {
                        name,
                        symbol,
                        params: Vec::new(),
                        ret,
                        slots: Vec::new(),
                        entry: BlockId(entry),
                        blocks: Vec::new(),
                        is_main,
                        is_test,
                        tags: Vec::new(),
                    });
                    cur_func = Some(module.funcs.len() - 1);
                    module.funcs_by_name.insert(
                        module.funcs.last().unwrap().name.clone(),
                        FuncId(module.funcs.len() - 1),
                    );
                }
                "params" => {
                    let f = cur_func.ok_or_else(|| err("params outside func"))?;
                    let n: usize = toks
                        .get(1)
                        .ok_or_else(|| err("bad params count"))?
                        .parse()
                        .map_err(|_| err("bad params count"))?;
                    let mut params = Vec::with_capacity(n);
                    let mut i = 2;
                    while i < toks.len() {
                        let name = unhex(toks[i])
                            .and_then(|b| String::from_utf8(b).ok())
                            .ok_or_else(|| err("bad param name"))?;
                        let ty_tok = toks.get(i + 1).ok_or_else(|| err("bad param ty"))?;
                        let ty = parse_irx_ty(ty_tok).ok_or_else(|| err("bad param ty"))?;
                        params.push(IrParam { name, ty });
                        i += 2;
                    }
                    if params.len() != n {
                        return Err(err("params count mismatch"));
                    }
                    module.funcs[f].params = params;
                }
                "slots" => {
                    let f = cur_func.ok_or_else(|| err("slots outside func"))?;
                    let n: usize = toks
                        .get(1)
                        .ok_or_else(|| err("bad slots count"))?
                        .parse()
                        .map_err(|_| err("bad slots count"))?;
                    let mut slots = Vec::with_capacity(n);
                    for t in toks.get(2..).unwrap_or_default() {
                        slots.push(parse_irx_ty(t).ok_or_else(|| err("bad slot ty"))?);
                    }
                    if slots.len() != n {
                        return Err(err("slots count mismatch"));
                    }
                    module.funcs[f].slots = slots;
                }
                "tags" => {
                    let f = cur_func.ok_or_else(|| err("tags outside func"))?;
                    let n: usize = toks
                        .get(1)
                        .ok_or_else(|| err("bad tags count"))?
                        .parse()
                        .map_err(|_| err("bad tags count"))?;
                    let mut tags = Vec::with_capacity(n);
                    for t in toks.get(2..).unwrap_or_default() {
                        let s = unhex(t)
                            .and_then(|b| String::from_utf8(b).ok())
                            .ok_or_else(|| err("bad tag"))?;
                        tags.push(s);
                    }
                    if tags.len() != n {
                        return Err(err("tags count mismatch"));
                    }
                    module.funcs[f].tags = tags;
                }
                "blocks" => {
                    let f = cur_func.ok_or_else(|| err("blocks outside func"))?;
                    let n: usize = toks
                        .get(1)
                        .ok_or_else(|| err("bad blocks count"))?
                        .parse()
                        .map_err(|_| err("bad blocks count"))?;
                    module.funcs[f].blocks = Vec::with_capacity(n);
                }
                "block" => {
                    let f = cur_func.ok_or_else(|| err("block outside func"))?;
                    let id: u32 = toks
                        .get(1)
                        .ok_or_else(|| err("bad block id"))?
                        .parse()
                        .map_err(|_| err("bad block id"))?;
                    module.funcs[f].blocks.push(IrBlock {
                        id: BlockId(id),
                        instrs: Vec::new(),
                        term: IrTerm::Unreachable,
                    });
                }
                "const" | "unop" | "binop" | "copy" | "itof" | "ftoi" | "chartoi" | "itochar"
                | "load" | "store" | "addr" | "loadraw" | "storeraw" | "call" | "callind"
                  | "gaddr" | "loadglobal" | "storeglobal" => {
                    let f = cur_func.ok_or_else(|| err("instruction outside func"))?;
                    let block_last = module.funcs[f]
                        .blocks
                        .len()
                        .checked_sub(1)
                        .ok_or_else(|| err("instruction before any block"))?;
                    let instr = parse_irx_instr(line).map_err(|e| err(&e))?;
                    module.funcs[f].blocks[block_last].instrs.push(instr);
                }
                "branch" | "branchif" | "return" | "unreachable" => {
                    let f = cur_func.ok_or_else(|| err("term outside func"))?;
                    let block_last = module.funcs[f]
                        .blocks
                        .len()
                        .checked_sub(1)
                        .ok_or_else(|| err("term before any block"))?;
                    module.funcs[f].blocks[block_last].term =
                        parse_irx_term(line).map_err(|e| err(&e))?;
                }
                v => return Err(err(&format!("unrecognised token {v}"))),
            }
        }
        Ok(module)
    }
}

pub fn parse_irx_ty(s: &str) -> Option<IrTy> {
    match s {
        "bool" => Some(IrTy::Bool),
        "char" => Some(IrTy::Char),
        "int64" => Some(IrTy::Int),
        "float64" => Some(IrTy::Float),
        "string" => Some(IrTy::Str),
        "unit" => Some(IrTy::Unit),
        "ptr" => Some(IrTy::Ptr),
        _ => None,
    }
}

fn parse_irx_instr(line: &str) -> Result<IrInstr, String> {
    let toks: Vec<&str> = line.split_whitespace().collect();
    if toks.is_empty() {
        return Err("empty instr".to_string());
    }
    let num = |s: &str, what: &str| -> Result<u32, String> {
        s.parse().map_err(|_| format!("bad {what} {s}"))
    };
    let temp = |s: &str, what: &str| -> Result<Temp, String> {
        if s == "-" {
            return Err(format!("bad temp {what}: `-`"));
        }
        Ok(Temp(num(s, what)?))
    };
    let slot = |s: &str, what: &str| -> Result<Slot, String> {
        if s == "-" {
            return Err(format!("bad slot {what}: `-`"));
        }
        Ok(Slot(num(s, what)?))
    };
    let ty = |s: &str, what: &str| -> Result<IrTy, String> {
        parse_irx_ty(s).ok_or_else(|| format!("bad {what} ty {s}"))
    };
    let rest = &toks[1..];
    let head = toks[0];
    match head {
        "const" => {
            if rest.len() < 2 {
                return Err("const needs dst kind [val]".to_string());
            }
            let d = temp(rest[0], "const dst")?;
            let kind = rest[1];
            let c = match kind {
                // Every kind but `null` carries a value token; a short line is an
                // error, not a defaulted value.
                "int" => IrConst::Int(
                    rest.get(2)
                        .ok_or_else(|| "int const needs a value".to_string())?
                        .parse()
                        .map_err(|_| "bad int const")?,
                ),
                "float" => IrConst::Float(
                    rest.get(2)
                        .ok_or_else(|| "float const needs a value".to_string())?
                        .parse::<f64>()
                        .map_err(|_| "bad float literal")?
                        .to_bits(),
                ),
                "bool" => match rest.get(2).copied() {
                    Some("true") => IrConst::Bool(true),
                    Some("false") => IrConst::Bool(false),
                    _ => return Err("bad bool const".to_string()),
                },
                "char" => {
                    // A char const is a Unicode scalar: the emitter only ever
                    // writes one the frontend parsed, and an out-of-range or
                    // surrogate value has no UTF-8 encoding to print, so reject
                    // it here rather than hand it to the JIT.
                    let v: u32 = rest
                        .get(2)
                        .ok_or_else(|| "char const needs a value".to_string())?
                        .parse()
                        .map_err(|_| "bad char const")?;
                    if char::from_u32(v).is_none() {
                        return Err(format!("char const {v} is not a Unicode scalar"));
                    }
                    IrConst::Char(v)
                }
                "null" => IrConst::Null,
                "str" => IrConst::Str(StrId(
                    rest.get(2)
                        .ok_or_else(|| "str const needs a value".to_string())?
                        .parse()
                        .map_err(|_| "bad str id")?,
                )),
                "straddr" => IrConst::StrAddr(StrId(
                    rest.get(2)
                        .ok_or_else(|| "straddr const needs a value".to_string())?
                        .parse()
                        .map_err(|_| "bad straddr")?,
                )),
                "fnaddr" => IrConst::FuncAddr(FuncId(
                    rest.get(2)
                        .ok_or_else(|| "fnaddr const needs a value".to_string())?
                        .parse()
                        .map_err(|_| "bad fnaddr")?,
                )),
                _ => return Err(format!("bad const kind {kind}")),
            };
            Ok(IrInstr::Const { dst: d, c })
        }
        "unop" => {
            if rest.len() != 3 {
                return Err("unop needs dst op v".to_string());
            }
            let d = temp(rest[0], "unop dst")?;
            let op = match rest[1] {
                "neg" => UnOp::Neg,
                "not" => UnOp::Not,
                "bitnot" => UnOp::BitNot,
                _ => return Err(format!("bad unop {}", rest[1])),
            };
            let v = temp(rest[2], "unop v")?;
            Ok(IrInstr::UnOp { dst: d, op, v })
        }
        "binop" => {
            if rest.len() != 4 {
                return Err("binop needs dst op a b".to_string());
            }
            let d = temp(rest[0], "binop dst")?;
            let op = parse_irx_binop(rest[1])?;
            let a = temp(rest[2], "binop a")?;
            let b = temp(rest[3], "binop b")?;
            Ok(IrInstr::BinOp { dst: d, op, a, b })
        }
        "copy" | "itof" | "ftoi" | "chartoi" | "itochar" => {
            if rest.len() != 2 {
                return Err(format!("{head} needs dst v"));
            }
            let d = temp(rest[0], &format!("{head} dst"))?;
            let v = temp(rest[1], &format!("{head} v"))?;
            Ok(match head {
                "copy" => IrInstr::Copy { dst: d, v },
                "itof" => IrInstr::Itof { dst: d, v },
                "ftoi" => IrInstr::Ftoi { dst: d, v },
                "chartoi" => IrInstr::Chartoi { dst: d, v },
                _ => IrInstr::Itochar { dst: d, v },
            })
        }
        "load" | "addr" => {
            // `load <dst> <slot>` ; `addr <dst> <slot>`
            if rest.len() != 2 {
                return Err(format!("{head} needs dst slot"));
            }
            let d = temp(rest[0], &format!("{head} dst"))?;
            let s = slot(rest[1], &format!("{head} slot"))?;
            Ok(if head == "load" {
                IrInstr::LoadSlot { dst: d, slot: s }
            } else {
                IrInstr::LocalAddr { dst: d, slot: s }
            })
        }
        "store" => {
            // `store <slot> <v>`
            if rest.len() != 2 {
                return Err("store needs slot v".to_string());
            }
            let s = slot(rest[0], "store slot")?;
            let v = temp(rest[1], "store v")?;
            Ok(IrInstr::StoreSlot { slot: s, v })
        }
        "loadraw" | "storeraw" => {
            // `loadraw <dst> <ty> <addr>` ; `storeraw <ty> <v> <addr>`
            if head == "loadraw" {
                if rest.len() != 3 {
                    return Err("loadraw needs dst ty addr".to_string());
                }
                let d = temp(rest[0], "loadraw dst")?;
                let t = ty(rest[1], "loadraw")?;
                let addr = temp(rest[2], "loadraw addr")?;
                Ok(IrInstr::LoadRaw {
                    dst: d,
                    addr,
                    ty: t,
                })
            } else {
                if rest.len() != 3 {
                    return Err("storeraw needs ty v addr".to_string());
                }
                let t = ty(rest[0], "storeraw")?;
                let v = temp(rest[1], "storeraw v")?;
                let addr = temp(rest[2], "storeraw addr")?;
                Ok(IrInstr::StoreRaw { addr, v, ty: t })
            }
        }
            "gaddr" => {
                // `gaddr <dst> <id>`
                if rest.len() != 2 {
                    return Err("gaddr needs dst id".to_string());
                }
                let d = temp(rest[0], "gaddr dst")?;
                let id = rest[1].parse().map_err(|_| "bad global id")?;
                Ok(IrInstr::GlobalAddr { dst: d, id })
            }
            "loadglobal" => {
                // `loadglobal <dst> <id> <ty>`
                if rest.len() != 3 {
                    return Err("loadglobal needs dst id ty".to_string());
                }
                let d = temp(rest[0], "loadglobal dst")?;
                let id = rest[1].parse().map_err(|_| "bad global id")?;
                let t = ty(rest[2], "loadglobal")?;
                Ok(IrInstr::LoadGlobal { dst: d, id, ty: t })
            }
            "storeglobal" => {
                // `storeglobal <id> <ty> <v>`
                if rest.len() != 3 {
                    return Err("storeglobal needs id ty v".to_string());
                }
                let id = rest[0].parse().map_err(|_| "bad global id")?;
                let t = ty(rest[1], "storeglobal")?;
                let v = temp(rest[2], "storeglobal v")?;
                Ok(IrInstr::StoreGlobal { id, v, ty: t })
            }
        "call" => {
            // `call <dst|- `> <fn|extern> <id> <arg>...`
            if rest.len() < 3 {
                return Err("call needs dst callee id args".to_string());
            }
            let d = if rest[0] == "-" {
                None
            } else {
                Some(temp(rest[0], "call dst")?)
            };
            let callee = if rest[1] == "fn" {
                Callee::Func(FuncId(rest[2].parse().map_err(|_| "bad fn id")?))
            } else if rest[1] == "extern" {
                Callee::Extern(ExternId(rest[2].parse().map_err(|_| "bad extern id")?))
            } else {
                return Err(format!("bad callee {}", rest[1]));
            };
            let args = rest[3..]
                .iter()
                .map(|s| temp(s, "call arg"))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(IrInstr::Call {
                dst: d,
                callee,
                args,
            })
        }
        "callind" => {
            // `callind <dst|- > <fn_addr> <ret> <nparams> <pty>* <nargs> <arg>*`
            if rest.len() < 5 {
                return Err("callind needs dst fn_addr ret nparams".to_string());
            }
            let d = if rest[0] == "-" {
                None
            } else {
                Some(temp(rest[0], "callind dst")?)
            };
            let fn_addr = temp(rest[1], "callind fn_addr")?;
            let ret = ty(rest[2], "callind ret")?;
            let nparams: usize = rest[3].parse().map_err(|_| "bad nparams".to_string())?;
            // The count is attacker-controlled, so bound it before slicing:
            // `4 + nparams` can overflow and `rest` may be shorter than claimed.
            let after_params = 4usize
                .checked_add(nparams)
                .ok_or_else(|| "callind nparams overflows".to_string())?;
            let param_slice = rest
                .get(4..after_params)
                .ok_or_else(|| format!("callind nparams {nparams} exceeds the line"))?;
            let params = param_slice
                .iter()
                .map(|s| ty(s, "callind param"))
                .collect::<Result<Vec<_>, _>>()?;
            let after = rest.get(after_params..).unwrap_or(&[]);
            if after.is_empty() {
                return Err("callind missing nargs".to_string());
            }
            let nargs: usize = after[0].parse().map_err(|_| "bad nargs".to_string())?;
            // `nargs == 0` is the empty-arg indirect call the emitter writes.
            let arg_slice = after
                .get(1..=nargs)
                .ok_or_else(|| format!("callind nargs {nargs} exceeds the line"))?;
            let args = arg_slice
                .iter()
                .map(|s| temp(s, "callind arg"))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(IrInstr::CallInd {
                dst: d,
                fn_addr,
                params,
                ret,
                args,
            })
        }
        _ => Err(format!("unrecognised instr head {head}")),
    }
}

fn parse_irx_binop(s: &str) -> Result<BinOp, String> {
    Ok(match s {
        "add" => BinOp::Add,
        "sub" => BinOp::Sub,
        "mul" => BinOp::Mul,
        "div" => BinOp::Div,
        "mod" => BinOp::Mod,
        "shl" => BinOp::Shl,
        "shr" => BinOp::Shr,
        "band" => BinOp::BitAnd,
        "bor" => BinOp::BitOr,
        "bxor" => BinOp::BitXor,
        "eq" => BinOp::Eq,
        "ne" => BinOp::Ne,
        "lt" => BinOp::Lt,
        "le" => BinOp::Le,
        "gt" => BinOp::Gt,
        "ge" => BinOp::Ge,
        _ => return Err(format!("bad binop {s}")),
    })
}

fn parse_irx_term(line: &str) -> Result<IrTerm, String> {
    let toks: Vec<&str> = line.split_whitespace().collect();
    match toks[0] {
        "branch" => Ok(IrTerm::Branch {
            target: BlockId(
                toks.get(1)
                    .ok_or_else(|| "missing target".to_string())?
                    .parse()
                    .map_err(|_| "bad target".to_string())?,
            ),
        }),
        "branchif" => Ok(IrTerm::BranchIf {
            cond: Temp(
                toks.get(1)
                    .ok_or_else(|| "missing cond".to_string())?
                    .parse()
                    .map_err(|_| "bad cond".to_string())?,
            ),
            then: BlockId(
                toks.get(2)
                    .ok_or_else(|| "missing then".to_string())?
                    .parse()
                    .map_err(|_| "bad then".to_string())?,
            ),
            else_: BlockId(
                toks.get(3)
                    .ok_or_else(|| "missing else".to_string())?
                    .parse()
                    .map_err(|_| "bad else".to_string())?,
            ),
        }),
        "return" => {
            let v = toks
                .get(1)
                .filter(|s| **s != "-")
                .map(|s| {
                    s.parse()
                        .map(Temp)
                        .map_err(|_| format!("bad return temp {s}"))
                })
                .transpose()?;
            Ok(IrTerm::Return { v })
        }
        "unreachable" => Ok(IrTerm::Unreachable),
        _ => Err(format!("bad term {}", toks[0])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> IrModule {
        let mut m = IrModule {
            funcs: Vec::new(),
            externs: Vec::new(),
            funcs_by_name: HashMap::new(),
            strings: vec![b"hi \xC3\xA9".to_vec()],
            globals: Vec::new(),
        };
        let ex = m.extern_id(IrExtern {
            symbol: "pickle_str_len".to_string(),
            params: vec![IrTy::Ptr],
            ret: IrTy::Int,
        });
        let fid = FuncId(0);
        m.funcs.push(IrFunc {
            name: "f".to_string(),
            symbol: "pkl_f".to_string(),
            params: vec![IrParam {
                name: "x".to_string(),
                ty: IrTy::Int,
            }],
            ret: IrTy::Int,
            slots: vec![IrTy::Int, IrTy::Ptr],
            entry: BlockId(0),
            blocks: vec![IrBlock {
                id: BlockId(0),
                instrs: vec![
                    IrInstr::Const {
                        dst: Temp(0),
                        c: IrConst::Int(3),
                    },
                    IrInstr::Const {
                        dst: Temp(3),
                        c: IrConst::Float((-0.5f64).to_bits()),
                    },
                    IrInstr::BinOp {
                        dst: Temp(1),
                        op: BinOp::Add,
                        a: Temp(0),
                        b: Temp(0),
                    },
                    IrInstr::Call {
                        dst: Some(Temp(2)),
                        callee: Callee::Extern(ex),
                        args: vec![Temp(0)],
                    },
                ],
                term: IrTerm::Return { v: Some(Temp(1)) },
            }],
            is_main: false,
            is_test: false,
            tags: vec!["solo".to_string()],
        });
        let _ = fid;
        m
    }

    /// A func header with no body lines, so one line can be replaced at a time.
    const HEAD: &str = "irx externs 0\nirx strings 0\nirx funcs 1\nfunc 0 6d 6d 1 0 0 unit\nparams 0 \nslots 0 \ntags 0 \nblocks 1\nblock 0\n";

    fn line(text: &str) -> String {
        format!("{HEAD}{text}\nreturn 0\n")
    }

    /// Malformed IRX must come back as `Err`, never as a panic. The reader used
    /// to index tokens and slice ranges without bounds checks, so a truncated
    /// line crashed the compiler instead of reporting the bad line.
    #[test]
    fn from_irx_rejects_short_lines_without_panicking() {
        let bad = [
            "extern",
            "esig",
            "string",
            "func",
            "func 0",
            "func 0 6d",
            "func 0 6d 6d",
            "func 0 6d 6d 1",
            "func 0 6d 6d 1 0",
            "func 0 6d 6d 1 0 0",
            "params",
            "params 1",
            "params 1 61",
            "slots",
            "slots 1",
            "tags",
            "tags 1",
            "blocks",
            "block",
            "const 0 int",
            "const 0 float",
            "const 0 bool",
            "const 0 char",
            "const 0 str",
            "const 0 straddr",
            "const 0 fnaddr",
            "callind 0 1 int64 9 0",
            "callind 0 1 int64 0 5 0",
            "callind 0 1 int64 18446744073709551615 0",
            "callind 0 1 int64 0 18446744073709551615 1",
            "irx",
            "irx externs",
            // Not Unicode scalars: no UTF-8 encoding, so they must not reach
            // the JIT as a char.
            "const 0 char 55296",
            "const 0 char 57343",
            "const 0 char 1114112",
        ];
        for src in bad {
            let full = line(src);
            assert!(
                IrModule::from_irx(&full).is_err(),
                "expected an error for {src:?}"
            );
        }
    }

    /// Lines that used to be fine must keep parsing: the guards added to make
    /// the reader total cannot be over-tight.
    #[test]
    fn from_irx_still_accepts_the_awkward_valid_lines() {
        let ok = [
            "params 0 ",
            "params 1 61 int64",
            "slots 0 ",
            "slots 2 int64 ptr",
            "tags 0 ",
            "tags 1 c3a9",
            "const 0 null",
            "const 0 int 0 junk",
            "callind - 3 unit 0 0",
            "callind 0 1 int64 2 int64 ptr 2 4 5",
            "callind 0 1 int64 0 0",
        ];
        for src in ok {
            let full = line(src);
            assert!(
                IrModule::from_irx(&full).is_ok(),
                "expected success for {src:?}"
            );
        }
        // `esig` needs its `extern` first, and a bare ret ty is the one-token form.
        let with_extern = "irx externs 1\nirx strings 0\nirx funcs 0\nextern 0 61\nesig unit\n";
        assert!(IrModule::from_irx(with_extern).is_ok());
        assert!(IrModule::from_irx("irx externs 1\nextern 0 61\nesig\n").is_err());
    }

    /// The zero-arg indirect call is exactly what the emitter writes, so it has
    /// to survive a serialize/parse round trip.
    #[test]
    fn irx_roundtrips_zero_arg_callind() {
        let mut m = IrModule {
            funcs: Vec::new(),
            externs: Vec::new(),
            funcs_by_name: HashMap::new(),
            strings: Vec::new(),
            globals: Vec::new(),
        };
        m.funcs.push(IrFunc {
            name: "f".to_string(),
            symbol: "pkl_f".to_string(),
            params: Vec::new(),
            ret: IrTy::Unit,
            slots: Vec::new(),
            entry: BlockId(0),
            blocks: vec![IrBlock {
                id: BlockId(0),
                instrs: vec![IrInstr::CallInd {
                    dst: None,
                    fn_addr: Temp(0),
                    params: Vec::new(),
                    ret: IrTy::Unit,
                    args: Vec::new(),
                }],
                term: IrTerm::Unreachable,
            }],
            is_main: false,
            is_test: false,
            tags: Vec::new(),
        });
        let text = irx_serialize(&m);
        let parsed = IrModule::from_irx(&text).expect("zero-arg callind round trip");
        match &parsed.funcs[0].blocks[0].instrs[0] {
            IrInstr::CallInd {
                dst, params, args, ..
            } => {
                assert!(dst.is_none());
                assert!(params.is_empty());
                assert!(args.is_empty());
            }
            other => panic!("expected the callind back, got {other:?}"),
        }
    }

    #[test]
    fn irx_roundtrip() {
        let m = sample();
        let text = irx_serialize(&m);
        let parsed = IrModule::from_irx(&text).expect("parse");
        assert_eq!(parsed.funcs.len(), m.funcs.len());
        assert_eq!(parsed.externs, m.externs);
        assert_eq!(parsed.strings, m.strings);
        let pf = &parsed.funcs[0];
        assert_eq!(pf.name, "f");
        assert_eq!(pf.symbol, "pkl_f");
        assert_eq!(pf.params.len(), 1);
        assert_eq!(pf.params[0].name, "x");
        assert_eq!(pf.params[0].ty, IrTy::Int);
        assert_eq!(pf.ret, IrTy::Int);
        assert_eq!(pf.slots, vec![IrTy::Int, IrTy::Ptr]);
        assert_eq!(pf.entry, BlockId(0));
        assert_eq!(pf.blocks.len(), 1);
        assert_eq!(pf.blocks[0].instrs.len(), 4);
        // The float const `-0.5` round-trips through its decimal token back to
        // the identical bit pattern (Rust `f64` Display is shortest-roundtrip).
        let f = pf.blocks[0].instrs[1].clone();
        let bits = match f {
            IrInstr::Const {
                c: IrConst::Float(b),
                ..
            } => b,
            _ => panic!("instr[1] not a float const"),
        };
        assert_eq!(bits, (-0.5f64).to_bits());
        assert_eq!(pf.blocks[0].term, IrTerm::Return { v: Some(Temp(1)) });
        assert_eq!(pf.tags, vec!["solo"]);
    }

    /// Globals are the one piece of module state that is not reachable from a
    /// function body, so nothing else in these tests exercises it. If the IRX
    /// round trip loses a data symbol, every downstream global silently
    /// becomes a zero slot -- which still *runs*, so no execution test catches
    /// it. Hence an explicit round trip for the whole table.
    #[test]
    fn irx_roundtrips_globals() {
        let mut m = IrModule {
            funcs: Vec::new(),
            externs: Vec::new(),
            funcs_by_name: HashMap::new(),
            strings: Vec::new(),
            globals: Vec::new(),
        };
        m.globals.push(IrGlobal {
            symbol: "pkl_global_main_counter".to_string(),
            ty: IrTy::Int,
            ty_explicit: true,
            init: Some(7i64.to_le_bytes().to_vec()),
        });
        m.globals.push(IrGlobal {
            symbol: "pkl_global_main_scratch".to_string(),
            ty: IrTy::Ptr,
            ty_explicit: false,
            // zero-init: must come back as `.bss`, not `.data`
            init: None,
        });
        // An empty-but-present init would collapse into `.bss` if we were not
        // careful, so pin the distinction at the encoder level.
        let text = irx_serialize(&m);
        assert!(text.contains("\nglobal 0 "), "{text}");
        assert!(text.contains("\nginit 0700000000000000\n"), "{text}");
        assert!(text.contains("\nginit -\n"), "{text}");

        let back = IrModule::from_irx(&text).expect("globals must round trip");
        assert_eq!(back.globals.len(), 2);
        assert_eq!(back.globals[0].symbol, "pkl_global_main_counter");
        assert_eq!(back.globals[0].ty, IrTy::Int);
        assert!(back.globals[0].ty_explicit);
        assert_eq!(
            back.globals[0].init.as_deref(),
            Some(7i64.to_le_bytes().as_slice())
        );
        assert_eq!(back.globals[1].symbol, "pkl_global_main_scratch");
        assert_eq!(back.globals[1].ty, IrTy::Ptr);
        assert!(!back.globals[1].ty_explicit);
        assert_eq!(back.globals[1].init, None);
        // Serializing the parsed copy must be byte-identical, not merely equal.
        assert_eq!(irx_serialize(&back), text);
    }

    /// A `ginit` payload that is not hex must be rejected rather than silently
    /// producing an empty init (which would turn `.data` into `.bss`).
    #[test]
    fn irx_rejects_malformed_ginit() {
        let head = "irx externs 0\nirx strings 0\nirx globals 1\nglobal 0 61 1 int64\n";
        assert!(IrModule::from_irx(&format!("{head}ginit -\n")).is_ok());
        assert!(IrModule::from_irx(&format!("{head}ginit 0700000000000000\n")).is_ok());
        assert!(IrModule::from_irx(&format!("{head}ginit zz\n")).is_err());
        assert!(IrModule::from_irx(&format!("{head}ginit 07 09\n")).is_err());
        assert!(IrModule::from_irx(&format!("{head}ginit\n")).is_err());
        assert!(IrModule::from_irx(&format!("{head}ginit -\nextra\n")).is_err());
        // ginit before any global would attach to the wrong slot.
        assert!(IrModule::from_irx("irx globals 0\nginit -\n").is_err());
    }
}
