//! `Computer` on Windows 10 1903+ and 11 (ADR-123 D4).
//!
//! - Coordinates: every call runs per-monitor DPI aware (v2) on its
//!   thread, so the desktop is in physical pixels and mixed-DPI monitors
//!   map correctly; a screen's `scale` is its DPI over 96.
//! - Windows: `EnumWindows` (visible, not cloaked by DWM, not tool
//!   windows), bounds from DWM's extended frame.
//! - Pictures: Windows.Graphics.Capture for a monitor or a window, without
//!   the yellow border where Windows allows it; `PrintWindow` / `BitBlt`
//!   where the capture API is unavailable.
//! - Input: `SendInput`; text with `KEYEVENTF_UNICODE`, surrogate pairs
//!   kept together. Windows of an elevated process are refused up front
//!   (UIPI would drop the input).
//! - Bringing a window forward: a synthetic Alt press, then
//!   `SetForegroundWindow` (Windows lets the process that sent the last
//!   input set the foreground); `ShowWindow(SW_RESTORE)` when minimized.
//! - The owner's hand: low-level mouse and keyboard hooks on their own
//!   thread; events flagged injected are ignored.
//! - Locked / secure desktop: `OpenInputDesktop` fails or names another
//!   desktop than `Default`.
//! - A service (session 0) has no desktop: the tool is unavailable there.

// FFI out-parameters are passed as `&mut`, which the bindings take.
#![allow(
    clippy::borrow_as_ptr,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use windows::core::{Interface, BOOL, PWSTR};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
    EnumDisplayMonitors, GetDC, GetDIBits, GetMonitorInfoW, ReleaseDC, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HDC, HMONITOR, MONITORINFO, SRCCOPY,
};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::Com::CoIncrementMTAUsage;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_CONTROL_FLAGS,
    DESKTOP_READOBJECTS, UOI_NAME,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::HiDpi::{
    GetDpiForMonitor, SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, MDT_EFFECTIVE_DPI,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, VkKeyScanW, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, MOUSE_EVENT_FLAGS,
    VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_HOME,
    VK_INSERT, VK_LEFT, VK_LWIN, VK_MENU, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT,
    VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CallNextHookEx, EnumWindows, GetAncestor, GetForegroundWindow, GetMessageW,
    GetSystemMetrics, GetWindowLongW, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, SetForegroundWindow, SetWindowsHookExW,
    ShowWindow, SystemParametersInfoW, WindowFromPoint, GA_ROOT, GWL_EXSTYLE, KBDLLHOOKSTRUCT,
    LLKHF_INJECTED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SPI_GETSCREENSAVERRUNNING, SW_RESTORE,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WH_KEYBOARD_LL, WH_MOUSE_LL, WS_EX_TOOLWINDOW,
};

use super::keys::{Chord, Key};
use super::{
    Button, Front, Image, InputEvent, Permissions, Platform, Rect, Screen, Target, WindowInfo,
};

const WHEEL_DELTA: i32 = 120;

/// Run `f` per-monitor DPI aware (v2) on this thread, then put the
/// thread's awareness back.
fn dpi_aware<T>(f: impl FnOnce() -> T) -> T {
    let old: DPI_AWARENESS_CONTEXT =
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let out = f();
    if !old.0.is_null() {
        unsafe { SetThreadDpiAwarenessContext(old) };
    }
    out
}

fn hwnd(id: u64) -> HWND {
    HWND(id as usize as *mut core::ffi::c_void)
}

fn hwnd_id(h: HWND) -> u64 {
    h.0 as usize as u64
}

fn rect(r: RECT) -> Rect {
    Rect::new(
        f64::from(r.left),
        f64::from(r.top),
        f64::from(r.right - r.left),
        f64::from(r.bottom - r.top),
    )
}

/// A process's executable name without `.exe`.
fn process_name(pid: u32) -> String {
    let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
    else {
        return format!("process {pid}");
    };
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    }
    .is_ok();
    let _ = unsafe { CloseHandle(process) };
    if !ok {
        return format!("process {pid}");
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    std::path::Path::new(&path)
        .file_stem()
        .map_or(path.clone(), |s| s.to_string_lossy().to_string())
}

