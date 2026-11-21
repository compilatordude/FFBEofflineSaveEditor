#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Exvius Save Editor (Rust) — edits campaign.json for Exvius Offline.
//!
//! Features: currencies/rank, per-unit point bank, inventory items+equipment,
//! roster unit editing, raw JSON, timestamped `.bak` backups, and optional
//! adb pull/push against a debuggable install.

use eframe::egui;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const APP_TITLE: &str = "Exvius Save Editor";
const GREEN: egui::Color32 = egui::Color32::from_rgb(90, 190, 110);
const AMBER: egui::Color32 = egui::Color32::from_rgb(220, 170, 70);
const RED: egui::Color32 = egui::Color32::from_rgb(220, 90, 90);
const INT64_MAX: i64 = i64::MAX;
const MAX_SAVE_BYTES: u64 = 512 * 1024 * 1024;
const DEFAULT_ADB: &str = r"D:\Program Files\Netease\MuMuPlayer\nx_main\adb.exe";
const DEFAULT_PACKAGE: &str = "com.exvius.offline.test";
const DEVICE_SAVE: &str = "files/exvius/state/campaign.json";

const CUR_KEYS: [(&str, &str); 4] = [
    ("lapis_free", "Lapis (free)"),
    ("lapis_paid", "Lapis (paid)"),
    ("gil", "Gil"),
    ("friend_point", "Friend points"),
];
const RANK_KEYS: [(&str, &str); 3] = [("level", "Rank level"), ("exp", "Rank EXP"), ("nrg", "NRG")];
const BANK_LABELS: [(&str, &str); 14] = [
    ("XeRVw3s2", "EXP"),
    ("jWS57HG3", "LB EXP"),
    ("LD4UJaf0", "HP pot points"),
    ("ui34MYgo", "MP pot points"),
    ("5sNq90o6", "ATK pot points"),
    ("0xgsDY9E", "DEF pot points"),
    ("VSL4Wj9z", "MAG pot points"),
    ("y7Z5AmhC", "SPR pot points"),
    ("h5s68EtJ", "EX limit HP"),
    ("eu2DXK4I", "EX limit MP"),
    ("8RUQtk4i", "EX limit ATK"),
    ("8cj19yhb", "EX limit DEF"),
    ("rjK5oB1H", "EX limit MAG"),
    ("16NCtrGw", "EX limit SPR"),
];

// ------------------------------------------------------------------ helpers

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn num_of(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_u64().and_then(|u| i64::try_from(u).ok()))
}

/// Save files mix numbers and numeric strings (roster master_id is a string).
fn num_like(v: &Value) -> Option<i64> {
    num_of(v).or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
}

fn get_num(root: &Value, path: &[&str]) -> i64 {
    let mut cur = root;
    for key in path {
        match cur.get(*key) {
            Some(v) => cur = v,
            None => return 0,
        }
    }
    num_of(cur).unwrap_or(0)
}

fn set_num(root: &mut Value, path: &[&str], value: i64) {
    if path.is_empty() {
        return;
    }
    let mut cur = root;
    for key in &path[..path.len() - 1] {
        if !cur.is_object() {
            return;
        }
        if cur.get(*key).is_none() {
            cur[*key] = Value::Object(Map::new());
        }
        cur = match cur.get_mut(*key) {
            Some(v) => v,
            None => return,
        };
    }
    if let Some(obj) = cur.as_object_mut() {
        obj.insert(path[path.len() - 1].to_string(), Value::from(value));
    }
}

fn parse_i64(text: &str, label: &str) -> Result<i64, String> {
    let cleaned: String = text.chars().filter(|c| *c != ',' && *c != '_' && *c != ' ').collect();
    if cleaned.is_empty() {
        return Err(format!("{label}: empty value"));
    }
    let parsed = if let Some(hex) = cleaned.strip_prefix("0x").or_else(|| cleaned.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16)
    } else {
        cleaned.parse::<i64>()
    };
    parsed.map_err(|e| format!("{label}: {e}"))
}

/// Android package names are interpolated into adb shell commands, so they are
/// only accepted when they match this strict shape.
fn valid_package(name: &str) -> bool {
    if name.is_empty() || name.len() > 200 {
        return false;
    }
    // Android package names need at least two segments ("a.b").
    if !name.contains('.') {
        return false;
    }
    let mut chars = name.chars();
    let first_ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic());
    if !first_ok {
        return false;
    }
    let mut segment_start = true;
    for c in name.chars() {
        if c == '.' {
            if segment_start {
                return false;
            }
            segment_start = true;
        } else if c.is_ascii_alphanumeric() || c == '_' {
            segment_start = false;
        } else {
            return false;
        }
    }
    if segment_start {
        return false;
    }
    // first char of every segment must be a letter
    name.split('.').all(|seg| seg.chars().next().map(|c| c.is_ascii_alphabetic()).unwrap_or(false))
}

// ------------------------------------------------------------------ apk inspection

fn kv(ui: &mut egui::Ui, key: &str, value: impl Into<String>) {
    ui.label(egui::RichText::new(key).weak());
    ui.label(value.into());
    ui.end_row();
}

#[derive(Default, Clone)]
struct ApkInfo {
    path: String,
    size: u64,
    sha256: String,
    package: String,
    version_code: i64,
    version_name: String,
    min_sdk: String,
    target_sdk: String,
    debuggable: bool,
    signer: String,
    signer_sha256: String,
    error: Option<String>,
}

#[derive(Default, Clone)]
struct PhoneInfo {
    package: String,
    version_code: i64,
    signer: String,
    error: Option<String>,
}

fn sha256_file(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 256 * 1024];
    loop {
        let read = file.read(&mut buf).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Some(format!("{:x}", hasher.finalize()))
}

fn candidate_sdk_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Ok(path) = std::env::var(var) {
            if !path.is_empty() {
                roots.push(PathBuf::from(path));
            }
        }
    }
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        roots.push(PathBuf::from(local).join("Android").join("Sdk"));
    }
    roots.push(PathBuf::from(r"C:\Android\Sdk"));
    roots.push(PathBuf::from(r"D:\Android\Sdk"));
    roots
}

fn version_key(name: &str) -> Vec<u64> {
    name.split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<u64>().ok())
        .collect()
}

/// Newest build-tools directory that contains both aapt2 and apksigner.
fn find_build_tools() -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    for root in candidate_sdk_roots() {
        let Ok(entries) = fs::read_dir(root.join("build-tools")) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.join("aapt2.exe").is_file() && path.join("apksigner.bat").is_file() {
                found.push(path);
            }
        }
    }
    found.sort_by_key(|p| version_key(&p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()));
    found.pop()
}

fn find_java_home() -> Option<PathBuf> {
    if let Ok(home) = std::env::var("JAVA_HOME") {
        let path = PathBuf::from(&home);
        if path.join("bin").join("java.exe").is_file() {
            return Some(path);
        }
    }
    let mut candidates = Vec::new();
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local).join("Programs").join("Android Studio").join("jbr"));
    }
    candidates.push(PathBuf::from(r"D:\Program Files\Android\Android Studio\jbr"));
    candidates.push(PathBuf::from(r"C:\Program Files\Android\Android Studio\jbr"));
    candidates.push(PathBuf::from(r"C:\Program Files\Java\jdk-21"));
    candidates.push(PathBuf::from(r"C:\Program Files\Java\jdk-17"));
    candidates.into_iter().find(|p| p.join("bin").join("java.exe").is_file())
}

fn quoted(line: &str, key: &str) -> Option<String> {
    let start = line.find(&format!("{key}='"))? + key.len() + 2;
    let rest = &line[start..];
    Some(rest[..rest.find('\'')?].to_string())
}

fn run_capture(cmd: &mut Command) -> Result<(bool, String, String), String> {
    let out = cmd.output().map_err(|e| e.to_string())?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    ))
}

fn inspect_apk(path: &Path) -> ApkInfo {
    let mut info = ApkInfo {
        path: path.display().to_string(),
        size: fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        sha256: sha256_file(path).unwrap_or_default(),
        ..Default::default()
    };
    let Some(tools) = find_build_tools() else {
        info.error = Some("Android SDK build-tools not found (aapt2.exe + apksigner.bat).".to_string());
        return info;
    };

    let aapt2 = tools.join("aapt2.exe");
    match run_capture(Command::new(&aapt2).args(["dump", "badging", &info.path])) {
        Ok((ok, stdout, stderr)) => {
            if !ok {
                info.error = Some(format!("aapt2 failed: {}", stderr.trim()));
                return info;
            }
            for line in stdout.lines() {
                if line.starts_with("package:") {
                    if let Some(v) = quoted(line, "name") {
                        info.package = v;
                    }
                    if let Some(v) = quoted(line, "versionCode") {
                        info.version_code = v.parse().unwrap_or(0);
                    }
                    if let Some(v) = quoted(line, "versionName") {
                        info.version_name = v;
                    }
                } else if let Some(rest) = line.strip_prefix("minSdkVersion:").or_else(|| line.strip_prefix("sdkVersion:")) {
                    info.min_sdk = rest.trim().trim_matches('\'').to_string();
                } else if let Some(rest) = line.strip_prefix("targetSdkVersion:") {
                    info.target_sdk = rest.trim().trim_matches('\'').to_string();
                } else if line.contains("application-debuggable") {
                    info.debuggable = true;
                }
            }
        }
        Err(err) => {
            info.error = Some(format!("could not run aapt2: {err}"));
            return info;
        }
    }

    let apksigner = tools.join("apksigner.bat");
    let mut cmd = Command::new("cmd");
    cmd.args(["/C", &apksigner.display().to_string(), "verify", "--print-certs", &info.path]);
    if let Some(java_home) = find_java_home() {
        cmd.env("JAVA_HOME", java_home);
    }
    match run_capture(&mut cmd) {
        Ok((_ok, stdout, stderr)) => {
            let text = format!("{stdout}\n{stderr}");
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("Signer #1 certificate DN: ") {
                    info.signer = rest.trim().to_string();
                } else if let Some(rest) = line.strip_prefix("Signer #1 certificate SHA-256 digest: ") {
                    info.signer_sha256 = rest.trim().to_string();
                }
            }
            if info.signer.is_empty() && info.error.is_none() {
                info.error = Some(format!("apksigner could not read a signer: {}", stderr.trim()));
            }
        }
        Err(err) => info.error = Some(format!("could not run apksigner: {err}")),
    }
    info
}

