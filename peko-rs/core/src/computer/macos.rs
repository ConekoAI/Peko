//! `Computer` on macOS 13+ (ADR-123 D4).
//!
//! - Windows: `CGWindowListCopyWindowInfo`, front to back.
//! - Pictures: ScreenCaptureKit's `SCScreenshotManager` (14+), else
//!   `CGWindowListCreateImage` (13; looked up at run time, as the SDK no
//!   longer declares it).
//! - Input: `CGEventPost` at the HID tap; every event carries
//!   [`EVENT_TAG`] in its source user data, so the listen-only tap that
//!   watches for the owner's hand tells them apart.
//! - Bringing a window forward: Accessibility (`AXFrontmost`, `AXRaise`,
//!   `AXMain`), with System Events as the fallback.
//! - Grants: Screen Recording and Accessibility, held by the app macOS
//!   holds responsible for the engine.

// FFI out-parameters are passed as `&mut`, which the declarations take.
#![allow(
    non_upper_case_globals,
    non_snake_case,
    clippy::upper_case_acronyms,
    clippy::borrow_as_ptr
)]

use std::ffi::{c_char, c_void, CStr};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;

use super::keys::{Chord, Key};
use super::{
    Button, Front, Image, InputEvent, Permissions, Platform, Rect, Screen, Target, WindowInfo,
    INDICATOR_TITLE,
};

/// Marks the events the runtime posts (`kCGEventSourceUserData`).
pub const EVENT_TAG: i64 = 0x5045_4B4F; // "PEKO"

// ─── CoreGraphics, Accessibility, IOKit ────────────────────────────────

type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFArrayRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CGEventRef = *mut c_void;
type CGEventSourceRef = *mut c_void;
type CGImageRef = *mut c_void;
type CGContextRef = *mut c_void;
type CGColorSpaceRef = *mut c_void;
type CGDisplayModeRef = *mut c_void;
type AXUIElementRef = *mut c_void;
type CFMachPortRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFRunLoopRef = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

type CGEventTapCallBack = extern "C" fn(
    proxy: *mut c_void,
    kind: u32,
    event: CGEventRef,
    info: *mut c_void,
) -> CGEventRef;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGGetActiveDisplayList(max: u32, displays: *mut u32, count: *mut u32) -> i32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGDisplayCopyDisplayMode(display: u32) -> CGDisplayModeRef;
    fn CGDisplayModeGetPixelWidth(mode: CGDisplayModeRef) -> usize;
    fn CGDisplayModeRelease(mode: CGDisplayModeRef);
    fn CGDisplayIsAsleep(display: u32) -> u32;
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> CFArrayRef;
    fn CGImageGetWidth(image: CGImageRef) -> usize;
    fn CGImageGetHeight(image: CGImageRef) -> usize;
    fn CGImageRelease(image: CGImageRef);
    fn CGImageRetain(image: CGImageRef) -> CGImageRef;
    fn CGColorSpaceCreateDeviceRGB() -> CGColorSpaceRef;
    fn CGColorSpaceRelease(space: CGColorSpaceRef);
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits: usize,
        row: usize,
        space: CGColorSpaceRef,
        info: u32,
    ) -> CGContextRef;
    fn CGContextDrawImage(context: CGContextRef, rect: CGRect, image: CGImageRef);
    fn CGContextRelease(context: CGContextRef);
    fn CGEventSourceCreate(state: i32) -> CGEventSourceRef;
    fn CGEventSourceSetUserData(source: CGEventSourceRef, data: i64);
    fn CGEventCreateMouseEvent(
        source: CGEventSourceRef,
        kind: u32,
        point: CGPoint,
        button: u32,
    ) -> CGEventRef;
    fn CGEventCreateKeyboardEvent(source: CGEventSourceRef, key: u16, down: bool) -> CGEventRef;
    fn CGEventCreateScrollWheelEvent2(
        source: CGEventSourceRef,
        units: u32,
        count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> CGEventRef;
    fn CGEventSetLocation(event: CGEventRef, point: CGPoint);
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
    fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventKeyboardSetUnicodeString(event: CGEventRef, len: usize, chars: *const u16);
    fn CGEventPost(tap: u32, event: CGEventRef);
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        mask: u64,
        callback: CGEventTapCallBack,
        info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
    fn CGSessionCopyCurrentDictionary() -> CFDictionaryRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(rl: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRun();
    static kCFRunLoopCommonModes: CFStringRef;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
    fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementSetAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> i32;
    fn AXUIElementPerformAction(element: AXUIElementRef, action: CFStringRef) -> i32;
    fn AXUIElementGetPid(element: AXUIElementRef, pid: *mut i32) -> i32;
    fn AXUIElementCopyElementAtPosition(
        element: AXUIElementRef,
        x: f32,
        y: f32,
        found: *mut AXUIElementRef,
    ) -> i32;
    fn AXUIElementSetMessagingTimeout(element: AXUIElementRef, seconds: f32) -> i32;
    fn _AXUIElementGetWindow(element: AXUIElementRef, id: *mut u32) -> i32;
}

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn IsSecureEventInputEnabled() -> bool;
}

#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOPMAssertionCreateWithName(
        kind: CFStringRef,
        level: u32,
        name: CFStringRef,
        id: *mut u32,
    ) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
}

// ScreenCaptureKit is used through the Objective-C runtime; linking it
// (and Foundation) makes its classes available.
#[link(name = "ScreenCaptureKit", kind = "framework")]
extern "C" {}
#[link(name = "Foundation", kind = "framework")]
extern "C" {}

extern "C" {
    fn proc_pidpath(pid: i32, buffer: *mut c_void, size: u32) -> i32;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
}

const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

// CGWindowList options
const kCGWindowListOptionAll: u32 = 0;
const kCGWindowListOptionOnScreenOnly: u32 = 1;
const kCGWindowListOptionIncludingWindow: u32 = 8;
const kCGWindowListExcludeDesktopElements: u32 = 16;
const kCGWindowImageBoundsIgnoreFraming: u32 = 1;
const kCGWindowImageBestResolution: u32 = 8;

