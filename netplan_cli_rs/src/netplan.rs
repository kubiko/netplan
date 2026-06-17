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

//! Safe wrappers around the raw libnetplan C API (crate::ffi).
//!
//! Objects follow RAII: `Parser` and `State` call their respective `_clear`
//! functions on drop.  All methods return `anyhow::Result` and convert C error
//! pointers into Rust errors.

use std::ffi::CString;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::raw::c_char;
use std::os::unix::io::AsRawFd;

use anyhow::{anyhow, Context, Result};
use nix::sys::memfd::{memfd_create, MFdFlags};

use crate::ffi;

// ── In-memory file descriptor ─────────────────────────────────────────────────

/// Create an anonymous in-memory file via `memfd_create(2)`.
/// The returned `File` can be read, written and seeked like a regular file.
pub fn memfd(name: &str) -> Result<std::fs::File> {
    let fd = memfd_create(name, MFdFlags::empty())
        .with_context(|| format!("memfd_create({:?}) failed", name))?;
    Ok(std::fs::File::from(fd))
}

// ── Error extraction ──────────────────────────────────────────────────────────

/// Drain an error pointer into an [`anyhow::Error`] and free the underlying
/// `GError`.
///
/// # Safety
/// `error` must be a valid (possibly null) `*mut NetplanError`.  After this
/// call `*error` is `NULL`.
pub unsafe fn drain_error(error: *mut ffi::NetplanError) -> anyhow::Error {
    if error.is_null() {
        return anyhow!("unknown libnetplan error");
    }
    let mut buf = [0u8; 2048];
    let n = ffi::netplan_error_message(error, buf.as_mut_ptr() as *mut c_char, buf.len());
    // netplan_error_clear takes **NetplanError; we pass a pointer to a local copy
    let mut p = error;
    ffi::netplan_error_clear(&mut p);
    if n > 1 {
        let n = (n as usize - 1).min(buf.len()); // exclude NUL terminator
        anyhow!("{}", String::from_utf8_lossy(&buf[..n]))
    } else {
        anyhow!("unknown libnetplan error")
    }
}

// ── Parser ────────────────────────────────────────────────────────────────────

/// Safe wrapper around `NetplanParser *`.
pub struct Parser(*mut ffi::NetplanParser);