fn inspect_phone(adb: &str, pkg: &str) -> PhoneInfo {
    let mut info = PhoneInfo { package: pkg.to_string(), ..Default::default() };
    if !Path::new(adb).is_file() {
        info.error = Some(format!("adb not found: {adb}"));
        return info;
    }
    match Command::new(adb).args(["shell", &format!("dumpsys package {pkg}")]).output() {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            let mut installed = false;
            for line in text.lines() {
                let trimmed = line.trim();
                if let Some(value) = trimmed.strip_prefix("versionCode=") {
                    let digits: String = value.chars().take_while(|c| c.is_ascii_digit()).collect();
                    info.version_code = digits.parse().unwrap_or(0);
                    installed = true;
                    break;
                }
            }
            if !installed {
                info.error = Some(format!("{pkg} is not installed on the phone"));
                return info;
            }
        }
        Ok(_) => {
            info.error = Some(format!("{pkg} is not installed on the phone"));
            return info;
        }
        Err(err) => {
            info.error = Some(format!("adb failed: {err}"));
            return info;
        }
    }

    if let Ok(out) = Command::new(adb).args(["shell", "pm", "path", pkg]).output() {
        let text = String::from_utf8_lossy(&out.stdout);
        if let Some(remote) = text.lines().next().and_then(|l| l.strip_prefix("package:")) {
            let remote = remote.trim().to_string();
            let tmp = std::env::temp_dir().join(format!("exvius_installed_{}.apk", now_secs()));
            if let Ok(pulled) = Command::new(adb).args(["pull", &remote, &tmp.display().to_string()]).output() {
                if pulled.status.success() {
                    info.signer = inspect_apk(&tmp).signer;
                }
            }
            let _ = fs::remove_file(&tmp);
        }
    }
    info
}

// ------------------------------------------------------------------ names

const TEXT_DIR: &str = "files/exvius/data/static/text_decoded";

#[derive(Clone)]
struct NameHit {
    id: i64,
    name: String,
    kind: String,
}

#[derive(Default)]
struct Names {
    items: HashMap<i64, String>,
    equipment: HashMap<i64, String>,
    materia: HashMap<i64, String>,
    units: HashMap<i64, String>,
}

impl Names {
    fn path() -> Option<PathBuf> {
        Config::path().map(|p| p.with_file_name("names.json"))
    }

    fn load() -> Option<Self> {
        let path = Self::path()?;
        let text = fs::read_to_string(path).ok()?;
        let value: Value = serde_json::from_str(&text).ok()?;
        let mut names = Names::default();
        fn fill(value: &Value, key: &str, map: &mut HashMap<i64, String>) {
            if let Some(obj) = value.get(key).and_then(|v| v.as_object()) {
                for (k, v) in obj {
                    if let (Ok(id), Some(name)) = (k.parse::<i64>(), v.as_str()) {
                        map.insert(id, name.to_string());
                    }
                }
            }
        }
        fill(&value, "items", &mut names.items);
        fill(&value, "equipment", &mut names.equipment);
        fill(&value, "materia", &mut names.materia);
        fill(&value, "units", &mut names.units);
        Some(names)
    }

    fn save_cache(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let value = serde_json::json!({
            "items": self.items,
            "equipment": self.equipment,
            "materia": self.materia,
            "units": self.units,
        });
        if let Ok(text) = serde_json::to_string(&value) {
            let _ = fs::write(path, text);
        }
    }

    fn total(&self) -> usize {
        self.items.len() + self.equipment.len() + self.materia.len() + self.units.len()
    }

    fn item(&self, id: i64, item_type: Option<i64>) -> Option<&str> {
        if item_type == Some(22) {
            self.materia.get(&id).or_else(|| self.items.get(&id)).map(|s| s.as_str())
        } else {
            self.items.get(&id).or_else(|| self.materia.get(&id)).map(|s| s.as_str())
        }
    }

    fn equipment(&self, id: i64) -> Option<&str> {
        self.equipment.get(&id).or_else(|| self.items.get(&id)).map(|s| s.as_str())
    }

    fn unit(&self, id: i64) -> Option<&str> {
        self.units.get(&id).map(|s| s.as_str())
    }

    /// Name search across items, equipment and materia.
    fn search(&self, needle: &str, limit: usize) -> Vec<NameHit> {
        let needle = needle.trim().to_lowercase();
        if needle.len() < 2 {
            return Vec::new();
        }
        let mut hits: Vec<NameHit> = Vec::new();
        let mut scan = |map: &HashMap<i64, String>, kind: &str| {
            for (id, name) in map {
                if hits.len() >= limit.saturating_mul(3) {
                    return;
                }
                if name.to_lowercase().contains(&needle) {
                    hits.push(NameHit { id: *id, name: name.clone(), kind: kind.to_string() });
                }
            }
        };
        scan(&self.items, "item");
        scan(&self.materia, "materia");
        scan(&self.equipment, "equipment");
        hits.sort_by_key(|h| (h.name.to_lowercase().find(&needle).unwrap_or(usize::MAX), h.name.len()));
        hits.truncate(limit);
        hits
    }

    /// Name search restricted to units.
    fn search_units(&self, needle: &str, limit: usize) -> Vec<NameHit> {
        let needle = needle.trim().to_lowercase();
        if needle.len() < 2 {
            return Vec::new();
        }
        let mut hits: Vec<NameHit> = self
            .units
            .iter()
            .filter(|(_, name)| name.to_lowercase().contains(&needle))
            .map(|(id, name)| NameHit { id: *id, name: name.clone(), kind: "unit".to_string() })
            .collect();
        hits.sort_by_key(|h| (h.name.to_lowercase().find(&needle).unwrap_or(usize::MAX), h.name.len()));
        hits.truncate(limit);
        hits
    }

    fn fetch_table(adb: &str, pkg: &str, file: &str, prefix: &str) -> Result<HashMap<i64, String>, String> {
        let path = format!("{TEXT_DIR}/{file}");
        let out = Command::new(adb)
            .args(["exec-out", "run-as", pkg, "cat", &path])
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut map = HashMap::new();
        for line in text.lines() {
            let Ok(value) = serde_json::from_str::<Value>(line) else { continue };
            let Some(key) = value.get("key").and_then(|k| k.as_str()) else { continue };
            let Some(id_text) = key.strip_prefix(prefix) else { continue };
            let Ok(id) = id_text.parse::<i64>() else { continue };
            if let Some(name) = value.get("v").and_then(|a| a.get(0)).and_then(|s| s.as_str()) {
                map.insert(id, name.to_string());
            }
        }
        Ok(map)
    }

    fn from_phone(adb: &str, pkg: &str) -> Result<Self, String> {
        Ok(Self {
            items: Self::fetch_table(adb, pkg, "F_TEXT_ITEM_NAME.ndjson", "MST_ITEM_NAME_")?,
            equipment: Self::fetch_table(adb, pkg, "F_TEXT_ITEM_EQUIP_NAME.ndjson", "MST_EQUIP_ITEM_NAME_")?,
            materia: Self::fetch_table(adb, pkg, "F_TEXT_MATERIA_NAME.ndjson", "MST_MATERIA_NAME_")?,
            units: Self::fetch_table(adb, pkg, "F_TEXT_UNITS_NAME.ndjson", "MST_UNIT_NAME_")?,
        })
    }
}

// ------------------------------------------------------------------ editing helpers

fn next_instance_id(roster: &[Value]) -> i64 {
    roster
        .iter()
        .filter_map(|u| u.get("instance_id"))
        .filter_map(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok())))
        .max()
        .unwrap_or(0)
        + 1
}

/// Clone a roster entry as a template and overwrite the fields that define a new unit.
fn make_unit_entry(template: &Value, instance_id: i64, master_id: i64, level: i64, exp: i64, favourite: bool) -> Value {
    let mut entry = if template.is_object() { template.clone() } else { Value::Object(Map::new()) };
    entry["instance_id"] = Value::from(instance_id.to_string());
    entry["master_id"] = Value::from(master_id.to_string());
    entry["level"] = Value::from(level);
    entry["exp"] = Value::from(exp);
    entry["total_exp"] = Value::from(0);
    entry["lb_exp"] = Value::from(0);
    entry["nv_ex_level"] = Value::from(0);
    entry["guest"] = Value::from(0);
    entry["storage_state"] = Value::from(0);
    entry["favourite"] = Value::from(if favourite { 1 } else { 0 });
    entry["granted_rarity"] = Value::Null;
    entry["first_get_unit_id"] = Value::Null;
    entry["lb_level"] = Value::from(1);
    entry["equipment"] = serde_json::json!({"1":"0","2":"0","3":"0","4":"0","5":"0","6":"0"});
    entry["materia"] = serde_json::json!(["0"]);
    entry["vision_card"] = Value::from("");
    entry["affinity_points"] = serde_json::json!(["",""]);
    entry["affinity_info"] = serde_json::json!(["",""]);
    entry["skin_id"] = Value::from(0);
    entry
}