// Events
const kCGHIDEventTap: u32 = 0;
const kCGSessionEventTap: u32 = 1;
const kCGHeadInsertEventTap: u32 = 0;
const kCGEventTapOptionListenOnly: u32 = 1;
const kCGEventSourceStateHIDSystemState: i32 = 1;
const kCGEventSourceUserData: u32 = 42;
const kCGMouseEventClickState: u32 = 1;
const kCGScrollEventUnitLine: u32 = 1;
const kCGEventLeftMouseDown: u32 = 1;
const kCGEventLeftMouseUp: u32 = 2;
const kCGEventRightMouseDown: u32 = 3;
const kCGEventRightMouseUp: u32 = 4;
const kCGEventMouseMoved: u32 = 5;
const kCGEventLeftMouseDragged: u32 = 6;
const kCGEventRightMouseDragged: u32 = 7;
const kCGEventKeyDown: u32 = 10;
const kCGEventKeyUp: u32 = 11;
const kCGEventFlagsChanged: u32 = 12;
const kCGEventScrollWheel: u32 = 22;
const kCGEventOtherMouseDown: u32 = 25;
const kCGEventOtherMouseUp: u32 = 26;
const kCGEventOtherMouseDragged: u32 = 27;
const kCGEventTapDisabledByTimeout: u32 = 0xFFFF_FFFE;
const kCGEventTapDisabledByUserInput: u32 = 0xFFFF_FFFF;
const kCGEventFlagMaskShift: u64 = 0x0002_0000;
const kCGEventFlagMaskControl: u64 = 0x0004_0000;
const kCGEventFlagMaskAlternate: u64 = 0x0008_0000;
const kCGEventFlagMaskCommand: u64 = 0x0010_0000;

// Bitmaps
const kCGImageAlphaPremultipliedLast: u32 = 1;
const kCGBitmapByteOrder32Big: u32 = 4 << 12;

fn cfstr(s: &str) -> CFString {
    CFString::new(s)
}

/// A CF object owned by us, released on drop.
struct Owned(CFTypeRef);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

// ─── Reading window and session dictionaries ───────────────────────────

fn dict_value(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<CFType> {
    dict.find(cfstr(key)).map(|v| v.clone())
}

fn as_i64(v: &CFType) -> Option<i64> {
    v.downcast::<CFNumber>().and_then(|n| n.to_i64())
}

fn as_f64(v: &CFType) -> Option<f64> {
    v.downcast::<CFNumber>().and_then(|n| n.to_f64())
}

fn as_string(v: &CFType) -> Option<String> {
    v.downcast::<CFString>().map(|s| s.to_string())
}

fn as_bool(v: &CFType) -> Option<bool> {
    v.downcast::<CFBoolean>().map(bool::from)
}

/// One entry of `CGWindowListCopyWindowInfo`.
#[derive(Debug, Clone)]
struct RawWindow {
    id: u64,
    pid: u32,
    app: String,
    title: String,
    layer: i64,
    bounds: Rect,
    on_screen: bool,
    alpha: f64,
}

fn window_list(option: u32, relative_to: u32) -> Vec<RawWindow> {
    let raw = unsafe { CGWindowListCopyWindowInfo(option, relative_to) };
    if raw.is_null() {
        return Vec::new();
    }
    let array: CFArray<CFDictionary<CFString, CFType>> =
        unsafe { CFArray::wrap_under_create_rule(raw.cast()) };
    array
        .iter()
        .filter_map(|dict| {
            let id = dict_value(&dict, "kCGWindowNumber").and_then(|v| as_i64(&v))?;
            let pid = dict_value(&dict, "kCGWindowOwnerPID").and_then(|v| as_i64(&v))?;
            let bounds = dict_value(&dict, "kCGWindowBounds")
                .and_then(|v| v.downcast::<CFDictionary>())
                .map(|b| {
                    let b: CFDictionary<CFString, CFType> =
                        unsafe { CFDictionary::wrap_under_get_rule(b.as_concrete_TypeRef()) };
                    let get = |k| dict_value(&b, k).and_then(|v| as_f64(&v)).unwrap_or(0.0);
                    Rect::new(get("X"), get("Y"), get("Width"), get("Height"))
                })?;
            Some(RawWindow {
                id: u64::try_from(id).ok()?,
                pid: u32::try_from(pid).ok()?,
                app: dict_value(&dict, "kCGWindowOwnerName")
                    .and_then(|v| as_string(&v))
                    .unwrap_or_default(),
                title: dict_value(&dict, "kCGWindowName")
                    .and_then(|v| as_string(&v))
                    .unwrap_or_default(),
                layer: dict_value(&dict, "kCGWindowLayer")
                    .and_then(|v| as_i64(&v))
                    .unwrap_or(0),
                bounds,
                on_screen: dict_value(&dict, "kCGWindowIsOnscreen")
                    .and_then(|v| as_bool(&v))
                    .unwrap_or(false),
                alpha: dict_value(&dict, "kCGWindowAlpha")
                    .and_then(|v| as_f64(&v))
                    .unwrap_or(1.0),
            })
        })
        .collect()
}

/// System processes whose windows mean a password prompt is up.
const SYSTEM_PROMPTS: &[&str] = &["SecurityAgent", "coreautha", "coreauthd"];

// ─── Accessibility helpers ─────────────────────────────────────────────

fn ax_copy(element: AXUIElementRef, attribute: &str) -> Option<Owned> {
    let name = cfstr(attribute);
    let mut value: CFTypeRef = std::ptr::null();
    let err = unsafe {
        AXUIElementCopyAttributeValue(element, name.as_concrete_TypeRef().cast(), &mut value)
    };
    (err == 0 && !value.is_null()).then(|| Owned(value))
}

fn ax_set_bool(element: AXUIElementRef, attribute: &str, on: bool) -> i32 {
    let name = cfstr(attribute);
    let value = if on {
        CFBoolean::true_value()
    } else {
        CFBoolean::false_value()
    };
    unsafe {
        AXUIElementSetAttributeValue(
            element,
            name.as_concrete_TypeRef().cast(),
            value.as_concrete_TypeRef().cast(),
        )
    }
}

fn ax_window_element(app: AXUIElementRef, window: u64) -> Option<Owned> {
    let windows = ax_copy(app, "AXWindows")?;
    let array: CFArray<CFType> = unsafe { CFArray::wrap_under_get_rule(windows.0.cast()) };
    for item in array.iter() {
        let el: AXUIElementRef = item.as_CFTypeRef().cast_mut();
        let mut id = 0u32;
        if unsafe { _AXUIElementGetWindow(el, &mut id) } == 0 && u64::from(id) == window {
            unsafe { core_foundation::base::CFRetain(el.cast()) };
            return Some(Owned(el.cast_const()));
        }
    }
    None
}

// ─── The platform ──────────────────────────────────────────────────────

pub struct MacPlatform {
    source: SendPtr,
}

struct SendPtr(CGEventSourceRef);
// The event source is only used to create events, which CoreGraphics
// allows from any thread.
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}