impl Parser {
    pub fn new() -> Result<Self> {
        let p = unsafe { ffi::netplan_parser_new() };
        if p.is_null() {
            return Err(anyhow!("netplan_parser_new returned NULL"));
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
        // SAFETY: `self.0` is a valid parser pointer for the lifetime of `self`;
        // `c` is a valid NUL-terminated string; `err` is a valid out-pointer.
        let ok = unsafe { ffi::netplan_parser_load_yaml_hierarchy(self.0, c.as_ptr(), &mut err) };
        if ok == 0 {
            // SAFETY: `err` was set by the call above on failure.
            return Err(unsafe { drain_error(err) });
        }
        Ok(())
    }

    /// Parse a YAML file given by its absolute path.
    pub fn load_yaml_file(&mut self, path: &str) -> Result<()> {
        let c = CString::new(path)?;
        let mut err: *mut ffi::NetplanError = std::ptr::null_mut();
        // SAFETY: `self.0` is a valid parser pointer; `c` is a valid
        // NUL-terminated string; `err` is a valid out-pointer.
        let ok = unsafe { ffi::netplan_parser_load_yaml(self.0, c.as_ptr(), &mut err) };
        if ok == 0 {
            // SAFETY: `err` was set by the call above on failure.
            return Err(unsafe { drain_error(err) });
        }
        Ok(())
    }

    /// Parse YAML from an already-opened file.
    /// The caller must seek `file` to the desired start position before calling.
    pub fn load_yaml_from_fd(&mut self, file: &std::fs::File) -> Result<()> {
        let mut err: *mut ffi::NetplanError = std::ptr::null_mut();
        // SAFETY: `self.0` is a valid parser pointer; `file` owns a valid fd
        // for the duration of this call; `err` is a valid out-pointer.
        let ok =
            unsafe { ffi::netplan_parser_load_yaml_from_fd(self.0, file.as_raw_fd(), &mut err) };
        if ok == 0 {
            // SAFETY: `err` was set by the call above on failure.
            return Err(unsafe { drain_error(err) });
        }
        Ok(())
    }

    /// Mark fields in the provided file as nullable (to-be-deleted).
    /// Seek `file` to position 0 before calling.
    pub fn load_nullable_fields(&mut self, file: &std::fs::File) -> Result<()> {
        let mut err: *mut ffi::NetplanError = std::ptr::null_mut();
        // SAFETY: `self.0` is a valid parser pointer; `file` owns a valid fd
        // for the duration of this call; `err` is a valid out-pointer.
        let ok =
            unsafe { ffi::netplan_parser_load_nullable_fields(self.0, file.as_raw_fd(), &mut err) };
        if ok == 0 {
            // SAFETY: `err` was set by the call above on failure.
            return Err(unsafe { drain_error(err) });
        }
        Ok(())
    }

    /// Mark netdefs / globals as nullable overrides constrained to `filename`.
    /// Seek `file` to position 0 before calling.
    pub fn load_nullable_overrides(&mut self, file: &std::fs::File, filename: &str) -> Result<()> {
        let c = CString::new(filename)?;
        let mut err: *mut ffi::NetplanError = std::ptr::null_mut();
        // SAFETY: `self.0` is a valid parser pointer; `file` owns a valid fd
        // for the duration of this call; `c` is a valid NUL-terminated string;
        // `err` is a valid out-pointer.
        let ok = unsafe {
            ffi::netplan_parser_load_nullable_overrides(
                self.0,
                file.as_raw_fd(),
                c.as_ptr(),
                &mut err,
            )
        };
        if ok == 0 {
            // SAFETY: `err` was set by the call above on failure.
            return Err(unsafe { drain_error(err) });
        }
        Ok(())
    }
}

impl Drop for Parser {
    fn drop(&mut self) {
        // SAFETY: `self.0` was created by `netplan_parser_new` and is only
        // freed here, on drop.
        unsafe { ffi::netplan_parser_clear(&mut self.0) };
    }
}

// ── State ─────────────────────────────────────────────────────────────────────

/// Safe wrapper around `NetplanState *`.
pub struct State(*mut ffi::NetplanState);

impl State {
    pub fn new() -> Result<Self> {
        // SAFETY: FFI call with no preconditions; returns either a valid
        // pointer or NULL, both checked below.
        let s = unsafe { ffi::netplan_state_new() };
        if s.is_null() {
            return Err(anyhow!("netplan_state_new returned NULL"));
        }
        Ok(Self(s))
    }

    /// Validate the parser contents and transfer ownership into this state.
    pub fn import_parser_results(&mut self, parser: &mut Parser) -> Result<()> {
        let mut err: *mut ffi::NetplanError = std::ptr::null_mut();
        // SAFETY: `self.0` and `parser.as_ptr()` are valid pointers owned by
        // their respective wrappers; `err` is a valid out-pointer.
        let ok =
            unsafe { ffi::netplan_state_import_parser_results(self.0, parser.as_ptr(), &mut err) };
        if ok == 0 {
            // SAFETY: `err` was set by the call above on failure.
            return Err(unsafe { drain_error(err) });
        }
        Ok(())
    }

