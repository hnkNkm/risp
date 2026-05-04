//! End-to-end tests: build each `examples/*.rsp` with the compiled `risp` binary
//! and verify stdout / exit code against expectations embedded in the source.
//!
//! Expectation header (one line each, anywhere near the top of the file):
//!   ;;! stdout: <expected line>      (repeatable; matched line-by-line)
//!   ;;! exit: <code>
//!
//! Lines without a header are not asserted.

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

    let mut entries: Vec<PathBuf> = fs::read_dir(&examples_dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("rsp"))
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