impl MacPlatform {
    pub fn new() -> Self {
        let source = unsafe { CGEventSourceCreate(kCGEventSourceStateHIDSystemState) };
        if !source.is_null() {
            unsafe { CGEventSourceSetUserData(source, EVENT_TAG) };
        }
        Self {
            source: SendPtr(source),
        }
    }

    fn responsible_app() -> Option<String> {
        type Responsible = unsafe extern "C" fn(i32) -> i32;
        let sym = unsafe {
            dlsym(
                RTLD_DEFAULT,
                c"responsibility_get_pid_responsible_for_pid".as_ptr(),
            )
        };
        let me = i32::try_from(std::process::id()).ok()?;
        let pid = if sym.is_null() {
            me
        } else {
            let f: Responsible = unsafe { std::mem::transmute(sym) };
            unsafe { f(me) }
        };
        let mut buf = vec![0u8; 4096];
        let n = unsafe { proc_pidpath(pid, buf.as_mut_ptr().cast(), 4096) };
        if n <= 0 {
            return None;
        }
        let path = String::from_utf8_lossy(&buf[..usize::try_from(n).ok()?]).to_string();
        // The innermost app bundle holds the grants.
        match path.rfind(".app/") {
            Some(i) => Some(path[..i + 4].to_string()),
            None => Some(path),
        }
    }

    fn app_label() -> String {
        Self::responsible_app().map_or_else(
            || "the app that started Peko (Peko, or your terminal)".to_string(),
            |p| {
                let name = std::path::Path::new(&p)
                    .file_name()
                    .map_or(p.clone(), |n| n.to_string_lossy().to_string());
                format!("{name} ({p})")
            },
        )
    }

    fn need_screen_recording() -> Result<()> {
        if unsafe { CGPreflightScreenCaptureAccess() } {
            return Ok(());
        }
        bail!(
            "macOS has not given {} permission to record the screen. Ask your owner to turn it on \
             in System Settings → Privacy & Security → Screen & System Audio Recording (or to \
             run `peko computer setup`), then restart Peko.",
            Self::app_label()
        )
    }

    fn need_accessibility() -> Result<()> {
        if unsafe { AXIsProcessTrusted() } {
            return Ok(());
        }
        bail!(
            "macOS has not given {} permission to control the computer. Ask your owner to turn it \
             on in System Settings → Privacy & Security → Accessibility (or to run `peko computer \
             setup`).",
            Self::app_label()
        )
    }

    fn post_event(&self, event: CGEventRef) -> Result<()> {
        if event.is_null() {
            bail!("macOS did not create the event");
        }
        unsafe {
            CGEventSetIntegerValueField(event, kCGEventSourceUserData, EVENT_TAG);
            CGEventPost(kCGHIDEventTap, event);
            CFRelease(event.cast_const());
        }
        Ok(())
    }

    fn post_key(&self, code: u16, down: bool, flags: u64) -> Result<()> {
        let event = unsafe { CGEventCreateKeyboardEvent(self.source.0, code, down) };
        if !event.is_null() {
            unsafe { CGEventSetFlags(event, flags) };
        }
        self.post_event(event)
    }

    fn post_chord(&self, chord: &Chord) -> Result<()> {
        let code = key_code(chord.key).ok_or_else(|| anyhow!("no such key on a Mac"))?;
        let mods: Vec<(bool, u16, u64)> = vec![
            (chord.mods.cmd, 0x37, kCGEventFlagMaskCommand),
            (chord.mods.ctrl, 0x3B, kCGEventFlagMaskControl),
            (chord.mods.alt, 0x3A, kCGEventFlagMaskAlternate),
            (chord.mods.shift, 0x38, kCGEventFlagMaskShift),
        ];
        let mut flags = 0u64;
        for &(on, code, flag) in &mods {
            if on {
                flags |= flag;
                self.post_key(code, true, flags)?;
            }
        }
        self.post_key(code, true, flags)?;
        self.post_key(code, false, flags)?;
        for &(on, code, flag) in mods.iter().rev() {
            if on {
                flags &= !flag;
                self.post_key(code, false, flags)?;
            }
        }
        Ok(())
    }

    fn post_text(&self, text: &str) -> Result<()> {
        let units: Vec<u16> = text.encode_utf16().collect();
        for down in [true, false] {
            let event = unsafe { CGEventCreateKeyboardEvent(self.source.0, 0, down) };
            if !event.is_null() {
                unsafe {
                    CGEventSetFlags(event, 0);
                    CGEventKeyboardSetUnicodeString(event, units.len(), units.as_ptr());
                }
            }
            self.post_event(event)?;
        }
        Ok(())
    }

    fn screen_scale_at(x: f64, y: f64) -> f64 {
        Self::screens_now()
            .iter()
            .find(|s| s.bounds.contains(x, y))
            .or(Self::screens_now().iter().find(|s| s.main))
            .map_or(2.0, |s| s.scale)
    }

    fn screens_now() -> Vec<Screen> {
        let mut ids = [0u32; 16];
        let mut count = 0u32;
        if unsafe { CGGetActiveDisplayList(16, ids.as_mut_ptr(), &mut count) } != 0 {
            return Vec::new();
        }
        let main = unsafe { CGMainDisplayID() };
        ids[..count as usize]
            .iter()
            .map(|&id| {
                let b = unsafe { CGDisplayBounds(id) };
                let mode = unsafe { CGDisplayCopyDisplayMode(id) };
                let pixels = if mode.is_null() {
                    b.size.width
                } else {
                    #[allow(clippy::cast_precision_loss)]
                    let w = unsafe { CGDisplayModeGetPixelWidth(mode) } as f64;
                    unsafe { CGDisplayModeRelease(mode) };
                    w
                };
                Screen {
                    id: u64::from(id),
                    bounds: Rect::new(b.origin.x, b.origin.y, b.size.width, b.size.height),
                    scale: if b.size.width > 0.0 {
                        (pixels / b.size.width).max(1.0)
                    } else {
                        1.0
                    },
                    main: id == main,
                }
            })
            .collect()
    }