    /// Serialise the entire state to YAML and return it as a `String`.
    pub fn dump_yaml(&self) -> Result<String> {
        let mut tmp = memfd("np-dump")?;
        let fd = tmp.as_raw_fd();
        let mut err: *mut ffi::NetplanError = std::ptr::null_mut();
        // SAFETY: `self.0` is a valid state pointer; `fd` is a valid,
        // writable file descriptor owned by `tmp`; `err` is a valid
        // out-pointer.
        let ok = unsafe { ffi::netplan_state_dump_yaml(self.0, fd, &mut err) };
        if ok == 0 {
            // SAFETY: `err` was set by the call above on failure.
            return Err(unsafe { drain_error(err) });
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
        // SAFETY: `self.0` is a valid state pointer; `cf`/`cr` are valid
        // NUL-terminated strings; `err` is a valid out-pointer.
        let ok = unsafe {
            ffi::netplan_state_write_yaml_file(self.0, cf.as_ptr(), cr.as_ptr(), &mut err)
        };
        if ok == 0 {
            // SAFETY: `err` was set by the call above on failure.
            return Err(unsafe { drain_error(err) });
        }
        Ok(())
    }

    /// Update all origin YAML files; data without an origin goes to
    /// `default_filename` under `rootdir`.
    pub fn update_yaml_hierarchy(&self, default_filename: &str, rootdir: &str) -> Result<()> {
        let cf = CString::new(default_filename)?;
        let cr = CString::new(rootdir)?;
        let mut err: *mut ffi::NetplanError = std::ptr::null_mut();
        // SAFETY: `self.0` is a valid state pointer; `cf`/`cr` are valid
        // NUL-terminated strings; `err` is a valid out-pointer.
        let ok = unsafe {
            ffi::netplan_state_update_yaml_hierarchy(self.0, cf.as_ptr(), cr.as_ptr(), &mut err)
        };
        if ok == 0 {
            // SAFETY: `err` was set by the call above on failure.
            return Err(unsafe { drain_error(err) });
        }
        Ok(())
    }

    /// Iterate over all `NetDefinition` objects in this state.
    ///
    /// The returned iterator borrows `self` for the duration of iteration.
    pub fn netdefs(&self) -> impl Iterator<Item = NetDef> + '_ {
        let mut iter = ffi::NetplanStateIterator {
            placeholder: std::ptr::null_mut(),
        };
        // SAFETY: `self.0` is a valid state pointer; `iter` is a freshly
        // initialised, stack-allocated iterator struct.
        unsafe { ffi::netplan_state_iterator_init(self.0, &mut iter) };
        NetDefIter { iter }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        // SAFETY: `self.0` was created by `netplan_state_new` and is only
        // freed here, on drop.
        unsafe { ffi::netplan_state_clear(&mut self.0) };
    }
}

// ── NetDefIter ────────────────────────────────────────────────────────────────

/// Iterator over `NetDef` references inside a `State`.
///
/// The pointed-to objects are owned by the `State`; do not outlive it.
/// Constructed only via [`State::netdefs`].
struct NetDefIter {
    iter: ffi::NetplanStateIterator,
}

impl Iterator for NetDefIter {
    type Item = NetDef;

    fn next(&mut self) -> Option<Self::Item> {
        // SAFETY: `self.iter` was initialised by `netplan_state_iterator_init`
        // and is only ever accessed through this iterator.
        if unsafe { ffi::netplan_state_iterator_has_next(&mut self.iter) } == 0 {
            return None;
        }
        // SAFETY: `has_next` returned true, so the iterator has at least one
        // more element to yield.
        let ptr = unsafe { ffi::netplan_state_iterator_next(&mut self.iter) };
        if ptr.is_null() {
            None
        } else {
            Some(NetDef(ptr))
        }
    }
}

// ── NetDef ────────────────────────────────────────────────────────────────────

/// A view into a single network definition inside a `State`.
///
/// The underlying memory is owned by the `State`; do not outlive it.
pub struct NetDef(*mut ffi::NetplanNetDefinition);

impl NetDef {
    pub fn def_type(&self) -> ffi::NetplanDefType {
        // SAFETY: `self.0` is a valid netdef pointer owned by the `State`
        // this `NetDef` was obtained from.
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
    pub fn id(&self) -> Result<String> {
        // SAFETY: `self.0` is a valid netdef pointer; `buf`/`len` describe the
        // growable buffer owned by `read_string_buf`.
        read_string_buf(|buf, len| unsafe { ffi::netplan_netdef_get_id(self.0, buf, len) })
    }

    /// The `set-name` value, or `None` if not configured.
    pub fn set_name(&self) -> Result<Option<String>> {
        // SAFETY: `self.0` is a valid netdef pointer; `buf`/`len` describe the
        // growable buffer owned by `read_string_buf`.
        let s = read_string_buf(|buf, len| unsafe {
            ffi::netplan_netdef_get_set_name(self.0, buf, len)
        })?;
        Ok(if s.is_empty() { None } else { Some(s) })
    }

    /// `true` if the netdef contains a `match:` stanza.
    pub fn has_match(&self) -> bool {
        // SAFETY: `self.0` is a valid netdef pointer.
        unsafe { ffi::netplan_netdef_has_match(self.0) != 0 }
    }

    pub fn backend(&self) -> ffi::NetplanBackend {
        // SAFETY: `self.0` is a valid netdef pointer.
        unsafe { ffi::netplan_netdef_get_backend(self.0) }
    }

    pub fn backend_name(&self) -> &'static str {
        match self.backend() {
            ffi::NETPLAN_BACKEND_NETWORKD => "networkd",
            ffi::NETPLAN_BACKEND_NM => "NetworkManager",
            ffi::NETPLAN_BACKEND_OVS => "openvswitch",
            _ => "none",
        }
    }

