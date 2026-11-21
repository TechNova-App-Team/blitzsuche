#![windows_subsystem = "windows"]

mod drives;
mod hook;
mod index;
mod recent;
mod shell;

use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tao::dpi::{PhysicalPosition, PhysicalSize};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::windows::{WindowBuilderExtWindows, WindowExtWindows};
use tao::window::{Theme, Window, WindowBuilder};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HWND, POINT};
use windows::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};

use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint};
use windows::Win32::System::Threading::{AttachThreadInput, CreateMutexW, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, GWL_EXSTYLE, GetForegroundWindow, GetWindowLongPtrW, GetWindowThreadProcessId, HWND_TOPMOST, SW_HIDE,
    SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    WS_EX_NOACTIVATE, SW_SHOW, SWP_SHOWWINDOW,
};
use windows::core::{s, w};
use wry::http::{Request, Response};
use wry::{WebContext, WebView, WebViewBuilder};

use index::Index;
use recent::Recent;
use shell::App;

const FLYOUT_HTML: &str = include_str!("../ui/flyout.html");
const PILL_HTML: &str = include_str!("../ui/pill.html");

enum Ev {
    Toggle,
    FocusCheck,
    Refocus,
    Dismiss,
    Raise,
    Fly(String),
    Results(String, String),
}

type Apps = Arc<RwLock<Vec<App>>>;

