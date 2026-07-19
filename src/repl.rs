//! Interactive REPL backed by LLVM JIT (`ExecutionEngine`).

use crate::codegen::Codegen;
use crate::diagnostic::{Loc, render};
use crate::parser::{self, FrontendError};
use crate::typeck::{self, TypeCk};
use inkwell::OptimizationLevel;
use inkwell::context::Context;
use inkwell::execution_engine::JitFunction;
use std::io::{self, BufRead, Write};

const REPL_MAIN: &str = "__risp_repl_main";

pub fn run() -> Result<(), String> {
    println!("Risp REPL (JIT). Type :help for commands, :quit to exit.");
    let mut session = Session::new();
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut buf = String::new();

    loop {
        let prompt = if buf.is_empty() { "risp> " } else { "....> " };
        write!(stdout, "{prompt}").map_err(plain)?;
        stdout.flush().map_err(plain)?;

        let mut line = String::new();
        let n = stdin.lock().read_line(&mut line).map_err(plain)?;
        if n == 0 {
            // EOF
            println!();
            break;
        }

        let trimmed = line.trim();
        if buf.is_empty() {
            match trimmed {
                "" => continue,
                ":q" | ":quit" | ":exit" => break,
                ":help" | ":h" => {
                    print_help();
                    continue;
                }
                ":clear" => {
                    session.clear();
                    println!("; cleared");
                    continue;
                }
                ":defs" => {
                    if session.defs.is_empty() {
                        println!("; (no definitions)");
                    } else {
                        for d in &session.defs {
                            println!("{d}");
                        }
                    }
                    continue;
                }
                _ => {}
            }
        }

        buf.push_str(&line);
        if paren_balance(&buf) > 0 {
            continue;
        }
        if paren_balance(&buf) < 0 {
            eprintln!("error: unmatched closing parenthesis");
            buf.clear();
            continue;
        }

        let input = buf.trim().to_string();
        buf.clear();
        if input.is_empty() {
            continue;
        }

        match session.eval(&input) {
            Ok(msg) => {
                if !msg.is_empty() {
                    println!("{msg}");
                }
            }
            Err(e) => eprint!("{e}"),
        }
    }
    Ok(())
}

fn print_help() {
    println!(
        "; Commands:\n\
         ;   :help / :h     show this help\n\
         ;   :quit / :q     exit\n\
         ;   :clear         drop accumulated definitions\n\
         ;   :defs          show accumulated definitions\n\
         ; Definitions (`defn` / `def` / `struct` / `enum` / `extern` / `trait` / `impl`) persist across inputs.\n\
         ; Other expressions are JIT-evaluated and printed."
    );
}

struct Session {
    /// Accumulated top-level definition source (defn / def).
    defs: Vec<String>,
}

impl Session {
    fn new() -> Self {
        Self { defs: Vec::new() }
    }

    fn clear(&mut self) {
        self.defs.clear();
    }

    fn eval(&mut self, input: &str) -> Result<String, String> {
        if is_definition(input) {
            self.eval_definition(input)
        } else {
            self.eval_expression(input)
        }
    }

    fn eval_definition(&mut self, input: &str) -> Result<String, String> {
        let mut trial = self.defs.clone();
        trial.push(input.to_string());
        let src = trial.join("\n");
        let mut prog = parser::parse(&src).map_err(|e| render_frontend("<repl>", &src, &e))?;
        let mut tyck = TypeCk::new();
        tyck.check_ex(&mut prog, false)
            .map_err(|e| render_typeck("<repl>", &src, &e))?;
        self.defs = trial;
        Ok("; ok".into())
    }

    fn eval_expression(&mut self, input: &str) -> Result<String, String> {
        // Ensure the fragment parses as an expression before wrapping.
        let _ = parser::parse_expr_src(input).map_err(|e| render_frontend("<repl>", input, &e))?;

        let mut parts = self.defs.clone();
        parts.push(format!(
            "(defn {REPL_MAIN} [] -> i32 (do (println {input}) 0))"
        ));
        let src = parts.join("\n");
        let mut prog = parser::parse(&src).map_err(|e| render_frontend("<repl>", &src, &e))?;
        let mut tyck = TypeCk::new();
        tyck.check_ex(&mut prog, false)
            .map_err(|e| render_typeck("<repl>", &src, &e))?;
        jit_call(&prog, &tyck, REPL_MAIN)?;
        Ok(String::new())
    }
}

/// JIT-compile `prog` and call `fn_name` (`[] -> i32`).
fn jit_call(prog: &crate::ast::Program, tyck: &TypeCk, fn_name: &str) -> Result<i32, String> {
    let context = Context::create();
    let mut cg = Codegen::new(&context, "risp_repl");
    cg.compile_program(prog, tyck).map_err(plain)?;
    cg.module.verify().map_err(|e| plain(e.to_string()))?;
    let module = cg.into_module();
    let ee = module
        .create_jit_execution_engine(OptimizationLevel::None)
        .map_err(|e| plain(e.to_string()))?;

    type ReplMainFn = unsafe extern "C" fn() -> i32;
    let f: JitFunction<ReplMainFn> = unsafe {
        ee.get_function(fn_name)
            .map_err(|e| plain(format!("JIT: missing {fn_name}: {e}")))?
    };
    let code = unsafe { f.call() };
    Ok(code)
}

fn is_definition(src: &str) -> bool {
    let t = src.trim_start();
    t.starts_with("(defn")
        || t.starts_with("(def ")
        || t.starts_with("(def\t")
        || t.starts_with("(def\n")
        || t.starts_with("(struct")
        || t.starts_with("(enum")
        || t.starts_with("(extern")
        || t.starts_with("(trait")
        || t.starts_with("(impl")
}

/// Net open paren/bracket count. Ignores contents of string literals.
fn paren_balance(s: &str) -> i32 {
    let mut bal = 0i32;
    let mut in_str = false;
    let mut escape = false;
    for c in s.chars() {
        if in_str {
            if escape {
                escape = false;
                continue;
            }
            match c {
                '\\' => escape = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '(' | '[' => bal += 1,
            ')' | ']' => bal -= 1,
            _ => {}
        }
    }
    bal
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paren_balance_basic() {
        assert_eq!(paren_balance("(+ 1 2)"), 0);
        assert_eq!(paren_balance("(+ 1"), 1);
        assert_eq!(paren_balance("(+ 1 2))"), -1);
        assert_eq!(paren_balance(r#"(println "a(b)")"#), 0);
    }

    #[test]
    fn session_defn_then_call() {
        let mut s = Session::new();
        assert_eq!(
            s.eval("(defn add [x: i32, y: i32] -> i32 (+ x y))")
                .unwrap(),
            "; ok"
        );
        // JIT prints "3\n"; we only assert it succeeds (exit 0).
        assert_eq!(s.eval("(add 1 2)").unwrap(), "");
    }

    #[test]
    fn session_expr_only() {
        let mut s = Session::new();
        assert_eq!(s.eval("(+ 10 32)").unwrap(), "");
    }
}
