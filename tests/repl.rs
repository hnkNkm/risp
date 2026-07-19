//! Smoke test for `risp repl` via piped stdin.

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn repl_piped_session() {
    let bin = env!("CARGO_BIN_EXE_risp");
    let mut child = Command::new(bin)
        .arg("repl")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn risp repl");

    {
        let mut stdin = child.stdin.take().expect("stdin");
        writeln!(stdin, "(defn add [x: i32, y: i32] -> i32 (+ x y))").unwrap();
        writeln!(stdin, "(add 20 22)").unwrap();
        writeln!(stdin, ":quit").unwrap();
    }

    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("; ok") && stdout.contains("42"),
        "unexpected stdout:\n{stdout}"
    );
}