fn elevated(process: HANDLE) -> Option<bool> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }.ok()?;
    let mut elevation = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&raw mut elevation).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    }
    .is_ok();
    let _ = unsafe { CloseHandle(token) };
    ok.then_some(elevation.TokenIsElevated != 0)
}

fn pid_of(h: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    pid
}

fn title_of(h: HWND) -> String {
    let len = unsafe { GetWindowTextLengthW(h) };
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let n = unsafe { GetWindowTextW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn frame_bounds(h: HWND) -> Option<Rect> {
    let mut r = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            h,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&raw mut r).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
    }
    .ok()?;
    Some(rect(r))
}

fn cloaked(h: HWND) -> bool {
    let mut value = 0u32;
    unsafe {
        DwmGetWindowAttribute(
            h,
            DWMWA_CLOAKED,
            (&raw mut value).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    }
    .is_ok()
        && value != 0
}

unsafe extern "system" fn collect_window(h: HWND, data: LPARAM) -> BOOL {
    let list = unsafe { &mut *(data.0 as *mut Vec<HWND>) };
    list.push(h);
    BOOL(1)
}

unsafe extern "system" fn collect_monitor(
    m: HMONITOR,
    _dc: HDC,
    _r: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let list = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
    list.push(m);
    BOOL(1)
}

fn top_windows() -> Vec<HWND> {
    let mut list: Vec<HWND> = Vec::new();
    let _ = unsafe { EnumWindows(Some(collect_window), LPARAM((&raw mut list) as isize)) };
    list
}

fn listed(h: HWND) -> Option<WindowInfo> {
    if !unsafe { IsWindowVisible(h) }.as_bool() || cloaked(h) {
        return None;
    }
    let ex = unsafe { GetWindowLongW(h, GWL_EXSTYLE) } as u32;
    if ex & WS_EX_TOOLWINDOW.0 != 0 {
        return None;
    }
    let minimized = unsafe { IsIconic(h) }.as_bool();
    let bounds = frame_bounds(h)?;
    let title = title_of(h);
    if title.is_empty() && !minimized && (bounds.width < 2.0 || bounds.height < 2.0) {
        return None;
    }
    let pid = pid_of(h);
    Some(WindowInfo {
        id: hwnd_id(h),
        pid,
        app: process_name(pid),
        title,
        bounds,
        minimized,
    })
}

// ─── Input ─────────────────────────────────────────────────────────────

fn key_input(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn mouse_input(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> Result<()> {
    if inputs.is_empty() {
        return Ok(());
    }
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        bail!(
            "Windows did not deliver the input (another program's window that runs as \
             administrator, or the secure desktop, is in front). Ask your owner."
        );
    }
    Ok(())
}

/// A point on the virtual desktop as `SendInput`'s absolute coordinates.
fn absolute(x: f64, y: f64) -> (i32, i32) {
    let vx = f64::from(unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) });
    let vy = f64::from(unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) });
    let vw = f64::from(unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) }.max(2));
    let vh = f64::from(unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) }.max(2));
    (
        (((x - vx) * 65535.0) / (vw - 1.0)).round() as i32,
        (((y - vy) * 65535.0) / (vh - 1.0)).round() as i32,
    )
}

fn move_to(x: f64, y: f64) -> INPUT {
    let (ax, ay) = absolute(x, y);
    mouse_input(
        ax,
        ay,
        0,
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
    )
}

