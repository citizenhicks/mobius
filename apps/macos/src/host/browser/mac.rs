//! Where Chromium meets AppKit: `NSApp` must report whether it is inside `sendEvent:`, and
//! CEF's work runs on the main dispatch queue that GPUI's run loop also drains.

use std::ffi::{CString, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use objc2::runtime::{AnyClass, AnyObject, AnyProtocol, Bool, Imp, Sel};
use objc2::{Encode, ffi, sel};

type SendEvent = unsafe extern "C-unwind" fn(&AnyObject, Sel, *mut AnyObject);
type IsHandling = extern "C-unwind" fn(&AnyObject, Sel) -> Bool;
type SetHandling = extern "C-unwind" fn(&AnyObject, Sel, Bool);

static HANDLING_SEND_EVENT: AtomicBool = AtomicBool::new(false);
static SEND_EVENT: OnceLock<SendEvent> = OnceLock::new();

extern "C-unwind" fn is_handling_send_event(_: &AnyObject, _: Sel) -> Bool {
    Bool::new(HANDLING_SEND_EVENT.load(Ordering::Relaxed))
}

extern "C-unwind" fn set_handling_send_event(_: &AnyObject, _: Sel, handling: Bool) {
    HANDLING_SEND_EVENT.store(handling.as_bool(), Ordering::Relaxed);
}

extern "C-unwind" fn send_event(app: &AnyObject, selector: Sel, event: *mut AnyObject) {
    let outer = !HANDLING_SEND_EVENT.swap(true, Ordering::Relaxed);
    if let Some(inherited) = SEND_EVENT.get() {
        // SAFETY: NSApplication's own `sendEvent:`, called with the arguments AppKit passed.
        unsafe { inherited(app, selector, event) };
    }
    if outer {
        HANDLING_SEND_EVENT.store(false, Ordering::Relaxed);
    }
}

/// Gives GPUI's application class Chromium's `CefAppProtocol`: the send-event flag that
/// tells Chromium's pump a nested AppKit loop from the main one. Call once, before CEF
/// starts and on the main thread.
pub(super) fn adopt_app_protocol() -> Result<()> {
    let app = AnyClass::get(c"GPUIApplication").context("GPUI's application class is missing")?;
    let inherited = app
        .instance_method(sel!(sendEvent:))
        .context("NSApplication has no sendEvent:")?
        .implementation();
    // SAFETY: every NSApplication implements `sendEvent:` with this signature.
    let inherited = unsafe { std::mem::transmute::<Imp, SendEvent>(inherited) };
    ensure!(
        SEND_EVENT.set(inherited).is_ok(),
        "Chromium's application protocol is already installed"
    );
    let class = std::ptr::from_ref(app).cast_mut();
    let object = <&AnyObject>::ENCODING;
    let flag = Bool::ENCODING;
    // SAFETY: C function pointers cast to the runtime's untyped IMP; the runtime calls each
    // with the signature its type encoding declares.
    let methods: [(Sel, Imp, String); 3] = unsafe {
        [
            (
                sel!(isHandlingSendEvent),
                std::mem::transmute::<IsHandling, Imp>(is_handling_send_event),
                format!("{flag}{object}:"),
            ),
            (
                sel!(setHandlingSendEvent:),
                std::mem::transmute::<SetHandling, Imp>(set_handling_send_event),
                format!("v{object}:{flag}"),
            ),
            (
                sel!(sendEvent:),
                std::mem::transmute::<SendEvent, Imp>(send_event),
                format!("v{object}:{object}"),
            ),
        ]
    };
    for (selector, implementation, types) in methods {
        let types = CString::new(types)?;
        // SAFETY: `class` is registered and GPUI declares none of these selectors itself,
        // so each is added to it rather than replacing one it depends on.
        let added =
            unsafe { ffi::class_addMethod(class, selector, implementation, types.as_ptr()) };
        ensure!(
            added.as_bool(),
            "GPUI's application already implements {selector}"
        );
    }
    for name in [c"CrAppProtocol", c"CrAppControlProtocol", c"CefAppProtocol"] {
        if let Some(protocol) = AnyProtocol::get(name) {
            // SAFETY: the class implements every method these protocols require.
            unsafe { ffi::class_addProtocol(class, protocol) };
        }
    }
    Ok(())
}

#[repr(C)]
struct DispatchQueue {
    _opaque: [u8; 0],
}

unsafe extern "C" {
    static _dispatch_main_q: DispatchQueue;
    fn dispatch_time(when: u64, delta: i64) -> u64;
    fn dispatch_after_f(
        when: u64,
        queue: *const DispatchQueue,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
}

/// Runs `work(context)` on the main thread once `delay` has passed. Any thread may call it.
pub(super) fn after_on_main(delay: Duration, context: usize, work: extern "C" fn(*mut c_void)) {
    const NOW: u64 = 0;
    let nanos = i64::try_from(delay.as_nanos()).unwrap_or(i64::MAX);
    // SAFETY: the main queue lives for the whole process; `context` is a plain integer.
    unsafe {
        dispatch_after_f(
            dispatch_time(NOW, nanos),
            &raw const _dispatch_main_q,
            context as *mut c_void,
            work,
        );
    }
}
