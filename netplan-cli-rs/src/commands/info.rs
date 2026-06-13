//! `netplan info` – show available libnetplan features.
//!
//! Feature flags are extracted from `src/*.{h,c}` at build time by `build.rs`
//! and embedded as a `&[&str]` constant, mirroring `_features.py`.

use std::process::ExitCode;

use anyhow::Result;
use clap::{Args, ValueEnum};
use serde_json::json;

// Feature flags generated at build time from /* netplan-feature: <name> */
// annotations in the C source tree.
mod features {
    include!(concat!(env!("OUT_DIR"), "/features.rs"));
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Format {
    Yaml,
    Json,
}

#[derive(Args, Debug)]
pub struct InfoArgs {
    /// Output in JSON format
    #[arg(long, conflicts_with = "yaml")]
    json: bool,

    /// Output in YAML format (default)
    #[arg(long, conflicts_with = "json")]
    yaml: bool,
}

pub fn run(args: InfoArgs) -> Result<ExitCode> {
    let flags = features::FEATURE_FLAGS;
    let website = "https://netplan.io/";

    if args.json {
        // Produce output equivalent to json.dumps(…, indent=2)
        let value = json!({
            "netplan.io": {
                "website": website,
                "features": flags,
            }
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        // YAML (default) – matches Python's hand-formatted output exactly
        println!("netplan.io:");
        println!("  website: \"{}\"", website);
        println!("  features:");
        for f in flags {
            println!("  - {}", f);
        }
    }
    Ok(ExitCode::SUCCESS)
}