    pub fn dhcp4(&self) -> bool {
        // SAFETY: `self.0` is a valid netdef pointer.
        unsafe { ffi::netplan_netdef_get_dhcp4(self.0) != 0 }
    }

    pub fn dhcp6(&self) -> bool {
        // SAFETY: `self.0` is a valid netdef pointer.
        unsafe { ffi::netplan_netdef_get_dhcp6(self.0) != 0 }
    }

    pub fn link_local_ipv4(&self) -> bool {
        // SAFETY: `self.0` is a valid netdef pointer.
        unsafe { ffi::netplan_netdef_get_link_local_ipv4(self.0) != 0 }
    }

    pub fn link_local_ipv6(&self) -> bool {
        // SAFETY: `self.0` is a valid netdef pointer.
        unsafe { ffi::netplan_netdef_get_link_local_ipv6(self.0) != 0 }
    }

    /// Returns `None` if not configured, `Some(true)` if enabled, `Some(false)` if disabled.
    pub fn accept_ra(&self) -> Option<bool> {
        // SAFETY: `self.0` is a valid netdef pointer.
        match unsafe { ffi::netplan_netdef_get_accept_ra(self.0) } {
            0 => None,
            1 => Some(true),
            _ => Some(false),
        }
    }

    pub fn macaddress(&self) -> Result<Option<String>> {
        // SAFETY: `self.0` is a valid netdef pointer; `buf`/`len` describe the
        // growable buffer owned by `read_string_buf`.
        let s = read_string_buf(|buf, len| unsafe {
            ffi::netplan_netdef_get_macaddress(self.0, buf, len)
        })?;
        Ok(if s.is_empty() { None } else { Some(s) })
    }

    pub fn bridge_link_id(&self) -> Result<Option<String>> {
        // SAFETY: `self.0` is a valid netdef pointer; the returned pointer is
        // either NULL or a netdef owned by the same `State`, checked below.
        let ptr = unsafe { ffi::netplan_netdef_get_bridge_link(self.0) };
        if ptr.is_null() {
            return Ok(None);
        }
        // SAFETY: `ptr` was just checked non-null and points to a valid
        // netdef owned by the `State`.
        let id = read_string_buf(|buf, len| unsafe { ffi::netplan_netdef_get_id(ptr, buf, len) })?;
        Ok(if id.is_empty() { None } else { Some(id) })
    }

    pub fn bond_link_id(&self) -> Result<Option<String>> {
        // SAFETY: `self.0` is a valid netdef pointer; the returned pointer is
        // either NULL or a netdef owned by the same `State`, checked below.
        let ptr = unsafe { ffi::netplan_netdef_get_bond_link(self.0) };
        if ptr.is_null() {
            return Ok(None);
        }
        // SAFETY: `ptr` was just checked non-null and points to a valid
        // netdef owned by the `State`.
        let id = read_string_buf(|buf, len| unsafe { ffi::netplan_netdef_get_id(ptr, buf, len) })?;
        Ok(if id.is_empty() { None } else { Some(id) })
    }

    pub fn vrf_link_id(&self) -> Result<Option<String>> {
        // SAFETY: `self.0` is a valid netdef pointer; the returned pointer is
        // either NULL or a netdef owned by the same `State`, checked below.
        let ptr = unsafe { ffi::netplan_netdef_get_vrf_link(self.0) };
        if ptr.is_null() {
            return Ok(None);
        }
        // SAFETY: `ptr` was just checked non-null and points to a valid
        // netdef owned by the `State`.
        let id = read_string_buf(|buf, len| unsafe { ffi::netplan_netdef_get_id(ptr, buf, len) })?;
        Ok(if id.is_empty() { None } else { Some(id) })
    }