/// The virtual key of `key`, and whether it is an extended key.
fn virtual_key(key: Key) -> Option<(VIRTUAL_KEY, bool, bool)> {
    let (vk, ext) = match key {
        Key::Enter => (VK_RETURN, false),
        Key::Tab => (VK_TAB, false),
        Key::Escape => (VK_ESCAPE, false),
        Key::Backspace => (VK_BACK, false),
        Key::Delete => (VK_DELETE, true),
        Key::Space => (VK_SPACE, false),
        Key::Up => (VK_UP, true),
        Key::Down => (VK_DOWN, true),
        Key::Left => (VK_LEFT, true),
        Key::Right => (VK_RIGHT, true),
        Key::Home => (VK_HOME, true),
        Key::End => (VK_END, true),
        Key::PageUp => (VK_PRIOR, true),
        Key::PageDown => (VK_NEXT, true),
        Key::Insert => (VK_INSERT, true),
        Key::F(n) => (VIRTUAL_KEY(VK_F1.0 + u16::from(n) - 1), false),
        Key::Char(c) => {
            if c.is_ascii_alphanumeric() {
                return Some((VIRTUAL_KEY(c.to_ascii_uppercase() as u16), false, false));
            }
            // Punctuation by the keyboard layout in use.
            let scan = unsafe { VkKeyScanW(c as u16) };
            if scan == -1 {
                return None;
            }
            let shift = (scan >> 8) & 1 != 0;
            return Some((VIRTUAL_KEY((scan & 0xFF) as u16), false, shift));
        }
    };
    Some((vk, ext, false))
}

fn chord_inputs(chord: &Chord) -> Result<Vec<INPUT>> {
    let (vk, ext, layout_shift) =
        virtual_key(chord.key).ok_or_else(|| anyhow!("no such key on this keyboard"))?;
    let mods: Vec<VIRTUAL_KEY> = [
        (chord.mods.cmd || chord.mods.ctrl, VK_CONTROL),
        (chord.mods.alt, VK_MENU),
        (chord.mods.shift || layout_shift, VK_SHIFT),
        (chord.mods.win, VK_LWIN),
    ]
    .into_iter()
    .filter_map(|(on, vk)| on.then_some(vk))
    .collect();
    let ext_flag = |e: bool| {
        if e {
            KEYEVENTF_EXTENDEDKEY
        } else {
            KEYBD_EVENT_FLAGS(0)
        }
    };
    let mut inputs = Vec::new();
    for m in &mods {
        inputs.push(key_input(*m, 0, ext_flag(*m == VK_LWIN)));
    }
    inputs.push(key_input(vk, 0, ext_flag(ext)));
    inputs.push(key_input(vk, 0, ext_flag(ext) | KEYEVENTF_KEYUP));
    for m in mods.iter().rev() {
        inputs.push(key_input(*m, 0, ext_flag(*m == VK_LWIN) | KEYEVENTF_KEYUP));
    }
    Ok(inputs)
}

fn text_inputs(text: &str) -> Vec<INPUT> {
    // Each UTF-16 unit down and up; a surrogate pair's two units go in
    // one call, so nothing comes between them.
    let mut inputs = Vec::new();
    for unit in text.encode_utf16() {
        inputs.push(key_input(VIRTUAL_KEY(0), unit, KEYEVENTF_UNICODE));
        inputs.push(key_input(
            VIRTUAL_KEY(0),
            unit,
            KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
        ));
    }
    inputs
}

/// Keep the process's multithreaded apartment alive for good.
///
/// Pictures are taken on the engine's blocking threads, which come and
/// go. When the last thread in the MTA exits, COM tears the apartment
/// down and may unload the WinRT libraries, but the `windows` crate keeps
/// the activation factories it got in a process-wide cache: the next
/// picture called through a dangling factory and the engine crashed
/// (access violation in `GraphicsCaptureSession::IsSupported`). Holding
/// one MTA usage makes the apartment outlive every thread.
fn keep_mta() -> Result<()> {
    static HELD: OnceLock<Result<(), String>> = OnceLock::new();
    HELD.get_or_init(|| {
        // The cookie is never handed back: the hold lasts as long as the
        // process.
        unsafe { CoIncrementMTAUsage() }
            .map(|_cookie| ())
            .map_err(|e| format!("COM refused to keep its apartment: {e}"))
    })
    .clone()
    .map_err(|e| anyhow!(e))
}

