//! In-memory file name index for all drives.
//!
//! All names live in two big buffers (original + lowercase). Search runs a
//! SIMD substring scan (memchr::memmem) over the lowercase buffer, so a query
//! over millions of names takes a few milliseconds.

use memchr::memmem;

pub const NONE: u32 = u32::MAX;
pub const F_DIR: u8 = 1;
pub const F_DEL: u8 = 2;
pub const F_HID: u8 = 4;

#[derive(Clone, Copy)]
struct Entry {
    o_off: u32,
    l_off: u32,
    o_len: u16,
    l_len: u16,
    parent: u32,
    flags: u8,
}

#[derive(Default)]
pub struct Index {
    entries: Vec<Entry>,
    orig: String,
    lower: Vec<u8>,
    /// (offset into `lower`, entry index), sorted by offset. A renamed entry
    /// gets a new segment; stale ones are skipped because the entry's
    /// `l_off` no longer points at them.
    seg: Vec<(u32, u32)>,
    live: usize,
}

pub struct Hit {
    pub idx: u32,
    pub score: i32,
}

/// Folder names whose contents are almost never what a person searches for.
const NOISE_HEAVY: &[&[u8]] = &[
    b"$recycle.bin",
    b"winsxs",
    b"system volume information",
    b"node_modules",
    b".git",
    b"__pycache__",
    b"site-packages",
    b".cargo",
    b".rustup",
    b"servicing",
    b"installer",
    b"$windows.~ws",
    b"$windows.~bt",
    b"temp",
    b"cache",
    b"caches",
    b"code cache",
    b"gpucache",
    b"windowsapps",
    b"driverstore",
    b".gradle",
    b".npm",
    b"target",
    b"recent",
    b"jumplists",
    b"automaticdestinations",
    b"customdestinations",
];

impl Index {
    pub fn add(&mut self, name: &str, parent: u32, flags: u8) -> u32 {
        let idx = self.entries.len() as u32;
        let o_off = self.orig.len() as u32;
        self.orig.push_str(name);
        let lname = name.to_lowercase();
        let l_off = self.lower.len() as u32;
        self.lower.extend_from_slice(lname.as_bytes());
        self.lower.push(0);
        self.seg.push((l_off, idx));
        self.entries.push(Entry {
            o_off,
            l_off,
            o_len: name.len() as u16,
            l_len: lname.len() as u16,
            parent,
            flags,
        });
        self.live += 1;
        idx
    }

    pub fn set_parent(&mut self, idx: u32, parent: u32) {
        self.entries[idx as usize].parent = parent;
    }

    pub fn live(&self) -> usize {
        self.live
    }

    pub fn delete(&mut self, idx: u32) {
        let e = &mut self.entries[idx as usize];
        if e.flags & F_DEL == 0 {
            e.flags |= F_DEL;
            self.live -= 1;
        }
    }

    /// Rename and/or move an entry (used by the live NTFS journal).
    pub fn update(&mut self, idx: u32, name: &str, parent: u32, flags: u8) {
        if self.name(idx) != name {
            let o_off = self.orig.len() as u32;
            self.orig.push_str(name);
            let lname = name.to_lowercase();
            let l_off = self.lower.len() as u32;
            self.lower.extend_from_slice(lname.as_bytes());
            self.lower.push(0);
            self.seg.push((l_off, idx));
            let e = &mut self.entries[idx as usize];
            e.o_off = o_off;
            e.o_len = name.len() as u16;
            e.l_off = l_off;
            e.l_len = lname.len() as u16;
        }
        let e = &mut self.entries[idx as usize];
        e.parent = parent;
        if e.flags & F_DEL != 0 {
            self.live += 1;
        }
        e.flags = flags;
    }

    pub fn name(&self, i: u32) -> &str {
        let e = &self.entries[i as usize];
        &self.orig[e.o_off as usize..e.o_off as usize + e.o_len as usize]
    }

    fn lname(&self, i: u32) -> &[u8] {
        let e = &self.entries[i as usize];
        &self.lower[e.l_off as usize..e.l_off as usize + e.l_len as usize]
    }

    pub fn is_dir(&self, i: u32) -> bool {
        self.entries[i as usize].flags & F_DIR != 0
    }

