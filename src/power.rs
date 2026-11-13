//! Keeps the computer from going to sleep on its own while uploads are pending.
//!
//! This only blocks *idle* sleep (nobody touching the computer). Locking the
//! screen or turning the display off is fine either way; closing the lid or
//! choosing "Sleep" still suspends everything, and the uploads carry on after
//! waking up.

use std::time::{Duration, Instant};

/// While alive, the system does not sleep because of inactivity.
pub struct KeepAwake(imp::Handle);

impl KeepAwake {
    pub fn new(reason: &str) -> Option<Self> {
        match imp::acquire(reason) {
            Ok(h) => Some(Self(h)),
            Err(e) => {
                log::warn!("cannot keep the computer awake: {e}");
                None
            }
        }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        imp::release(&self.0);
    }
}

/// Uploads stuck (failing, waiting on a file) for this long no longer keep the
/// computer awake: they would hold it up forever.
const STALL: Duration = Duration::from_secs(30 * 60);

/// Decides, from what the engine reports now and then, whether the computer
/// should stay awake.
#[derive(Default)]
pub struct Policy {
    last: Option<(u64, u64)>,
    since: Option<Instant>,
}

impl Policy {
    pub fn wants_awake(&mut self, online: bool, pending: u64, uploading: u64) -> bool {
        if !online || pending + uploading == 0 {
            self.last = None;
            return false;
        }
        let now = Instant::now();
        if self.last != Some((pending, uploading)) {
            self.last = Some((pending, uploading));
            self.since = Some(now);
        }
        // Something is being sent: keep going, however long it takes.
        uploading > 0 || self.since.is_some_and(|t| now.duration_since(t) < STALL)
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{c_char, c_void, CString};

    type CFStringRef = *const c_void;
    const UTF8: u32 = 0x0800_0100;
    const LEVEL_ON: u32 = 255;

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(alloc: *const c_void, s: *const c_char, encoding: u32) -> CFStringRef;
        fn CFRelease(cf: *const c_void);
    }

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOPMAssertionCreateWithName(kind: CFStringRef, level: u32, name: CFStringRef, id: *mut u32) -> i32;
        fn IOPMAssertionRelease(id: u32) -> i32;
    }

    pub struct Handle(u32);

    fn cfstr(s: &str) -> CFStringRef {
        let c = CString::new(s.replace('\0', "")).unwrap_or_default();
        unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), UTF8) }
    }

    pub fn acquire(reason: &str) -> Result<Handle, String> {
        // Also keeps App Nap away: a process holding a power assertion is never napped.
        let kind = cfstr("PreventUserIdleSystemSleep");
        let name = cfstr(reason);
        if kind.is_null() || name.is_null() {
            return Err("CFString".into());
        }
        let mut id = 0u32;
        let rc = unsafe { IOPMAssertionCreateWithName(kind, LEVEL_ON, name, &mut id) };
        unsafe {
            CFRelease(kind);
            CFRelease(name);
        }
        if rc == 0 {
            Ok(Handle(id))
        } else {
            Err(format!("IOPMAssertionCreateWithName: {rc:#x}"))
        }
    }

    pub fn release(h: &Handle) {
        unsafe { IOPMAssertionRelease(h.0) };
    }
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Power::{PowerClearRequest, PowerCreateRequest, PowerRequestSystemRequired, PowerSetRequest};
    use windows_sys::Win32::System::Threading::{POWER_REQUEST_CONTEXT_SIMPLE_STRING, REASON_CONTEXT, REASON_CONTEXT_0};

    /// A power request is tied to its handle, not to a thread (unlike
    /// `SetThreadExecutionState`), so any tokio thread may hold it.
    pub struct Handle(HANDLE);
    unsafe impl Send for Handle {}
    unsafe impl Sync for Handle {}

    pub fn acquire(reason: &str) -> Result<Handle, String> {
        let mut text: Vec<u16> = reason.encode_utf16().chain(std::iter::once(0)).collect();
        let ctx = REASON_CONTEXT { Version: 0, Flags: POWER_REQUEST_CONTEXT_SIMPLE_STRING, Reason: REASON_CONTEXT_0 { SimpleReasonString: text.as_mut_ptr() } };
        let h = unsafe { PowerCreateRequest(&ctx) };
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return Err(format!("PowerCreateRequest: {}", std::io::Error::last_os_error()));
        }
        if unsafe { PowerSetRequest(h, PowerRequestSystemRequired) } == 0 {
            let e = std::io::Error::last_os_error();
            unsafe { CloseHandle(h) };
            return Err(format!("PowerSetRequest: {e}"));
        }
        Ok(Handle(h))
    }

    pub fn release(h: &Handle) {
        unsafe {
            PowerClearRequest(h.0, PowerRequestSystemRequired);
            CloseHandle(h.0);
        }
    }
}

/// Linux: nothing yet (it would go through systemd-logind's inhibitor locks).
#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    pub struct Handle;

    pub fn acquire(_reason: &str) -> Result<Handle, String> {
        Ok(Handle)
    }

    pub fn release(_h: &Handle) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy() {
        let mut p = Policy::default();
        assert!(!p.wants_awake(true, 0, 0));
        assert!(p.wants_awake(true, 3, 0));
        assert!(!p.wants_awake(false, 3, 0), "offline: nothing to wait for");
        assert!(p.wants_awake(true, 3, 1));
        p.since = Some(Instant::now() - STALL);
        assert!(p.wants_awake(true, 3, 1), "an upload in progress keeps it awake");
        p.last = Some((3, 0));
        assert!(!p.wants_awake(true, 3, 0), "stuck for too long");
        assert!(p.wants_awake(true, 2, 0), "progress again");
    }
}