    fn system_events_raise(pid: u32) {
        let script = format!(
            "tell application \"System Events\" to set frontmost of (first process whose unix id \
             is {pid}) to true"
        );
        let child = std::process::Command::new("/usr/bin/osascript")
            .args(["-e", &script])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if let Ok(mut child) = child {
            for _ in 0..20 {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
        }
    }

    /// The focused window of app `pid`, per Accessibility: the window that
    /// takes keys. System overlays drawn in the app's process (an
    /// autocorrect bubble, an input method's candidates) sit above it in the
    /// window list but are never focused.
    fn focused_window(pid: u32) -> Option<u64> {
        let app = unsafe { AXUIElementCreateApplication(i32::try_from(pid).ok()?) };
        if app.is_null() {
            return None;
        }
        let app = Owned(app.cast_const());
        unsafe { AXUIElementSetMessagingTimeout(app.0.cast_mut(), 1.0) };
        let window = ax_copy(app.0.cast_mut(), "AXFocusedWindow")?;
        let mut id = 0u32;
        (unsafe { _AXUIElementGetWindow(window.0.cast_mut(), &mut id) } == 0 && id != 0)
            .then_some(u64::from(id))
    }

    fn frontmost_pid() -> Option<u32> {
        let system = unsafe { AXUIElementCreateSystemWide() };
        if system.is_null() {
            return None;
        }
        let system = Owned(system.cast_const());
        unsafe { AXUIElementSetMessagingTimeout(system.0.cast_mut(), 1.0) };
        let app = ax_copy(system.0.cast_mut(), "AXFocusedApplication")?;
        let mut pid = 0i32;
        (unsafe { AXUIElementGetPid(app.0.cast_mut(), &mut pid) } == 0)
            .then(|| u32::try_from(pid).ok())
            .flatten()
    }
}

impl Platform for MacPlatform {
    fn system(&self) -> &'static str {
        "macos"
    }

    fn unavailable(&self) -> Option<String> {
        None
    }

    fn permissions(&self) -> Permissions {
        Permissions {
            screen_recording: Some(unsafe { CGPreflightScreenCaptureAccess() }),
            accessibility: Some(unsafe { AXIsProcessTrusted() }),
            responsible_app: Self::responsible_app(),
        }
    }