fn main() {
    unsafe {
        let _m = CreateMutexW(None, true, w!("Local\\Blitzsuche-Instanz"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        std::mem::forget(_m);
    }

    let index: drives::Shared = Arc::new(RwLock::new(Index::default()));
    drives::start(index.clone());

    let apps: Apps = Arc::new(RwLock::new(Vec::new()));
    {
        let apps = apps.clone();
        std::thread::spawn(move || {
            shell::com_init();
            loop {
                let list = shell::list_apps();
                if !list.is_empty() {
                    *apps.write().unwrap() = list;
                }
                std::thread::sleep(Duration::from_secs(600));
            }
        });
    }
    let recent = Arc::new(Mutex::new(Recent::load()));

    let event_loop = EventLoopBuilder::<Ev>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    {
        let p = proxy.clone();
        let p2 = proxy.clone();
        hook::start(
            move || {
                let _ = p.send_event(Ev::Toggle);
            },
            move || {
                let _ = p2.send_event(Ev::Dismiss);
            },
        );
    }
    {
        let p = proxy.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(250));
            if hook::VISIBLE.load(Ordering::SeqCst) {
                let _ = p.send_event(Ev::Raise);
            }
        });
    }
    if let Some(r) = load_rect() {
        hook::set_rect(r);
    }
    std::thread::spawn(|| {
        shell::com_init();
        let Some(mut uia) = shell::Uia::new() else { return };
        // A centred taskbar shifts the search box whenever an app opens or
        // closes, so check often: a stale rect lets clicks reach Windows Search
        // and misplaces the pill.
        let mut tick = 0u32;
        loop {
            if !hook::VISIBLE.load(Ordering::SeqCst) {
                if let Some(r) = uia.search_box(tick % 40 == 0) {
                    if hook::rect() != Some(r) {
                        save_rect(r);
                    }
                    hook::set_rect(r);
                }
            }
            tick = tick.wrapping_add(1);
            std::thread::sleep(Duration::from_millis(250));
        }
    });

    let search_tx = start_search_worker(index.clone(), apps.clone(), recent.clone(), proxy.clone());
    let icon_tx = start_icon_worker();

    // ---- windows -------------------------------------------------------
    let flyout = WindowBuilder::new()
        .with_title("Blitzsuche")
        .with_decorations(false)
        .with_transparent(true)
        .with_visible(false)
        .with_resizable(false)
        .with_always_on_top(true)
        .with_skip_taskbar(true)
        .with_undecorated_shadow(false)
        .with_theme(Some(Theme::Dark))
        .with_inner_size(PhysicalSize::new(832u32, 864u32))
        .build(&event_loop)
        .expect("Fenster");
    round_corners(&flyout);
    acrylic(&flyout);

    let pill = WindowBuilder::new()
        .with_title("Blitzsuche Eingabe")
        .with_decorations(false)
        .with_transparent(true)
        .with_visible(false)
        .with_resizable(false)
        .with_always_on_top(true)
        .with_skip_taskbar(true)
        .with_undecorated_shadow(false)
        .with_theme(Some(Theme::Dark))
        .with_inner_size(PhysicalSize::new(220u32, 32u32))
        .build(&event_loop)
        .expect("Fenster");

    let mut ctx = WebContext::new(Some(recent::data_dir().join("webview")));
    let fly_wv = make_webview(&mut ctx, &flyout, "flyout.html", icon_tx.clone(), {
        let p = proxy.clone();
        move |s| {
            let _ = p.send_event(Ev::Fly(s));
        }
    });
    let pill_wv = make_webview(&mut ctx, &pill, "pill.html", icon_tx, {
        let p = proxy.clone();
        move |s| {
            let _ = p.send_event(Ev::Fly(s));
        }
    });

    let mut visible = false;
    let mut last_q = String::new();
    let accent = shell::accent();
    no_activate(&flyout);

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        let _keep = &ctx;
        match event {
            Event::UserEvent(Ev::Toggle) => {
                if visible {
                    hide(&flyout, &pill, &fly_wv, &pill_wv, &mut visible);
                } else {
                    last_q.clear();
                    let state = home_state(&recent, &index, &accent);
                    show(&flyout, &pill, &fly_wv, &pill_wv, &state);
                    let p = proxy.clone();
                    std::thread::spawn(move || {
                        for ms in [60, 200] {
                            std::thread::sleep(Duration::from_millis(ms));
                            let _ = p.send_event(Ev::Refocus);
                        }
                    });
                    visible = true;
                }
            }
            Event::UserEvent(Ev::FocusCheck) => {
                if visible {
                    let fg = unsafe { GetForegroundWindow() };
                    if fg != hwnd(&flyout) && fg != hwnd(&pill) {

                        hide(&flyout, &pill, &fly_wv, &pill_wv, &mut visible);
                    }
                }
            }
            Event::UserEvent(Ev::Dismiss) => {
                if visible {
                    hide(&flyout, &pill, &fly_wv, &pill_wv, &mut visible);
                }
            }
            Event::UserEvent(Ev::Raise) => {
                if visible {
                    raise_pill(&pill);
                }
            }
            Event::UserEvent(Ev::Refocus) => {
                if visible {
                    focus_pill(&pill, &pill_wv);
                    raise_pill(&pill);
                }
            }
            Event::UserEvent(Ev::Fly(msg)) => {
                if msg.contains("\"dbg\"") || msg.contains("\"err\"") {
                    recent::log(&msg[..msg.len().min(300)]);
                }
                let Ok(v) = serde_json::from_str::<Value>(&msg) else { return };
                match v["t"].as_str() {
                    Some("q") => {
                        let q = v["v"].as_str().unwrap_or("").trim().to_string();
                        if q == last_q {
                            return;
                        }
                        last_q = q.clone();
                        if q.is_empty() {
                            let state = home_state(&recent, &index, &accent);
                            let _ = fly_wv.evaluate_script(&format!("B.home({state})"));
                        } else {
                            let _ = search_tx.send(q);
                        }
                    }
                    Some("key") => {
                        if v["k"] == "Escape" {
                            hide(&flyout, &pill, &fly_wv, &pill_wv, &mut visible);
                        } else {
                            let _ = fly_wv.evaluate_script(&format!("B.key({msg})"));
                        }
                    }
                    Some("open") => {
                        let it = &v["it"];
                        let k = it["k"].as_str().unwrap_or("file");
                        let id = it["id"].as_str().unwrap_or("");
                        let action = v["a"].as_str().unwrap_or("open");
                        if id.is_empty() {
                            return;
                        }
                        if action == "copy" {
                            shell::set_clipboard(id);
                        } else {
                            hide(&flyout, &pill, &fly_wv, &pill_wv, &mut visible);
                            shell::open(k, id, action);
                        }
                        recent.lock().unwrap().push(recent::Item {
                            k: k.into(),
                            id: id.into(),
                            name: it["name"].as_str().unwrap_or("").into(),
                            sub: it["sub"].as_str().unwrap_or("").into(),
                            loc: it["loc"].as_str().unwrap_or("").into(),
                            t: 0,
                        });
                    }
                    Some("forget") => {
                        let id = v["id"].as_str().unwrap_or("");
                        recent.lock().unwrap().remove(id);
                    }
                    _ => {}
                }
            }
            Event::UserEvent(Ev::Results(q, json)) => {
                if visible && q == last_q {
                    let _ = fly_wv.evaluate_script(&format!("B.results({json})"));
                }
            }
            Event::WindowEvent { event: WindowEvent::Focused(false), .. } => {
                let p = proxy.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(120));
                    let _ = p.send_event(Ev::FocusCheck);
                });
            }
            _ => {}
        }
    });
}

