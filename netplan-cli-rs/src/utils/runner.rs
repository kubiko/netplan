//! Centralised subprocess execution.
//!
//! All command invocations that are not special-cased (e.g. Stdio::null(),
//! piped stdin) go through these functions so that the implementation can be
//! swapped for a mock in future integration tests.

use std::process::{Command, Output};

use anyhow::{anyhow, Context, Result};

/// Run `program args…` with inherited stdio and return the exit code.
/// Returns 1 when the process cannot be spawned.
pub fn run(program: &str, args: &[&str]) -> i32 {
    Command::new(program)
        .args(args)
        .status()
        .map(|s| s.code().unwrap_or(1))
        .unwrap_or(1)
}

/// Run `program args…` and return `Err` when the exit code is non-zero.
pub fn check(program: &str, args: &[&str]) -> Result<()> {
    let code = run(program, args);
    if code != 0 {
        return Err(anyhow!("'{program}' exited with code {code}"));
    }
    Ok(())
}

/// Run `program args…` and capture its stdout as a `String`.
pub fn capture(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to run '{program}'"))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run `program args…` and return the full `Output` (stdout, stderr, status).
pub fn output(program: &str, args: &[&str]) -> Result<Output> {
    Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to run '{program}'"))
}
