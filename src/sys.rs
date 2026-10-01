//! Thin wrappers over the handful of Win32 calls this app needs: a layered
//! popup window we can paint into directly, polled input, monitor geometry and
//! the registry plumbing for wallpaper/accent/autostart.

use std::ffi::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    GetLastError, COLORREF, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, GENERIC_WRITE, HINSTANCE, HWND,
    LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW,
    MonitorFromPoint, ReleaseDC, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO,
    BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, RGBQUAD,
};
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Console::{
    AttachConsole, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_BINARY, REG_DWORD,
    REG_OPEN_CREATE_OPTIONS, REG_SZ, REG_VALUE_TYPE,
};
use windows::Win32::System::Threading::{
    CreateMutexW, GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, FindWindowW, GetAncestor,
    GetClassNameW, GetCursorPos, GetForegroundWindow, LoadCursorW, PeekMessageW, PostMessageW,
    RegisterClassW, SendMessageTimeoutW, SetWindowPos, SetWindowsHookExW, ShowWindow,
    SystemParametersInfoW, TranslateMessage, UnhookWindowsHookEx, UpdateLayeredWindow, GA_ROOT,
    HC_ACTION, HHOOK, HWND_BROADCAST, HWND_TOPMOST, IDC_ARROW, KBDLLHOOKSTRUCT, MSG, PM_REMOVE,
    SMTO_ABORTIFHUNG, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE, SPI_SETDESKWALLPAPER, SWP_NOACTIVATE,
    SWP_SHOWWINDOW, SW_HIDE, ULW_ALPHA, WH_KEYBOARD_LL, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_DISPLAYCHANGE, WM_DWMCOLORIZATIONCOLORCHANGED, WM_KEYDOWN, WM_KEYUP, WM_QUIT,
    WM_SETTINGCHANGE, WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::tray;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_CONTROL,
    MOD_NOREPEAT, MOD_SHIFT, VK_CONTROL, VK_DOWN, VK_ESCAPE, VK_LBUTTON, VK_LEFT, VK_Q, VK_RBUTTON,
    VK_RETURN, VK_RIGHT, VK_SHIFT, VK_UP,
};

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_bytes(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect()
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScreenRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

pub fn enable_dpi_awareness() {
    // Physical pixels everywhere, or the strip would be blurry on scaled displays.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

// ---------------------------------------------------------------------------
// startup: console, single instance
// ---------------------------------------------------------------------------

/// Release builds are linked as a GUI app (`windows_subsystem`), which is what
/// keeps a console window from flashing up at logon. That also means a build has
/// no console to print to, so the command-line modes borrow the one belonging to
/// whatever launched them — a terminal's, in practice. Text goes to that
/// terminal; started from Explorer there is nothing to borrow and nothing is
/// printed.
pub fn attach_parent_console() {
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            return;
        }
        // The Rust runtime reads the standard handles the first time they are
        // used, so rebinding them here — before the first `println!` — is enough.
        let name = wide("CONOUT$");
        if let Ok(h) = CreateFileW(
            PCWSTR(name.as_ptr()),
            GENERIC_WRITE.0,
            FILE_SHARE_WRITE | FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        ) {
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, h);
            let _ = SetStdHandle(STD_ERROR_HANDLE, h);
        }
    }
}

/// A named mutex is the cheapest way to keep a tray app single-instance: the
/// kernel drops it when the process ends, however it ends.
pub fn claim_single_instance() -> bool {
    unsafe {
        let name = wide("Local\\carasoul-single-instance");
        match CreateMutexW(None, true, PCWSTR(name.as_ptr())) {
            // The handle is deliberately leaked: it has to outlive this call.
            Ok(_) => GetLastError() != ERROR_ALREADY_EXISTS,
            Err(_) => true, // no mutex, no guard: better to run than to refuse
        }
    }
}

