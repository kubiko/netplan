use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());

    // Emit every candidate search path we can find, from most to least specific.
    // The linker uses the first directory that contains the library.
    emit_link_search_paths(&manifest_dir);

    println!("cargo:rustc-link-lib=netplan");

    // Re-run if any source C/H files change (feature flag extraction)
    let src_dir = manifest_dir.parent().unwrap().join("src");
    println!("cargo:rerun-if-changed={}", src_dir.display());

    // Extract feature flags from /* netplan-feature: <name> */ annotations
    // in src/*.{h,c}, mirroring features_py_generator.sh logic.
    let features = extract_features(&src_dir);

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let code = format!(
        "pub const FEATURE_FLAGS: &[&str] = &[{}];\n",
        features
            .iter()
            .map(|f| format!("\"{}\"", f))
            .collect::<Vec<_>>()
            .join(", ")
    );
    std::fs::write(out_dir.join("features.rs"), code).expect("write features.rs");
}

/// Locate `libnetplan.so*` and ensure the linker can find `-lnetplan`.
///
/// The linker requires an *unversioned* `libnetplan.so` symlink (normally
/// provided by the `-dev` package).  When only a versioned file exists
/// (e.g. `libnetplan.so.1` from the runtime package or a meson build), we
/// create a `libnetplan.so` symlink inside `OUT_DIR` and add that to the
/// search path.
fn emit_link_search_paths(manifest_dir: &PathBuf) {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    // 1. NETPLAN_LIB_DIR env var — explicit override.
    if let Ok(dir) = std::env::var("NETPLAN_LIB_DIR") {
        ensure_unversioned_symlink(&PathBuf::from(&dir), &out_dir);
        println!("cargo:rustc-link-search=native={}", dir);
        println!("cargo:rustc-link-search=native={}", out_dir.display());
        return;
    }

    // 2. pkg-config — authoritative when libnetplan-dev is installed or
    //    PKG_CONFIG_PATH points at the meson build's uninstalled pc files.
    for pc_name in &["netplan", "libnetplan"] {
        if let Some(dir) = pkg_config_libdir(pc_name) {
            let dir = PathBuf::from(&dir);
            ensure_unversioned_symlink(&dir, &out_dir);
            println!("cargo:rustc-link-search=native={}", dir.display());
            println!("cargo:rustc-link-search=native={}", out_dir.display());
            return;
        }
    }

    // 3. Walk every candidate directory.  Stop at the first one that holds
    //    any `libnetplan.so*` file and make the symlink if needed.
    for dir in candidate_dirs(manifest_dir) {
        if let Some(lib) = find_libnetplan_file(&dir) {
            if lib.file_name().and_then(|n| n.to_str()) == Some("libnetplan.so") {
                // Unversioned file already present — dir is ready to use.
                println!("cargo:rustc-link-search=native={}", dir.display());
            } else {
                // Only a versioned file; create a libnetplan.so symlink.
                make_symlink(&lib, &out_dir.join("libnetplan.so"));
                println!("cargo:rustc-link-search=native={}", out_dir.display());
            }
            return;
        }
    }

    // 4. Nothing found — emit all standard paths and hope for the best.
    for dir in &[
        "/usr/lib/aarch64-linux-gnu",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib/arm-linux-gnueabihf",
        "/usr/local/lib",
        "/usr/lib",
    ] {
        println!("cargo:rustc-link-search=native={}", dir);
    }
}

/// If `dir` contains only a versioned `libnetplan.so.X` (no plain `.so`),
/// create a `libnetplan.so` symlink in `link_dir`.
fn ensure_unversioned_symlink(dir: &PathBuf, link_dir: &PathBuf) {
    if dir.join("libnetplan.so").exists() {
        return; // already have an unversioned file
    }
    if let Some(versioned) = find_libnetplan_file(dir) {
        make_symlink(&versioned, &link_dir.join("libnetplan.so"));
    }
}

/// Create (or replace) a symlink at `link` pointing to `target`.
fn make_symlink(target: &PathBuf, link: &PathBuf) {
    let _ = std::fs::remove_file(link);
    let _ = std::os::unix::fs::symlink(target, link);
}

/// Collect all directories worth searching, meson build tree first.
fn candidate_dirs(manifest_dir: &PathBuf) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    // Meson / cmake build trees inside the parent repo directory.
    if let Some(repo_root) = manifest_dir.parent() {
        for bd_name in &["build", "builddir", "_build", ".build", "out", "obj",
                         "debug", "release"] {
            let bd = repo_root.join(bd_name);
            if !bd.is_dir() { continue; }
            for sub in &["src", "lib", ""] {
                let dir = if sub.is_empty() { bd.clone() } else { bd.join(sub) };
                if dir.is_dir() { dirs.push(dir); }
            }
        }
    }

    // Standard Ubuntu/Debian multiarch paths.
    for p in &[
        "/usr/lib/aarch64-linux-gnu",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib/arm-linux-gnueabihf",
        "/usr/local/lib",
        "/usr/lib",
    ] {
        dirs.push(PathBuf::from(p));
    }

    dirs
}

/// Return the path to any `libnetplan.so*` file in `dir`, preferring the
/// unversioned name.  Returns `None` if no such file exists.
fn find_libnetplan_file(dir: &PathBuf) -> Option<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return None };
    let mut versioned: Option<PathBuf> = None;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("libnetplan.so") { continue; }
        let path = entry.path();
        if name == "libnetplan.so" {
            return Some(path); // exact match — no need to look further
        }
        if versioned.is_none() {
            versioned = Some(path);
        }
    }
    versioned
}

/// Query `pkg-config --variable=libdir <name>` and return the library directory,
/// or `None` if pkg-config is unavailable or the package is unknown.
fn pkg_config_libdir(name: &str) -> Option<String> {
    let out = std::process::Command::new("pkg-config")
        .args(["--variable=libdir", name])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8(out.stdout).ok()?;
    let path = path.trim().to_string();
    if path.is_empty() { None } else { Some(path) }
}

/// Parse `src/*.{h,c}` (excluding `_`-prefixed files) for lines containing
/// `netplan-feature: <name>` and return a deduplicated, ordered list of names.
fn extract_features(src_dir: &std::path::Path) -> Vec<String> {
    let mut features: Vec<String> = Vec::new();

    let dir = match std::fs::read_dir(src_dir) {
        Ok(d) => d,
        Err(_) => return features,
    };

    let mut paths: Vec<_> = dir
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            !name.starts_with('_')
                && (name.ends_with(".h") || name.ends_with(".c"))
        })
        .collect();
    // Sort for deterministic output
    paths.sort();

    for path in paths {
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for line in content.lines() {
            if let Some(pos) = line.find("netplan-feature:") {
                let rest = &line[pos + "netplan-feature:".len()..];
                if let Some(name) = rest.split_whitespace().next() {
                    // Skip if first char is not alphabetic (e.g. stray "*/" tokens)
                    if name.starts_with(|c: char| c.is_ascii_alphabetic()) {
                        let name = name.to_string();
                        if !features.contains(&name) {
                            features.push(name);
                        }
                    }
                }
            }
        }
    }

    features
}
