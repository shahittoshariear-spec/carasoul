//! The notification-area icon — the only thing carasoul puts on screen by
//! itself, and the reason the app can start with the PC without ever showing a
//! window. Windows Defender lives in the same place and for the same reasons.
//!
//! Everything here hangs off the overlay window's message proc (`sys::wndproc`
//! forwards to [`handle_message`]), which is driven by the same message pump the
//! frame loop already runs. The proc does no work of its own: a click or a menu
//! pick is recorded as a bit in [`PENDING`] and the frame loop acts on it, so
//! nothing re-enters the renderer from inside a Win32 callback.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
    BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HGDIOBJ, RGBQUAD,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIIF_NOSOUND, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos,
    GetSystemMetrics, PostMessageW, RegisterWindowMessageW, SetForegroundWindow, TrackPopupMenu,
    HICON, ICONINFO, MENU_ITEM_FLAGS, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, SM_CXSMICON,
    SM_CYSMICON, TPM_RIGHTBUTTON, WM_APP, WM_COMMAND, WM_CONTEXTMENU, WM_LBUTTONDBLCLK,
    WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP,
};

/// Selection events a notification icon delivers in version 4. windows-rs does not
/// name them; the left/right clicks below stay mouse messages, these are the
/// keyboard-invoked equivalents.
const NIN_SELECT: u32 = 0x0400;
const NIN_KEYSELECT: u32 = 0x0401;

/// The full-colour master raster, at the largest size the export ships that is
/// still cheap to embed. The tray asks for 16–32 px depending on DPI and the
/// shell draws larger variants in a few places, so this is scaled to whatever
/// size the shell wants rather than being shipped in one fixed size.
const MASTER_PNG: &[u8] = include_bytes!("../assets/icons/android/icon-512.png");

/// Menu ids, chosen so each is also a distinct bit in the pending set below.
pub const CMD_OPEN: u32 = 1;
pub const CMD_NEXT: u32 = 2;
pub const CMD_FOLDER: u32 = 4;
pub const CMD_RESCAN: u32 = 8;
pub const CMD_AUTOSTART: u32 = 16;
pub const CMD_QUIT: u32 = 32;

/// Identifies our icon within the notification area.
const TRAY_ID: u32 = 1;
/// Our own callback message; anything >= WM_APP is free for the app to use.
const CALLBACK: u32 = WM_APP + 1;
/// Posted by a second launch (`sys::wake_running_instance`) to the instance that
/// already owns the icon, so re-launching the app opens the shelf instead of
/// starting a rival copy.
pub const WM_OPEN_SHELF: u32 = WM_APP + 2;

static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);
static TRAY_ICON: AtomicIsize = AtomicIsize::new(0);
/// Whether the shell accepted the icon. A tray-only app with no tray icon cannot
/// be driven at all, so a failure here is worth being able to see.
static TRAY_ADDED: AtomicBool = AtomicBool::new(false);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static HAS_WALLPAPERS: AtomicBool = AtomicBool::new(false);
static PENDING: AtomicU32 = AtomicU32::new(0);
static TOOLTIP: Mutex<String> = Mutex::new(String::new());

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Copes the text into one of `NOTIFYICONDATAW`'s fixed arrays, truncating rather
/// than overflowing and always NUL-terminating.
fn put(dst: &mut [u16], s: &str) {
    let n = dst.len().saturating_sub(1);
    let mut written = 0;
    for c in s.encode_utf16().take(n) {
        dst[written] = c;
        written += 1;
    }
    dst[written] = 0;
}

// ---------------------------------------------------------------------------
// lifecycle
// ---------------------------------------------------------------------------

pub fn init(hwnd: HWND) {
    TRAY_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
    unsafe {
        let name = wide("TaskbarCreated");
        TASKBAR_CREATED.store(
            RegisterWindowMessageW(PCWSTR(name.as_ptr())),
            Ordering::Relaxed,
        );
    }
    add();
}

/// Whether the shell is currently showing our icon.
#[cfg(test)]
fn added() -> bool {
    TRAY_ADDED.load(Ordering::Relaxed)
}

/// Re-adds the icon (and re-applies the tooltip). Called on startup and whenever
/// Explorer restarts, which takes the notification area — and our icon with it.
pub fn refresh() {
    add();
}

