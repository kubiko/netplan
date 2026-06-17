// Copyright (C) 2026 Canonical, Ltd.
// SPDX-License-Identifier: GPL-3.0-only
//
// This program is free software; you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation; version 3.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! netplan – Rust CLI replacing the Python netplan_cli for the core commands:
//! apply / generate / get / set / info / ip / try.

use std::process::ExitCode;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};

mod commands;
mod ffi;
mod netplan;
mod utils;
mod yaml;

use self::commands::{apply, generate, get, info, ip, migrate, set, status, try_command};
use self::commands::{
    apply::ApplyArgs, generate::GenerateArgs, get::GetArgs, info::InfoArgs, ip::IpArgs,
    migrate::MigrateArgs, set::SetArgs, status::StatusArgs, try_command::TryArgs,
};

// ── CLI skeleton ──────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "netplan", about = "Network configuration in YAML", version)]
struct Cli {
    /// Enable verbose debug output
    #[arg(long, global = true)]
    debug: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Apply current netplan config to running system
    Apply(ApplyArgs),
    /// Generate backend specific configuration files from /etc/netplan/*.yaml
    Generate(GenerateArgs),
    /// Get a setting by specifying a nested key like "ethernets.eth0.addresses", or "all"
    Get(GetArgs),
    /// Retrieve IP information from the system
    Ip(IpArgs),
    /// Migration of /etc/network/interfaces to netplan
    Migrate(MigrateArgs),
    /// Add/update/delete a setting via a dotted key=value pair
    Set(SetArgs),
    /// Show available features
    Info(InfoArgs),
    /// Query networking state of the running system
    Status(StatusArgs),
    /// Try to apply a new netplan config with automatic rollback on timeout or rejection
    Try(TryArgs),
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() -> ExitCode {
    env_logger::init();

    // Match Python CLI environment setup.
    // SAFETY: called once, single-threaded, at the very start of main, before
    // any other code reads the environment.
    unsafe {
        std::env::set_var("LC_ALL", "C.UTF-8");
    }

    // ── Detect systemd-generator calling convention ───────────────────────────
    //
    // When this binary is invoked as a systemd generator (or as `exe_generate`
    // in the test framework), the call looks like:
    //
    //   netplan [--root-dir ROOTDIR] [--ignore-errors] GEN_DIR GEN_EARLY GEN_LATE
    //
    // i.e. there is NO `generate` subcommand keyword.  Detect this by two
    // signals that are specific to the generator calling convention:
    //
    //   1. `--root-dir` is present  (always used in test/dev scenarios)
    //   2. The first non-option token is a filesystem path (starts with `/`),
    //      which is how `systemd` passes the generator output directories on a
    //      real system without --root-dir.
    //
    // Importantly, `netplan --help`, `netplan --version`, and unknown flags like
    // `netplan --foo` must NOT be treated as generator mode; those are handled
    // normally by clap.

    let mut raw_args: Vec<String> = std::env::args().collect();

    // Derive the known subcommand names from clap rather than maintaining a
    // separate hardcoded list.  `--generator-mode` inside GenerateArgs is
    // declared `#[arg(hide = true)]` so it stays invisible to normal users.
    let cli_cmd = Cli::command();
    let known_subcommands: Vec<&str> = cli_cmd.get_subcommands().map(|c| c.get_name()).collect();

    // First non-option argument (if any).
    let first_positional = raw_args.iter().skip(1).find(|a| !a.starts_with('-'));

    let has_known_subcommand = first_positional
        .map(|a| known_subcommands.contains(&a.as_str()))
        .unwrap_or(false);

    // Is this a systemd-generator / test-framework invocation?
    let has_root_dir = raw_args.iter().skip(1).any(|a| a == "--root-dir");
    let first_is_path = first_positional
        .map(|a| a.starts_with('/'))
        .unwrap_or(false);
    let generator_mode = (has_root_dir || first_is_path) && !has_known_subcommand;

    if generator_mode {
        // Inject subcommand and hidden mode flag.
        raw_args.insert(1, "generate".to_string());
        raw_args.insert(2, "--generator-mode".to_string());
    }

    // ── Parse (with C-compatible error messages) ──────────────────────────────
    let cli = match Cli::try_parse_from(&raw_args) {
        Ok(c) => c,
        Err(e) => {
            // Reformat "unexpected argument" errors to match the C binary:
            //   "failed to parse options: Unknown option --foo"
            // so that existing tests keep passing.
            let msg = e.to_string();
            if msg.contains("unexpected argument '") {
                if let Some(arg) = extract_unknown_arg(&msg) {
                    eprintln!("failed to parse options: Unknown option {}", arg);
                    return ExitCode::from(1);
                }
            }
            // All other errors: let clap print its own message and exit.
            e.exit();
        }
    };

    if cli.debug {
        // SAFETY: called once, single-threaded, before any subcommand runs
        // and before any other code reads the environment.
        unsafe {
            std::env::set_var("G_MESSAGES_DEBUG", "all");
        }
        eprintln!("[netplan] debug mode enabled");
    }

    let result: Result<ExitCode> = match cli.command {
        Command::Apply(args) => apply::run(args),
        Command::Generate(args) => generate::run(args),
        Command::Get(args) => get::run(args),
        Command::Ip(args) => ip::run(args),
        Command::Migrate(args) => migrate::run(args),
        Command::Set(args) => set::run(args),
        Command::Status(args) => status::run(args),
        Command::Info(args) => info::run(args),
        Command::Try(args) => try_command::run(args),
    };

    match result {
        Ok(code) => code,
        Err(e) => {
            // {e:?} includes anyhow's full "Caused by" chain, which is more
            // useful for debugging than the plain Display message.
            eprintln!("Command failed: {e:?}");
            ExitCode::from(1)
        }
    }
}

/// Extract the unknown argument name from a clap 4 error string.
///
/// Clap says: `error: unexpected argument '--foo' found\n...`
/// We want:   `--foo`
fn extract_unknown_arg(clap_error: &str) -> Option<String> {
    let prefix = "unexpected argument '";
    let start = clap_error.find(prefix)? + prefix.len();
    let rest = &clap_error[start..];
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}
