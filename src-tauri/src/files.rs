//! The Files panel (docs/design/sidebar-restructure.md): one folder's entries, read-only. Nothing here writes, renames,
//! deletes, opens or runs a file, and links are reported as links without being followed. Pure logic with no Tauri types;
//! the commands are in `props.rs`.
use serde::Serialize;
use std::{fs, path::Path, time::UNIX_EPOCH};

/// Entries returned for one folder; the rest are counted, not listed.
pub const MAX_ENTRIES: usize = 2000;
/// Folders larger than this are not read to the end (the count of the rest is then a lower bound).
const MAX_SCANNED: usize = 50_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Folder,
    File,
    Link,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    /// Bytes, for files; 0 for folders and links.
    pub size: u64,
    /// Milliseconds since 1970, or 0 when unknown.
    pub modified_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Listing {
    /// The folder as listed (the path that was asked for, trimmed).
    pub path: String,
    pub entries: Vec<Entry>,
    /// Entries that exist but were not returned (over the cap, or past the scan limit).
    pub more: usize,
    /// Hidden or system entries that were left out.
    pub hidden: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListError {
    /// Not an absolute drive path of the allowed shape.
    Invalid,
    /// It does not exist, is not a folder, or cannot be read.
    Unreadable,
}

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Folders first, then files and links, each by name ignoring case.
fn order(a: &Entry, b: &Entry) -> std::cmp::Ordering {
    let rank = |e: &Entry| u8::from(e.kind != Kind::Folder);
    rank(a).cmp(&rank(b)).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())).then_with(|| a.name.cmp(&b.name))
}

/// List one folder. `path` must be an absolute drive path (the same rule as a terminal start folder).
pub fn list(path: &str, show_hidden: bool) -> Result<Listing, ListError> {
    use std::os::windows::fs::MetadataExt;
    let path = path.trim();
    if !wb_session::proto::valid_start_dir(path) {
        return Err(ListError::Invalid);
    }
    let dir = Path::new(path);
    if !fs::metadata(dir).map(|m| m.is_dir()).unwrap_or(false) {
        return Err(ListError::Unreadable);
    }
    let reader = fs::read_dir(dir).map_err(|_| ListError::Unreadable)?;
    let mut entries = Vec::new();
    let mut hidden = 0usize;
    let mut scanned = 0usize;
    let mut truncated = false;
    for item in reader {
        scanned += 1;
        if scanned > MAX_SCANNED {
            truncated = true;
            break;
        }
        let Ok(item) = item else { continue };
        // `DirEntry::metadata` does not follow links, so a link is reported as itself.
        let Ok(meta) = item.metadata() else { continue };
        let attributes = meta.file_attributes();
        if !show_hidden && attributes & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0 {
            hidden += 1;
            continue;
        }
        let Some(name) = item.file_name().to_str().map(str::to_owned) else { continue };
        let kind = if meta.file_type().is_symlink() || attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            Kind::Link
        } else if meta.is_dir() {
            Kind::Folder
        } else {
            Kind::File
        };
        let modified_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        entries.push(Entry { name, size: if kind == Kind::File { meta.len() } else { 0 }, kind, modified_ms });
    }
    entries.sort_by(order);
    let mut more = entries.len().saturating_sub(MAX_ENTRIES);
    entries.truncate(MAX_ENTRIES);
    if truncated {
        more += 1;
    }
    Ok(Listing { path: path.to_owned(), entries, more, hidden })
}

/// A path the panel may hand to Explorer or a terminal: an absolute drive path that exists. Returns whether it is a file.
pub fn existing(path: &str) -> Option<bool> {
    let path = path.trim();
    if !wb_session::proto::valid_start_dir(path) {
        return None;
    }
    fs::symlink_metadata(path).ok().map(|m| !m.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("wb-files-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn folders_come_first_and_names_sort_ignoring_case() {
        let d = dir();
        fs::write(d.join("b.txt"), b"hello").unwrap();
        fs::write(d.join("A.md"), b"").unwrap();
        fs::create_dir(d.join("zeta")).unwrap();
        fs::create_dir(d.join("Alpha")).unwrap();
        let l = list(&d.display().to_string(), false).unwrap();
        let names: Vec<_> = l.entries.iter().map(|e| (e.name.as_str(), e.kind)).collect();
        assert_eq!(names, [("Alpha", Kind::Folder), ("zeta", Kind::Folder), ("A.md", Kind::File), ("b.txt", Kind::File)]);
        assert_eq!(l.entries[3].size, 5);
        assert!(l.entries[3].modified_ms > 0);
        assert_eq!((l.more, l.hidden), (0, 0));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn only_absolute_existing_folders_are_listed() {
        for bad in ["", "relative", r"\\server\share", r"C:\x\..\y", "C:"] {
            assert_eq!(list(bad, false), Err(ListError::Invalid), "{bad:?}");
        }
        assert_eq!(list(r"Z:\definitely\not\here", false), Err(ListError::Unreadable));
        let d = dir();
        let file = d.join("f.txt");
        fs::write(&file, b"").unwrap();
        assert_eq!(list(&file.display().to_string(), false), Err(ListError::Unreadable), "a file is not a folder");
        assert_eq!(existing(&file.display().to_string()), Some(true));
        assert_eq!(existing(&d.display().to_string()), Some(false));
        assert_eq!(existing("relative"), None);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn the_listing_is_capped_and_counts_the_rest() {
        let d = dir();
        for i in 0..(MAX_ENTRIES + 5) {
            fs::write(d.join(format!("f{i:05}")), b"").unwrap();
        }
        let l = list(&d.display().to_string(), false).unwrap();
        assert_eq!((l.entries.len(), l.more), (MAX_ENTRIES, 5));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn hidden_entries_are_left_out_unless_asked_for() {
        let d = dir();
        let secret = d.join("secret");
        fs::write(&secret, b"").unwrap();
        fs::write(d.join("plain"), b"").unwrap();
        let status = std::process::Command::new("attrib").arg("+h").arg(&secret).status().unwrap();
        assert!(status.success());
        let l = list(&d.display().to_string(), false).unwrap();
        assert_eq!(l.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["plain"]);
        assert_eq!(l.hidden, 1);
        assert_eq!(list(&d.display().to_string(), true).unwrap().entries.len(), 2);
        let _ = fs::remove_dir_all(&d);
    }
}
