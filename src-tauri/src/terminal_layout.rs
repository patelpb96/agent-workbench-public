//! The terminals pane arrangement: a tmux-style tree of splits whose leaves are panes (each
//! showing at most one terminal), plus which pane has focus. Kept in the app data folder so it
//! survives a cleared WebView profile and can be read without the UI. It is VIEW state only: the
//! terminals themselves live in the service, and an id that no longer exists is simply dropped by
//! the UI when it reconciles. Pure logic with no Tauri types; the commands live in `terminals.rs`.
//!
//! Wire format (the UI's `PaneState`): `{version:2, root, focused, nextId}` where a node is
//! `{kind:"leaf", id, session}` or `{kind:"split", id, dir:"row"|"column", ratio, a, b}`.
//! The earlier fixed-layout format (`{layout, colRatio, rowRatio, slots, focused}`) is still
//! accepted on load; the UI converts it.
use serde_json::Value;
use std::{collections::HashSet, fs, io, path::Path};

/// Must match `MAX_LEAVES` in `frontend/src/terminals/panes.ts`.
pub const MAX_LEAVES: usize = 64;
const MAX_FILE_BYTES: u64 = 16 * 1024;
const MIN_RATIO: f64 = 0.15;
const MAX_RATIO: f64 = 0.85;