    pub fn path(&self, i: u32) -> String {
        let mut parts = Vec::new();
        let mut c = i;
        while c != NONE && parts.len() < 256 {
            parts.push(self.name(c));
            c = self.entries[c as usize].parent;
        }
        parts.reverse();
        let mut s = parts.join("\\");
        if parts.len() == 1 {
            s.push('\\');
        }
        s
    }

    fn path_penalty(&self, i: u32) -> i32 {
        let mut pen = 0;
        let mut depth = 0;
        let mut c = self.entries[i as usize].parent;
        while c != NONE && depth < 256 {
            let e = &self.entries[c as usize];
            let n = self.lname(c);
            if NOISE_HEAVY.contains(&n) {
                pen = pen.min(-450);
            } else if n == b"appdata" || n == b"programdata" {
                pen = pen.min(-140);
            } else if n == b"windows" && e.parent != NONE && self.entries[e.parent as usize].parent == NONE {
                pen = pen.min(-320);
            }
            if e.flags & F_HID != 0 && e.parent != NONE {
                pen = pen.min(-200);
            }
            depth += 1;
            c = e.parent;
        }
        pen - depth * 6
    }

    pub fn search(&self, tokens: &[String], full: &str, limit: usize) -> Vec<Hit> {
        let Some(key) = tokens.iter().max_by_key(|t| t.len()) else {
            return Vec::new();
        };
        let finder = memmem::Finder::new(key.as_bytes());
        let others: Vec<memmem::Finder> = tokens
            .iter()
            .filter(|t| *t != key)
            .map(|t| memmem::Finder::new(t.as_bytes()))
            .collect();
        let mut hits = Vec::new();
        let mut last = NONE;
        for pos in finder.find_iter(&self.lower) {
            let si = match self.seg.binary_search_by(|s| s.0.cmp(&(pos as u32))) {
                Ok(i) => i,
                Err(i) => i - 1,
            };
            let (off, idx) = self.seg[si];
            if idx == last {
                continue;
            }
            let e = self.entries[idx as usize];
            if e.l_off != off || e.flags & F_DEL != 0 {
                continue;
            }
            last = idx;
            let name = &self.lower[off as usize..off as usize + e.l_len as usize];
            if !others.iter().all(|f| f.find(name).is_some()) {
                continue;
            }
            let s = score_name(name, full, tokens, pos - off as usize, e.flags);
            hits.push(Hit { idx, score: s });
        }
        // Stage 1: cheap name score. Stage 2: path penalty for the best few.
        let pre = (limit * 8).max(400);
        if hits.len() > pre {
            hits.select_nth_unstable_by(pre, |a, b| b.score.cmp(&a.score));
            hits.truncate(pre);
        }
        for h in hits.iter_mut() {
            h.score += self.path_penalty(h.idx);
        }
        hits.sort_unstable_by(|a, b| b.score.cmp(&a.score));
        hits.truncate(limit);
        hits
    }
}

fn is_sep(b: u8) -> bool {
    matches!(b, b' ' | b'-' | b'_' | b'.' | b'(' | b'[' | b'+' | b',')
}

pub fn score_name(name: &[u8], full: &str, tokens: &[String], pos: usize, flags: u8) -> i32 {
    let q = full.as_bytes();
    let stem_end = name
        .iter()
        .rposition(|&b| b == b'.')
        .filter(|&p| p > 0 && flags & F_DIR == 0)
        .unwrap_or(name.len());
    let stem = &name[..stem_end];
    let mut s = if name == q {
        1000
    } else if stem == q {
        950
    } else if name.starts_with(q) {
        700
    } else if tokens.len() > 1 && name.starts_with(tokens[0].as_bytes()) {
        500
    } else if pos > 0 && pos <= name.len() && is_sep(name[pos - 1]) {
        400
    } else {
        150
    };
    s -= (name.len() as i32).min(120);
    let ext = &name[stem_end..];
    if matches!(
        ext,
        b".exe" | b".lnk" | b".bat" | b".cmd" | b".ps1" | b".msi" | b".url" | b".appref-ms"
    ) {
        s += 120;
    }
    if flags & F_DIR != 0 {
        s += 30;
    }
    if flags & F_HID != 0 {
        s -= 250;
    }
    s
}
