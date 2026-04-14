//! `netplan get` – query a configuration key from the YAML hierarchy.
//!
//! Mirrors `netplan_cli/cli/commands/get.py` and the `NetplanConfigState`
//! class in `cli/state.py`.

use anyhow::Result;
use clap::Args;

use crate::netplan;

#[derive(Args, Debug)]
pub struct GetArgs {
    /// Nested key in dotted format, e.g. "ethernets.eth0.addresses".
    /// Use "all" (default) to dump the entire configuration.
    #[arg(default_value = "all")]
    key: String,

    /// Read configuration from this root directory instead of /
    #[arg(long, default_value = "/")]
    root_dir: String,
}

pub fn run(args: GetArgs) -> Result<()> {
    // Parse the YAML hierarchy and validate
    let state = netplan::load_state(&args.root_dir)?;

    // Dump the full config to a string
    let full_yaml = state.dump_yaml()?;

    let output = if args.key == "all" {
        full_yaml
    } else {
        // Build the path prefix: prepend "network" if not already present
        let key = if args.key.starts_with("network") {
            args.key.clone()
        } else {
            format!("network.{}", args.key)
        };

        // Split on '.' but treat '\.' as a literal dot (negative lookbehind)
        let prefix: Vec<String> = split_dotted_path(&key);

        netplan::dump_yaml_subtree(&prefix, &full_yaml)?
    };

    // Print without a trailing newline, matching Python's `print(state, end='')`
    print!("{}", output);
    Ok(())
}

/// Split a dotted key path on `.` but not on `\.`.
/// Each part has `\.` sequences replaced with `.`.
///
/// Equivalent to:
/// ```python
/// [s.replace(r'\.', '.') for s in re.split(r'(?<!\\)\.', key)]
/// ```
fn split_dotted_path(key: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut chars = key.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'.') => {
                chars.next(); // consume the escaped dot
                current.push('.');
            }
            '.' => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    parts.push(current);
    parts
}