    fn request_permissions(&self) -> Result<Vec<String>> {
        let mut opened = Vec::new();
        if !unsafe { AXIsProcessTrusted() } {
            let key = cfstr("AXTrustedCheckOptionPrompt");
            let options = CFDictionary::from_CFType_pairs(&[(
                key.as_CFType(),
                CFBoolean::true_value().as_CFType(),
            )]);
            unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef().cast()) };
            let _ = std::process::Command::new("/usr/bin/open")
                .arg(
                    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
                )
                .status();
            opened.push("Accessibility".to_string());
        }
        if !unsafe { CGPreflightScreenCaptureAccess() } {
            unsafe { CGRequestScreenCaptureAccess() };
            let _ = std::process::Command::new("/usr/bin/open")
                .arg(
                    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
                )
                .status();
            opened.push("Screen Recording".to_string());
        }
        Ok(opened)
    }

    fn screens(&self) -> Result<Vec<Screen>> {
        let screens = Self::screens_now();
        anyhow::ensure!(!screens.is_empty(), "macOS reports no display");
        Ok(screens)
    }

    fn windows(&self) -> Result<Vec<WindowInfo>> {
        let on = window_list(
            kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
            0,
        );
        let mut out: Vec<WindowInfo> = on
            .iter()
            .filter(|w| w.layer == 0 && w.alpha > 0.0)
            .map(|w| WindowInfo {
                id: w.id,
                pid: w.pid,
                app: w.app.clone(),
                title: w.title.clone(),
                bounds: w.bounds,
                minimized: false,
            })
            .collect();
        // Windows that are not on screen: minimized, of a hidden app, or
        // on another desktop. Only titled ones (untitled ones are helpers).
        for w in window_list(
            kCGWindowListOptionAll | kCGWindowListExcludeDesktopElements,
            0,
        ) {
            if w.layer == 0
                && !w.on_screen
                && !w.title.is_empty()
                && w.bounds.width >= 50.0
                && !out.iter().any(|o| o.id == w.id)
            {
                out.push(WindowInfo {
                    id: w.id,
                    pid: w.pid,
                    app: w.app,
                    title: w.title,
                    bounds: w.bounds,
                    minimized: true,
                });
            }
        }
        Ok(out)
    }

    fn front(&self) -> Result<Front> {
        let on = window_list(
            kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
            0,
        );
        let pid = Self::frontmost_pid()
            .or_else(|| on.iter().find(|w| w.layer == 0).map(|w| w.pid))
            .ok_or_else(|| anyhow!("macOS does not say which app is in front"))?;
        let window = Self::focused_window(pid).or_else(|| {
            on.iter()
                .find(|w| {
                    w.pid == pid && w.layer == 0 && w.alpha > 0.0 && w.title != INDICATOR_TITLE
                })
                .map(|w| w.id)
        });
        let app = on
            .iter()
            .find(|w| w.pid == pid)
            .map(|w| w.app.clone())
            .unwrap_or_else(|| format!("process {pid}"));
        Ok(Front { pid, app, window })
    }

    fn raise(&self, window: &WindowInfo) -> Result<()> {
        Self::need_accessibility()?;
        let pid = i32::try_from(window.pid)?;
        let app = unsafe { AXUIElementCreateApplication(pid) };
        anyhow::ensure!(!app.is_null(), "no such app");
        let app = Owned(app.cast_const());
        let el = ax_window_element(app.0.cast_mut(), window.id);
        if let Some(el) = el.as_ref() {
            if window.minimized {
                ax_set_bool(el.0.cast_mut(), "AXMinimized", false);
            }
        }
        ax_set_bool(app.0.cast_mut(), "AXFrontmost", true);
        if let Some(el) = el.as_ref() {
            let raise = cfstr("AXRaise");
            unsafe {
                AXUIElementPerformAction(el.0.cast_mut(), raise.as_concrete_TypeRef().cast());
            }
            ax_set_bool(el.0.cast_mut(), "AXMain", true);
        }
        std::thread::sleep(Duration::from_millis(120));
        if Self::frontmost_pid() != Some(window.pid) {
            Self::system_events_raise(window.pid);
        }
        Ok(())
    }

    fn owner_at(&self, x: f64, y: f64) -> Option<(u32, String)> {
        let system = unsafe { AXUIElementCreateSystemWide() };
        if system.is_null() {
            return None;
        }
        let system = Owned(system.cast_const());
        let mut found: AXUIElementRef = std::ptr::null_mut();
        #[allow(clippy::cast_possible_truncation)]
        let err = unsafe {
            AXUIElementCopyElementAtPosition(system.0.cast_mut(), x as f32, y as f32, &mut found)
        };
        if err != 0 || found.is_null() {
            return None;
        }
        let found = Owned(found.cast_const());
        let mut pid = 0i32;
        if unsafe { AXUIElementGetPid(found.0.cast_mut(), &mut pid) } != 0 {
            return None;
        }
        let pid = u32::try_from(pid).ok()?;
        let app = window_list(kCGWindowListOptionOnScreenOnly, 0)
            .into_iter()
            .find(|w| w.pid == pid)
            .map_or_else(|| format!("process {pid}"), |w| w.app);
        Some((pid, app))
    }

    fn obstruction(&self) -> Option<String> {
        let session = unsafe { CGSessionCopyCurrentDictionary() };
        if !session.is_null() {
            let dict: CFDictionary<CFString, CFType> =
                unsafe { CFDictionary::wrap_under_create_rule(session.cast()) };
            if dict_value(&dict, "CGSSessionScreenIsLocked")
                .and_then(|v| as_bool(&v))
                .unwrap_or(false)
            {
                return Some(
                    "The screen is locked; nothing can be seen or done until your owner unlocks \
                     it."
                    .into(),
                );
            }
            if dict_value(&dict, "kCGSSessionOnConsoleKey").and_then(|v| as_bool(&v)) == Some(false)
            {
                return Some("Another user is using this computer's screen.".into());
            }
        }
        if unsafe { CGDisplayIsAsleep(CGMainDisplayID()) } != 0 {
            return Some("The display is asleep.".into());
        }
        let on = window_list(kCGWindowListOptionOnScreenOnly, 0);
        if on.iter().any(|w| w.app == "ScreenSaverEngine") {
            return Some("The screen saver is on.".into());
        }
        if let Some(w) = on.iter().find(|w| SYSTEM_PROMPTS.contains(&w.app.as_str())) {
            return Some(format!(
                "A system password prompt is open ({}); it is for your owner. Nothing was done.",
                w.app
            ));
        }
        None
    }

    fn secure_input(&self) -> bool {
        unsafe { IsSecureEventInputEnabled() }
    }

    fn unreachable(&self, _window: &WindowInfo) -> Option<String> {
        None
    }

    fn capture(&self, target: Target) -> Result<Image> {
        Self::need_screen_recording()?;
        let (bounds, scale) = match target {
            Target::Screen(id) => {
                let s = Self::screens_now()
                    .into_iter()
                    .find(|s| s.id == id)
                    .ok_or_else(|| anyhow!("there is no screen {id}"))?;
                (s.bounds, s.scale)
            }
            Target::Window(id) => {
                let w = window_list(kCGWindowListOptionIncludingWindow, u32::try_from(id)?)
                    .into_iter()
                    .find(|w| w.id == id)
                    .ok_or_else(|| anyhow!("window {id} is gone"))?;
                let scale = Self::screen_scale_at(
                    w.bounds.x + w.bounds.width / 2.0,
                    w.bounds.y + w.bounds.height / 2.0,
                );
                (w.bounds, scale)
            }
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let size = (
            (bounds.width * scale).round().max(1.0) as usize,
            (bounds.height * scale).round().max(1.0) as usize,
        );
        let image = match sck::capture(target, size) {
            Ok(Some(image)) => image,
            Ok(None) => cg_capture(target, bounds)?,
            Err(e) => {
                tracing::debug!("computer: ScreenCaptureKit failed ({e:#}); using CoreGraphics");
                cg_capture(target, bounds)?
            }
        };
        let rgba = image_rgba(image.0);
        unsafe { CGImageRelease(image.0) };
        rgba
    }

    fn post(&self, event: &InputEvent) -> Result<()> {
        Self::need_accessibility()?;
        let src = self.source.0;
        match event {
            InputEvent::Move { x, y, held } => {
                let (kind, button) = match held {
                    None => (kCGEventMouseMoved, 0),
                    Some(Button::Left) => (kCGEventLeftMouseDragged, 0),
                    Some(Button::Right) => (kCGEventRightMouseDragged, 1),
                    Some(Button::Middle) => (kCGEventOtherMouseDragged, 2),
                };
                let ev =
                    unsafe { CGEventCreateMouseEvent(src, kind, CGPoint { x: *x, y: *y }, button) };
                self.post_event(ev)
            }
            InputEvent::Press {
                x,
                y,
                button,
                down,
                count,
            } => {
                let (kind, b) = match (button, down) {
                    (Button::Left, true) => (kCGEventLeftMouseDown, 0),
                    (Button::Left, false) => (kCGEventLeftMouseUp, 0),
                    (Button::Right, true) => (kCGEventRightMouseDown, 1),
                    (Button::Right, false) => (kCGEventRightMouseUp, 1),
                    (Button::Middle, true) => (kCGEventOtherMouseDown, 2),
                    (Button::Middle, false) => (kCGEventOtherMouseUp, 2),
                };
                let ev = unsafe { CGEventCreateMouseEvent(src, kind, CGPoint { x: *x, y: *y }, b) };
                if !ev.is_null() {
                    unsafe {
                        CGEventSetIntegerValueField(ev, kCGMouseEventClickState, i64::from(*count));
                    }
                }
                self.post_event(ev)
            }
            InputEvent::Scroll { x, y, dx, dy } => {
                // CoreGraphics counts up and left as positive.
                let ev = unsafe {
                    CGEventCreateScrollWheelEvent2(src, kCGScrollEventUnitLine, 2, -*dy, -*dx, 0)
                };
                if !ev.is_null() {
                    unsafe { CGEventSetLocation(ev, CGPoint { x: *x, y: *y }) };
                }
                self.post_event(ev)
            }
            InputEvent::Chord(chord) => self.post_chord(chord),
            InputEvent::Text(text) => self.post_text(text),
        }
    }

    fn watch_input(&self, owner_input: Arc<dyn Fn() + Send + Sync>) -> Result<()> {
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        std::thread::Builder::new()
            .name("peko-computer-watch".into())
            .spawn(move || tap_thread(owner_input, &tx))?;
        rx.recv_timeout(Duration::from_secs(3))
            .map_err(|_| anyhow!("the input watch did not start"))?
            .map_err(|e| anyhow!(e))
    }

    fn hold_display_awake(&self) -> Option<Box<dyn Send>> {
        let kind = cfstr("PreventUserIdleDisplaySleep");
        let name = cfstr("A peko is using this computer");
        let mut id = 0u32;
        let ok = unsafe {
            IOPMAssertionCreateWithName(
                kind.as_concrete_TypeRef().cast(),
                255,
                name.as_concrete_TypeRef().cast(),
                &mut id,
            )
        } == 0;
        ok.then(|| Box::new(Assertion(id)) as Box<dyn Send>)
    }
}

