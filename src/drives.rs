//! Reads every drive into the index.
//!
//! NTFS drives: the whole file table is enumerated via FSCTL_ENUM_USN_DATA
//! (needs admin, takes ~1-2 s per million files), then a thread follows the
//! USN change journal so new, renamed and deleted files show up live.
//! Other drives (FAT32, exFAT, Google Drive, ...): plain directory walk.

use crate::index::{F_DIR, F_HID, Index, NONE};
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetDriveTypeW, GetFileInformationByHandle,
    GetLogicalDrives, GetVolumeInformationW, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    CREATE_USN_JOURNAL_DATA, FSCTL_CREATE_USN_JOURNAL, FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL,
    FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V0, READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0,
};
use windows::core::HSTRING;

pub type Shared = Arc<RwLock<Index>>;

/// Number of drives still being read in.
pub static PENDING: AtomicUsize = AtomicUsize::new(0);

const MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;

#[derive(Clone, Copy)]
struct H(isize);
impl H {
    fn h(self) -> HANDLE {
        HANDLE(self.0 as *mut c_void)
    }
}

pub fn start(index: Shared) {
    let mask = unsafe { GetLogicalDrives() };
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let letter = (b'A' + i as u8) as char;
        let root = format!("{letter}:\\");
        let ty = unsafe { GetDriveTypeW(&HSTRING::from(root.as_str())) };
        if ty != DRIVE_FIXED && ty != DRIVE_REMOVABLE {
            continue;
        }
        let fs = fs_name(&root);
        let index = index.clone();
        PENDING.fetch_add(1, Ordering::SeqCst);
        std::thread::spawn(move || {
            let t = std::time::Instant::now();
            let ok = fs == "NTFS" && index_ntfs(letter, &index).is_ok();
            if !ok {
                walk(letter, &index);
            }
            crate::recent::log(&format!("Laufwerk {letter}: ({fs}) {} in {} ms", if ok { "MFT" } else { "Ordnerscan" }, t.elapsed().as_millis()));
            PENDING.fetch_sub(1, Ordering::SeqCst);
        });
    }
}

