//! Free-form terminals on the Agents canvas: floating windows (position, size, stacking) and links
//! between a session and a terminal (a session panel can flip between its transcript and that
//! terminal). VIEW state only, kept in the app data folder; the terminals live in the service and the
//! sessions in the journal. Same wire format as `frontend/src/terminals/floating.ts`:
//! `{version:2, windows:[{terminal,x,y,w,h,z[,stage]}], watches:[{source,agent,label,x,y,w,h,z}],
//! links:{<session uuid>:{terminal,flipped}}, active}`. Version 1 (before watch windows) is still read
//! and has no `watches`; the app only writes version 2. A minimized window carries `stage` ("compact", a
//! bar, or "block"); a full one leaves it out.
//! Pure logic with no Tauri types; the commands live in `terminals.rs`.
use serde_json::Value;
use std::{collections::HashSet, fs, io, path::Path};

/// Must match the limits in `floating.ts`.
pub const MAX_WINDOWS: usize = 32;
pub const MAX_LINKS: usize = 16;
pub const MAX_WATCHES: usize = 16;
const MAX_COORD: f64 = 100_000.0;
const MAX_FILE_BYTES: u64 = 16 * 1024;

fn terminal_id(id: &str) -> bool {
    id.len() == 8 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A lowercase UUID v4 (8-4-4-4-12, version 4, variant 8..b), the form session ids have.
fn session_id(id: &str) -> bool {
    let parts: Vec<&str> = id.split('-').collect();
    let lens = [8, 4, 4, 4, 12];
    parts.len() == 5
        && parts.iter().zip(lens).all(|(p, n)| p.len() == n && p.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        && parts[2].starts_with('4')
        && matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b')
}

/// An opaque inspector source id: 16 lowercase hex.
fn source_id(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn number_ok(v: Option<&Value>, min: f64) -> bool {
    v.and_then(Value::as_f64).is_some_and(|n| n.is_finite() && n.abs() <= MAX_COORD && n >= min)
}

/// A valid arrangement: exact shape (no unknown fields), bounded counts and numbers, each terminal in at
/// most one window and one link, and a terminal never both floating and flipped into a panel.
pub fn is_valid(value: &Value) -> bool {
    let Some(obj) = value.as_object() else { return false };
    let version = obj.get("version").and_then(Value::as_u64);
    let expected_keys = match version {
        Some(1) => 4,
        Some(2) => 5,
        _ => return false,
    };
    if obj.len() != expected_keys {
        return false;
    }
    let (Some(windows), Some(links), Some(active)) = (
        obj.get("windows").and_then(Value::as_array),
        obj.get("links").and_then(Value::as_object),
        obj.get("active"),
    ) else {
        return false;
    };
    if windows.len() > MAX_WINDOWS || links.len() > MAX_LINKS {
        return false;
    }
    if !(active.is_null() || active.as_str().is_some_and(terminal_id)) {
        return false;
    }
    let mut floating = HashSet::new();
    for win in windows {
        let Some(w) = win.as_object() else { return false };
        let stage_ok = match w.get("stage") {
            None => w.len() == 6,
            Some(stage) => w.len() == 7 && matches!(stage.as_str(), Some("compact" | "block")),
        };
        let Some(t) = w.get("terminal").and_then(Value::as_str).filter(|t| terminal_id(t)) else { return false };
        if !stage_ok
            || !floating.insert(t.to_owned())
            || !number_ok(w.get("x"), -MAX_COORD)
            || !number_ok(w.get("y"), -MAX_COORD)
            || !number_ok(w.get("w"), 1.0)
            || !number_ok(w.get("h"), 1.0)
            || !number_ok(w.get("z"), 0.0)
        {
            return false;
        }
    }
    if version == Some(2) {
        let Some(watches) = obj.get("watches").and_then(Value::as_array).filter(|w| w.len() <= MAX_WATCHES) else { return false };
        let mut watched = HashSet::new();
        for watch in watches {
            let Some(w) = watch.as_object().filter(|w| w.len() == 8) else { return false };
            let Some(source) = w.get("source").and_then(Value::as_str).filter(|s| source_id(s)) else { return false };
            let label_ok = w.get("label").and_then(Value::as_str).is_some_and(|l| l.chars().count() <= 120 && !l.chars().any(char::is_control));
            let agent_ok = matches!(w.get("agent").and_then(Value::as_str), Some("claude" | "pi" | "codex"));
            if !watched.insert(source.to_owned())
                || !label_ok
                || !agent_ok
                || !number_ok(w.get("x"), -MAX_COORD)
                || !number_ok(w.get("y"), -MAX_COORD)
                || !number_ok(w.get("w"), 1.0)
                || !number_ok(w.get("h"), 1.0)
                || !number_ok(w.get("z"), 0.0)
            {
                return false;
            }
        }
    }
    let mut linked = HashSet::new();
    for (session, link) in links {
        let Some(l) = link.as_object().filter(|l| l.len() == 2) else { return false };
        let Some(t) = l.get("terminal").and_then(Value::as_str).filter(|t| terminal_id(t)) else { return false };
        let Some(flipped) = l.get("flipped").and_then(Value::as_bool) else { return false };
        if !session_id(session) || !linked.insert(t.to_owned()) || (flipped && floating.contains(t)) {
            return false;
        }
    }
    true
}

/// Missing, oversized, corrupt or invalid files all read as "nothing saved".
pub fn load(path: &Path) -> Option<Value> {
    let meta = fs::metadata(path).ok()?;
    if meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let value: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    is_valid(&value).then_some(value)
}

/// Validated, atomic write (temp file then rename).
pub fn save(path: &Path, state: &Value) -> io::Result<()> {
    if !is_valid(state) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid terminal windows"));
    }
    let bytes = serde_json::to_vec(state).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "terminal windows too large"));
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const S1: &str = "00000000-0000-4000-8000-000000000001";
    const S2: &str = "00000000-0000-4000-9000-000000000002";

    fn win(t: &str, x: f64) -> Value {
        json!({"terminal":t,"x":x,"y":40.0,"w":300,"h":200,"z":1})
    }
    fn good() -> Value {
        json!({"version":1,"active":"deadbeef","windows":[win("deadbeef",10.0), win("0a1b2c3d",-20.0)],
            "links":{S1:{"terminal":"0000aaaa","flipped":true}, S2:{"terminal":"0000bbbb","flipped":false}}})
    }
    fn watch(source: &str) -> Value {
        json!({"source":source,"agent":"pi","label":"my-project","x":5.0,"y":6.0,"w":400,"h":300,"z":2})
    }
    fn good_v2() -> Value {
        let mut v = good();
        v["version"] = json!(2);
        v["watches"] = json!([watch("0123456789abcdef"), watch("fedcba9876543210")]);
        v
    }
    fn dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("wb-termwin-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn round_trips_and_an_empty_state_is_valid() {
        let d = dir();
        let p = d.join("terminal-windows.json");
        save(&p, &good()).unwrap();
        assert_eq!(load(&p), Some(good()));
        assert!(is_valid(&json!({"version":1,"windows":[],"links":{},"active":null})));
        assert!(!p.with_extension("tmp").exists());
        let mut minimized = good_v2();
        minimized["windows"][0]["stage"] = json!("compact");
        minimized["windows"][1]["stage"] = json!("block");
        save(&p, &minimized).unwrap();
        assert_eq!(load(&p), Some(minimized), "minimized windows round-trip");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn watch_windows_are_version_two_only_and_strictly_checked() {
        let d = dir();
        let p = d.join("terminal-windows.json");
        assert!(is_valid(&good_v2()));
        save(&p, &good_v2()).unwrap();
        assert_eq!(load(&p), Some(good_v2()));
        assert!(is_valid(&good()), "version 1 is still read");
        let mut cases: Vec<(&str, Value)> = Vec::new();
        let mut v = good();
        v["watches"] = json!([]);
        cases.push(("version 1 with watches", v));
        let mut v = good_v2();
        v.as_object_mut().unwrap().remove("watches");
        cases.push(("version 2 without watches", v));
        let mut v = good_v2();
        v["watches"][0]["source"] = json!("0123");
        cases.push(("source not an opaque id", v));
        let mut v = good_v2();
        v["watches"][0]["source"] = json!("C:\\secret\\a.jsonl");
        cases.push(("a path is never accepted as a source", v));
        let mut v = good_v2();
        v["watches"][1]["source"] = json!("0123456789abcdef");
        cases.push(("one session watched twice", v));
        let mut v = good_v2();
        v["watches"][0]["agent"] = json!("bash");
        cases.push(("unknown agent", v));
        let mut v = good_v2();
        v["watches"][0]["label"] = json!("a\nb");
        cases.push(("control character in the label", v));
        let mut v = good_v2();
        v["watches"][0]["label"] = json!("x".repeat(121));
        cases.push(("label too long", v));
        let mut v = good_v2();
        v["watches"][0]["w"] = json!(0);
        cases.push(("zero width", v));
        let mut v = good_v2();
        v["watches"][0]["extra"] = json!(1);
        cases.push(("unknown watch field", v));
        let mut v = good_v2();
        v["watches"] = json!((0..=MAX_WATCHES).map(|i| watch(&format!("{:016x}", 0x1000 + i))).collect::<Vec<_>>());
        cases.push(("too many watches", v));
        for (name, bad) in cases {
            assert!(!is_valid(&bad), "{name}");
            assert!(save(&p, &bad).is_err() || !is_valid(&bad), "{name}");
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn the_caps_hold() {
        let many: Vec<Value> = (0..=MAX_WINDOWS).map(|i| win(&format!("{:08x}", 0x1000 + i), 1.0)).collect();
        assert!(!is_valid(&json!({"version":1,"windows":many,"links":{},"active":null})));
        let ok: Vec<Value> = (0..MAX_WINDOWS).map(|i| win(&format!("{:08x}", 0x1000 + i), 1.0)).collect();
        let state = json!({"version":1,"windows":ok,"links":{},"active":null});
        assert!(is_valid(&state));
        let d = dir();
        let p = d.join("terminal-windows.json");
        save(&p, &state).unwrap();
        assert!(load(&p).is_some(), "the largest state stays under the file cap");
        let mut links = serde_json::Map::new();
        for i in 0..=MAX_LINKS {
            links.insert(format!("00000000-0000-4000-8000-{:012}", i), json!({"terminal": format!("{:08x}", 0x2000 + i), "flipped": false}));
        }
        assert!(!is_valid(&json!({"version":1,"windows":[],"links":links,"active":null})));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn invalid_states_are_refused_on_save_and_ignored_on_load() {
        let d = dir();
        let p = d.join("terminal-windows.json");
        let mut cases: Vec<(&str, Value)> = Vec::new();
        let mut v = good();
        v["windows"][0]["terminal"] = json!("DEADBEEF");
        cases.push(("bad terminal id", v));
        let mut v = good();
        v["windows"][1]["terminal"] = json!("deadbeef");
        cases.push(("terminal in two windows", v));
        let mut v = good();
        v["windows"][0]["w"] = json!(0);
        cases.push(("zero width", v));
        let mut v = good();
        v["windows"][0]["x"] = json!(1.0e9);
        cases.push(("absurd position", v));
        let mut v = good();
        v["windows"][0]["z"] = json!(-1);
        cases.push(("negative z", v));
        let mut v = good();
        v["windows"][0]["extra"] = json!(1);
        cases.push(("unknown window field", v));
        let mut v = good();
        v["windows"][0]["stage"] = json!("tiny");
        cases.push(("unknown stage", v));
        let mut v = good();
        v["windows"][0]["stage"] = json!("full");
        cases.push(("a full window leaves the stage out", v));
        let mut v = good();
        v["windows"][0]["stage"] = json!("block");
        v["windows"][0]["extra"] = json!(1);
        cases.push(("unknown field beside a stage", v));
        let mut v = good();
        v["links"]["not-a-session"] = json!({"terminal":"0000cccc","flipped":false});
        cases.push(("bad session id", v));
        let mut v = good();
        v["links"][S2]["terminal"] = json!("0000aaaa");
        cases.push(("terminal linked twice", v));
        let mut v = good();
        v["links"][S2] = json!({"terminal":"deadbeef","flipped":true});
        cases.push(("floating and flipped at once", v));
        let mut v = good();
        v["links"][S1]["flipped"] = json!("yes");
        cases.push(("flipped not a bool", v));
        let mut v = good();
        v["links"][S1]["extra"] = json!(true);
        cases.push(("unknown link field", v));
        let mut v = good();
        v["active"] = json!("nope");
        cases.push(("bad active", v));
        let mut v = good();
        v["version"] = json!(2);
        cases.push(("unknown version", v));
        let mut v = good();
        v["extra"] = json!(1);
        cases.push(("unknown field", v));
        cases.push(("not an object", json!(3)));
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
    fn session_ids_must_be_uuid_v4() {
        assert!(session_id(S1) && session_id(S2));
        for bad in ["", "00000000-0000-1000-8000-000000000001", "00000000-0000-4000-c000-000000000001", "00000000-0000-4000-8000-00000000000G", "0000000000004000800000000000000001"] {
            assert!(!session_id(bad), "{bad}");
        }
    }

    #[test]
    fn a_missing_file_is_nothing_saved() {
        assert_eq!(load(&dir().join("none.json")), None);
    }
}