/// A power assertion, released on drop.
struct Assertion(u32);

impl Drop for Assertion {
    fn drop(&mut self) {
        unsafe { IOPMAssertionRelease(self.0) };
    }
}

// ─── Watching for the owner's hand ─────────────────────────────────────

struct TapInfo {
    owner_input: Arc<dyn Fn() + Send + Sync>,
    port: std::sync::atomic::AtomicPtr<c_void>,
}

extern "C" fn tap_callback(
    _proxy: *mut c_void,
    kind: u32,
    event: CGEventRef,
    info: *mut c_void,
) -> CGEventRef {
    let info = unsafe { &*(info as *const TapInfo) };
    if kind == kCGEventTapDisabledByTimeout || kind == kCGEventTapDisabledByUserInput {
        let port = info.port.load(std::sync::atomic::Ordering::Relaxed);
        if !port.is_null() {
            unsafe { CGEventTapEnable(port, true) };
        }
        return event;
    }
    if !event.is_null()
        && unsafe { CGEventGetIntegerValueField(event, kCGEventSourceUserData) } != EVENT_TAG
    {
        (info.owner_input)();
    }
    event
}

fn mask(kinds: &[u32]) -> u64 {
    kinds.iter().fold(0u64, |m, k| m | (1u64 << k))
}

fn tap_thread(
    owner_input: Arc<dyn Fn() + Send + Sync>,
    started: &mpsc::Sender<Result<(), String>>,
) {
    let mouse = [
        kCGEventLeftMouseDown,
        kCGEventLeftMouseUp,
        kCGEventRightMouseDown,
        kCGEventRightMouseUp,
        kCGEventMouseMoved,
        kCGEventLeftMouseDragged,
        kCGEventRightMouseDragged,
        kCGEventScrollWheel,
        kCGEventOtherMouseDown,
        kCGEventOtherMouseUp,
        kCGEventOtherMouseDragged,
    ];
    let keys = [kCGEventKeyDown, kCGEventKeyUp, kCGEventFlagsChanged];
    let info = Box::into_raw(Box::new(TapInfo {
        owner_input,
        port: std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()),
    }));
    let create = |m: u64| unsafe {
        CGEventTapCreate(
            kCGSessionEventTap,
            kCGHeadInsertEventTap,
            kCGEventTapOptionListenOnly,
            m,
            tap_callback,
            info.cast(),
        )
    };
    let mut port = create(mask(&mouse) | mask(&keys));
    if port.is_null() {
        // Keyboard events need Input Monitoring; the mouse needs nothing.
        port = create(mask(&mouse));
        if !port.is_null() {
            tracing::info!("computer: watching the owner's mouse only (no Input Monitoring)");
        }
    }
    if port.is_null() {
        let _ = started.send(Err("macOS refused to watch for input".into()));
        drop(unsafe { Box::from_raw(info) });
        return;
    }
    unsafe {
        (*info)
            .port
            .store(port, std::sync::atomic::Ordering::Relaxed)
    };
    let source = unsafe { CFMachPortCreateRunLoopSource(std::ptr::null(), port, 0) };
    unsafe {
        CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
        CGEventTapEnable(port, true);
    }
    let _ = started.send(Ok(()));
    unsafe { CFRunLoopRun() };
}

// ─── Pictures ──────────────────────────────────────────────────────────

/// A CGImage we own.
struct CgImage(CGImageRef);
unsafe impl Send for CgImage {}

fn image_rgba(image: CGImageRef) -> Result<Image> {
    let (w, h) = unsafe { (CGImageGetWidth(image), CGImageGetHeight(image)) };
    anyhow::ensure!(w > 0 && h > 0, "the picture is empty");
    let mut data = vec![0u8; w * h * 4];
    let space = unsafe { CGColorSpaceCreateDeviceRGB() };
    let context = unsafe {
        CGBitmapContextCreate(
            data.as_mut_ptr().cast(),
            w,
            h,
            8,
            w * 4,
            space,
            kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big,
        )
    };
    unsafe { CGColorSpaceRelease(space) };
    anyhow::ensure!(!context.is_null(), "could not read the picture");
    #[allow(clippy::cast_precision_loss)]
    let rect = CGRect {
        origin: CGPoint::default(),
        size: CGSize {
            width: w as f64,
            height: h as f64,
        },
    };
    unsafe {
        CGContextDrawImage(context, rect, image);
        CGContextRelease(context);
    }
    Ok(Image {
        width: u32::try_from(w)?,
        height: u32::try_from(h)?,
        rgba: data,
    })
}

