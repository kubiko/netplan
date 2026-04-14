//! `netplan try` – apply config with automatic rollback on timeout or rejection.
//!
//! Mirrors `netplan_cli/cli/commands/try_command.py` and `configmanager.py`.
//!
//! Signal protocol (mirrors Python):
//!   SIGUSR1 → accept   (sent by netplan-dbus Apply() while try is active)
//!   SIGINT  → reject   (sent by netplan-dbus Cancel(), or user Ctrl-C)
//!   SIGTERM → reject   (external kill; dbus also restores YAML independently)

use std::io::{BufRead, Write};
use std::os::raw::c_int;
use std::path::Path;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;


const DEFAULT_TIMEOUT: u64 = 120;

// ── Signal constants (Linux) ──────────────────────────────────────────────────
const SIGINT: c_int = 2;
const SIGTERM: c_int = 15;
const SIGUSR1: c_int = 10;

// Stores the last signal number received (0 = none).
static SIGNAL_RECEIVED: AtomicI32 = AtomicI32::new(0);

unsafe extern "C" fn signal_handler(sig: c_int) {
    SIGNAL_RECEIVED.store(sig, Ordering::SeqCst);
}

extern "C" {
    fn signal(
        signum: c_int,
        handler: Option<unsafe extern "C" fn(c_int)>,
    ) -> Option<unsafe extern "C" fn(c_int)>;
    fn isatty(fd: c_int) -> c_int;
}

// ── termios (Linux x86-64 / aarch64, glibc layout, NCCS=32, sizeof=60) ───────
#[repr(C)]
struct Termios {
    c_iflag: u32,
    c_oflag: u32,
    c_cflag: u32,
    c_lflag: u32,
    c_line:  u8,
    c_cc:    [u8; 32], // NCCS=32 in glibc; c_ispeed follows at 4-byte-aligned offset
    c_ispeed: u32,
    c_ospeed: u32,
}

const TCSANOW: c_int = 0;
const ECHO: u32 = 8; // 0000010 octal

extern "C" {
    fn tcgetattr(fd: c_int, termios_p: *mut Termios) -> c_int;
    fn tcsetattr(fd: c_int, optional_actions: c_int, termios_p: *const Termios) -> c_int;
}

// ── Args ──────────────────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct TryArgs {
    /// Apply this config file in addition to the current configuration
    #[arg(long)]
    config_file: Option<String>,

    /// Seconds to wait for user confirmation before reverting (default 120)
    #[arg(long, default_value_t = DEFAULT_TIMEOUT)]
    timeout: u64,

    /// Directory containing previous YAML configuration (for virtual link cleanup)
    #[arg(long)]
    state: Option<String>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run(args: TryArgs) -> Result<()> {
    let rootdir = std::env::var("DBUS_TEST_NETPLAN_ROOT").unwrap_or_else(|_| "/".to_string());
    let stamp = format!("{}run/netplan/netplan-try.ready", rootdir.trim_end_matches('/').to_string() + "/");

    // ── Validate config is parseable ──────────────────────────────────────────
    if let Err(e) = crate::netplan::load_state(&rootdir) {
        eprintln!("[netplan] Configuration error: {}", e);
        std::process::exit(78); // EX_CONFIG
    }
    if let Some(ref cf) = args.config_file {
        // Validate the extra file too
        if let Err(e) = validate_extra_config(&rootdir, cf) {
            eprintln!("[netplan] Configuration error: {}", e);
            std::process::exit(78);
        }
    }

    // ── Save terminal state, install signal handlers ──────────────────────────
    let term = TermState::save(0 /* stdin fd */);
    install_signals();

    // ── Backup ────────────────────────────────────────────────────────────────
    let backup_dir = make_tempdir("netplan-try-backup-")?;
    let _backup_cleanup = DirCleanup(&backup_dir);

    backup(&rootdir, args.config_file.is_some(), &backup_dir)
        .context("Failed to back up configuration")?;

    // ── Setup: copy --config-file if given ────────────────────────────────────
    let extra_file_dest = if let Some(ref cf) = args.config_file {
        Some(install_config_file(cf)?)
    } else {
        None
    };

    // ── Apply the new configuration ───────────────────────────────────────────
    let mut apply_args: Vec<&str> = vec!["apply"];
    // Reborrow so the &str lives long enough
    let state_ref: Option<&str> = args.state.as_deref();
    if let Some(s) = state_ref {
        apply_args.push("--state");
        apply_args.push(s);
    }
    run_self(&apply_args)?;

    // ── Touch the ready stamp (signals netplan-dbus we're waiting) ────────────
    touch_stamp(&stamp)?;
    let _stamp_cleanup = StampCleanup(&stamp);

    // ── Wait for confirmation ─────────────────────────────────────────────────
    let result = wait_for_confirmation(args.timeout);

    // ── Handle outcome ────────────────────────────────────────────────────────
    term.restore(0);

    match result {
        Outcome::Accepted => {
            println!("\nConfiguration accepted.");
        }
        Outcome::Rejected => {
            println!("\nReverting.");
            if let Err(e) = revert(&rootdir, &backup_dir, &stamp, extra_file_dest.as_deref()) {
                eprintln!("[netplan] Revert failed: {}", e);
            }
        }
    }

    Ok(())
}