/// Asks the instance that is already running to open its shelf — so launching the
/// app again from the Start menu does something sensible instead of nothing.
pub fn wake_running_instance() -> bool {
    unsafe {
        let class = wide(WINDOW_CLASS);
        // The other instance may still be setting its window up.
        for _ in 0..10 {
            let hwnd = FindWindowW(PCWSTR(class.as_ptr()), PCWSTR::null());
            if let Ok(hwnd) = hwnd {
                if !hwnd.is_invalid() {
                    let _ = PostMessageW(Some(hwnd), tray::WM_OPEN_SHELF, WPARAM(0), LPARAM(0));
                    return true;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        false
    }
}

pub fn cursor_pos() -> (i32, i32) {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    (p.x, p.y)
}

pub fn monitor_rect_at(pt: (i32, i32)) -> ScreenRect {
    unsafe {
        let mon = MonitorFromPoint(POINT { x: pt.0, y: pt.1 }, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            rcMonitor: RECT::default(),
            rcWork: RECT::default(),
            dwFlags: 0,
        };
        if GetMonitorInfoW(mon, &mut mi).as_bool() {
            let r = mi.rcMonitor;
            ScreenRect {
                x: r.left,
                y: r.top,
                w: r.right - r.left,
                h: r.bottom - r.top,
            }
        } else {
            ScreenRect {
                x: 0,
                y: 0,
                w: 1920,
                h: 1080,
            }
        }
    }
}

/// True when the shell's desktop (the homescreen) currently has focus. The strip
/// is deliberately desktop-only: Shift is a modifier people hold in every app.
pub fn desktop_has_focus() -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() {
            return false;
        }
        if is_desktop_window(fg) {
            return true;
        }
        // Clicking the desktop's file view can focus a child window instead.
        let root = GetAncestor(fg, GA_ROOT);
        !root.0.is_null() && root != fg && is_desktop_window(root)
    }
}

fn is_desktop_window(hwnd: HWND) -> bool {
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    if n <= 0 {
        return false;
    }
    let class = String::from_utf16_lossy(&buf[..n as usize]);
    class.eq_ignore_ascii_case("Progman")
        || class.eq_ignore_ascii_case("WorkerW")
        || class.eq_ignore_ascii_case("SysListView32")
}

/// Keys the strip consumes while it is open. Two mechanisms are used together:
/// a low-level keyboard hook, which swallows them before any window sees them,
/// and hotkey registrations as a fallback if the hook cannot be installed.
const GRABBED: [(i32, u32, u32); 6] = [
    (9001, 0, VK_LEFT.0 as u32),
    (9002, 0, VK_RIGHT.0 as u32),
    (9003, 0, VK_UP.0 as u32),
    (9004, 0, VK_DOWN.0 as u32),
    (9005, 0, VK_RETURN.0 as u32),
    (9006, 0, VK_ESCAPE.0 as u32),
];

const QUIT_HOTKEY_ID: i32 = 9007;

// Which navigation key was pressed, as reported by the hook below.
const BIT_LEFT: u32 = 1 << 0;
const BIT_RIGHT: u32 = 1 << 1;
const BIT_UP: u32 = 1 << 2;
const BIT_DOWN: u32 = 1 << 3;
const BIT_ENTER: u32 = 1 << 4;
const BIT_ESC: u32 = 1 << 5;

static PENDING: AtomicU32 = AtomicU32::new(0);

fn bit_for(vk: u32) -> u32 {
    const TABLE: [(u32, u32); 6] = [
        (VK_LEFT.0 as u32, BIT_LEFT),
        (VK_RIGHT.0 as u32, BIT_RIGHT),
        (VK_UP.0 as u32, BIT_UP),
        (VK_DOWN.0 as u32, BIT_DOWN),
        (VK_RETURN.0 as u32, BIT_ENTER),
        (VK_ESCAPE.0 as u32, BIT_ESC),
    ];
    TABLE
        .iter()
        .find(|(v, _)| *v == vk)
        .map(|(_, b)| *b)
        .unwrap_or(0)
}

/// Used by `--demo` to drive the navigation path as if the keys were pressed.
pub fn inject_pending(bits: u32) {
    PENDING.fetch_or(bits, Ordering::Relaxed);
}

/// Navigation bit for "next wallpaper", for `--demo`.
pub const INJECT_NEXT: u32 = BIT_RIGHT;

static SWALLOWING: AtomicBool = AtomicBool::new(false);
static HOOK: AtomicIsize = AtomicIsize::new(0);

fn is_grabbed(vk: u32) -> bool {
    vk == VK_Q.0 as u32 || GRABBED.iter().any(|(_, _, v)| *v == vk)
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code as u32 == HC_ACTION && SWALLOWING.load(Ordering::Relaxed) {
        let msg = wparam.0 as u32;
        if msg == WM_KEYDOWN || msg == WM_KEYUP || msg == WM_SYSKEYDOWN || msg == WM_SYSKEYUP {
            let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
            if is_grabbed(kb.vkCode) {
                if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
                    let bit = bit_for(kb.vkCode);
                    if bit != 0 {
                        PENDING.fetch_or(bit, Ordering::Relaxed);
                    }
                }
                return LRESULT(1); // consumed: no window ever sees it
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// Claims the navigation keys while the strip is open. Returns the number of
/// hotkeys that registered successfully (the hook is the primary mechanism; the
/// count is reported by `--demo` so a failure is visible rather than silent).
pub fn grab_keys(grab: bool) -> u32 {
    SWALLOWING.store(grab, Ordering::Relaxed);
    unsafe {
        if grab {
            if HOOK.load(Ordering::Relaxed) == 0 {
                if let Ok(h) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0) {
                    HOOK.store(h.0 as isize, Ordering::Relaxed);
                }
            }
        } else if HOOK.load(Ordering::Relaxed) != 0 {
            let _ = UnhookWindowsHookEx(HHOOK(HOOK.load(Ordering::Relaxed) as *mut c_void));
            HOOK.store(0, Ordering::Relaxed);
        }

        let mut ok = 0u32;
        for (id, _, vk) in GRABBED {
            if grab {
                if RegisterHotKey(None, id, MOD_NOREPEAT, vk).is_ok() {
                    ok += 1;
                }
            } else {
                let _ = UnregisterHotKey(None, id);
            }
        }
        if grab {
            let mods: HOT_KEY_MODIFIERS = MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT;
            if RegisterHotKey(None, QUIT_HOTKEY_ID, mods, VK_Q.0 as u32).is_ok() {
                ok += 1;
            }
        } else {
            let _ = UnregisterHotKey(None, QUIT_HOTKEY_ID);
        }
        ok
    }
}

/// True when the low-level keyboard hook is installed.
pub fn hook_installed() -> bool {
    HOOK.load(Ordering::Relaxed) != 0
}

/// The thumbnail decode runs alongside the animation; keeping it below normal
/// priority stops it from stealing frames while the shelf is moving.
pub fn lower_current_thread_priority() {
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

// ---------------------------------------------------------------------------
// polled input
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default, Debug)]
pub struct Edges {
    pub shift_held: bool,
    pub shift_down: bool,
    pub shift_up: bool,
    pub ctrl_held: bool,
    pub left: bool,
    pub right: bool,
    pub up: bool,
    pub down: bool,
    pub enter: bool,
    pub esc: bool,
    pub click: bool,
    pub right_click: bool,
    pub q: bool,
}

impl Edges {
    /// Any key that means "the user is doing something else, not asking for the
    /// strip" — used to cancel a pending Shift-hold before it fires.
    pub fn other_key(&self) -> bool {
        self.left || self.right || self.up || self.down || self.enter || self.esc || self.q
    }
}

const NKEYS: usize = 11;

pub struct Input {
    prev: [bool; NKEYS],
}

impl Input {
    pub fn new() -> Self {
        Self {
            prev: [false; NKEYS],
        }
    }

    pub fn poll(&mut self) -> Edges {
        const VKS: [i32; NKEYS] = [
            VK_SHIFT.0 as i32,
            VK_CONTROL.0 as i32,
            VK_LEFT.0 as i32,
            VK_RIGHT.0 as i32,
            VK_UP.0 as i32,
            VK_DOWN.0 as i32,
            VK_RETURN.0 as i32,
            VK_ESCAPE.0 as i32,
            VK_LBUTTON.0 as i32,
            VK_Q.0 as i32,
            VK_RBUTTON.0 as i32,
        ];
        let mut now = [false; NKEYS];
        for (i, vk) in VKS.iter().enumerate() {
            now[i] = key_down(*vk);
        }
        // Keys we swallowed in the low-level hook never reach GetAsyncKeyState, so
        // the hook hands them over here instead. `--demo` injects the same bits to
        // exercise this path without a keyboard.
        let pend = PENDING.swap(0, Ordering::Relaxed);
        let e = Edges {
            shift_held: now[0],
            shift_down: now[0] && !self.prev[0],
            shift_up: !now[0] && self.prev[0],
            ctrl_held: now[1],
            left: (now[2] && !self.prev[2]) || pend & BIT_LEFT != 0,
            right: (now[3] && !self.prev[3]) || pend & BIT_RIGHT != 0,
            up: (now[4] && !self.prev[4]) || pend & BIT_UP != 0,
            down: (now[5] && !self.prev[5]) || pend & BIT_DOWN != 0,
            enter: (now[6] && !self.prev[6]) || pend & BIT_ENTER != 0,
            esc: (now[7] && !self.prev[7]) || pend & BIT_ESC != 0,
            click: now[8] && !self.prev[8],
            right_click: now[10] && !self.prev[10],
            q: now[9] && !self.prev[9],
        };
        self.prev = now;
        e
    }
}

fn key_down(vk: i32) -> bool {
    unsafe { (GetAsyncKeyState(vk) as u16 & 0x8000) != 0 }
}

// ---------------------------------------------------------------------------
// overlay window + layered surface
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct Pump {
    pub quit: bool,
    pub display_changed: bool,
}

pub struct Overlay {
    pub hwnd: HWND,
    pub w: i32,
    pub h: i32,
    memdc: HDC,
    dib: HBITMAP,
    prev_obj: HGDIOBJ,
    data: *mut u32,
}

/// The window class name, so a second launch can find the instance already
/// running and hand it the request that brought the user here.
pub const WINDOW_CLASS: &str = "CarasoulOverlay";

/// The tray icon's callback messages, the tray's menu commands and every other
/// message that lands on our window come through here first; anything the tray
/// does not claim is the window's own business (which is nothing, so far).
extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if crate::tray::handle_message(hwnd, msg, w, l) {
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

impl Overlay {
    pub fn create(w: i32, h: i32) -> Result<Self, String> {
        let class = wide("CarasoulOverlay");
        let hinst = unsafe { GetModuleHandleW(None) }.map_err(|e| e.to_string())?;
        let hinstance = HINSTANCE(hinst.0);

        // Both sizes of the window's icon come from the same master the tray uses,
        // so the app has one identity rather than a tray icon and a default one.
        let icon = crate::tray::window_icon();
        let wc = WNDCLASSW {
            style: Default::default(),
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: icon,
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
            hbrBackground: Default::default(),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR(class.as_ptr()),
        };
        unsafe {
            RegisterClassW(&wc);
        }

        let ex_style: WINDOW_EX_STYLE =
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
        let style: WINDOW_STYLE = WS_POPUP;
        let hwnd = unsafe {
            CreateWindowExW(
                ex_style,
                PCWSTR(class.as_ptr()),
                PCWSTR(class.as_ptr()),
                style,
                0,
                0,
                w,
                h,
                None,
                None,
                Some(hinstance),
                None,
            )
        }
        .map_err(|e| e.to_string())?;

        let mut o = Overlay {
            hwnd,
            w,
            h,
            memdc: HDC::default(),
            dib: HBITMAP::default(),
            prev_obj: HGDIOBJ::default(),
            data: std::ptr::null_mut(),
        };
        // The backing bitmap is deliberately not allocated here: at boot the app
        // holds no pixel buffer at all, and grabs it on the first frame it draws.
        let _ = &mut o;
        Ok(o)
    }

    /// Allocates the drawing surface on first use. Returns false if it fails.
    pub fn ensure_surface(&mut self) -> bool {
        if !self.data.is_null() {
            return true;
        }
        self.build_surface(self.w, self.h).is_ok()
    }

    fn build_surface(&mut self, w: i32, h: i32) -> Result<(), String> {
        unsafe {
            let screen = GetDC(None);
            let memdc = CreateCompatibleDC(Some(screen));
            ReleaseDC(None, screen);

            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: 0, // BI_RGB
                    biSizeImage: (w * h * 4) as u32,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                },
                bmiColors: [RGBQUAD {
                    rgbBlue: 0,
                    rgbGreen: 0,
                    rgbRed: 0,
                    rgbReserved: 0,
                }],
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            let dib = CreateDIBSection(Some(memdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
                .map_err(|e| e.to_string())?;
            let prev_obj = SelectObject(memdc, HGDIOBJ(dib.0));

            self.memdc = memdc;
            self.dib = dib;
            self.prev_obj = prev_obj;
            self.data = bits as *mut u32;
            self.w = w;
            self.h = h;
        }
        Ok(())
    }

    /// Rebuilds the surface after a resolution/DPI change.
    pub fn resize(&mut self, w: i32, h: i32) -> Result<(), String> {
        unsafe {
            let (memdc, dib, prev) = (self.memdc, self.dib, self.prev_obj);
            if !prev.is_invalid() {
                SelectObject(memdc, prev);
            }
            let _ = DeleteObject(HGDIOBJ(dib.0));
            let _ = DeleteDC(memdc);
        }
        self.memdc = HDC::default();
        self.dib = HBITMAP::default();
        self.data = std::ptr::null_mut();
        self.w = 0;
        self.h = 0;
        self.build_surface(w, h)
    }

    pub fn pixels(&mut self) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.data, (self.w * self.h) as usize) }
    }

    fn blend_function() -> BLENDFUNCTION {
        BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        }
    }

    pub fn present(&self, x: i32, y: i32) {
        if self.data.is_null() {
            return;
        }
        let dst = POINT { x, y };
        let size = SIZE {
            cx: self.w,
            cy: self.h,
        };
        let src = POINT { x: 0, y: 0 };
        let blend = Self::blend_function();
        unsafe {
            let _ = UpdateLayeredWindow(
                self.hwnd,
                None,
                Some(&dst),
                Some(&size),
                Some(self.memdc),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
        }
    }

    /// Shows the window without stealing focus. Content is expected to have been
    /// presented already, so nothing stale flashes on screen.
    pub fn show_at(&self, x: i32, y: i32) {
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                x,
                y,
                self.w,
                self.h,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            );
        }
    }

    pub fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    /// Drains the message queue. Our window proc ignores everything, but the
    /// queue still has to be emptied so the system does not pile messages up.
    pub fn pump(&self) -> Pump {
        let mut out = Pump::default();
        let mut msg = MSG::default();
        loop {
            let got = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) };
            if got.0 == 0 {
                break;
            }
            if msg.message == WM_QUIT {
                out.quit = true;
                continue;
            }
            if msg.message == WM_DISPLAYCHANGE {
                out.display_changed = true;
            }
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// timing
// ---------------------------------------------------------------------------