/// `CGWindowListCreateImage` and friends: obsolete in the SDK, still in
/// the system, used where ScreenCaptureKit cannot.
fn cg_capture(target: Target, bounds: Rect) -> Result<CgImage> {
    type CreateImage = unsafe extern "C" fn(CGRect, u32, u32, u32) -> CGImageRef;
    type CreateFromArray = unsafe extern "C" fn(CGRect, CFArrayRef, u32) -> CGImageRef;
    let lib = unsafe {
        dlopen(
            c"/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics".as_ptr(),
            1,
        )
    };
    let lookup = |name: &CStr| unsafe { dlsym(lib, name.as_ptr()) };
    let rect = CGRect {
        origin: CGPoint {
            x: bounds.x,
            y: bounds.y,
        },
        size: CGSize {
            width: bounds.width,
            height: bounds.height,
        },
    };
    let image = match target {
        Target::Window(id) => {
            let f = lookup(c"CGWindowListCreateImage");
            anyhow::ensure!(!f.is_null(), "this macOS cannot take a picture of a window");
            let f: CreateImage = unsafe { std::mem::transmute(f) };
            let null = CGRect {
                origin: CGPoint {
                    x: f64::INFINITY,
                    y: f64::INFINITY,
                },
                size: CGSize::default(),
            };
            unsafe {
                f(
                    null,
                    kCGWindowListOptionIncludingWindow,
                    u32::try_from(id)?,
                    kCGWindowImageBoundsIgnoreFraming | kCGWindowImageBestResolution,
                )
            }
        }
        Target::Screen(_) => {
            // Every window on screen but the indicator, front to back.
            let ids: Vec<CFNumber> = window_list(kCGWindowListOptionOnScreenOnly, 0)
                .into_iter()
                .filter(|w| w.title != INDICATOR_TITLE)
                .map(|w| CFNumber::from(i64::try_from(w.id).unwrap_or(0)))
                .collect();
            let f = lookup(c"CGWindowListCreateImageFromArray");
            anyhow::ensure!(
                !f.is_null(),
                "this macOS cannot take a picture of the screen"
            );
            let f: CreateFromArray = unsafe { std::mem::transmute(f) };
            let array = CFArray::from_CFTypes(&ids);
            unsafe {
                f(
                    rect,
                    array.as_concrete_TypeRef().cast(),
                    kCGWindowImageBestResolution,
                )
            }
        }
    };
    anyhow::ensure!(!image.is_null(), "macOS gave no picture");
    Ok(CgImage(image))
}

/// ScreenCaptureKit (`SCScreenshotManager`, macOS 14+).
mod sck {
    use std::sync::mpsc;
    use std::time::Duration;

    use anyhow::{anyhow, Result};
    use block2::RcBlock;
    use objc2::rc::{autoreleasepool, Retained};
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2::{msg_send, sel};

    use super::{CGImageRef, CGImageRetain, CgImage, Target, INDICATOR_TITLE};

    struct SendObj(Retained<AnyObject>);
    // Moved once from ScreenCaptureKit's completion queue to the waiting
    // thread, which alone uses it after.
    unsafe impl Send for SendObj {}

    fn nsstring(obj: *mut AnyObject) -> String {
        if obj.is_null() {
            return String::new();
        }
        let p: *const std::ffi::c_char = unsafe { msg_send![obj, UTF8String] };
        if p.is_null() {
            return String::new();
        }
        unsafe { std::ffi::CStr::from_ptr(p) }
            .to_string_lossy()
            .to_string()
    }

    fn error_text(error: *mut AnyObject) -> String {
        if error.is_null() {
            return "no reason given".into();
        }
        let d: *mut AnyObject = unsafe { msg_send![error, localizedDescription] };
        nsstring(d)
    }

    fn shareable() -> Result<Retained<AnyObject>> {
        let cls = AnyClass::get(c"SCShareableContent")
            .ok_or_else(|| anyhow!("ScreenCaptureKit is not available"))?;
        let (tx, rx) = mpsc::channel::<Result<SendObj, String>>();
        let block = RcBlock::new(move |content: *mut AnyObject, error: *mut AnyObject| {
            let sent = match unsafe { Retained::retain(content) } {
                Some(c) => Ok(SendObj(c)),
                None => Err(error_text(error)),
            };
            let _ = tx.send(sent);
        });
        unsafe {
            let _: () = msg_send![
                cls,
                getShareableContentExcludingDesktopWindows: Bool::NO,
                onScreenWindowsOnly: Bool::YES,
                completionHandler: &*block
            ];
        }
        rx.recv_timeout(Duration::from_secs(5))
            .map_err(|_| anyhow!("ScreenCaptureKit did not answer"))?
            .map(|s| s.0)
            .map_err(|e| anyhow!("ScreenCaptureKit: {e}"))
    }

    fn items(array: *mut AnyObject) -> Vec<*mut AnyObject> {
        if array.is_null() {
            return Vec::new();
        }
        let count: usize = unsafe { msg_send![array, count] };
        (0..count)
            .map(|i| unsafe { msg_send![array, objectAtIndex: i] })
            .collect()
    }

