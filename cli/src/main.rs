use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use pickle_compiler::diag::DiagnosticSink;
use pickle_compiler::diag::SourceMap;
use pickle_compiler::front::{frontend, frontend_checked};

use std::io::IsTerminal;

mod build;
mod history_cli;
mod iced;
mod jit;

#[derive(Parser)]
#[command(name = "pickle", version, about = "The PickleScript toolchain")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Parse, resolve, and type-check a module
    Check {
        /// Source file to check
        file: PathBuf,
        /// Emit machine-readable JSON for every diagnostic on stdout
        #[arg(long)]
        json: bool,
        /// Group diagnostics under their error code
        #[arg(long)]
        group: bool,
    },
    /// Print the parsed AST for a module
    Ast {
        /// Source file to parse
        file: PathBuf,
    },
    /// Lex a module and print its canonical token stream, one token per line.
    /// The format is the serialized contract the self-hosted lexer must match,
    /// so `pickle lex` output on both compilers diffs cleanly.
    Lex {
        /// Source file to lex
        file: PathBuf,
    },
    /// Lower a checked module to PickleIR and print it
    Ir {
        /// Source file to lower
        file: PathBuf,
    },
    /// Lower a checked module to the lossless, round-trippable IRX text
    /// format and print it. Unlike `ir`, IRX includes the full extern and
    /// string tables plus per-func slots/tags/entry, and re-parses back into
    /// exactly the same module via `run-irx`.
    Irx {
        /// Source file to lower
        file: PathBuf,
    },
    /// Parse an IRX file back into a module and run its `main` with the JIT
    IrxRun {
        /// IRX file to run
        file: PathBuf,
        /// Extra arguments surfaced to the program via `args()`
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Explain a stable error code (e.g. `E0308`); with no code, list every
    /// code in the catalogue
    Explain {
        /// Error code to explain
        code: Option<String>,
        /// Also show, oldest first, every committed version of a `*.pkl`
        /// file that produced this code
        #[arg(long)]
        history: bool,
    },
    /// Show how public types and dependencies evolved across git history
    History {
        /// Only show history for this file
        #[arg(long)]
        file: Option<String>,
        /// Number of versions to show per file
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Show, newest first, which committed versions compiled and which
    /// failed, with the error codes of the failures
    Builds {
        /// Number of builds to show overall
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Compile a module with the JIT and run it
    Run {
        /// Source file to run
        file: PathBuf,
        /// Extra arguments surfaced to the program via `args()`
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Run the `test fn` items of one or more modules
    Test {
        /// A `*.pkl` file, or a directory of them (defaults to `tests/`).
        /// Directory targets only pick up `*_test.pkl` / `*.test.pkl` files.
        target: Option<PathBuf>,
        /// Run only tests whose name (after `test `) contains this substring
        #[arg(long, short)]
        filter: Option<String>,
        /// Run only tests carrying this tag; repeatable, a test matches any
        /// requested tag (combined with `--filter` with AND semantics)
        #[arg(long)]
        tag: Vec<String>,
    },
    /// Compile a module to a native executable, linking the runtime
    /// Compile a module to a native executable, linking the runtime
    Build {
        /// Source file to compile
        file: PathBuf,
        /// Output executable path (defaults to next to the source)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// 🥒
    #[command(hide = true)]
    Jar,
}

fn read_source(path: &PathBuf) -> Result<String> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(text)
}

/// Default output path for `pickle build`: the source's stem plus the
/// platform executable suffix, next to the source file.
fn default_output(file: &Path) -> PathBuf {
    let dir = file
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "pickle_program".to_string());
    let name = if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem
    };
    dir.join(name)
}

/// Forward the CLI `--` arguments to the runtime so `args()` in compiled
/// pickle code sees them (argv[0] is the tool name and is skipped, matching
/// the runtime contract). Must run after the runtime is initialised.
fn forward_program_args(args: &[String]) {
    let mut cstrings: Vec<std::ffi::CString> = vec![std::ffi::CString::new("pickle").unwrap()];
    for a in args {
        // NUL in a CLI arg is pathological; drop it rather than fail the run.
        match std::ffi::CString::new(a.as_str()) {
            Ok(c) => cstrings.push(c),
            Err(_) => cstrings.push(std::ffi::CString::new("").unwrap()),
        }
    }
    let ptrs: Vec<*const u8> = cstrings.iter().map(|c| c.as_ptr() as *const u8).collect();
    // SAFETY: `ptrs` outlives the call; `pickle_args_set` copies the bytes.
    unsafe {
        pickle_runtime::abi::pickle_args_set(ptrs.len(), ptrs.as_ptr());
    }
}

fn load(file: &PathBuf) -> Result<(String, SourceMap, DiagnosticSink)> {
    let source = read_source(file)?;
    let map = SourceMap::default();
    let diags = DiagnosticSink::new();
    Ok((source, map, diags))
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let colored = std::io::stderr().is_terminal();

    match &cli.command {
        Command::Check { file, json, group } => {
            let (source, mut map, diags) = load(file)?;
            let ok = frontend_checked(&file.display().to_string(), &source, &mut map, &diags);
            if *json {
                // Machine mode: the JSON itself goes to stdout, human
                // diagnostics are suppressed. The consumer decides what to
                // show and the exit code carries pass/fail.
                print!("{}", diags.render_all_json(&map));
            } else if *group {
                let rendered = diags.render_all_grouped(&map, colored);
                if !rendered.is_empty() {
                    eprint!("{rendered}");
                }
            } else {
                let rendered = diags.render_all(&map, colored);
                if !rendered.is_empty() {
                    eprint!("{rendered}");
                }
            }
            if diags.any_error() || ok.is_none() {
                std::process::exit(1);
            }
        }
        Command::Explain { code, history } => match code {
            Some(code) => {
                let id = format!("E{}", code.trim().to_uppercase().trim_start_matches('E'));
                let entry = pickle_compiler::error::explain(&id);
                if *history {
                    if let Some(entry) = entry {
                        println!("error[{}] -- {}", entry.code.id(), entry.title);
                        println!();
                        println!("{}", entry.rule);
                        println!();
                        println!("example:");
                        for line in entry.example.lines() {
                            println!("    {line}");
                        }
                        println!();
                    } else {
                        println!("(not in the error catalogue -- checking its history anyway)");
                        println!();
                    }
                    let root = history_cli::git_root(&std::env::current_dir()?)?;
                    let all = history_cli::history_for(&root)?;
                    history_cli::render_explain_history(&all, &id);
                } else {
                    match entry {
                        Some(entry) => {
                            println!("error[{}] -- {}", entry.code.id(), entry.title);
                            println!();
                            println!("{}", entry.rule);
                            println!();
                            println!("example:");
                            for line in entry.example.lines() {
                                println!("    {line}");
                            }
                        }
                        None => {
                            anyhow::bail!("unknown error code `{id}`");
                        }
                    }
                }
            }
            None => {
                for entry in pickle_compiler::error::CATALOGUE {
                    println!("error[{}] -- {}", entry.code.id(), entry.title);
                }
            }
        },
        Command::History { file, limit } => {
            let root = history_cli::git_root(&std::env::current_dir()?)?;
            let all = history_cli::history_for(&root)?;
            if all.is_empty() {
                println!("no `*.pkl` files in git history yet");
            } else {
                history_cli::render_history(&all, file.as_deref(), *limit);
            }
        }
        Command::Builds { limit } => {
            let root = history_cli::git_root(&std::env::current_dir()?)?;
            let all = history_cli::history_for(&root)?;
            if all.is_empty() {
                println!("no `*.pkl` files in git history yet");
            } else {
                history_cli::render_builds(&all, *limit);
            }
        }
        Command::Ast { file } => {
            let (source, mut map, diags) = load(file)?;
            if let Some(out) = frontend(&file.display().to_string(), &source, &mut map, &diags) {
                println!("{:#?}", out.program);
            }
            let rendered = diags.render_all(&map, colored);
            if !rendered.is_empty() {
                eprint!("{rendered}");
            }
            if diags.any_error() {
                std::process::exit(1);
            }
        }
        Command::Lex { file } => {
            let (source, mut map, diags) = load(file)?;
            // Register the file so diagnostics can point into it, then dump
            // every token the lexer produces, including trivia-era tokens
            // (Newline, Eof) the parser otherwise discards.
            let fid = map.add(file.display().to_string(), source.clone());
            for t in pickle_compiler::lexer::lex(fid, &source, &diags) {
                println!("{}", t.canonical());
            }
            let rendered = diags.render_all(&map, colored);
            if !rendered.is_empty() {
                eprint!("{rendered}");
            }
            if diags.any_error() {
                std::process::exit(1);
            }
        }
        Command::Ir { file } => {
            let (source, mut map, diags) = load(file)?;
            let module =
                frontend(&file.display().to_string(), &source, &mut map, &diags).and_then(|out| {
                    pickle_compiler::emit::emit_ir(&out.program, &out.resolved, &diags)
                });
            let rendered = diags.render_all(&map, colored);
            if !rendered.is_empty() {
                eprint!("{rendered}");
            }
            match module {
                Some(m) => println!("{m}"),
                None => std::process::exit(1),
            }
        }
        Command::Irx { file } => {
            let (source, mut map, diags) = load(file)?;
            let module =
                frontend(&file.display().to_string(), &source, &mut map, &diags).and_then(|out| {
                    pickle_compiler::emit::emit_ir(&out.program, &out.resolved, &diags)
                });
            let rendered = diags.render_all(&map, colored);
            if !rendered.is_empty() {
                eprint!("{rendered}");
            }
            match module {
                Some(m) => print!("{}", pickle_compiler::ir::irx_serialize(&m)),
                None => std::process::exit(1),
            }
        }
        Command::IrxRun { file, args } => {
            let text = read_source(file)?;
            let module = pickle_compiler::ir::IrModule::from_irx(&text)
                .map_err(anyhow::Error::msg)
                .context("parsing IRX")?;
            let runner = jit::Jit::new().with_context(|| "while setting up the JIT")?;
            jit::register_runtime_symbols();
            // Compiled pickle_* calls need the heap etc. alive while we run.
            pickle_runtime::abi::pickle_runtime_init();
            // Initialising the runtime grabs the world lock for this thread as
            // a side effect of `gc_mut()`. Hand it back before spawning the run
            // thread: the vm thread acquires it the moment it touches the heap,
            // and this thread only sits in `.join()`, so holding it here would
            // deadlock.
            pickle_runtime::world::release();
            forward_program_args(args);
            let program = runner
                .compile(&module)
                .with_context(|| "while JIT-compiling")?;
            // SAFETY: runtime is initialised and pickle_main is a no-arg
            // void-returning host-convention function.
            let res = std::thread::Builder::new()
                .stack_size(512 * 1024 * 1024)
                .spawn(move || unsafe {
                    program.run();
                })
                .with_context(|| "while spawning run thread")?
                .join();
            if res.is_err() {
                eprintln!("[main] run thread panicked (res.is_err)");
                std::process::exit(101);
            }
            pickle_runtime::abi::pickle_runtime_shutdown();
        }
        Command::Run { file, args } => {
            let (source, mut map, diags) = load(file)?;
            let module =
                frontend(&file.display().to_string(), &source, &mut map, &diags).and_then(|out| {
                    pickle_compiler::emit::emit_ir(&out.program, &out.resolved, &diags)
                });
            let rendered = diags.render_all(&map, colored);
            if !rendered.is_empty() {
                eprint!("{rendered}");
            }
            let module = module.context("frontend failed")?;
            let runner = jit::Jit::new().with_context(|| "while setting up the JIT")?;
            jit::register_runtime_symbols();
            // Compiled pickle_* calls need the heap etc. alive while we run.
            pickle_runtime::abi::pickle_runtime_init();
            // Initialising the runtime grabs the world lock for this thread as
            // a side effect of `gc_mut()`. Hand it back before spawning the run
            // thread: the vm thread acquires it the moment it touches the heap,
            // and this thread only sits in `.join()`, so holding it here would
            // deadlock.
            pickle_runtime::world::release();
            forward_program_args(args);
            let program = runner
                .compile(&module)
                .with_context(|| "while JIT-compiling")?;
            // SAFETY: runtime is initialised and pickle_main is a no-arg
            // void-returning host-convention function.
            let res = std::thread::Builder::new()
                .stack_size(512 * 1024 * 1024)
                .spawn(move || unsafe {
                    program.run();
                })
                .with_context(|| "while spawning run thread")?
                .join();
            if res.is_err() {
                // TEMP: identify whether the 101 comes from a panic here.
                eprintln!("[main] run thread panicked (res.is_err)");
                std::process::exit(101);
            }
            pickle_runtime::abi::pickle_runtime_shutdown();
        }
        Command::Test {
            target,
            filter,
            tag,
        } => {
            let target = target.clone().unwrap_or_else(|| PathBuf::from("tests"));
            let files: Vec<PathBuf> = if target.is_dir() {
                let mut out: Vec<PathBuf> = std::fs::read_dir(&target)
                    .with_context(|| format!("cannot read {}", target.display()))?
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| is_discoverable_test_file(p))
                    .collect();
                out.sort();
                out
            } else {
                vec![target.clone()]
            };
            if files.is_empty() {
                if target.is_dir() {
                    anyhow::bail!(
                        "no `*_test.pkl`/`*.test.pkl` files found under {}",
                        target.display()
                    );
                }
                anyhow::bail!("no tests found at {}", target.display());
            }
            // Test bodies call the runtime (strings, GC, ...), so bring it up
            // once and leave it up until every module has run.
            pickle_runtime::abi::pickle_runtime_init();
            let runner = jit::Jit::new().with_context(|| "while setting up the JIT")?;
            let mut failed = false;
            for file in &files {
                let (source, mut map, diags) = load(file)?;
                let module = frontend(&file.display().to_string(), &source, &mut map, &diags)
                    .and_then(|out| {
                        pickle_compiler::emit::emit_ir(&out.program, &out.resolved, &diags)
                    });
                let rendered = diags.render_all(&map, colored);
                if !rendered.is_empty() {
                    eprint!("{rendered}");
                }
                let Some(module) = module else {
                    eprintln!("error: {} failed to compile", file.display());
                    failed = true;
                    continue;
                };
                let test_fns: Vec<&pickle_compiler::ir::IrFunc> =
                    module.funcs.iter().filter(|f| f.is_test).collect();
                if test_fns.is_empty() {
                    eprintln!("note: {} has no tests", file.display());
                    continue;
                }
                let filter = filter.as_deref().filter(|f| !f.is_empty());
                let tags: Vec<&str> = tag.iter().map(String::as_str).collect();
                let tests = test_fns
                    .iter()
                    .copied()
                    .filter(|f| {
                        let name_ok = filter.is_none_or(|p| f.name.contains(p));
                        let tag_ok =
                            tags.is_empty() || tags.iter().any(|t| f.tags.iter().any(|g| g == t));
                        name_ok && tag_ok
                    })
                    .collect::<Vec<_>>();
                if tests.is_empty() {
                    eprintln!("note: {} has no matching tests", file.display());
                    continue;
                }
                // SAFETY: the runtime is initialised and the compiled test
                // bodies are zero-argument functions with the host calling
                // convention; `test_fns`/`names` below outlive the run.
                let program = runner
                    .compile_no_main(&module)
                    .with_context(|| format!("while JIT-compiling {}", file.display()))?;
                // Test modules have no `main`, so class registration + static
                // init live in the synthesized `pkl_test_setup` (no-op fn when
                // the module has nothing to set up).
                if let Some(addr) = program.symbol_addr("pkl_test_setup") {
                    let setup: unsafe extern "C" fn() =
                        unsafe { std::mem::transmute::<usize, unsafe extern "C" fn()>(addr) };
                    unsafe { setup() };
                }
                let names: Vec<Vec<u8>> =
                    tests.iter().map(|f| f.name.as_bytes().to_vec()).collect();
                let descriptors: Vec<pickle_runtime::abi::PickleTest> = tests
                    .iter()
                    .enumerate()
                    .map(|(i, f)| {
                        let addr = program
                            .symbol_addr(&f.symbol)
                            .unwrap_or_else(|| panic!("missing compiled symbol {}", f.symbol));
                        let body: extern "C" fn() =
                            unsafe { std::mem::transmute::<usize, extern "C" fn()>(addr) };
                        pickle_runtime::abi::PickleTest {
                            name: names[i].as_ptr(),
                            name_len: names[i].len() as u32,
                            body,
                        }
                    })
                    .collect();
                pickle_runtime::abi::pickle_test_register_table(
                    descriptors.as_ptr(),
                    descriptors.len() as u32,
                );
                // Hooks are hidden `fn`s whose mangled names encode the hook
                // kind and the `" > "`-joined group path.
                let hook_fns: Vec<&pickle_compiler::ir::IrFunc> = module
                    .funcs
                    .iter()
                    .filter(|f| f.name.starts_with("__pkl_hook_"))
                    .collect();
                let hook_groups: Vec<Vec<u8>> = hook_fns
                    .iter()
                    .map(|f| decode_hook(&f.name).0.into_bytes())
                    .collect();
                let hook_descriptors: Vec<pickle_runtime::abi::PickleHook> = hook_fns
                    .iter()
                    .enumerate()
                    .map(|(i, f)| {
                        let addr = program
                            .symbol_addr(&f.symbol)
                            .unwrap_or_else(|| panic!("missing compiled symbol {}", f.symbol));
                        let body: extern "C" fn() =
                            unsafe { std::mem::transmute::<usize, extern "C" fn()>(addr) };
                        let (_, kind) = decode_hook(&f.name);
                        pickle_runtime::abi::PickleHook {
                            group: hook_groups[i].as_ptr(),
                            group_len: hook_groups[i].len() as u32,
                            kind,
                            body,
                        }
                    })
                    .collect();
                pickle_runtime::abi::pickle_test_register_hooks(
                    hook_descriptors.as_ptr(),
                    hook_descriptors.len() as u32,
                );
                println!("running {} test(s) from {}", tests.len(), file.display());
                let code = pickle_runtime::abi::pickle_runtime_run_tests();
                pickle_runtime::abi::pickle_runtime_reset();
                if code != 0 {
                    failed = true;
                }
            }
            pickle_runtime::abi::pickle_runtime_shutdown();
            if failed {
                std::process::exit(1);
            }
        }
        Command::Build { file, output } => {
            let (source, mut map, diags) = load(file)?;
            let module =
                frontend(&file.display().to_string(), &source, &mut map, &diags).and_then(|out| {
                    pickle_compiler::emit::emit_ir(&out.program, &out.resolved, &diags)
                });
            let rendered = diags.render_all(&map, colored);
            if !rendered.is_empty() {
                eprint!("{rendered}");
            }
            let module = module.context("frontend failed")?;
            if module.funcs.iter().all(|f| !f.is_main) {
                anyhow::bail!("no `main` in this module; nothing to build");
            }
            let object =
                build::emit_object(&module).with_context(|| "while compiling to machine code")?;
            let out = output.clone().unwrap_or_else(|| default_output(file));
            let target_dir = std::env::temp_dir().join("pickle-aot");
            let rlib = build::build_runtime_rlib(&target_dir)
                .with_context(|| "while building the runtime for AOT")?;
            build::link_object(&object, &target_dir, &rlib, &out)
                .with_context(|| format!("while linking {}", out.display()))?;
            println!("built {}", out.display());
        }
        Command::Jar => {
            println!("🥒 Pickle Jar Diagnostics");
            println!();
            println!("Compiler: healthy");
            println!("Parser: fermented");
            println!("Resolver: brined");
            println!("Emitter: still soaking");
            println!();
            println!("Vinegar: ██████████ 100%");
            println!("Jar integrity: acceptable");
        }
    }
    Ok(())
}

