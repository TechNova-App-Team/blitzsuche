//! Everything that talks to the Windows shell: installed apps, icons,
//! file type names, launching, clipboard, taskbar search box position.

use std::ffi::c_void;
use std::os::windows::process::CommandExt;
use std::process::Command;
use windows::Win32::Foundation::{HANDLE, HWND, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits,
    GetObjectW, HBITMAP, ReleaseDC,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    IBindCtx,
};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, TreeScope_Descendants, UIA_AutomationIdPropertyId,
};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, FOLDERID_AppsFolder, IEnumShellItems, IShellItem, IShellItemImageFactory,
    KF_FLAG_DEFAULT, SHCreateItemFromParsingName, SHFILEINFOW, SHGFI_TYPENAME, SHGFI_USEFILEATTRIBUTES,
    SHGetFileInfoW, SHGetKnownFolderItem, SIGDN, SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING,
    SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY, SIIGBF_RESIZETOFIT, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, SW_SHOWNORMAL};
use windows::core::{HSTRING, PCWSTR, VARIANT, w};

pub fn com_init() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

#[derive(Clone)]
pub struct App {
    pub name: String,
    pub lower: String,
    pub id: String,
}

fn display_name(item: &IShellItem, kind: SIGDN) -> Option<String> {
    unsafe {
        let p = item.GetDisplayName(kind).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s
    }
}

/// All entries of shell:AppsFolder (classic + Store apps), like the Start menu.
pub fn list_apps() -> Vec<App> {
    let mut out = Vec::new();
    unsafe {
        let Ok(folder) = SHGetKnownFolderItem::<_, IShellItem>(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, HANDLE::default())
        else {
            return out;
        };
        let Ok(en) = folder.BindToHandler::<_, IEnumShellItems>(None::<&IBindCtx>, &BHID_EnumItems) else {
            return out;
        };
        loop {
            let mut arr = [None];
            let mut n = 0u32;
            if en.Next(&mut arr, Some(&mut n)).is_err() || n == 0 {
                break;
            }
            let Some(item) = arr[0].take() else { break };
            let (Some(name), Some(id)) = (
                display_name(&item, SIGDN_NORMALDISPLAY),
                display_name(&item, SIGDN_PARENTRELATIVEPARSING),
            ) else {
                continue;
            };
            out.push(App { lower: name.to_lowercase(), name, id });
        }
    }
    out
}

fn parsing_name(key: &str) -> Option<String> {
    if let Some(id) = key.strip_prefix("app:") {
        Some(format!("shell:AppsFolder\\{id}"))
    } else {
        key.strip_prefix("f:").map(|p| p.to_string())
    }
}

/// PNG icon (or thumbnail) for "app:<id>" or "f:<path>".
pub fn icon_png(key: &str, size: i32, thumb: bool) -> Option<Vec<u8>> {
    let path = parsing_name(key)?;
    unsafe {
        let fac: IShellItemImageFactory =
            SHCreateItemFromParsingName(&HSTRING::from(path.as_str()), None::<&IBindCtx>).ok()?;
        let flags = if thumb { SIIGBF_RESIZETOFIT } else { SIIGBF_ICONONLY | SIIGBF_BIGGERSIZEOK };
        let hbmp = fac.GetImage(SIZE { cx: size, cy: size }, flags).ok()?;
        let png = bitmap_to_png(hbmp);
        let _ = DeleteObject(hbmp);
        png
    }
}

unsafe fn bitmap_to_png(hbmp: HBITMAP) -> Option<Vec<u8>> {
    unsafe {
        let mut bm = BITMAP::default();
        if GetObjectW(hbmp, size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut c_void)) == 0 {
            return None;
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
        let mut bi = BITMAPINFO::default();
        bi.bmiHeader = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let mut px = vec![0u8; (w * h * 4) as usize];
        let dc = GetDC(HWND::default());
        let lines = GetDIBits(dc, hbmp, 0, h as u32, Some(px.as_mut_ptr() as *mut c_void), &mut bi, DIB_RGB_COLORS);
        ReleaseDC(HWND::default(), dc);
        if lines == 0 {
            return None;
        }
        let has_alpha = px.chunks_exact(4).any(|c| c[3] != 0);
        for c in px.chunks_exact_mut(4) {
            c.swap(0, 2); // BGRA -> RGBA
            if !has_alpha {
                c[3] = 255;
            } else if c[3] > 0 && c[3] < 255 {
                // premultiplied -> straight alpha
                let a = c[3] as u32;
                for k in 0..3 {
                    c[k] = ((c[k] as u32 * 255 + a / 2) / a).min(255) as u8;
                }
            }
        }
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w as u32, h as u32);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_compression(png::Compression::Fast);
            let mut wr = enc.write_header().ok()?;
            wr.write_image_data(&px).ok()?;
        }
        Some(out)
    }
}