fn add() {
    let hwnd = hwnd();
    if hwnd.is_invalid() {
        return;
    }
    let size = unsafe { GetSystemMetrics(SM_CXSMICON).max(GetSystemMetrics(SM_CYSMICON)) };
    let icon = load_icon(size);
    let mut nid = empty();
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = CALLBACK;
    nid.hIcon = icon.unwrap_or_default();
    if let Ok(tip) = TOOLTIP.lock() {
        put(&mut nid.szTip, &tip);
    }
    unsafe {
        // Version 4 is what Windows 10/11 want; the event in the low word of the
        // callback's `wParam` is what `handle_message` reads either way.
        let accepted = Shell_NotifyIconW(NIM_ADD, &nid).as_bool();
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
        TRAY_ADDED.store(accepted, Ordering::Relaxed);
    }
    let old = TRAY_ICON.swap(icon.map_or(0, |i| i.0 as isize), Ordering::Relaxed);
    if old != 0 {
        unsafe {
            let _ = DestroyIcon(HICON(old as *mut c_void));
        }
    }
}

pub fn remove() {
    let hwnd = hwnd();
    if !hwnd.is_invalid() {
        let nid = empty();
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
        }
    }
    TRAY_HWND.store(0, Ordering::Relaxed);
    TRAY_ADDED.store(false, Ordering::Relaxed);
    let icon = TRAY_ICON.swap(0, Ordering::Relaxed);
    if icon != 0 {
        unsafe {
            let _ = DestroyIcon(HICON(icon as *mut c_void));
        }
    }
}

fn hwnd() -> HWND {
    HWND(TRAY_HWND.load(Ordering::Relaxed) as *mut c_void)
}

fn empty() -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd(),
        uID: TRAY_ID,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// what the app tells the tray
// ---------------------------------------------------------------------------

