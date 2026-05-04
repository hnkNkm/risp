//! End-to-end tests for `examples/`.
//!
//! Two suites:
//!   1. `examples_match_expectations` — happy-path examples in `examples/*.rsp`.
//!      Builds, runs, compares stdout + exit code.
//!      Headers:
//!        ;;! stdout: <expected line>   (repeatable; matched line-by-line)
//!        ;;! exit: <code>
//!
//!   2. `error_examples_match_diagnostics` — failure-path examples in
//!      `examples/errors/*.rsp`. Compilation must fail with a diagnostic
//!      pointing at the expected line:col.
//!      Headers:
//!        ;;! error_at: <line>:<col>
//!        ;;! error_contains: <substring>   (repeatable)

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Default, Debug)]
struct Expect {
    stdout: Vec<String>,
    exit: Option<i32>,
}

fn parse_expect(src: &str) -> Expect {
    let mut e = Expect::default();
    for line in src.lines() {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix(";;!") else { continue };
        let rest = rest.trim();
        if let Some(v) = rest.strip_prefix("stdout:") {
            e.stdout.push(v.trim_start().to_string());
        } else if let Some(v) = rest.strip_prefix("exit:") {
            e.exit = v.trim().parse().ok();
        }
    }
    e
}

fn risp_bin() -> PathBuf {
    // Cargo sets CARGO_BIN_EXE_<name> for integration tests.
    PathBuf::from(env!("CARGO_BIN_EXE_risp"))
}

fn run_example(rsp: &Path, tmp: &Path) -> (String, i32) {
    let stem = rsp.file_stem().unwrap().to_string_lossy().into_owned();
    let out = tmp.join(&stem);

    let status = Command::new(risp_bin())
        .args([OsStr::new("build"), rsp.as_os_str(), OsStr::new("-o"), out.as_os_str()])
        .status()
        .expect("failed to spawn risp build");
    assert!(status.success(), "risp build failed for {}", rsp.display());

    let output = Command::new(&out).output().expect("failed to run built binary");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let code = output.status.code().unwrap_or(-1);
    (stdout, code)
}

#[test]
fn examples_match_expectations() {
    let examples_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let tmp = std::env::temp_dir().join("risp-e2e");
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    // Only top-level .rsp; subdirs (e.g. errors/) are handled by other tests.
    let mut entries: Vec<PathBuf> = fs::read_dir(&examples_dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("rsp"))
        .collect();
    entries.sort();
    assert!(!entries.is_empty(), "no .rsp examples found");

    let mut failures: Vec<String> = Vec::new();
    for rsp in &entries {
        let src = fs::read_to_string(rsp).unwrap();
        let exp = parse_expect(&src);
        if exp.stdout.is_empty() && exp.exit.is_none() {
            // No expectations declared — skip silently.
            continue;
        }

        let (stdout, code) = run_example(rsp, &tmp);
        let actual_lines: Vec<&str> = stdout.lines().collect();

        for (i, want) in exp.stdout.iter().enumerate() {
            match actual_lines.get(i) {
                Some(got) if got == want => {}
                Some(got) => failures.push(format!(
                    "{}: stdout line {} mismatch\n  expected: {:?}\n  actual:   {:?}",
                    rsp.display(),
                    i + 1,
                    want,
                    got
                )),
                None => failures.push(format!(
                    "{}: stdout line {} missing\n  expected: {:?}\n  full stdout: {:?}",
                    rsp.display(),
                    i + 1,
                    want,
                    stdout
                )),
            }
        }

        if let Some(want) = exp.exit {
            if code != want {
                failures.push(format!(
                    "{}: exit code mismatch (expected {}, got {})",
                    rsp.display(),
                    want,
                    code
                ));
            }
        }
    }

    assert!(failures.is_empty(), "e2e failures:\n{}", failures.join("\n"));
}

#[derive(Default, Debug)]
struct ErrExpect {
    error_at: Option<(usize, usize)>,
    contains: Vec<String>,
}

fn parse_err_expect(src: &str) -> ErrExpect {
    let mut e = ErrExpect::default();
    for line in src.lines() {
        let line = line.trim_start();
        let Some(rest) = line.strip_prefix(";;!") else { continue };
        let rest = rest.trim();
        if let Some(v) = rest.strip_prefix("error_at:") {
            let v = v.trim();
            if let Some((l, c)) = v.split_once(':') {
                if let (Ok(l), Ok(c)) = (l.trim().parse(), c.trim().parse()) {
                    e.error_at = Some((l, c));
                }
            }
        } else if let Some(v) = rest.strip_prefix("error_contains:") {
            e.contains.push(v.trim().to_string());
        }
    }
    e
}

#[test]
fn error_examples_match_diagnostics() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples").join("errors");
    let tmp = std::env::temp_dir().join("risp-e2e-err");
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().and_then(|s| s.to_str()) == Some("rsp"))
        .collect();
    entries.sort();
    assert!(!entries.is_empty(), "no error examples found");

    let mut failures: Vec<String> = Vec::new();
    for rsp in &entries {
        let src = fs::read_to_string(rsp).unwrap();
        let exp = parse_err_expect(&src);
        if exp.error_at.is_none() && exp.contains.is_empty() {
            continue;
        }

        let stem = rsp.file_stem().unwrap().to_string_lossy().into_owned();
        let out = tmp.join(&stem);
        let output = Command::new(risp_bin())
            .args([OsStr::new("build"), rsp.as_os_str(), OsStr::new("-o"), out.as_os_str()])
            .output()
            .expect("failed to spawn risp build");

        if output.status.success() {
            failures.push(format!(
                "{}: expected compile failure, but build succeeded",
                rsp.display()
            ));
            continue;
        }
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        if let Some((line, col)) = exp.error_at {
            // Match `--> <path>:LINE:COL` — our diagnostic format.
            let needle = format!(":{line}:{col}");
            if !stderr.contains(&needle) {
                failures.push(format!(
                    "{}: expected error at {}:{}\n  stderr:\n{}",
                    rsp.display(),
                    line,
                    col,
                    indent(&stderr)
                ));
            }
        }
        for sub in &exp.contains {
            if !stderr.contains(sub) {
                failures.push(format!(
                    "{}: stderr missing expected substring {:?}\n  stderr:\n{}",
                    rsp.display(),
                    sub,
                    indent(&stderr)
                ));
            }
        }
    }

    assert!(failures.is_empty(), "error-example failures:\n{}", failures.join("\n"));
}

fn indent(s: &str) -> String {
    s.lines().map(|l| format!("    {l}")).collect::<Vec<_>>().join("\n")
}
