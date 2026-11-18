//! "Aktuelle Suchen": what was opened, newest first, saved as JSON.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone)]
pub struct Item {
    pub k: String,
    pub id: String,
    pub name: String,
    pub sub: String,
    #[serde(default)]
    pub loc: String,
    pub t: u64,
}

pub struct Recent {
    file: PathBuf,
    pub items: Vec<Item>,
}

pub fn data_dir() -> PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
    let d = PathBuf::from(base).join("Blitzsuche");
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Recent {
    pub fn load() -> Recent {
        let file = data_dir().join("recent.json");
        let items = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Recent { file, items }
    }

    pub fn push(&mut self, mut it: Item) {
        it.t = now();
        self.items.retain(|x| x.id != it.id);
        self.items.insert(0, it);
        self.items.truncate(40);
        self.save();
    }

    pub fn remove(&mut self, id: &str) {
        self.items.retain(|x| x.id != id);
        self.save();
    }

    fn save(&self) {
        if let Ok(s) = serde_json::to_vec(&self.items) {
            let _ = std::fs::write(&self.file, s);
        }
    }

    /// Entries whose file still exists.
    pub fn visible(&self, n: usize) -> Vec<Item> {
        self.items
            .iter()
            .filter(|i| i.k == "app" || std::path::Path::new(&i.id).exists())
            .take(n)
            .cloned()
            .collect()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.items.iter().any(|i| i.id == id)
    }
}

/// Small debug log in %LOCALAPPDATA%\Blitzsuche\log.txt (kept below 256 KB).
pub fn log(msg: &str) {
    use std::io::Write;
    let p = data_dir().join("log.txt");
    if std::fs::metadata(&p).map(|m| m.len() > 256 << 10).unwrap_or(false) {
        let _ = std::fs::remove_file(&p);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let _ = writeln!(f, "{t} {msg}");
    }
}
