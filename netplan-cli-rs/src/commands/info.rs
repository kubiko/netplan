//! `netplan info` – show available libnetplan features.
//!
//! Feature flags are extracted from `src/*.{h,c}` at build time by `build.rs`
//! and embedded as a `&[&str]` constant, mirroring `_features.py`.

use anyhow::Result;
use clap::{Args, ValueEnum};

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

pub fn run(args: InfoArgs) -> Result<()> {
    let flags = features::FEATURE_FLAGS;
    let website = "https://netplan.io/";

    if args.json {
        // Produce output equivalent to json.dumps(…, indent=2)
        let features_json = flags
            .iter()
            .map(|f| format!("      \"{}\"", f))
            .collect::<Vec<_>>()
            .join(",\n");
        println!(
            "{{\n  \"netplan.io\": {{\n    \"website\": \"{}\",\n    \"features\": [\n{}\n    ]\n  }}\n}}",
            website, features_json
        );
    } else {
        // YAML (default) – matches Python's hand-formatted output exactly
        println!("netplan.io:");
        println!("  website: \"{}\"", website);
        println!("  features:");
        for f in flags {
            println!("  - {}", f);
        }
    }
    Ok(())
}