/// While the strip is animating we want a fast tick, so ask Windows for a 1 ms
/// timer while it is open and hand it back on close.
pub fn set_timer_resolution(high: bool) {
    unsafe {
        if high {
            timeBeginPeriod(1);
        } else {
            timeEndPeriod(1);
        }
    }
}

// ---------------------------------------------------------------------------
// registry
// ---------------------------------------------------------------------------

const REG_OPTION_NON_VOLATILE: REG_OPEN_CREATE_OPTIONS = REG_OPEN_CREATE_OPTIONS(0);

fn reg_set(subkey: &str, name: &str, ty: REG_VALUE_TYPE, data: &[u8]) -> bool {
    unsafe {
        let mut h = HKEY::default();
        let sk = wide(subkey);
        let rc = RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sk.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut h,
            None,
        );
        if rc != ERROR_SUCCESS {
            return false;
        }
        let n = wide(name);
        let rc = RegSetValueExW(h, PCWSTR(n.as_ptr()), None, ty, Some(data));
        let _ = RegCloseKey(h);
        rc == ERROR_SUCCESS
    }
}

fn reg_set_dword(subkey: &str, name: &str, v: u32) -> bool {
    reg_set(subkey, name, REG_DWORD, &v.to_le_bytes())
}

fn reg_set_str(subkey: &str, name: &str, v: &str) -> bool {
    reg_set(subkey, name, REG_SZ, &wide_bytes(v))
}

