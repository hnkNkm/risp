mod ast;
mod codegen;
mod lexer;
mod parser;
mod typeck;

use clap::{Parser as ClapParser, Subcommand};
use codegen::Codegen;
use inkwell::context::Context;
use inkwell::targets::{
    CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine,
};
use inkwell::OptimizationLevel;
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
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.cmd {
        Cmd::EmitAst { input } => {
            let src = fs::read_to_string(&input)?;
            let prog = parser::parse(&src)?;
            println!("{prog:#?}");
        }
        Cmd::EmitLlvm { input } => {
            let src = fs::read_to_string(&input)?;
            let mut prog = parser::parse(&src)?;
            let mut tyck = typeck::TypeCk::new();
            tyck.check(&mut prog)?;
            let context = Context::create();
            let mod_name = input.file_stem().and_then(|s| s.to_str()).unwrap_or("main");
            let mut cg = Codegen::new(&context, mod_name);
            cg.compile_program(&prog, &tyck)?;
            cg.module.verify().map_err(|e| e.to_string())?;
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
                let mut p = std::env::current_dir()?;
                p.push(out);
                p
            };
            let status = Command::new(&exec).status()?;
            std::process::exit(status.code().unwrap_or(1));
        }
    }
    Ok(())
}

fn build(input: &Path, output: Option<&Path>) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let src = fs::read_to_string(input)?;
    let mut prog = parser::parse(&src)?;
    let mut tyck = typeck::TypeCk::new();
    tyck.check(&mut prog)?;

    let context = Context::create();
    let mod_name = input.file_stem().and_then(|s| s.to_str()).unwrap_or("main");
    let mut cg = Codegen::new(&context, mod_name);
    cg.compile_program(&prog, &tyck)?;
    cg.module.verify().map_err(|e| e.to_string())?;

    Target::initialize_native(&InitializationConfig::default())?;
    let triple = TargetMachine::get_default_triple();
    let target = Target::from_triple(&triple).map_err(|e| e.to_string())?;
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
        .ok_or("failed to create target machine")?;
    cg.module.set_triple(&triple);
    cg.module.set_data_layout(&tm.get_target_data().get_data_layout());

    let out_path = match output {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(input.file_stem().unwrap()),
    };
    let obj_path = out_path.with_extension("o");
    tm.write_to_file(&cg.module, FileType::Object, &obj_path)
        .map_err(|e| e.to_string())?;

    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let status = Command::new(&cc)
        .arg(&obj_path)
        .arg("-o")
        .arg(&out_path)
        .status()?;
    if !status.success() {
        return Err(format!("linker {cc} failed").into());
    }
    let _ = fs::remove_file(&obj_path);
    Ok(out_path)
}