fn main() {
    iced::install();
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// True when `p` is a directory-discoverable test file: extension `pkl`
/// and either `*_test.pkl` or `*.test.pkl` naming. Explicitly-passed files
/// are never filtered by name.
fn is_discoverable_test_file(p: &std::path::Path) -> bool {
    let Some(ext) = p.extension().map(|e| e.to_string_lossy()) else {
        return false;
    };
    if ext != "pkl" {
        return false;
    }
    let Some(stem) = p.file_stem().map(|s| s.to_string_lossy()) else {
        return false;
    };
    stem.ends_with("_test") || stem.ends_with(".test")
}

/// Decode a hidden hook function name (`__pkl_hook_<kind>_<group path>` or
/// `__pkl_hook_<kind>_` for the root group) into its (group, kind). `kind`
/// maps to the runtime's `HOOK_*` constants.
fn decode_hook(name: &str) -> (String, u8) {
    let rest = name.strip_prefix("__pkl_hook_").unwrap_or(name);
    let (kind_bits, rest) = match rest.get(..2) {
        Some("ba") => (0, &rest[2..]),
        Some("be") => (1, &rest[2..]),
        Some("ae") => (2, &rest[2..]),
        Some("aa") => (3, &rest[2..]),
        _ => (0, rest),
    };
    // Root hooks leave a leading `_` where the group path would be.
    let group = rest.strip_prefix('_').unwrap_or(rest).to_string();
    (group, kind_bits)
}
