//! Safe wrappers around the raw libnetplan C API (crate::ffi).
//!
//! Objects follow RAII: `Parser` and `State` call their respective `_clear`
//! functions on drop.  All methods return `anyhow::Result` and convert C error
//! pointers into Rust errors.

use std::ffi::CString;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::raw::{c_char, c_int};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

use anyhow::{bail, Result};

use crate::ffi;

// ── In-memory file descriptor ─────────────────────────────────────────────────

/// Create an anonymous in-memory file via `memfd_create(2)`.
/// The returned `File` can be read, written and seeked like a regular file.
pub fn memfd(name: &str) -> Result<std::fs::File> {
    let cname = CString::new(name)?;
    let fd: c_int = unsafe { ffi::memfd_create(cname.as_ptr(), 0) };
    if fd < 0 {
        bail!(
            "memfd_create({:?}) failed: {}",
            name,
            std::io::Error::last_os_error()
        );
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

// ── Error extraction ──────────────────────────────────────────────────────────

/// Drain an error pointer into a `String` and free the underlying `GError`.
///
/// # Safety
/// `error` must be a valid (possibly null) `*mut NetplanError`.  After this
/// call `*error` is `NULL`.
pub unsafe fn drain_error(error: *mut ffi::NetplanError) -> String {
    if error.is_null() {
        return "unknown libnetplan error".to_string();
    }
    let mut buf = vec![0u8; 2048];
    let n = ffi::netplan_error_message(error, buf.as_mut_ptr() as *mut c_char, buf.len());
    // netplan_error_clear takes **NetplanError; we pass a pointer to a local copy
    let mut p = error;
    ffi::netplan_error_clear(&mut p);
    if n > 1 {
        let n = (n as usize - 1).min(buf.len()); // exclude NUL terminator
        String::from_utf8_lossy(&buf[..n]).into_owned()
    } else {
        "unknown libnetplan error".to_string()
    }
}

// ── Parser ────────────────────────────────────────────────────────────────────

/// Safe wrapper around `NetplanParser *`.
pub struct Parser(*mut ffi::NetplanParser);

impl Parser {
    pub fn new() -> Result<Self> {
        let p = unsafe { ffi::netplan_parser_new() };
        if p.is_null() {
            bail!("netplan_parser_new returned NULL");
        }
        Ok(Self(p))
    }

    #[inline]
    pub(crate) fn as_ptr(&mut self) -> *mut ffi::NetplanParser {
        self.0
    }

    /// Parse the full `/etc/netplan`, `/run/netplan`, `/usr/lib/netplan`
    /// hierarchy under `rootdir`.  Pass `"/"` for the real system root.
    pub fn load_yaml_hierarchy(&mut self, rootdir: &str) -> Result<()> {
        let c = CString::new(rootdir)?;
        let mut err: *mut ffi::NetplanError = std::ptr::null_mut();
        let ok = unsafe {
            ffi::netplan_parser_load_yaml_hierarchy(self.0, c.as_ptr(), &mut err)
        };
        if ok == 0 {
            bail!("{}", unsafe { drain_error(err) });
        }
        Ok(())
    }

    /// Parse a YAML file given by its absolute path.
    pub fn load_yaml_file(&mut self, path: &str) -> Result<()> {
        let c = CString::new(path)?;
        let mut err: *mut ffi::NetplanError = std::ptr::null_mut();
        let ok =
            unsafe { ffi::netplan_parser_load_yaml(self.0, c.as_ptr(), &mut err) };
        if ok == 0 {
            bail!("{}", unsafe { drain_error(err) });
        }
        Ok(())
    }

    /// Parse YAML from an already-opened file descriptor.
    /// The caller must seek `fd` to the desired start position before calling.
    pub fn load_yaml_from_fd(&mut self, fd: RawFd) -> Result<()> {
        let mut err: *mut ffi::NetplanError = std::ptr::null_mut();
        let ok = unsafe { ffi::netplan_parser_load_yaml_from_fd(self.0, fd, &mut err) };
        if ok == 0 {
            bail!("{}", unsafe { drain_error(err) });
        }
        Ok(())
    }

    /// Mark fields in the provided FD as nullable (to-be-deleted).
    /// Seek `fd` to position 0 before calling.
    pub fn load_nullable_fields(&mut self, fd: RawFd) -> Result<()> {
        let mut err: *mut ffi::NetplanError = std::ptr::null_mut();
        let ok =
            unsafe { ffi::netplan_parser_load_nullable_fields(self.0, fd, &mut err) };
        if ok == 0 {
            bail!("{}", unsafe { drain_error(err) });
        }
        Ok(())
    }

    /// Mark netdefs / globals as nullable overrides constrained to `filename`.
    /// Seek `fd` to position 0 before calling.
    pub fn load_nullable_overrides(&mut self, fd: RawFd, filename: &str) -> Result<()> {
        let c = CString::new(filename)?;
        let mut err: *mut ffi::NetplanError = std::ptr::null_mut();
        let ok = unsafe {
            ffi::netplan_parser_load_nullable_overrides(
                self.0,
                fd,
                c.as_ptr(),
                &mut err,
            )
        };
        if ok == 0 {
            bail!("{}", unsafe { drain_error(err) });
        }
        Ok(())
    }
}

impl Drop for Parser {
    fn drop(&mut self) {
        unsafe { ffi::netplan_parser_clear(&mut self.0) };
    }
}

// ── State ─────────────────────────────────────────────────────────────────────

/// Safe wrapper around `NetplanState *`.
pub struct State(*mut ffi::NetplanState);

impl State {
    pub fn new() -> Result<Self> {
        let s = unsafe { ffi::netplan_state_new() };
        if s.is_null() {
            bail!("netplan_state_new returned NULL");
        }
        Ok(Self(s))
    }

    /// Validate the parser contents and transfer ownership into this state.
    pub fn import_parser_results(&mut self, parser: &mut Parser) -> Result<()> {
        let mut err: *mut ffi::NetplanError = std::ptr::null_mut();
        let ok = unsafe {
            ffi::netplan_state_import_parser_results(self.0, parser.as_ptr(), &mut err)
        };
        if ok == 0 {
            bail!("{}", unsafe { drain_error(err) });
        }
        Ok(())
    }

    /// Serialise the entire state to YAML and return it as a `String`.
    pub fn dump_yaml(&self) -> Result<String> {
        let mut tmp = memfd("np-dump")?;
        let fd = tmp.as_raw_fd();
        let mut err: *mut ffi::NetplanError = std::ptr::null_mut();
        let ok = unsafe { ffi::netplan_state_dump_yaml(self.0, fd, &mut err) };
        if ok == 0 {
            bail!("{}", unsafe { drain_error(err) });
        }
        tmp.seek(SeekFrom::Start(0))?;
        let mut out = String::new();
        tmp.read_to_string(&mut out)?;
        Ok(out)
    }

    /// Write the YAML for a single origin file (`filename`) under `rootdir`.
    pub fn write_yaml_file(&self, filename: &str, rootdir: &str) -> Result<()> {
        let cf = CString::new(filename)?;
        let cr = CString::new(rootdir)?;
        let mut err: *mut ffi::NetplanError = std::ptr::null_mut();
        let ok = unsafe {
            ffi::netplan_state_write_yaml_file(
                self.0,
                cf.as_ptr(),
                cr.as_ptr(),
                &mut err,
            )
        };
        if ok == 0 {
            bail!("{}", unsafe { drain_error(err) });
        }
        Ok(())
    }

    /// Update all origin YAML files; data without an origin goes to
    /// `default_filename` under `rootdir`.
    pub fn update_yaml_hierarchy(&self, default_filename: &str, rootdir: &str) -> Result<()> {
        let cf = CString::new(default_filename)?;
        let cr = CString::new(rootdir)?;
        let mut err: *mut ffi::NetplanError = std::ptr::null_mut();
        let ok = unsafe {
            ffi::netplan_state_update_yaml_hierarchy(
                self.0,
                cf.as_ptr(),
                cr.as_ptr(),
                &mut err,
            )
        };
        if ok == 0 {
            bail!("{}", unsafe { drain_error(err) });
        }
        Ok(())
    }

    /// Iterate over all `NetDefinition` objects in this state.
    ///
    /// The returned iterator borrows `self` for the duration of iteration.
    pub fn iter_netdefs(&self) -> NetDefIter {
        let mut iter = ffi::NetplanStateIterator {
            placeholder: std::ptr::null_mut(),
        };
        unsafe { ffi::netplan_state_iterator_init(self.0, &mut iter) };
        NetDefIter { iter }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        unsafe { ffi::netplan_state_clear(&mut self.0) };
    }
}

// ── NetDefIter ────────────────────────────────────────────────────────────────

/// Iterator over `NetDef` references inside a `State`.
///
/// The pointed-to objects are owned by the `State`; do not outlive it.
pub struct NetDefIter {
    iter: ffi::NetplanStateIterator,
}

impl Iterator for NetDefIter {
    type Item = NetDef;

    fn next(&mut self) -> Option<Self::Item> {
        if unsafe { ffi::netplan_state_iterator_has_next(&mut self.iter) } == 0 {
            return None;
        }
        let ptr = unsafe { ffi::netplan_state_iterator_next(&mut self.iter) };
        if ptr.is_null() { None } else { Some(NetDef(ptr)) }
    }
}

// ── NetDef ────────────────────────────────────────────────────────────────────

/// A view into a single network definition inside a `State`.
///
/// The underlying memory is owned by the `State`; do not outlive it.
pub struct NetDef(*mut ffi::NetplanNetDefinition);

impl NetDef {
    pub fn def_type(&self) -> ffi::NetplanDefType {
        unsafe { ffi::netplan_netdef_get_type(self.0) }
    }

    /// Returns `true` for ethernet / wifi / modem (physical) types.
    pub fn is_physical(&self) -> bool {
        matches!(
            self.def_type(),
            ffi::NETPLAN_DEF_TYPE_ETHERNET
                | ffi::NETPLAN_DEF_TYPE_WIFI
                | ffi::NETPLAN_DEF_TYPE_MODEM
        )
    }

    /// Returns `true` for virtual interface types (bridge, bond, vlan, …).
    pub fn is_virtual(&self) -> bool {
        matches!(
            self.def_type(),
            ffi::NETPLAN_DEF_TYPE_BRIDGE   // = VIRTUAL
                | ffi::NETPLAN_DEF_TYPE_BOND
                | ffi::NETPLAN_DEF_TYPE_VLAN
                | ffi::NETPLAN_DEF_TYPE_TUNNEL
                | ffi::NETPLAN_DEF_TYPE_PORT
                | ffi::NETPLAN_DEF_TYPE_VRF
                | ffi::NETPLAN_DEF_TYPE_NM
                | ffi::NETPLAN_DEF_TYPE_DUMMY
                | ffi::NETPLAN_DEF_TYPE_VETH
        )
    }

    /// The Netplan ID string (equals interface name for virtual interfaces).
    pub fn id(&self) -> String {
        read_string_buf(|buf, len| unsafe {
            ffi::netplan_netdef_get_id(self.0, buf, len)
        })
    }

    /// The `set-name` value, or `None` if not configured.
    pub fn set_name(&self) -> Option<String> {
        let s = read_string_buf(|buf, len| unsafe {
            ffi::netplan_netdef_get_set_name(self.0, buf, len)
        });
        if s.is_empty() { None } else { Some(s) }
    }

    /// `true` if the netdef contains a `match:` stanza.
    pub fn has_match(&self) -> bool {
        unsafe { ffi::netplan_netdef_has_match(self.0) != 0 }
    }

    pub fn backend(&self) -> ffi::NetplanBackend {
        unsafe { ffi::netplan_netdef_get_backend(self.0) }
    }

    pub fn backend_name(&self) -> &'static str {
        match self.backend() {
            ffi::NETPLAN_BACKEND_NETWORKD => "networkd",
            ffi::NETPLAN_BACKEND_NM       => "NetworkManager",
            ffi::NETPLAN_BACKEND_OVS      => "openvswitch",
            _                             => "none",
        }
    }

    pub fn dhcp4(&self) -> bool {
        unsafe { ffi::netplan_netdef_get_dhcp4(self.0) != 0 }
    }

    pub fn dhcp6(&self) -> bool {
        unsafe { ffi::netplan_netdef_get_dhcp6(self.0) != 0 }
    }

    pub fn link_local_ipv4(&self) -> bool {
        unsafe { ffi::netplan_netdef_get_link_local_ipv4(self.0) != 0 }
    }

    pub fn link_local_ipv6(&self) -> bool {
        unsafe { ffi::netplan_netdef_get_link_local_ipv6(self.0) != 0 }
    }

    /// Returns `None` if not configured, `Some(true)` if enabled, `Some(false)` if disabled.
    pub fn accept_ra(&self) -> Option<bool> {
        match unsafe { ffi::netplan_netdef_get_accept_ra(self.0) } {
            0 => None,
            1 => Some(true),
            _ => Some(false),
        }
    }

    pub fn macaddress(&self) -> Option<String> {
        let s = read_string_buf(|buf, len| unsafe {
            ffi::netplan_netdef_get_macaddress(self.0, buf, len)
        });
        if s.is_empty() { None } else { Some(s) }
    }

    pub fn bridge_link_id(&self) -> Option<String> {
        let ptr = unsafe { ffi::netplan_netdef_get_bridge_link(self.0) };
        if ptr.is_null() { return None; }
        let id = read_string_buf(|buf, len| unsafe {
            ffi::netplan_netdef_get_id(ptr, buf, len)
        });
        if id.is_empty() { None } else { Some(id) }
    }

    pub fn bond_link_id(&self) -> Option<String> {
        let ptr = unsafe { ffi::netplan_netdef_get_bond_link(self.0) };
        if ptr.is_null() { return None; }
        let id = read_string_buf(|buf, len| unsafe {
            ffi::netplan_netdef_get_id(ptr, buf, len)
        });
        if id.is_empty() { None } else { Some(id) }
    }

    pub fn vrf_link_id(&self) -> Option<String> {
        let ptr = unsafe { ffi::netplan_netdef_get_vrf_link(self.0) };
        if ptr.is_null() { return None; }
        let id = read_string_buf(|buf, len| unsafe {
            ffi::netplan_netdef_get_id(ptr, buf, len)
        });
        if id.is_empty() { None } else { Some(id) }
    }

    #[allow(dead_code)]
    pub fn type_str(&self) -> &'static str {
        match self.def_type() {
            ffi::NETPLAN_DEF_TYPE_ETHERNET => "ethernet",
            ffi::NETPLAN_DEF_TYPE_WIFI     => "wifi",
            ffi::NETPLAN_DEF_TYPE_MODEM    => "modem",
            ffi::NETPLAN_DEF_TYPE_BRIDGE   => "bridge",
            ffi::NETPLAN_DEF_TYPE_BOND     => "bond",
            ffi::NETPLAN_DEF_TYPE_VLAN     => "vlan",
            ffi::NETPLAN_DEF_TYPE_TUNNEL   => "tunnel",
            ffi::NETPLAN_DEF_TYPE_VRF      => "vrf",
            ffi::NETPLAN_DEF_TYPE_DUMMY    => "dummy-device",
            ffi::NETPLAN_DEF_TYPE_VETH     => "virtual-ethernet",
            _                              => "other",
        }
    }

    /// Returns `true` if `name`/`mac`/`driver` all satisfy this netdef's
    /// match rules.  `None` arguments are passed as NULL (wildcard).
    pub fn matches_interface(
        &self,
        name: &str,
        mac: Option<&str>,
        driver: Option<&str>,
    ) -> bool {
        let cn = CString::new(name).unwrap_or_default();
        let cm = mac.and_then(|m| CString::new(m).ok());
        let cd = driver.and_then(|d| CString::new(d).ok());
        unsafe {
            ffi::netplan_netdef_match_interface(
                self.0,
                cn.as_ptr(),
                cm.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                cd.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            ) != 0
        }
    }
}