fn reg_delete(subkey: &str, name: &str) -> bool {
    unsafe {
        let mut h = HKEY::default();
        let sk = wide(subkey);
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sk.as_ptr()),
            None,
            KEY_SET_VALUE,
            &mut h,
        ) != ERROR_SUCCESS
        {
            return false;
        }
        let n = wide(name);
        let rc = RegDeleteValueW(h, PCWSTR(n.as_ptr()));
        let _ = RegCloseKey(h);
        rc == ERROR_SUCCESS
    }
}

/// Reads a string value; `None` when the key or value is not there.
fn reg_get_str(subkey: &str, name: &str) -> Option<String> {
    let (ty, data) = reg_get(subkey, name)?;
    if ty != REG_SZ {
        return None;
    }
    let u16s: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Some(
        String::from_utf16_lossy(&u16s)
            .trim_end_matches('\0')
            .to_string(),
    )
}

/// Reads a DWORD value; `None` when the key or value is not there.
fn reg_get_dword(subkey: &str, name: &str) -> Option<u32> {
    let (ty, data) = reg_get(subkey, name)?;
    if ty != REG_DWORD || data.len() < 4 {
        return None;
    }
    Some(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
}

fn reg_get(subkey: &str, name: &str) -> Option<(REG_VALUE_TYPE, Vec<u8>)> {
    unsafe {
        let mut h = HKEY::default();
        let sk = wide(subkey);
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sk.as_ptr()),
            None,
            KEY_READ,
            &mut h,
        ) != ERROR_SUCCESS
        {
            return None;
        }
        let n = wide(name);
        let mut ty = REG_VALUE_TYPE::default();
        let mut buf = [0u8; 1024];
        let mut cb = buf.len() as u32;
        let rc = RegQueryValueExW(
            h,
            PCWSTR(n.as_ptr()),
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr()),
            Some(&mut cb),
        );
        let _ = RegCloseKey(h);
        if rc != ERROR_SUCCESS {
            return None;
        }
        Some((ty, buf[..(cb as usize).min(buf.len())].to_vec()))
    }
}

