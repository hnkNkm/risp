mod ast;
mod codegen;
mod diagnostic;
mod lexer;
mod macroexpand;
mod parser;
mod repl;
mod resolve;
mod typeck;

use clap::{Parser as ClapParser, Subcommand};
use codegen::Codegen;
use diagnostic::{Loc, render};
use inkwell::OptimizationLevel;
use inkwell::context::Context;
use inkwell::targets::{
    CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine,
};
use parser::FrontendError;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(ClapParser)]
#[command(name = "risp", about = "Risp language compiler")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build a .rsp file into a native executable
    Build {
        input: PathBuf,
        /// Output path (default: input stem)
        #[arg(short = 'o', long)]
        output: Option<PathBuf>,
    },
    /// Build and run
    Run { input: PathBuf },
    /// Print LLVM IR to stdout
    EmitLlvm { input: PathBuf },
    /// Print parsed AST to stdout
    EmitAst { input: PathBuf },
    /// Interactive REPL (LLVM JIT)
    Repl,
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprint!("{e}");
        std::process::exit(1);
    }
}

/// Run the requested command. The returned error string is already formatted
/// for terminal display (may be a multi-line diagnostic ending in `\n`).
fn run(cli: Cli) -> Result<(), String> {
    match cli.cmd {
        Cmd::EmitAst { input } => {
            let (src, file) = load(&input)?;
            let prog = parser::parse(&src).map_err(|e| render_frontend(&file, &src, &e))?;
            println!("{prog:#?}");
        }
        Cmd::EmitLlvm { input } => {
            let (src, file) = load(&input)?;
            let mut prog = parser::parse(&src).map_err(|e| render_frontend(&file, &src, &e))?;
            resolve::resolve(&mut prog, &input, &src)
                .map_err(|e| render_resolve(&file, &src, &e))?;
            macroexpand::expand(&mut prog).map_err(|e| render_macro(&file, &src, &e))?;
            let mut tyck = typeck::TypeCk::new();
            tyck.check(&mut prog).map_err(|e| render_typeck(&file, &src, &e))?;
            let context = Context::create();
            let mod_name = input.file_stem().and_then(|s| s.to_str()).unwrap_or("main");
            let mut cg = Codegen::new(&context, mod_name);
            cg.compile_program(&prog, &tyck).map_err(plain)?;
            cg.module.verify().map_err(|e| plain(e.to_string()))?;
            print!("{}", cg.module.print_to_string().to_string());
        }
        Cmd::Build { input, output } => {
            build(&input, output.as_deref())?;
        }
        Cmd::Run { input } => {
            let out = build(&input, None)?;
            // Ensure relative path is invokable
            let exec = if out.is_absolute() {
                out
            } else {
                let mut p = std::env::current_dir().map_err(plain)?;
                p.push(out);
                p
            };
            let status = Command::new(&exec).status().map_err(plain)?;
            std::process::exit(status.code().unwrap_or(1));
        }
        Cmd::Repl => {
            repl::run()?;
        }
    }
    Ok(())
}

fn load(input: &Path) -> Result<(String, String), String> {
    let src = fs::read_to_string(input)
        .map_err(|e| plain(format!("could not read {}: {e}", input.display())))?;
    let file = input.display().to_string();
    Ok((src, file))
}

fn plain<E: std::fmt::Display>(e: E) -> String {
    format!("error: {e}\n")
}

fn render_frontend(file: &str, src: &str, e: &FrontendError) -> String {
    match e {
        FrontendError::Lex(le) => render(file, src, Loc::point(le.byte()), &le.to_string()),
        FrontendError::Parse(pe) => match pe.byte() {
            Some(b) => render(file, src, Loc::point(b), &pe.to_string()),
            None => format!("error: {pe}\n  --> {file}\n"),
        },
    }
}

fn render_typeck(file: &str, src: &str, e: &typeck::TypeError) -> String {
    match e.span() {
        Some(s) => render(file, src, Loc::from_span(s), &e.to_string()),
        None => format!("error: {e}\n  --> {file}\n"),
    }
}

fn render_macro(file: &str, src: &str, e: &macroexpand::MacroError) -> String {
    match e.span() {
        Some(s) => render(file, src, Loc::from_span(s), &e.to_string()),
        None => format!("error: {e}\n  --> {file}\n"),
    }
}

fn render_resolve(file: &str, src: &str, e: &resolve::ResolveError) -> String {
    match e {
        resolve::ResolveError::Frontend {
            file: f,
            src: s,
            err,
        } => render_frontend(f, s, err),
        _ => match e.span() {
            Some(span) => render(file, src, Loc::from_span(span), &e.to_string()),
            None => format!("error: {e}\n  --> {file}\n"),
        },
    }
}

fn build(input: &Path, output: Option<&Path>) -> Result<PathBuf, String> {
    let (src, file) = load(input)?;
    let mut prog = parser::parse(&src).map_err(|e| render_frontend(&file, &src, &e))?;
    resolve::resolve(&mut prog, input, &src).map_err(|e| render_resolve(&file, &src, &e))?;
    macroexpand::expand(&mut prog).map_err(|e| render_macro(&file, &src, &e))?;
    let mut tyck = typeck::TypeCk::new();
    tyck.check(&mut prog).map_err(|e| render_typeck(&file, &src, &e))?;

    let context = Context::create();
    let mod_name = input.file_stem().and_then(|s| s.to_str()).unwrap_or("main");
    let mut cg = Codegen::new(&context, mod_name);
    cg.compile_program(&prog, &tyck).map_err(plain)?;
    cg.module.verify().map_err(|e| plain(e.to_string()))?;

    Target::initialize_native(&InitializationConfig::default()).map_err(plain)?;
    let triple = TargetMachine::get_default_triple();
    let target = Target::from_triple(&triple).map_err(|e| plain(e.to_string()))?;
    let cpu = TargetMachine::get_host_cpu_name().to_string();
    let features = TargetMachine::get_host_cpu_features().to_string();
    let tm = target
        .create_target_machine(
            &triple,
            &cpu,
            &features,
            OptimizationLevel::Default,
            RelocMode::PIC,
            CodeModel::Default,
        )
        .ok_or_else(|| plain("failed to create target machine"))?;
    cg.module.set_triple(&triple);
    cg.module.set_data_layout(&tm.get_target_data().get_data_layout());

    let out_path = match output {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(input.file_stem().unwrap()),
    };
    let obj_path = out_path.with_extension("o");
    tm.write_to_file(&cg.module, FileType::Object, &obj_path)
        .map_err(|e| plain(e.to_string()))?;

    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let runtime = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("runtime/risp_rt.c");
    let status = Command::new(&cc)
        .arg(&obj_path)
        .arg(&runtime)
        .arg("-o")
        .arg(&out_path)
        .status()
        .map_err(plain)?;
    if !status.success() {
        return Err(plain(format!("linker {cc} failed")));
    }
    let _ = fs::remove_file(&obj_path);
    Ok(out_path)
}