/// Shown when the shell asks for the icon's tooltip, and after a re-add.
pub fn set_tooltip(text: &str) {
    if let Ok(mut tip) = TOOLTIP.lock() {
        tip.clear();
        tip.push_str(text);
    }
    let hwnd = hwnd();
    if hwnd.is_invalid() {
        return;
    }
    let mut nid = empty();
    nid.uFlags = NIF_TIP;
    put(&mut nid.szTip, text);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// Greys out the items that need a wallpaper folder with something in it.
pub fn set_has_wallpapers(has: bool) {
    HAS_WALLPAPERS.store(has, Ordering::Relaxed);
}

/// A balloon from the notification area: how a windowless app gets to say
/// something without opening a window. Silent — it is used for first-run advice
/// and for failures, not for anything worth a sound.
pub fn notify(title: &str, text: &str) {
    let hwnd = hwnd();
    if hwnd.is_invalid() {
        return;
    }
    let mut nid = empty();
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO | NIIF_NOSOUND;
    nid.Anonymous.uTimeout = 10_000;
    put(&mut nid.szInfoTitle, title);
    put(&mut nid.szInfo, text);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// Drains the commands the message proc recorded, as a set of `CMD_*` bits.
pub fn poll() -> u32 {
    PENDING.swap(0, Ordering::Relaxed)
}

fn post(cmd: u32) {
    PENDING.fetch_or(cmd, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// window messages
// ---------------------------------------------------------------------------

/// Handles the messages addressed to the tray. Returns true if the message was
/// ours and should not go on to `DefWindowProcW`.
pub fn handle_message(hwnd: HWND, msg: u32, w: WPARAM, _l: LPARAM) -> bool {
    let taskbar = TASKBAR_CREATED.load(Ordering::Relaxed);

    if msg == CALLBACK {
        // LOWORD(wParam) is the event in every notification-icon version; the
        // older ones simply put nothing in the high word.
        match (w.0 as u32) & 0xFFFF {
            WM_LBUTTONUP | WM_LBUTTONDBLCLK | NIN_SELECT | NIN_KEYSELECT => post(CMD_OPEN),
            WM_RBUTTONUP | WM_CONTEXTMENU => unsafe { show_menu(hwnd) },
            _ => {}
        }
        return true;
    }

    if msg == WM_OPEN_SHELF {
        post(CMD_OPEN);
        return true;
    }

    if msg == WM_COMMAND {
        let id = (w.0 as u32) & 0xFFFF;
        if matches!(
            id,
            CMD_OPEN | CMD_NEXT | CMD_FOLDER | CMD_RESCAN | CMD_AUTOSTART | CMD_QUIT
        ) {
            post(id);
            return true;
        }
        return false;
    }

    if taskbar != 0 && msg == taskbar {
        refresh();
        return true;
    }
    false
}

/// The right-click menu. `TrackPopupMenu` swallows the pick and posts it back as
/// `WM_COMMAND`, which is why the ids above double as bit positions.
unsafe fn show_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let has_items = HAS_WALLPAPERS.load(Ordering::Relaxed);
    let usable = if has_items {
        MF_STRING
    } else {
        MF_STRING | MF_GRAYED
    };
    let checked = if crate::sys::autostart_enabled() {
        MF_STRING | MF_CHECKED
    } else {
        MF_STRING
    };
    let entries: [(u32, &str, MENU_ITEM_FLAGS); 6] = [
        (CMD_OPEN, "Open shelf", MF_STRING),
        (CMD_NEXT, "Next wallpaper", usable),
        (CMD_FOLDER, "Open wallpaper folder", MF_STRING),
        (CMD_RESCAN, "Rescan wallpapers", usable),
        (CMD_AUTOSTART, "Start with Windows", checked),
        (CMD_QUIT, "Quit", MF_STRING),
    ];
    // The label buffers have to outlive the calls that read them.
    let mut owned: Vec<Vec<u16>> = Vec::with_capacity(entries.len());
    for (i, (id, label, flags)) in entries.iter().enumerate() {
        if i == 2 || i == 5 {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        }
        let text = wide(label);
        let _ = AppendMenuW(menu, *flags, *id as usize, PCWSTR(text.as_ptr()));
        owned.push(text);
    }

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // A tray popup is owned by a background window, so without the foreground
    // claim first it will not close when the user clicks away from it.
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, pt.x, pt.y, None, hwnd, None);
    let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
}

// ---------------------------------------------------------------------------
// the icon itself
// ---------------------------------------------------------------------------

/// Scales the embedded master to the size the shell asked for and turns it into
/// an `HICON`.
fn load_icon(size: i32) -> Option<HICON> {
    let size = size.clamp(16, 256) as u32;
    let img = image::load_from_memory(MASTER_PNG).ok()?;
    let rgba = img
        .resize_exact(size, size, image::imageops::FilterType::Lanczos3)
        .to_rgba8();

    unsafe {
        let screen = GetDC(None);
        let memdc = CreateCompatibleDC(Some(screen));
        ReleaseDC(None, screen);

        let (w, h) = (rgba.width() as i32, rgba.height() as i32);
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
        let Ok(color) = CreateDIBSection(Some(memdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
        else {
            let _ = DeleteDC(memdc);
            return None;
        };

        // 0xAARRGGBB little-endian is exactly the BGRA byte order a DIB wants.
        let px = std::slice::from_raw_parts_mut(bits as *mut u32, (w * h) as usize);
        for (dst, p) in px.iter_mut().zip(rgba.pixels()) {
            *dst =
                ((p[3] as u32) << 24) | ((p[0] as u32) << 16) | ((p[1] as u32) << 8) | p[2] as u32;
        }

        // The colour bitmap carries the alpha, so the AND mask stays empty.
        let mask = CreateBitmap(w, h, 1, 1, None);
        let info = ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let hicon = CreateIconIndirect(&info).ok();

        let _ = DeleteObject(HGDIOBJ(color.0));
        let _ = DeleteObject(HGDIOBJ(mask.0));
        let _ = DeleteDC(memdc);
        hicon
    }
}

/// Small twin of the icon, for the overlay's window class. Built from the same
/// master so the two can never drift apart.
pub fn window_icon() -> HICON {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) };
    load_icon(size).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Gdi::{CreateCompatibleBitmap, GetPixel, CLR_INVALID};
    use windows::Win32::UI::WindowsAndMessaging::{DrawIconEx, DI_NORMAL};

    /// The icon is only usable if GDI keeps the alpha channel from the DIB we
    /// hand it: draw it onto a white surface and check that the corners stay
    /// white (transparent) while the middle is painted.
    #[test]
    fn icon_keeps_its_alpha() {
        let icon = load_icon(32).expect("icon");
        unsafe {
            let screen = GetDC(None);
            let memdc = CreateCompatibleDC(Some(screen));
            let bitmap = CreateCompatibleBitmap(screen, 32, 32);
            ReleaseDC(None, screen);
            let prev = windows::Win32::Graphics::Gdi::SelectObject(memdc, HGDIOBJ(bitmap.0));

            let white = windows::Win32::Foundation::COLORREF(0x00FF_FFFF);
            let brush = windows::Win32::Graphics::Gdi::CreateSolidBrush(white);
            let rect = windows::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: 32,
                bottom: 32,
            };
            windows::Win32::Graphics::Gdi::FillRect(memdc, &rect, brush);
            let _ = windows::Win32::Graphics::Gdi::DeleteObject(HGDIOBJ(brush.0));

            let _ = DrawIconEx(memdc, 0, 0, icon, 32, 32, 0, None, DI_NORMAL);

            // The master is a full-bleed square, so the corners must have been
            // painted: if the alpha had been dropped they would still be white.
            let corner = GetPixel(memdc, 1, 1);
            assert_ne!(corner.0, CLR_INVALID);
            assert_ne!(
                corner.0 & 0x00FF_FFFF,
                0x00FF_FFFF,
                "corner was left unpainted"
            );

            let _ = windows::Win32::Graphics::Gdi::SelectObject(memdc, prev);
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            let _ = DeleteDC(memdc);
        }
        unsafe {
            let _ = DestroyIcon(icon);
        }
    }

    /// A tooltip longer than the fixed array must be clipped, not overflow.
    #[test]
    fn tooltip_is_clipped() {
        let mut buf = [0u16; 8];
        put(&mut buf, "0123456789abcdef");
        assert_eq!(buf[7], 0);
        assert_eq!(String::from_utf16_lossy(&buf[..7]), "0123456");
    }

    #[test]
    fn menu_ids_are_distinct_bits() {
        let all = [
            CMD_OPEN,
            CMD_NEXT,
            CMD_FOLDER,
            CMD_RESCAN,
            CMD_AUTOSTART,
            CMD_QUIT,
        ];
        let mut seen = 0u32;
        for id in all {
            assert_eq!(id.count_ones(), 1, "{id} is not a single bit");
            assert_eq!(seen & id, 0, "{id} collides with another id");
            seen |= id;
        }
    }

    /// The window proc's job: turn whatever the shell sends into a bit the frame
    /// loop can act on, and leave everything else to `DefWindowProcW`.
    #[test]
    fn tray_messages_become_commands() {
        let hwnd = HWND(std::ptr::null_mut());
        let event = |e: u32| WPARAM(e as usize);

        PENDING.store(0, Ordering::Relaxed);
        assert!(handle_message(
            hwnd,
            CALLBACK,
            event(WM_LBUTTONUP),
            LPARAM(0)
        ));
        assert_eq!(poll(), CMD_OPEN, "a left click should open the shelf");

        assert!(handle_message(
            hwnd,
            CALLBACK,
            event(NIN_KEYSELECT),
            LPARAM(0)
        ));
        assert_eq!(poll(), CMD_OPEN, "so should invoking it from the keyboard");

        // Menu picks arrive as WM_COMMAND, with the id in the low word.
        assert!(handle_message(
            hwnd,
            WM_COMMAND,
            WPARAM(CMD_NEXT as usize),
            LPARAM(0)
        ));
        assert_eq!(poll(), CMD_NEXT);
        assert!(!handle_message(hwnd, WM_COMMAND, WPARAM(4242), LPARAM(0)));
        assert_eq!(poll(), 0, "an unknown command must not be acted on");

        // A second launch (see `sys::wake_running_instance`) asks for the same thing.
        assert!(handle_message(hwnd, WM_OPEN_SHELF, WPARAM(0), LPARAM(0)));
        assert_eq!(poll(), CMD_OPEN);

        assert!(!handle_message(hwnd, 0x1234, WPARAM(0), LPARAM(0)));
    }

    /// The shell is the one part of this the tests cannot fake, so the icon is
    /// registered and taken down for real. It needs a window to talk to, and the
    /// overlay is the one the app already has.
    #[test]
    fn icon_registers_with_the_shell() {
        let Ok(overlay) = crate::sys::Overlay::create(64, 64) else {
            panic!("overlay window");
        };
        init(overlay.hwnd);
        assert!(added(), "Shell_NotifyIconW refused the icon");
        set_tooltip("carasoul");
        remove();
        assert!(!added());
    }
}
