#![cfg(target_os = "macos")]

use log::debug;
use std::ffi::{c_char, CStr, CString};

type AXError = i32;
const AX_SUCCESS: AXError = 0;
const K_CFSTRING_UTF8: u32 = 0x0800_0100;

#[repr(C)]
struct __AXUIElement(std::ffi::c_void);
type AXUIElementRef = *mut __AXUIElement;
type CFStringRef = *const std::ffi::c_void;
type CFTypeRef = *const std::ffi::c_void;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    fn AXUIElementSetMessagingTimeout(element: AXUIElementRef, timeoutInSeconds: f32) -> AXError;
    fn CFRelease(obj: CFTypeRef);
    fn CFGetTypeID(cf: CFTypeRef) -> u64;
    fn CFStringGetTypeID() -> u64;
    fn CFStringGetLength(str: CFStringRef) -> isize;
    fn CFStringGetCStringPtr(str: CFStringRef, encoding: u32) -> *const c_char;
    fn CFStringGetCString(
        str: CFStringRef,
        buffer: *mut c_char,
        buf_size: isize,
        encoding: u32,
    ) -> u8;
    fn CFStringCreateWithCString(
        alloc: *const std::ffi::c_void,
        c_str: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
}

unsafe fn cfstring_to_string(ptr: CFTypeRef) -> Option<String> {
    if ptr.is_null() || CFGetTypeID(ptr) != CFStringGetTypeID() {
        if !ptr.is_null() {
            CFRelease(ptr);
        }
        return None;
    }
    let result = {
        let c_ptr = CFStringGetCStringPtr(ptr as CFStringRef, K_CFSTRING_UTF8);
        if !c_ptr.is_null() {
            CStr::from_ptr(c_ptr).to_string_lossy().into_owned()
        } else {
            let len = CFStringGetLength(ptr as CFStringRef);
            let max = len * 4 + 1;
            let mut buf = vec![0u8; max as usize];
            if CFStringGetCString(
                ptr as CFStringRef,
                buf.as_mut_ptr() as *mut c_char,
                max,
                K_CFSTRING_UTF8,
            ) == 0
            {
                CFRelease(ptr);
                return None;
            }
            CStr::from_ptr(buf.as_ptr() as *const c_char)
                .to_string_lossy()
                .into_owned()
        }
    };
    CFRelease(ptr);
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

unsafe fn get_ax_attr(element: AXUIElementRef, attr: &str) -> Option<CFTypeRef> {
    let c_attr = CString::new(attr).ok()?;
    let cf_attr = CFStringCreateWithCString(std::ptr::null(), c_attr.as_ptr(), K_CFSTRING_UTF8);
    if cf_attr.is_null() {
        return None;
    }
    let mut value: CFTypeRef = std::ptr::null();
    let ret = AXUIElementCopyAttributeValue(element, cf_attr, &mut value);
    CFRelease(cf_attr);
    if ret == AX_SUCCESS && !value.is_null() {
        Some(value)
    } else {
        None
    }
}

unsafe fn get_ax_str(element: AXUIElementRef, attr: &str) -> Option<String> {
    let ptr = get_ax_attr(element, attr)?;
    cfstring_to_string(ptr)
}

pub fn get_focused_ui_text() -> Option<String> {
    unsafe {
        let system = AXUIElementCreateSystemWide();
        if system.is_null() {
            return None;
        }
        AXUIElementSetMessagingTimeout(system, 1.0);

        let app = get_ax_attr(system, "AXFocusedApplication");
        CFRelease(system as CFTypeRef);
        let app = match app {
            Some(a) => a as AXUIElementRef,
            None => {
                debug!("AXFocusedApplication returned None");
                return None;
            }
        };

        let app_name = get_ax_str(app, "AXTitle");
        let focused = get_ax_attr(app, "AXFocusedUIElement");
        let window = get_ax_attr(app, "AXFocusedWindow");
        let window_title = window.and_then(|w| {
            let t = get_ax_str(w as AXUIElementRef, "AXTitle");
            CFRelease(w);
            t
        });
        CFRelease(app as CFTypeRef);

        let focused = match focused {
            Some(f) => f as AXUIElementRef,
            None => {
                debug!("AXFocusedUIElement returned None");
                return None;
            }
        };

        let role = get_ax_str(focused, "AXRole");
        let has_text = role.as_deref().is_some_and(|r| {
            matches!(
                r,
                "AXTextArea"
                    | "AXTextField"
                    | "AXComboBox"
                    | "AXWebArea"
                    | "AXScrollArea"
                    | "AXGroup"
            )
        });

        let mut parts = Vec::new();
        if let Some(ref n) = app_name {
            parts.push(format!("[App: {n}]"));
        }
        if let Some(ref t) = window_title {
            parts.push(format!("[Window: {t}]"));
        }
        if let Some(t) = get_ax_str(focused, "AXSelectedText") {
            parts.push(format!("[Selection]: {t}"));
        }
        if has_text {
            if let Some(t) = get_ax_str(focused, "AXValue") {
                parts.push(format!("[Content]: {t}"));
            }
        }
        if let Some(t) = get_ax_str(focused, "AXTitle") {
            parts.push(format!("[Title]: {t}"));
        }
        if let Some(t) = get_ax_str(focused, "AXDescription") {
            parts.push(format!("[Description]: {t}"));
        }

        CFRelease(focused as CFTypeRef);

        if parts.is_empty() {
            return None;
        }
        let mut text = parts.join("\n");
        if text.len() > 4000 {
            text.truncate(4000);
            text.push_str("\n...");
        }
        Some(text)
    }
}