// ── YAML utility functions ────────────────────────────────────────────────────

/// Parse a full netplan config hierarchy and return the validated `State`.
/// Convenience wrapper used by `get` and `apply`.
pub fn load_state(rootdir: &str) -> Result<State> {
    let mut parser = Parser::new()?;
    parser.load_yaml_hierarchy(rootdir)?;
    let mut state = State::new()?;
    state.import_parser_results(&mut parser)?;
    Ok(state)
}

/// Extract a YAML subtree keyed by `prefix_path` from `full_yaml`.
///
/// `prefix_path` is a slice of path components, e.g.
/// `&["network", "ethernets", "eth0"]`.  The TAB-joining and FD plumbing
/// are handled internally.
pub fn dump_yaml_subtree(prefix_path: &[String], full_yaml: &str) -> Result<String> {
    let tab_prefix = prefix_path.join("\t");
    let c_prefix = CString::new(tab_prefix)?;

    // Write full YAML into input memfd
    let mut input = memfd("np-subtree-in")?;
    input.write_all(full_yaml.as_bytes())?;
    input.seek(SeekFrom::Start(0))?;

    let mut output = memfd("np-subtree-out")?;

    let mut err: *mut ffi::NetplanError = std::ptr::null_mut();
    let ok = unsafe {
        ffi::netplan_util_dump_yaml_subtree(
            c_prefix.as_ptr(),
            input.as_raw_fd(),
            output.as_raw_fd(),
            &mut err,
        )
    };
    if ok == 0 {
        bail!("{}", unsafe { drain_error(err) });
    }

    output.seek(SeekFrom::Start(0))?;
    let mut result = String::new();
    output.read_to_string(&mut result)?;
    Ok(result)
}

