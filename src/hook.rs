//! Low-level mouse/keyboard hooks: a click on the taskbar search box and
//! Win+S open Blitzsuche instead of the Windows search.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering::SeqCst};
use windows::Win32::Foundation::{LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, HHOOK, KBDLLHOOKSTRUCT, LLKHF_INJECTED, MSG,
    MSLLHOOKSTRUCT, SetWindowsHookExW, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDOWN, WM_RBUTTONDOWN, WM_SYSKEYDOWN,
};

static L: AtomicI32 = AtomicI32::new(0);
static T: AtomicI32 = AtomicI32::new(0);
static R: AtomicI32 = AtomicI32::new(0);
static B: AtomicI32 = AtomicI32::new(0);
pub static VISIBLE: AtomicBool = AtomicBool::new(false);
static SWALLOW_UP: AtomicBool = AtomicBool::new(false);
static WIN_DOWN: AtomicBool = AtomicBool::new(false);
static S_DOWN: AtomicBool = AtomicBool::new(false);
static FIRE: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();
static DISMISS: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();
// flyout rectangle while open (light dismiss on clicks outside)
static FL: AtomicI32 = AtomicI32::new(0);
static FT: AtomicI32 = AtomicI32::new(0);
static FR: AtomicI32 = AtomicI32::new(0);
static FB: AtomicI32 = AtomicI32::new(0);

pub fn set_flyout_rect(r: RECT) {
    FL.store(r.left, SeqCst);
    FT.store(r.top, SeqCst);
    FR.store(r.right, SeqCst);
    FB.store(r.bottom, SeqCst);
}

fn in_box(x: i32, y: i32, l: &AtomicI32, t: &AtomicI32, r: &AtomicI32, b: &AtomicI32) -> bool {
    x >= l.load(SeqCst) && x < r.load(SeqCst) && y >= t.load(SeqCst) && y < b.load(SeqCst)
}

pub fn set_rect(r: RECT) {
    L.store(r.left, SeqCst);
    T.store(r.top, SeqCst);
    R.store(r.right, SeqCst);
    B.store(r.bottom, SeqCst);
}

pub fn rect() -> Option<RECT> {
    let r = RECT { left: L.load(SeqCst), top: T.load(SeqCst), right: R.load(SeqCst), bottom: B.load(SeqCst) };
    (r.right > r.left).then_some(r)
}

fn fire() {
    if let Some(f) = FIRE.get() {
        f();
    }
}

pub fn start(toggle: impl Fn() + Send + Sync + 'static, dismiss: impl Fn() + Send + Sync + 'static) {
    let _ = FIRE.set(Box::new(toggle));
    let _ = DISMISS.set(Box::new(dismiss));
    std::thread::spawn(|| unsafe {
        let hm = GetModuleHandleW(None).unwrap_or_default();
        let _m: Option<HHOOK> = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse), hm, 0).ok();
        let _k: Option<HHOOK> = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kbd), hm, 0).ok();
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    });
}

unsafe extern "system" fn mouse(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 {
            let m = &*(l.0 as *const MSLLHOOKSTRUCT);
            let msg = w.0 as u32;
            if VISIBLE.load(SeqCst) && (msg == WM_LBUTTONDOWN || msg == WM_RBUTTONDOWN || msg == WM_MBUTTONDOWN) {
                let (x, y) = (m.pt.x, m.pt.y);
                if !in_box(x, y, &FL, &FT, &FR, &FB) && !in_box(x, y, &L, &T, &R, &B) {
                    if let Some(f) = DISMISS.get() {
                        f();
                    }
                }
            }
            if msg == WM_LBUTTONDOWN && !VISIBLE.load(SeqCst) {
                let (x, y) = (m.pt.x, m.pt.y);
                if R.load(SeqCst) > L.load(SeqCst)
                    && x >= L.load(SeqCst)
                    && x < R.load(SeqCst)
                    && y >= T.load(SeqCst)
                    && y < B.load(SeqCst)
                {
                    SWALLOW_UP.store(true, SeqCst);
                    fire();
                    return LRESULT(1);
                }
            }
            if msg == WM_LBUTTONUP && SWALLOW_UP.swap(false, SeqCst) {
                return LRESULT(1);
            }
        }
        CallNextHookEx(None, code, w, l)
    }
}

unsafe extern "system" fn kbd(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 {
            let k = &*(l.0 as *const KBDLLHOOKSTRUCT);
            if k.flags.0 & LLKHF_INJECTED.0 == 0 {
                let msg = w.0 as u32;
                let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
                match k.vkCode {
                    0x5B | 0x5C => WIN_DOWN.store(down, SeqCst),
                    0x53 if WIN_DOWN.load(SeqCst) || S_DOWN.load(SeqCst) => {
                        if down {
                            if !S_DOWN.swap(true, SeqCst) {
                                suppress_start_menu();
                                fire();
                            }
                        } else {
                            S_DOWN.store(false, SeqCst);
                        }
                        return LRESULT(1);
                    }
                    _ => {}
                }
            }
        }
        CallNextHookEx(None, code, w, l)
    }
}

/// A dummy key press between Win-down and Win-up stops the Start menu from
/// opening when Win is released (same trick PowerToys uses).
fn suppress_start_menu() {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(0xE8), wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 },
        },
    };
    let inputs = [key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)];
    unsafe {
        SendInput(&inputs, size_of::<INPUT>() as i32);
    }
}