/// Add (or top up) an inventory entry. Returns false when the save has no inventory object.
fn add_inventory(data: &mut Value, section: u8, item_type: i64, item_id: i64, count: i64) -> bool {
    let Some(inv) = data.get_mut("inventory").and_then(|v| v.as_object_mut()) else {
        return false;
    };
    let key = if section == 1 { "equipment" } else { "items" };
    if !inv.contains_key(key) {
        inv.insert(key.to_string(), Value::Array(Vec::new()));
    }
    let Some(arr) = inv.get_mut(key).and_then(|v| v.as_array_mut()) else {
        return false;
    };
    for entry in arr.iter_mut() {
        let same = entry.get("item_id").and_then(num_of) == Some(item_id)
            && (section == 1 || entry.get("item_type").and_then(num_of) == Some(item_type));
        if same {
            let current = entry.get("count").and_then(num_of).unwrap_or(0);
            entry["count"] = Value::from(current.saturating_add(count));
            return true;
        }
    }
    if section == 1 {
        arr.push(serde_json::json!({"item_id": item_id, "count": count}));
    } else {
        arr.push(serde_json::json!({"item_type": item_type, "item_id": item_id, "count": count}));
    }
    true
}

/// Duplicate an existing roster entry: fresh instance_id, optionally dropping the loadout.
fn clone_roster_entry(source: &Value, instance_id: i64, copy_gear: bool) -> Value {
    let mut entry = source.clone();
    if !entry.is_object() {
        entry = Value::Object(Map::new());
    }
    entry["instance_id"] = Value::from(instance_id.to_string());
    entry["guest"] = Value::from(0);
    entry["storage_state"] = Value::from(0);
    if !copy_gear {
        entry["equipment"] = serde_json::json!({"1":"0","2":"0","3":"0","4":"0","5":"0","6":"0"});
        entry["materia"] = serde_json::json!(["0"]);
        entry["vision_card"] = Value::from("");
        entry["skin_id"] = Value::from(0);
    }
    entry
}

// ------------------------------------------------------------------ config

#[derive(Default, Clone)]
struct Config {
    last_path: Option<String>,
    adb: Option<String>,
    package: Option<String>,
}

impl Config {
    fn path() -> Option<PathBuf> {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("ExviusSaveEditor").join("config.json"))
    }

    fn load() -> Self {
        let Some(path) = Self::path() else { return Self::default() };
        let Ok(text) = fs::read_to_string(path) else { return Self::default() };
        let Ok(value) = serde_json::from_str::<Value>(&text) else { return Self::default() };
        let s = |k: &str| value.get(k).and_then(|v| v.as_str()).map(|v| v.to_string());
        Self { last_path: s("last_path"), adb: s("adb"), package: s("package") }
    }

    fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let value = serde_json::json!({
            "last_path": self.last_path,
            "adb": self.adb,
            "package": self.package,
        });
        if let Ok(text) = serde_json::to_string_pretty(&value) {
            let _ = fs::write(path, text);
        }
    }
}

// ------------------------------------------------------------------ app state

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Currencies,
    Bank,
    Items,
    Roster,
    Raw,
}

#[derive(Clone)]
struct UnitEdit {
    idx: usize,
    instance: String,
    master: String,
    fields: [String; 5],
}

struct Row {
    section: u8, // 0 = items, 1 = equipment
    idx: usize,
    item_type: Option<i64>,
    item_id: i64,
    name: String,
    count: i64,
}

struct RosterRow {
    idx: usize,
    instance: String,
    master: String,
    name: String,
    level: i64,
    exp: i64,
    lb_level: i64,
    lb_exp: i64,
    ex_level: i64,
}

struct App {
    data: Option<Value>,
    path: Option<PathBuf>,
    dirty: bool,
    status: String,
    tab: Tab,
    cfg: Config,

    cur: Vec<String>,
    rank: Vec<String>,
    bank: Vec<String>,

    filter_items: String,
    filter_roster: String,
    raw: String,
    names: Names,

    show_settings: bool,
    adb_edit: String,
    pkg_edit: String,

    show_add: bool,
    add_section: usize,
    add_type: String,
    add_id: String,
    add_count: String,
    add_search: String,

    show_add_unit: bool,
    unit_search: String,
    unit_master: String,
    unit_level: String,
    unit_exp: String,
    unit_favourite: bool,

    unit_edit: Option<UnitEdit>,
    clone_idx: Option<usize>,
    clone_gear: bool,
    show_about: bool,
    show_apk: bool,
    apk: Option<ApkInfo>,
    phone: Option<PhoneInfo>,
}

impl App {
    fn new(_cc: &eframe::CreationContext<'_>, initial: Option<String>, apk_initial: Option<String>) -> Self {
        let cfg = Config::load();
        let mut app = App {
            data: None,
            path: None,
            dirty: false,
            status: "Open a campaign.json to begin (File > Open).".to_string(),
            tab: Tab::Currencies,
            adb_edit: cfg.adb.clone().unwrap_or_else(|| DEFAULT_ADB.to_string()),
            pkg_edit: cfg.package.clone().unwrap_or_else(|| DEFAULT_PACKAGE.to_string()),
            cfg,
            cur: vec![String::new(); CUR_KEYS.len()],
            rank: vec![String::new(); RANK_KEYS.len()],
            bank: vec![String::new(); BANK_LABELS.len()],
            filter_items: String::new(),
            filter_roster: String::new(),
            raw: String::new(),
            names: Names::load().unwrap_or_default(),
            show_settings: false,
            show_add: false,
            add_section: 0,
            add_type: "20".to_string(),
            add_id: String::new(),
            add_count: "1".to_string(),
            add_search: String::new(),
            show_add_unit: false,
            unit_search: String::new(),
            unit_master: String::new(),
            unit_level: "1".to_string(),
            unit_exp: "0".to_string(),
            unit_favourite: false,
            unit_edit: None,
            clone_idx: None,
            clone_gear: false,
            show_about: false,
            show_apk: false,
            apk: None,
            phone: None,
        };
        if let Some(p) = initial.or_else(|| app.cfg.last_path.clone()) {
            if Path::new(&p).is_file() {
                app.load_path(PathBuf::from(p));
            }
        }
        if let Some(p) = apk_initial {
            app.apk = Some(inspect_apk(Path::new(&p)));
            app.show_apk = true;
        }
        app
    }

    fn adb(&self) -> String {
        self.cfg.adb.clone().unwrap_or_else(|| DEFAULT_ADB.to_string())
    }

    fn pkg(&self) -> String {
        self.cfg.package.clone().unwrap_or_else(|| DEFAULT_PACKAGE.to_string())
    }

    // ---------------------------------------------------------- load / save
    fn load_path(&mut self, path: PathBuf) {
        if let Ok(meta) = fs::metadata(&path) {
            if meta.len() > MAX_SAVE_BYTES {
                self.status = format!("Refusing to load {} (too large).", path.display());
                return;
            }
        }
        let parsed = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        match parsed {
            Some(value) if value.is_object() => {
                self.data = Some(value);
                self.path = Some(path.clone());
                self.dirty = false;
                self.cfg.last_path = Some(path.display().to_string());
                self.cfg.save();
                self.refresh_forms();
                self.status = format!("Loaded {} ({} bytes).", path.display(), fs::metadata(&path).map(|m| m.len()).unwrap_or(0));
            }
            _ => {
                self.status = format!("Could not read {} as a campaign save.", path.display());
            }
        }
    }

    fn refresh_forms(&mut self) {
        let Some(data) = &self.data else { return };
        for (i, (key, _)) in CUR_KEYS.iter().enumerate() {
            self.cur[i] = get_num(data, &["currencies", key]).to_string();
        }
        for (i, (key, _)) in RANK_KEYS.iter().enumerate() {
            self.rank[i] = get_num(data, &["rank", key]).to_string();
        }
        for (i, (token, _)) in BANK_LABELS.iter().enumerate() {
            self.bank[i] = get_num(data, &["exvius_unit_points", "banks", "0", token]).to_string();
        }
        self.raw = serde_json::to_string_pretty(data).unwrap_or_default();
    }

    fn collect(&mut self) -> Result<(), String> {
        let App { data, cur, rank, bank, .. } = self;
        let data = data.as_mut().ok_or("No file loaded")?;
        for (i, (key, _)) in CUR_KEYS.iter().enumerate() {
            let v = parse_i64(&cur[i], key)?;
            set_num(data, &["currencies", key], v);
        }
        for (i, (key, _)) in RANK_KEYS.iter().enumerate() {
            let v = parse_i64(&rank[i], key)?;
            set_num(data, &["rank", key], v);
        }
        for (i, (token, _)) in BANK_LABELS.iter().enumerate() {
            let v = parse_i64(&bank[i], token)?;
            set_num(data, &["exvius_unit_points", "banks", "0", token], v);
        }
        Ok(())
    }

    fn save(&mut self) {
        if self.data.is_none() {
            self.save_as();
            return;
        }
        if let Err(err) = self.collect() {
            self.status = err;
            return;
        }
        if let Some(path) = self.path.clone() {
            self.write_to(path);
        } else {
            self.save_as();
        }
    }

    fn save_as(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_file_name("campaign.json")
            .add_filter("JSON save", &["json"])
            .save_file()
        else {
            return;
        };
        if let Err(err) = self.collect() {
            self.status = err;
            return;
        }
        self.write_to(path);
    }

    fn write_to(&mut self, path: PathBuf) {
        let Some(data) = &self.data else { return };
        let Ok(mut text) = serde_json::to_string_pretty(data) else {
            self.status = "Could not serialise the save.".to_string();
            return;
        };
        text.push('\n');
        let mut backup_note = String::new();
        if path.is_file() {
            let backup = format!("{}.{}.bak", path.display(), now_secs());
            match fs::copy(&path, &backup) {
                Ok(_) => backup_note = format!(" Backup: {}", Path::new(&backup).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()),
                Err(err) => {
                    self.status = format!("Could not create backup: {err}");
                    return;
                }
            }
        }
        match fs::write(&path, text) {
            Ok(_) => {
                self.path = Some(path.clone());
                self.dirty = false;
                self.cfg.last_path = Some(path.display().to_string());
                self.cfg.save();
                self.status = format!("Saved {}.{}", path.display(), backup_note);
            }
            Err(err) => self.status = format!("Could not write {}: {err}", path.display()),
        }
    }