fn make_webview(
    ctx: &mut WebContext,
    win: &Window,
    page: &str,
    icon_tx: Sender<IconJob>,
    on_msg: impl Fn(String) + 'static,
) -> WebView {
    WebViewBuilder::new_with_web_context(ctx)
        .with_transparent(true)
        .with_background_color((0, 0, 0, 0))
        .with_devtools(cfg!(debug_assertions))
        .with_asynchronous_custom_protocol("bs".into(), move |_, req, responder| {
            let path = req.uri().path().to_string();
            let body: Option<&'static str> = match path.as_str() {
                "/flyout.html" => Some(FLYOUT_HTML),
                "/pill.html" => Some(PILL_HTML),
                _ => None,
            };
            if let Some(b) = body {
                let r = Response::builder()
                    .header("Content-Type", "text/html; charset=utf-8")
                    .body(Cow::Borrowed(b.as_bytes()))
                    .unwrap();
                responder.respond(r);
            } else if path == "/icon" {
                let _ = icon_tx.send(IconJob { query: req.uri().query().unwrap_or("").to_string(), responder });
            } else {
                responder.respond(Response::builder().status(404).body(Cow::Borrowed(&[][..])).unwrap());
            }
        })
        .with_ipc_handler(move |req: Request<String>| on_msg(req.body().clone()))
        .with_url(format!("http://bs.localhost/{page}"))
        .build(win)
        .expect("WebView2")
}

fn hwnd(w: &Window) -> HWND {
    HWND(w.hwnd() as *mut c_void)
}

fn round_corners(w: &Window) {
    let pref = DWMWCP_ROUND;
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd(w),
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const _ as *const c_void,
            size_of_val(&pref) as u32,
        );
    }
}

/// Windows 11 flyout material (DWM "transient window" acrylic, dark).
/// DWM only draws it while the window is active, which is why the flyout -
/// not the search box - holds the keyboard focus.
fn acrylic(w: &Window) {
    #[repr(C)]
    struct AccentPolicy {
        state: u32,
        flags: u32,
        color: u32,
        anim: u32,
    }
    #[repr(C)]
    struct Data {
        attrib: u32,
        data: *mut c_void,
        size: usize,
    }
    type Swca = unsafe extern "system" fn(HWND, *mut Data) -> i32;
    let h = hwnd(w);
    unsafe {
        let dark: i32 = 1;
        let _ = DwmSetWindowAttribute(h, DWMWA_USE_IMMERSIVE_DARK_MODE, &dark as *const _ as *const c_void, 4);
        let Ok(user32) = GetModuleHandleW(w!("user32.dll")) else { return };
        let Some(f) = GetProcAddress(user32, s!("SetWindowCompositionAttribute")) else { return };
        let f: Swca = std::mem::transmute(f);
        // ACCENT_ENABLE_ACRYLICBLURBEHIND; tint is 0xAABBGGRR
        let mut policy = AccentPolicy { state: 4, flags: 0, color: 0x7A1C1C1C, anim: 0 };
        let mut data = Data { attrib: 19, data: &mut policy as *mut _ as *mut c_void, size: size_of::<AccentPolicy>() };
        f(h, &mut data);
    }
}