fn fs_name(root: &str) -> String {
    let mut buf = [0u16; 64];
    let ok = unsafe {
        GetVolumeInformationW(&HSTRING::from(root), None, None, None, None, Some(&mut buf))
    };
    if ok.is_err() {
        return String::new();
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

fn flags_of(attr: u32) -> u8 {
    let mut f = 0;
    if attr & 0x10 != 0 {
        f |= F_DIR;
    }
    if attr & 0x2 != 0 || attr & 0x4 != 0 {
        f |= F_HID;
    }
    f
}

fn open_volume(letter: char) -> windows::core::Result<H> {
    let h = unsafe {
        CreateFileW(
            &HSTRING::from(format!("\\\\.\\{letter}:")),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            HANDLE::default(),
        )?
    };
    Ok(H(h.0 as isize))
}

fn root_frn(letter: char) -> windows::core::Result<u64> {
    unsafe {
        let h = CreateFileW(
            &HSTRING::from(format!("{letter}:\\")),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            HANDLE::default(),
        )?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        let r = GetFileInformationByHandle(h, &mut info);
        let _ = CloseHandle(h);
        r?;
        Ok(((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64)
    }
}

fn query_journal(vol: H) -> windows::core::Result<USN_JOURNAL_DATA_V0> {
    let mut jd = USN_JOURNAL_DATA_V0::default();
    let mut ret = 0u32;
    let q = unsafe {
        DeviceIoControl(
            vol.h(),
            FSCTL_QUERY_USN_JOURNAL,
            None,
            0,
            Some(&mut jd as *mut _ as *mut c_void),
            size_of::<USN_JOURNAL_DATA_V0>() as u32,
            Some(&mut ret),
            None,
        )
    };
    if q.is_ok() {
        return Ok(jd);
    }
    let create = CREATE_USN_JOURNAL_DATA { MaximumSize: 32 << 20, AllocationDelta: 8 << 20 };
    unsafe {
        DeviceIoControl(
            vol.h(),
            FSCTL_CREATE_USN_JOURNAL,
            Some(&create as *const _ as *const c_void),
            size_of::<CREATE_USN_JOURNAL_DATA>() as u32,
            None,
            0,
            Some(&mut ret),
            None,
        )?;
        DeviceIoControl(
            vol.h(),
            FSCTL_QUERY_USN_JOURNAL,
            None,
            0,
            Some(&mut jd as *mut _ as *mut c_void),
            size_of::<USN_JOURNAL_DATA_V0>() as u32,
            Some(&mut ret),
            None,
        )?;
    }
    Ok(jd)
}

struct Rec {
    frn: u64,
    parent: u64,
    reason: u32,
    attr: u32,
    name: String,
}

fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn rd_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Parses USN_RECORD_V2 entries starting at byte 8 of the buffer.
fn parse(buf: &[u8], mut f: impl FnMut(Rec)) {
    let mut off = 8;
    while off + 60 <= buf.len() {
        let len = rd_u32(buf, off) as usize;
        if len == 0 || off + len > buf.len() {
            break;
        }
        if rd_u16(buf, off + 4) == 2 {
            let nlen = rd_u16(buf, off + 56) as usize;
            let noff = rd_u16(buf, off + 58) as usize;
            let start = off + noff;
            if start + nlen <= buf.len() {
                let u: Vec<u16> = buf[start..start + nlen]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                f(Rec {
                    frn: rd_u64(buf, off + 8) & MASK,
                    parent: rd_u64(buf, off + 16) & MASK,
                    reason: rd_u32(buf, off + 40),
                    attr: rd_u32(buf, off + 52),
                    name: String::from_utf16_lossy(&u),
                });
            }
        }
        off += len;
    }
}

fn index_ntfs(letter: char, index: &Shared) -> windows::core::Result<()> {
    let vol = open_volume(letter)?;
    let jd = query_journal(vol)?;
    let root = root_frn(letter)? & MASK;

    let mut recs: Vec<Rec> = Vec::with_capacity(1 << 20);
    let mut med = MFT_ENUM_DATA_V0 { StartFileReferenceNumber: 0, LowUsn: 0, HighUsn: jd.NextUsn };
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let mut ret = 0u32;
        let r = unsafe {
            DeviceIoControl(
                vol.h(),
                FSCTL_ENUM_USN_DATA,
                Some(&med as *const _ as *const c_void),
                size_of::<MFT_ENUM_DATA_V0>() as u32,
                Some(buf.as_mut_ptr() as *mut c_void),
                buf.len() as u32,
                Some(&mut ret),
                None,
            )
        };
        if r.is_err() || ret <= 8 {
            break;
        }
        let data = &buf[..ret as usize];
        med.StartFileReferenceNumber = rd_u64(data, 0);
        parse(data, |r| {
            if r.frn != root {
                recs.push(r)
            }
        });
    }

    // Insert in chunks so searches (and the UI) never wait long on the lock.
    const CHUNK: usize = 50_000;
    let mut map: HashMap<u64, u32> = HashMap::with_capacity(recs.len() + 1);
    let root_idx = index.write().unwrap().add(&format!("{letter}:"), NONE, F_DIR);
    map.insert(root, root_idx);
    let mut ids: Vec<u32> = Vec::with_capacity(recs.len());
    for chunk in recs.chunks(CHUNK) {
        let mut ix = index.write().unwrap();
        for r in chunk {
            let i = ix.add(&r.name, root_idx, flags_of(r.attr));
            map.insert(r.frn, i);
            ids.push(i);
        }
    }
    for (rc, ic) in recs.chunks(CHUNK).zip(ids.chunks(CHUNK)) {
        let mut ix = index.write().unwrap();
        for (r, &i) in rc.iter().zip(ic) {
            let p = map.get(&r.parent).copied().unwrap_or(root_idx);
            ix.set_parent(i, p);
        }
    }
    drop(recs);
    drop(ids);

    let index = index.clone();
    std::thread::spawn(move || follow_journal(vol, jd, map, root_idx, index));
    Ok(())
}

fn follow_journal(vol: H, mut jd: USN_JOURNAL_DATA_V0, mut map: HashMap<u64, u32>, root: u32, index: Shared) {
    let mut usn = jd.NextUsn;
    let mut buf = vec![0u8; 256 << 10];
    loop {
        let rd = READ_USN_JOURNAL_DATA_V0 {
            StartUsn: usn,
            ReasonMask: 0xFFFF_FFFF,
            ReturnOnlyOnClose: 0,
            Timeout: 0,
            BytesToWaitFor: 1,
            UsnJournalID: jd.UsnJournalID,
        };
        let mut ret = 0u32;
        let r = unsafe {
            DeviceIoControl(
                vol.h(),
                FSCTL_READ_USN_JOURNAL,
                Some(&rd as *const _ as *const c_void),
                size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
                Some(buf.as_mut_ptr() as *mut c_void),
                buf.len() as u32,
                Some(&mut ret),
                None,
            )
        };
        if r.is_err() {
            std::thread::sleep(Duration::from_secs(2));
            if let Ok(j) = query_journal(vol) {
                jd = j;
                usn = j.NextUsn;
            }
            continue;
        }
        if ret < 8 {
            continue;
        }
        let data = &buf[..ret as usize];
        usn = rd_u64(data, 0) as i64;
        if ret == 8 {
            continue;
        }
        let mut ix = index.write().unwrap();
        parse(data, |r| {
            const CREATE: u32 = 0x100;
            const DELETE: u32 = 0x200;
            const OLD_NAME: u32 = 0x1000;
            const NEW_NAME: u32 = 0x2000;
            if r.reason & DELETE != 0 {
                if let Some(i) = map.remove(&r.frn) {
                    ix.delete(i);
                }
                return;
            }
            if r.reason & OLD_NAME != 0 && r.reason & NEW_NAME == 0 {
                return;
            }
            if r.reason & (CREATE | NEW_NAME) != 0 {
                let p = map.get(&r.parent).copied().unwrap_or(root);
                let fl = flags_of(r.attr);
                match map.get(&r.frn) {
                    Some(&i) => ix.update(i, &r.name, p, fl),
                    None => {
                        let i = ix.add(&r.name, p, fl);
                        map.insert(r.frn, i);
                    }
                }
            }
        });
    }
}

fn walk(letter: char, index: &Shared) {
    let root = index.write().unwrap().add(&format!("{letter}:"), NONE, F_DIR);
    let mut stack = vec![(PathBuf::from(format!("{letter}:\\")), root)];
    while let Some((dir, di)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let items: Vec<(String, bool)> = rd
            .filter_map(|e| e.ok())
            .map(|e| {
                let d = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                (e.file_name().to_string_lossy().into_owned(), d)
            })
            .collect();
        let mut ix = index.write().unwrap();
        for (n, d) in items {
            let i = ix.add(&n, di, if d { F_DIR } else { 0 });
            if d {
                stack.push((dir.join(&n), i));
            }
        }
    }
}