    // ---------------------------------------------------------- phone
    fn refresh_names(&mut self) {
        let adb = self.adb();
        if !Path::new(&adb).is_file() {
            self.status = format!("adb not found: {adb}");
            return;
        }
        let pkg = self.pkg();
        if !valid_package(&pkg) {
            self.status = "Refusing to refresh names: package name is not valid (Settings).".to_string();
            return;
        }
        match Names::from_phone(&adb, &pkg) {
            Ok(names) => {
                let total = names.total();
                names.save_cache();
                self.names = names;
                self.status = format!("Loaded {total} names from the phone.");
            }
            Err(err) => self.status = format!("Name refresh failed: {err}"),
        }
    }

    fn pull_from_phone(&mut self) {
        let adb = self.adb();
        if !Path::new(&adb).is_file() {
            self.status = format!("adb not found: {adb}");
            return;
        }
        let pkg = self.pkg();
        if !valid_package(&pkg) {
            self.status = "Refusing to pull: package name is not valid (Phone > Settings).".to_string();
            return;
        }
        let output = Command::new(&adb)
            .args(["exec-out", "run-as", &pkg, "cat", DEVICE_SAVE])
            .output();
        match output {
            Ok(out) if out.status.success() && !out.stdout.is_empty() => {
                let Some(path) = rfd::FileDialog::new()
                    .set_file_name("campaign.json")
                    .add_filter("JSON save", &["json"])
                    .save_file()
                else {
                    return;
                };
                if let Err(err) = fs::write(&path, &out.stdout) {
                    self.status = format!("Could not write {}: {err}", path.display());
                    return;
                }
                let len = out.stdout.len();
                self.load_path(path);
                self.status = format!("Pulled {len} bytes from the phone.");
            }
            Ok(out) => {
                let err = String::from_utf8_lossy(&out.stderr);
                self.status = format!("Pull failed: {}", err.trim());
            }
            Err(err) => self.status = format!("adb failed: {err}"),
        }
    }

    fn push_to_phone(&mut self) {
        let Some(path) = self.path.clone() else {
            self.status = "Open and save a campaign.json first.".to_string();
            return;
        };
        if self.dirty {
            self.save();
        }
        let adb = self.adb();
        if !Path::new(&adb).is_file() {
            self.status = format!("adb not found: {adb}");
            return;
        }
        let pkg = self.pkg();
        if !valid_package(&pkg) {
            self.status = "Refusing to push: package name is not valid (Phone > Settings).".to_string();
            return;
        }
        let Ok(bytes) = fs::read(&path) else {
            self.status = "Could not read the open save.".to_string();
            return;
        };
        let tmp = std::env::temp_dir().join(format!("exvius_push_{}_{}.json", std::process::id(), now_secs()));
        if let Err(err) = fs::write(&tmp, &bytes) {
            self.status = format!("Could not stage the save: {err}");
            return;
        }
        let tmp_arg = tmp.display().to_string();
        let steps: Vec<(Vec<String>, &str)> = vec![
            (vec!["shell".into(), format!("am force-stop {pkg}")], "force-stop"),
            (vec!["push".into(), tmp_arg.clone(), "/data/local/tmp/campaign_push.json".into()], "push"),
            (vec!["shell".into(), "chmod 644 /data/local/tmp/campaign_push.json".into()], "chmod"),
            (
                vec!["shell".into(), format!("run-as {pkg} cp /data/local/tmp/campaign_push.json {DEVICE_SAVE}")],
                "copy into app",
            ),
        ];
        for (args, label) in steps {
            match Command::new(&adb).args(&args).output() {
                Ok(out) if out.status.success() => {}
                Ok(out) => {
                    let err = String::from_utf8_lossy(&out.stderr);
                    let msg = String::from_utf8_lossy(&out.stdout);
                    let _ = fs::remove_file(&tmp);
                    self.status = format!("Push failed at '{label}': {}{}", err.trim(), msg.trim());
                    return;
                }
                Err(err) => {
                    let _ = fs::remove_file(&tmp);
                    self.status = format!("adb failed at '{label}': {err}");
                    return;
                }
            }
        }
        let _ = Command::new(&adb)
            .args(["shell", &format!("am start -n {pkg}/com.exvius.local.DataDownloadActivity")])
            .output();
        let _ = fs::remove_file(&tmp);
        self.status = "Save pushed to the phone; the game was restarted.".to_string();
    }