/// Path of the wallpaper Windows currently has set, so we can start focused on it.
pub fn current_wallpaper() -> Option<String> {
    unsafe {
        let sk = wide(r"Control Panel\Desktop");
        let mut h = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sk.as_ptr()),
            None,
            KEY_READ,
            &mut h,
        ) != ERROR_SUCCESS
        {
            return None;
        }
        let name = wide("WallPaper");
        let mut ty = REG_VALUE_TYPE::default();
        let mut buf = [0u8; 2048];
        let mut cb = buf.len() as u32;
        let rc = RegQueryValueExW(
            h,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr()),
            Some(&mut cb),
        );
        let _ = RegCloseKey(h);
        if rc != ERROR_SUCCESS || ty != REG_SZ {
            return None;
        }
        let u16s: Vec<u16> = buf[..cb as usize]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let s: String = String::from_utf16_lossy(&u16s)
            .trim_end_matches('\0')
            .to_string();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

/// Applies the wallpaper, stretched to fill, and tells the shell.
pub fn set_wallpaper(path: &Path) -> bool {
    let full = match path.canonicalize() {
        Ok(p) => p,
        Err(_) => path.to_path_buf(),
    };
    let text = full.to_string_lossy().to_string();
    // \\.\ style prefixes confuse SPI; a plain absolute path is what it wants.
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text).to_string();

    reg_set_str(r"Control Panel\Desktop", "WallpaperStyle", "10"); // 10 = fill
    reg_set_str(r"Control Panel\Desktop", "TileWallpaper", "0");

    let mut w = wide(&text);
    unsafe {
        SystemParametersInfoW(
            SPI_SETDESKWALLPAPER,
            0,
            Some(w.as_mut_ptr() as *mut c_void),
            SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
        )
        .is_ok()
    }
}

