//! A hook for code that can recover from an exception nothing handled
//! (the process-wide unhandled-exception filter calls it first).

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};

static HOOK: AtomicUsize = AtomicUsize::new(0);

/// `hook` is called with the exception pointers of every exception that
/// reaches the unhandled-exception filter, before the crash is reported.
/// It may never return (a thread that is parked for good).
pub fn set_hook(hook: extern "system" fn(*const c_void)) {
    HOOK.store(hook as usize, Ordering::Release);
}

/// Calls the hook, if there is one.
pub fn call_hook(info: *const c_void) {
    let hook = HOOK.load(Ordering::Acquire);
    if hook != 0 {
        // SAFETY: only `set_hook` stores here, a function of this type.
        let hook: extern "system" fn(*const c_void) = unsafe { std::mem::transmute(hook) };
        hook(info);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CALLS: AtomicUsize = AtomicUsize::new(0);

    extern "system" fn count(_: *const std::ffi::c_void) {
        CALLS.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn the_hook_is_called_with_what_reaches_the_filter() {
        super::call_hook(std::ptr::null());
        assert_eq!(CALLS.load(Ordering::Relaxed), 0);
        super::set_hook(count);
        super::call_hook(std::ptr::null());
        assert_eq!(CALLS.load(Ordering::Relaxed), 1);
    }
}