    /// A picture of `target` at `size` pixels, or `None` where
    /// `SCScreenshotManager` does not exist (macOS 13).
    pub(super) fn capture(target: Target, size: (usize, usize)) -> Result<Option<CgImage>> {
        let Some(manager) = AnyClass::get(c"SCScreenshotManager") else {
            return Ok(None);
        };
        autoreleasepool(|_| {
            let content = shareable()?;
            let filter_cls = AnyClass::get(c"SCContentFilter")
                .ok_or_else(|| anyhow!("ScreenCaptureKit is not available"))?;
            let windows: *mut AnyObject = unsafe { msg_send![&*content, windows] };
            let windows = items(windows);
            let filter: Retained<AnyObject> = match target {
                Target::Window(id) => {
                    let window = windows
                        .iter()
                        .copied()
                        .find(|&w| {
                            let wid: u32 = unsafe { msg_send![w, windowID] };
                            u64::from(wid) == id
                        })
                        .ok_or_else(|| anyhow!("window {id} is not on screen"))?;
                    unsafe {
                        let alloc: *mut AnyObject = msg_send![filter_cls, alloc];
                        let f: *mut AnyObject =
                            msg_send![alloc, initWithDesktopIndependentWindow: window];
                        Retained::from_raw(f).ok_or_else(|| anyhow!("no filter"))?
                    }
                }
                Target::Screen(id) => {
                    let displays: *mut AnyObject = unsafe { msg_send![&*content, displays] };
                    let display = items(displays)
                        .into_iter()
                        .find(|&d| {
                            let did: u32 = unsafe { msg_send![d, displayID] };
                            u64::from(did) == id
                        })
                        .ok_or_else(|| anyhow!("there is no screen {id}"))?;
                    let array_cls = AnyClass::get(c"NSMutableArray")
                        .ok_or_else(|| anyhow!("Foundation is not available"))?;
                    let excluded: Retained<AnyObject> = unsafe { msg_send![array_cls, new] };
                    for &w in &windows {
                        let title: *mut AnyObject = unsafe { msg_send![w, title] };
                        if nsstring(title) == INDICATOR_TITLE {
                            unsafe {
                                let _: () = msg_send![&*excluded, addObject: w];
                            }
                        }
                    }
                    unsafe {
                        let alloc: *mut AnyObject = msg_send![filter_cls, alloc];
                        let f: *mut AnyObject = msg_send![
                            alloc,
                            initWithDisplay: display,
                            excludingWindows: &*excluded
                        ];
                        Retained::from_raw(f).ok_or_else(|| anyhow!("no filter"))?
                    }
                }
            };
            let config_cls = AnyClass::get(c"SCStreamConfiguration")
                .ok_or_else(|| anyhow!("ScreenCaptureKit is not available"))?;
            let config: Retained<AnyObject> = unsafe { msg_send![config_cls, new] };
            unsafe {
                let _: () = msg_send![&*config, setWidth: size.0];
                let _: () = msg_send![&*config, setHeight: size.1];
                let _: () = msg_send![&*config, setShowsCursor: Bool::NO];
                let shadows: Bool = msg_send![
                    &*config,
                    respondsToSelector: sel!(setIgnoreShadowsSingleWindow:)
                ];
                if shadows.as_bool() {
                    let _: () = msg_send![&*config, setIgnoreShadowsSingleWindow: Bool::YES];
                }
            }
            let (tx, rx) = mpsc::channel::<Result<CgImage, String>>();
            let block = RcBlock::new(move |image: CGImageRef, error: *mut AnyObject| {
                let sent = if image.is_null() {
                    Err(error_text(error))
                } else {
                    Ok(CgImage(unsafe { CGImageRetain(image) }))
                };
                let _ = tx.send(sent);
            });
            unsafe {
                let _: () = msg_send![
                    manager,
                    captureImageWithFilter: &*filter,
                    configuration: &*config,
                    completionHandler: &*block
                ];
            }
            rx.recv_timeout(Duration::from_secs(5))
                .map_err(|_| anyhow!("ScreenCaptureKit took no picture"))?
                .map(Some)
                .map_err(|e| anyhow!("ScreenCaptureKit: {e}"))
        })
    }
}

/// The macOS virtual key code of `key` (ANSI positions).
fn key_code(key: Key) -> Option<u16> {
    Some(match key {
        Key::Enter => 0x24,
        Key::Tab => 0x30,
        Key::Space => 0x31,
        Key::Backspace => 0x33,
        Key::Escape => 0x35,
        Key::Delete => 0x75,
        Key::Home => 0x73,
        Key::End => 0x77,
        Key::PageUp => 0x74,
        Key::PageDown => 0x79,
        Key::Left => 0x7B,
        Key::Right => 0x7C,
        Key::Down => 0x7D,
        Key::Up => 0x7E,
        Key::Insert => 0x72, // Help, where Insert sits on a Mac keyboard
        Key::F(n) => match n {
            1 => 0x7A,
            2 => 0x78,
            3 => 0x63,
            4 => 0x76,
            5 => 0x60,
            6 => 0x61,
            7 => 0x62,
            8 => 0x64,
            9 => 0x65,
            10 => 0x6D,
            11 => 0x67,
            12 => 0x6F,
            13 => 0x69,
            14 => 0x6B,
            15 => 0x71,
            16 => 0x6A,
            17 => 0x40,
            18 => 0x4F,
            19 => 0x50,
            20 => 0x5A,
            _ => return None,
        },
        Key::Char(c) => match c {
            'a' => 0x00,
            's' => 0x01,
            'd' => 0x02,
            'f' => 0x03,
            'h' => 0x04,
            'g' => 0x05,
            'z' => 0x06,
            'x' => 0x07,
            'c' => 0x08,
            'v' => 0x09,
            'b' => 0x0B,
            'q' => 0x0C,
            'w' => 0x0D,
            'e' => 0x0E,
            'r' => 0x0F,
            'y' => 0x10,
            't' => 0x11,
            '1' => 0x12,
            '2' => 0x13,
            '3' => 0x14,
            '4' => 0x15,
            '6' => 0x16,
            '5' => 0x17,
            '=' => 0x18,
            '9' => 0x19,
            '7' => 0x1A,
            '-' => 0x1B,
            '8' => 0x1C,
            '0' => 0x1D,
            ']' => 0x1E,
            'o' => 0x1F,
            'u' => 0x20,
            '[' => 0x21,
            'i' => 0x22,
            'p' => 0x23,
            'l' => 0x25,
            'j' => 0x26,
            '\'' => 0x27,
            'k' => 0x28,
            ';' => 0x29,
            '\\' => 0x2A,
            ',' => 0x2B,
            '/' => 0x2C,
            'n' => 0x2D,
            'm' => 0x2E,
            '.' => 0x2F,
            '`' => 0x32,
            _ => return None,
        },
    })
}

/// Real-screen checks, run by hand with `PEKO_TEST_COMPUTER=1` on a Mac
/// whose terminal has the grants.
#[cfg(test)]
mod real_screen {
    use super::*;

    fn wanted() -> bool {
        std::env::var("PEKO_TEST_COMPUTER").is_ok_and(|v| v == "1")
    }

    #[test]
    fn screens_windows_and_a_picture() {
        if !wanted() {
            return;
        }
        let mac = MacPlatform::new();
        let screens = mac.screens().unwrap();
        assert!(screens.iter().any(|s| s.main));
        let windows = mac.windows().unwrap();
        assert!(!windows.is_empty());
        let front = mac.front().unwrap();
        assert!(front.pid > 0);
        let main = screens.iter().find(|s| s.main).unwrap();
        let image = mac.capture(Target::Screen(main.id)).unwrap();
        assert!(image.width > 100);
        let win = windows.iter().find(|w| !w.minimized).unwrap();
        let image = mac.capture(Target::Window(win.id)).unwrap();
        assert!(image.width > 10);
        assert!(mac.obstruction().is_none());
    }
}