// ─── The platform ──────────────────────────────────────────────────────

pub struct WindowsPlatform;

impl WindowsPlatform {
    pub fn new() -> Self {
        Self
    }

    fn session_zero() -> bool {
        let mut session = 0u32;
        unsafe { ProcessIdToSessionId(std::process::id(), &mut session) }.is_ok() && session == 0
    }

    fn input_desktop_name() -> Option<String> {
        let desk =
            unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS) }
                .ok()?;
        let mut buf = [0u16; 128];
        let mut needed = 0u32;
        let ok = unsafe {
            GetUserObjectInformationW(
                HANDLE(desk.0),
                UOI_NAME,
                Some(buf.as_mut_ptr().cast()),
                (buf.len() * 2) as u32,
                Some(&mut needed),
            )
        }
        .is_ok();
        let _ = unsafe { CloseDesktop(desk) };
        ok.then(|| {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..end])
        })
    }

    fn screens_now() -> Vec<Screen> {
        dpi_aware(|| {
            let mut monitors: Vec<HMONITOR> = Vec::new();
            unsafe {
                let _ = EnumDisplayMonitors(
                    None,
                    None,
                    Some(collect_monitor),
                    LPARAM((&raw mut monitors) as isize),
                );
            }
            monitors
                .into_iter()
                .filter_map(|m| {
                    let mut info = MONITORINFO {
                        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                        ..MONITORINFO::default()
                    };
                    if !unsafe { GetMonitorInfoW(m, &mut info) }.as_bool() {
                        return None;
                    }
                    let (mut dx, mut dy) = (96u32, 96u32);
                    let _ = unsafe { GetDpiForMonitor(m, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) };
                    Some(Screen {
                        id: m.0 as usize as u64,
                        bounds: rect(info.rcMonitor),
                        scale: f64::from(dx) / 96.0,
                        main: info.dwFlags & 1 != 0,
                    })
                })
                .collect()
        })
    }

    /// The foreground window, by its top-level window.
    fn foreground() -> Option<HWND> {
        let fg = unsafe { GetForegroundWindow() };
        if fg.0.is_null() {
            return None;
        }
        let root = unsafe { GetAncestor(fg, GA_ROOT) };
        Some(if root.0.is_null() { fg } else { root })
    }

    fn capture_wgc(target: Target) -> Result<Image> {
        keep_mta()?;
        let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
        anyhow::ensure!(
            GraphicsCaptureSession::IsSupported().unwrap_or(false),
            "Windows.Graphics.Capture is not supported here"
        );
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = match target {
            Target::Window(id) => unsafe { interop.CreateForWindow(hwnd(id))? },
            Target::Screen(id) => unsafe {
                interop.CreateForMonitor(HMONITOR(id as usize as *mut core::ffi::c_void))?
            },
        };
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                windows::Win32::Foundation::HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let device = device.ok_or_else(|| anyhow!("no Direct3D device"))?;
        let context = context.ok_or_else(|| anyhow!("no Direct3D context"))?;
        let dxgi: IDXGIDevice = device.cast()?;
        let d3d: IDirect3DDevice =
            unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? }.cast()?;
        let size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &d3d,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            1,
            size,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        // Windows 11: no yellow border, no cursor in the picture.
        let _ = session.SetIsBorderRequired(false);
        let _ = session.SetIsCursorCaptureEnabled(false);
        let (tx, rx) = mpsc::channel::<()>();
        pool.FrameArrived(&windows::Foundation::TypedEventHandler::new(move |_, _| {
            let _ = tx.send(());
            Ok(())
        }))?;
        session.StartCapture()?;
        let got = rx.recv_timeout(Duration::from_secs(3));
        let frame = pool.TryGetNextFrame();
        let _ = session.Close();
        let _ = pool.Close();
        got.map_err(|_| anyhow!("Windows took no picture"))?;
        let frame = frame?;
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
        let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut staging: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging))? };
        let staging = staging.ok_or_else(|| anyhow!("no staging texture"))?;
        unsafe { context.CopyResource(&staging, &texture) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))? };
        let (w, h) = (desc.Width as usize, desc.Height as usize);
        let mut rgba = vec![0u8; w * h * 4];
        let pitch = mapped.RowPitch as usize;
        let src = mapped.pData.cast::<u8>();
        for row in 0..h {
            let line = unsafe { std::slice::from_raw_parts(src.add(row * pitch), w * 4) };
            let out = &mut rgba[row * w * 4..(row + 1) * w * 4];
            for (o, p) in out.chunks_exact_mut(4).zip(line.chunks_exact(4)) {
                o[0] = p[2];
                o[1] = p[1];
                o[2] = p[0];
                o[3] = 255;
            }
        }
        unsafe { context.Unmap(&staging, 0) };
        Ok(Image {
            width: w as u32,
            height: h as u32,
            rgba,
        })
    }

    /// `PrintWindow` for a window, `BitBlt` of the screen for a monitor.
    fn capture_gdi(target: Target) -> Result<Image> {
        let (bounds, window) = match target {
            Target::Window(id) => {
                let h = hwnd(id);
                let mut r = RECT::default();
                unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowRect(h, &mut r)? };
                (rect(r), Some((h, frame_bounds(h))))
            }
            Target::Screen(id) => {
                let s = Self::screens_now()
                    .into_iter()
                    .find(|s| s.id == id)
                    .ok_or_else(|| anyhow!("there is no screen {id}"))?;
                (s.bounds, None)
            }
        };
        let (w, h) = (bounds.width as i32, bounds.height as i32);
        anyhow::ensure!(w > 0 && h > 0, "nothing to take a picture of");
        let screen = unsafe { GetDC(None) };
        let dc = unsafe { CreateCompatibleDC(Some(screen)) };
        let bitmap = unsafe { CreateCompatibleBitmap(screen, w, h) };
        let old = unsafe { SelectObject(dc, bitmap.into()) };
        let drawn = match window {
            // PW_RENDERFULLCONTENT: windows drawn with DirectComposition too.
            Some((hw, _)) => unsafe { PrintWindow(hw, dc, PRINT_WINDOW_FLAGS(2)) }.as_bool(),
            None => unsafe {
                BitBlt(
                    dc,
                    0,
                    0,
                    w,
                    h,
                    Some(screen),
                    bounds.x as i32,
                    bounds.y as i32,
                    SRCCOPY | CAPTUREBLT,
                )
            }
            .is_ok(),
        };
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..BITMAPINFOHEADER::default()
            },
            ..BITMAPINFO::default()
        };
        let mut bgra = vec![0u8; (w * h * 4) as usize];
        let lines = unsafe {
            GetDIBits(
                dc,
                bitmap,
                0,
                h as u32,
                Some(bgra.as_mut_ptr().cast()),
                &mut info,
                DIB_RGB_COLORS,
            )
        };
        unsafe {
            SelectObject(dc, old);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);
        }
        anyhow::ensure!(drawn && lines > 0, "Windows gave no picture");
        let mut image = Image {
            width: w as u32,
            height: h as u32,
            rgba: bgra
                .chunks_exact(4)
                .flat_map(|p| [p[2], p[1], p[0], 255])
                .collect(),
        };
        // `PrintWindow` draws the whole window rectangle, invisible resize
        // borders included; keep the visible frame the bounds describe.
        if let Some((_, Some(frame))) = window {
            image = image.crop(
                (frame.x - bounds.x).max(0.0) as u32,
                (frame.y - bounds.y).max(0.0) as u32,
                frame.width as u32,
                frame.height as u32,
            );
        }
        Ok(image)
    }
}