/// Windows' own type name, e.g. "Windows-Batchdatei". Does not touch the disk.
pub fn type_name(path: &str, dir: bool) -> String {
    let mut info = SHFILEINFOW::default();
    let attr = if dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
    unsafe {
        SHGetFileInfoW(
            &HSTRING::from(path),
            attr,
            Some(&mut info),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_TYPENAME | SHGFI_USEFILEATTRIBUTES,
        );
    }
    let n = info.szTypeName.iter().position(|&c| c == 0).unwrap_or(0);
    String::from_utf16_lossy(&info.szTypeName[..n])
}

const DETACHED: u32 = 0x0000_0008;

/// Opens through explorer.exe so the started program runs with normal
/// (non-admin) rights even though Blitzsuche itself runs elevated.
pub fn open(kind: &str, id: &str, action: &str) {
    let target = if kind == "app" { format!("shell:AppsFolder\\{id}") } else { id.to_string() };
    match action {
        "admin" => unsafe {
            let verb = if kind == "app" { w!("open") } else { w!("runas") };
            ShellExecuteW(HWND::default(), verb, &HSTRING::from(target), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
        },
        "folder" if kind != "app" => {
            let _ = Command::new("explorer.exe")
                .raw_arg(format!("/select,\"{}\"", id))
                .creation_flags(DETACHED)
                .spawn();
        }
        _ => {
            let _ = Command::new("explorer.exe").raw_arg(format!("\"{}\"", target)).creation_flags(DETACHED).spawn();
        }
    }
}

pub fn set_clipboard(text: &str) {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if OpenClipboard(HWND::default()).is_err() {
            return;
        }
        let _ = EmptyClipboard();
        if let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) {
            let p = GlobalLock(h) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                let _ = GlobalUnlock(h);
                let _ = SetClipboardData(CF_UNICODETEXT.0 as u32, HANDLE(h.0));
            }
        }
        let _ = CloseClipboard();
    }
}

/// System accent colour as "#rrggbb".
pub fn accent() -> String {
    let mut v = 0u32;
    let mut size = 4u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\DWM"),
            w!("AccentColor"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut v as *mut _ as *mut c_void),
            Some(&mut size),
        )
    };
    if r.is_err() {
        return "#0078d4".into();
    }
    // stored as 0xAABBGGRR
    format!("#{:02x}{:02x}{:02x}", v & 0xff, (v >> 8) & 0xff, (v >> 16) & 0xff)
}

pub struct Uia {
    uia: IUIAutomation,
    // the search box element, kept so the frequent position check is one cheap call
    cached: Option<IUIAutomationElement>,
}

impl Uia {
    pub fn new() -> Option<Uia> {
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok().map(|uia| Uia { uia, cached: None }) }
    }

    fn find(&self) -> Option<IUIAutomationElement> {
        unsafe {
            let tray = FindWindowW(w!("Shell_TrayWnd"), PCWSTR::null()).ok()?;
            let el = self.uia.ElementFromHandle(tray).ok()?;
            let cond = self
                .uia
                .CreatePropertyCondition(UIA_AutomationIdPropertyId, &VARIANT::from("SearchButton"))
                .ok()?;
            el.FindFirst(TreeScope_Descendants, &cond).ok()
        }
    }

    /// Screen rectangle (physical pixels) of the taskbar search box.
    /// `refind` searches the taskbar again instead of trusting the cached element.
    pub fn search_box(&mut self, refind: bool) -> Option<RECT> {
        let read = |e: &IUIAutomationElement| {
            let r = unsafe { e.CurrentBoundingRectangle() }.ok()?;
            (r.right - r.left >= 8).then_some(r)
        };
        if !refind {
            if let Some(r) = self.cached.as_ref().and_then(read) {
                return Some(r);
            }
        }
        // stale element (Explorer restarted) or periodic refresh
        self.cached = self.find();
        self.cached.as_ref().and_then(read)
    }
}