/// Best-effort "PC theme follows the wallpaper": writes the undocumented DWM
/// accent values and nudges every window to pick them up.
pub fn apply_accent(rgb: (u8, u8, u8)) {
    let (r, g, b) = (rgb.0 as u32, rgb.1 as u32, rgb.2 as u32);
    let abgr = 0xFF00_0000u32 | (b << 16) | (g << 8) | r;

    reg_set_dword(r"Software\Microsoft\Windows\DWM", "AccentColor", abgr);
    reg_set_dword(r"Software\Microsoft\Windows\DWM", "ColorPrevalence", 1);
    let explorer = r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent";
    reg_set_dword(explorer, "AccentColorMenu", abgr);
    reg_set_dword(explorer, "StartColorMenu", abgr);

    // Windows keeps an 8-entry light->dark palette next to the accent.
    let mut palette = Vec::with_capacity(32);
    for i in 0..8 {
        let k = 1.0 - (i as f32 / 9.0);
        palette.extend_from_slice(&[
            (r as f32 * k) as u8,
            (g as f32 * k) as u8,
            (b as f32 * k) as u8,
            0,
        ]);
    }
    reg_set(explorer, "AccentPalette", REG_BINARY, &palette);

    unsafe {
        // WM_DWMCOLORIZATIONCOLORCHANGED tells running apps to re-read the accent.
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_DWMCOLORIZATIONCOLORCHANGED,
            WPARAM(abgr as usize),
            LPARAM(0),
            SMTO_ABORTIFHUNG,
            400,
            None,
        );
        let theme = wide("ImmersiveColorSet");
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(theme.as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            400,
            None,
        );
    }
}

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "Carasoul";
/// The name this app registered under before it was called carasoul. Cleaned up on
/// every start so an upgrade does not leave two entries racing at logon.
const RUN_NAME_LEGACY: &str = "WallpaperSwitcher";
/// Where the "you have seen the first-run balloon" flag lives.
const APP_KEY: &str = r"Software\Carasoul";
const SEEN_INTRO: &str = "Introduced";

/// Registers (or removes) "start with Windows" for the current executable.
pub fn register_autostart(enable: bool) -> bool {
    if !enable {
        reg_delete(RUN_KEY, RUN_NAME);
        reg_delete(RUN_KEY, RUN_NAME_LEGACY);
        return true;
    }
    reg_delete(RUN_KEY, RUN_NAME_LEGACY);
    match std::env::current_exe() {
        Ok(p) => reg_set_str(RUN_KEY, RUN_NAME, &format!("\"{}\"", p.display())),
        Err(_) => false,
    }
}

/// Whether the startup entry is in place right now, for the tray menu's tick.
pub fn autostart_enabled() -> bool {
    reg_get_str(RUN_KEY, RUN_NAME).is_some_and(|v| !v.is_empty())
}

/// True the first time it is called, then false forever after — used to show the
/// "here is how you drive a windowless app" balloon exactly once.
pub fn claim_first_run() -> bool {
    if reg_get_dword(APP_KEY, SEEN_INTRO) == Some(1) {
        return false;
    }
    reg_set_dword(APP_KEY, SEEN_INTRO, 1)
}