/// Create a YAML patch document for a `netplan set` expression.
///
/// Returns a seeked-to-start `File` (memfd) containing the patch YAML.
///
/// * `obj_path` – path components, e.g. `["network", "ethernets", "eth0"]`
/// * `payload`  – YAML value, e.g. `"{dhcp4: true}"` or `"NULL"`
pub fn create_yaml_patch(
    obj_path: &[String],
    payload: &str,
) -> Result<std::fs::File> {
    let tab_path = obj_path.join("\t");
    let c_path = CString::new(tab_path)?;
    let c_payload = CString::new(payload)?;

    let tmp = memfd("np-patch")?;
    let fd = tmp.as_raw_fd();

    let mut err: *mut ffi::NetplanError = std::ptr::null_mut();
    let ok = unsafe {
        ffi::netplan_util_create_yaml_patch(
            c_path.as_ptr(),
            c_payload.as_ptr(),
            fd,
            &mut err,
        )
    };
    if ok == 0 {
        bail!("{}", unsafe { drain_error(err) });
    }
    Ok(tmp)
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Call `f(buf_ptr, buf_len)` with a growing buffer until the return value
/// fits, then return the resulting `String`.
///
/// Mirrors `_string_realloc_call_no_error` from the Python CFFI layer.
fn read_string_buf<F>(f: F) -> String
where
    F: Fn(*mut c_char, usize) -> isize,
{
    let mut buf = vec![0u8; 256];
    loop {
        let n = f(buf.as_mut_ptr() as *mut c_char, buf.len());
        if n == ffi::NETPLAN_BUFFER_TOO_SMALL {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if n <= 0 {
            return String::new();
        }
        let content_len = (n as usize - 1).min(buf.len()); // exclude NUL
        return String::from_utf8_lossy(&buf[..content_len]).into_owned();
    }
}