impl Platform for WindowsPlatform {
    fn system(&self) -> &'static str {
        "windows"
    }

    fn unavailable(&self) -> Option<String> {
        Self::session_zero().then(|| {
            "Peko runs as a Windows service here, which has no desktop to see or use. Run Peko \
             from your own signed-in session (Peko Desktop, or `peko engine start` in a \
             terminal) to use the computer."
                .to_string()
        })
    }

    fn permissions(&self) -> Permissions {
        // Windows asks for nothing: any program in the session may see the
        // screen and send input.
        Permissions::default()
    }

    fn request_permissions(&self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    fn screens(&self) -> Result<Vec<Screen>> {
        let screens = Self::screens_now();
        anyhow::ensure!(!screens.is_empty(), "Windows reports no display");
        Ok(screens)
    }

    fn windows(&self) -> Result<Vec<WindowInfo>> {
        Ok(dpi_aware(|| {
            let mut out: Vec<WindowInfo> = Vec::new();
            let mut minimized = Vec::new();
            for h in top_windows() {
                if let Some(w) = listed(h) {
                    if w.minimized {
                        minimized.push(w);
                    } else {
                        out.push(w);
                    }
                }
            }
            out.extend(minimized);
            out
        }))
    }

    fn front(&self) -> Result<Front> {
        let fg = Self::foreground().ok_or_else(|| anyhow!("no window is in front"))?;
        let pid = pid_of(fg);
        Ok(Front {
            pid,
            app: process_name(pid),
            window: Some(hwnd_id(fg)),
        })
    }

    fn raise(&self, window: &WindowInfo) -> Result<()> {
        let h = hwnd(window.id);
        dpi_aware(|| unsafe {
            if IsIconic(h).as_bool() {
                let _ = ShowWindow(h, SW_RESTORE);
            }
            // Windows gives the foreground to the process that sent the
            // last input: a tap of Alt makes that this one.
            let _ = send(&[
                key_input(VK_MENU, 0, KEYBD_EVENT_FLAGS(0)),
                key_input(VK_MENU, 0, KEYEVENTF_KEYUP),
            ]);
            let _ = SetForegroundWindow(h);
            let _ = BringWindowToTop(h);
        });
        Ok(())
    }

    fn owner_at(&self, x: f64, y: f64) -> Option<(u32, String)> {
        dpi_aware(|| {
            let h = unsafe {
                WindowFromPoint(POINT {
                    x: x.round() as i32,
                    y: y.round() as i32,
                })
            };
            if h.0.is_null() {
                return None;
            }
            let root = unsafe { GetAncestor(h, GA_ROOT) };
            let pid = pid_of(if root.0.is_null() { h } else { root });
            (pid != 0).then(|| (pid, process_name(pid)))
        })
    }

    fn obstruction(&self) -> Option<String> {
        match Self::input_desktop_name() {
            None => {
                return Some(
                    "The screen is locked (or the sign-in screen is up); nothing can be seen or \
                     done until your owner unlocks it."
                        .into(),
                )
            }
            Some(name) if !name.eq_ignore_ascii_case("Default") => {
                return Some(format!(
                    "Windows shows its secure desktop ({name}): a sign-in or an administrator \
                     prompt is for your owner. Nothing was done."
                ))
            }
            Some(_) => {}
        }
        let mut running = BOOL(0);
        let saver = unsafe {
            SystemParametersInfoW(
                SPI_GETSCREENSAVERRUNNING,
                0,
                Some((&raw mut running).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        }
        .is_ok()
            && running.as_bool();
        saver.then(|| "The screen saver is on.".to_string())
    }

    fn secure_input(&self) -> bool {
        // A UAC prompt (consent.exe) or another desktop holds the keyboard.
        if Self::input_desktop_name().is_none_or(|n| !n.eq_ignore_ascii_case("Default")) {
            return true;
        }
        Self::foreground()
            .is_some_and(|fg| process_name(pid_of(fg)).eq_ignore_ascii_case("consent"))
    }

    fn unreachable(&self, window: &WindowInfo) -> Option<String> {
        let mine = elevated(unsafe { GetCurrentProcess() }).unwrap_or(false);
        if mine {
            return None;
        }
        let theirs =
            match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, window.pid) } {
                Ok(process) => {
                    let e = elevated(process);
                    let _ = unsafe { CloseHandle(process) };
                    e
                }
                Err(_) => None,
            };
        (theirs == Some(true)).then(|| {
            format!(
                "{} runs as administrator and Peko does not, so Windows drops input sent to it. \
                 Nothing was sent; ask your owner to do this step.",
                window.app
            )
        })
    }

    fn capture(&self, target: Target) -> Result<Image> {
        dpi_aware(|| match Self::capture_wgc(target) {
            Ok(image) => Ok(image),
            Err(e) => {
                tracing::debug!("computer: Windows.Graphics.Capture failed ({e:#}); using GDI");
                Self::capture_gdi(target)
            }
        })
    }

    fn post(&self, event: &InputEvent) -> Result<()> {
        dpi_aware(|| match event {
            InputEvent::Move { x, y, .. } => send(&[move_to(*x, *y)]),
            InputEvent::Press {
                x, y, button, down, ..
            } => {
                let flag = match (button, down) {
                    (Button::Left, true) => MOUSEEVENTF_LEFTDOWN,
                    (Button::Left, false) => MOUSEEVENTF_LEFTUP,
                    (Button::Right, true) => MOUSEEVENTF_RIGHTDOWN,
                    (Button::Right, false) => MOUSEEVENTF_RIGHTUP,
                    (Button::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
                    (Button::Middle, false) => MOUSEEVENTF_MIDDLEUP,
                };
                let (ax, ay) = absolute(*x, *y);
                send(&[mouse_input(
                    ax,
                    ay,
                    0,
                    flag | MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                )])
            }
            InputEvent::Scroll { x, y, dx, dy } => {
                let mut inputs = vec![move_to(*x, *y)];
                if *dy != 0 {
                    // Windows counts forward (up) as positive.
                    inputs.push(mouse_input(0, 0, -dy * WHEEL_DELTA, MOUSEEVENTF_WHEEL));
                }
                if *dx != 0 {
                    inputs.push(mouse_input(0, 0, dx * WHEEL_DELTA, MOUSEEVENTF_HWHEEL));
                }
                send(&inputs)
            }
            InputEvent::Chord(chord) => send(&chord_inputs(chord)?),
            InputEvent::Text(text) => send(&text_inputs(text)),
        })
    }

    fn watch_input(&self, owner_input: Arc<dyn Fn() + Send + Sync>) -> Result<()> {
        let _ = OWNER_INPUT.set(owner_input);
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        std::thread::Builder::new()
            .name("peko-computer-watch".into())
            .spawn(move || hook_thread(&tx))?;
        rx.recv_timeout(Duration::from_secs(3))
            .map_err(|_| anyhow!("the input watch did not start"))?
            .map_err(|e| anyhow!(e))
    }

    fn hold_display_awake(&self) -> Option<Box<dyn Send>> {
        // The state belongs to a thread: one lives as long as the hold.
        let (release, wait) = mpsc::channel::<()>();
        std::thread::Builder::new()
            .name("peko-computer-awake".into())
            .spawn(move || {
                unsafe {
                    SetThreadExecutionState(
                        ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED,
                    )
                };
                let _ = wait.recv();
                unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
            })
            .ok()?;
        Some(Box::new(AwakeHold(release)))
    }
}

struct AwakeHold(mpsc::Sender<()>);

impl Drop for AwakeHold {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

// ─── Watching for the owner's hand ─────────────────────────────────────

static OWNER_INPUT: OnceLock<Arc<dyn Fn() + Send + Sync>> = OnceLock::new();
static NOTIFY: OnceLock<mpsc::SyncSender<()>> = OnceLock::new();

/// A hook must return quickly: it only signals, and another thread tells
/// the desk.
fn signal() {
    if let Some(tx) = NOTIFY.get() {
        let _ = tx.try_send(());
    }
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        if info.flags & LLMHF_INJECTED == 0 {
            signal();
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if info.flags.0 & LLKHF_INJECTED.0 == 0 {
            signal();
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn hook_thread(started: &mpsc::Sender<Result<(), String>>) {
    let (tx, rx) = mpsc::sync_channel::<()>(1);
    let _ = NOTIFY.set(tx);
    std::thread::Builder::new()
        .name("peko-computer-owner".into())
        .spawn(move || {
            while rx.recv().is_ok() {
                if let Some(cb) = OWNER_INPUT.get() {
                    cb();
                }
            }
        })
        .ok();
    let module = unsafe { GetModuleHandleW(None) }.ok();
    let hinstance = module.map(|m| windows::Win32::Foundation::HINSTANCE(m.0));
    let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), hinstance, 0) };
    let keyboard = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), hinstance, 0) };
    if mouse.is_err() && keyboard.is_err() {
        let _ = started.send(Err("Windows refused to watch for input".into()));
        return;
    }
    let _ = started.send(Ok(()));
    // Low-level hooks are called through this thread's message loop.
    let mut msg = MSG::default();
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_map_to_windows_codes_not_mac_ones() {
        let chord = super::super::keys::parse("cmd+a", false).unwrap();
        let inputs = chord_inputs(&chord).unwrap();
        // Ctrl down, A down, A up, Ctrl up.
        assert_eq!(inputs.len(), 4);
        let vk = |i: &INPUT| unsafe { i.Anonymous.ki.wVk };
        assert_eq!(vk(&inputs[0]), VK_CONTROL);
        assert_eq!(vk(&inputs[1]), VIRTUAL_KEY(u16::from(b'A')));
        assert_eq!(virtual_key(Key::F(5)).unwrap().0, VIRTUAL_KEY(VK_F1.0 + 4));
        assert!(
            virtual_key(Key::Left).unwrap().1,
            "arrows are extended keys"
        );
    }

    #[test]
    fn text_keeps_surrogate_pairs_together() {
        let inputs = text_inputs("a😀");
        // a down/up, then each half of the pair down/up.
        assert_eq!(inputs.len(), 6);
        let scan = |i: &INPUT| unsafe { i.Anonymous.ki.wScan };
        assert_eq!(scan(&inputs[2]), 0xD83D);
        assert_eq!(scan(&inputs[4]), 0xDE00);
    }

    /// Real-screen checks, by hand with `PEKO_TEST_COMPUTER=1` in a
    /// signed-in session.
    #[test]
    fn screens_windows_and_a_picture() {
        if std::env::var("PEKO_TEST_COMPUTER").ok().as_deref() != Some("1") {
            return;
        }
        let win = WindowsPlatform::new();
        assert!(win.unavailable().is_none());
        let screens = win.screens().unwrap();
        assert!(screens.iter().any(|s| s.main));
        let main = screens.iter().find(|s| s.main).unwrap();
        let image = win.capture(Target::Screen(main.id)).unwrap();
        assert!(image.width > 100);
        let windows = win.windows().unwrap();
        if let Some(w) = windows.iter().find(|w| !w.minimized) {
            let image = win.capture(Target::Window(w.id)).unwrap();
            assert!(image.width > 10);
        }
        assert!(win.front().is_ok());
    }

    /// The engine takes pictures on blocking threads that exit when idle.
    /// A picture on a thread that has exited, then one on a new thread,
    /// crashed the process (access violation) until the MTA was held.
    /// Real screen, by hand with `PEKO_TEST_COMPUTER=1`.
    #[test]
    fn pictures_from_threads_that_come_and_go() {
        if std::env::var("PEKO_TEST_COMPUTER").ok().as_deref() != Some("1") {
            return;
        }
        let picture = || {
            std::thread::spawn(|| {
                let win = WindowsPlatform::new();
                let main = win.screens().unwrap().into_iter().find(|s| s.main).unwrap();
                // Windows.Graphics.Capture itself, not the GDI fallback.
                WindowsPlatform::capture_wgc(Target::Screen(main.id))
                    .unwrap()
                    .width
            })
            .join()
            .expect("the picture's thread")
        };
        for _ in 0..3 {
            assert!(picture() > 100);
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}
