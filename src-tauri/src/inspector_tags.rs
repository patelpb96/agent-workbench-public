//! Colour tags the user puts on sessions in the inspector (a view aid, like the window layout): one small
//! strictly validated file in the app data folder, `inspector-tags.json`. Keys are the inspector's opaque
//! source ids (never paths); values come from a fixed palette. Pure logic with no Tauri types.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, io, path::Path};

pub const COLORS: [&str; 6] = ["red", "orange", "yellow", "green", "blue", "purple"];
pub const MAX_TAGS: usize = 256;
const MAX_FILE_BYTES: u64 = 32 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tags {
    pub version: u32,
    /// Source id (16 lowercase hex) -> palette colour.
    pub tags: BTreeMap<String, String>,
}

impl Tags {
    pub fn new() -> Self {
        Tags { version: 1, tags: BTreeMap::new() }
    }
    pub fn is_valid(&self) -> bool {
        self.version == 1
            && self.tags.len() <= MAX_TAGS
            && self.tags.iter().all(|(id, color)| id.len() == 16 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) && COLORS.contains(&color.as_str()))
    }
}

/// Missing, oversized, corrupt or invalid files read as no tags.
pub fn load(path: &Path) -> Tags {
    let read = || -> Option<Tags> {
        if fs::metadata(path).ok()?.len() > MAX_FILE_BYTES {
            return None;
        }
        let tags: Tags = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
        tags.is_valid().then_some(tags)
    };
    read().unwrap_or_else(Tags::new)
}

/// Validated, atomic write.
pub fn save(path: &Path, tags: &Tags) -> io::Result<()> {
    if !tags.is_valid() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid tags"));
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec(tags).map_err(io::Error::other)?)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(items: &[(&str, &str)]) -> Tags {
        Tags { version: 1, tags: items.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() }
    }

    #[test]
    fn only_palette_colours_on_opaque_ids_are_valid() {
        assert!(Tags::new().is_valid());
        assert!(tags(&[("0123456789abcdef", "red"), ("fedcba9876543210", "purple")]).is_valid());
        for bad in [
            tags(&[("0123456789ABCDEF", "red")]),
            tags(&[("0123456789abcde", "red")]),
            tags(&[(r"C:\Users\me\x.jsonl", "red")]),
            tags(&[("0123456789abcdef", "magenta")]),
            tags(&[("0123456789abcdef", "")]),
            Tags { version: 2, ..Tags::new() },
        ] {
            assert!(!bad.is_valid(), "{bad:?}");
        }
        let many = Tags { version: 1, tags: (0..MAX_TAGS + 1).map(|i| (format!("{i:016x}"), "red".to_string())).collect() };
        assert!(!many.is_valid());
    }

    #[test]
    fn the_file_round_trips_and_bad_files_read_as_no_tags() {
        let d = std::env::temp_dir().join(format!("wb-tags-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        let p = d.join("inspector-tags.json");
        assert_eq!(load(&p), Tags::new());
        let t = tags(&[("0123456789abcdef", "green")]);
        save(&p, &t).unwrap();
        assert_eq!(load(&p), t);
        assert!(save(&p, &tags(&[("nope", "red")])).is_err());
        assert_eq!(load(&p), t, "an invalid save leaves the file alone");
        fs::write(&p, br#"{"version":1,"tags":{},"extra":1}"#).unwrap();
        assert_eq!(load(&p), Tags::new(), "unknown field");
        fs::write(&p, vec![b' '; (MAX_FILE_BYTES + 1) as usize]).unwrap();
        assert_eq!(load(&p), Tags::new(), "oversized");
        let _ = fs::remove_dir_all(&d);
    }
}