    // ---------------------------------------------------------- UI
    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("Open…").clicked() {
                    if let Some(path) = rfd::FileDialog::new().add_filter("JSON save", &["json"]).pick_file() {
                        self.load_path(path);
                    }
                }
                if ui.button("Save").clicked() {
                    self.save();
                }
                if ui.button("Save as…").clicked() {
                    self.save_as();
                }
                if ui.button("Reload").clicked() {
                    if let Some(path) = self.path.clone() {
                        self.load_path(path);
                    }
                }
                ui.separator();
                if ui.button("Pull from phone").clicked() {
                    self.pull_from_phone();
                }
                if ui.button("Push to phone").clicked() {
                    self.push_to_phone();
                }
                if ui.button("Settings…").clicked() {
                    self.adb_edit = self.adb();
                    self.pkg_edit = self.pkg();
                    self.show_settings = true;
                }
                if ui.button("Refresh names").clicked() {
                    self.refresh_names();
                }
                if ui.button("Check APK…").clicked() {
                    if let Some(path) = rfd::FileDialog::new().add_filter("Android package", &["apk"]).pick_file() {
                        self.apk = Some(inspect_apk(&path));
                        self.phone = None;
                        self.show_apk = true;
                    }
                }
                ui.separator();
                if ui.button("About").clicked() {
                    self.show_about = true;
                }
            });
        });
    }

    fn status_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(&self.status);
                if self.dirty {
                    ui.label(egui::RichText::new("● unsaved").color(egui::Color32::from_rgb(220, 160, 60)));
                }
            });
        });
    }

    fn tabs(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Currencies, "Currencies & Rank");
            ui.selectable_value(&mut self.tab, Tab::Bank, "Point Bank");
            ui.selectable_value(&mut self.tab, Tab::Items, "Items & Equipment");
            ui.selectable_value(&mut self.tab, Tab::Roster, "Roster");
            ui.selectable_value(&mut self.tab, Tab::Raw, "Raw JSON");
        });
        ui.separator();
        match self.tab {
            Tab::Currencies => self.tab_currencies(ui),
            Tab::Bank => self.tab_bank(ui),
            Tab::Items => self.tab_items(ui),
            Tab::Roster => self.tab_roster(ui),
            Tab::Raw => self.tab_raw(ui),
        }
    }

    fn tab_currencies(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Currencies");
            egui::Grid::new("cur_grid").num_columns(3).spacing([12.0, 6.0]).show(ui, |ui| {
                for (i, (key, label)) in CUR_KEYS.iter().enumerate() {
                    ui.label(*label);
                    ui.add_sized([240.0, 22.0], egui::TextEdit::singleline(&mut self.cur[i]));
                    ui.label(egui::RichText::new(format!("currencies.{key}")).weak());
                    ui.end_row();
                }
            });
            ui.horizontal(|ui| {
                if ui.button("Set lapis to 999,999,999").clicked() {
                    self.cur[0] = "999999999".to_string();
                }
                if ui.button("Set gil to 999,999,999").clicked() {
                    self.cur[2] = "999999999".to_string();
                }
                if ui.button("Set friend points to 1,000,000").clicked() {
                    self.cur[3] = "1000000".to_string();
                }
            });
            ui.add_space(16.0);
            ui.heading("Rank");
            egui::Grid::new("rank_grid").num_columns(3).spacing([12.0, 6.0]).show(ui, |ui| {
                for (i, (key, label)) in RANK_KEYS.iter().enumerate() {
                    ui.label(*label);
                    ui.add_sized([240.0, 22.0], egui::TextEdit::singleline(&mut self.rank[i]));
                    ui.label(egui::RichText::new(format!("rank.{key}")).weak());
                    ui.end_row();
                }
            });
            ui.add_space(12.0);
            ui.label(egui::RichText::new("Values are written into campaign.json when you press Save. A timestamped .bak is made first.").weak());
        });
    }

    fn tab_bank(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Unit point bank  (exvius_unit_points.banks[\"0\"])");
            ui.label(egui::RichText::new("Read by the Enhance Unit screen: EXP / LB EXP / stat pot points / EX stat limits.").weak());
            ui.add_space(8.0);
            egui::Grid::new("bank_grid").num_columns(3).spacing([12.0, 6.0]).show(ui, |ui| {
                for (i, (token, label)) in BANK_LABELS.iter().enumerate() {
                    ui.label(*label);
                    ui.add_sized([280.0, 22.0], egui::TextEdit::singleline(&mut self.bank[i]));
                    ui.label(egui::RichText::new(*token).weak());
                    ui.end_row();
                }
            });
            ui.horizontal(|ui| {
                if ui.button("90% of int64 max").clicked() {
                    let v = (INT64_MAX as i128 * 9 / 10) as i64;
                    for s in self.bank.iter_mut() {
                        *s = v.to_string();
                    }
                }
                if ui.button("int64 max").clicked() {
                    for s in self.bank.iter_mut() {
                        *s = INT64_MAX.to_string();
                    }
                }
                if ui.button("Zero all").clicked() {
                    for s in self.bank.iter_mut() {
                        *s = "0".to_string();
                    }
                }
            });
        });
    }

    fn collect_rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        let Some(data) = &self.data else { return rows };
        let needle = self.filter_items.trim().to_lowercase();
        if let Some(items) = data.get("inventory").and_then(|v| v.get("items")).and_then(|v| v.as_array()) {
            for (idx, entry) in items.iter().enumerate() {
                let item_type = entry.get("item_type").and_then(num_of);
                let item_id = entry.get("item_id").and_then(num_of).unwrap_or(0);
                let count = entry.get("count").and_then(num_of).unwrap_or(0);
                let name = self.names.item(item_id, item_type).unwrap_or("—").to_string();
                let hay = format!("{item_id} {} {} item", item_type.unwrap_or(0), name);
                if needle.is_empty() || hay.to_lowercase().contains(&needle) {
                    rows.push(Row { section: 0, idx, item_type, item_id, name, count });
                }
            }
        }
        if let Some(eq) = data.get("inventory").and_then(|v| v.get("equipment")).and_then(|v| v.as_array()) {
            for (idx, entry) in eq.iter().enumerate() {
                let item_id = entry.get("item_id").and_then(num_of).unwrap_or(0);
                let count = entry.get("count").and_then(num_of).unwrap_or(0);
                let name = self.names.equipment(item_id).unwrap_or("—").to_string();
                let hay = format!("{item_id} {name} equipment");
                if needle.is_empty() || hay.to_lowercase().contains(&needle) {
                    rows.push(Row { section: 1, idx, item_type: None, item_id, name, count });
                }
            }
        }
        rows
    }

    fn tab_items(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Filter:");
            ui.add(egui::TextEdit::singleline(&mut self.filter_items).desired_width(200.0));
            if ui.button("Add entry…").clicked() {
                self.add_id.clear();
                self.add_count = "1".to_string();
                self.add_section = 0;
                self.add_type = "20".to_string();
                self.show_add = true;
            }
        });
        if self.data.is_none() {
            ui.label("Open a save first.");
            return;
        }
        let rows = self.collect_rows();
        let total = rows.len();
        let mut edits: Vec<(u8, usize, i64)> = Vec::new();
        let mut deletes: Vec<(u8, usize)> = Vec::new();
        ui.label(egui::RichText::new(format!("{total} matching rows")).weak());
        if self.names.total() == 0 {
            ui.label(egui::RichText::new("Names not loaded — press “Refresh names” to pull them from the phone.").weak());
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("items_grid").num_columns(5).striped(true).spacing([12.0, 4.0]).show(ui, |ui| {
                ui.label(egui::RichText::new("Type").strong());
                ui.label(egui::RichText::new("Item ID").strong());
                ui.label(egui::RichText::new("Name").strong());
                ui.label(egui::RichText::new("Count").strong());
                ui.label("");
                ui.end_row();
                for row in rows.iter().take(1000) {
                    ui.label(row.item_type.map(|t| t.to_string()).unwrap_or_else(|| if row.section == 1 { "equip".to_string() } else { String::new() }));
                    ui.label(row.item_id.to_string());
                    ui.label(&row.name);
                    let mut count = row.count;
                    if ui.add(egui::DragValue::new(&mut count).speed(1.0).range(0..=INT64_MAX)).changed() {
                        edits.push((row.section, row.idx, count));
                    }
                    if ui.small_button("✕").clicked() {
                        deletes.push((row.section, row.idx));
                    }
                    ui.end_row();
                }
            });
        });
        if let Some(data) = self.data.as_mut() {
            for (section, idx, count) in edits {
                let array = if section == 0 {
                    data.get_mut("inventory").and_then(|v| v.get_mut("items"))
                } else {
                    data.get_mut("inventory").and_then(|v| v.get_mut("equipment"))
                };
                if let Some(arr) = array.and_then(|v| v.as_array_mut()) {
                    if let Some(entry) = arr.get_mut(idx) {
                        entry["count"] = Value::from(count);
                        self.dirty = true;
                    }
                }
            }
            deletes.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
            for (section, idx) in deletes {
                let array = if section == 0 {
                    data.get_mut("inventory").and_then(|v| v.get_mut("items"))
                } else {
                    data.get_mut("inventory").and_then(|v| v.get_mut("equipment"))
                };
                if let Some(arr) = array.and_then(|v| v.as_array_mut()) {
                    if idx < arr.len() {
                        arr.remove(idx);
                        self.dirty = true;
                    }
                }
            }
        }
    }

    fn collect_roster(&self) -> Vec<RosterRow> {
        let mut rows = Vec::new();
        let Some(data) = &self.data else { return rows };
        let needle = self.filter_roster.trim().to_lowercase();
        if let Some(roster) = data.get("roster").and_then(|v| v.as_array()) {
            for (idx, unit) in roster.iter().enumerate() {
                let instance = unit.get("instance_id").map(|v| v.to_string().trim_matches('"').to_string()).unwrap_or_default();
                let master = unit.get("master_id").map(|v| v.to_string().trim_matches('"').to_string()).unwrap_or_default();
                let master_id = master.parse::<i64>().unwrap_or(0);
                let name = self.names.unit(master_id).unwrap_or("—").to_string();
                if !needle.is_empty()
                    && !instance.to_lowercase().contains(&needle)
                    && !master.to_lowercase().contains(&needle)
                    && !name.to_lowercase().contains(&needle)
                {
                    continue;
                }
                rows.push(RosterRow {
                    idx,
                    instance,
                    master,
                    name,
                    level: unit.get("level").and_then(num_of).unwrap_or(0),
                    exp: unit.get("exp").and_then(num_of).unwrap_or(0),
                    lb_level: unit.get("lb_level").and_then(num_of).unwrap_or(0),
                    lb_exp: unit.get("lb_exp").and_then(num_of).unwrap_or(0),
                    ex_level: unit.get("nv_ex_level").and_then(num_of).unwrap_or(0),
                });
            }
        }
        rows
    }

    fn tab_roster(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Filter:");
            ui.add(egui::TextEdit::singleline(&mut self.filter_roster).desired_width(200.0));
            if ui.button("Add unit…").clicked() {
                self.unit_search.clear();
                self.unit_master.clear();
                self.unit_level = "1".to_string();
                self.unit_exp = "0".to_string();
                self.unit_favourite = false;
                self.show_add_unit = true;
            }
        });
        if self.data.is_none() {
            ui.label("Open a save first.");
            return;
        }
        let rows = self.collect_roster();
        let total_units = self.data.as_ref().and_then(|d| d.get("roster")).and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
        ui.label(egui::RichText::new(format!("{} / {} units", rows.len(), total_units)).weak());
        if self.names.total() == 0 {
            ui.label(egui::RichText::new("Names not loaded — press “Refresh names” to pull them from the phone.").weak());
        }
        let mut open_edit: Option<usize> = None;
        let mut open_clone: Option<usize> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("roster_grid").num_columns(10).striped(true).spacing([12.0, 4.0]).show(ui, |ui| {
                for h in ["instance_id", "master_id", "Name", "Lv", "EXP", "LB Lv", "LB EXP", "EX Lv", "", ""] {
                    ui.label(egui::RichText::new(h).strong());
                }
                ui.end_row();
                for row in rows.iter().take(1000) {
                    ui.label(&row.instance);
                    ui.label(&row.master);
                    ui.label(&row.name);
                    ui.label(row.level.to_string());
                    ui.label(row.exp.to_string());
                    ui.label(row.lb_level.to_string());
                    ui.label(row.lb_exp.to_string());
                    ui.label(row.ex_level.to_string());
                    if ui.small_button("Edit…").clicked() {
                        open_edit = Some(row.idx);
                    }
                    if ui.small_button("Clone").clicked() {
                        open_clone = Some(row.idx);
                    }
                    ui.end_row();
                }
            });
        });
        if let Some(idx) = open_clone {
            self.clone_idx = Some(idx);
            self.clone_gear = false;
        }
        if let Some(idx) = open_edit {
            if let Some(unit) = self.data.as_ref().and_then(|d| d.get("roster")).and_then(|v| v.as_array()).and_then(|a| a.get(idx)) {
                let fields = [
                    unit.get("level").and_then(num_of).unwrap_or(0).to_string(),
                    unit.get("exp").and_then(num_of).unwrap_or(0).to_string(),
                    unit.get("lb_level").and_then(num_of).unwrap_or(0).to_string(),
                    unit.get("lb_exp").and_then(num_of).unwrap_or(0).to_string(),
                    unit.get("nv_ex_level").and_then(num_of).unwrap_or(0).to_string(),
                ];
                self.unit_edit = Some(UnitEdit {
                    idx,
                    instance: rows.iter().find(|r| r.idx == idx).map(|r| r.instance.clone()).unwrap_or_default(),
                    master: rows.iter().find(|r| r.idx == idx).map(|r| r.master.clone()).unwrap_or_default(),
                    fields,
                });
            }
        }
    }

    fn tab_raw(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Apply raw JSON").clicked() {
                match serde_json::from_str::<Value>(&self.raw) {
                    Ok(value) if value.is_object() => {
                        self.data = Some(value);
                        self.dirty = true;
                        self.refresh_forms();
                        self.status = "Raw JSON applied (not saved yet).".to_string();
                    }
                    Ok(_) => self.status = "Top-level JSON must be an object.".to_string(),
                    Err(err) => self.status = format!("Invalid JSON: {err}"),
                }
            }
            ui.label(egui::RichText::new("Editing here replaces the whole file when you press Apply, then Save.").weak());
        });
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut self.raw)
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .desired_rows(30),
            );
        });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if self.show_settings {
            let mut open = true;
            egui::Window::new("Device settings").open(&mut open).collapsible(false).show(ctx, |ui| {
                egui::Grid::new("settings_grid").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                    ui.label("adb.exe path");
                    ui.add(egui::TextEdit::singleline(&mut self.adb_edit).desired_width(430.0));
                    ui.end_row();
                    ui.label("Package");
                    ui.add(egui::TextEdit::singleline(&mut self.pkg_edit).desired_width(260.0));
                    ui.end_row();
                });
                ui.horizontal(|ui| {
                    if ui.button("Browse adb…").clicked() {
                        if let Some(path) = rfd::FileDialog::new().add_filter("adb", &["exe"]).pick_file() {
                            self.adb_edit = path.display().to_string();
                        }
                    }
                    if ui.button("Save").clicked() {
                        if !valid_package(&self.pkg_edit.trim()) {
                            self.status = "That is not a valid Android package name.".to_string();
                        } else {
                            self.cfg.adb = Some(self.adb_edit.trim().to_string());
                            self.cfg.package = Some(self.pkg_edit.trim().to_string());
                            self.cfg.save();
                            self.status = "Device settings saved.".to_string();
                            self.show_settings = false;
                        }
                    }
                });
            });
            if !open {
                self.show_settings = false;
            }
        }

        if self.show_add {
            let mut open = true;
            let mut chosen: Option<NameHit> = None;
            egui::Window::new("Add inventory entry").open(&mut open).collapsible(false).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Find by name:");
                    ui.add(egui::TextEdit::singleline(&mut self.add_search).desired_width(260.0));
                });
                let hits = self.names.search(&self.add_search, 8);
                if !hits.is_empty() {
                    for hit in &hits {
                        if ui.selectable_label(false, format!("{}  —  {}  ({})", hit.name, hit.id, hit.kind)).clicked() {
                            chosen = Some(hit.clone());
                        }
                    }
                    ui.separator();
                }
                egui::ComboBox::from_label("Section")
                    .selected_text(if self.add_section == 0 { "item" } else { "equipment" })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.add_section, 0, "item");
                        ui.selectable_value(&mut self.add_section, 1, "equipment");
                    });
                egui::Grid::new("add_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("item_type (items only)");
                    ui.add(egui::TextEdit::singleline(&mut self.add_type).desired_width(160.0));
                    ui.end_row();
                    ui.label("item_id");
                    ui.add(egui::TextEdit::singleline(&mut self.add_id).desired_width(160.0));
                    ui.end_row();
                    ui.label("count");
                    ui.add(egui::TextEdit::singleline(&mut self.add_count).desired_width(160.0));
                    ui.end_row();
                });
                ui.horizontal(|ui| {
                    if ui.button("Add").clicked() {
                        match (parse_i64(&self.add_id, "item_id"), parse_i64(&self.add_count, "count"), parse_i64(&self.add_type, "item_type")) {
                            (Ok(id), Ok(count), Ok(item_type)) if id > 0 && count >= 0 => {
                                self.add_entry(item_type, id, count);
                                self.show_add = false;
                            }
                            (Err(err), _, _) | (_, Err(err), _) | (_, _, Err(err)) => self.status = err,
                            _ => self.status = "item_id must be > 0 and count >= 0.".to_string(),
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        self.show_add = false;
                    }
                });
            });
            if let Some(hit) = chosen {
                self.add_id = hit.id.to_string();
                match hit.kind.as_str() {
                    "equipment" => {
                        self.add_section = 1;
                    }
                    "materia" => {
                        self.add_section = 0;
                        self.add_type = "22".to_string();
                    }
                    _ => {
                        self.add_section = 0;
                        self.add_type = "20".to_string();
                    }
                }
                self.status = format!("Selected {} ({}). Fill in the count and press Add.", hit.name, hit.id);
            }
            if !open {
                self.show_add = false;
            }
        }

        if self.show_add_unit {
            let mut open = true;
            let mut submit = false;
            egui::Window::new("Add unit").open(&mut open).collapsible(false).default_width(520.0).show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Find by name:");
                    ui.add(egui::TextEdit::singleline(&mut self.unit_search).desired_width(280.0));
                });
                let hits = self.names.search_units(&self.unit_search, 8);
                if !hits.is_empty() {
                    for hit in &hits {
                        if ui.selectable_label(false, format!("{}  —  {}", hit.name, hit.id)).clicked() {
                            self.unit_master = hit.id.to_string();
                        }
                    }
                    ui.separator();
                }
                egui::Grid::new("add_unit_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("master_id");
                    ui.add(egui::TextEdit::singleline(&mut self.unit_master).desired_width(180.0));
                    ui.end_row();
                    ui.label("level");
                    ui.add(egui::TextEdit::singleline(&mut self.unit_level).desired_width(180.0));
                    ui.end_row();
                    ui.label("exp");
                    ui.add(egui::TextEdit::singleline(&mut self.unit_exp).desired_width(180.0));
                    ui.end_row();
                });
                ui.checkbox(&mut self.unit_favourite, "favourite (stops material units being auto-converted)");
                ui.label(egui::RichText::new("The new unit is appended with a fresh instance_id and counts.unit is updated.").weak());
                ui.horizontal(|ui| {
                    if ui.button("Add unit").clicked() {
                        submit = true;
                    }
                    if ui.button("Cancel").clicked() {
                        self.show_add_unit = false;
                    }
                });
            });
            if submit {
                match (
                    parse_i64(&self.unit_master, "master_id"),
                    parse_i64(&self.unit_level, "level"),
                    parse_i64(&self.unit_exp, "exp"),
                ) {
                    (Ok(master), Ok(level), Ok(exp)) if master > 0 && level >= 1 && exp >= 0 => {
                        self.add_unit(master, level, exp, self.unit_favourite);
                        self.show_add_unit = false;
                    }
                    (Err(err), _, _) | (_, Err(err), _) | (_, _, Err(err)) => self.status = err,
                    _ => self.status = "master_id must be > 0, level >= 1, exp >= 0.".to_string(),
                }
            }
            if !open {
                self.show_add_unit = false;
            }
        }

        if let Some(mut edit) = self.unit_edit.clone() {
            let mut open = true;
            let mut apply = false;
            egui::Window::new(format!("Unit {} (master {})", edit.instance, edit.master))
                .open(&mut open)
                .collapsible(false)
                .show(ctx, |ui| {
                    let labels = ["Level", "EXP", "LB level", "LB EXP", "EX level"];
                    egui::Grid::new("unit_grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                        for (i, label) in labels.iter().enumerate() {
                            ui.label(*label);
                            ui.add(egui::TextEdit::singleline(&mut edit.fields[i]).desired_width(200.0));
                            ui.end_row();
                        }
                    });
                    if ui.button("Apply").clicked() {
                        apply = true;
                    }
                });
            if apply {
                let mut parsed = [0i64; 5];
                let keys = ["level", "exp", "lb_level", "lb_exp", "nv_ex_level"];
                let mut ok = true;
                for i in 0..5 {
                    match parse_i64(&edit.fields[i], keys[i]) {
                        Ok(v) => parsed[i] = v,
                        Err(err) => {
                            self.status = err;
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    if let Some(roster) = self.data.as_mut().and_then(|d| d.get_mut("roster")).and_then(|v| v.as_array_mut()) {
                        if let Some(unit) = roster.get_mut(edit.idx) {
                            for i in 0..5 {
                                unit[keys[i]] = Value::from(parsed[i]);
                            }
                            self.dirty = true;
                            self.status = "Unit updated (not saved yet).".to_string();
                        }
                    }
                    self.unit_edit = None;
                }
            } else if !open {
                self.unit_edit = None;
            } else {
                self.unit_edit = Some(edit);
            }
        }

        if self.show_apk {
            let mut open = true;
            let mut compare = false;
            let info = self.apk.clone();
            let phone = self.phone.clone();
            egui::Window::new("APK check").open(&mut open).collapsible(false).default_width(760.0).show(ctx, |ui| {
                let Some(info) = &info else {
                    ui.label("No APK loaded.");
                    return;
                };
                let file_name = Path::new(&info.path)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                ui.label(egui::RichText::new(file_name).strong());
                ui.add_space(4.0);
                egui::Grid::new("apk_grid").num_columns(2).spacing([14.0, 4.0]).show(ui, |ui| {
                    kv(ui, "Size", format!("{} bytes", info.size));
                    kv(ui, "SHA-256", if info.sha256.is_empty() { "—".to_string() } else { info.sha256.clone() });
                    kv(ui, "Package", if info.package.is_empty() { "—".to_string() } else { info.package.clone() });
                    kv(ui, "Version", format!("{}  (code {})", info.version_name, info.version_code));
                    kv(ui, "minSdk / targetSdk", format!("{} / {}", info.min_sdk, info.target_sdk));
                    ui.label(egui::RichText::new("Debuggable").weak());
                    if info.debuggable {
                        ui.label(egui::RichText::new("yes — run-as / save editing will work").color(GREEN));
                    } else {
                        ui.label(egui::RichText::new("no — run-as (save editing) will fail").color(AMBER));
                    }
                    ui.end_row();
                    kv(ui, "Signer", if info.signer.is_empty() { "—".to_string() } else { info.signer.clone() });
                    kv(ui, "Cert SHA-256", if info.signer_sha256.is_empty() { "—".to_string() } else { info.signer_sha256.clone() });
                });
                if let Some(err) = &info.error {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(err).color(RED));
                }
                ui.add_space(8.0);
                ui.separator();
                match &phone {
                    None => {
                        ui.label(egui::RichText::new("Not compared with the phone yet.").weak());
                    }
                    Some(phone) => {
                        ui.label(egui::RichText::new("Installed on phone").strong());
                        let mut rows: Vec<(String, egui::Color32)> = Vec::new();
                        match &phone.error {
                            Some(err) => rows.push((err.clone(), AMBER)),
                            None => {
                                let pkg_ok = !info.package.is_empty() && info.package == phone.package;
                                rows.push((
                                    format!("package {} vs installed {}", info.package, phone.package),
                                    if pkg_ok { GREEN } else { RED },
                                ));
                                let version_ok = info.version_code >= phone.version_code;
                                rows.push((
                                    format!("versionCode {} vs installed {}", info.version_code, phone.version_code),
                                    if version_ok { GREEN } else { RED },
                                ));
                                let signer_ok = !info.signer.is_empty()
                                    && !phone.signer.is_empty()
                                    && info.signer.eq_ignore_ascii_case(&phone.signer);
                                let signer_text = format!(
                                    "signer {} vs installed {}",
                                    if info.signer.is_empty() { "?" } else { &info.signer },
                                    if phone.signer.is_empty() { "?" } else { &phone.signer }
                                );
                                rows.push((signer_text, if signer_ok { GREEN } else { RED }));
                            }
                        }
                        for (text, color) in &rows {
                            ui.label(egui::RichText::new(text).color(*color));
                        }
                        if phone.error.is_none() {
                            let all_ok = rows.iter().all(|(_, c)| *c == GREEN);
                            ui.add_space(4.0);
                            if all_ok && info.debuggable {
                                ui.label(
                                    egui::RichText::new("Installable over the current build — data (save + packs) will be preserved.")
                                        .color(GREEN)
                                        .strong(),
                                );
                            } else if all_ok {
                                ui.label(
                                    egui::RichText::new("Installable, but not debuggable — the save editor's run-as access will stop working.")
                                        .color(AMBER)
                                        .strong(),
                                );
                            } else {
                                ui.label(
                                    egui::RichText::new("Cannot install over the current build — an uninstall (save/pack loss) would be required.")
                                        .color(RED)
                                        .strong(),
                                );
                            }
                        }
                    }
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Compare with phone").clicked() {
                        compare = true;
                    }
                    if ui.button("Close").clicked() {
                        compare = false;
                    }
                });
            });
            if compare {
                self.status = "Comparing with the phone…".to_string();
                let adb = self.adb();
                let pkg = self.pkg();
                self.phone = Some(inspect_phone(&adb, &pkg));
                self.status = "APK compared with the phone.".to_string();
            }
            if !open {
                self.show_apk = false;
            }
        }

        if let Some(idx) = self.clone_idx {
            let source_info = self
                .data
                .as_ref()
                .and_then(|d| d.get("roster"))
                .and_then(|v| v.as_array())
                .and_then(|a| a.get(idx))
                .map(|u| {
                    let master = u
                        .get("master_id")
                        .map(|v| v.to_string().trim_matches('"').to_string())
                        .unwrap_or_default();
                    let level = u.get("level").and_then(num_of).unwrap_or(0);
                    (master, level)
                });
            let mut open = true;
            let mut submit = false;
            egui::Window::new("Clone unit").open(&mut open).collapsible(false).default_width(520.0).show(ctx, |ui| {
                if let Some((master, level)) = &source_info {
                    let name = master.parse::<i64>().ok().and_then(|id| self.names.unit(id)).unwrap_or("—");
                    ui.label(format!("Source: {name}   (master {master}, Lv {level})"));
                    ui.label(egui::RichText::new("Level, EXP, LB and EX state are copied as-is; a fresh instance_id is assigned.").weak());
                }
                ui.checkbox(&mut self.clone_gear, "copy equipment, materia and vision card");
                ui.label(egui::RichText::new("Off by default — a clone with the same gear would point at the same inventory items.").weak());
                ui.horizontal(|ui| {
                    if ui.button("Clone").clicked() {
                        submit = true;
                    }
                    if ui.button("Cancel").clicked() {
                        self.clone_idx = None;
                    }
                });
            });
            if submit {
                self.clone_unit(idx, self.clone_gear);
                self.clone_idx = None;
            } else if !open {
                self.clone_idx = None;
            }
        }

        if self.show_about {
            let mut open = true;
            egui::Window::new("About").open(&mut open).collapsible(false).show(ctx, |ui| {
                ui.label("Exvius Save Editor (Rust)");
                ui.label("Edits campaign.json for Exvius Offline.");
                ui.label("Every save writes a timestamped .bak next to the file.");
                ui.label("Phone buttons use adb + run-as (debuggable build required).");
            });
            if !open {
                self.show_about = false;
            }
        }
    }

    fn add_entry(&mut self, item_type: i64, item_id: i64, count: i64) {
        let Some(data) = self.data.as_mut() else { return };
        if !data.is_object() {
            return;
        }
        if data.get("inventory").is_none() {
            data["inventory"] = Value::Object(Map::new());
        }
        if add_inventory(data, self.add_section as u8, item_type, item_id, count) {
            self.dirty = true;
            self.status = format!(
                "Added {count} × {} {} (not saved yet).",
                if self.add_section == 1 { "equipment" } else { "item" },
                item_id
            );
        }
    }

    fn add_unit(&mut self, master_id: i64, level: i64, exp: i64, favourite: bool) {
        let Some(data) = self.data.as_mut() else { return };
        let Some(roster) = data.get_mut("roster").and_then(|v| v.as_array_mut()) else { return };
        let instance_id = next_instance_id(roster);
        let template = roster.first().cloned().unwrap_or(Value::Null);
        let entry = make_unit_entry(&template, instance_id, master_id, level, exp, favourite);
        roster.push(entry);
        let count = roster.len();
        if let Some(counts) = data.get_mut("counts").and_then(|v| v.as_object_mut()) {
            counts.insert("unit".to_string(), Value::from(count as i64));
        }
        self.dirty = true;
        self.status = format!("Added unit {master_id} (instance {instance_id}); counts.unit = {count}. Not saved yet.");
    }

    fn clone_unit(&mut self, idx: usize, copy_gear: bool) {
        let Some(data) = self.data.as_mut() else { return };
        let Some(roster) = data.get_mut("roster").and_then(|v| v.as_array_mut()) else { return };
        let Some(source) = roster.get(idx).cloned() else { return };
        let instance_id = next_instance_id(roster);
        let entry = clone_roster_entry(&source, instance_id, copy_gear);
        roster.push(entry);
        let count = roster.len();
        if let Some(counts) = data.get_mut("counts").and_then(|v| v.as_object_mut()) {
            counts.insert("unit".to_string(), Value::from(count as i64));
        }
        self.dirty = true;
        self.status = format!(
            "Cloned unit to instance {instance_id}{}; counts.unit = {count}. Not saved yet.",
            if copy_gear { " (gear copied)" } else { "" }
        );
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let title = match &self.path {
            Some(p) => format!("{APP_TITLE} — {}", p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default()),
            None => APP_TITLE.to_string(),
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
        self.top_bar(ctx);
        self.status_bar(ctx);
        egui::CentralPanel::default().show(ctx, |ui| self.tabs(ui));
        self.dialogs(ctx);
    }
}