fn terminal_id(id: &str) -> bool {
    id.len() == 8 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn ratio_ok(v: &Value) -> bool {
    v.as_f64().is_some_and(|r| r.is_finite() && (MIN_RATIO..=MAX_RATIO).contains(&r))
}

struct Seen {
    ids: HashSet<u64>,
    sessions: HashSet<String>,
    leaves: usize,
}

fn node_ok(node: &Value, seen: &mut Seen, depth: usize) -> bool {
    let Some(obj) = node.as_object() else { return false };
    if depth > MAX_LEAVES {
        return false;
    }
    let Some(id) = obj.get("id").and_then(Value::as_u64).filter(|id| *id >= 1) else { return false };
    if !seen.ids.insert(id) {
        return false;
    }
    match obj.get("kind").and_then(Value::as_str) {
        Some("leaf") => {
            if obj.len() != 3 {
                return false;
            }
            seen.leaves += 1;
            if seen.leaves > MAX_LEAVES {
                return false;
            }
            match obj.get("session") {
                Some(Value::Null) => true,
                Some(Value::String(s)) => terminal_id(s) && seen.sessions.insert(s.clone()),
                _ => false,
            }
        }
        Some("split") => {
            obj.len() == 6
                && matches!(obj.get("dir").and_then(Value::as_str), Some("row" | "column"))
                && obj.get("ratio").is_some_and(ratio_ok)
                && obj.get("a").is_some_and(|a| node_ok(a, seen, depth + 1))
                && obj.get("b").is_some_and(|b| node_ok(b, seen, depth + 1))
        }
        _ => false,
    }
}

fn leaf_ids(node: &Value, out: &mut Vec<u64>) {
    match node.get("kind").and_then(Value::as_str) {
        Some("leaf") => out.extend(node.get("id").and_then(Value::as_u64)),
        _ => {
            for side in ["a", "b"] {
                if let Some(child) = node.get(side) {
                    leaf_ids(child, out);
                }
            }
        }
    }
}

/// A valid current-format arrangement: strict shape, no unknown fields, at most `MAX_LEAVES`
/// panes, distinct node ids, each terminal shown at most once, focus on an existing pane.
pub fn is_valid(value: &Value) -> bool {
    let Some(obj) = value.as_object() else { return false };
    if obj.len() != 4 || obj.get("version").and_then(Value::as_u64) != Some(2) {
        return false;
    }
    let (Some(root), Some(focused), Some(next)) = (
        obj.get("root"),
        obj.get("focused").and_then(Value::as_u64),
        obj.get("nextId").and_then(Value::as_u64),
    ) else {
        return false;
    };
    let mut seen = Seen { ids: HashSet::new(), sessions: HashSet::new(), leaves: 0 };
    if !node_ok(root, &mut seen, 0) {
        return false;
    }
    let mut leaves = Vec::new();
    leaf_ids(root, &mut leaves);
    leaves.contains(&focused) && seen.ids.iter().all(|id| *id < next)
}

/// The earlier fixed-layout format; only ever read, never written.
fn legacy_ok(value: &Value) -> bool {
    let Some(obj) = value.as_object() else { return false };
    let layout = obj.get("layout").and_then(Value::as_str);
    let visible = match layout {
        Some("single") => 1,
        Some("columns" | "rows") => 2,
        Some("grid") => 4,
        _ => return false,
    };
    let slots = obj.get("slots").and_then(Value::as_array);
    obj.len() == 5
        && obj.get("colRatio").is_some_and(ratio_ok)
        && obj.get("rowRatio").is_some_and(ratio_ok)
        && obj.get("focused").and_then(Value::as_u64).is_some_and(|f| (f as usize) < visible)
        && slots.is_some_and(|s| {
            let ids: Vec<&str> = s.iter().filter_map(Value::as_str).collect();
            s.len() == 4
                && s.iter().all(|x| x.is_null() || x.as_str().is_some_and(terminal_id))
                && ids.iter().enumerate().all(|(i, id)| !ids[..i].contains(id))
        })
}

/// Missing, oversized, corrupt or invalid files all read as "nothing saved". The legacy format is
/// returned as is (the UI converts it).
pub fn load(path: &Path) -> Option<Value> {
    let meta = fs::metadata(path).ok()?;
    if meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let value: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    (is_valid(&value) || legacy_ok(&value)).then_some(value)
}

/// Validated, atomic write (temp file then rename). Only the current format is written.
pub fn save(path: &Path, panes: &Value) -> io::Result<()> {
    if !is_valid(panes) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid terminal layout"));
    }
    let bytes = serde_json::to_vec(panes).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "terminal layout too large"));
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn leaf(id: u64, session: Option<&str>) -> Value {
        json!({"kind":"leaf","id":id,"session":session})
    }
    fn split(id: u64, dir: &str, ratio: f64, a: Value, b: Value) -> Value {
        json!({"kind":"split","id":id,"dir":dir,"ratio":ratio,"a":a,"b":b})
    }
    fn good() -> Value {
        json!({"version":2,"focused":3,"nextId":6,"root": split(1,"row",0.3,
            leaf(2,Some("0a1b2c3d")), split(4,"column",0.5, leaf(3,Some("deadbeef")), leaf(5,None)))})
    }
    fn dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("wb-termlayout-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d
    }
    /// A chain of `n` leaves: the worst case for depth.
    fn chain(n: u64) -> Value {
        let mut node = leaf(1, None);
        for i in 1..n {
            node = split(2 * i, "row", 0.5, node, leaf(2 * i + 1, None));
        }
        json!({"version":2,"focused":1,"nextId":2 * n + 2,"root":node})
    }

    #[test]
    fn round_trips_and_stays_small() {
        let d = dir();
        let p = d.join("terminal-layout.json");
        save(&p, &good()).unwrap();
        assert_eq!(load(&p), Some(good()));
        assert!(!p.with_extension("tmp").exists(), "temp file is renamed away");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn sixty_four_panes_fit_and_sixty_five_do_not() {
        assert!(is_valid(&chain(MAX_LEAVES as u64)));
        assert!(!is_valid(&chain(MAX_LEAVES as u64 + 1)));
        let d = dir();
        let p = d.join("terminal-layout.json");
        save(&p, &chain(MAX_LEAVES as u64)).unwrap();
        assert!(load(&p).is_some(), "the largest arrangement stays under the file cap");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn invalid_arrangements_are_refused_on_save_and_ignored_on_load() {
        let d = dir();
        let p = d.join("terminal-layout.json");
        let mut cases: Vec<(&str, Value)> = Vec::new();
        let mut v = good();
        v["root"]["ratio"] = json!(0.01);
        cases.push(("ratio too small", v));
        let mut v = good();
        v["root"]["dir"] = json!("diagonal");
        cases.push(("bad direction", v));
        let mut v = good();
        v["root"]["a"]["session"] = json!("ABCDEF12");
        cases.push(("bad id", v));
        let mut v = good();
        v["root"]["b"]["a"]["session"] = json!("0a1b2c3d");
        cases.push(("session shown twice", v));
        let mut v = good();
        v["root"]["b"]["id"] = json!(2);
        cases.push(("duplicate node id", v));
        let mut v = good();
        v["focused"] = json!(99);
        cases.push(("focus on nothing", v));
        let mut v = good();
        v["focused"] = json!(1);
        cases.push(("focus on a split, not a pane", v));
        let mut v = good();
        v["nextId"] = json!(5);
        cases.push(("next id not above every id", v));
        let mut v = good();
        v["extra"] = json!(1);
        cases.push(("unknown field", v));
        let mut v = good();
        v["version"] = json!(3);
        cases.push(("unknown version", v));
        let mut v = good();
        v["root"]["a"]["extra"] = json!(true);
        cases.push(("unknown node field", v));
        cases.push(("not an object", json!([1, 2])));
        for (name, bad) in cases {
            assert!(!is_valid(&bad), "{name}");
            assert!(save(&p, &bad).is_err(), "{name}");
            assert!(!p.exists(), "{name}: nothing written");
            fs::write(&p, serde_json::to_vec(&bad).unwrap()).unwrap();
            assert_eq!(load(&p), None, "{name}: a hand-written bad file is ignored");
            fs::remove_file(&p).unwrap();
        }
        fs::write(&p, b"not json").unwrap();
        assert_eq!(load(&p), None);
        fs::write(&p, vec![b' '; (MAX_FILE_BYTES + 1) as usize]).unwrap();
        assert_eq!(load(&p), None, "oversized");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn the_earlier_fixed_layout_file_is_still_read_but_never_written() {
        let d = dir();
        let p = d.join("terminal-layout.json");
        let old = json!({"layout":"columns","colRatio":0.3,"rowRatio":0.5,"slots":["0a1b2c3d","deadbeef",null,null],"focused":1});
        fs::write(&p, serde_json::to_vec(&old).unwrap()).unwrap();
        assert_eq!(load(&p), Some(old.clone()));
        assert!(save(&p, &old).is_err(), "only the current format is written");
        for bad in [
            json!({"layout":"columns","colRatio":0.3,"rowRatio":0.5,"slots":["0a1b2c3d","0a1b2c3d",null,null],"focused":1}),
            json!({"layout":"columns","colRatio":0.3,"rowRatio":0.5,"slots":[null,null,null,null],"focused":2}),
            json!({"layout":"columns","colRatio":0.3,"rowRatio":0.5,"slots":[null],"focused":0}),
        ] {
            fs::write(&p, serde_json::to_vec(&bad).unwrap()).unwrap();
            assert_eq!(load(&p), None, "{bad}");
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_missing_file_is_nothing_saved() {
        assert_eq!(load(&dir().join("none.json")), None);
    }
}