/// The flyout never takes the focus; the search box keeps the keyboard.
fn no_activate(w: &Window) {
    unsafe {
        let h = hwnd(w);
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        SetWindowLongPtrW(h, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE.0 as isize);
    }
}

fn force_foreground(h: HWND) {
    unsafe {
        let fg = GetForegroundWindow();
        let ft = GetWindowThreadProcessId(fg, None);
        let me = GetCurrentThreadId();
        let attached = ft != 0 && ft != me && AttachThreadInput(me, ft, true).as_bool();
        let _ = SetForegroundWindow(h);
        let _ = BringWindowToTop(h);
        let _ = SetFocus(h);
        if attached {
            let _ = AttachThreadInput(me, ft, false);
        }
    }
}

fn focus_pill(pill: &Window, pill_wv: &WebView) {
    force_foreground(hwnd(pill));
    let _ = pill_wv.focus();
    let _ = pill_wv.evaluate_script("P.focus()");
}

/// Start menu (Ctrl+Esc / Win) still open? Its window would stay on top of
/// the flyout, so close it with Esc and wait briefly until it is gone.
fn close_start_menu() {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
    use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VK_ESCAPE};
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
    unsafe fn is_start(h: HWND) -> bool {
        unsafe {
            let mut cls = [0u16; 64];
            let n = GetClassNameW(h, &mut cls) as usize;
            if String::from_utf16_lossy(&cls[..n]) != "Windows.UI.Core.CoreWindow" {
                return false;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(h, Some(&mut pid));
            let Ok(ph) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return false };
            let mut buf = [0u16; 260];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(ph, PROCESS_NAME_WIN32, windows::core::PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
            let _ = CloseHandle(ph);
            let path = String::from_utf16_lossy(&buf[..len as usize]).to_lowercase();
            ok && (path.ends_with(r"\searchhost.exe") || path.ends_with(r"\startmenuexperiencehost.exe"))
        }
    }
    unsafe {
        let fg = GetForegroundWindow();
        if !is_start(fg) {
            return;
        }
        let key = |flags| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_ESCAPE, wScan: 0, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
        };
        SendInput(&[key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)], size_of::<INPUT>() as i32);
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(10));
            if GetForegroundWindow() != fg {
                break;
            }
        }
    }
}