// ------------------------------------------------------------------ self-test

fn run_selftest(path: &str) -> i32 {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("selftest: cannot read {path}: {e}");
            return 2;
        }
    };
    let data: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("selftest: invalid JSON: {e}");
            return 2;
        }
    };
    let roster = data.get("roster").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    let equipment = data
        .get("inventory")
        .and_then(|v| v.get("equipment"))
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    println!("selftest: roster = {roster}, equipment = {equipment}");

    if let Some(names) = Names::load() {
        let roster_named = data
            .get("roster")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter(|u| u.get("master_id").and_then(num_like).and_then(|id| names.unit(id)).is_some())
                    .count()
            })
            .unwrap_or(0);
        let equipment_named = data
            .get("inventory")
            .and_then(|v| v.get("equipment"))
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter(|e| e.get("item_id").and_then(num_of).and_then(|id| names.equipment(id)).is_some())
                    .count()
            })
            .unwrap_or(0);
        let items_named = data
            .get("inventory")
            .and_then(|v| v.get("items"))
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter(|e| {
                        let id = e.get("item_id").and_then(num_of).unwrap_or(0);
                        let ty = e.get("item_type").and_then(num_of);
                        names.item(id, ty).is_some()
                    })
                    .count()
            })
            .unwrap_or(0);
        println!("selftest: names cached = {}", names.total());
        println!("selftest: named roster = {roster_named}/{roster}, equipment = {equipment_named}/{equipment}, items = {items_named}");
    } else {
        println!("selftest: no name cache found");
    }

    let tmp = std::env::temp_dir().join(format!("exvius_selftest_{}.json", now_secs()));
    let pretty = serde_json::to_string_pretty(&data).unwrap();
    if let Err(e) = fs::write(&tmp, pretty) {
        eprintln!("selftest: write failed: {e}");
        return 2;
    }
    let again: Value = match fs::read_to_string(&tmp).ok().and_then(|t| serde_json::from_str(&t).ok()) {
        Some(v) => v,
        None => {
            eprintln!("selftest: reload failed");
            return 2;
        }
    };
    let roster2 = again.get("roster").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    let gil = again.get("currencies").and_then(|c| c.get("gil")).and_then(num_of).unwrap_or(0);
    println!("selftest: roundtrip roster = {roster2}, gil = {gil}, bytes = {}", fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0));
    let _ = fs::remove_file(&tmp);
    println!("selftest: OK");
    0
}

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--check-apk") {
        let path = args.get(pos + 1).cloned().unwrap_or_default();
        let info = inspect_apk(Path::new(&path));
        println!("file        : {}", info.path);
        println!("size        : {} bytes", info.size);
        println!("sha256      : {}", info.sha256);
        println!("package     : {}", info.package);
        println!("version     : {} (code {})", info.version_name, info.version_code);
        println!("sdk         : min {} target {}", info.min_sdk, info.target_sdk);
        println!("debuggable  : {}", info.debuggable);
        println!("signer      : {}", if info.signer.is_empty() { "?" } else { &info.signer });
        println!("cert sha256 : {}", if info.signer_sha256.is_empty() { "?" } else { &info.signer_sha256 });
        if let Some(err) = &info.error {
            println!("error       : {err}");
        }
        if args.iter().any(|a| a == "--compare") {
            let phone = inspect_phone(DEFAULT_ADB, DEFAULT_PACKAGE);
            println!("--- phone ---");
            match &phone.error {
                Some(err) => println!("phone error : {err}"),
                None => {
                    println!("installed   : {} (code {})", phone.package, phone.version_code);
                    println!("signer      : {}", if phone.signer.is_empty() { "?" } else { &phone.signer });
                    let pkg_ok = info.package == phone.package;
                    let version_ok = info.version_code >= phone.version_code;
                    let signer_ok = !info.signer.is_empty()
                        && !phone.signer.is_empty()
                        && info.signer.eq_ignore_ascii_case(&phone.signer);
                    println!("package ok  : {pkg_ok}");
                    println!("version ok  : {version_ok}");
                    println!("signer ok   : {signer_ok}");
                    println!(
                        "verdict     : {}",
                        if pkg_ok && version_ok && signer_ok {
                            "installs over the current build (data preserved)"
                        } else {
                            "would be refused — uninstall/restore required"
                        }
                    );
                }
            }
        }
        std::process::exit(0);
    }
    if let Some(pos) = args.iter().position(|a| a == "--dryrun-add") {
        let path = args.get(pos + 1).cloned().unwrap_or_default();
        let text = fs::read_to_string(&path).unwrap_or_default();
        let mut data: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !data.is_object() {
            eprintln!("dryrun: could not parse {path}");
            std::process::exit(2);
        }
        let before_units = data.get("roster").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
        let before_items = data
            .get("inventory")
            .and_then(|v| v.get("items"))
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let added_unit = {
            let roster = data.get_mut("roster").and_then(|v| v.as_array_mut());
            match roster {
                Some(roster) => {
                    let id = next_instance_id(roster);
                    let template = roster.first().cloned().unwrap_or(Value::Null);
                    roster.push(make_unit_entry(&template, id, 906000105, 1, 0, true));
                    let count = roster.len();
                    if let Some(counts) = data.get_mut("counts").and_then(|v| v.as_object_mut()) {
                        counts.insert("unit".to_string(), Value::from(count as i64));
                    }
                    Some(id)
                }
                None => None,
            }
        };
        let added_item = add_inventory(&mut data, 0, 20, 290050300, 999);
        let cloned = {
            let roster = data.get_mut("roster").and_then(|v| v.as_array_mut());
            match roster {
                Some(roster) if !roster.is_empty() => {
                    let source = roster[0].clone();
                    let id = next_instance_id(roster);
                    let entry = clone_roster_entry(&source, id, false);
                    let gear_cleared = entry
                        .get("equipment")
                        .and_then(|e| e.get("1"))
                        .map(|v| v == "0")
                        .unwrap_or(false);
                    roster.push(entry);
                    let count = roster.len();
                    if let Some(counts) = data.get_mut("counts").and_then(|v| v.as_object_mut()) {
                        counts.insert("unit".to_string(), Value::from(count as i64));
                    }
                    Some((id, gear_cleared))
                }
                _ => None,
            }
        };
        let after_units = data.get("roster").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
        let after_items = data
            .get("inventory")
            .and_then(|v| v.get("items"))
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let counts_unit = data.get("counts").and_then(|c| c.get("unit")).and_then(num_of).unwrap_or(-1);
        println!("dryrun: units {before_units} -> {after_units} (new instance {added_unit:?}), counts.unit={counts_unit}");
        println!("dryrun: cloned -> {cloned:?}");
        println!("dryrun: item stacks {before_items} -> {after_items} (added={added_item})");
        println!("dryrun: json valid = {}", serde_json::to_string(&data).is_ok());
        std::process::exit(0);
    }
    if let Some(pos) = args.iter().position(|a| a == "--selftest") {
        let path = args.get(pos + 1).cloned().unwrap_or_default();
        std::process::exit(run_selftest(&path));
    }

    let apk_initial = args
        .iter()
        .position(|a| a == "--show-apk")
        .and_then(|pos| args.get(pos + 1))
        .cloned();
    let initial = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--") && apk_initial.as_deref() != Some(a.as_str()))
        .cloned();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1150.0, 800.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        APP_TITLE,
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, initial.clone(), apk_initial.clone())))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_validation_accepts_real_package() {
        assert!(valid_package("com.exvius.offline.test"));
        assert!(valid_package("a.b"));
        assert!(valid_package("com.a_b.c1"));
    }

    #[test]
    fn package_validation_rejects_injection() {
        for bad in [
            "com.exvius.offline.test; rm -rf /sdcard/x",
            "com.exvius.offline.test`id`",
            "com.exvius.offline.test$(id)",
            "com.exvius.offline.test\nam start",
            "",
            "nodots",
            ".leading",
            "trailing.",
            "com..double",
            "com.1digit",
        ] {
            assert!(!valid_package(bad), "should reject {bad:?}");
        }
    }

    #[test]
    fn parse_numbers_handles_separators_and_hex() {
        assert_eq!(parse_i64("1,234", "x").unwrap(), 1234);
        assert_eq!(parse_i64("1_000", "x").unwrap(), 1000);
        assert_eq!(parse_i64("0x10", "x").unwrap(), 16);
        assert_eq!(parse_i64("9223372036854775807", "x").unwrap(), i64::MAX);
        assert!(parse_i64("", "x").is_err());
        assert!(parse_i64("abc", "x").is_err());
        assert!(parse_i64("99999999999999999999", "x").is_err());
    }

    #[test]
    fn next_instance_id_handles_strings_and_numbers() {
        let roster = vec![
            serde_json::json!({"instance_id": "10"}),
            serde_json::json!({"instance_id": 20}),
            serde_json::json!({"instance_id": "15"}),
        ];
        assert_eq!(next_instance_id(&roster), 21);
        assert_eq!(next_instance_id(&[]), 1);
    }

    #[test]
    fn make_unit_entry_sets_new_unit_fields() {
        let template = serde_json::json!({
            "instance_id": "1", "master_id": "100", "level": 50, "exp": 10,
            "equipment": {"1": "0"}, "materia": ["0"], "favourite": 0, "lb_level": 7
        });
        let entry = make_unit_entry(&template, 42, 906000105, 1, 0, true);
        assert_eq!(entry["instance_id"], "42");
        assert_eq!(entry["master_id"], "906000105");
        assert_eq!(entry["level"], 1);
        assert_eq!(entry["exp"], 0);
        assert_eq!(entry["lb_level"], 1);
        assert_eq!(entry["favourite"], 1);
        assert!(entry["granted_rarity"].is_null());
        assert_eq!(entry["equipment"]["1"], "0");
    }

    #[test]
    fn add_inventory_appends_and_merges() {
        let mut data = serde_json::json!({
            "inventory": {
                "items": [{"item_type": 20, "item_id": 1, "count": 5}],
                "equipment": [{"item_id": 7, "count": 1}]
            }
        });
        assert!(add_inventory(&mut data, 0, 20, 1, 2));
        assert_eq!(data["inventory"]["items"][0]["count"], 7);
        assert!(add_inventory(&mut data, 0, 22, 2, 3));
        assert_eq!(data["inventory"]["items"].as_array().unwrap().len(), 2);
        assert_eq!(data["inventory"]["items"][1]["item_type"], 22);
        assert!(add_inventory(&mut data, 1, 0, 7, 4));
        assert_eq!(data["inventory"]["equipment"][0]["count"], 5);
        assert!(add_inventory(&mut data, 1, 0, 8, 1));
        assert_eq!(data["inventory"]["equipment"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn name_search_matches_and_sorts() {
        let mut names = Names::default();
        names.items.insert(1, "Potion".into());
        names.items.insert(2, "Hi-Potion".into());
        names.items.insert(3, "Ether".into());
        names.equipment.insert(10, "Excalibur".into());
        names.units.insert(100, "Rain".into());
        names.units.insert(101, "Lasswell".into());

        let hits = names.search("potion", 8);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].name, "Potion");
        assert!(hits.iter().any(|h| h.kind == "item"));

        let hits = names.search("excal", 8);
        assert_eq!(hits[0].kind, "equipment");
        assert_eq!(hits[0].id, 10);

        assert!(names.search("a", 8).is_empty(), "one letter must not match");
        let units = names.search_units("las", 8);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].id, 101);
    }

    #[test]
    fn clone_entry_keeps_stats_and_optionally_gear() {
        let source = serde_json::json!({
            "instance_id": "5", "master_id": "100", "level": 120, "exp": 99,
            "equipment": {"1": "302000100"}, "materia": ["504243486"],
            "vision_card": "1", "skin_id": 3, "guest": 1
        });
        let clone = clone_roster_entry(&source, 999, false);
        assert_eq!(clone["instance_id"], "999");
        assert_eq!(clone["master_id"], "100");
        assert_eq!(clone["level"], 120);
        assert_eq!(clone["equipment"]["1"], "0");
        assert_eq!(clone["materia"][0], "0");
        assert_eq!(clone["vision_card"], "");
        assert_eq!(clone["guest"], 0);
        assert_eq!(clone["skin_id"], 0);

        let keep = clone_roster_entry(&source, 1000, true);
        assert_eq!(keep["equipment"]["1"], "302000100");
        assert_eq!(keep["materia"][0], "504243486");
        assert_eq!(keep["skin_id"], 3);
    }

    #[test]
    fn json_helpers_set_and_read_nested() {
        let mut value = serde_json::json!({"a": {"b": 1}});
        set_num(&mut value, &["a", "c"], 7);
        assert_eq!(get_num(&value, &["a", "c"]), 7);
        set_num(&mut value, &["x", "y", "z"], 9);
        assert_eq!(get_num(&value, &["x", "y", "z"]), 9);
        assert_eq!(get_num(&value, &["missing"]), 0);
    }
}