// ── Confirmation wait loop ────────────────────────────────────────────────────

enum Outcome {
    Accepted,
    Rejected,
}

fn wait_for_confirmation(timeout: u64) -> Outcome {
    println!("Do you want to keep these settings?\n");
    println!("Press ENTER before the timeout to accept the new configuration\n");

    let width = timeout.to_string().len();

    // Spawn a thread to do a blocking stdin read (waiting for ENTER)
    let (tx, rx) = std::sync::mpsc::channel::<bool>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut line = String::new();
        // A successful read_line means the user pressed Enter (or EOF)
        let _ = stdin.lock().read_line(&mut line);
        let _ = tx.send(true);
    });

    for remaining in (0..=timeout).rev() {
        print!("Changes will revert in {:>width$} seconds\r", remaining, width = width);
        std::io::stdout().flush().ok();

        // Check for ENTER (wait up to 1 second)
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(_) => return Outcome::Accepted,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // stdin closed
                return Outcome::Rejected;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }

        // Check for signals
        let sig = SIGNAL_RECEIVED.swap(0, Ordering::SeqCst);
        if sig == SIGUSR1 {
            return Outcome::Accepted;
        }
        if sig == SIGINT || sig == SIGTERM {
            return Outcome::Rejected;
        }
    }

    // Countdown reached zero
    Outcome::Rejected
}

// ── Revert ────────────────────────────────────────────────────────────────────

fn revert(
    rootdir: &str,
    backup_dir: &str,
    stamp: &str,
    extra_file_dest: Option<&str>,
) -> Result<()> {
    // 1. Save current /etc/netplan as the "tried state" for apply --state
    let state_dir = make_tempdir("netplan-revert-state-")?;
    let _state_cleanup = DirCleanup(&state_dir);

    let etc_src = format!("{}etc/netplan", rootdir.trim_end_matches('/').to_string() + "/");
    let etc_dst = format!("{}/etc/netplan", state_dir);
    std::fs::create_dir_all(&etc_dst)?;
    copy_tree(&etc_src, &etc_dst, true)?;

    // 2. Remove the --config-file copy from /etc/netplan (if present)
    if let Some(dest) = extra_file_dest {
        let _ = std::fs::remove_file(dest);
    }

    // 3. Restore backed-up run directories
    restore_run_dirs(rootdir, backup_dir)?;

    // 4. Clear stamp so that apply → generate doesn't refuse to run
    clear_stamp(stamp);

    // 5. Re-apply with --state pointing to the tried config (best-effort)
    if let Err(e) = run_self(&["apply", "--state", state_dir.as_str()]) {
        eprintln!("[netplan] Warning: apply during revert failed: {}", e);
    }

    Ok(())
}

// ── Backup / restore ──────────────────────────────────────────────────────────

fn backup(rootdir: &str, include_etc: bool, backup_dir: &str) -> Result<()> {
    let root = rootdir.trim_end_matches('/');

    if include_etc {
        let src = format!("{}/etc/netplan", root);
        let dst = format!("{}/etc/netplan", backup_dir);
        std::fs::create_dir_all(&dst)?;
        copy_tree(&src, &dst, true)?;
    }

    let run_dirs = [
        ("run/NetworkManager/system-connections", "run/NetworkManager/system-connections"),
        ("run/systemd/network", "run/systemd/network"),
    ];

    for (rel_src, rel_dst) in &run_dirs {
        let src = format!("{}/{}", root, rel_src);
        let dst = format!("{}/{}", backup_dir, rel_dst);
        if Path::new(&src).exists() {
            std::fs::create_dir_all(&dst)?;
            copy_tree(&src, &dst, true)?;
        }
    }

    Ok(())
}

fn restore_run_dirs(rootdir: &str, backup_dir: &str) -> Result<()> {
    let root = rootdir.trim_end_matches('/');

    let run_dirs = [
        "run/NetworkManager/system-connections",
        "run/systemd/network",
    ];

    for rel in &run_dirs {
        let backup_src = format!("{}/{}", backup_dir, rel);
        let live_dst = format!("{}/{}", root, rel);

        if Path::new(&backup_src).exists() {
            // Remove current state and restore from backup
            let _ = std::fs::remove_dir_all(&live_dst);
            std::fs::create_dir_all(&live_dst)?;
            copy_tree(&backup_src, &live_dst, true)?;
        }
    }

    Ok(())
}

// ── Config file installation ──────────────────────────────────────────────────

fn install_config_file(config_file: &str) -> Result<String> {
    let dest_dir = "/etc/netplan";
    let base = Path::new(config_file)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("netplan-try-extra");
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let dest = format!("{}/{}.{:.0}.yaml", dest_dir, base, ts);
    std::fs::copy(config_file, &dest)
        .with_context(|| format!("Failed to copy {} to {}", config_file, dest))?;
    Ok(dest)
}