fn show(flyout: &Window, pill: &Window, fly_wv: &WebView, pill_wv: &WebView, state: &str) {
    close_start_menu();
    let r = hook::rect().unwrap_or(windows::Win32::Foundation::RECT { left: 8, top: 1040, right: 228, bottom: 1072 });
    let (work, scale) = unsafe {
        let mon = MonitorFromPoint(POINT { x: (r.left + r.right) / 2, y: (r.top + r.bottom) / 2 }, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        (mi.rcWork, dx as f64 / 96.0)
    };
    let px = |v: f64| (v * scale).round() as i32;
    let gap = px(12.0);
    let w = px(832.0).min(work.right - work.left - 2 * gap);
    let h = px(864.0).min(work.bottom - work.top - 2 * gap);
    let x = r.left.clamp(work.left + gap, (work.right - gap - w).max(work.left));
    let y = work.bottom - gap - h;

    hook::set_flyout_rect(windows::Win32::Foundation::RECT { left: x, top: y, right: x + w, bottom: y + h });
    hook::VISIBLE.store(true, Ordering::SeqCst);
    flyout.set_outer_position(PhysicalPosition::new(x, y));
    flyout.set_inner_size(PhysicalSize::new(w as u32, h as u32));
    pill.set_outer_position(PhysicalPosition::new(r.left, r.top));
    pill.set_inner_size(PhysicalSize::new((r.right - r.left) as u32, (r.bottom - r.top) as u32));

    let _ = fly_wv.evaluate_script(&format!("B.show({state})"));
    let _ = pill_wv.evaluate_script("P.show()");
    unsafe {
        let _ = ShowWindow(hwnd(flyout), SW_SHOWNOACTIVATE);
        let _ = SetWindowPos(hwnd(flyout), HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        let _ = ShowWindow(hwnd(pill), SW_SHOW);
    }
    focus_pill(pill, pill_wv);
    raise_pill(pill);
}

fn hide(flyout: &Window, pill: &Window, fly_wv: &WebView, pill_wv: &WebView, visible: &mut bool) {
    *visible = false;
    unsafe {
        let _ = ShowWindow(hwnd(pill), SW_HIDE);
        let _ = ShowWindow(hwnd(flyout), SW_HIDE);
    }
    let _ = fly_wv.evaluate_script("B.hide()");
    let _ = pill_wv.evaluate_script("P.hide()");
    hook::VISIBLE.store(false, Ordering::SeqCst);
}

fn home_state(recent: &Mutex<Recent>, index: &drives::Shared, accent: &str) -> String {
    let items = recent.lock().unwrap().visible(13);
    static LAST: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    if let Ok(i) = index.try_read() {
        LAST.store(i.live(), Ordering::Relaxed);
    }
    let count = LAST.load(Ordering::Relaxed);
    json!({
        "recent": items,
        "count": count,
        "pending": drives::PENDING.load(Ordering::SeqCst),
        "now": recent::now(),
        "accent": accent,
    })
    .to_string()
}

// ---- search ------------------------------------------------------------

fn category(name: &str, dir: bool) -> &'static str {
    if dir {
        return "Ordner";
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "exe" | "lnk" | "bat" | "cmd" | "msi" | "ps1" | "appref-ms" | "url" => "Apps",
        "pdf" | "doc" | "docx" | "txt" | "md" | "rtf" | "odt" | "xls" | "xlsx" | "csv" | "ods" | "ppt" | "pptx"
        | "odp" | "epub" => "Dokumente",
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "heic" | "svg" | "ico" | "tif" | "tiff" | "raw" => "Fotos",
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "wmv" | "m4v" => "Videos",
        "mp3" | "wav" | "flac" | "m4a" | "ogg" | "aac" | "wma" | "opus" => "Musik",
        _ => "Dateien",
    }
}

const GROUP_ORDER: [&str; 7] = ["Apps", "Ordner", "Dokumente", "Fotos", "Videos", "Musik", "Dateien"];

fn start_search_worker(index: drives::Shared, apps: Apps, recent: Arc<Mutex<Recent>>, proxy: EventLoopProxy<Ev>) -> Sender<String> {
    let (tx, rx): (Sender<String>, Receiver<String>) = channel();
    std::thread::spawn(move || {
        shell::com_init();
        let mut types: HashMap<String, String> = HashMap::new();
        while let Ok(mut q) = rx.recv() {
            while let Ok(newer) = rx.try_recv() {
                q = newer;
            }
            let json = run_search(&q, &index, &apps, &recent, &mut types);
            let _ = proxy.send_event(Ev::Results(q, json));
        }
    });
    tx
}

fn run_search(
    q: &str,
    index: &drives::Shared,
    apps: &Apps,
    recent: &Mutex<Recent>,
    types: &mut HashMap<String, String>,
) -> String {
    let started = std::time::Instant::now();
    let tokens: Vec<String> = q.to_lowercase().split_whitespace().map(String::from).collect();
    let full = tokens.join(" ");
    let recent = recent.lock().unwrap();

    // (score, category, item json)
    let mut all: Vec<(i32, &'static str, Value)> = Vec::new();

    for a in apps.read().unwrap().iter() {
        if !tokens.iter().all(|t| a.lower.contains(t.as_str())) {
            continue;
        }
        let pos = a.lower.find(tokens[0].as_str()).unwrap_or(0);
        let mut s = index::score_name(a.lower.as_bytes(), &full, &tokens, pos, 0) + 320;
        if recent.contains(&a.id) {
            s += 400;
        }
        all.push((s, "Apps", json!({"k":"app","id":a.id,"name":a.name,"sub":"App","loc":""})));
    }
    all.sort_by(|a, b| b.0.cmp(&a.0));
    all.truncate(8);

    let total;
    {
        let ix = index.read().unwrap();
        let hits = ix.search(&tokens, &full, 80);
        total = hits.len();
        for h in hits {
            let path = ix.path(h.idx);
            if path.contains(r"\Microsoft\Windows\Recent\") {
                continue;
            }
            let name = ix.name(h.idx).to_string();
            let dir = ix.is_dir(h.idx);
            let mut s = h.score;
            if recent.contains(&path) {
                s += 400;
            }
            let ext_key = if dir {
                "<dir>".to_string()
            } else {
                name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
            };
            let sub = types.entry(ext_key).or_insert_with(|| shell::type_name(&path, dir)).clone();
            let loc = path.rsplit_once('\\').map(|(p, _)| if p.ends_with(':') { format!("{p}\\") } else { p.to_string() }).unwrap_or_default();
            let cat = category(&name, dir);
            all.push((s, cat, json!({"k": if dir {"dir"} else {"file"}, "id": path, "name": name, "sub": sub, "loc": loc})));
        }
    }
    all.sort_by(|a, b| b.0.cmp(&a.0));

    let best = all.first().map(|x| x.2.clone());
    let mut groups: Vec<Value> = Vec::new();
    for g in GROUP_ORDER {
        let max = if g == "Apps" { 6 } else { 8 };
        let items: Vec<Value> = all.iter().skip(1).filter(|x| x.1 == g).take(max).map(|x| x.2.clone()).collect();
        if !items.is_empty() {
            groups.push(json!({"title": g, "items": items}));
        }
    }
    json!({
        "q": q,
        "best": best,
        "groups": groups,
        "total": total,
        "ms": started.elapsed().as_millis() as u64,
        "pending": drives::PENDING.load(Ordering::SeqCst),
    })
    .to_string()
}

// ---- icons -------------------------------------------------------------

struct IconJob {
    query: String,
    responder: wry::RequestAsyncResponder,
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let h = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                if let Some(v) = h {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(b'%');
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn start_icon_worker() -> Sender<IconJob> {
    let (tx, rx) = channel::<IconJob>();
    std::thread::spawn(move || {
        shell::com_init();
        let mut cache: HashMap<String, Option<Arc<Vec<u8>>>> = HashMap::new();
        while let Ok(job) = rx.recv() {
            let mut key = String::new();
            let mut size = 48;
            let mut thumb = false;
            for part in job.query.split('&') {
                if let Some((k, v)) = part.split_once('=') {
                    match k {
                        "k" => key = url_decode(v),
                        "s" => size = v.parse().unwrap_or(48).clamp(16, 256),
                        "t" => thumb = v == "1",
                        _ => {}
                    }
                }
            }
            let ck = format!("{size}|{thumb}|{key}");
            let png = cache
                .entry(ck)
                .or_insert_with(|| shell::icon_png(&key, size, thumb).map(Arc::new))
                .clone();
            let resp = match png {
                Some(p) => Response::builder()
                    .header("Content-Type", "image/png")
                    .header("Cache-Control", "max-age=3600")
                    .body(Cow::Owned(p.as_ref().clone()))
                    .unwrap(),
                None => Response::builder().status(404).body(Cow::Borrowed(&[][..])).unwrap(),
            };
            job.responder.respond(resp);
        }
    });
    tx
}


fn save_rect(r: windows::Win32::Foundation::RECT) {
    let s = format!("{} {} {} {}", r.left, r.top, r.right, r.bottom);
    let _ = std::fs::write(recent::data_dir().join("searchbox.txt"), s);
}

fn load_rect() -> Option<windows::Win32::Foundation::RECT> {
    let s = std::fs::read_to_string(recent::data_dir().join("searchbox.txt")).ok()?;
    let v: Vec<i32> = s.split_whitespace().filter_map(|x| x.parse().ok()).collect();
    (v.len() == 4).then(|| windows::Win32::Foundation::RECT { left: v[0], top: v[1], right: v[2], bottom: v[3] })
}

/// The taskbar is a topmost window too and pushes itself back to the top now
/// and then; keep the search box above it while the flyout is open.
fn raise_pill(pill: &Window) {
    unsafe {
        let _ = SetWindowPos(hwnd(pill), HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW);
    }
}