    #[allow(dead_code)]
    pub fn type_str(&self) -> &'static str {
        match self.def_type() {
            ffi::NETPLAN_DEF_TYPE_ETHERNET => "ethernet",
            ffi::NETPLAN_DEF_TYPE_WIFI => "wifi",
            ffi::NETPLAN_DEF_TYPE_MODEM => "modem",
            ffi::NETPLAN_DEF_TYPE_BRIDGE => "bridge",
            ffi::NETPLAN_DEF_TYPE_BOND => "bond",
            ffi::NETPLAN_DEF_TYPE_VLAN => "vlan",
            ffi::NETPLAN_DEF_TYPE_TUNNEL => "tunnel",
            ffi::NETPLAN_DEF_TYPE_VRF => "vrf",
            ffi::NETPLAN_DEF_TYPE_DUMMY => "dummy-device",
            ffi::NETPLAN_DEF_TYPE_VETH => "virtual-ethernet",
            _ => "other",
        }
    }

    /// Returns `true` if `name`/`mac`/`driver` all satisfy this netdef's
    /// match rules.  `None` arguments are passed as NULL (wildcard).
    pub fn matches_interface(&self, name: &str, mac: Option<&str>, driver: Option<&str>) -> bool {
        let cn = CString::new(name).unwrap_or_default();
        let cm = mac.and_then(|m| CString::new(m).ok());
        let cd = driver.and_then(|d| CString::new(d).ok());
        // SAFETY: `self.0` is a valid netdef pointer; `cn` is a valid
        // NUL-terminated string; `cm`/`cd` are either NULL or valid
        // NUL-terminated strings.
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
/// `prefix_path` is an iterator of path components, e.g.
/// `["network", "ethernets", "eth0"]`.  The TAB-joining and FD plumbing
/// are handled internally.
pub fn dump_yaml_subtree<'a>(
    prefix_path: impl Iterator<Item = &'a str>,
    full_yaml: &str,
) -> Result<String> {
    let tab_prefix = prefix_path.collect::<Vec<_>>().join("\t");
    let c_prefix = CString::new(tab_prefix)?;

    // Write full YAML into input memfd
    let mut input = memfd("np-subtree-in")?;
    input.write_all(full_yaml.as_bytes())?;
    input.seek(SeekFrom::Start(0))?;

    let mut output = memfd("np-subtree-out")?;

    let mut err: *mut ffi::NetplanError = std::ptr::null_mut();
    // SAFETY: `c_prefix` is a valid NUL-terminated string; `input`/`output`
    // own valid file descriptors; `err` is a valid out-pointer.
    let ok = unsafe {
        ffi::netplan_util_dump_yaml_subtree(
            c_prefix.as_ptr(),
            input.as_raw_fd(),
            output.as_raw_fd(),
            &mut err,
        )
    };
    if ok == 0 {
        // SAFETY: `err` was set by the call above on failure.
        return Err(unsafe { drain_error(err) });
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
pub fn create_yaml_patch<'a>(
    obj_path: impl Iterator<Item = &'a str>,
    payload: &str,
) -> Result<std::fs::File> {
    let tab_path = obj_path.collect::<Vec<_>>().join("\t");
    let c_path = CString::new(tab_path)?;
    let c_payload = CString::new(payload)?;

    let tmp = memfd("np-patch")?;
    let fd = tmp.as_raw_fd();

    let mut err: *mut ffi::NetplanError = std::ptr::null_mut();
    // SAFETY: `c_path`/`c_payload` are valid NUL-terminated strings; `fd` is
    // a valid, writable file descriptor owned by `tmp`; `err` is a valid
    // out-pointer.
    let ok = unsafe {
        ffi::netplan_util_create_yaml_patch(c_path.as_ptr(), c_payload.as_ptr(), fd, &mut err)
    };
    if ok == 0 {
        // SAFETY: `err` was set by the call above on failure.
        return Err(unsafe { drain_error(err) });
    }
    Ok(tmp)
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Call `f(buf_ptr, buf_len)` with a growing buffer until the return value
/// fits, then return the resulting `String`.
///
/// Returns `Err` if libnetplan signals an unexpected error code or if the
/// returned bytes are not valid UTF-8.
///
/// Mirrors `_string_realloc_call_no_error` from the Python CFFI layer.
fn read_string_buf<F>(f: F) -> Result<String>
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
        if n < 0 {
            return Err(anyhow!("libnetplan string getter returned error code {n}"));
        }
        if n == 0 {
            return Ok(String::new());
        }
        let content_len = (n as usize - 1).min(buf.len()); // exclude NUL
        let s = String::from_utf8(buf[..content_len].to_vec())
            .with_context(|| "libnetplan returned non-UTF-8 string")?;
        return Ok(s);
    }
}
