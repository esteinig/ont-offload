// SPDX-License-Identifier: AGPL-3.0-or-later
//! `cargo xtask`: tests, lint, coverage, supply chain and medtrace traceability.
//!
//! Standard library only. External tools are optional locally (CI installs them):
//! cargo-nextest, cargo-llvm-cov, cargo-deny, cargo-cyclonedx, cog, medtrace
//! (from `PATH` or the `MEDTRACE` environment variable).
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const USAGE: &str = "\
cargo xtask <task> [options]

tasks:
  test [--unit|--integration]  run tests (nextest with JUnit if installed, else cargo test)
  lint                         rustfmt --check and clippy -D warnings
  fmt                          format the workspace
  coverage                     llvm-cov lcov report (lcov.info)
  deny                         cargo-deny advisories, licences, bans, sources
  sbom                         CycloneDX SBOM (sbom.cdx.json)
  trace [--strict] [--gate G]  medtrace ingest JUnit, matrix, gate (pr|release)
  soup [--write]               SOUP items from Cargo.lock (medtrace)
  check-commits [RANGE]        Conventional Commits and Refs: trailers (cog + medtrace)
  set-version <X.Y.Z>          set the workspace version
  ci                           lint, test, trace --strict, deny (as CI does)";

type Res = Result<(), String>;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some((task, rest)) = args.split_first() else {
        println!("{USAGE}");
        return ExitCode::from(2);
    };
    let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
    let result = match task.as_str() {
        "test" => test(&rest),
        "lint" => lint(),
        "fmt" => cargo(&["fmt", "--all"]),
        "coverage" => cargo(&[
            "llvm-cov",
            "--workspace",
            "--lcov",
            "--output-path",
            "lcov.info",
        ]),
        "deny" => cargo(&["deny", "check"]),
        "sbom" => cargo(&[
            "cyclonedx",
            "--format",
            "json",
            "--override-filename",
            "sbom.cdx",
        ]),
        "trace" => trace(&rest),
        "soup" => soup(&rest),
        "check-commits" => check_commits(&rest),
        "set-version" => set_version(&rest),
        "ci" => ci(),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown task `{other}`\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

fn run(program: &str, args: &[&str]) -> Res {
    eprintln!("$ {program} {}", args.join(" "));
    let status = Command::new(program)
        .args(args)
        .current_dir(root())
        .status()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} failed ({status})"))
    }
}

fn cargo(args: &[&str]) -> Res {
    run(&env::var("CARGO").unwrap_or_else(|_| "cargo".into()), args)
}

fn has(program: &str, probe: &[&str]) -> bool {
    Command::new(program)
        .args(probe)
        .output()
        .is_ok_and(|o| o.status.success())
}

fn has_nextest() -> bool {
    has("cargo", &["nextest", "--version"])
}

fn medtrace() -> String {
    env::var("MEDTRACE").unwrap_or_else(|_| "medtrace".into())
}

fn test(args: &[&str]) -> Res {
    let unit = !args.contains(&"--integration");
    let integration = !args.contains(&"--unit");
    if has_nextest() {
        if unit {
            cargo(&["nextest", "run", "--workspace", "--profile", "unit"])?;
        }
        if integration {
            cargo(&["nextest", "run", "--workspace", "--profile", "integration"])?;
        }
        if unit {
            cargo(&["test", "--workspace", "--doc"])?;
        }
        Ok(())
    } else {
        eprintln!("xtask: cargo-nextest not found; falling back to cargo test (no JUnit reports)");
        match (unit, integration) {
            (true, false) => cargo(&["test", "--workspace", "--lib", "--bins"]),
            (false, true) => cargo(&["test", "--workspace", "--tests"]),
            _ => cargo(&["test", "--workspace"]),
        }
    }
}

fn lint() -> Res {
    cargo(&["fmt", "--all", "--check"])?;
    cargo(&[
        "clippy",
        "--workspace",
        "--all-targets",
        "--all-features",
        "--",
        "-D",
        "warnings",
    ])
}

fn trace(args: &[&str]) -> Res {
    let m = medtrace();
    if !has(&m, &["--version"]) {
        return Err(format!(
            "medtrace not found (set MEDTRACE or add `{m}` to PATH)"
        ));
    }
    let gate = args
        .iter()
        .position(|a| *a == "--gate")
        .and_then(|i| args.get(i + 1).copied())
        .unwrap_or("pr");
    run(&m, &["check"])?;
    for report in [
        "target/nextest/unit/junit-unit.xml",
        "target/nextest/integration/junit-integration.xml",
    ] {
        if root().join(report).exists() {
            run(&m, &["results", "ingest", "--junit", report])?;
        } else if args.contains(&"--strict") {
            return Err(format!(
                "missing {report}: run `cargo xtask test` with cargo-nextest first"
            ));
        }
    }
    run(&m, &["trace", "matrix"])?;
    let mut gate_args = vec!["gate", gate];
    if args.contains(&"--strict") {
        gate_args.push("--strict");
    }
    run(&m, &gate_args)
}

fn soup(args: &[&str]) -> Res {
    let mut a = vec![
        "soup",
        "sync",
        "--lockfile",
        "Cargo.lock",
        "--dir",
        "compliance/soup",
    ];
    if !args.contains(&"--write") {
        a.push("--check");
    }
    run(&medtrace(), &a)
}

fn check_commits(args: &[&str]) -> Res {
    let range = args.first().copied().unwrap_or("origin/dev..HEAD");
    run("cog", &["check", "--from-latest-tag"])?;
    run(
        &medtrace(),
        &["commits", "check", "--range", range, "--require-refs"],
    )
}

fn set_version(args: &[&str]) -> Res {
    let version = args.first().ok_or("usage: cargo xtask set-version X.Y.Z")?;
    let ok = version.split('.').count() == 3
        && version
            .split('.')
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    if !ok {
        return Err(format!("not a SemVer version: {version}"));
    }
    let path = root().join("Cargo.toml");
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut in_ws_pkg = false;
    let mut done = false;
    let out: Vec<String> = text
        .lines()
        .map(|line| {
            let t = line.trim();
            if t.starts_with('[') {
                in_ws_pkg = t == "[workspace.package]";
            }
            if in_ws_pkg && !done && t.starts_with("version") {
                done = true;
                format!("version = \"{version}\"")
            } else {
                line.to_string()
            }
        })
        .collect();
    if !done {
        return Err("no version in [workspace.package]".into());
    }
    std::fs::write(&path, out.join("\n") + "\n").map_err(|e| e.to_string())?;
    cargo(&["update", "--workspace"])?;
    eprintln!("version set to {version}");
    Ok(())
}

fn ci() -> Res {
    lint()?;
    test(&[])?;
    trace(&["--strict"])?;
    if has("cargo", &["deny", "--version"]) {
        cargo(&["deny", "check"])?;
    } else {
        eprintln!("xtask: cargo-deny not installed; skipped (CI runs it)");
    }
    Ok(())
}