fn validate_extra_config(rootdir: &str, config_file: &str) -> Result<()> {
    // Parse the full hierarchy plus the extra file to check validity
    let mut parser = crate::netplan::Parser::new()?;
    parser.load_yaml_hierarchy(rootdir)?;
    parser.load_yaml_file(config_file)?;
    let mut state = crate::netplan::State::new()?;
    state.import_parser_results(&mut parser)?;
    Ok(())
}

// ── Stamp file ────────────────────────────────────────────────────────────────

fn touch_stamp(path: &str) -> Result<()> {
    let p = Path::new(path);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, b"").context("Failed to create try-ready stamp")?;
    Ok(())
}

fn clear_stamp(path: &str) {
    let _ = std::fs::remove_file(path);
}

// ── Run self as subprocess ────────────────────────────────────────────────────

fn run_self(args: &[&str]) -> Result<()> {
    let exe = std::env::current_exe().context("Cannot determine current executable path")?;
    let status = std::process::Command::new(&exe)
        .args(args)
        .status()
        .with_context(|| format!("Failed to exec {:?}", exe))?;
    let rc = status.code().unwrap_or(1);
    if rc != 0 {
        anyhow::bail!("'netplan {}' exited with code {}", args.first().unwrap_or(&"?"), rc);
    }
    Ok(())
}

// ── Tree copy ─────────────────────────────────────────────────────────────────

/// Recursively copy `src` directory into `dst` (which must already exist).
/// When `missing_ok` is true, a non-existent `src` is silently ignored.
fn copy_tree(src: &str, dst: &str, missing_ok: bool) -> Result<()> {
    let src_path = Path::new(src);
    if !src_path.exists() {
        if missing_ok {
            return Ok(());
        }
        anyhow::bail!("Source directory does not exist: {}", src);
    }

    for entry in walkdir(src_path) {
        let (entry_path, metadata) = entry;
        let relative = entry_path.strip_prefix(src_path).unwrap_or(&entry_path);
        let dest = Path::new(dst).join(relative);

        if metadata.is_dir() {
            std::fs::create_dir_all(&dest)?;
        } else {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&entry_path, &dest)
                .with_context(|| format!("Failed to copy {:?} → {:?}", entry_path, dest))?;
            // Preserve permissions
            let _ = std::fs::set_permissions(&dest, metadata.permissions());
        }
    }
    Ok(())
}

/// Minimal recursive directory walker returning (path, metadata) pairs.
fn walkdir(dir: &Path) -> Vec<(std::path::PathBuf, std::fs::Metadata)> {
    let mut result = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return result };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        result.push((entry.path(), meta.clone()));
        if meta.is_dir() {
            result.extend(walkdir(&entry.path()));
        }
    }
    result
}

// ── Temp directory ────────────────────────────────────────────────────────────

fn make_tempdir(prefix: &str) -> Result<String> {
    let pid = std::process::id();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let path = format!("/tmp/{}{}-{}", prefix, pid, ts);
    std::fs::create_dir_all(&path).context("Failed to create temp directory")?;
    Ok(path)
}

// ── RAII guards ───────────────────────────────────────────────────────────────

struct DirCleanup<'a>(&'a str);
impl Drop for DirCleanup<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0);
    }
}

struct StampCleanup<'a>(&'a str);
impl Drop for StampCleanup<'_> {
    fn drop(&mut self) {
        clear_stamp(self.0);
    }
}

// ── Terminal state ────────────────────────────────────────────────────────────

struct TermState {
    is_tty: bool,
    orig: Termios,
}

impl TermState {
    /// Save current terminal attributes for `fd`.
    fn save(fd: c_int) -> Self {
        let is_tty = unsafe { isatty(fd) } != 0;
        let mut orig = Termios {
            c_iflag: 0, c_oflag: 0, c_cflag: 0, c_lflag: 0,
            c_line: 0, c_cc: [0; 32], c_ispeed: 0, c_ospeed: 0,
        };
        if is_tty {
            unsafe { tcgetattr(fd, &mut orig) };
            // Disable ECHO so the countdown isn't cluttered by keystrokes
            let new_attrs = Termios {
                c_iflag: orig.c_iflag,
                c_oflag: orig.c_oflag,
                c_cflag: orig.c_cflag,
                c_lflag: orig.c_lflag & !ECHO,
                c_line: orig.c_line,
                c_cc: orig.c_cc,
                c_ispeed: orig.c_ispeed,
                c_ospeed: orig.c_ospeed,
            };
            unsafe { tcsetattr(fd, TCSANOW, &new_attrs) };
        }
        TermState { is_tty, orig }
    }

    /// Restore saved terminal attributes for `fd`.
    fn restore(&self, fd: c_int) {
        if self.is_tty {
            unsafe { tcsetattr(fd, TCSANOW, &self.orig) };
        }
    }
}

// ── Signal installation ───────────────────────────────────────────────────────

fn install_signals() {
    unsafe {
        signal(SIGINT,  Some(signal_handler));
        signal(SIGUSR1, Some(signal_handler));
        signal(SIGTERM, Some(signal_handler));
    }
}
