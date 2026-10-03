//! Links `pickle-cli` itself with a large stack reserve.
//!
//! The compiler's own stack is what recursive lowering runs on, and Cargo
//! builds a binary with the toolchain default: 1 MiB of reserve on Windows,
//! committed 4 KiB at a time. That is the same budget that made the self-hosted
//! path die with an access violation partway down a valid program, and the
//! native path runs out of it too -- an expression nested a few hundred levels
//! deep overflows it (`0xc00000fd`) even though the program is perfectly valid.
//!
//! Raising the reserve for the *executables we link* (`src/build.rs`) fixes the
//! programs a compiler produces, but the compiler doing the linking needs the
//! headroom just as much, so the same value is applied here. This is address
//! space only: pages are committed on demand, so the reserve costs nothing
//! unless lowering actually goes that deep.
//!
//! Windows only. Unix has no link-time stack reserve -- the main thread starts
//! at `ulimit -s` and the kernel grows it on demand -- so there is nothing for
//! a linker flag to change. `main.rs` covers Unix by running the work on a
//! thread with an explicit stack; this covers the main thread itself and
//! anything that reaches it before or after that thread starts.
//!
//! Keep in step with `STACK_RESERVE`/`STACK_COMMIT` in `src/build.rs`, with
//! `WORKER_STACK` in `src/main.rs`, and with the `write64` PE fields in
//! `pickle/compiler/pelink.pkl`; `cli/tests/stack_reserve.rs` fails if they
//! ever disagree.

use std::env;

const STACK_RESERVE: u64 = 256 * 1024 * 1024;
const STACK_COMMIT: u64 = 1024 * 1024;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/build.rs");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // `-bins` rather than a bare link-arg so this does not try to apply a
        // PE/COFF switch to the test harnesses or any non-binary target.
        println!("cargo:rustc-link-arg-bins=/STACK:{STACK_RESERVE},{STACK_COMMIT}");
    }
}
