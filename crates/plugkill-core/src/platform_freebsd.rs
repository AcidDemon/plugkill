//! FreeBSD sysctl primitives shared by the hardware monitors.
//!
//! Everything here is FreeBSD-only; the Linux backends read sysfs instead.

use std::ffi::CString;

/// Read an integer sysctl by name, e.g. `hw.acpi.acline`.
///
/// ACPI sysctls are `int` (32-bit); the value is widened to `i64`. Returns
/// `None` if the sysctl is absent (common on desktops/VMs without ACPI).
pub fn sysctl_int(name: &str) -> Option<i64> {
    let cname = CString::new(name).ok()?;
    let mut val: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    let ret = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            &mut val as *mut _ as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (ret == 0).then_some(val as i64)
}

/// Read a string sysctl by name. Trailing NULs are stripped.
pub fn sysctl_string(name: &str) -> Option<String> {
    let cname = CString::new(name).ok()?;

    // Size probe: NULL buffer returns the required length in `len`.
    let mut len = 0usize;
    let ret = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if ret != 0 || len == 0 {
        return None;
    }

    let mut buf = vec![0u8; len];
    let ret = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if ret != 0 {
        return None;
    }

    buf.truncate(len);
    while buf.last() == Some(&0) {
        buf.pop();
    }
    String::from_utf8(buf).ok()
}
