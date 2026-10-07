// Prevent console window in addition to Slint window in Windows release builds when, e.g., starting the app via file manager. Ignored on other platforms.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{error::Error, io::BufReader};
use std::fs;
use std::fs::File;
use std::sync::{Arc, Mutex, OnceLock};
use std::io::{BufRead, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use lyceris::{http::fetch::fetch, install, json::version::manifest::{Type, VersionManifest}, launch, minecraft::{config::{ConfigBuilder, Profile}, loader::Loader}};
use lyceris::minecraft::config::Memory;
use slint::{Image, ModelRc, VecModel, Weak};
use std::rc::Rc;
use tokio::runtime::Builder;
use slint::SharedString;
use slint::Model;
use std::collections::HashMap;
use lyceris::minecraft::loader::{
    fabric::Fabric, forge::Forge, neoforge::NeoForge, quilt::Quilt,
};
use lyceris::minecraft::emitter::{Emitter, Event};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader as TokioBufReader};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

slint::include_modules!();

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ModpackJsonData {
    name: String,
    minecraft_version: String,
    loader: String,
    ram_mb: i32,
    #[serde(default)]
    icon_path: Option<String>,
    #[serde(default)]
    loader_version: Option<String>,
    // NEU: Herkunft der Instanz (für Modpack-Update). Ältere Instanzen haben None.
    #[serde(default)]
    origin_source: Option<String>,      // "Modrinth" | "CurseForge"
    #[serde(default)]
    origin_project_id: Option<String>,
    #[serde(default)]
    origin_version_id: Option<String>,
}

#[derive(Deserialize)]
struct LoaderEntry {
    version: String,
    #[serde(default)]
    stable: bool,
}

// Struct für die Settings, wird 1:1 als settings.json gespeichert/geladen
// (gleiches Pattern wie ModpackJsonData für instance.json)
#[derive(Debug, Serialize, Deserialize)]
struct LauncherSettings {
    default_ram_mb: i32,
    java_path: String,
    default_username: String,
    close_after_launch: bool,
    open_on_startup: bool,
    show_snapshots: bool,
    #[serde(default = "default_items_per_page")]
    items_per_page: i32,
    // "auto" | "low" | "mid" | "high" — steuert Icon-Größe, Concurrency, Log-Puffer, etc.
    #[serde(default = "default_device_tier")]
    device_tier: String,
}

fn default_device_tier() -> String { "auto".to_string() }

fn default_items_per_page() -> i32 { 12 }

// Lädt settings.json aus dem Launcher-Ordner. Fehlt die Datei oder ist sie kaputt,
// kommen die Defaults zurück, damit ein Spielstart nie daran scheitert.
fn load_settings(launcher_dir: &Path) -> LauncherSettings {
    let path = launcher_dir.join("settings.json");
    let Ok(file) = File::open(&path) else {
        return LauncherSettings::default();
    };
    serde_json::from_reader(BufReader::new(file)).unwrap_or_default()
}

// Default-Werte falls noch keine settings.json existiert (erster Start)
impl Default for LauncherSettings {
    fn default() -> Self {
        Self {
            default_ram_mb: 2048,
            java_path: String::new(),
            default_username: "TestUser".to_string(),
            close_after_launch: false,
            open_on_startup: false,
            show_snapshots: false,
            items_per_page: 12,
            device_tier: default_device_tier(),
        }
    }
}

// ===================== Device-Tier-Erkennung =====================
// Erkennt anhand von CPU-Kernen und Gesamt-RAM, welche Klasse Rechner
// vorliegt. Wird zur Laufzeit nur einmal ermittelt, danach global gecacht.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceTier { Low, Mid, High }

// AtomicU8 damit wir das Tier zur Laufzeit ändern können (OnceLock geht nur einmal).
// Wert: 0=Low, 1=Mid, 2=High, 255=noch nicht gesetzt.
static DEVICE_TIER: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(255);

fn detect_device_tier() -> DeviceTier {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    let total_ram_gb = sys.total_memory() / (1024 * 1024 * 1024);

    if cores <= 2 || total_ram_gb <= 6 {
        DeviceTier::Low
    } else if cores >= 8 && total_ram_gb >= 16 {
        DeviceTier::High
    } else {
        DeviceTier::Mid
    }
}

/// Liefert das aktive Tier. Wenn noch nichts gesetzt wurde, wird auto-erkannt.
fn device_tier() -> DeviceTier {
    match DEVICE_TIER.load(AtomicOrdering::Relaxed) {
        0 => DeviceTier::Low,
        1 => DeviceTier::Mid,
        2 => DeviceTier::High,
        _ => {
            let t = detect_device_tier();
            set_device_tier(t);
            t
        }
    }
}

/// Setzt das Tier zur Laufzeit (überschreibbar, siehe AtomicU8).
fn set_device_tier(t: DeviceTier) {
    let v = match t {
        DeviceTier::Low => 0u8,
        DeviceTier::Mid => 1u8,
        DeviceTier::High => 2u8,
    };
    DEVICE_TIER.store(v, AtomicOrdering::Relaxed);
}

/// Leert alle In-Memory-Caches (Instanz-Cache + UI-Image-Cache + Modrinth-Caches).
fn clear_all_caches() {
    if let Ok(mut c) = instance_cache().lock() { c.clear(); }
    UI_IMAGE_CACHE.with(|c| c.borrow_mut().clear());
    if let Some(m) = MODRINTH_VERSIONS_CACHE.get() { if let Ok(mut g) = m.lock() { g.clear(); } }
    if let Some(m) = MODRINTH_PROJECT_CACHE.get() { if let Ok(mut g) = m.lock() { g.clear(); } }
}

fn parse_tier_choice(choice: &str) -> Option<DeviceTier> {
    match choice.to_lowercase().as_str() {
        "low"  => Some(DeviceTier::Low),
        "mid"  => Some(DeviceTier::Mid),
        "high" => Some(DeviceTier::High),
        _ => None, // "auto" oder unbekannt
    }
}

// Tier-abhängige Parameter
fn tier_icon_size(t: DeviceTier) -> u32 {
    match t { DeviceTier::Low => 64, DeviceTier::Mid => 96, DeviceTier::High => 128 }
}
fn tier_log_lines(t: DeviceTier) -> usize {
    match t { DeviceTier::Low => 200, DeviceTier::Mid => 500, DeviceTier::High => 1000 }
}
fn tier_parallel_workers(t: DeviceTier) -> usize {
    match t { DeviceTier::Low => 2, DeviceTier::Mid => 4, DeviceTier::High => 6 }
}
fn tier_download_concurrency(t: DeviceTier) -> usize {
    match t { DeviceTier::Low => 4, DeviceTier::Mid => 8, DeviceTier::High => 16 }
}
fn tier_decode_concurrency(t: DeviceTier) -> usize {
    match t { DeviceTier::Low => 1, DeviceTier::Mid => 2, DeviceTier::High => 4 }
}

// Instanz-eigene Overrides (RAM/Java/Username), optional aktivierbar, wird als
// instance_overrides.json direkt im jeweiligen Instanz-Ordner abgelegt.
// TODO: java_path wird gespeichert, aber noch nicht in den ConfigBuilder beim
// Start eingespeist (das war schon bei den globalen Settings nicht verdrahtet).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstanceOverrides {
    enabled: bool,
    ram_mb: i32,
    java_path: String,       // "Launcher Standard" oder konkreter Pfad
    account_id: String,      // "Launcher Standard" oder die ID des Accounts
    window_width: i32,
    window_height: i32,
    fullscreen: bool,
}

impl Default for InstanceOverrides {
    fn default() -> Self {
        Self {
            enabled: false,
            ram_mb: 2048,
            java_path: "Launcher Standard".to_string(),
            account_id: "Launcher Standard".to_string(),
            window_width: 854,
            window_height: 480,
            fullscreen: false,
        }
    }
}

// Ein gespeicherter Account (Offline oder Microsoft), wird in accounts.json
// im launcherDir abgelegt.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct AccountEntry {
    id: String,
    kind: String, // "offline" | "microsoft"
    username: String,
    // Nur bei Microsoft-Accounts gefüllt:
    #[serde(default)] uuid: Option<String>,
    #[serde(default)] xuid: Option<String>,
    #[serde(default)] access_token: Option<String>,
    #[serde(default)] refresh_token: Option<String>,
    #[serde(default)] token_exp: Option<u64>, // Ablaufzeit des Zugriffstokens (Unix-Sekunden)
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct AccountsData {
    accounts: Vec<AccountEntry>,
    selected_id: Option<String>,
}

// Response-Struct für die Modrinth Such-API (https://api.modrinth.com/v2/search)
// nur die Felder die wir für die BrowserTile brauchen. Alles hier drin sind
// plain Strings (Send), damit das problemlos über handle.spawn wandern kann.
#[derive(Deserialize)]
struct ModrinthHit {
    project_id: String,
    title: String,
    description: String,
    author: String,
    icon_url: Option<String>,
}

#[derive(Deserialize)]
struct ModrinthSearchResponse {
    hits: Vec<ModrinthHit>,
}

const CF_API_KEY: &str = "$2a$10$bL4bIL5pUWqfcO7KQtnMReakwtfHbNKh6v1uTpKlzhwoueEJQnPnm";

#[derive(Deserialize)]
struct CfLogo { thumbnailUrl: String }

// NEU: Hilfs-Struct für die Autoren-Liste von CurseForge
#[derive(Deserialize)]
struct CfAuthor {
    name: String,
}

#[derive(Deserialize)]
struct CfSearchHit {
    id: i64,
    name: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    authors: Vec<CfAuthor>, // <-- GEÄNDERT: CurseForge liefert ein Array von Autoren
    #[serde(default)]
    logo: Option<CfLogo>,
}

#[derive(Deserialize)]
struct CfSearchResponse {
    data: Vec<CfSearchHit>,
}

#[derive(Debug, Clone, Deserialize)]
struct CfFile {
    id: i64,
    fileName: String,
    downloadUrl: String,
    gameVersions: Vec<String>,
}

#[derive(Deserialize)]
struct CfFilesResponse {
    data: Vec<CfFile>,
}

#[derive(Deserialize)]
struct CfManifest {
    name: String,
    minecraft: CfMinecraft,
    files: Vec<CfManifestFile>,
}
#[derive(Deserialize)]
struct CfMinecraft { version: String, modLoaders: Vec<CfLoader> }
#[derive(Deserialize)]
struct CfLoader { id: String }
#[derive(Deserialize)]
struct CfManifestFile { projectID: i64, fileID: i64 }

// Hilfs-Struct, das nur Send-typen enthält, um es sicher durch den async-Block zu schleusen
struct RawBrowserPackInfo {
    project_id: String,
    name: String,
    summary: String,
    author: String,
    icon_path: Option<PathBuf>,
    source: String,
}

// Response-Struct für die Modrinth Projekt-API (https://api.modrinth.com/v2/project/{id}),
// wird genutzt um nachträglich (z.B. beim Installieren) an die icon_url zu kommen,
// falls die nicht schon aus einem Suchtreffer bekannt ist.
#[derive(Deserialize, Clone)]
struct ModrinthProjectInfo {
    title: String, // NEU
    icon_url: Option<String>,
}

// Eine einzelne Datei innerhalb einer Modrinth-Version (meistens die .mrpack
// oder einzelne Mod-Jars, je nach Projekt). "primary" markiert die Hauptdatei.
#[derive(Debug, Clone, Deserialize)]
struct ModrinthFile {
    url: String,
    filename: String,
    #[serde(default)]
    primary: bool,
}

// Response-Struct für die Modrinth Versions-API
// (https://api.modrinth.com/v2/project/{id}/version), inkl. der Felder die
// wir für den echten Download brauchen (loaders, files).
/*#[derive(Debug, Clone, Deserialize)]
struct ModrinthVersionInfo {
    version_number: String,
    game_versions: Vec<String>,
    loaders: Vec<String>,
    files: Vec<ModrinthFile>,
}*/

// NEU: version_id des Objekts selbst wird gebraucht, um Versionen zu vergleichen/zu fixieren.
#[derive(Debug, Clone, Deserialize)]
struct ModrinthVersionInfo {
    id: String, // NEU
    version_number: String,
    game_versions: Vec<String>,
    loaders: Vec<String>,
    files: Vec<ModrinthFile>,
    #[serde(default)]
    dependencies: Vec<ModrinthDependency>, // NEU
}

// NEU: eine einzelne Abhängigkeitsangabe aus Modrinths Versions-API
#[derive(Debug, Clone, Deserialize)]
struct ModrinthDependency {
    version_id: Option<String>,
    project_id: Option<String>,
    dependency_type: String, // "required" | "optional" | "incompatible" | "embedded"
}

// Hält die zuletzt geladenen Versionen für das aktuell offene Install-Popup,
// damit on_browser_confirm_install (bekommt nur den gewählten Index von Slint)
// weiß welches Pack/welche Version genau gemeint war. project_id wird
// zusätzlich für den Icon-Download beim Bestätigen gebraucht.
struct PendingInstall {
    project_id: String,
    pack_name: String,
    source: String, // <-- NEU: "Modrinth" oder "CurseForge"
    mrpack_versions: Vec<ModrinthVersionInfo>, // <-- UMBENANNT
    cf_files: Vec<CfFile>, // <-- NEU: Für CurseForge Versionen
}

// Eine Datei-Referenz aus der modrinth.index.json innerhalb eines .mrpack.
// "path" ist relativ zum Instanz-Ordner (meist "mods/xyz.jar").
#[derive(Debug, Deserialize)]
struct MrpackIndexFile {
    path: String,
    downloads: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct MrpackIndex {
    // #[serde(default)]: manche Packs lassen Felder weg, das soll den Import nicht abbrechen
    #[serde(default)]
    name: String,
    #[serde(default, rename = "versionId")]
    version_id: String,
    // z.B. {"minecraft": "1.20.1", "fabric-loader": "0.15.7"}
    #[serde(default)]
    dependencies: HashMap<String, String>,
    files: Vec<MrpackIndexFile>,
}

// Plain-Data-Variante der Mod-Zeilen für die Detailansicht (Send-sicher, ohne
// slint::Image). Das eigentliche ModFileInfo (mit geladenem Icon) wird erst
// auf dem UI-Thread daraus gebaut, siehe build_mod_file_info.
#[derive(Clone)] // <-- NEU: Damit wir .clone() und .cloned() verwenden können
struct ModFileEntry {
    filename: String,
    display_name: String,
    enabled: bool,
    icon_path: Option<String>,
    duplicate: bool, // Mod kommt mehrfach vor (orange in der UI)
    dup_total: i32,
    orphan: bool,
}

// Verknüpft eine Mod-Datei mit ihrer Modrinth-Herkunft (project_id + gewählte
// Version). Nur Mods, die über das Add-Mod-Popup installiert wurden, tauchen
// hier auf - Mods aus einem .mrpack-Modpack werden (noch) nicht getrackt,
// weil wir dort keine project_id pro Datei haben. Wird als mod_meta.json
// direkt im Instanz-Ordner gespeichert.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModEntryMeta {
    filename: String,
    project_id: String,
    project_name: String,
    version_id: String,
}

struct CompatAction {
    project_id: String,
    project_name: String,
    description: String,
    confirmed: bool,
}

fn load_mod_meta(instance_dir: &Path) -> Vec<ModEntryMeta> {
    let path = instance_dir.join("mod_meta.json");
    let Ok(file) = File::open(&path) else { return Vec::new(); };
    serde_json::from_reader(BufReader::new(file)).unwrap_or_default()
}

fn save_mod_meta(instance_dir: &Path, meta: &[ModEntryMeta]) {
    if let Ok(json) = serde_json::to_string_pretty(meta) {
        if let Err(e) = fs::write(instance_dir.join("mod_meta.json"), json) {
            println!("Konnte mod_meta.json nicht speichern: {e}");
        }
    }
}

// NEU: Eintrag für die zentrale mods-list.json
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModListEntry {
    filename: String,      // z.B. "sodium.jar"
    project_id: String,    // z.B. "AANobbMI" oder "282963"
}

fn save_mods_list(instance_dir: &Path, list: &[ModListEntry]) {
    if let Ok(json) = serde_json::to_string_pretty(list) {
        if let Err(e) = fs::write(instance_dir.join("mods-list.json"), json) {
            println!("Konnte mods-list.json nicht speichern: {e}");
        }
    }
}

fn load_mods_list(instance_dir: &Path) -> Vec<ModListEntry> {
    let path = instance_dir.join("mods-list.json");
    let Ok(file) = File::open(&path) else { return Vec::new(); };
    serde_json::from_reader(BufReader::new(file)).unwrap_or_default()
}

// NEU: Extrahiert die Modrinth Project ID aus einer Download-URL
// Format: https://cdn.modrinth.com/data/AANobbMI/versions/...

fn extract_modrinth_project_id(url: &str) -> Option<String> {
    if url.contains("/data/") {
        let parts: Vec<&str> = url.split("/data/").collect();
        if parts.len() >= 2 {
            return parts[1].split('/').next().map(|s| s.to_string());
        }
    }
    None
}

// ===================== Gemeinsame Hilfen für die neuen Features =====================

// Kleiner, stabiler Hash (FNV-1a, 64 Bit). Reicht, um "Datei hat sich geändert?" zu prüfen.
// (DefaultHasher wäre nicht über Rust-Versionen hinweg stabil, deshalb selbst gebaut.)
fn fnv64(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// Snapshot dessen, was ein Modpack bei der Installation mitgebracht hat (pack_snapshot.json).
// Damit erkennt das Update später: "Hat der Nutzer diese Config verändert oder nicht?"
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct PackSnapshot {
    #[serde(default)]
    overrides: HashMap<String, String>, // relativer Pfad -> fnv64 der Pack-Datei
    #[serde(default)]
    mod_paths: Vec<String>,             // Pfade der Mods, die das Pack selbst geliefert hat
}

fn load_pack_snapshot(instance_dir: &Path) -> Option<PackSnapshot> {
    let f = File::open(instance_dir.join("pack_snapshot.json")).ok()?;
    serde_json::from_reader(BufReader::new(f)).ok()
}

fn save_pack_snapshot(instance_dir: &Path, snap: &PackSnapshot) {
    if let Ok(json) = serde_json::to_string_pretty(snap) {
        let _ = fs::write(instance_dir.join("pack_snapshot.json"), json);
    }
}

// Verwaiste Libraries (orphans.json): Liste von Mod-Dateinamen, die in der Mod-Liste lila werden.
fn load_orphans(instance_dir: &Path) -> std::collections::HashSet<String> {
    let Ok(f) = File::open(instance_dir.join("orphans.json")) else { return Default::default(); };
    serde_json::from_reader::<_, Vec<String>>(BufReader::new(f)).unwrap_or_default().into_iter().collect()
}

fn save_orphans(instance_dir: &Path, list: &[String]) {
    if let Ok(json) = serde_json::to_string_pretty(list) {
        let _ = fs::write(instance_dir.join("orphans.json"), json);
    }
}

// Rekursiv einen Ordner in ein Zip schreiben. `base` bestimmt den relativen Pfad,
// `prefix` wird davor gesetzt (z.B. "overrides/" beim .mrpack, "" beim Server-Pack).
fn zip_add_dir(
    zip: &mut zip::ZipWriter<File>,
    base: &Path,
    dir: &Path,
    prefix: &str,
    opts: zip::write::SimpleFileOptions,
) -> Result<usize, String> {
    let mut n = 0;
    let Ok(rd) = fs::read_dir(dir) else { return Ok(0); };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            n += zip_add_dir(zip, base, &p, prefix, opts)?;
        } else {
            let rel = p.strip_prefix(base).map_err(|e| e.to_string())?
                .to_string_lossy().replace('\\', "/");
            zip.start_file(format!("{prefix}{rel}"), opts).map_err(|e| e.to_string())?;
            let mut f = File::open(&p).map_err(|e| e.to_string())?;
            std::io::copy(&mut f, zip).map_err(|e| e.to_string())?;
            n += 1;
        }
    }
    Ok(n)
}

// Desktop-Benachrichtigung. In eigenem Thread, weil D-Bus blockieren kann.
fn notify_desktop(title: &str, body: &str) {
    let (t, b) = (title.to_string(), body.to_string());
    std::thread::spawn(move || {
        let _ = notify_rust::Notification::new()
            .appname("SRUSM Launcher")
            .summary(&t)
            .body(&b)
            .icon("applications-games")
            .show();
    });
}

// ---- Modrinth: Mods anhand ihres SHA-1 erkennen (für Server-Pack, .mrpack-Export, Waisen-Suche) ----
#[derive(Deserialize, Clone)]
struct MrHashes { #[serde(default)] sha1: String, #[serde(default)] sha512: String }

#[derive(Deserialize, Clone)]
struct MrVersionFile {
    hashes: MrHashes,
    url: String,
    #[allow(dead_code)]
    filename: String,
    #[serde(default)]
    size: u64,
}

#[derive(Deserialize, Clone)]
struct MrVersionLookup {
    project_id: String,
    #[serde(default)]
    dependencies: Vec<ModrinthDependency>,
    #[serde(default)]
    files: Vec<MrVersionFile>,
}

#[derive(Deserialize, Clone)]
struct MrProjectBulk {
    id: String,
    #[serde(default)]
    categories: Vec<String>,
    #[serde(default)]
    additional_categories: Vec<String>,
    #[serde(default)]
    server_side: String, // "required" | "optional" | "unsupported"
}

struct IdentifiedMod {
    filename: String,
    sha1: String,
    lookup: Option<MrVersionLookup>, // None = Modrinth kennt die Datei nicht
}

// Liest alle aktiven .jar-Dateien aus mods/, hasht sie und fragt Modrinth in einem Rutsch.
async fn identify_mods(instance_dir: &Path) -> Result<Vec<IdentifiedMod>, String> {
    let mods_dir = instance_dir.join("mods");
    let hashed: Vec<(String, String)> = tokio::task::spawn_blocking(move || {
        use sha1::{Digest, Sha1};
        let mut out = Vec::new();
        if let Ok(rd) = fs::read_dir(&mods_dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if !name.ends_with(".jar") { continue; } // ".disabled" zählt nicht
                if let Ok(bytes) = fs::read(e.path()) {
                    let mut h = Sha1::new();
                    h.update(&bytes);
                    out.push((name, format!("{:x}", h.finalize())));
                }
            }
        }
        out
    }).await.map_err(|e| format!("Interner Fehler: {e}"))?;

    let mut found: HashMap<String, MrVersionLookup> = HashMap::new();
    for chunk in hashed.chunks(200) {
        let body = serde_json::json!({
            "hashes": chunk.iter().map(|(_, h)| h.clone()).collect::<Vec<_>>(),
            "algorithm": "sha1"
        });
        let resp = http_client()
            .post("https://api.modrinth.com/v2/version_files")
            .json(&body)
            .send().await
            .map_err(|e| format!("Modrinth-Abfrage fehlgeschlagen: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("Modrinth HTTP {}", resp.status()));
        }
        let part: HashMap<String, MrVersionLookup> =
            resp.json().await.map_err(|e| format!("Modrinth-Antwort ungültig: {e}"))?;
        found.extend(part);
    }

    Ok(hashed.into_iter().map(|(filename, sha1)| {
        let lookup = found.get(&sha1).cloned();
        IdentifiedMod { filename, sha1, lookup }
    }).collect())
}

// Projekt-Infos (Kategorien, server_side) für viele IDs auf einmal.
async fn fetch_projects_bulk(ids: &[String]) -> HashMap<String, MrProjectBulk> {
    let mut out = HashMap::new();
    for chunk in ids.chunks(100) {
        // URL-codiert: ["a","b"]  ->  %5B%22a%22,%22b%22%5D
        let list = chunk.iter().map(|i| format!("%22{i}%22")).collect::<Vec<_>>().join(",");
        let url = format!("https://api.modrinth.com/v2/projects?ids=%5B{list}%5D");
        let r: Option<Vec<MrProjectBulk>> = fetch(url, None).await.ok();
        if let Some(v) = r { for p in v { out.insert(p.id.clone(), p); } }
    }
    out
}

// Alle bekannten Java-GA-Releases von Adoptium. Reihenfolge = Anzeige im Dropdown.
const JAVA_VERSIONS: &[u32] = &[8, 11, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25];

/// Verzeichnis, in dem eine Java-Version installiert wird:
/// <launcher>/shared/java/<version>/
fn java_install_dir(launcher_dir: &Path, version: u32) -> PathBuf {
    launcher_dir.join("shared").join("java").join(version.to_string())
}

/// Das "Home" einer installierten Java-Version. Unter macOS liegt es eine Ebene tiefer.
fn java_home_dir(launcher_dir: &Path, version: u32) -> PathBuf {
    let base = java_install_dir(launcher_dir, version);
    if cfg!(target_os = "macos") { base.join("Contents").join("Home") } else { base }
}

/// Pfad zur java-Binary einer installierten Version (unter Windows mit .exe).
fn java_binary_path(launcher_dir: &Path, version: u32) -> PathBuf {
    let exe = if cfg!(windows) { "java.exe" } else { "java" };
    java_home_dir(launcher_dir, version).join("bin").join(exe)
}

/// Listet alle im Launcher installierten Java-Versionen.
fn get_installed_java_versions(launcher_dir: &Path) -> Vec<u32> {
    let java_root = launcher_dir.join("shared").join("java");
    let Ok(entries) = fs::read_dir(&java_root) else { return Vec::new(); };
    let mut out: Vec<u32> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()))
        .collect();
    out.sort();
    out
}

/// Passt die Java-Runtime für eine Instanz an. lyceris (und Mojang) erwarten
/// die Runtime unter `<runtime_dir>/<component>/bin/java`, wobei `<component>`
/// aus der Version-JSON der jeweiligen MC-Version stammt (z.B. "java-runtime-delta").
/// Wenn der User im Dropdown eine Java-Version gewählt hat, kopieren wir unsere
/// Installation aus `<launcher>/shared/java/<version>/` an genau diese Stelle.
fn ensure_java_runtime_for_instance(
    launcher_dir: &Path,
    mc_version: &str,
    selected_java: u32,
) -> Result<(), String> {
    // 1. Component-Namen aus den Version-JSONs ermitteln
    let versions_dir = launcher_dir.join("shared").join("versions");
    let component = find_java_component(&versions_dir, mc_version)
        .ok_or_else(|| format!("Kein Version-JSON für MC {} gefunden", mc_version))?;

    // 2. Quell-Java-Verzeichnis prüfen
    let source = java_home_dir(launcher_dir, selected_java);
    let source_java_bin = java_binary_path(launcher_dir, selected_java);
    if !source_java_bin.is_file() {
        return Err(format!("Java {} nicht installiert ({})", selected_java, source_java_bin.display()));
    }

    // 3. Ziel-Pfad + Marker
    let runtime_dir = launcher_dir.join("shared").join("runtime");
    let target = runtime_dir.join(&component);
    let marker = target.join(".java_version_marker");

    // 4. Wenn schon die richtige Version drin liegt UND die java-Binary
    //    seit dem Kopieren nicht überschrieben wurde, fertig.
    if marker.is_file() {
        let marker_ok = fs::read_to_string(&marker)
            .map(|s| s.trim() == selected_java.to_string())
            .unwrap_or(false);

        if marker_ok {
            // Prüfen, ob lyceris die Runtime nach unserem Kopieren überschrieben hat
            let marker_time = fs::metadata(&marker).and_then(|m| m.modified()).ok();
            let _java_time = fs::metadata(&source_java_bin).and_then(|m| m.modified()).ok();

            // Wir vergleichen mit dem Kopie-Ziel in der Runtime, nicht mit der Quelle
            let target_java_bin = target.join("bin").join(if cfg!(windows) { "java.exe" } else { "java" });
            let target_time = fs::metadata(&target_java_bin).and_then(|m| m.modified()).ok();

            // Wenn Ziel existiert und neuer als der Marker, hat lyceris überschrieben
            match (marker_time, target_time) {
                (Some(mt), Some(tt)) if tt <= mt => {
                    // Ziel ist älter als unser Marker -> noch unsere Kopie
                    return Ok(());
                }
                _ => {
                    // Ziel fehlt oder ist neuer -> lyceris hat überschrieben
                    println!("🔄 Runtime wurde überschrieben, kopiere Java {} neu", selected_java);
                }
            }
        }
    }

    // 5. Alten Runtime-Ordner entfernen
    if target.exists() {
        fs::remove_dir_all(&target)
            .map_err(|e| format!("Alten Runtime-Ordner entfernen: {e}"))?;
    }

    // 6. Kopieren (Quelle -> Ziel). Java-Runtime ist ~180 MB, das dauert
    //    ein paar Sekunden, aber nur beim ersten Start bzw. bei Wechsel.
    copy_dir_recursive(&source, &target)
        .map_err(|e| format!("Runtime kopieren: {e}"))?;

    // 7. Marker schreiben, damit wir beim nächsten Start nicht neu kopieren
    let _ = fs::write(&marker, selected_java.to_string());

    Ok(())
}

/// Sucht in `shared/versions/` einen Ordner, dessen Name mit der MC-Version
/// beginnt, und liest daraus den Java-Component-Namen.
/// Probiert zuerst exakte Treffer, dann Präfix-Match.
fn find_java_component(versions_dir: &Path, mc: &str) -> Option<String> {
    let Ok(entries) = fs::read_dir(versions_dir) else { return None; };

    // Erster Durchlauf: exakter Treffer (z.B. "1.21.1")
    let mut exact = None;
    let mut prefix_match = None;

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        if !path.is_dir() { continue; }

        // Version-JSON heißt genauso wie der Ordner
        let json_path = path.join(format!("{}.json", name));
        let Some(component) = read_java_component_from(&json_path) else { continue; };

        if name == mc {
            exact = Some(component);
            break;
        } else if name.starts_with(&format!("{}-", mc)) && prefix_match.is_none() {
            prefix_match = Some(component);
        }
    }

    exact.or(prefix_match)
}

/// Liest `javaVersion.component` aus einer Version-JSON.
fn read_java_component_from(json_path: &Path) -> Option<String> {
    let file = File::open(json_path).ok()?;
    let json: serde_json::Value = serde_json::from_reader(BufReader::new(file)).ok()?;
    json.get("javaVersion")?.get("component")?.as_str().map(String::from)
}

/// Dropdown-Modell: "Launcher Standard" + alle Zahlen-Strings der bekannten Versionen.
fn get_available_java_paths() -> Vec<String> {
    let mut paths = vec!["Launcher Standard".to_string()];
    for v in JAVA_VERSIONS {
        paths.push(v.to_string());
    }
    paths
}

/// Stellt sicher, dass die angegebene Java-Version im Launcher installiert ist.
/// Lädt bei Bedarf die JRE von Adoptium (passend zu OS und CPU) und entpackt sie nach
/// <launcher>/shared/java/<version>/. Gibt den Pfad zur java-Binary zurück.
async fn ensure_java_runtime(
    launcher_dir: &Path,
    version: u32,
    ui_handle: &Weak<AppWindow>,
) -> Result<PathBuf, String> {
    let java_bin = java_binary_path(launcher_dir, version);
    if java_bin.is_file() {
        return Ok(java_bin);
    }

    // Plattform für die Adoptium-API: Windows liefert .zip, Linux/macOS .tar.gz
    let os = if cfg!(target_os = "windows") { "windows" } else if cfg!(target_os = "macos") { "mac" } else { "linux" };
    let arch = if cfg!(target_arch = "aarch64") { "aarch64" } else { "x64" };
    let is_zip = cfg!(target_os = "windows");

    let url = format!(
        "https://api.adoptium.net/v3/binary/latest/{}/ga/{}/{}/jre/hotspot/normal/eclipse",
        version, os, arch
    );

    report_progress(ui_handle, format!("Lade Java {} herunter...", version));

    let response = http_client()
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Java-Download fehlgeschlagen: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("Adoptium HTTP {} für Java {} ({}/{})", response.status(), version, os, arch));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("Konnte Java-Archiv nicht lesen: {e}"))?;

    report_progress(ui_handle, format!("Entpacke Java {}...", version));

    let dir = java_install_dir(launcher_dir, version);
    let bytes_vec = bytes.to_vec();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        fs::create_dir_all(&dir).map_err(|e| format!("Ordner anlegen: {e}"))?;

        if is_zip {
            // Windows: .zip. Der Top-Level-Ordner (z.B. "jdk-17.0.13+11-jre/") wird abgeschnitten.
            let mut archive = zip::ZipArchive::new(Cursor::new(bytes_vec))
                .map_err(|e| format!("Zip lesen: {e}"))?;
            for i in 0..archive.len() {
                let mut entry = archive.by_index(i).map_err(|e| format!("Zip-Eintrag: {e}"))?;
                let Some(path) = entry.enclosed_name() else { continue; }; // schützt vor "../"-Pfaden
                let stripped: PathBuf = path.components().skip(1).collect();
                if stripped.as_os_str().is_empty() { continue; }
                let out_path = dir.join(&stripped);
                if entry.is_dir() {
                    fs::create_dir_all(&out_path).map_err(|e| format!("Ordner anlegen: {e}"))?;
                } else {
                    if let Some(parent) = out_path.parent() {
                        fs::create_dir_all(parent).map_err(|e| format!("Ordner anlegen: {e}"))?;
                    }
                    let mut f = File::create(&out_path).map_err(|e| format!("Datei anlegen: {e}"))?;
                    std::io::copy(&mut entry, &mut f).map_err(|e| format!("Entpacken: {e}"))?;
                }
            }
        } else {
            // Linux/macOS: .tar.gz, wieder ohne Top-Level-Ordner
            let gz = flate2::read::GzDecoder::new(Cursor::new(bytes_vec));
            let mut archive = tar::Archive::new(gz);
            for entry in archive.entries().map_err(|e| format!("Archiv lesen: {e}"))? {
                let mut entry = entry.map_err(|e| format!("Eintrag lesen: {e}"))?;
                let path = entry.path().map_err(|e| format!("Pfad lesen: {e}"))?.into_owned();
                let stripped: PathBuf = path.components().skip(1).collect();
                if stripped.as_os_str().is_empty() { continue; }

                let out_path = dir.join(&stripped);
                if entry.header().entry_type().is_dir() {
                    fs::create_dir_all(&out_path).map_err(|e| format!("Ordner anlegen: {e}"))?;
                } else {
                    if let Some(parent) = out_path.parent() {
                        fs::create_dir_all(parent).map_err(|e| format!("Ordner anlegen: {e}"))?;
                    }
                    entry.unpack(&out_path).map_err(|e| format!("Entpacken: {e}"))?;
                }
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| format!("Interner Fehler: {e}"))??;

    // Ausführbar-Bit sicherstellen (nur Unix)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(&java_bin) {
            let mut perms = meta.permissions();
            perms.set_mode(0o755);
            let _ = fs::set_permissions(&java_bin, perms);
        }
    }

    if !java_bin.is_file() {
        return Err(format!("Java-Binary nach dem Entpacken nicht gefunden: {}", java_bin.display()));
    }
    Ok(java_bin)
}

// Gemeinsamer HTTP-Client mit ordentlichem User-Agent (Modrinth bittet in
// ihrer API-Doku darum, einen aussagekräftigen User-Agent zu setzen; ohne
// einen kann es vereinzelt zu Blockaden/Fehlern kommen). Wird lazy beim
// ersten Zugriff gebaut und danach wiederverwendet.
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent("srusm-minecraft-launcher/0.1 (contact: basti)")
            .build()
            .expect("Konnte HTTP-Client nicht bauen")
    })
}

// extra_facets = zusätzliche AND-Filter im Format "versions:1.21.1" oder "categories:fabric".
// Leeres Slice = keine Zusatzfilter (so nutzt es der Modpack-Browser weiter wie bisher).
// Modrinth verknüpft die äußeren Facet-Gruppen mit AND, deshalb bekommt jeder Filter
// seine eigene Gruppe: [["project_type:mod"],["versions:1.21.1"],["categories:fabric"]]
/*async fn search_projects(query: &str, project_type: &str, limit: i32, sort: &str) -> Option<Vec<ModrinthHit>> {
    let escaped_query = query.replace(' ', "%20");
    let facet = format!("%5B%5B%22project_type%3A{}%22%5D%5D", project_type);
    let limit = limit.clamp(1, 100);
    
    // Wenn die Suche leer ist, lassen wir den 'query'-Parameter ganz weg, 
    // um Probleme mit der API oder dem HTTP-Client zu vermeiden.
    let url = if escaped_query.is_empty() {
        format!(
            "https://api.modrinth.com/v2/search?facets={}&limit={}&sort={}",
            facet, limit, sort
        )
    } else {
        format!(
            "https://api.modrinth.com/v2/search?facets={}&limit={}&query={}&sort={}",
            facet, limit, escaped_query, sort
        )
    };
    
    let response: ModrinthSearchResponse = fetch(url, None).await.ok()?;
    Some(response.hits)
}*/

async fn search_projects(query: &str, project_type: &str, limit: i32, sort: &str, extra_facets: &[String]) -> Option<Vec<ModrinthHit>> {
    let escaped_query = query.replace(' ', "%20");
    let limit = limit.clamp(1, 100);

    // Erste Gruppe: der Projekttyp. %5B = [, %5D = ], %22 = ", %3A = :
    let mut groups: Vec<String> = vec![format!("%5B%22project_type%3A{}%22%5D", project_type)];
    for f in extra_facets {
        groups.push(format!("%5B%22{}%22%5D", f.replace(':', "%3A")));
    }
    let facet = format!("%5B{}%5D", groups.join(","));

    let url = if escaped_query.is_empty() {
        format!(
            "https://api.modrinth.com/v2/search?facets={}&limit={}&sort={}",
            facet, limit, sort
        )
    } else {
        format!(
            "https://api.modrinth.com/v2/search?facets={}&limit={}&query={}&sort={}",
            facet, limit, escaped_query, sort
        )
    };

    let response: ModrinthSearchResponse = fetch(url, None).await.ok()?;
    Some(response.hits)
}

// Baut die Modrinth-Zusatzfilter aus MC-Version + Loader (gleiche Logik wie beim
// "Mod hinzufügen"-Popup der Instanzen). Leere Version / "vanilla" / "none" = kein Filter.
// Bei CurseForge-IDs ("forge-14.23.5.2860") wird alles ab dem ersten '-' abgeschnitten.
fn build_mod_facets(mc: &str, loader: &str) -> Vec<String> {
    let mut facets: Vec<String> = Vec::new();
    let mc = mc.trim();
    if !mc.is_empty() {
        facets.push(format!("versions:{}", mc));
    }
    let loader = loader.split('-').next().unwrap_or("").trim().to_lowercase();
    if !loader.is_empty() && loader != "none" && loader != "vanilla" {
        facets.push(format!("categories:{}", loader));
    }
    facets
}

// Lädt alle verfügbaren Versionen eines Modrinth-Projekts (Modpack oder Mod).
// Neueste zuerst laut Modrinth-API-Reihenfolge.

// In-Memory Cache für Modrinth-Antworten mit TTL. Wird u.a. vom Resolver
// stark frequentiert (mehrere Aufrufe pro Projekt hintereinander).
static MODRINTH_VERSIONS_CACHE: OnceLock<Mutex<HashMap<String, (std::time::Instant, Vec<ModrinthVersionInfo>)>>> = OnceLock::new();
static MODRINTH_PROJECT_CACHE: OnceLock<Mutex<HashMap<String, (std::time::Instant, ModrinthProjectInfo)>>> = OnceLock::new();

const MODRINTH_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(1800); // 30 Min

async fn fetch_pack_versions(project_id: &str) -> Option<Vec<ModrinthVersionInfo>> {
    let cache = MODRINTH_VERSIONS_CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    if let Ok(g) = cache.lock() {
        if let Some((t, v)) = g.get(project_id) {
            if t.elapsed() < MODRINTH_CACHE_TTL {
                return Some(v.clone());
            }
        }
    }

    let url = format!("https://api.modrinth.com/v2/project/{}/version", project_id);
    let result: Vec<ModrinthVersionInfo> = fetch(url, None).await.ok()?;

    if let Ok(mut g) = cache.lock() {
        g.insert(project_id.to_string(), (std::time::Instant::now(), result.clone()));
    }
    Some(result)
}

// Fragt die icon_url eines Modrinth-Projekts ab (falls nicht schon aus einem
// Suchtreffer bekannt, z.B. beim Bestätigen einer Installation).

async fn fetch_project_icon_url(project_id: &str) -> Option<String> {
    let url = format!("https://api.modrinth.com/v2/project/{}", project_id);
    let project: ModrinthProjectInfo = fetch(url, None).await.ok()?;
    project.icon_url
}


async fn fetch_project_info(project_id: &str) -> Option<ModrinthProjectInfo> {
    let cache = MODRINTH_PROJECT_CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    if let Ok(g) = cache.lock() {
        if let Some((t, v)) = g.get(project_id) {
            if t.elapsed() < MODRINTH_CACHE_TTL {
                return Some(v.clone());
            }
        }
    }

    let url = format!("https://api.modrinth.com/v2/project/{}", project_id);
    let result: ModrinthProjectInfo = fetch(url, None).await.ok()?;

    if let Ok(mut g) = cache.lock() {
        g.insert(project_id.to_string(), (std::time::Instant::now(), result.clone()));
    }
    Some(result)
}

/*async fn search_cf_projects(query: &str, limit: i32, sort_by_downloads: bool) -> Option<Vec<CfSearchHit>> {
    let url = format!(
        "https://api.curseforge.com/v1/mods/search?gameId=432&searchFilter={}&pageSize={}&sortField={}&sortOrder=desc",
        query.replace(' ', "%20"),
        limit.clamp(1, 100),
        if sort_by_downloads { 2 } else { 0 } // 2 = Downloads, 0 = Relevance
    );
    let client = http_client();
    
    let response = client.get(&url)
        .header("x-api-key", CF_API_KEY)
        .header("Accept", "application/json")
        .send().await;
        
    match response {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                println!("⚠️ CurseForge API HTTP Fehler: {}", status);
                if let Ok(text) = resp.text().await {
                    println!("👉 Raw Response: {}", text);
                }
                return None;
            }
            
            // Zuerst die rohen Bytes lesen
            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    println!("⚠️ Konnte Response-Bytes nicht lesen: {}", e);
                    return None;
                }
            };
            
            // Dann versuchen zu parsen
            let parsed: Result<CfSearchResponse, _> = serde_json::from_slice(&bytes);
            match parsed {
                Ok(data) => Some(data.data),
                Err(e) => {
                    println!("⚠️ CurseForge JSON Parse Fehler: {}", e);
                    // Raw Response ausgeben, damit wir sehen was die API schickt
                    if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                        println!("👉 Raw Response (erste 500 Zeichen): {}", &text[..text.len().min(500)]);
                    }
                    None
                }
            }
        }
        Err(e) => {
            println!("⚠️ CurseForge Netzwerk Fehler: {}", e);
            None
        }
    }
}*/

/*async fn search_projects(query: &str, project_type: &str, limit: i32, sort: &str) -> Option<Vec<ModrinthHit>> {
    let escaped_query = query.replace(' ', "%20");
    let facet = format!("%5B%5B%22project_type%3A{}%22%5D%5D", project_type);
    let limit = limit.clamp(1, 100);
    let url = format!(
        "https://api.modrinth.com/v2/search?facets={}&limit={}&query={}&sort={}",
        facet, limit, escaped_query, sort
    );
    let response: ModrinthSearchResponse = fetch(url, None).await.ok()?;
    Some(response.hits)
}*/


async fn search_cf_projects(query: &str, limit: i32, sort_by_downloads: bool) -> Option<Vec<CfSearchHit>> {
    let limit = limit.clamp(1, 100);
    
    // classId=4471 = "Modpacks" bei CurseForge (Minecraft, gameId 432).
    // Ohne diesen Filter kommen auch normale Mods, Resourcepacks, Welten etc. zurück.
    // Als Konstante, damit wir den Wert nicht dreimal hart in die URLs schreiben müssen.
    const CF_CLASS_MODPACKS: i32 = 4471;

    // URL bauen: searchFilter nur wenn Query nicht leer, sortField nur wenn Downloads gewünscht
    let url = if query.trim().is_empty() {
        // Leere Suche: Keine Filter, aber nach Downloads sortieren
        format!(
            "https://api.curseforge.com/v1/mods/search?gameId=432&classId={}&pageSize={}&sortField=2&sortOrder=desc",
            CF_CLASS_MODPACKS, limit
        )
    } else if sort_by_downloads {
        // Mit Suchbegriff + Downloads-Sortierung
        format!(
            "https://api.curseforge.com/v1/mods/search?gameId=432&classId={}&searchFilter={}&pageSize={}&sortField=2&sortOrder=desc",
            CF_CLASS_MODPACKS,
            query.replace(' ', "%20"),
            limit
        )
    } else {
        // Mit Suchbegriff, Standard-Sortierung (Relevanz)
        format!(
            "https://api.curseforge.com/v1/mods/search?gameId=432&classId={}&searchFilter={}&pageSize={}",
            CF_CLASS_MODPACKS,
            query.replace(' ', "%20"),
            limit
        )
    };
    
    let client = http_client();
    
    let response = client.get(&url)
        .header("x-api-key", CF_API_KEY)
        .header("Accept", "application/json")
        .send().await;
        
    match response {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                println!("⚠️ CurseForge API HTTP Fehler: {}", status);
                if let Ok(text) = resp.text().await {
                    println!("👉 Raw Response: {}", text);
                }
                return None;
            }
            
            let bytes = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    println!("️ Konnte Response-Bytes nicht lesen: {}", e);
                    return None;
                }
            };
            
            let parsed: Result<CfSearchResponse, _> = serde_json::from_slice(&bytes);
            match parsed {
                Ok(data) => Some(data.data),
                Err(e) => {
                    println!("⚠️ CurseForge JSON Parse Fehler: {}", e);
                    if let Ok(text) = String::from_utf8(bytes.to_vec()) {
                        println!("👉 Raw Response (erste 500 Zeichen): {}", &text[..text.len().min(500)]);
                    }
                    None
                }
            }
        }
        Err(e) => {
            println!("⚠️ CurseForge Netzwerk Fehler: {}", e);
            None
        }
    }
}

// <-- NEU: CurseForge Versionen (Dateien) eines Modpacks abrufen

async fn fetch_cf_files(project_id: &str) -> Option<Vec<CfFile>> {
    let url = format!("https://api.curseforge.com/v1/mods/{}/files", project_id);
    let client = http_client();
    let response = client.get(&url)
        .header("x-api-key", CF_API_KEY)
        .header("Accept", "application/json")
        .send().await.ok()?;
    let parsed: CfFilesResponse = response.json().await.ok()?;
    // Wir filtern nur nach Dateien, die tatsächlich eine Download-URL haben (manche sind server-side only)
    Some(parsed.data.into_iter().filter(|f| !f.downloadUrl.is_empty()).collect())
}

// <-- NEU: Einzelne Mod von CurseForge herunterladen (für Manifest-Verarbeitung)

async fn download_cf_file(file_id: i64) -> Result<(String, Vec<u8>), String> {
    let url = format!("https://api.curseforge.com/v1/mods/files/{}", file_id);
    let client = http_client();
    let response = client.get(&url)
        .header("x-api-key", CF_API_KEY)
        .header("Accept", "application/json")
        .send().await.map_err(|e| format!("CF API Fehler: {e}"))?;
    
    #[derive(Deserialize)]
    struct CfFileData { fileName: String, downloadUrl: String }
    #[derive(Deserialize)]
    struct CfFileResponse { data: CfFileData }

    let parsed: CfFileResponse = response.json().await.map_err(|e| format!("CF JSON Fehler: {e}"))?;
    let bytes = download_bytes(&parsed.data.downloadUrl).await?;
    Ok((parsed.data.fileName, bytes))
}

async fn fetch_version_by_id(version_id: &str) -> Option<ModrinthVersionInfo> {
    let url = format!("https://api.modrinth.com/v2/version/{}", version_id);
    fetch(url, None).await.ok()
}

// Lädt eine URL als rohe Bytes herunter (für .mrpack-Archive und einzelne Mod-Dateien).
#[hotpath::measure]
async fn download_bytes(url: &str) -> Result<Vec<u8>, String> {
    let response = http_client()
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Download fehlgeschlagen: {e}"))?;
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("Konnte Antwort nicht lesen: {e}"))?;
    Ok(bytes.to_vec())
}

// Lädt ein Icon herunter und legt es im Icon-Cache-Ordner ab (Dateiname = key,
// z.B. die Modrinth-Projekt-ID). Liegt die Datei schon im Cache, wird nicht
// erneut heruntergeladen. Gibt den Pfad zurück (Send-sicher als PathBuf),
// das eigentliche slint::Image wird erst auf dem UI-Thread daraus gebaut.
//
// WICHTIG: Modrinth liefert Icons teils als WebP statt PNG/JPEG. Slints
// Image::load_from_path kann WebP nicht zuverlässig laden und lädt dann
// einfach nichts (kein Fehler, nur ein leeres Bild) - das war der Grund für
// fehlende Icons. Deshalb wird hier über die "image"-Crate dekodiert und
// IMMER als PNG zwischengespeichert, unabhängig vom Quellformat.

#[hotpath::measure]
async fn cache_icon(icon_cache_dir: &Path, key: &str, icon_url: &Option<String>) -> Option<PathBuf> {
    let url = icon_url.as_ref()?;
    let cache_path = icon_cache_dir.join(format!("{key}.png"));

    if !cache_path.is_file() {
        let bytes = download_bytes(url).await.ok()?;

        // Zielgröße hängt vom Device-Tier ab (64/96/128 px).
        let target_size = tier_icon_size(device_tier());
        let cdir = icon_cache_dir.to_path_buf();
        let cpath = cache_path.clone();
        tokio::task::spawn_blocking(move || -> Option<()> {
            let decoded = image::load_from_memory(&bytes).ok()?.to_rgba8();
            let resized = image::imageops::resize(
                &decoded, target_size, target_size,
                image::imageops::FilterType::Triangle,
            );
            fs::create_dir_all(&cdir).ok()?;
            resized.save_with_format(&cpath, image::ImageFormat::Png).ok()?;
            Some(())
        })
        .await
        .ok()??;
    }

    Some(cache_path)
}

// ===================== Fehlende Icons automatisch reparieren =====================

// Merkt sich Icon-Keys (= Dateiname ohne ".png" = Modrinth-Projekt-ID), deren Datei fehlt.
static MISSING_ICONS: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
fn missing_icons() -> &'static Mutex<std::collections::HashSet<String>> {
    MISSING_ICONS.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

fn note_missing_icon(path: &Path) {
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { return; };
    if stem.starts_with("srv_") { return; } // Server-Icons kommen aus servers.dat, nicht aus dem Netz
    if let Ok(mut g) = missing_icons().lock() { g.insert(stem.to_string()); }
}

// Lädt ein Icon von Platte als slint::Image. Muss auf dem UI-Thread laufen.
// WICHTIG: Slint prüft bei Pfad-Bildern nicht, ob die Datei existiert (der Fehler
// "Error loading image from ..." kommt erst beim Zeichnen). Deshalb prüfen wir selbst
// und merken uns fehlende Dateien, damit repair_missing_icons() sie neu laden kann.
fn load_icon(path: &Option<PathBuf>) -> Image {
    match path {
        Some(p) if p.is_file() => Image::load_from_path(p).unwrap_or_default(),
        Some(p) => { note_missing_icon(p); Image::default() }
        None => Image::default(),
    }
}

// Baut alle Instanz-Kacheln + (falls offen) Detail-Icon und Mod-Liste neu auf.
// Nur auf dem UI-Thread aufrufen.
fn refresh_all_tiles(ui: &AppWindow) {
    let instances = launcher_dir().join("instances");
    if let Ok(rd) = fs::read_dir(&instances) {
        for e in rd.flatten() {
            let Some(cfg) = load_instance_config(&e.path()) else { continue; };
            with_packs_model(ui, |vm| {
                if let Some(i) = find_tile_row(vm, &cfg.name) {
                    vm.set_row_data(i, tile_from_config(&cfg));
                }
            });
        }
    }

    // Detailansicht der aktuell geöffneten Instanz
    let detail = ui.get_detail_instance_name().to_string();
    if !detail.is_empty() {
        let dir = instances.join(&detail);
        if let Some(cfg) = load_instance_config(&dir) {
            let p = cfg.icon_path.map(PathBuf::from);
            ui.set_detail_instance_icon(load_icon(&p));
        }
        invalidate_instance_cache(&detail);
        ui.set_detail_mod_files(ModelRc::new(VecModel::from(list_mod_files_cached(&detail, &dir))));
    }
}

// Lädt alle gemerkten fehlenden Icons neu und aktualisiert danach die UI.
// Wartet kurz, damit der UI-Thread vorher alle Kacheln gebaut (und gemeldet) hat.
async fn repair_missing_icons(ui: Weak<AppWindow>) {
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;

    let keys: Vec<String> = match missing_icons().lock() {
        Ok(mut g) => g.drain().collect(), // leert die Liste -> jedes Icon wird nur 1x versucht
        Err(_) => return,
    };
    if keys.is_empty() { return; }

    println!("🩹 Repariere {} fehlende Icons...", keys.len());
    let icon_dir = launcher_dir().join("icon_cache");
    let sem = Arc::new(tokio::sync::Semaphore::new(tier_download_concurrency(device_tier()).min(4)));

    let mut tasks = Vec::with_capacity(keys.len());
    for key in keys {
        let (sem, dir) = (sem.clone(), icon_dir.clone());
        tasks.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.ok()?;
            let info = fetch_project_info(&key).await?;     // Modrinth-Projekt (Key = Projekt-ID)
            let icon_url = info.icon_url?;
            cache_icon(&dir, &key, &Some(icon_url)).await.map(|_| ())
        }));
    }

    let mut ok = 0;
    for t in tasks {
        if let Ok(Some(())) = t.await { ok += 1; }
    }
    println!("🩹 {} Icons neu geladen.", ok);

    if ok > 0 {
        let _ = ui.upgrade_in_event_loop(|ui| { refresh_all_tiles(&ui); });
    }
}

// Schreibt eine kleine Statusmeldung ins Install-Popup, sicher vom Tokio-Thread
// aus aufrufbar (Weak<AppWindow> ist Send, im Gegensatz zu Rc<VecModel<...>>).

fn report_progress(ui_handle: &Weak<AppWindow>, text: String) {
    let _ = ui_handle.upgrade_in_event_loop(move |ui| {
        ui.set_install_popup_status(text.into());
    });
}

// Gleiches Prinzip wie report_progress, aber fürs Add-Mod-Popup in der Detailansicht.

fn report_mod_progress(ui_handle: &Weak<AppWindow>, text: String) {
    let _ = ui_handle.upgrade_in_event_loop(move |ui| {
        ui.set_add_mod_status(text.into());
    });
}

// ===================== Start-Popup (Hilfen) =====================

/// Setzt Text + Fortschritt (0.0 - 1.0) im Start-Popup. Aus jedem Thread aufrufbar.
fn launch_ui(ui: &Weak<AppWindow>, status: impl Into<String>, progress: f32) {
    let s: String = status.into();
    let _ = ui.upgrade_in_event_loop(move |ui| {
        ui.set_launch_status(s.into());
        ui.set_launch_progress(progress);
    });
}

/// Blendet das Start-Popup aus.
fn launch_ui_hide(ui: &Weak<AppWindow>) {
    let _ = ui.upgrade_in_event_loop(|ui| {
        ui.set_launch_popup_visible(false);
    });
}

/// Zeigt eine Fehlermeldung 4 Sekunden im Popup an (die Funktion kehrt danach zurück,
/// danach schließt der LaunchGuard das Popup).
async fn launch_error(ui: &Weak<AppWindow>, msg: String) {
    println!("❌ {msg}");
    launch_ui(ui, format!("Fehler: {msg}"), 0.0);
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
}

/// Schließt das Popup automatisch, sobald der Start-Task endet (egal ob Fehler,
/// "return" mitten drin oder normales Spielende). So bleibt es nie hängen.
struct LaunchGuard(Weak<AppWindow>);
impl Drop for LaunchGuard {
    fn drop(&mut self) { launch_ui_hide(&self.0); }
}

// Log-Zeilen, die kurz nach dem Erscheinen des Minecraft-Fensters kommen.
// Falls das Popup bei einer Version zu früh/spät verschwindet: hier anpassen.
const WINDOW_MARKERS: [&str; 5] = [
    "Backend library",             // 1.17+: direkt nach Fenster-Erstellung
    "LWJGL Version",               // ältere Versionen
    "Reloading ResourceManager",   // 1.8 - 1.12
    "OpenAL initialized",
    "Sound engine started",        // dein STARTUP_MARKER, als Fallback
];

// Ringpuffer für Debug-Meldungen, wird bei aktivem debug_ui in die UI gespiegelt.
// Macht selbst nichts, wenn debug_ui aus ist (spart die Model-Rebuild-Arbeit).
//fn push_debug_log(ui_handle: &Weak<AppWindow>, message: String) {
//    let _ = ui_handle.upgrade_in_event_loop(move |ui| {
//        if !ui.get_debug_ui() { return; }
//        let model = ui.get_debug_log();
//        let mut items: Vec<SharedString> = (0..model.row_count())
//            .filter_map(|i| model.row_data(i))
//            .collect();
//        items.push(message.into());
//        if items.len() > 200 { items.remove(0); } // Ringpuffer, kein unbegrenztes Wachstum
//        ui.set_debug_log(ModelRc::new(VecModel::from(items)));
//    });
//}

// Entpackt ein .mrpack-Archiv (Zip) in den Instanz-Ordner:
// - "overrides/" bzw. "client-overrides/" werden 1:1 in den Instanz-Ordner kopiert
//   (Configs, Resourcepacks, Shader etc., die im Pack mitgeliefert werden)
// - "modrinth.index.json" listet die eigentlichen Mod-Jars, die wir einzeln
//   nachladen und unter ihrem "path" (meist "mods/xyz.jar") ablegen
#[hotpath::measure]
async fn install_mrpack(
    instance_dir: &PathBuf,
    mrpack_bytes: Vec<u8>,
    ui_handle: &Weak<AppWindow>,
) -> Result<(), String> {
    // Alles Blocking (Zip öffnen, Index lesen, Overrides entpacken) in EINEM
    // spawn_blocking-Block. Die Downloads danach laufen wieder async.
    let dir = instance_dir.clone();
    let ui_for_progress = ui_handle.clone();

    let (index, override_hashes) = tokio::task::spawn_blocking(move || -> Result<(MrpackIndex, HashMap<String, String>), String> {
        let mut snapshot: HashMap<String, String> = HashMap::new();
        let cursor = Cursor::new(mrpack_bytes);
        let mut archive = zip::ZipArchive::new(cursor)
            .map_err(|e| format!("Konnte .mrpack nicht als Zip öffnen: {e}"))?;

        // 1. modrinth.index.json
        let index: MrpackIndex = {
            let mut index_file = archive
                .by_name("modrinth.index.json")
                .map_err(|e| format!("modrinth.index.json fehlt im .mrpack: {e}"))?;
            let mut contents = String::new();
            index_file.read_to_string(&mut contents)
                .map_err(|e| format!("Konnte modrinth.index.json nicht lesen: {e}"))?;
            serde_json::from_str(&contents)
                .map_err(|e| format!("Konnte modrinth.index.json nicht parsen: {e}"))?
        };

        // 2. Overrides entpacken
        let _ = ui_for_progress.upgrade_in_event_loop(|ui| {
            ui.set_install_popup_status("Entpacke Overrides...".into());
        });

        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).map_err(|e| format!("Zip-Fehler: {e}"))?;
            let name = entry.name().to_string();

            let Some(rel_path) = name
                .strip_prefix("overrides/")
                .or_else(|| name.strip_prefix("client-overrides/"))
            else { continue; };

            if rel_path.is_empty() { continue; }

            let out_path = dir.join(rel_path);
            if entry.is_dir() {
                fs::create_dir_all(&out_path).map_err(|e| format!("Ordner anlegen: {e}"))?;
            } else {
                if let Some(parent) = out_path.parent() {
                    fs::create_dir_all(parent).map_err(|e| format!("Ordner anlegen: {e}"))?;
                }
                // GEÄNDERT: erst in den Speicher lesen, hashen, dann schreiben
                let mut data = Vec::new();
                entry.read_to_end(&mut data).map_err(|e| format!("Datei lesen: {e}"))?;
                snapshot.insert(rel_path.to_string(), fnv64(&data));
                fs::write(&out_path, &data).map_err(|e| format!("Datei schreiben: {e}"))?;
            }
        }

        Ok((index, snapshot))
    })
    .await
    .map_err(|e| format!("Interner Fehler beim Entpacken: {e}"))??;

    // 3. Mod-Dateien parallel nachladen (Concurrency je nach Device-Tier).
    //    Reihenfolge bleibt erhalten: wir indizieren die Tasks nach Position
    //    im index.files-Vec und sortieren das Ergebnis danach.
    let dl_concurrency = tier_download_concurrency(device_tier());
    let sem = Arc::new(tokio::sync::Semaphore::new(dl_concurrency));

    let files_to_download: Vec<(usize, String, String, Option<String>)> = index.files.iter().enumerate()
        .filter_map(|(i, f)| {
            let url = f.downloads.first()?.clone();
            let project_id = extract_modrinth_project_id(&url);
            Some((i, url, f.path.clone(), project_id))
        })
        .collect();

    let total = files_to_download.len();
    let mut handles = Vec::with_capacity(total);
    for (i, url, path, project_id) in files_to_download {
        let sem = sem.clone();
        let ui = ui_handle.clone();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.ok()?;
            report_progress(&ui, format!("Lade Mod {}/{}: {}", i + 1, total, path));
            let bytes = download_bytes(&url).await.ok()?;
            Some((i, path, project_id, bytes))
        }));
    }

    // Ergebnisse einsammeln + sortieren
    let mut results: Vec<(usize, String, Option<String>, Vec<u8>)> = Vec::with_capacity(total);
    for h in handles {
        if let Ok(Some(r)) = h.await { results.push(r); }
    }
    results.sort_by_key(|(i, _, _, _)| *i);

    // Jetzt sequentiell schreiben + mods_list aufbauen
    let mut mods_list: Vec<ModListEntry> = Vec::new();
    for (_i, path, project_id, bytes) in results {
        let out_path = instance_dir.join(&path);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("Ordner anlegen: {e}"))?;
        }
        fs::write(&out_path, &bytes)
            .map_err(|e| format!("Datei schreiben ({}): {e}", path))?;

        if let Some(project_id) = project_id {
            let filename = path.split('/').last().unwrap_or(&path).to_string();
            mods_list.push(ModListEntry { filename, project_id });
        }
    }

    save_mods_list(instance_dir, &mods_list);

    save_pack_snapshot(instance_dir, &PackSnapshot {
        overrides: override_hashes,
        mod_paths: index.files.iter().map(|f| f.path.clone()).collect(),
    });

    Ok(())
}

// Liest nur die modrinth.index.json aus einem .mrpack (Zip im Speicher), ohne etwas zu entpacken.

fn read_mrpack_index(bytes: &[u8]) -> Result<MrpackIndex, String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| format!("Datei ist kein gültiges .mrpack (Zip): {e}"))?;

    let mut index_file = archive
        .by_name("modrinth.index.json")
        .map_err(|_| "modrinth.index.json fehlt - ist das ein Modrinth-Modpack (.mrpack)?".to_string())?;

    let mut contents = String::new();
    index_file
        .read_to_string(&mut contents)
        .map_err(|e| format!("Konnte modrinth.index.json nicht lesen: {e}"))?;

    let index: MrpackIndex = serde_json::from_str(&contents)
        .map_err(|e| format!("Konnte modrinth.index.json nicht parsen: {e}"))?;
    Ok(index)
}

// Bestimmt aus den "dependencies" die Minecraft-Version und unseren Loader-Kind
// ("fabric" | "quilt" | "neoforge" | "forge" | "none").

fn mrpack_mc_and_loader(deps: &HashMap<String, String>) -> (String, String) {
    let mc = deps.get("minecraft").cloned().unwrap_or_default();
    let loader = if deps.contains_key("fabric-loader") {
        "fabric"
    } else if deps.contains_key("quilt-loader") {
        "quilt"
    } else if deps.contains_key("neoforge") {
        "neoforge"
    } else if deps.contains_key("forge") {
        "forge"
    } else {
        "none"
    };
    (mc, loader.to_string())
}

// Liest die vom .mrpack geforderte Loader-Version aus den "dependencies".
// Die Schlüssel heißen je nach Loader anders (z.B. "fabric-loader" für Fabric).

fn mrpack_loader_version(deps: &HashMap<String, String>, loader_kind: &str) -> Option<String> {
    let key = match loader_kind {
        "fabric" => "fabric-loader",
        "quilt" => "quilt-loader",
        "neoforge" => "neoforge",
        "forge" => "forge",
        _ => return None,
    };
    deps.get(key).cloned()
}

// Zerlegt eine CurseForge-Loader-ID wie "fabric-0.16.9" in ("fabric", Some("0.16.9")).
// Eine ID ohne "-" (z.B. "none") ergibt (id, None).

fn split_loader_id(id: &str) -> (String, Option<String>) {
    match id.split_once('-') {
        Some((kind, ver)) if !ver.is_empty() => (kind.to_lowercase(), Some(ver.to_string())),
        _ => (id.to_lowercase(), None),
    }
}

// Bereinigt einen eingefügten Pfad: Anführungszeichen (kommen z.B. beim Reinziehen ins Terminal),
// file://-Prefix (manche Dateimanager) und "~/" werden aufgelöst.

fn clean_path_input(input: &str) -> PathBuf {
    let mut s = input
        .trim()
        .trim_matches(|c: char| c == '"' || c == '\'')
        .trim()
        .to_string();

    if let Some(rest) = s.strip_prefix("file://") {
        s = rest.replace("%20", " "); // nur Leerzeichen decodiert, reicht für normale Pfade
    }

    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }

    PathBuf::from(s)
}

// Entfernt Zeichen, die in Ordnernamen Probleme machen (v.a. unter Windows).

fn sanitize_dir_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if "/\\:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    cleaned.trim().to_string()
}

// Existiert schon eine Instanz mit dem Namen, wird " (2)", " (3)", ... angehängt,
// damit ein Import nie eine bestehende Instanz überschreibt.

fn unique_instance_name(instances_dir: &Path, base: &str) -> String {
    if !instances_dir.join(base).exists() {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base} ({n})");
        if !instances_dir.join(&candidate).exists() {
            return candidate;
        }
        n += 1;
    }
}

// Fallback für Versionen OHNE .mrpack (z.B. einzelne Mod-Jars als "files"):
// lädt jede Datei einzeln direkt in den "mods"-Unterordner der Instanz.

async fn download_files_direct(
    instance_dir: &PathBuf,
    files: &[ModrinthFile],
    ui_handle: &Weak<AppWindow>,
) -> Result<(), String> {
    let mods_dir = instance_dir.join("mods");
    fs::create_dir_all(&mods_dir).map_err(|e| format!("Konnte mods-Ordner nicht anlegen: {e}"))?;

    let dl_concurrency = tier_download_concurrency(device_tier());
    let sem = Arc::new(tokio::sync::Semaphore::new(dl_concurrency));
    let total = files.len();

    let mut handles = Vec::with_capacity(total);
    for (i, file) in files.iter().enumerate() {
        let url = file.url.clone();
        let fname = file.filename.clone();
        let ui = ui_handle.clone();
        let sem = sem.clone();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.ok()?;
            report_progress(&ui, format!("Lade Datei {}/{}: {}", i + 1, total, fname));
            let bytes = download_bytes(&url).await.ok()?;
            Some((fname, bytes))
        }));
    }

    for h in handles {
        if let Ok(Some((fname, bytes))) = h.await {
            fs::write(mods_dir.join(&fname), &bytes)
                .map_err(|e| format!("Konnte Datei nicht schreiben ({}): {e}", fname))?;
        }
    }

    Ok(())
}

// Sucht unter den Versionen eines Mod-Projekts die beste Übereinstimmung zur
// Ziel-Minecraft-Version (und wenn bekannt zum Loader), sonst einfach die
// neueste Version. Best-effort, kein hartes Ausschlusskriterium.

fn pick_best_matching_version<'a>(
    versions: &'a [ModrinthVersionInfo],
    mc_version: &str,
    loader_kind: &str,
) -> Option<&'a ModrinthVersionInfo> {
    versions
        .iter()
        .find(|v| {
            v.game_versions.iter().any(|gv| gv == mc_version)
                && (loader_kind == "none"
                    || v.loaders.iter().any(|l| l.to_lowercase() == loader_kind))
        })
        .or_else(|| versions.iter().find(|v| v.game_versions.iter().any(|gv| gv == mc_version)))
        .or_else(|| versions.first())
}

// Ein Mod-Projekt im Resolver-Zustand: welche Version ist (probeweise) gewählt,
// und ob das ein neu hinzuzufügender Mod ist oder einer, der schon installiert war.
#[derive(Clone)]
struct ResolvedMod {
    project_id: String,
    project_name: String,
    version: ModrinthVersionInfo,
    is_new: bool,
    original_version_id: Option<String>, // None bei neuen Mods
}

enum Problem {
    Incompatible { a: String, b: String },
    NeedsVersion { project_id: String, required_version_id: String, requested_by: String },
    MissingRequired { project_id: String, version_id: Option<String>, requested_by: String }, // NEU
}

fn state_signature(state: &HashMap<String, ResolvedMod>) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = state.iter().map(|(k, m)| (k.clone(), m.version.id.clone())).collect();
    v.sort();
    v
}

// Prüft nur Abhängigkeiten ZWISCHEN Mods, die wir tatsächlich kennen (im state
// vorhanden sind) - ein "required" auf eine Mod, die gar nicht installiert
// ist, wird bewusst nicht als Problem gewertet, weil wir keine automatische
// Mod-Auto-Installation von Fremdabhängigkeiten machen (Scope-Entscheidung).
#[hotpath::measure]
fn find_problems(state: &HashMap<String, ResolvedMod>) -> Vec<Problem> {
    let mut problems = Vec::new();
    for (pid, rmod) in state.iter() {
        for dep in &rmod.version.dependencies {
            let Some(dep_pid) = &dep.project_id else { continue };

            match dep.dependency_type.as_str() {
                "incompatible" => {
                    if state.contains_key(dep_pid) {
                        problems.push(Problem::Incompatible { a: pid.clone(), b: dep_pid.clone() });
                    }
                }
                "required" => {
                    if !state.contains_key(dep_pid) {
                        // NEU: Abhängigkeit fehlt komplett -> als eigenes Problem melden
                        problems.push(Problem::MissingRequired {
                            project_id: dep_pid.clone(),
                            version_id: dep.version_id.clone(),
                            requested_by: rmod.project_name.clone(),
                        });
                    } else if let Some(req_version_id) = &dep.version_id {
                        let other = &state[dep_pid];
                        if &other.version.id != req_version_id {
                            problems.push(Problem::NeedsVersion {
                                project_id: dep_pid.clone(),
                                required_version_id: req_version_id.clone(),
                                requested_by: rmod.project_name.clone(),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }
    problems
}
/*fn find_problems(state: &HashMap<String, ResolvedMod>) -> Vec<Problem> {
    let mut problems = Vec::new();
    for (pid, rmod) in state.iter() {
        for dep in &rmod.version.dependencies {
            let Some(dep_pid) = &dep.project_id else { continue };
            if !state.contains_key(dep_pid) { continue; }

            match dep.dependency_type.as_str() {
                "incompatible" => {
                    problems.push(Problem::Incompatible { a: pid.clone(), b: dep_pid.clone() });
                }
                "required" => {
                    if let Some(req_version_id) = &dep.version_id {
                        let other = &state[dep_pid];
                        if &other.version.id != req_version_id {
                            problems.push(Problem::NeedsVersion {
                                project_id: dep_pid.clone(),
                                required_version_id: req_version_id.clone(),
                                requested_by: rmod.project_name.clone(),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }
    problems
}*/

// NEU: Umfassende Kompatibilitätsprüfung.
// Prüft, ob Version `v` mit der angegebenen Version eines anderen Projekts kompatibel ist.
// Berücksichtigt sowohl "incompatible" als auch "required" Dependencies mit spezifischen version_ids.

fn is_compatible_with(v: &ModrinthVersionInfo, other_project: &str, other_version_id: &str) -> bool {
    v.dependencies.iter().all(|d| {
        if d.project_id.as_deref() != Some(other_project) {
            return true; // Keine Abhängigkeit zu diesem Projekt, also hier kein Konflikt
        }
        
        match d.dependency_type.as_str() {
            "incompatible" => {
                match &d.version_id {
                    None => false, // Inkompatibel mit ALLEN Versionen dieses Projekts
                    Some(vid) => vid != other_version_id, // Nur inkompatibel, wenn es GENAU diese Version ist
                }
            }
            "required" => {
                match &d.version_id {
                    None => true, // Kompatibel mit JEDER Version dieses Projekts
                    Some(vid) => vid == other_version_id, // Nur kompatibel, wenn es GENAU diese Version ist
                }
            }
            _ => true, // "optional" oder "embedded" ignorieren wir für strikte Konflikte
        }
    })
}

// Sucht eine andere Version von `to_change`, die mit der AKTUELLEN Version von `other` 
// kreuzweise verträglich ist. Prüft beide Richtungen.

async fn find_alternate_compatible_version(
    to_change: &str,
    other: &str,
    state: &HashMap<String, ResolvedMod>,
    mc_version: &str,
    loader_kind: &str,
) -> Option<ModrinthVersionInfo> {
    let versions = fetch_pack_versions(to_change).await?;
    let current_id = state.get(to_change).map(|m| m.version.id.clone());
    let other_version = &state.get(other)?.version;

    // WICHTIG: .take(20) begrenzt die Suche auf die 20 neuesten Versionen.
    // Das verhindert Endlosschleifen oder extrem lange Ladezeiten und erfüllt 
    // genau deinen Wunsch, im Iris/Sodium-Fall max. 20 Versionen rückwärts zu gehen.
    let candidates = versions.into_iter()
        .filter(|v| v.game_versions.iter().any(|gv| gv == mc_version))
        .filter(|v| loader_kind == "none" || v.loaders.iter().any(|l| l.to_lowercase() == loader_kind))
        .filter(|v| Some(&v.id) != current_id.as_ref())
        .take(20);

    for v in candidates {
        // Bidirektionale Prüfung:
        // 1. Der Kandidat (z.B. Iris) darf die andere Mod (Sodium) nicht inkompatibel finden / eine andere Version erzwingen.
        // 2. Die andere Mod (Sodium) darf den Kandidaten (Iris) nicht inkompatibel finden.
        if is_compatible_with(&v, other, &other_version.id) 
           && is_compatible_with(other_version, to_change, &v.id) {
            return Some(v);
        }
    }
    None
}

// Gibt die EINE Version älter als die aktuell gewählte zurück (Modrinth liefert
// neueste zuerst, also: nächstes Element nach der aktuellen Position).
// None wenn es keine ältere (passende) Version mehr gibt.

async fn downgrade_project(
    project_id: &str,
    state: &HashMap<String, ResolvedMod>,
    mc_version: &str,
    loader_kind: &str,
) -> Option<ModrinthVersionInfo> {
    let versions = fetch_pack_versions(project_id).await?;
    let current_id = state.get(project_id)?.version.id.clone();

    let pos = versions.iter().position(|v| v.id == current_id)?;

    versions[pos + 1..]
        .iter()
        .find(|v| {
            v.game_versions.iter().any(|gv| gv == mc_version)
                && (loader_kind == "none" || v.loaders.iter().any(|l| l.to_lowercase() == loader_kind))
        })
        .cloned()
}

// Rekursiv (iterativ mit Memoisation) versuchen, einen widerspruchsfreien
// Zustand zu finden. Ok(state) = Lösung gefunden. Err(state) = keine Lösung,
// state zeigt den letzten (weiterhin fehlerhaften) Versuch für die UI.
#[hotpath::measure]
async fn resolve_compatibility(
    mut state: HashMap<String, ResolvedMod>,
    mc_version: &str,
    loader_kind: &str,
    ui_handle: &Weak<AppWindow>,
) -> Result<HashMap<String, ResolvedMod>, HashMap<String, ResolvedMod>> {
    let mut tried: std::collections::HashSet<Vec<(String, String)>> = std::collections::HashSet::new();
    const MAX_ITERATIONS: usize = 50;

    for iteration in 0..MAX_ITERATIONS {
        let signature = state_signature(&state);
        if tried.contains(&signature) {
            //push_debug_log(ui_handle, "Resolver: Zustand bereits versucht, breche ab.".to_string());
            return Err(state);
        }
        tried.insert(signature);

        let problems = find_problems(&state);
        if problems.is_empty() {
            //push_debug_log(ui_handle, format!("Resolver: Lösung gefunden nach {} Durchlauf/Durchläufen.", iteration + 1));
            return Ok(state);
        }

        //push_debug_log(ui_handle, format!("Resolver Durchlauf {}: {} Problem(e) gefunden.", iteration + 1, problems.len()));
        
        // Erst versuchen, ein "braucht andere Version"-Problem zu fixen (billiger als Konflikt-Suche).
        let mut fixed = false;
        let mut added_missing = false;
        for problem in &problems {
            if let Problem::MissingRequired { project_id, version_id, requested_by } = problem {
                //push_debug_log(ui_handle, format!("Fehlende Abhängigkeit: {} (benötigt von {}).", project_id, requested_by));

                let version = if let Some(vid) = version_id {
                    fetch_version_by_id(vid).await
                } else {
                    fetch_pack_versions(project_id).await
                        .and_then(|vs| pick_best_matching_version(&vs, mc_version, loader_kind).cloned())
                };

                let Some(version) = version else {
                    //push_debug_log(ui_handle, format!("Konnte keine Version für {} laden.", project_id));
                    continue;
                };

                let name = fetch_project_info(project_id).await.map(|p| p.title).unwrap_or_else(|| project_id.clone());

                state.insert(project_id.clone(), ResolvedMod {
                    project_id: project_id.clone(),
                    project_name: name,
                    version,
                    is_new: true,
                    original_version_id: None,
                });
                added_missing = true;
            }
            // --- HIER IST DIE VERBESSERTE LOGIK FÜR NEEDS VERSION ---
            if let Problem::NeedsVersion { project_id, required_version_id, requested_by } = problem {
                //push_debug_log(ui_handle, format!("Versionskonflikt: {} benötigt {} in Version {}.", requested_by, project_id, required_version_id));
                
                // SCHRITT 1: Zuerst versuchen, den anfordernden Mod (z.B. Iris) herunterzustufen,
                // damit er mit der AKTUELLEN Version von project_id (z.B. Sodium) kompatibel ist.
                // Das verhindert, dass wir Sodium upgraden und dadurch Konflikte mit Voxy erzeugen.
                if let Some(alternate) = find_alternate_compatible_version(requested_by, project_id, &state, mc_version, loader_kind).await {
                    //push_debug_log(ui_handle, format!("Lösung: Downgrade von {} auf {}, um mit {} kompatibel zu sein.", requested_by, alternate.version_number, project_id));
                    if let Some(existing) = state.get_mut(requested_by) {
                        existing.version = alternate;
                        fixed = true;
                        break; // Erfolgreich gefixt, nächste Resolver-Runde starten
                    }
                } else {
                    // SCHRITT 2 (Fallback): Wenn kein kompatibles Downgrade für requested_by gefunden wurde,
                    // versuchen wir immer noch, project_id auf die benötigte Version zu setzen.
                    //push_debug_log(ui_handle, format!("Kein kompatibles Downgrade für {} gefunden. Versuche Upgrade von {}.", requested_by, project_id));
                    if let Some(version) = fetch_version_by_id(required_version_id).await {
                        if let Some(existing) = state.get_mut(project_id) {
                            existing.version = version;
                            fixed = true;
                            break;
                        }
                    } else {
                        //push_debug_log(ui_handle, format!("Konnte Version {} nicht laden.", required_version_id));
                    }
                }
            }
        }
        if fixed { continue; }
        if added_missing { continue; }

        //push_debug_log(ui_handle, format!("Resolver Durchlauf {}: {} Problem(e) gefunden.", iteration + 1, problems.len()));

        let mut resolved = false;
        for problem in &problems {
            if let Problem::Incompatible { a, b } = problem {
                for to_change in [a, b] {
                    let other = if to_change == a { b } else { a };
                    //push_debug_log(ui_handle, format!(
                    //    "Inkompatibilität: {} <-> {}. Suche Alternative für {}.",
                    //    a, b, to_change
                    //));
                    if let Some(candidate) =
                        find_alternate_compatible_version(to_change, other, &state, mc_version, loader_kind).await
                    {
                        if let Some(existing) = state.get_mut(to_change) {
                            existing.version = candidate;
                            resolved = true;
                            break;
                        }
                    }
                }
                if resolved { break; }
            }
        }
        if resolved { continue; }

        // Fallback: Wenn weder Versions-Pinning noch Seitenwechsel helfen,
        // den "Störenfried" eine Version runtersetzen. Störenfried = das Projekt,
        // das am häufigsten als Verursacher (requested_by / Konfliktseite) auftaucht.
        let mut trouble_counts: HashMap<String, usize> = HashMap::new();
        for p in &problems {
            match p {
                Problem::NeedsVersion { project_id, requested_by, .. }
                | Problem::MissingRequired { project_id, requested_by, .. } => {
                    // Wer fordert hier etwas? Der FordERER ist meist das Problem.
                    *trouble_counts.entry(requested_by.clone()).or_insert(0) += 1;
                    let _ = project_id;
                }
                Problem::Incompatible { a, .. } => {
                    *trouble_counts.entry(a.clone()).or_insert(0) += 1;
                }
            }
        }

        if let Some((trouble_name, _)) = trouble_counts.into_iter().max_by_key(|(_, c)| *c) {
            // project_id zum Namen finden (requested_by speichert Namen, state keyed by id)
            if let Some(trouble_id) = state
                .values()
                .find(|m| m.project_name == trouble_name)
                .map(|m| m.project_id.clone())
            {
                //push_debug_log(ui_handle, format!(
                //    "Resolver: versuche Downgrade von {} ({})...", trouble_name, trouble_id
                //));
                if let Some(older) =
                    downgrade_project(&trouble_id, &state, mc_version, loader_kind).await
                {
                    if let Some(existing) = state.get_mut(&trouble_id) {
                        existing.version = older;
                        continue; // neuen Durchlauf mit der älteren Version starten
                    }
                }
                //push_debug_log(ui_handle, format!("Resolver: {} hat keine ältere Version mehr.", trouble_name));
            }
        }

        //push_debug_log(ui_handle, "Resolver: keine automatische Lösung mehr möglich.".to_string());
        return Err(state);
    }

    //push_debug_log(ui_handle, "Resolver: Iterationslimit erreicht.".to_string());
    Err(state)
}

// Lädt für ein Mod-Projekt die am besten passende Version und legt deren
// primäre Datei direkt in den mods-Ordner der Instanz. Kein .mrpack-Handling
// hier, da einzelne Mods normalerweise keine .mrpack-Dateien sind.
// Gibt bei Erfolg (Dateiname, Versionsnummer) zurück, damit der Aufrufer
// danach noch das Icon dazu cachen und in mod_icons.json eintragen kann.

async fn install_mod_into_instance(
    instance_dir: &PathBuf,
    project_id: &str,
    mc_version: &str,
    loader_kind: &str,
    ui_handle: &Weak<AppWindow>,
) -> Result<(String, String), String> {
    report_mod_progress(ui_handle, "Suche passende Version...".to_string());

    let versions = fetch_pack_versions(project_id)
        .await
        .ok_or_else(|| "Konnte Versionen nicht laden".to_string())?;

    let version = pick_best_matching_version(&versions, mc_version, loader_kind)
        .ok_or_else(|| "Keine Version gefunden".to_string())?;

    let file = version
        .files
        .iter()
        .find(|f| f.primary)
        .or_else(|| version.files.first())
        .ok_or_else(|| "Version hat keine Datei".to_string())?;

    report_mod_progress(ui_handle, format!("Lade {}...", file.filename));

    let bytes = download_bytes(&file.url).await?;

    let mods_dir = instance_dir.join("mods");
    fs::create_dir_all(&mods_dir).map_err(|e| format!("Konnte mods-Ordner nicht anlegen: {e}"))?;
    fs::write(mods_dir.join(&file.filename), &bytes)
        .map_err(|e| format!("Konnte Datei nicht schreiben: {e}"))?;

    Ok((file.filename.clone(), version.version_number.clone()))
}

// NEU, auf Modul-Ebene (vor struct InstanceCache):

/// Konvertiert rohe RGBA-Bytes in ein Slint-Image (nur UI-Thread).
#[hotpath::measure]
fn rgba_to_image(rgba: &[u8], w: u32, h: u32) -> Image {
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(w, h);
    buffer.make_mut_bytes().copy_from_slice(rgba);
    Image::from_rgba8(buffer)
}

/// Laedt ein PNG von Platte als rohe RGBA-Bytes (Send).
/// Die Icons sind bereits auf 96x96 verkleinert gecacht (siehe cache_icon),
/// deshalb ist hier KEIN Resize mehr noetig.
#[hotpath::measure]
fn load_icon_as_pixels(path: &Path) -> Option<(Vec<u8>, u32, u32)> {
    let img = image::open(path).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some((img.into_raw(), w, h))
}

// ===================== Instance-Cache (#2/#3/#8) =====================
// Hält pro Instanz die geladene Mod-Liste + Icons (als rohe RGBA-Bytes, weil
// slint::Image nicht Send ist). Wird beim ersten Öffnen befüllt und danach
// bei Filter-Tippen/Toggle nur aus dem Speicher gelesen.
struct InstanceCache {
    mods: Vec<ModFileEntry>,                       // rohe Einträge ohne Icon
    icons: HashMap<String, (Vec<u8>, u32, u32)>,   // display_name -> RGBA-Pixel
    loaded: bool,
}

// UI-thread-only cache for slint::Image objects. Image is not Send, so it can't
// live in the Send-safe InstanceCache — but list_mod_files_cached only ever
// runs on the UI thread anyway, so a thread-local is fine.
thread_local! {
    static UI_IMAGE_CACHE: std::cell::RefCell<HashMap<(String, String), Image>> =
        std::cell::RefCell::new(HashMap::new());
}

/// Holt das Slint-Image aus dem UI-Cache oder baut es einmalig aus den Pixeln.
/// Muss auf dem UI-Thread laufen.
fn cached_ui_image(
    instance_name: &str,
    display_name: &str,
    pixels: Option<&(Vec<u8>, u32, u32)>,
) -> Image {
    UI_IMAGE_CACHE.with(|cache| {
        let key = (instance_name.to_string(), display_name.to_string());
        let mut cache = cache.borrow_mut();
        if let Some(img) = cache.get(&key) {
            return img.clone();
        }
        let img = match pixels {
            Some((rgba, w, h)) => rgba_to_image(rgba, *w, *h),
            None => Image::default(),
        };
        cache.insert(key, img.clone());
        img
    })
}

static INSTANCE_CACHE: OnceLock<Mutex<HashMap<String, InstanceCache>>> = OnceLock::new();
#[hotpath::measure]
fn instance_cache() -> &'static Mutex<HashMap<String, InstanceCache>> {
    INSTANCE_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Cache für eine Instanz verwerfen (nach Toggle/Remove/Add).
#[hotpath::measure]
fn invalidate_instance_cache(instance_name: &str) {
    if let Ok(mut c) = instance_cache().lock() {
        c.remove(instance_name);
    }
}

/// Baut die Mod-Liste einer Instanz aus dem Cache (lädt sie beim ersten Mal).
/// MUSS auf dem UI-Thread laufen, weil slint::Image dort gebaut werden muss.
#[hotpath::measure]
fn list_mod_files_cached(instance_name: &str, instance_dir: &Path) -> Vec<ModFileInfo> {
    let mut cache = instance_cache().lock().unwrap();
    let entry = cache.entry(instance_name.to_string())
        .or_insert_with(|| InstanceCache { mods: Vec::new(), icons: HashMap::new(), loaded: false });

    if !entry.loaded {
        let mods = list_mod_files_all(instance_dir);
        let icons_json = load_mod_icons(instance_dir);

        let mut icons_px = HashMap::with_capacity(icons_json.len());
        for (name, path) in icons_json.iter() {
            if let Some(px) = load_icon_as_pixels(Path::new(path)) {
                icons_px.insert(name.clone(), px);
            }
        }

        entry.mods = mods;
        entry.icons = icons_px;
        entry.loaded = true;

        // Pixel haben sich geändert -> alte UI-Images für diese Instanz verwerfen.
        // Wir sind hier garantiert auf dem UI-Thread (siehe Doc-Kommentar oben).
        UI_IMAGE_CACHE.with(|c| {
            c.borrow_mut().retain(|(inst, _), _| inst != instance_name);
        });
    }

    // Nach Filter reduzieren + Slint-Images aus dem UI-Cache nehmen
    let filter = mod_filter().lock().unwrap().to_lowercase();
    entry.mods.iter()
        .filter(|e| filter.is_empty() || e.display_name.to_lowercase().contains(&filter))
        .map(|e| {
            let icon = cached_ui_image(
                instance_name,
                &e.display_name,
                entry.icons.get(&e.display_name),
            );
            ModFileInfo {
                filename: e.filename.clone().into(),
                display_name: e.display_name.clone().into(),
                enabled: e.enabled,
                icon,
                duplicate: e.duplicate,
                dup_total: e.dup_total,
                orphan: e.orphan,
            }
        })
        .collect()
}

// Alle Dateien im mods-Ordner, ungefiltert. Erkennt außerdem doppelte Mods: gleiche Projekt-ID
// (aus mods-list.json) bei mehr als einer aktiven Datei. Mods, die dort nicht stehen, werden
// noch nicht erkannt: die Erkennung ist nur so zuverlässig wie die mods-list.json.
#[hotpath::measure]
fn list_mod_files_all(instance_dir: &Path) -> Vec<ModFileEntry> {
    let mods_dir = instance_dir.join("mods");
    let Ok(entries) = fs::read_dir(&mods_dir) else { return Vec::new(); };

    let icons = load_mod_icons(instance_dir);
    let orphans = load_orphans(instance_dir);
    let pid_by_file: HashMap<String, String> = load_mods_list(instance_dir)
        .into_iter().map(|e| (e.filename, e.project_id)).collect();

    let mut result: Vec<ModFileEntry> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_file())
        .map(|entry| {
            let filename = entry.file_name().to_string_lossy().to_string();
            let enabled = !filename.ends_with(".disabled");
            let display_name = filename.strip_suffix(".disabled").unwrap_or(&filename).to_string();
            let icon_path = icons.get(&display_name).cloned();
            let orphan = orphans.contains(&display_name);
            ModFileEntry { filename, display_name, enabled, icon_path, duplicate: false, dup_total: 0, orphan }
        })
        .collect();

    // Projekt-ID -> Anzahl aktiver Dateien; > 1 bedeutet doppelt.
    let mut counts: HashMap<String, usize> = HashMap::new();
    for e in result.iter().filter(|e| e.enabled) {
        if let Some(pid) = pid_by_file.get(&e.display_name) {
            *counts.entry(pid.clone()).or_insert(0) += 1;
        }
    }
    for e in result.iter_mut().filter(|e| e.enabled) {
        if let Some(pid) = pid_by_file.get(&e.display_name) {
            e.duplicate = counts.get(pid).copied().unwrap_or(0) > 1;
        }
    }

    // Gesamtzahl der doppelten Dateien in JEDE Zeile schreiben: so kann Slint ohne Zusatz-Property prüfen, ob es welche gibt.
    let dup_count = result.iter().filter(|e| e.duplicate).count() as i32;
    for e in result.iter_mut() { e.dup_total = dup_count; }

    result.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    result
}

// Die Liste für die UI: zusätzlich nach dem Suchfeld gefiltert (nur Dateiname, keine weiteren Filter).
#[hotpath::measure]
fn list_mod_files_raw(instance_dir: &Path) -> Vec<ModFileEntry> {
    let mut all = list_mod_files_all(instance_dir);
    let filter = mod_filter().lock().unwrap().to_lowercase();
    if !filter.is_empty() {
        all.retain(|e| e.display_name.to_lowercase().contains(&filter));
    }
    all
}

// Baut aus einem ModFileEntry (reine Daten) das Slint-ModFileInfo inkl.
// geladenem Icon. MUSS auf dem UI-Thread aufgerufen werden.

fn build_mod_file_info(entry: ModFileEntry) -> ModFileInfo {
    let icon_path = entry.icon_path.map(PathBuf::from);
    ModFileInfo {
        filename: entry.filename.into(),
        display_name: entry.display_name.into(),
        enabled: entry.enabled,
        icon: load_icon(&icon_path),
        duplicate: entry.duplicate,
        dup_total: entry.dup_total,
        orphan: entry.orphan,
    }
}

// Aktiviert/deaktiviert einen Mod durch Umbenennen (".disabled"-Suffix an-/abhängen).

fn toggle_mod_file(instance_dir: &Path, filename: &str) {
    let mods_dir = instance_dir.join("mods");
    let old_path = mods_dir.join(filename);

    let new_path = if let Some(stripped) = filename.strip_suffix(".disabled") {
        mods_dir.join(stripped) // aktivieren
    } else {
        mods_dir.join(format!("{filename}.disabled")) // deaktivieren
    };

    if let Err(e) = fs::rename(&old_path, &new_path) {
        println!("Konnte Mod nicht umbenennen: {e}");
    }
}

fn remove_mod_file(instance_dir: &Path, filename: &str) {
    let path = instance_dir.join("mods").join(filename);
    if let Err(e) = fs::remove_file(&path) {
        println!("Konnte Mod nicht entfernen: {e}");
    }
}

// mod_icons.json: Zuordnung Mod-Dateiname (ohne ".disabled") -> Pfad zur
// gecachten Icon-Datei, liegt direkt im Instanz-Ordner.

fn load_mod_icons(instance_dir: &Path) -> HashMap<String, String> {
    let path = instance_dir.join("mod_icons.json");
    let Ok(file) = File::open(&path) else {
        return HashMap::new();
    };
    let reader = BufReader::new(file);
    serde_json::from_reader(reader).unwrap_or_default()
}

fn save_mod_icons(instance_dir: &Path, icons: &HashMap<String, String>) {
    if let Ok(json) = serde_json::to_string_pretty(icons) {
        if let Err(e) = fs::write(instance_dir.join("mod_icons.json"), json) {
            println!("Konnte mod_icons.json nicht speichern: {e}");
        }
    }
}

// Lädt die instance_overrides.json einer Instanz, oder Defaults falls noch keine existiert.

fn load_instance_overrides(instance_dir: &Path) -> InstanceOverrides {
    let path = instance_dir.join("instance_overrides.json");
    let Ok(file) = File::open(&path) else {
        return InstanceOverrides::default();
    };
    let reader = BufReader::new(file);
    serde_json::from_reader(reader).unwrap_or_default()
}

fn save_instance_overrides(instance_dir: &Path, overrides: &InstanceOverrides) {
    let json = serde_json::to_string_pretty(overrides).expect("Error Making Overrides json");
    if let Err(e) = fs::write(instance_dir.join("instance_overrides.json"), json) {
        println!("Konnte Overrides nicht speichern: {e}");
    }
}

// Lädt accounts.json (Offline- + Microsoft-Accounts), oder Defaults falls
// noch keine existiert.

fn load_accounts(launcher_dir: &Path) -> AccountsData {
    let path = launcher_dir.join("accounts.json");
    let Ok(file) = File::open(&path) else {
        return AccountsData::default();
    };
    let reader = BufReader::new(file);
    serde_json::from_reader(reader).unwrap_or_default()
}

fn save_accounts(launcher_dir: &Path, data: &AccountsData) {
    if let Ok(json) = serde_json::to_string_pretty(data) {
        let path = launcher_dir.join("accounts.json");
        if let Err(e) = fs::write(&path, json) {
            println!("Konnte accounts.json nicht speichern: {e}");
            return;
        }
        // Enthält bei Microsoft-Accounts Zugriffs-/Refresh-Tokens: nur für den eigenen User lesbar
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }
    }
}

// Baut aus AccountsData die Slint-Liste inkl. "selected"-Markierung.

fn accounts_to_ui_model(data: &AccountsData) -> Vec<AccountInfo> {
    data.accounts
        .iter()
        .map(|a| AccountInfo {
            id: a.id.clone().into(),
            username: a.username.clone().into(),
            kind: a.kind.clone().into(),
            selected: Some(&a.id) == data.selected_id.as_ref(),
        })
        .collect()
}

// Öffnet eine URL im Standardbrowser (ohne extra Crate). Unter Windows bewusst über rundll32,
// weil "cmd /C start" das "&" in der URL als Befehlstrenner behandeln würde.

fn open_in_browser(url: &str) {
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();

    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).spawn();

    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();

    if let Err(e) = result {
        println!("Konnte Browser nicht öffnen: {e}");
    }
}

// Öffnet einen Ordner im Dateimanager des Systems (gleiches Prinzip wie open_in_browser).
// spawn() wartet nicht auf den Dateimanager, der Launcher bleibt also bedienbar.

fn open_in_file_manager(path: &Path) {
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("explorer").arg(path).spawn();

    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(path).spawn();

    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(path).spawn();

    if let Err(e) = result {
        println!("Konnte Ordner nicht öffnen: {e}");
    }
}

// Holt den Auth-Code aus dem, was der Nutzer eingefügt hat: entweder die komplette Adresse der
// leeren Seite (".../oauth20_desktop.srf?code=XYZ&lc=1031") oder nur der Code selbst.

fn extract_auth_code(input: &str) -> Option<String> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }

    // Komplette Adresse: Query-Teil zerlegen und den "code"-Parameter suchen
    if input.contains("://") || input.contains('?') {
        let query = input.split_once('?').map(|(_, q)| q)?;
        let query = query.split('#').next().unwrap_or(query);
        return query
            .split('&')
            .find_map(|pair| pair.strip_prefix("code="))
            .filter(|c| !c.is_empty())
            .map(|c| c.to_string());
    }

    // Sonst hat der Nutzer direkt nur den Code eingefügt
    Some(input.to_string())
}

// Baut aus einem gespeicherten Microsoft-Account die AuthMethod für lyceris.
// Ist der Zugriffstoken abgelaufen, wird er per refresh_token erneuert und der
// Account in accounts.json aktualisiert.

async fn microsoft_auth_for(launcher_dir: &Path, mut acc: AccountEntry) -> Result<lyceris::AuthMethod, String> {
    if !lyceris::auth::microsoft::validate(acc.token_exp.unwrap_or(0)) {
        let refresh_token = acc
            .refresh_token
            .clone()
            .ok_or("Kein Refresh-Token gespeichert - bitte Account neu hinzufügen.")?;

        let fresh = lyceris::auth::microsoft::refresh(refresh_token, http_client())
            .await
            .map_err(|e| format!("Token-Erneuerung fehlgeschlagen: {e:?}"))?;

        acc.username = fresh.username;
        acc.uuid = Some(fresh.uuid);
        acc.xuid = Some(fresh.xuid);
        acc.access_token = Some(fresh.access_token);
        acc.refresh_token = Some(fresh.refresh_token);
        acc.token_exp = Some(fresh.exp);

        // Erneuerte Tokens sofort zurückschreiben, sonst wäre der alte Refresh-Token verbraucht
        let mut data = load_accounts(launcher_dir);
        if let Some(slot) = data.accounts.iter_mut().find(|a| a.id == acc.id) {
            *slot = acc.clone();
        }
        save_accounts(launcher_dir, &data);
    }

    Ok(lyceris::AuthMethod::Microsoft {
        username: acc.username,
        xuid: acc.xuid.unwrap_or_default(),
        uuid: acc.uuid.unwrap_or_default(),
        access_token: acc.access_token.unwrap_or_default(),
        refresh_token: acc.refresh_token.unwrap_or_default(),
    })
}

async fn latest_loader_version(kind: &str, mc: &str) -> Option<String> {
    match kind {
        "fabric" => {
            let list: Vec<LoaderEntry> =
                fetch("https://meta.fabricmc.net/v2/versions/loader".to_string(), None)
                    .await.ok()?;
            list.into_iter().find(|e| e.stable).map(|e| e.version)
        }
        "quilt" => {
            let list: Vec<LoaderEntry> =
                fetch("https://meta.quiltmc.org/v3/versions/loader".to_string(), None)
                    .await.ok()?;
            // Quilt liefert kein stable-Flag, deshalb Pre-Releases (mit "-") überspringen
            list.into_iter().find(|e| !e.version.contains('-')).map(|e| e.version)
        }
        "forge" => {
            #[derive(Deserialize)]
            struct Promos { promos: HashMap<String, String> }
            let p: Promos = fetch(
                "https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json"
                    .to_string(),
                None,
            ).await.ok()?;
            p.promos
                .get(&format!("{mc}-recommended"))
                .or_else(|| p.promos.get(&format!("{mc}-latest")))
                .cloned()
        }
        "neoforge" => {
            #[derive(Deserialize)]
            struct Versions { versions: Vec<String> }
            let v: Versions = fetch(
                "https://maven.neoforged.net/api/maven/versions/releases/net/neoforged/neoforge"
                    .to_string(),
                None,
            ).await.ok()?;
            let prefix = neoforge_prefix(mc)?;
            v.versions.into_iter()
                .filter(|v| v.starts_with(&prefix) && !v.contains('-'))
                .last()
        }
        _ => None,
    }
}

// 1.21.1 -> "21.1.", 1.21 -> "21.0."
// Ab Minecraft 26.x (neues Jahres-Schema ohne führendes "1.") wird der komplette
// mc-String 1:1 als Prefix übernommen, siehe match-Zweige unten.

fn neoforge_prefix(mc: &str) -> Option<String> {
    let parts: Vec<&str> = mc.split('.').collect();

    match parts.as_slice() {
        // Altes Schema: "1.21" -> "21.0.", "1.21.1" -> "21.1."
        ["1", minor] => Some(format!("{minor}.0.")),
        ["1", minor, patch] => Some(format!("{minor}.{patch}.")),

        // Neues Schema ab 2026: "26.1" -> "26.1.", "26.1.1" -> "26.1.1."
        [_year, _drop] => Some(format!("{mc}.")),
        [_year, _drop, _hotfix] => Some(format!("{mc}.")),

        _ => None,
    }
}

fn make_loader(kind: &str, version: String) -> Option<Box<dyn Loader>> {
    Some(match kind {
        "fabric" => Fabric(version).into(),
        "quilt" => Quilt(version).into(),
        "forge" => Forge(version).into(),
        "neoforge" => NeoForge(version).into(),
        _ => return None,
    })
}

// Bestimmt aus Modrinths "loaders"-Feld (z.B. ["fabric"] oder ["forge","neoforge"])
// welchen unserer bekannten Loader-Kinds wir nehmen. Erster Treffer gewinnt.
// Findet Modrinth keinen bekannten Loader (z.B. reines Datapack/Resourcepack), "none".

fn pick_loader_kind(modrinth_loaders: &[String]) -> String {
    let known = ["fabric", "forge", "neoforge", "quilt"];
    for loader in modrinth_loaders {
        let lower = loader.to_lowercase();
        if known.contains(&lower.as_str()) {
            return lower;
        }
    }
    "none".to_string()
}

// Patcht die Forge version.json im Cache, um minecraftArguments zu arguments zu konvertieren.
// Forge < 1.13 verwendet minecraftArguments (String), aber lyceris erwartet arguments (JSON-Objekt).

fn patch_forge_version_json(launcher_dir: &Path, mc_version: &str, loader_version: &str) {
    let version_json_path = launcher_dir
        .join("shared")
        .join(".forge")
        .join("profiles")
        .join(format!("{}-{}", mc_version, loader_version))
        .join(format!("version-{}-{}.json", mc_version, loader_version));
    
    if !version_json_path.exists() {
        // Ausgabe hilft, falls der von uns angenommene Pfad nicht zu lyceris passt
        println!("⚠️ Forge version.json nicht gefunden unter: {}", version_json_path.display());
        return;
    }
    
    let content = match fs::read_to_string(&version_json_path) {
        Ok(c) => c,
        Err(_) => return,
    };
    
    // Bereits gepatcht? Dann nichts tun
    if content.contains("\"arguments\"") {
        return;
    }
    
    // minecraftArguments zu arguments konvertieren
    if let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&content) {
        if let Some(mc_args) = json.get("minecraftArguments").and_then(|v| v.as_str()) {
            let args_array: Vec<serde_json::Value> = mc_args
                .split_whitespace()
                .map(|s| serde_json::Value::String(s.to_string()))
                .collect();
            
            json["arguments"] = serde_json::json!({
                "game": args_array,
                "jvm": []
            });
            
            if let Ok(new_content) = serde_json::to_string_pretty(&json) {
                if let Err(e) = fs::write(&version_json_path, new_content) {
                    println!("⚠️ Konnte version.json nicht patchen: {}", e);
                } else {
                    println!("✅ Forge version.json erfolgreich gepatcht für {}-{}", mc_version, loader_version);
                }
            }
        }
    }
}

// Ergänzt in der Start-JSON (shared/versions/<mc>-<forge>/<mc>-<forge>.json) die "--tweakClass"-Argumente
// aus dem Forge-Profil. Lyceris übernimmt bei altem Forge (<= 1.12.2) nur mainClass und Bibliotheken,
// aber nicht die Startargumente. Ohne FMLTweaker startet stattdessen der VanillaTweaker und das Spiel
// findet die (obfuskierten) Minecraft-Klassen nicht.
// Ist idempotent: steht "--tweakClass" schon drin, passiert nichts. Bei modernem Forge
// (JSON mit "arguments" statt "minecraftArguments") tut die Funktion ebenfalls nichts.

fn patch_forge_launch_args(launcher_dir: &Path, mc_version: &str, loader_version: &str) {
    let id = format!("{}-{}", mc_version, loader_version);

    let profile_path = launcher_dir
        .join("shared/.forge/profiles")
        .join(&id)
        .join(format!("version-{}.json", id));
    let target_path = launcher_dir
        .join("shared/versions")
        .join(&id)
        .join(format!("{}.json", id));

    // 1. Tweak-Argumente aus dem Forge-Profil holen (Original-Feld minecraftArguments)
    let Ok(profile_str) = fs::read_to_string(&profile_path) else {
        println!("⚠️ Forge-Profil nicht gefunden: {}", profile_path.display());
        return;
    };
    let Ok(profile_json) = serde_json::from_str::<serde_json::Value>(&profile_str) else { return; };
    let Some(forge_args) = profile_json.get("minecraftArguments").and_then(|v| v.as_str()) else { return; };

    // Alle Paare "--tweakClass <Klasse>" einsammeln
    let tokens: Vec<&str> = forge_args.split_whitespace().collect();
    let mut extra: Vec<String> = Vec::new();
    for i in 0..tokens.len().saturating_sub(1) {
        if tokens[i] == "--tweakClass" {
            extra.push(format!("--tweakClass {}", tokens[i + 1]));
        }
    }
    if extra.is_empty() { return; }

    // 2. In die Start-JSON einfügen
    let Ok(target_str) = fs::read_to_string(&target_path) else {
        println!("⚠️ Start-JSON nicht gefunden: {}", target_path.display());
        return;
    };
    let Ok(mut target_json) = serde_json::from_str::<serde_json::Value>(&target_str) else { return; };

    let Some(current) = target_json.get("minecraftArguments").and_then(|v| v.as_str()).map(|s| s.to_string()) else {
        return; // kein minecraftArguments -> modernes Format, hier nichts zu tun
    };
    if current.contains("--tweakClass") {
        return; // schon gepatcht
    }

    target_json["minecraftArguments"] = serde_json::Value::String(format!("{} {}", current, extra.join(" ")));

    match serde_json::to_string(&target_json) {
        Ok(new_content) => match fs::write(&target_path, new_content) {
            Ok(_) => println!("✅ Forge-Tweaker in Start-JSON ergänzt: {}", extra.join(" ")),
            Err(e) => println!("⚠️ Konnte Start-JSON nicht schreiben: {}", e),
        },
        Err(e) => println!("⚠️ Konnte Start-JSON nicht serialisieren: {}", e),
    }
}

#[hotpath::measure]
async fn install_cf_modpack(
    instance_dir: &PathBuf,
    zip_bytes: Vec<u8>,
    ui_handle: &Weak<AppWindow>,
) -> Result<(String, String), String> {
    let dir = instance_dir.clone();
    let ui_for_progress = ui_handle.clone();

    let (mc_version, loader, cf_files) = tokio::task::spawn_blocking(
        move || -> Result<(String, String, Vec<CfManifestFile>), String> {
            let cursor = Cursor::new(zip_bytes);
            let mut archive = zip::ZipArchive::new(cursor)
                .map_err(|e| format!("Ungültige Zip-Datei: {e}"))?;

            let manifest: CfManifest = {
                let mut mf = archive.by_name("manifest.json")
                    .map_err(|_| "Kein manifest.json gefunden. Ist das ein CF-Modpack?".to_string())?;
                let mut s = String::new();
                mf.read_to_string(&mut s).map_err(|e| format!("Manifest Lesefehler: {e}"))?;
                serde_json::from_str(&s).map_err(|e| format!("Manifest Parse Fehler: {e}"))?
            };

            let mc_version = manifest.minecraft.version.clone();
            let loader = manifest.minecraft.modLoaders.first()
                .map(|l| l.id.clone())
                .unwrap_or_else(|| "none".to_string());
            let files = manifest.files;

            let _ = ui_for_progress.upgrade_in_event_loop(|ui| {
                ui.set_install_popup_status("Entpacke CurseForge Overrides...".into());
            });

            for i in 0..archive.len() {
                let mut entry = archive.by_index(i).map_err(|e| format!("Zip-Fehler: {e}"))?;
                let name = entry.name().to_string();
                if let Some(rel_path) = name.strip_prefix("overrides/") {
                    if rel_path.is_empty() { continue; }
                    let out_path = dir.join(rel_path);
                    if entry.is_dir() {
                        fs::create_dir_all(&out_path).ok();
                    } else {
                        if let Some(parent) = out_path.parent() { fs::create_dir_all(parent).ok(); }
                        let mut out_file = File::create(&out_path)
                            .map_err(|e| format!("Datei erstellen: {e}"))?;
                        std::io::copy(&mut entry, &mut out_file).ok();
                    }
                }
            }

            Ok((mc_version, loader, files))
        }
    )
    .await
    .map_err(|e| format!("Interner Fehler: {e}"))??;

    let mods_dir = instance_dir.join("mods");
    fs::create_dir_all(&mods_dir).map_err(|e| format!("mods-Ordner anlegen: {e}"))?;

    let total = cf_files.len();
    let dl_concurrency = tier_download_concurrency(device_tier());
    let sem = Arc::new(tokio::sync::Semaphore::new(dl_concurrency));

    let mut handles = Vec::with_capacity(total);
    for (i, file) in cf_files.iter().enumerate() {
        let file_id = file.fileID;
        let project_id = file.projectID;
        let ui = ui_handle.clone();
        let sem = sem.clone();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.ok()?;
            report_progress(&ui, format!("Lade Mod {}/{} herunter...", i + 1, total));
            match download_cf_file(file_id).await {
                Ok((filename, bytes)) => Some((filename, bytes, project_id)),
                Err(e) => {
                    println!("⚠️ Mod-Download übersprungen (Projekt-ID: {}, Datei-ID: {}): {}",
                        project_id, file_id, e);
                    None
                }
            }
        }));
    }

    let mut mods_list: Vec<ModListEntry> = Vec::new();
    for h in handles {
        if let Ok(Some((filename, bytes, project_id))) = h.await {
            fs::write(mods_dir.join(&filename), &bytes)
                .map_err(|e| format!("Mod speichern fehlgeschlagen: {e}"))?;
            mods_list.push(ModListEntry {
                filename,
                project_id: project_id.to_string(),
            });
        }
    }

    save_mods_list(instance_dir, &mods_list);
    Ok((mc_version, loader))
}

// ===================== Modpack-Update (Feature 1) =====================

// Diese Dateien gehören dem Nutzer und werden nie überschrieben (wenn sie existieren).
const KEEP_ALWAYS: [&str; 4] = ["options.txt", "servers.dat", "optionsof.txt", "optionsshaders.txt"];

// Alles, was zwischen "Prüfen" und "Durchführen" gemerkt werden muss.
struct UpdatePlan {
    instance: String,
    mrpack_bytes: Vec<u8>,
    new_hashes: HashMap<String, String>,
    auto_new: Vec<String>,             // wird ohne Rückfrage geschrieben
    conflicts: Vec<String>,            // Nutzer entscheidet im Popup
    to_download: Vec<(String, String)>,// (Pfad, URL) neue Mods
    to_remove: Vec<String>,            // Mods, die das alte Pack lieferte und das neue nicht mehr
    all_mod_paths: Vec<String>,
    minecraft_version: String,
    old_mc: String,
    loader: String,
    loader_version: Option<String>,
    version_id: String,
    had_snapshot: bool,
}

struct UpdateState {
    versions: Vec<ModrinthVersionInfo>, // Versionsliste für das ComboBox-Model (Index -> Version)
    plan: Option<UpdatePlan>,
}

static UPDATE_STATE: OnceLock<Mutex<UpdateState>> = OnceLock::new();
fn update_state() -> &'static Mutex<UpdateState> {
    UPDATE_STATE.get_or_init(|| Mutex::new(UpdateState { versions: Vec::new(), plan: None }))
}

// Hashes aller Dateien unter overrides/ bzw. client-overrides/ im .mrpack.
fn read_mrpack_override_hashes(bytes: &[u8]) -> Result<HashMap<String, String>, String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|e| format!("Kein gültiges .mrpack: {e}"))?;
    let mut out = HashMap::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| format!("Zip-Fehler: {e}"))?;
        if entry.is_dir() { continue; }
        let name = entry.name().to_string();
        let Some(rel) = name.strip_prefix("overrides/").or_else(|| name.strip_prefix("client-overrides/")) else { continue; };
        if rel.is_empty() { continue; }
        let mut data = Vec::new();
        entry.read_to_end(&mut data).map_err(|e| format!("Lesefehler: {e}"))?;
        out.insert(rel.to_string(), fnv64(&data));
    }
    Ok(out)
}

// "Prüfen" (Modrinth): lädt die .mrpack der gewählten Version und baut den Plan.
async fn build_update_plan(instance_name: &str, version: &ModrinthVersionInfo) -> Result<UpdatePlan, String> {
    let file = version.files.iter()
        .find(|f| f.primary && f.filename.ends_with(".mrpack"))
        .or_else(|| version.files.iter().find(|f| f.filename.ends_with(".mrpack")))
        .ok_or("Diese Version enthält keine .mrpack-Datei.")?;

    let bytes = download_bytes(&file.url).await?;
    build_update_plan_from_bytes(instance_name, bytes, version.id.clone()).await
}

// Eigentliche Plan-Berechnung. Funktioniert mit jeder .mrpack (Download ODER lokale Datei).
// Ändert nichts auf der Platte. version_id leer = origin_version_id der Instanz bleibt unverändert.
async fn build_update_plan_from_bytes(instance_name: &str, bytes: Vec<u8>, version_id: String) -> Result<UpdatePlan, String> {
    let dir = launcher_dir().join("instances").join(instance_name);
    let cfg = load_instance_config(&dir).ok_or("instance.json fehlt.")?;

    let index = read_mrpack_index(&bytes)?;
    let (mc, loader) = mrpack_mc_and_loader(&index.dependencies);
    let loader_version = mrpack_loader_version(&index.dependencies, &loader);

    // Hashes der neuen Override-Dateien (blocking -> eigener Thread)
    let b = bytes.clone();
    let new_hashes = tokio::task::spawn_blocking(move || read_mrpack_override_hashes(&b))
        .await.map_err(|e| format!("Interner Fehler: {e}"))??;

    let old = load_pack_snapshot(&dir);

    // Config-Dateien klassifizieren
    let (nh, d2, old2) = (new_hashes.clone(), dir.clone(), old.clone());
    let (auto_new, conflicts) = tokio::task::spawn_blocking(move || {
        let mut auto_new: Vec<String> = Vec::new();
        let mut conflicts: Vec<String> = Vec::new();
        for (rel, new_hash) in nh.iter() {
            if rel.starts_with("saves/") || rel.contains("..") { continue; } // Welten nie anfassen
            let path = d2.join(rel);
            if !path.exists() { auto_new.push(rel.clone()); continue; }
            if KEEP_ALWAYS.contains(&rel.as_str()) { continue; }
            let Ok(cur) = fs::read(&path) else { continue; };
            let cur_hash = fnv64(&cur);
            if &cur_hash == new_hash { continue; } // identisch
            match old2.as_ref().and_then(|o| o.overrides.get(rel)) {
                Some(old_hash) if &cur_hash == old_hash => auto_new.push(rel.clone()), // du hast nichts geändert
                Some(old_hash) if new_hash == old_hash => {}                           // Pack unverändert, deine bleibt
                _ => conflicts.push(rel.clone()),                                       // beide geändert / unbekannt
            }
        }
        auto_new.sort();
        conflicts.sort();
        (auto_new, conflicts)
    }).await.map_err(|e| format!("Interner Fehler: {e}"))?;

    // Mods: nur fehlende laden. Ein deaktivierter Mod (".disabled") zählt als vorhanden.
    let mut to_download = Vec::new();
    let mut all_mod_paths = Vec::new();
    for f in &index.files {
        all_mod_paths.push(f.path.clone());
        let Some(url) = f.downloads.first() else { continue; };
        if f.path.contains("..") { continue; }
        let exists = dir.join(&f.path).exists() || dir.join(format!("{}.disabled", f.path)).exists();
        if !exists { to_download.push((f.path.clone(), url.clone())); }
    }

    // Mods, die das ALTE Pack lieferte und das neue nicht mehr
    let new_set: std::collections::HashSet<String> = all_mod_paths.iter().cloned().collect();
    let to_remove: Vec<String> = old.as_ref().map(|o| {
        o.mod_paths.iter()
            .filter(|p| !new_set.contains(*p) && dir.join(p).exists())
            .cloned().collect()
    }).unwrap_or_default();

    Ok(UpdatePlan {
        instance: instance_name.to_string(),
        mrpack_bytes: bytes,
        new_hashes,
        auto_new,
        conflicts,
        to_download,
        to_remove,
        all_mod_paths,
        minecraft_version: mc,
        old_mc: cfg.minecraft_version.clone(),
        loader,
        loader_version,
        version_id,
        had_snapshot: old.is_some(),
    })
}

fn manage_status_post(ui: &Weak<AppWindow>, text: String) {
    let _ = ui.upgrade_in_event_loop(move |ui| ui.set_manage_status(text.into()));
}

// "Durchführen": schreibt Dateien. keep_mine = Konflikte, bei denen der Nutzer SEINE Datei behalten will.
async fn apply_update_plan(
    plan: UpdatePlan,
    keep_mine: std::collections::HashSet<String>,
    ui: &Weak<AppWindow>,
) -> Result<String, String> {
    let dir = launcher_dir().join("instances").join(&plan.instance);

    // 1. Config-Dateien: automatische + vom Nutzer auf "neue Datei" gestellte Konflikte
    manage_status_post(ui, "Update: schreibe Config-Dateien...".into());
    let write_set: std::collections::HashSet<String> = plan.auto_new.iter().cloned()
        .chain(plan.conflicts.iter().filter(|c| !keep_mine.contains(*c)).cloned())
        .collect();
    let (bytes, d2) = (plan.mrpack_bytes.clone(), dir.clone());
    let written = tokio::task::spawn_blocking(move || -> Result<usize, String> {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("Zip: {e}"))?;
        let mut n = 0;
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).map_err(|e| format!("Zip-Fehler: {e}"))?;
            if entry.is_dir() { continue; }
            let name = entry.name().to_string();
            let Some(rel) = name.strip_prefix("overrides/").or_else(|| name.strip_prefix("client-overrides/")) else { continue; };
            if !write_set.contains(rel) { continue; }
            let out = d2.join(rel);
            if let Some(p) = out.parent() { fs::create_dir_all(p).map_err(|e| format!("Ordner: {e}"))?; }
            let mut f = File::create(&out).map_err(|e| format!("Datei anlegen: {e}"))?;
            std::io::copy(&mut entry, &mut f).map_err(|e| format!("Schreiben: {e}"))?;
            n += 1;
        }
        Ok(n)
    }).await.map_err(|e| format!("Interner Fehler: {e}"))??;

    // 2. Mods, die das neue Pack nicht mehr enthält
    for p in &plan.to_remove {
        let _ = fs::remove_file(dir.join(p));
        let _ = fs::remove_file(dir.join(format!("{p}.disabled")));
        mods_list_remove(&dir, p.rsplit('/').next().unwrap_or(p));
    }

    // 3. Neue Mods parallel laden (gleiches Muster wie install_mrpack)
    let total = plan.to_download.len();
    let sem = Arc::new(tokio::sync::Semaphore::new(tier_download_concurrency(device_tier())));
    let mut handles = Vec::with_capacity(total);
    for (i, (path, url)) in plan.to_download.iter().cloned().enumerate() {
        let (sem, ui2) = (sem.clone(), ui.clone());
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.ok()?;
            manage_status_post(&ui2, format!("Update: lade Mod {}/{}: {}", i + 1, total, path));
            let bytes = download_bytes(&url).await.ok()?;
            Some((path, url, bytes))
        }));
    }
    let mut downloaded = 0;
    for h in handles {
        if let Ok(Some((path, url, bytes))) = h.await {
            let out = dir.join(&path);
            if let Some(p) = out.parent() { let _ = fs::create_dir_all(p); }
            if fs::write(&out, &bytes).is_ok() {
                downloaded += 1;
                if let Some(pid) = extract_modrinth_project_id(&url) {
                    let filename = path.rsplit('/').next().unwrap_or(&path).to_string();
                    mods_list_upsert(&dir, &pid, None, &filename);
                }
            }
        }
    }

    // 4. Snapshot + instance.json auf die neue Version setzen
    save_pack_snapshot(&dir, &PackSnapshot { overrides: plan.new_hashes.clone(), mod_paths: plan.all_mod_paths.clone() });
    let mut cfg = load_instance_config(&dir).ok_or("instance.json fehlt.")?;
    if !plan.minecraft_version.is_empty() { cfg.minecraft_version = plan.minecraft_version.clone(); }
    if plan.loader != "none" { cfg.loader = plan.loader.clone(); }
    cfg.loader_version = plan.loader_version.clone(); // Loader wird beim nächsten Start installiert
    if !plan.version_id.is_empty() {
        cfg.origin_version_id = Some(plan.version_id.clone());
    }
    save_instance_config(&dir, &cfg)?;

    // 5. UI: Kachel + Mod-Liste aktualisieren
    invalidate_instance_cache(&plan.instance);
    let (iname, idir) = (plan.instance.clone(), dir.clone());
    let _ = ui.upgrade_in_event_loop(move |ui| {
        with_packs_model(&ui, |vm| {
            if let Some(i) = find_tile_row(vm, &iname) { vm.set_row_data(i, tile_from_config(&cfg)); }
        });
        if ui.get_detail_instance_name().as_str() == iname {
            ui.set_detail_mod_files(ModelRc::new(VecModel::from(list_mod_files_cached(&iname, &idir))));
        }
        ui.set_manage_update_pending(false);
        ui.set_manage_update_report("".into());
    });

    Ok(format!(
        "Update fertig: {} Config-Dateien geschrieben, {} Mods geladen, {} entfernt. Welten blieben unberührt.",
        written, downloaded, plan.to_remove.len()
    ))
}

// Startet apply_update_plan im Hintergrund (nimmt den gemerkten Plan aus dem State).
fn start_update(wh: Weak<AppWindow>, hdl: tokio::runtime::Handle, keep_mine: std::collections::HashSet<String>) {
    let Some(plan) = update_state().lock().unwrap().plan.take() else { return; };
    manage_status_post(&wh, "Update läuft...".into());
    hdl.spawn(async move {
        match apply_update_plan(plan, keep_mine, &wh).await {
            Ok(msg) => manage_status_post(&wh, msg),
            Err(e) => manage_status_post(&wh, format!("Update fehlgeschlagen: {e}")),
        }
    });
}

// Nimmt den gestarteten Minecraft-Prozess und spiegelt dessen Ausgabe live ins Terminal.
// stdout und stderr werden jeweils in einem eigenen Task gelesen, damit keiner den
// anderen blockiert. Danach wartet die Funktion, bis das Spiel beendet wird.

async fn stream_minecraft_log(mut child: tokio::process::Child) {
    // take() holt den Pipe-Handle aus dem Child heraus (bleibt None, falls lyceris
    // die Ausgabe nicht umleitet, dann erbt der Prozess das Terminal ohnehin).
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(async move {
            let mut lines = TokioBufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                println!("[MC] {line}");
            }
        });
    }

    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = TokioBufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                eprintln!("[MC ERR] {line}");
            }
        });
    }

    // Auf das Ende des Spiels warten und den Exit-Status melden
    match child.wait().await {
        Ok(status) => println!("Minecraft beendet: {status}"),
        Err(e) => println!("Fehler beim Warten auf Minecraft: {e}"),
    }
}

// Sucht eine systemweit installierte GLFW an den üblichen Orten der großen Distributionen.
// Gibt None zurück, wenn keine gefunden wird, dann bleibt es bei der GLFW, die Minecraft
// selbst mitbringt (das ist bei den meisten Systemen völlig in Ordnung).
#[cfg(target_os = "linux")]
fn find_system_glfw() -> Option<&'static str> {
    const CANDIDATES: [&str; 6] = [
        "/usr/lib/libglfw.so",                      // Arch / CachyOS
        "/usr/lib/libglfw.so.3",
        "/usr/lib64/libglfw.so.3",                  // Fedora / openSUSE
        "/usr/lib/x86_64-linux-gnu/libglfw.so.3",   // Debian / Ubuntu
        "/usr/lib/aarch64-linux-gnu/libglfw.so.3",
        "/usr/local/lib/libglfw.so",
    ];
    CANDIDATES.iter().copied().find(|p| Path::new(p).exists())
}

// Laufende Instanzen: Instanzname -> Sender, mit dem wir dem Warte-Task "kill!" zurufen.
// Arc<Mutex<..>>, weil UI-Callbacks und Tokio-Tasks darauf zugreifen.
type RunningMap = Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>>;

// Baut die Liste für die untere Leiste neu auf. Läuft über den Event-Loop, weil
// Slint-Modelle nur im UI-Thread angefasst werden dürfen.

fn refresh_running_ui(ui_handle: &Weak<AppWindow>, running: &RunningMap) {
    let mut names: Vec<String> = running.lock().unwrap().keys().cloned().collect();
    names.sort();
    let _ = ui_handle.upgrade_in_event_loop(move |ui| {
        let items: Vec<RunningInstance> = names
            .into_iter()
            .map(|n| RunningInstance { name: n.into() })
            .collect();
        ui.set_running_instances(ModelRc::new(VecModel::from(items)));
    });
}

async fn run_and_track(
    mut child: tokio::process::Child,
    name: String,
    running: RunningMap,
    ui_handle: Weak<AppWindow>,
) {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    running.lock().unwrap().insert(name.clone(), tx);
    refresh_running_ui(&ui_handle, &running);
    let started = std::time::Instant::now();
    let started_sys = std::time::SystemTime::now(); // NEU: für die Absturz-Analyse (nur Reports NACH diesem Start zählen)

    // Flag setzen, damit parallel_limit() sich drosselt
    MC_RUNNING.store(true, AtomicOrdering::Relaxed);

    // Launcher minimieren, solange gespielt wird (Einstellung "close_after_launch", siehe oben).
    let minimize = load_settings(&launcher_dir()).close_after_launch;
    if minimize {
        let _ = ui_handle.upgrade_in_event_loop(|ui| ui.window().set_minimized(true));
    }

    // Wie ging das Spiel zu Ende? Für die Benachrichtigung.
    let mut crashed: Option<String> = None;
    let mut stopped_by_user = false;

    tokio::select! {
        result = child.wait() => match result {
            Ok(status) => {
                println!("✅ Minecraft beendet: {status}");
                if !status.success() {
                    // Exit-Code != 0 = Crash (oder vom System beendet)
                    crashed = Some(match status.code() {
                        Some(c) => format!("Exit-Code {c}"),
                        None => "vom System beendet".to_string(),
                    });
                }
            }
            Err(e) => {
                println!("❌ Fehler beim Warten auf Minecraft: {e}");
                crashed = Some(format!("Fehler: {e}"));
            }
        },
        Ok(()) = rx => {
            println!("🛑 Force Stop: {name}");
            let _ = child.kill().await;
            stopped_by_user = true;
        }
    }

    MC_RUNNING.store(false, AtomicOrdering::Relaxed);

    let secs = started.elapsed().as_secs();
    add_playtime_secs(&launcher_dir().join("instances").join(&name), secs);
    let _ = ui_handle.upgrade_in_event_loop(|ui| { ui.set_playtime_tick(ui.get_playtime_tick() + 1); });

    running.lock().unwrap().remove(&name);
    refresh_running_ui(&ui_handle, &running);

    // Benachrichtigung
    let did_crash = crashed.is_some(); // NEU: muss VOR dem "if let Some(reason) = crashed" stehen, das den Wert verbraucht
    let played = format_playtime(secs);
    if let Some(reason) = crashed {
        notify_desktop("Minecraft abgestürzt", &format!("{name}: {reason} nach {played}. Details in der Live-Konsole bzw. crash-reports/."));
    } else if stopped_by_user {
        notify_desktop("Minecraft gestoppt", &format!("{name} wurde per Force Stop beendet ({played})."));
    } else {
        notify_desktop("Minecraft beendet", &format!("{name}: {played} gespielt."));
    }

    // NEU: Nach einem Crash automatisch analysieren und das Ergebnis-Popup öffnen
    if did_crash {
        show_crash_analysis(ui_handle.clone(), name.clone(), Some(started_sys)).await;
    }

    // Fenster zurückholen, aber nur wenn keine andere Instanz mehr läuft
    let none_left = running.lock().unwrap().is_empty();
    if minimize && none_left {
        show_main_window(&ui_handle);
    }
}

// Fenster wieder anzeigen (aus dem Minimieren holen). Darf aus jedem Thread aufgerufen werden.
fn show_main_window(ui: &Weak<AppWindow>) {
    let _ = ui.upgrade_in_event_loop(|ui| {
        let w = ui.window();
        w.set_minimized(false);
        let _ = w.show();
    });
}

// Tray-Icon über StatusNotifierItem (KDE funktioniert direkt, GNOME braucht die
// "AppIndicator"-Extension). Läuft in einem eigenen Thread, blockiert die UI also nicht.
#[cfg(target_os = "linux")]
struct LauncherTray { ui: Weak<AppWindow> }

#[cfg(target_os = "linux")]
impl ksni::Tray for LauncherTray {
    fn id(&self) -> String { "srusm-launcher".into() }
    fn title(&self) -> String { "SRUSM Launcher".into() }
    fn icon_name(&self) -> String { "applications-games".into() } // Standard-Icon aus dem Icon-Theme

    // Linksklick auf das Tray-Icon
    fn activate(&mut self, _x: i32, _y: i32) { show_main_window(&self.ui); }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        vec![
            StandardItem {
                label: "Launcher anzeigen".into(),
                activate: Box::new(|t: &mut LauncherTray| show_main_window(&t.ui)),
                ..Default::default()
            }.into(),
            MenuItem::Separator,
            StandardItem {
                label: "Beenden".into(),
                activate: Box::new(|_| { let _ = slint::quit_event_loop(); }),
                ..Default::default()
            }.into(),
        ]
    }
}

// ===================== Block 1: Misc-Einstellungen, Spielzeit, Presets =====================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EnvVar { name: String, value: String }

// misc.json im Instanz-Ordner. Struct-weites #[serde(default)]: fehlende Felder in alten
// Dateien werden mit Default::default() gefüllt, das Laden scheitert also nie an neuen Feldern.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct InstanceMisc {
    jvm_args: String,            // freie zusätzliche JVM-Argumente (mit Leerzeichen getrennt)
    jvm_preset: String,          // id aus JVM_PRESETS, "none" = kein Preset
    env_vars: Vec<EnvVar>,       // zusätzliche Umgebungsvariablen
    wrappers: Vec<String>,       // ids der aktivierten Wrapper (gamemode, mangohud, ...)
    use_system_glfw: bool,       // Linux: System-GLFW statt der mitgelieferten (Standard: an)
    quick_enabled: bool,         // Quick Play beim Start?
    quick_server: String,        // Server-Adresse (hat Vorrang vor der Welt)
    quick_world: String,         // Weltname
    auto_fix_duplicates: bool,   // Duplikate automatisch beheben erlaubt (Standard: verboten)
}

impl Default for InstanceMisc {
    fn default() -> Self {
        Self {
            jvm_args: String::new(),
            jvm_preset: "none".to_string(),
            env_vars: Vec::new(),
            wrappers: Vec::new(),
            use_system_glfw: true,
            quick_enabled: false,
            quick_server: String::new(),
            quick_world: String::new(),
            auto_fix_duplicates: false,
        }
    }
}

fn load_misc(instance_dir: &Path) -> InstanceMisc {
    let Ok(file) = File::open(instance_dir.join("misc.json")) else { return InstanceMisc::default(); };
    serde_json::from_reader(BufReader::new(file)).unwrap_or_default()
}

fn save_misc(instance_dir: &Path, misc: &InstanceMisc) {
    if let Ok(json) = serde_json::to_string_pretty(misc) {
        if let Err(e) = fs::write(instance_dir.join("misc.json"), json) {
            println!("Konnte misc.json nicht speichern: {e}");
        }
    }
}

// Kurzform für den Launcher-Ordner (die neuen Teile nutzen ihn oft).

fn launcher_dir() -> PathBuf {
    dirs::data_local_dir().expect("kein Local Data Dir").join("srusm")
}

fn load_instance_config(instance_dir: &Path) -> Option<ModpackJsonData> {
    let file = File::open(instance_dir.join("instance.json")).ok()?;
    serde_json::from_reader(BufReader::new(file)).ok()
}

// ---- Spielzeit: eigene Datei, damit Speichern im Misc-Popup sie nie überschreibt ----
//#[derive(Debug, Default, Serialize, Deserialize)]
//#[serde(default)]
// ---- Spielzeit: jede Session wird mit Startzeit gespeichert (für "pro Woche") ----
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct PlaySession { start: u64, secs: u64 } // start = Unix-Sekunden

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)] // alte playtime.json ohne "sessions" bleibt lesbar
struct Playtime { total_secs: u64, sessions: Vec<PlaySession> }

fn load_playtime(instance_dir: &Path) -> Playtime {
    let Ok(file) = File::open(instance_dir.join("playtime.json")) else { return Playtime::default(); };
    serde_json::from_reader(BufReader::new(file)).unwrap_or_default()
}

fn load_playtime_secs(instance_dir: &Path) -> u64 { load_playtime(instance_dir).total_secs }

// Signatur unverändert, run_and_track muss dafür nicht angepasst werden.
fn add_playtime_secs(instance_dir: &Path, add: u64) {
    let mut p = load_playtime(instance_dir);
    p.total_secs += add;
    p.sessions.push(PlaySession { start: unix_now().saturating_sub(add), secs: add });
    if p.sessions.len() > 500 { let d = p.sessions.len() - 500; p.sessions.drain(0..d); } // Datei klein halten
    if let Ok(json) = serde_json::to_string_pretty(&p) {
        let _ = fs::write(instance_dir.join("playtime.json"), json);
    }
}

// ---- Start-Zeit-Messung (startups.json pro Instanz) ----
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StartupRecord {
    ts: u64,
    preset: String,     // JVM-Preset-id aus misc.json
    game_secs: f32,     // ab Java-Start bis Hauptmenü
    total_secs: f32,    // ab Klick auf "Play" (inkl. Download/Installation)
    mods: usize,
    ram_mb: i32,
}

// Diese Zeile im Minecraft-Log markiert "Hauptmenü ist (fast) da". Falls bei dir nichts
// gemessen wird, hier eine andere Log-Zeile eintragen.
const STARTUP_MARKER: &str = "Sound engine started";

fn load_startups(instance_dir: &Path) -> Vec<StartupRecord> {
    let Ok(f) = File::open(instance_dir.join("startups.json")) else { return Vec::new(); };
    serde_json::from_reader(BufReader::new(f)).unwrap_or_default()
}

fn save_startup(instance_dir: &Path, rec: StartupRecord) {
    let mut all = load_startups(instance_dir);
    all.push(rec);
    if all.len() > 30 { let d = all.len() - 30; all.drain(0..d); }
    if let Ok(json) = serde_json::to_string_pretty(&all) {
        let _ = fs::write(instance_dir.join("startups.json"), json);
    }
}

fn format_playtime(secs: u64) -> String {
    if secs == 0 { return "Noch nicht gespielt".to_string(); }
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    if h > 0 { format!("{h} h {m:02} min") } else if m > 0 { format!("{m} min") } else { "< 1 min".to_string() }
}

// ---- Mod-Suchfeld: der Filtertext liegt global, damit ALLE Stellen, die die Mod-Liste neu
// aufbauen (Toggle, Entfernen, Add-Mod, ...), automatisch gefiltert werden, ohne geändert zu werden.
static MOD_FILTER: OnceLock<Mutex<String>> = OnceLock::new();

fn mod_filter() -> &'static Mutex<String> {
    MOD_FILTER.get_or_init(|| Mutex::new(String::new()))
}

// ---- Java-/Minecraft-Hilfen ----
// Grobe Zuordnung Minecraft-Version -> Java-Hauptversion, die lyceris/Mojang-Runtime nutzt.
// Das neue Jahres-Schema (26.x) ist geraten (vermutlich Java 25), genau weiß ich es nicht.

fn java_major_for(mc: &str) -> u32 {
    let parts: Vec<u32> = mc.split('.').filter_map(|p| p.parse().ok()).collect();
    match parts.as_slice() {
        [1, minor, rest @ ..] => {
            let patch = rest.first().copied().unwrap_or(0);
            if *minor <= 16 { 8 }
            else if *minor == 17 { 16 }
            else if *minor < 20 || (*minor == 20 && patch < 5) { 17 }
            else { 21 }
        }
        _ => 25,
    }
}

// Quick Play (--quickPlayMultiplayer / --quickPlaySingleplayer) gibt es ab Minecraft 1.20.

fn supports_quick_play(mc: &str) -> bool {
    let parts: Vec<u32> = mc.split('.').filter_map(|p| p.parse().ok()).collect();
    match parts.as_slice() { [1, minor, ..] => *minor >= 20, _ => true }
}

fn count_mods(instance_dir: &Path) -> usize {
    fs::read_dir(instance_dir.join("mods")).map(|rd| {
        rd.filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().ends_with(".jar")).count()
    }).unwrap_or(0)
}

// ---- JVM-Presets ----
struct JvmPreset { id: &'static str, title: &'static str, description: &'static str }

const JVM_PRESETS: [JvmPreset; 4] = [
    JvmPreset {
        id: "none",
        title: "Standard (keine zusätzlichen Flags)",
        description: "Java nutzt seine eigenen Vorgaben. Für kleine bis mittlere Packs und wenig RAM (unter ca. 4 GB) meist die beste Wahl: Zusätzliche Flags bringen dort selten etwas.",
    },
    JvmPreset {
        id: "mojang_default",
        title: "Mojang-Standard (G1, kurze Pausen)",
        description: "Die Flags, die der offizielle Launcher früher mitgegeben hat (G1 mit kurzer Ziel-Pause und großen Regionen). Sinnvoll bei 4-8 GB RAM, besonders für Minecraft bis 1.16: Java 8 nutzt sonst den Parallel-GC, der spürbare Pausen machen kann. Bei sehr großen Packs kann Aikar's besser passen.",
    },
    JvmPreset {
        id: "aikar_client",
        title: "Aikar's Flags (für den Client angepasst)",
        description: "Bekannte G1-Feineinstellung, ursprünglich für Server, hier ohne AlwaysPreTouch, damit der Start nicht den ganzen RAM auf einmal belegt. Lohnt sich bei großen Modpacks (ab ca. 60-100 Mods) und mindestens 5-6 GB RAM, wenn Ruckler durch Garbage-Collection auftreten. Bei kleinen Packs oder wenig RAM bringt es nichts und kann sogar schaden.",
    },
    JvmPreset {
        id: "zgc",
        title: "ZGC (Low-Latency-Collector)",
        description: "Sehr kurze GC-Pausen. Braucht Java 17 oder neuer (bei Java 21 mit der generationalen Variante) und Platz: ab ca. 8 GB RAM und mehreren CPU-Kernen sinnvoll. Bei knappem RAM kann er mehr Speicher brauchen als G1. Nicht für Minecraft mit Java 8.",
    },
];

fn jvm_preset_flags(id: &str, java_major: u32) -> Vec<String> {
    let flags: Vec<&str> = match id {
        "mojang_default" => vec![
            "-XX:+UnlockExperimentalVMOptions", "-XX:+UseG1GC", "-XX:G1NewSizePercent=20",
            "-XX:G1ReservePercent=20", "-XX:MaxGCPauseMillis=50", "-XX:G1HeapRegionSize=32M",
        ],
        "aikar_client" => vec![
            "-XX:+UseG1GC", "-XX:+ParallelRefProcEnabled", "-XX:MaxGCPauseMillis=200",
            "-XX:+UnlockExperimentalVMOptions", "-XX:+DisableExplicitGC", "-XX:G1NewSizePercent=30",
            "-XX:G1MaxNewSizePercent=40", "-XX:G1HeapRegionSize=8M", "-XX:G1ReservePercent=20",
            "-XX:G1HeapWastePercent=5", "-XX:G1MixedGCCountTarget=4", "-XX:InitiatingHeapOccupancyPercent=15",
            "-XX:G1MixedGCLiveThresholdPercent=90", "-XX:G1RSetUpdatingPauseTimePercent=5",
            "-XX:SurvivorRatio=32", "-XX:+PerfDisableSharedMem", "-XX:MaxTenuringThreshold=1",
        ],
        // ZGC gibt es erst ab Java 17. -XX:+ZGenerational nur bei genau Java 21, in neueren
        // Java-Versionen ist das Flag obsolet bzw. entfernt und könnte den Start verhindern.
        "zgc" if java_major >= 17 => {
            if java_major == 21 || java_major == 22 { vec!["-XX:+UseZGC", "-XX:+ZGenerational"] } else { vec!["-XX:+UseZGC"] }
        }
        _ => vec![],
    };
    flags.into_iter().map(String::from).collect()
}

// Auto-Vorschlag: geht bekannte Fälle von oben nach unten durch; der erste passende gewinnt.
// Liefert (Preset-id, Begründungstext).

// ---- Mod-abhängige JVM-Empfehlungen ----
// Manche Mods (v.a. LOD-/Chunk-Mods) halten riesige Datenmengen im RAM und profitieren
// von einem bestimmten GC. Erkennung über Teile des Jar-Dateinamens (alles klein geschrieben).
struct ModHint {
    needles: &'static [&'static str], // reicht, wenn EIN Teil im Dateinamen steckt
    name: &'static str,               // Anzeigename im Vorschlagstext
    preset: &'static str,             // empfohlenes JVM-Preset
    min_java: u32,                    // kleinste Java-Version, mit der das Preset geht
    min_ram_mb: i32,                  // ab diesem RAM ergibt das Preset Sinn
    why: &'static str,                // Begründung für den Nutzer
}

const MOD_HINTS: [ModHint; 2] = [
    ModHint {
        needles: &["distanthorizons", "distant-horizons", "distant_horizons"],
        name: "Distant Horizons",
        preset: "zgc",
        min_java: 17,
        min_ram_mb: 6144,
        why: "Distant Horizons hält sehr viele LOD-Daten im Speicher und erzeugt viel Müll; ZGC hält die Garbage-Collection-Pausen dabei extrem kurz, das vermeidet Mikroruckler",
    },
    ModHint {
        needles: &["voxy"],
        name: "Voxy",
        preset: "zgc",
        min_java: 17,
        min_ram_mb: 6144,
        why: "Voxy verwaltet große LOD-Datenmengen im RAM; ZGC vermeidet hier lange GC-Pausen",
    },
];

// Alle aktiven Mod-Dateinamen (klein geschrieben) für die Erkennung oben.
fn mod_filenames_lower(instance_dir: &Path) -> Vec<String> {
    let Ok(rd) = fs::read_dir(instance_dir.join("mods")) else { return Vec::new(); };
    rd.filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_lowercase())
        .filter(|n| n.ends_with(".jar"))
        .collect()
}

// Auto-Vorschlag. Reihenfolge: zuerst Mod-Hinweise (Distant Horizons usw.), danach die
// allgemeinen Regeln nach Java-Version / Mod-Anzahl / RAM / CPU-Kernen.
// Liefert (Preset-id, Begründungstext).
fn suggest_jvm_preset(java_major: u32, mods: usize, ram_mb: i32, cores: usize, mod_names: &[String]) -> (&'static str, String) {
    // 0. Mods, die ein bestimmtes Preset empfehlen
    for h in MOD_HINTS.iter() {
        let present = mod_names.iter().any(|n| h.needles.iter().any(|needle| n.contains(needle)));
        if !present { continue; }

        if java_major < h.min_java {
            return ("aikar_client", format!(
                "Vorschlag: Aikar's Flags. {} ist installiert. {}. ZGC braucht aber Java {} oder neuer, diese Instanz nutzt Java {}. Aikar's G1-Feineinstellung ist dafür die beste Alternative.",
                h.name, h.why, h.min_java, java_major));
        }
        if ram_mb < 4096 {
            return ("none", format!(
                "Vorschlag: Standard. {} ist installiert. {}. Mit nur {} MB RAM geht das aber nicht gut: Weise der Instanz zuerst mindestens {} MB zu, dann ist {} sinnvoll.",
                h.name, h.why, ram_mb, h.min_ram_mb, h.preset.to_uppercase()));
        }
        if ram_mb < h.min_ram_mb {
            return ("aikar_client", format!(
                "Vorschlag: Aikar's Flags. {} ist installiert. {}. Dafür wären ca. {} MB RAM ideal, du hast {} MB: Aikar's Flags sind bei diesem Speicher stabiler. Mit mehr RAM wechsle auf ZGC.",
                h.name, h.why, h.min_ram_mb, ram_mb));
        }
        if cores < 4 {
            return ("aikar_client", format!(
                "Vorschlag: Aikar's Flags. {} ist installiert. {}. ZGC arbeitet mit mehreren Threads parallel, bei {} CPU-Kernen ist G1 (Aikar's Flags) die bessere Wahl.",
                h.name, h.why, cores));
        }
        return (h.preset, format!(
            "Vorschlag: {}. {} ist installiert: {}. Deine Instanz hat dafür genug Platz ({} MB RAM, {} CPU-Kerne, Java {}).",
            h.preset.to_uppercase(), h.name, h.why, ram_mb, cores, java_major));
    }

    // 1. Wenig RAM: Flags bringen nichts
    if ram_mb < 3072 {
        return ("none", format!("Vorschlag: Standard. Nur {ram_mb} MB RAM sind zugewiesen, bei so wenig Speicher bringen GC-Flags kaum etwas. Erhöhe lieber zuerst den RAM."));
    }
    // 2. Java 8 (Minecraft bis 1.16)
    if java_major <= 8 {
        if mods >= 100 && ram_mb >= 4096 {
            return ("aikar_client", format!("Vorschlag: Aikar's Flags. Diese Instanz läuft mit Java 8 (Minecraft bis 1.16), hat {mods} Mods und {ram_mb} MB RAM. Java 8 startet sonst mit dem Parallel-GC, was bei großen Packs zu längeren Pausen führen kann. Ob es bei dir spürbar hilft, hängt vom Rechner ab: einfach testen."));
        }
        return ("mojang_default", format!("Vorschlag: Mojang-Standard. Java 8 (Minecraft bis 1.16) nutzt ohne Flags den Parallel-GC. G1 mit kurzer Ziel-Pause ist bei {mods} Mods und {ram_mb} MB RAM meist die ruhigere Wahl. Der Unterschied ist bei kleinen Packs gering."));
    }
    // 3. Kleine Packs
    if mods < 30 && ram_mb <= 4096 {
        return ("none", format!("Vorschlag: Standard. Mit {mods} Mods und {ram_mb} MB RAM ist das Pack klein; zusätzliche Flags bringen hier selten etwas."));
    }
    // 4. Große Packs mit viel RAM
    if java_major >= 17 && ram_mb >= 8192 && mods >= 150 && cores >= 6 {
        return ("zgc", format!("Vorschlag: ZGC. {mods} Mods, {ram_mb} MB RAM und {cores} CPU-Kerne sind genug Platz für den Low-Latency-Collector, der GC-Ruckler am stärksten reduziert. Falls es Probleme gibt, nimm Aikar's Flags."));
    }
    if mods >= 60 && ram_mb >= 5120 {
        return ("aikar_client", format!("Vorschlag: Aikar's Flags. {mods} Mods und {ram_mb} MB RAM sind ein Fall, in dem die G1-Feineinstellung gegen GC-Ruckler helfen kann. Sicher ist der Effekt nicht: einfach vergleichen."));
    }
    ("none", format!("Vorschlag: Standard. Bei {mods} Mods und {ram_mb} MB RAM kenne ich kein Preset mit sicherem Vorteil. Wenn du Ruckler siehst, probiere Aikar's Flags."))
}

// ---- Wrapper-Programme (id, Anzeigename, Programm im PATH, Hinweis) ----

fn wrapper_defs() -> Vec<(&'static str, &'static str, &'static str, &'static str)> {
    if cfg!(target_os = "linux") {
        vec![
            ("gamemode", "GameMode (gamemoderun)", "gamemoderun", "Schaltet den CPU-Governor und weitere Optimierungen, solange das Spiel läuft (Paket: gamemode)."),
            ("mangohud", "MangoHud", "mangohud", "FPS- und Performance-Overlay (Paket: mangohud). Bei Minecraft (OpenGL) hängt die Funktion vom Setup ab."),
            ("prime_run", "prime-run", "prime-run", "Startet auf der NVIDIA-GPU bei Laptops mit Hybrid-Grafik (Paket: nvidia-prime)."),
            ("gamescope", "Gamescope", "gamescope", "Mikro-Compositor (feste Auflösung, FPS-Limit). Kann je nach Setup Fenster- oder Eingabeprobleme machen."),
        ]
    } else if cfg!(target_os = "windows") {
        vec![("high_priority", "Hohe Prozess-Priorität", "", "Startet das Spiel mit höherer Priorität, kann bei Hintergrundlast helfen. Experimentell.")]
    } else {
        vec![]
    }
}

fn find_in_path(program: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else { return false; };
    std::env::split_paths(&paths).any(|dir| dir.join(program).is_file())
}

fn wrapper_warning(enabled: &[String]) -> String {
    let has = |id: &str| enabled.iter().any(|w| w == id);
    let mut w: Vec<&str> = Vec::new();
    if has("mangohud") && has("gamescope") {
        w.push("MangoHud zusammen mit Gamescope kann doppelte Overlays erzeugen; Gamescope hat je nach Version ein eigenes (--mangoapp).");
    }
    if has("prime_run") {
        w.push("prime-run ist nur auf Laptops mit zwei Grafikkarten sinnvoll.");
    }
    w.join(" ")
}

// ---- Was beim Spielstart an lyceris weitergegeben werden soll ----
// JVM-Argumente: System-GLFW (Linux), Preset-Flags, freie Argumente. Reihenfolge = Priorität.

fn collect_jvm_args(misc: &InstanceMisc, java_major: u32) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    #[cfg(target_os = "linux")]
    {
        if misc.use_system_glfw {
            if let Some(glfw) = find_system_glfw() {
                args.push(format!("-Dorg.lwjgl.glfw.libname={glfw}"));
            }
        }
    }
    args.extend(jvm_preset_flags(&misc.jvm_preset, java_major));
    args.extend(misc.jvm_args.split_whitespace().map(String::from));
    args
}

// Spiel-Argumente: Quick Play (nur ab 1.20). Der Server hat Vorrang vor der Welt.

fn collect_game_args(misc: &InstanceMisc, mc: &str) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if misc.quick_enabled && supports_quick_play(mc) {
        if !misc.quick_server.trim().is_empty() {
            args.push("--quickPlayMultiplayer".to_string());
            args.push(misc.quick_server.trim().to_string());
        } else if !misc.quick_world.trim().is_empty() {
            args.push("--quickPlaySingleplayer".to_string());
            args.push(misc.quick_world.trim().to_string());
        }
    }
    args
}

// ---- Daten -> UI ----
// Nur die Listen (Env + Wrapper): wird nach Hinzufügen/Entfernen genutzt, ohne ungespeicherte Textfelder zu überschreiben.

fn push_misc_lists(ui: &AppWindow, instance_name: &str) {
    let misc = load_misc(&launcher_dir().join("instances").join(instance_name));

    let envs: Vec<EnvVarUi> = misc.env_vars.iter()
        .map(|v| EnvVarUi { name: v.name.clone().into(), value: v.value.clone().into() })
        .collect();
    ui.set_misc_env_vars(ModelRc::new(VecModel::from(envs)));

    let wrappers: Vec<WrapperUi> = wrapper_defs().into_iter().map(|(id, label, program, hint)| WrapperUi {
        id: id.into(),
        label: label.into(),
        hint: hint.into(),
        available: program.is_empty() || find_in_path(program), // nicht gefunden -> Checkbox gesperrt
        enabled: misc.wrappers.iter().any(|w| w == id),
    }).collect();
    ui.set_misc_wrappers(ModelRc::new(VecModel::from(wrappers)));
    ui.set_misc_wrapper_warning(wrapper_warning(&misc.wrappers).into());
}

// Alles fürs Misc-Popup (wird beim Öffnen aufgerufen).

fn push_misc_to_ui(ui: &AppWindow, instance_name: &str) {
    let launcher = launcher_dir();
    let instance_dir = launcher.join("instances").join(instance_name);
    let misc = load_misc(&instance_dir);
    let mc = load_instance_config(&instance_dir).map(|c| c.minecraft_version).unwrap_or_default();

    // Effektiver RAM wie beim Start: Instanz-Override (falls aktiv), sonst Settings.
    let overrides = load_instance_overrides(&instance_dir);
    let ram_mb = if overrides.enabled { overrides.ram_mb } else { load_settings(&launcher).default_ram_mb };

    let java_major = java_major_for(&mc);
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    // Dateinamen der Mods mitgeben, damit z.B. Distant Horizons erkannt wird
    let mod_names = mod_filenames_lower(&instance_dir);
    let (suggested_id, suggestion) = suggest_jvm_preset(java_major, count_mods(&instance_dir), ram_mb, cores, &mod_names);

    let presets: Vec<JvmPresetUi> = JVM_PRESETS.iter().map(|p| JvmPresetUi {
        id: p.id.into(),
        title: p.title.into(),
        description: p.description.into(),
        flags: jvm_preset_flags(p.id, java_major).join(" ").into(),
        suggested: p.id == suggested_id,
    }).collect();
    ui.set_misc_presets(ModelRc::new(VecModel::from(presets)));
    ui.set_misc_suggestion(suggestion.into());

    ui.set_misc_jvm_args(misc.jvm_args.clone().into());
    ui.set_misc_preset_id(misc.jvm_preset.clone().into());
    ui.set_misc_use_system_glfw(misc.use_system_glfw);
    ui.set_misc_auto_fix_dupes(misc.auto_fix_duplicates);
    ui.set_misc_is_linux(cfg!(target_os = "linux"));
    #[cfg(target_os = "linux")]
    ui.set_misc_glfw_found(find_system_glfw().is_some());
    ui.set_misc_status("".into());

    push_misc_lists(ui, instance_name);
}

// ===================== Block 3: Manage Instanz =====================

fn is_running(running: &RunningMap, name: &str) -> bool {
    running.lock().unwrap().contains_key(name)
}

// Neuer Instanzname: Sonderzeichen entfernen, Punkte am Rand weg, leer = ungültig.

fn clean_new_name(raw: &str) -> Option<String> {
    let s = sanitize_dir_name(raw.trim());
    let s = s.trim_matches('.').trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn save_instance_config(instance_dir: &Path, cfg: &ModpackJsonData) -> Result<(), String> {
    let json = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    fs::write(instance_dir.join("instance.json"), json).map_err(|e| format!("instance.json schreiben: {e}"))
}

// Kachel für die Instanzliste aus der instance.json. MUSS im UI-Thread laufen (lädt ein slint::Image).

fn tile_from_config(cfg: &ModpackJsonData) -> ModpackInfo {
    let icon_path = cfg.icon_path.clone().map(PathBuf::from);
    ModpackInfo {
        name: cfg.name.clone().into(),
        minecraft_version: cfg.minecraft_version.clone().into(),
        loader: cfg.loader.clone().into(),
        ram_mb: cfg.ram_mb,
        icon: load_icon(&icon_path),
        starred: is_favorite("instances", &cfg.name),
    }
}

// Zugriff auf das VecModel der Instanz-Kacheln (gleiche Downcast-Methode wie beim Install).

fn with_packs_model(ui: &AppWindow, f: impl FnOnce(&VecModel<ModpackInfo>)) {
    let model = ui.get_packs();
    if let Some(vm) = model.as_any().downcast_ref::<VecModel<ModpackInfo>>() {
        f(vm);
    }
}

fn find_tile_row(vm: &VecModel<ModpackInfo>, name: &str) -> Option<usize> {
    (0..vm.row_count()).find(|&i| vm.row_data(i).map(|r| r.name.as_str() == name).unwrap_or(false))
}

// Sortiert die Instanz-Kacheln so, dass Favoriten ganz vorne stehen.
// `!r.starred` ist false für Favoriten, und false < true, also kommen sie zuerst.
// sort_by_key ist stabil: die bisherige Reihenfolge bleibt in beiden Gruppen erhalten.
fn sort_packs_starred_first(ui: &AppWindow) {
    with_packs_model(ui, |vm| {
        let mut rows: Vec<ModpackInfo> = (0..vm.row_count()).filter_map(|i| vm.row_data(i)).collect();
        rows.sort_by_key(|r| !r.starred);
        vm.set_vec(rows); // ersetzt den kompletten Inhalt des VecModel
    });
}

// Setzt den Stern in einem Browser-Model (Modpacks oder Mods) und sortiert Favoriten nach vorne.
// Muss auf dem UI-Thread laufen (wird nur aus upgrade_in_event_loop aufgerufen).
fn set_star_and_sort(model: ModelRc<BrowserPackInfo>, key: &str, starred: bool) {
    if let Some(vm) = model.as_any().downcast_ref::<VecModel<BrowserPackInfo>>() {
        let mut rows: Vec<BrowserPackInfo> = (0..vm.row_count()).filter_map(|i| vm.row_data(i)).collect();
        for r in rows.iter_mut() {
            if r.project_id.as_str() == key { r.starred = starred; }
        }
        rows.sort_by_key(|r| !r.starred);
        vm.set_vec(rows);
    }
}

/// Macht alles unter `dir` für den Besitzer schreibbar (Ordner: rwx, Dateien: +w).
/// Nötig, weil Adoptium-Dateien teils schreibgeschützt sind und lyceris sie beim
/// nächsten install() überschreiben/löschen können muss.
#[cfg(unix)]
fn make_tree_writable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(rd) = fs::read_dir(dir) else { return; };
    // Den Ordner selbst zuerst, sonst kann man den Inhalt nicht ändern
    if let Ok(m) = fs::metadata(dir) {
        let mut p = m.permissions();
        p.set_mode(p.mode() | 0o700);
        let _ = fs::set_permissions(dir, p);
    }
    for e in rd.flatten() {
        let path = e.path();
        let Ok(ft) = e.file_type() else { continue; };
        if ft.is_symlink() { continue; } // Symlinks nicht anfassen
        if ft.is_dir() {
            make_tree_writable(&path);
        } else if let Ok(m) = fs::metadata(&path) {
            let mut p = m.permissions();
            p.set_mode(p.mode() | 0o200); // Owner darf schreiben, Exec-Bit bleibt
            let _ = fs::set_permissions(&path, p);
        }
    }
}

#[cfg(not(unix))]
fn make_tree_writable(_dir: &Path) {}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    // Ziel schreibbar machen (siehe make_tree_writable)
    make_tree_writable(dst);
    Ok(())
}

// Packt die Instanz als .zip (ohne logs/ und crash-reports/). Gibt die unkomprimierte Größe zurück.

fn export_instance_zip(instance_dir: &Path, out_path: &Path) -> Result<u64, String> {
    let file = File::create(out_path).map_err(|e| format!("Zip anlegen: {e}"))?;
    let mut zip = zip::ZipWriter::new(file);
    let base_opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut stack = vec![instance_dir.to_path_buf()];
    let mut total = 0u64;
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            let rel = path.strip_prefix(instance_dir).map_err(|e| e.to_string())?;
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if path.is_dir() {
                if rel_str == "logs" || rel_str == "crash-reports" { continue; }
                stack.push(path);
            } else {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                // Dateien über 4 GiB brauchen das Zip64-Flag
                let opts = if size > 0xFFFF_FFFF { base_opts.large_file(true) } else { base_opts };
                zip.start_file(rel_str, opts).map_err(|e| e.to_string())?;
                let mut f = File::open(&path).map_err(|e| e.to_string())?;
                std::io::copy(&mut f, &mut zip).map_err(|e| e.to_string())?;
                total += size;
            }
        }
    }
    zip.finish().map_err(|e| e.to_string())?;
    Ok(total)
}

// ===================== .mrpack-Export (Feature 3) =====================

// Ordner, die als overrides/ ins .mrpack wandern (keine Welten, keine Logs).
const EXPORT_DIRS: [&str; 6] = ["config", "defaultconfigs", "kubejs", "scripts", "resourcepacks", "shaderpacks"];

async fn export_mrpack(instance_name: &str, out_path: &Path) -> Result<String, String> {
    let dir = launcher_dir().join("instances").join(instance_name);
    let cfg = load_instance_config(&dir).ok_or("instance.json fehlt.")?;
    let mods = identify_mods(&dir).await?;

    // dependencies: Minecraft + Loader (Version aus instance.json, sonst neueste stabile)
    let (kind, id_ver) = split_loader_id(&cfg.loader);
    let mut deps = serde_json::Map::new();
    deps.insert("minecraft".into(), cfg.minecraft_version.clone().into());
    if kind != "none" {
        let ver = match cfg.loader_version.clone().or(id_ver) {
            Some(v) => v,
            None => latest_loader_version(&kind, &cfg.minecraft_version).await
                .ok_or("Loader-Version konnte nicht ermittelt werden.")?,
        };
        let key = match kind.as_str() {
            "fabric" => "fabric-loader", "quilt" => "quilt-loader",
            "neoforge" => "neoforge", "forge" => "forge", _ => "",
        };
        if !key.is_empty() { deps.insert(key.into(), ver.into()); }
    }

    // Mods aufteilen: bei Modrinth bekannt (Download-Link) / unbekannt (direkt ins overrides/mods)
    let mut files: Vec<serde_json::Value> = Vec::new();
    let mut local_jars: Vec<String> = Vec::new();
    for m in &mods {
        let hit = m.lookup.as_ref().and_then(|l| l.files.iter().find(|f| f.hashes.sha1 == m.sha1));
        match hit {
            Some(f) => files.push(serde_json::json!({
                "path": format!("mods/{}", m.filename),
                "hashes": { "sha1": f.hashes.sha1, "sha512": f.hashes.sha512 },
                "downloads": [f.url],
                "fileSize": f.size
            })),
            None => local_jars.push(m.filename.clone()),
        }
    }

    let index = serde_json::json!({
        "formatVersion": 1,
        "game": "minecraft",
        "versionId": "1.0.0",
        "name": cfg.name,
        "summary": "Exportiert mit dem SRUSM Launcher",
        "files": files,
        "dependencies": serde_json::Value::Object(deps),
    });

    let (d2, out2, n_known) = (dir.clone(), out_path.to_path_buf(), files.len());
    let n_local = local_jars.len();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        use std::io::Write;
        let file = File::create(&out2).map_err(|e| format!("Datei anlegen: {e}"))?;
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

        zip.start_file("modrinth.index.json", opts).map_err(|e| e.to_string())?;
        zip.write_all(serde_json::to_string_pretty(&index).map_err(|e| e.to_string())?.as_bytes())
            .map_err(|e| e.to_string())?;

        for sub in EXPORT_DIRS {
            zip_add_dir(&mut zip, &d2, &d2.join(sub), "overrides/", opts)?;
        }
        for jar in &local_jars {
            zip.start_file(format!("overrides/mods/{jar}"), opts).map_err(|e| e.to_string())?;
            let mut f = File::open(d2.join("mods").join(jar)).map_err(|e| e.to_string())?;
            std::io::copy(&mut f, &mut zip).map_err(|e| e.to_string())?;
        }
        zip.finish().map_err(|e| e.to_string())?;
        Ok(())
    }).await.map_err(|e| format!("Interner Fehler: {e}"))??;

    Ok(format!("Exportiert nach {} ({} Mods per Link, {} Mods eingebettet).", out_path.display(), n_known, n_local))
}

// ===================== Server-Pack (Feature 2) =====================

// Fabric-Mods können sich selbst als "nur Client" markieren.
fn fabric_client_only(jar: &Path) -> bool {
    let Ok(f) = File::open(jar) else { return false; };
    let Ok(mut z) = zip::ZipArchive::new(f) else { return false; };
    let Some(text) = read_zip_text(&mut z, "fabric.mod.json") else { return false; };
    serde_json::from_str::<serde_json::Value>(&text).ok()
        .and_then(|j| j.get("environment").and_then(|e| e.as_str()).map(|s| s == "client"))
        .unwrap_or(false)
}

async fn export_server_pack(instance_name: &str, out_path: &Path) -> Result<String, String> {
    let dir = launcher_dir().join("instances").join(instance_name);
    let cfg = load_instance_config(&dir).ok_or("instance.json fehlt.")?;
    let mods = identify_mods(&dir).await?;

    // Modrinth fragen, welche Projekte auf dem Server nicht laufen
    let ids: Vec<String> = mods.iter()
        .filter_map(|m| m.lookup.as_ref().map(|l| l.project_id.clone()))
        .collect::<std::collections::HashSet<_>>().into_iter().collect();
    let projects = fetch_projects_bulk(&ids).await;

    let mut client_only: HashMap<String, String> = HashMap::new(); // Dateiname -> Grund
    for m in &mods {
        if let Some(p) = m.lookup.as_ref().and_then(|l| projects.get(&l.project_id)) {
            if p.server_side == "unsupported" {
                client_only.insert(m.filename.clone(), "Modrinth: server_side = unsupported".into());
            }
        }
    }

    let (d2, out2) = (dir.clone(), out_path.to_path_buf());
    let all_files: Vec<String> = mods.iter().map(|m| m.filename.clone()).collect();
    let (kind, id_ver) = split_loader_id(&cfg.loader);
    let loader_info = format!("{} {}", kind, cfg.loader_version.clone().or(id_ver).unwrap_or_else(|| "(neueste stabile)".into()));
    let mc = cfg.minecraft_version.clone();

    let (kept, removed) = tokio::task::spawn_blocking(move || -> Result<(usize, Vec<(String, String)>), String> {
        use std::io::Write;
        let file = File::create(&out2).map_err(|e| format!("Datei anlegen: {e}"))?;
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

        let mut kept = 0usize;
        let mut removed: Vec<(String, String)> = Vec::new();
        for name in &all_files {
            let path = d2.join("mods").join(name);
            // Grund aus Modrinth oder (Fabric) aus der Jar selbst
            let reason = client_only.get(name).cloned()
                .or_else(|| if fabric_client_only(&path) { Some("fabric.mod.json: environment = client".to_string()) } else { None });
            if let Some(r) = reason { removed.push((name.clone(), r)); continue; }
            zip.start_file(format!("mods/{name}"), opts).map_err(|e| e.to_string())?;
            let mut f = File::open(&path).map_err(|e| e.to_string())?;
            std::io::copy(&mut f, &mut zip).map_err(|e| e.to_string())?;
            kept += 1;
        }

        // Config-Ordner (Welten, Resourcepacks und Shader gehören nicht auf den Server)
        for sub in ["config", "defaultconfigs", "kubejs", "scripts"] {
            zip_add_dir(&mut zip, &d2, &d2.join(sub), "", opts)?;
        }

        let mut info = format!(
            "SRUSM Server-Pack\nMinecraft: {mc}\nLoader: {loader_info}\n\nBitte Server-Software passend zu Minecraft/Loader installieren.\n\nEntfernte Client-only-Mods ({}):\n",
            removed.len()
        );
        for (n, r) in &removed { info.push_str(&format!("- {n}  ({r})\n")); }
        info.push_str("\nHinweis: Mods, die Modrinth nicht kennt, sind enthalten (nicht prüfbar). Bei Startfehlern auf dem Server die Crash-Meldung prüfen.\n");
        zip.start_file("SERVER-INFO.txt", opts).map_err(|e| e.to_string())?;
        zip.write_all(info.as_bytes()).map_err(|e| e.to_string())?;
        zip.finish().map_err(|e| e.to_string())?;
        Ok((kept, removed))
    }).await.map_err(|e| format!("Interner Fehler: {e}"))??;

    Ok(format!("Server-Pack: {} Mods enthalten, {} Client-only entfernt -> {}", kept, removed.len(), out_path.display()))
}

// ===================== Verwaiste Libraries (Feature 4) =====================

// Rückgabe: (verwaiste Dateinamen, Anzahl nicht erkannter Mods)
async fn find_orphans(instance_dir: &Path) -> Result<(Vec<String>, usize), String> {
    let mods = identify_mods(instance_dir).await?;
    let unknown = mods.iter().filter(|m| m.lookup.is_none()).count();

    // Alle Projekte, die irgendein installierter Mod braucht (required) oder optional nutzt
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in &mods {
        let Some(l) = &m.lookup else { continue; };
        for d in &l.dependencies {
            if d.dependency_type == "required" || d.dependency_type == "optional" {
                if let Some(p) = &d.project_id { if *p != l.project_id { used.insert(p.clone()); } }
            }
        }
    }

    let ids: Vec<String> = mods.iter().filter_map(|m| m.lookup.as_ref().map(|l| l.project_id.clone())).collect();
    let projects = fetch_projects_bulk(&ids).await;

    let mut out = Vec::new();
    for m in &mods {
        let Some(l) = &m.lookup else { continue; };
        let Some(p) = projects.get(&l.project_id) else { continue; };
        let is_library = p.categories.iter().chain(p.additional_categories.iter()).any(|c| c == "library");
        if is_library && !used.contains(&l.project_id) { out.push(m.filename.clone()); }
    }
    out.sort();
    Ok((out, unknown))
}

// ---- Verfügbare Loader-Versionen (für die Auswahlliste) ----

async fn list_loader_versions(kind: &str, mc: &str) -> Option<Vec<String>> {
    match kind {
        "fabric" => {
            let list: Vec<LoaderEntry> = fetch("https://meta.fabricmc.net/v2/versions/loader".to_string(), None).await.ok()?;
            Some(list.into_iter().filter(|e| e.stable).map(|e| e.version).take(30).collect())
        }
        "quilt" => {
            let list: Vec<LoaderEntry> = fetch("https://meta.quiltmc.org/v3/versions/loader".to_string(), None).await.ok()?;
            Some(list.into_iter().filter(|e| !e.version.contains('-')).map(|e| e.version).take(30).collect())
        }
        "forge" => {
            // Nur "recommended" und "latest" aus der Promotions-Liste (die volle Liste braucht einen anderen Endpunkt).
            #[derive(Deserialize)]
            struct Promos { promos: HashMap<String, String> }
            let p: Promos = fetch("https://files.minecraftforge.net/net/minecraftforge/forge/promotions_slim.json".to_string(), None).await.ok()?;
            let mut v: Vec<String> = Vec::new();
            for key in [format!("{mc}-recommended"), format!("{mc}-latest")] {
                if let Some(x) = p.promos.get(&key) { if !v.contains(x) { v.push(x.clone()); } }
            }
            Some(v)
        }
        "neoforge" => {
            #[derive(Deserialize)]
            struct Versions { versions: Vec<String> }
            let v: Versions = fetch("https://maven.neoforged.net/api/maven/versions/releases/net/neoforged/neoforge".to_string(), None).await.ok()?;
            let prefix = neoforge_prefix(mc)?;
            Some(v.versions.into_iter().filter(|x| x.starts_with(&prefix) && !x.contains('-')).rev().take(40).collect())
        }
        _ => None,
    }
}

// ---- Loader-Check: liest die Loader-Anforderungen aus den Mod-Jars und testet die Zielversion ----
use std::cmp::Ordering;

// "0.16.9" -> [0,16,9]. Alles ab '-' oder '+' (Pre-Release) wird abgeschnitten.

fn parse_ver(s: &str) -> Option<Vec<u64>> {
    let s = s.trim().trim_start_matches('v');
    let core = s.split(|c: char| c == '-' || c == '+').next()?;
    if core.is_empty() { return None; }
    core.split('.').map(|p| p.parse::<u64>().ok()).collect()
}

fn cmp_ver(a: &[u64], b: &[u64]) -> Ordering {
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y { return x.cmp(&y); }
    }
    Ordering::Equal
}

// Ein Fabric-Vergleich wie ">=0.16.2", "<0.17-", "~0.16", "^1.2", "0.16.x". None = nicht lesbar.

fn fabric_cmp_matches(token: &str, v: &[u64]) -> Option<bool> {
    let t = token.trim();
    if t.is_empty() || t == "*" { return Some(true); }
    let (op, rest) = ["<=", ">=", "<", ">", "=", "~", "^"].iter()
        .find_map(|o| t.strip_prefix(*o).map(|r| (*o, r.trim())))
        .unwrap_or(("=", t));
    if rest.chars().any(|c| matches!(c, 'x' | 'X' | '*')) {
        if op != "=" { return None; }
        let prefix: Vec<u64> = rest.split('.').take_while(|p| !matches!(*p, "x" | "X" | "*")).filter_map(|p| p.parse().ok()).collect();
        return Some(prefix.iter().enumerate().all(|(i, p)| v.get(i).copied().unwrap_or(0) == *p));
    }
    let r = parse_ver(rest)?;
    Some(match op {
        ">=" => cmp_ver(v, &r) != Ordering::Less,
        "<=" => cmp_ver(v, &r) != Ordering::Greater,
        ">" => cmp_ver(v, &r) == Ordering::Greater,
        "<" => cmp_ver(v, &r) == Ordering::Less,
        "=" => cmp_ver(v, &r) == Ordering::Equal,
        "~" => {
            let mut up = r.clone();
            if up.len() >= 2 { up.truncate(2); up[1] += 1; } else { up[0] += 1; }
            cmp_ver(v, &r) != Ordering::Less && cmp_ver(v, &up) == Ordering::Less
        }
        "^" => cmp_ver(v, &r) != Ordering::Less && cmp_ver(v, &[r[0] + 1]) == Ordering::Less,
        _ => return None,
    })
}

// "A B || C": Gruppen mit "||" = ODER, Leerzeichen = UND.

fn fabric_pred_matches(pred: &str, v: &[u64]) -> Option<bool> {
    let mut unknown_seen = false;
    for group in pred.split("||") {
        let (mut ok, mut unknown) = (true, false);
        for tok in group.split_whitespace() {
            match fabric_cmp_matches(tok, v) { Some(true) => {}, Some(false) => ok = false, None => unknown = true }
        }
        if unknown { unknown_seen = true; continue; }
        if ok { return Some(true); }
    }
    if unknown_seen { None } else { Some(false) }
}

// Maven-Bereich der Forge-Welt: "[47,)", "[47.1.0,48)", "[47]", "47" (= Mindestversion).

fn maven_range_matches(range: &str, v: &[u64]) -> Option<bool> {
    let r = range.trim();
    if r.is_empty() { return Some(true); }
    let first = r.chars().next()?;
    if first != '[' && first != '(' {
        return Some(cmp_ver(v, &parse_ver(r)?) != Ordering::Less);
    }
    if r.len() < 2 || !(r.ends_with(']') || r.ends_with(')')) { return None; }
    let (lo_incl, hi_incl) = (first == '[', r.ends_with(']'));
    let inner = &r[1..r.len() - 1];
    if inner.contains(|c| matches!(c, '[' | ']' | '(' | ')')) { return None; } // mehrere Bereiche: nicht unterstützt
    match inner.split_once(',') {
        None => Some(cmp_ver(v, &parse_ver(inner)?) == Ordering::Equal),
        Some((lo, hi)) => {
            let mut ok = true;
            if !lo.trim().is_empty() {
                let c = cmp_ver(v, &parse_ver(lo)?);
                ok &= if lo_incl { c != Ordering::Less } else { c == Ordering::Greater };
            }
            if !hi.trim().is_empty() {
                let c = cmp_ver(v, &parse_ver(hi)?);
                ok &= if hi_incl { c != Ordering::Greater } else { c == Ordering::Less };
            }
            Some(ok)
        }
    }
}

fn toml_kv(line: &str) -> Option<(String, String)> {
    let (k, v) = line.split_once('=')?;
    let v = v.trim();
    let val = if let Some(rest) = v.strip_prefix('"') {
        rest.split('"').next()?.to_string()
    } else {
        v.split(|c: char| c.is_whitespace() || c == '#').next()?.to_string()
    };
    Some((k.trim().to_string(), val))
}

// Grobes Lesen der mods.toml: sammelt die Version-Bereiche der Pflicht-Abhängigkeit auf den Loader
// ([[dependencies.xyz]] mit modId = "forge"/"neoforge"). Kein vollständiger TOML-Parser.

fn forge_loader_ranges(text: &str, loader_modid: &str) -> Vec<String> {
    fn flush(in_dep: bool, id: &str, range: &str, required: bool, want: &str, out: &mut Vec<String>) {
        if in_dep && id == want && required && !range.is_empty() { out.push(range.to_string()); }
    }
    let mut out = Vec::new();
    let (mut in_dep, mut id, mut range, mut required) = (false, String::new(), String::new(), true);
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with("[[dependencies.") {
            flush(in_dep, &id, &range, required, loader_modid, &mut out);
            in_dep = true; id.clear(); range.clear(); required = true;
            continue;
        }
        if l.starts_with('[') {
            flush(in_dep, &id, &range, required, loader_modid, &mut out);
            in_dep = false;
            continue;
        }
        if !in_dep { continue; }
        if let Some((k, val)) = toml_kv(l) {
            match k.as_str() {
                "modId" => id = val,
                "versionRange" => range = val,
                "mandatory" => required = val != "false",
                "type" => required = val == "required",
                _ => {}
            }
        }
    }
    flush(in_dep, &id, &range, required, loader_modid, &mut out);
    out
}

fn read_zip_text(zip: &mut zip::ZipArchive<File>, name: &str) -> Option<String> {
    let mut f = zip.by_name(name).ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    Some(s)
}

struct LoaderCheck {
    supported: bool,        // false = für diesen Loader kann nicht geprüft werden
    checked: usize,         // Mods mit lesbarer Loader-Anforderung
    unchecked: usize,       // Mods, deren Anforderung ich nicht auswerten konnte
    conflicts: Vec<String>, // Mods, die die Zielversion nicht erlauben
}


fn check_loader_change(instance_dir: &Path, kind: &str, target: &str) -> LoaderCheck {
    let mut res = LoaderCheck { supported: matches!(kind, "fabric" | "forge" | "neoforge"), checked: 0, unchecked: 0, conflicts: Vec::new() };
    let Some(tv) = parse_ver(target) else { res.supported = false; return res; };
    if !res.supported { return res; }

    let Ok(entries) = fs::read_dir(instance_dir.join("mods")) else { return res; };
    for entry in entries.filter_map(|e| e.ok()) {
        let fname = entry.file_name().to_string_lossy().to_string();
        if !fname.ends_with(".jar") { continue; } // deaktivierte Mods (.disabled) zählen nicht
        let Ok(file) = File::open(entry.path()) else { continue; };
        let Ok(mut zip) = zip::ZipArchive::new(file) else { continue; };

        // Anforderungen (OR-Liste) und Anzeigename je nach Loader lesen
        let (display, preds): (String, Vec<String>) = if kind == "fabric" {
            let Some(text) = read_zip_text(&mut zip, "fabric.mod.json") else { continue; };
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else { continue; };
            let name = json.get("name").and_then(|v| v.as_str()).unwrap_or(&fname).to_string();
            let preds = match json.get("depends").and_then(|d| d.get("fabricloader")) {
                Some(serde_json::Value::String(s)) => vec![s.clone()],
                Some(serde_json::Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
                _ => continue, // keine Anforderung an den Loader
            };
            (name, preds)
        } else {
            let (modid, toml_names): (&str, &[&str]) = if kind == "neoforge" {
                ("neoforge", &["META-INF/neoforge.mods.toml", "META-INF/mods.toml"])
            } else {
                ("forge", &["META-INF/mods.toml"])
            };
            let Some(text) = toml_names.iter().find_map(|n| read_zip_text(&mut zip, n)) else { continue; };
            let ranges = forge_loader_ranges(&text, modid);
            if ranges.is_empty() { continue; }
            (fname.clone(), ranges)
        };

        // Alternativen (ODER): passt eine, ist der Mod in Ordnung.
        let mut any_true = false;
        let mut any_unknown = false;
        for p in &preds {
            let r = if kind == "fabric" { fabric_pred_matches(p, &tv) } else { maven_range_matches(p, &tv) };
            match r { Some(true) => any_true = true, Some(false) => {}, None => any_unknown = true }
        }
        if any_true { res.checked += 1; }
        else if any_unknown { res.unchecked += 1; }
        else {
            res.checked += 1;
            res.conflicts.push(format!("{} ({}): verlangt {} {}, Ziel ist {}", display, fname, kind, preds.join(" oder "), target));
        }
    }
    res
}

// Aus dem Check den Anzeigetext bauen. Rückgabe: (Wechsel erlaubt?, Text).
fn format_loader_report(check: &LoaderCheck, kind: &str, from: &str, to: &str) -> (bool, String) {
    if !check.supported {
        return (true, format!("Hinweis: Für {kind} kann ich die Mod-Anforderungen nicht prüfen. Der Wechsel {from} -> {to} ist ungeprüft; bestätige nur, wenn du sicher bist."));
    }
    if check.conflicts.is_empty() {
        let extra = if check.unchecked > 0 { format!(" {} Mods konnte ich nicht prüfen (Anforderung nicht lesbar).", check.unchecked) } else { String::new() };
        return (true, format!("OK: {kind} {from} -> {to}. {} Mods geprüft, kein Konflikt gefunden.{extra} Es sind keine Mod-Änderungen nötig.", check.checked));
    }
    let mut lines: Vec<String> = check.conflicts.iter().take(8).map(|c| format!("- {c}")).collect();
    if check.conflicts.len() > 8 { lines.push(format!("... und {} weitere", check.conflicts.len() - 8)); }
    (false, format!("Nicht möglich: {kind} {to} passt nicht zu {} Mods. Es wurde nichts geändert.\n{}", check.conflicts.len(), lines.join("\n")))
}

// Startet den Check im Hintergrund (Zip-Lesen blockiert) und schreibt das Ergebnis in die UI.
// target = None: Zielversion ist die neueste stabile.
fn start_loader_check(wh: Weak<AppWindow>, hdl: tokio::runtime::Handle, name: String, target: Option<String>) {
    hdl.spawn(async move {
        let post = |wh: &Weak<AppWindow>, text: String, pending: String| {
            let _ = wh.upgrade_in_event_loop(move |ui| {
                ui.set_manage_loader_report(text.into());
                ui.set_manage_loader_pending(pending.into());
            });
        };
        let dir = launcher_dir().join("instances").join(&name);
        let Some(cfg) = load_instance_config(&dir) else { post(&wh, "instance.json fehlt.".into(), String::new()); return; };
        let (kind, id_ver) = split_loader_id(&cfg.loader);
        if kind == "none" { post(&wh, "Vanilla-Instanz: kein Loader.".into(), String::new()); return; }
        let from = cfg.loader_version.clone().or(id_ver).unwrap_or_else(|| "neueste stabile".to_string());

        let to = match target {
            Some(t) if !t.trim().is_empty() => t.trim().to_string(),
            Some(_) => { post(&wh, "Bitte zuerst eine Version wählen.".into(), String::new()); return; }
            None => match latest_loader_version(&kind, &cfg.minecraft_version).await {
                Some(v) => v,
                None => { post(&wh, "Neueste Version konnte nicht ermittelt werden.".into(), String::new()); return; }
            },
        };
        if to == from { post(&wh, format!("Die Instanz nutzt schon {kind} {to}."), String::new()); return; }

        let (d2, k2, t2) = (dir.clone(), kind.clone(), to.clone());
        let Ok(check) = tokio::task::spawn_blocking(move || check_loader_change(&d2, &k2, &t2)).await else {
            post(&wh, "Interner Fehler beim Prüfen.".into(), String::new());
            return;
        };
        let (ok, text) = format_loader_report(&check, &kind, &from, &to);
        post(&wh, text, if ok { to } else { String::new() });
    });
}

// ===================== Block 4: Doppelte Mods + mods-list.json =====================

// Vorschläge zum Beheben: pro Projekt mit mehreren aktiven Dateien wird die NEUESTE Datei behalten (Änderungsdatum),
// die anderen werden zum Deaktivieren vorgeschlagen. Deaktiviert (.disabled) statt gelöscht, damit es umkehrbar bleibt.
// Rückgabe: (Dateiname, Anzeigetext).
fn duplicate_proposals(instance_dir: &Path) -> Vec<(String, String)> {
    let mods_dir = instance_dir.join("mods");
    let pid_by_file: HashMap<String, String> = load_mods_list(instance_dir)
        .into_iter().map(|e| (e.filename, e.project_id)).collect();

    let mut groups: HashMap<String, Vec<(String, std::time::SystemTime)>> = HashMap::new();
    for e in list_mod_files_all(instance_dir).into_iter().filter(|e| e.enabled) {
        if let Some(pid) = pid_by_file.get(&e.display_name) {
            let mtime = fs::metadata(mods_dir.join(&e.filename)).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
            groups.entry(pid.clone()).or_default().push((e.filename, mtime));
        }
    }

    let mut out = Vec::new();
    for (_pid, mut files) in groups {
        if files.len() < 2 { continue; }
        files.sort_by(|a, b| b.1.cmp(&a.1)); // neueste zuerst
        let keep = files[0].0.clone();
        for (f, _) in files.into_iter().skip(1) {
            out.push((f.clone(), format!("Deaktivieren: {f}  (behalte {keep}, die neuere Datei)")));
        }
    }
    out.sort();
    out
}

// mods-list.json pflegen, OHNE die Einträge aus dem Modpack-Install zu verlieren (die alte Variante
// hat die Datei aus mod_meta.json neu geschrieben und dabei alle Pack-Mods gelöscht).
fn mods_list_remove(instance_dir: &Path, filename: &str) {
    let mut list = load_mods_list(instance_dir);
    list.retain(|e| e.filename != filename);
    save_mods_list(instance_dir, &list);
}

fn mods_list_upsert(instance_dir: &Path, project_id: &str, old_filename: Option<&str>, new_filename: &str) {
    let mut list = load_mods_list(instance_dir);
    if let Some(old) = old_filename { list.retain(|e| e.filename != old); }
    list.retain(|e| e.filename != new_filename);
    list.push(ModListEntry { filename: new_filename.to_string(), project_id: project_id.to_string() });
    save_mods_list(instance_dir, &list);
}

// ===================== Achievements =====================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct AchievementEntry {
    instance: String,   // Instanz, in der es zuletzt erfasst wurde (mit Version im Namen)
    #[serde(default)]
    pack: String,       // Modpack-Schlüssel: gleich über alle Versionen eines Modpacks
    title: String,
    description: String,
    kind: String,       // "advancement" | "goal" | "challenge" | "achievement"
    context: String,    // "Welt: X" | "Server: host" | ""
    mc_version: String,
    session: u64,
    seq: u32,
}

fn achievements_path(launcher: &Path) -> PathBuf { launcher.join("achievements.json") }

// "Create and More (1.2.0)" -> "Create and More". Ohne Klammer am Ende bleibt der Name wie er ist.
fn strip_version_suffix(name: &str) -> String {
    let n = name.trim();
    if n.ends_with(')') {
        if let Some(i) = n.rfind(" (") {
            let base = n[..i].trim();
            if !base.is_empty() { return base.to_string(); }
        }
    }
    n.to_string()
}

// Modpack-Schlüssel: Herkunft (Modrinth/CurseForge-Projekt), sonst Name ohne Versions-Klammer.
fn pack_key_from(cfg: Option<&ModpackJsonData>, instance_name: &str) -> String {
    if let Some(c) = cfg {
        if let (Some(src), Some(id)) = (c.origin_source.as_ref(), c.origin_project_id.as_ref()) {
            return format!("{src}:{id}");
        }
    }
    strip_version_suffix(instance_name).to_lowercase()
}

fn pack_key(instance_dir: &Path, instance_name: &str) -> String {
    pack_key_from(load_instance_config(instance_dir).as_ref(), instance_name)
}

fn load_achievements(launcher: &Path) -> Vec<AchievementEntry> {
    let Ok(f) = File::open(achievements_path(launcher)) else { return Vec::new(); };
    let mut list: Vec<AchievementEntry> = serde_json::from_reader(BufReader::new(f)).unwrap_or_default();

    // Alte Einträge ohne Modpack-Schlüssel nachtragen
    for a in list.iter_mut() {
        if a.pack.is_empty() {
            a.pack = pack_key(&launcher.join("instances").join(&a.instance), &a.instance);
        }
    }
    list
}

// Entfernt Achievements, deren Welt oder Server in KEINER Instanz dieses Modpacks mehr existiert.
// Einträge ohne Kontext oder mit "Einzelspieler" bleiben. Rückgabe: true, wenn etwas entfernt wurde.
fn prune_achievements(launcher: &Path, all: &mut Vec<AchievementEntry>) -> bool {
    // Modpack-Schlüssel -> alle Welten (Anzeigename + Ordnername) und Server-Hosts aller seiner Instanzen
    let mut worlds: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
    let mut servers: HashMap<String, Vec<String>> = HashMap::new();

    if let Ok(rd) = fs::read_dir(launcher.join("instances")) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() { continue; }
            let name = e.file_name().to_string_lossy().to_string();
            let key = pack_key(&p, &name);

            let w = worlds.entry(key.clone()).or_default();
            for world in read_worlds(&p) {
                w.insert(world.display_name.to_lowercase());
                w.insert(world.folder_name.to_lowercase());
            }
            let s = servers.entry(key).or_default();
            for srv in read_servers(&p) {
                // "mc.example.com:25565" -> "mc.example.com"
                s.push(srv.ip.split(':').next().unwrap_or("").trim().to_lowercase());
            }
        }
    }

    let before = all.len();
    all.retain(|a| {
        if let Some(w) = a.context.strip_prefix("Welt: ") {
            return worlds.get(&a.pack).map(|set| set.contains(&w.trim().to_lowercase())).unwrap_or(false);
        }
        if let Some(h) = a.context.strip_prefix("Server: ") {
            let h = h.trim().to_lowercase();
            return servers.get(&a.pack)
                .map(|list| list.iter().any(|s| !s.is_empty() && (s == &h || h.contains(s.as_str()) || s.contains(h.as_str()))))
                .unwrap_or(false);
        }
        true // kein Kontext / Einzelspieler: behalten
    });
    all.len() != before
}

fn save_achievements(launcher: &Path, list: &[AchievementEntry]) {
    if let Ok(json) = serde_json::to_string_pretty(list) {
        let _ = fs::write(achievements_path(launcher), json);
    }
}

// Sucht Beschreibungen zu Achievement-Titeln in den Sprachdateien (en_us.json) der
// Minecraft-Jar und aller Mod-Jars. Schlüssel: Titel in Kleinbuchstaben.
// Neu (1.12+):  "advancements.x.y.title"  + "advancements.x.y.description"
// Alt (<=1.11): "achievement.x"          + "achievement.x.desc"  (nur in manchen Jars vorhanden)
fn achievement_descriptions(launcher: &Path, instance_dir: &Path, mc: &str) -> HashMap<String, String> {
    let mut jars: Vec<PathBuf> = vec![
        launcher.join("shared").join("versions").join(mc).join(format!("{mc}.jar")),
    ];
    if let Ok(rd) = fs::read_dir(instance_dir.join("mods")) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("jar") { jars.push(p); }
        }
    }

    let mut map: HashMap<String, String> = HashMap::new();
    for jar in jars {
        let Ok(f) = File::open(&jar) else { continue; };
        let Ok(mut z) = zip::ZipArchive::new(f) else { continue; };
        let names: Vec<String> = z.file_names()
            .filter(|n| n.starts_with("assets/") && n.ends_with("/lang/en_us.json"))
            .map(String::from)
            .collect();
        for n in names {
            let Some(text) = read_zip_text(&mut z, &n) else { continue; };
            let Ok(serde_json::Value::Object(obj)) = serde_json::from_str::<serde_json::Value>(&text) else { continue; };
            for (k, v) in obj.iter() {
                let Some(title) = v.as_str() else { continue; };
                let desc_key = if k.ends_with(".title") && k.contains("advancement") {
                    format!("{}.description", &k[..k.len() - 6])
                } else if k.starts_with("achievement.") && !k.ends_with(".desc") {
                    format!("{k}.desc")
                } else {
                    continue;
                };
                if let Some(d) = obj.get(&desc_key).and_then(|x| x.as_str()) {
                    map.entry(title.to_lowercase()).or_insert_with(|| d.to_string());
                }
            }
        }
    }
    map
}

// Liest logs/latest.log der gerade beendeten Session und speichert neue Achievements des Spielers.
// Blockierend (Dateien lesen) -> aus async per spawn_blocking aufrufen.
// Rückgabe: Anzahl neu gespeicherter Achievements.
fn record_session_achievements(instance_dir: &Path, instance_name: &str, player: &str, mc: &str) -> usize {
    let Ok(bytes) = fs::read(instance_dir.join("logs").join("latest.log")) else { return 0; };
    let text = String::from_utf8_lossy(&bytes);

    const MARKERS: [(&str, &str); 4] = [
        (" has made the advancement [", "advancement"),
        (" has completed the challenge [", "challenge"),
        (" has reached the goal [", "goal"),
        (" has just earned the achievement [", "achievement"), // Minecraft <= 1.11
    ];

    let mut ctx = String::new();
    let mut found: Vec<(String, String, String)> = Vec::new(); // (Titel, Art, Kontext)
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();

    for line in text.lines() {
        // ---- Kontext mitführen: Welt oder Server ----
        if let Some(i) = line.find("Preparing level \"") {
            let rest = &line[i + 17..];
            if let Some(n) = rest.split('"').next() { ctx = format!("Welt: {n}"); }
        } else if line.contains("Starting integrated") {
            ctx = "Welt: ?".to_string(); // wird unten durch die zuletzt gespielte Welt ersetzt, falls kein Name im Log steht
        } else if let Some(i) = line.find("Connecting to ") {
            // Format: "Connecting to mc.example.com, 25565"
            let rest = line[i + 14..].trim();
            let mut parts = rest.split(", ");
            if let (Some(host), Some(port)) = (parts.next(), parts.next()) {
                if port.trim().parse::<u16>().is_ok() { ctx = format!("Server: {host}"); }
            }
        }

        // ---- Achievement-Zeile? ----
        for (marker, kind) in MARKERS.iter() {
            let Some(i) = line.find(marker) else { continue; };
            let who = line[..i].rsplit(' ').next().unwrap_or("");
            if !who.eq_ignore_ascii_case(player) { break; } // Achievement eines anderen Spielers
            let rest = &line[i + marker.len()..];
            let Some(end) = rest.rfind(']') else { break; };
            let title = rest[..end].trim().to_string();
            if title.is_empty() { break; }
            // Einzelspieler loggt die Meldung oft doppelt (Server-Thread + Chat): einmal zählen
            if seen.insert((title.clone(), ctx.clone())) {
                found.push((title, kind.to_string(), ctx.clone()));
            }
            break;
        }
    }
    if found.is_empty() { return 0; }

        // Fallback für "Welt: ?": die zuletzt gespielte Welt der Instanz
    let fallback_world = read_worlds(instance_dir).first()
        .map(|w| format!("Welt: {}", w.display_name))
        .unwrap_or_else(|| "Einzelspieler".to_string());

    let ldir = launcher_dir();
    let descs = achievement_descriptions(&ldir, instance_dir, mc);
    let pack = pack_key(instance_dir, instance_name); // gleich für alle Versionen dieses Modpacks
    let mut all = load_achievements(&ldir);
    let session = unix_now();
    let mut added = 0usize;

    for (seq, (title, kind, c)) in found.into_iter().enumerate() {
        let context = if c == "Welt: ?" { fallback_world.clone() } else { c };

        // Alter Eintrag ohne Kontext: nur die Welt nachtragen
        if let Some(old) = all.iter_mut().find(|a| a.pack == pack && a.title == title && a.context.is_empty()) {
            old.context = context;
            continue;
        }
        // Gleiches Achievement in gleicher Welt/Server desselben Modpacks (egal welche Version): überspringen
        if all.iter().any(|a| a.pack == pack && a.title == title && a.context == context) { continue; }

        all.push(AchievementEntry {
            instance: instance_name.to_string(),
            pack: pack.clone(),
            description: descs.get(&title.to_lowercase()).cloned().unwrap_or_default(),
            title,
            kind,
            context,
            mc_version: mc.to_string(),
            session,
            seq: seq as u32,
        });
        added += 1;
    }

    if all.len() > 5000 {
        all.sort_by(|a, b| (a.session, a.seq).cmp(&(b.session, b.seq)));
        let d = all.len() - 5000;
        all.drain(0..d);
    }
    save_achievements(&ldir, &all);
    added
}

// Welche Instanz-Gruppen im Achievements-Tab eingeklappt sind (gilt, solange der Launcher läuft)
static ACH_COLLAPSED: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
fn ach_collapsed() -> &'static Mutex<std::collections::HashSet<String>> {
    ACH_COLLAPSED.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

// Baut die beiden UI-Listen (Neueste 5 + Gruppen pro Modpack). Nur auf dem UI-Thread aufrufen.
fn push_achievements_to_ui(ui: &AppWindow) {
    let ldir = launcher_dir();
    let mut all = load_achievements(&ldir);

    // Einträge zu gelöschten Welten/Servern entfernen (und die Datei gleich bereinigen)
    if prune_achievements(&ldir, &mut all) {
        save_achievements(&ldir, &all);
    }
    all.sort_by(|a, b| (b.session, b.seq).cmp(&(a.session, a.seq))); // neueste zuerst

    // Letzte Session pro Modpack -> deren Einträge werden grün/fett
    let mut latest: HashMap<String, u64> = HashMap::new();
    for a in &all {
        let e = latest.entry(a.pack.clone()).or_insert(0);
        if a.session > *e { *e = a.session; }
    }

    // Icon pro Modpack aus einer noch existierenden Instanz (die alte Version kann gelöscht sein)
    let mut pack_icons: HashMap<String, Option<PathBuf>> = HashMap::new();
    if let Ok(rd) = fs::read_dir(ldir.join("instances")) {
        for e in rd.flatten() {
            let Some(cfg) = load_instance_config(&e.path()) else { continue; };
            let key = pack_key_from(Some(&cfg), &e.file_name().to_string_lossy());
            pack_icons.insert(key, cfg.icon_path.clone().map(PathBuf::from));
        }
    }

    let collapsed_set = ach_collapsed().lock().unwrap().clone();

    let mut images: HashMap<String, Image> = HashMap::new();
    let mut to_ui = |a: &AchievementEntry, header: String, collapsed: bool| -> AchievementUi {
        let icon = images.entry(a.pack.clone()).or_insert_with(|| {
            load_icon(&pack_icons.get(&a.pack).cloned().flatten())
        }).clone();

        let when = format_relative_time((a.session * 1000) as i64);

        // Karte: Modpack · Welt/Server · MC-Version · Zeit
        let mut meta: Vec<String> = vec![strip_version_suffix(&a.instance)];
        if !a.context.is_empty() { meta.push(a.context.clone()); }
        if !a.mc_version.is_empty() { meta.push(format!("MC {}", a.mc_version)); }
        meta.push(when.clone());

        // Listenzeile: "Welt/Server - Achievement"
        let line = if a.context.is_empty() { a.title.clone() } else { format!("{} - {}", a.context, a.title) };

        AchievementUi {
            title: a.title.clone().into(),
            title_big: a.title.to_uppercase().into(),
            description: a.description.clone().into(),
            kind: a.kind.clone().into(),
            meta: meta.join(" · ").into(),
            short: when.into(),
            line: line.into(),
            header: header.into(),
            instance: a.pack.clone().into(), // Schlüssel fürs Ein-/Ausklappen
            collapsed,
            icon,
            is_new: latest.get(&a.pack).copied() == Some(a.session),
        }
    };

    // Neueste 5 über alle Modpacks
    let recent: Vec<AchievementUi> = all.iter().take(5).map(|a| to_ui(a, String::new(), false)).collect();

    // Gruppen: (Schlüssel, Anzeigename), alphabetisch nach Name
    let mut groups: Vec<(String, String)> = Vec::new();
    for a in &all { // all ist neueste-zuerst, der Anzeigename kommt also vom neuesten Eintrag
        if !groups.iter().any(|(p, _)| p == &a.pack) {
            groups.push((a.pack.clone(), strip_version_suffix(&a.instance)));
        }
    }
    groups.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));

    let mut grouped: Vec<AchievementUi> = Vec::new();
    for (pack, display) in &groups {
        let items: Vec<&AchievementEntry> = all.iter().filter(|a| &a.pack == pack).collect();
        let is_collapsed = collapsed_set.contains(pack);
        let arrow = if is_collapsed { "▶" } else { "▼" };
        let header = format!("{} {} ({})", arrow, display, items.len());

        if is_collapsed {
            grouped.push(to_ui(items[0], header, true));
        } else {
            for (i, a) in items.iter().enumerate() {
                let h = if i == 0 { header.clone() } else { String::new() };
                grouped.push(to_ui(a, h, false));
            }
        }
    }

    ui.set_achievements_summary(format!("{} Achievements in {} Modpacks", all.len(), groups.len()).into());
    ui.set_achievements_recent(ModelRc::new(VecModel::from(recent)));
    ui.set_achievements_all(ModelRc::new(VecModel::from(grouped)));
}

// ===================== Favoriten (Feature 6) =====================
// Drei Listen mit unterschiedlichen Keys, weil Instanzen keine project_id
// haben (nur einen Namen) und Browser-Einträge keine Instanz sind.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Favorites {
    instances: Vec<String>,   // Instanznamen
    modpacks: Vec<String>,    // Modrinth/CF project_ids (Modpacks)
    mods: Vec<String>,        // Modrinth/CF project_ids (Mods)
}

/// Erstellt eine Desktop-Verknüpfung, die den Launcher mit `--launch <instance>`
/// startet. Linux: ~/.local/share/applications/<name>.desktop
/// Windows: eigene .lnk ist aufwändiger — wir machen erstmal nur Linux.
fn create_desktop_shortcut(instance_name: &str) -> Result<PathBuf, String> {
    #[cfg(target_os = "linux")]
    {
        let exe = std::env::current_exe()
            .map_err(|e| format!("Konnte eigenen Pfad nicht ermitteln: {e}"))?;

        let home = dirs::home_dir().ok_or("Kein Home-Verzeichnis")?;
        let apps_dir = home.join(".local/share/applications");
        fs::create_dir_all(&apps_dir).map_err(|e| format!("Ordner anlegen: {e}"))?;

        let filename = format!("srusm-{}.desktop", sanitize_dir_name(instance_name));
        let path = apps_dir.join(&filename);

        // Achtung: Exec-Zeile darf keine unescapten Anführungszeichen enthalten.
        // Wir escapen einfache " für den Instanznamen.
        let safe_name = instance_name.replace('"', "\\\"");
        let content = format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name={name}\n\
             Comment=Minecraft-Instanz '{name}' über SRUSM starten\n\
             Exec={exe} --launch \"{name}\"\n\
             Terminal=false\n\
             Categories=Game;\n",
            name = safe_name,
            exe = exe.display(),
        );

        fs::write(&path, content).map_err(|e| format!("Schreiben: {e}"))?;
        Ok(path)
    }

    #[cfg(target_os = "windows")]
    {
        // .lnk via PowerShell — noch nicht implementiert, kommt später.
        Err("Windows-Verknüpfungen sind noch nicht implementiert.".to_string())
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = instance_name;
        Err("Auf diesem System nicht unterstützt.".to_string())
    }
}

fn favorites_path(launcher_dir: &Path) -> PathBuf {
    launcher_dir.join("favorites.json")
}

fn load_favorites(launcher_dir: &Path) -> Favorites {
    let Ok(file) = File::open(favorites_path(launcher_dir)) else { return Favorites::default(); };
    serde_json::from_reader(BufReader::new(file)).unwrap_or_default()
}

fn save_favorites(launcher_dir: &Path, favs: &Favorites) {
    if let Ok(json) = serde_json::to_string_pretty(favs) {
        if let Err(e) = fs::write(favorites_path(launcher_dir), json) {
            println!("Konnte favorites.json nicht speichern: {e}");
        }
    }
}

// Globaler Cache, damit Favoriten-Statusabfragen günstig sind
static FAVORITES_CACHE: OnceLock<Mutex<Favorites>> = OnceLock::new();
fn favorites_cache() -> &'static Mutex<Favorites> {
    FAVORITES_CACHE.get_or_init(|| Mutex::new(Favorites::default()))
}

/// Prüft, ob ein Key in der entsprechenden Kategorie markiert ist.
/// kind = "instances" | "modpacks" | "mods"
fn is_favorite(kind: &str, key: &str) -> bool {
    let Ok(g) = favorites_cache().lock() else { return false; };
    match kind {
        "instances" => g.instances.iter().any(|x| x == key),
        "modpacks"  => g.modpacks.iter().any(|x| x == key),
        "mods"      => g.mods.iter().any(|x| x == key),
        _ => false,
    }
}

/// Toggelt einen Favoriten und schreibt die Datei neu.
/// kind = "instances" | "modpacks" | "mods"
/// Rückgabe: neuer Zustand (true = jetzt favorisiert)
fn toggle_favorite(launcher_dir: &Path, kind: &str, key: &str) -> bool {
    let mut new_state = false;
    {
        let Ok(mut g) = favorites_cache().lock() else { return false; };
        let list = match kind {
            "instances" => &mut g.instances,
            "modpacks"  => &mut g.modpacks,
            "mods"      => &mut g.mods,
            _ => return false,
        };
        if let Some(pos) = list.iter().position(|x| x == key) {
            list.remove(pos);
            new_state = false;
        } else {
            list.push(key.to_string());
            new_state = true;
        }
        if let Ok(json) = serde_json::to_string_pretty(&*g) {
            let _ = fs::write(favorites_path(launcher_dir), json);
        }
    }
    new_state
}

// ===================== Presets (Neue Features 3) =====================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Preset {
    id: String,               // Eindeutig, z.B. "preset-<timestamp>"
    name: String,
    minecraft_version: String,
    loader: String,           // "vanilla" | "fabric" | "forge" | "neoforge" | "quilt"
    loader_version: Option<String>,
    mods: Vec<PresetMod>,     // Liste der enthaltenen Mods
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PresetMod {
    project_id: String,
    project_name: String,
    icon_cache_key: Option<String>,  // project_id oder CF-id
}

fn presets_path(launcher_dir: &Path) -> PathBuf {
    launcher_dir.join("presets.json")
}

fn load_presets(launcher_dir: &Path) -> Vec<Preset> {
    let Ok(file) = File::open(presets_path(launcher_dir)) else { return Vec::new(); };
    serde_json::from_reader(BufReader::new(file)).unwrap_or_default()
}

fn save_presets(launcher_dir: &Path, presets: &[Preset]) {
    if let Ok(json) = serde_json::to_string_pretty(presets) {
        if let Err(e) = fs::write(presets_path(launcher_dir), json) {
            println!("Konnte presets.json nicht speichern: {e}");
        }
    }
}

// ===================== Welten + Server (Feature 14) =====================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct WorldInfo {
    folder_name: String,          // Ordnername in saves/
    display_name: String,         // aus level.dat (falls lesbar)
    icon_path: Option<String>,    // saves/<name>/icon.png
    last_played_ms: i64,          // aus level.dat (0 wenn unbekannt)
    game_type: i32,               // 0=Survival, 1=Creative, 2=Adventure, 3=Spectator
    hardcore: bool,
    version: String,              // Minecraft-Version der Welt
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ServerInfo {
    name: String,
    ip: String,
    icon_base64: Option<String>,  // base64 PNG aus servers.dat (Minecraft-Format)
    // Status-Felder (per Ping aktualisiert, nicht persistiert)
    #[serde(skip)]
    status: String,               // "unknown" | "online" | "offline" | "checking"
    #[serde(skip)]
    players_online: i32,
    #[serde(skip)]
    players_max: i32,
    #[serde(skip)]
    motd: String,
}

/// Liest alle Welten aus <instance>/saves/. Die level.dat wird per fastnbt
/// geparst, aber Fehler führen nur zu Default-Werten statt zu einem Abbruch.
fn read_worlds(instance_dir: &Path) -> Vec<WorldInfo> {
    let saves = instance_dir.join("saves");
    let Ok(entries) = fs::read_dir(&saves) else { return Vec::new(); };
    let mut out = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() { continue; }

        let folder_name = entry.file_name().to_string_lossy().to_string();
        let mut info = WorldInfo {
            folder_name: folder_name.clone(),
            display_name: folder_name.clone(),
            ..Default::default()
        };

        // icon.png im Welt-Ordner
        let icon = path.join("icon.png");
        if icon.is_file() {
            info.icon_path = Some(icon.to_string_lossy().to_string());
        }

        // level.dat lesen (NBT gzip-komprimiert). fastnbt liefert ein Enum,
        // deshalb matchen wir direkt auf Value::Compound / Value::String / ...
        if let Ok(bytes) = fs::read(path.join("level.dat")) {
            if let Ok(fastnbt::Value::Compound(root)) = fastnbt::from_bytes::<fastnbt::Value>(&bytes) {
                if let Some(fastnbt::Value::Compound(data)) = root.get("Data") {
                    if let Some(fastnbt::Value::String(name)) = data.get("LevelName") {
                        info.display_name = name.clone();
                    }
                    if let Some(fastnbt::Value::Long(lp)) = data.get("LastPlayed") {
                        info.last_played_ms = *lp;
                    }
                    if let Some(fastnbt::Value::Int(gt)) = data.get("GameType") {
                        info.game_type = *gt;
                    }
                    if let Some(fastnbt::Value::Byte(hc)) = data.get("hardcore") {
                        info.hardcore = *hc != 0;
                    }
                    if let Some(fastnbt::Value::Compound(ver)) = data.get("Version") {
                        if let Some(fastnbt::Value::String(name)) = ver.get("Name") {
                            info.version = name.clone();
                        }
                    }
                }
            }
        }

        out.push(info);
    }

    // Neueste zuerst
    out.sort_by(|a, b| b.last_played_ms.cmp(&a.last_played_ms));
    out
}

/// Liest <instance>/servers.dat (NBT, unkomprimiert). Liefert leere Liste,
/// wenn die Datei fehlt oder nicht lesbar ist.
fn read_servers(instance_dir: &Path) -> Vec<ServerInfo> {
    let path = instance_dir.join("servers.dat");
    let Ok(bytes) = fs::read(&path) else { return Vec::new(); };
    let Ok(fastnbt::Value::Compound(root)) = fastnbt::from_bytes::<fastnbt::Value>(&bytes) else { return Vec::new(); };

    let mut out = Vec::new();
    if let Some(fastnbt::Value::List(servers)) = root.get("servers") {
        for s in servers {
            if let fastnbt::Value::Compound(srv) = s {
                let name = match srv.get("name") {
                    Some(fastnbt::Value::String(x)) => x.clone(),
                    _ => String::new(),
                };
                let ip = match srv.get("ip") {
                    Some(fastnbt::Value::String(x)) => x.clone(),
                    _ => continue,
                };
                if ip.is_empty() { continue; }
                let icon_b64 = match srv.get("icon") {
                    Some(fastnbt::Value::String(x)) => Some(x.clone()),
                    _ => None,
                };
                out.push(ServerInfo {
                    name,
                    ip,
                    icon_base64: icon_b64,
                    status: "unknown".to_string(),
                    players_online: 0,
                    players_max: 0,
                    motd: String::new(),
                });
            }
        }
    }
    out
}

/// Konvertiert einen Unix-ms-Timestamp in einen relativen Text wie
/// "vor 2 Tagen" oder "gerade eben". 0 = nie gespielt.
fn format_relative_time(ms: i64) -> String {
    if ms <= 0 { return "nie".to_string(); }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let diff_sec = ((now - ms) / 1000).max(0);
    if diff_sec < 60 { "gerade eben".to_string() }
    else if diff_sec < 3600 { format!("vor {} min", diff_sec / 60) }
    else if diff_sec < 86400 { format!("vor {} h", diff_sec / 3600) }
    else { format!("vor {} Tagen", diff_sec / 86400) }
}

/// Minecraft speichert Server-Icons als base64-String, dessen Inhalt
/// ein "data:image/png;base64,<...>"-Präfix haben kann oder auch nicht.
/// Wir dekodieren und schreiben in den Cache.
fn decode_server_icon(b64: &Option<String>, launcher_dir: &Path) -> Image {
    use base64::Engine;
    let Some(b64) = b64 else { return Image::default(); };
    let payload = b64.split(',').last().unwrap_or(b64);

    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(payload) else { return Image::default(); };

    // Cache-Pfad mit Hash des Inhalts, damit wir nicht jedes Mal neu dekodieren
    let hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut h);
        h.finish()
    };
    let cache_dir = launcher_dir.join("icon_cache");
    let cache_path = cache_dir.join(format!("srv_{:x}.png", hash));

    if !cache_path.is_file() {
        if let Ok(img) = image::load_from_memory(&bytes) {
            let _ = fs::create_dir_all(&cache_dir);
            let _ = img.save_with_format(&cache_path, image::ImageFormat::Png);
        } else {
            return Image::default();
        }
    }

    Image::load_from_path(&cache_path).unwrap_or_default()
}

/// Minecraft-SRV-Lookup: _minecraft._tcp.<host> -> (echter Host, echter Port).
/// None, wenn es keinen SRV-Eintrag gibt (dann gilt der Standardport 25565).
async fn resolve_srv(host: &str) -> Option<(String, u16)> {
    use hickory_resolver::TokioAsyncResolver;
    let resolver = TokioAsyncResolver::tokio_from_system_conf().ok()?;
    let lookup = resolver.srv_lookup(format!("_minecraft._tcp.{host}.")).await.ok()?;
    let rec = lookup.iter().next()?;
    let target = rec.target().to_utf8().trim_end_matches('.').to_string();
    Some((target, rec.port()))
}

/// Liest den Minecraft-Server-Status per ServerListPing-Protokoll (1.7+).
/// `host` = "mc.example.com" oder "mc.example.com:25565" (Port optional).
async fn ping_server(host: &str) -> Result<(i32, i32, String), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut hostname, mut port) = match host.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(25565)),
        None => (host.to_string(), 25565u16),
    };

    // Ohne expliziten Port: wie der Minecraft-Client zuerst den SRV-Eintrag fragen
    if !host.contains(':') {
        if let Some((srv_host, srv_port)) = resolve_srv(&hostname).await {
            println!("🔎 SRV: {} -> {}:{}", hostname, srv_host, srv_port);
            hostname = srv_host;
            port = srv_port;
        }
    }

    fn write_varint(buf: &mut Vec<u8>, value: i32) {
        let mut v = value as u32;
        loop {
            if v & !0x7F == 0 { buf.push(v as u8); break; }
            buf.push(((v & 0x7F) | 0x80) as u8);
            v >>= 7;
        }
    }

    // Liest ein VarInt direkt aus dem Stream (Byte für Byte)
    async fn read_varint<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> Result<i32, String> {
        let mut result: i32 = 0;
        let mut shift = 0;
        loop {
            let b = r.read_u8().await.map_err(|e| format!("Read: {e}"))?;
            result |= ((b & 0x7F) as i32) << shift;
            if b & 0x80 == 0 { break; }
            shift += 7;
            if shift >= 35 { return Err("VarInt zu lang".to_string()); }
        }
        Ok(result)
    }

    let mut stream = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::net::TcpStream::connect((hostname.as_str(), port)),
    ).await.map_err(|_| "Connect-Timeout".to_string())?
     .map_err(|e| format!("Connect: {e}"))?;

    // Handshake-Paket: ID 0x00, Protokoll, Host, Port, Next State = 1 (Status)
    let mut hs = Vec::new();
    write_varint(&mut hs, 0x00);
    write_varint(&mut hs, 767);
    write_varint(&mut hs, hostname.len() as i32);
    hs.extend_from_slice(hostname.as_bytes());
    hs.extend_from_slice(&port.to_be_bytes());
    write_varint(&mut hs, 0x01);

    let mut out = Vec::new();
    write_varint(&mut out, hs.len() as i32);
    out.extend(hs);
    // Status-Request: Länge 1, Paket-ID 0x00 (vorher fälschlich Länge 0)
    out.extend_from_slice(&[0x01, 0x00]);

    stream.write_all(&out).await.map_err(|e| format!("Send: {e}"))?;

    // Antwort sauber lesen: Paketlänge, Paket-ID, String-Länge, dann genau so viele Bytes
    let json_bytes = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let _packet_len = read_varint(&mut stream).await?;
        let packet_id = read_varint(&mut stream).await?;
        if packet_id != 0x00 { return Err(format!("Unerwartete Paket-ID {packet_id}")); }
        let str_len = read_varint(&mut stream).await?;
        if str_len <= 0 || str_len > 4 * 1024 * 1024 { return Err("Ungültige Antwortlänge".to_string()); }
        let mut data = vec![0u8; str_len as usize];
        stream.read_exact(&mut data).await.map_err(|e| format!("Read: {e}"))?;
        Ok(data)
    }).await.map_err(|_| "Read-Timeout".to_string())??;

    let v: serde_json::Value = serde_json::from_slice(&json_bytes).map_err(|e| format!("JSON: {e}"))?;
    let players_online = v.get("players").and_then(|p| p.get("online")).and_then(|x| x.as_i64()).unwrap_or(0) as i32;
    let players_max = v.get("players").and_then(|p| p.get("max")).and_then(|x| x.as_i64()).unwrap_or(0) as i32;

    // MOTD: String, Objekt mit "text" oder Objekt mit "extra"-Liste
    let motd = match v.get("description") {
        Some(d) if d.is_string() => d.as_str().unwrap_or("").to_string(),
        Some(d) => {
            let mut s = d.get("text").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if let Some(extra) = d.get("extra").and_then(|e| e.as_array()) {
                for part in extra {
                    if let Some(t) = part.get("text").and_then(|x| x.as_str()) { s.push_str(t); }
                }
            }
            s
        }
        None => String::new(),
    };

    Ok((players_online, players_max, motd))
}

static MC_RUNNING: AtomicBool = AtomicBool::new(false);

// Hält den Stop-Flag der aktuell laufenden Console-Stream-Aufgabe, damit wir
// sie beim Öffnen einer anderen Instanz oder beim Schließen sauber beenden
// und ihren Speicher wieder freigeben können.
struct ConsoleStreamState {
    stop_flag: Arc<AtomicBool>,
}

static CONSOLE_STREAM: OnceLock<Mutex<Option<ConsoleStreamState>>> = OnceLock::new();
fn console_stream_state() -> &'static Mutex<Option<ConsoleStreamState>> {
    CONSOLE_STREAM.get_or_init(|| Mutex::new(None))
}

/// Wie viele parallele Aufgaben dürfen wir starten?
/// Während MC läuft deutlich weniger, um dessen Ressourcen nicht zu stehlen.
fn parallel_limit(cap: usize) -> usize {
    if MC_RUNNING.load(AtomicOrdering::Relaxed) {
        cap.min(2)
    } else {
        cap
    }
}

// =====================================================================================
// ===================== NEU: Absturz-Analyse ==========================================
// =====================================================================================
// Idee: Ein Absturz hinterlässt Spuren (crash-reports/*.txt und logs/latest.log).
// Wir lesen beides und prüfen eine Liste von Regeln (reines String-Matching, keine
// zusätzliche Crate nötig). Jede Regel liefert einen "Fund" mit Erklärung + Lösung.

#[derive(Debug, Clone)]
struct CrashFinding {
    title: String,
    explanation: String,
    suggestion: String,
    mod_file: String, // Dateiname des verdächtigen Mods (für den Deaktivieren-Button), leer = keiner
}

// Kurzschreibweise zum Bauen eines Fundes
fn cf(title: &str, explanation: String, suggestion: String, mod_file: String) -> CrashFinding {
    CrashFinding { title: title.to_string(), explanation, suggestion, mod_file }
}

// Text auf n Zeichen kürzen (die UI-Karten haben feste Höhen, siehe Slint)
fn short(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n).collect::<String>() + "…" }
}

// Neuester Crash-Report. since = Some(t): nur Reports, die NACH t geschrieben wurden
// (sonst würden wir bei einem normalen Crash einen uralten Report anzeigen).
fn newest_crash_report_path(instance_dir: &Path, since: Option<std::time::SystemTime>) -> Option<PathBuf> {
    let rd = fs::read_dir(instance_dir.join("crash-reports")).ok()?;
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("txt") { continue; }
        let Ok(modified) = e.metadata().and_then(|m| m.modified()) else { continue; };
        if let Some(s) = since { if modified < s { continue; } }
        if best.as_ref().map(|b| modified > b.0).unwrap_or(true) { best = Some((modified, p)); }
    }
    best.map(|b| b.1)
}

// Letzte `max` Bytes einer (evtl. riesigen) Log-Datei lesen
fn read_log_tail(path: &Path, max: u64) -> String {
    let Ok(mut f) = File::open(path) else { return String::new(); };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len > max { let _ = f.seek(SeekFrom::Start(len - max)); }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).to_string()
}

// Findet die Mod-Datei (aktiv, .jar) zu einer Mod-ID. Erst schnell über den Dateinamen,
// dann genau über fabric.mod.json / mods.toml in den Jars. Leer = nichts gefunden.
fn file_for_mod_id(instance_dir: &Path, id: &str) -> String {
    let want = id.trim().to_lowercase();
    // Pseudo-Mods, zu denen es keine Datei gibt
    if want.len() < 2 || ["minecraft", "java", "forge", "neoforge", "fabric", "fabricloader", "quilt_loader"].contains(&want.as_str()) {
        return String::new();
    }
    let Ok(rd) = fs::read_dir(instance_dir.join("mods")) else { return String::new(); };
    let jars: Vec<PathBuf> = rd.flatten().map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jar"))
        .collect();
    let norm = |s: &str| s.to_lowercase().replace('_', "-");

    // 1. Schnell: steckt die ID im Dateinamen?
    for p in &jars {
        let f = p.file_name().unwrap_or_default().to_string_lossy().to_string();
        if norm(&f).contains(&norm(&want)) { return f; }
    }
    // 2. Genau: ID in der Jar selbst nachlesen
    for p in &jars {
        let Ok(file) = File::open(p) else { continue; };
        let Ok(mut z) = zip::ZipArchive::new(file) else { continue; };
        let mut ids: Vec<String> = Vec::new();
        if let Some(t) = read_zip_text(&mut z, "fabric.mod.json") {
            if let Ok(j) = serde_json::from_str::<serde_json::Value>(&t) {
                if let Some(i) = j.get("id").and_then(|v| v.as_str()) { ids.push(i.to_lowercase()); }
            }
        }
        for n in ["META-INF/neoforge.mods.toml", "META-INF/mods.toml"] {
            if let Some(t) = read_zip_text(&mut z, n) {
                for l in t.lines() {
                    if let Some((k, v)) = toml_kv(l.trim()) { if k == "modId" { ids.push(v.to_lowercase()); } }
                }
            }
        }
        if ids.iter().any(|i| *i == want) {
            return p.file_name().unwrap_or_default().to_string_lossy().to_string();
        }
    }
    String::new()
}

// Zieht die Mod-ID aus einer Abhängigkeits-Zeile.
//  Fabric: "Mod 'Iris' (iris) 1.7 requires ..."  -> "iris"
//  Forge:  "Mod ID: 'x', Requested by: 'y', ..."  -> "y"
fn mod_id_from_dep_line(l: &str) -> String {
    const REQ: &str = "Requested by: '";
    if let Some(i) = l.find(REQ) {
        return l[i + REQ.len()..].split('\'').next().unwrap_or("").to_string();
    }
    if let Some(i) = l.find("' (") {
        return l[i + 3..].split(')').next().unwrap_or("").to_string();
    }
    String::new()
}

// Die eigentliche Analyse. Blockierend (Dateien lesen) -> per spawn_blocking aufrufen.
fn analyze_crash(instance_dir: &Path, mc: &str, ram_mb: i32, since: Option<std::time::SystemTime>) -> Vec<CrashFinding> {
    // Report (nur wenn neu genug) + Ende der latest.log
    let report = newest_crash_report_path(instance_dir, since)
        .and_then(|p| fs::read(&p).ok())
        .map(|b| String::from_utf8_lossy(&b[..b.len().min(400_000)]).to_string())
        .unwrap_or_default();
    let log = read_log_tail(&instance_dir.join("logs").join("latest.log"), 300_000);

    if report.is_empty() && log.is_empty() {
        return vec![cf(
            "Keine Daten gefunden",
            "Es gibt weder einen passenden Crash-Report noch eine logs/latest.log in dieser Instanz.".to_string(),
            "Starte die Instanz einmal und analysiere erneut nach dem Absturz.".to_string(),
            String::new(),
        )];
    }

    let all = format!("{report}\n{log}");
    let lower = all.to_lowercase();
    let mut out: Vec<CrashFinding> = Vec::new();

    // ---- Regel 1: falsche Java-Version -------------------------------------------------
    // "class file version 65.0"  bzw.  "Unsupported class file major version 65"
    // Java-Version = Klassenversion - 44 (65 -> 21, 61 -> 17, 52 -> 8)
    for needle in ["class file version ", "class file major version "] {
        if let Some(i) = all.find(needle) {
            let num: String = all[i + needle.len()..].chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(v) = num.parse::<u32>() {
                if v > 44 {
                    let need = v - 44;
                    out.push(cf(
                        "Falsche Java-Version",
                        format!("Ein Mod (oder Minecraft selbst) wurde für Java {need} gebaut (Klassenversion {v}), die Instanz läuft aber mit einer älteren Java-Version."),
                        format!("Setze in den Erweiterten Einstellungen der Instanz den Haken für die Überschreibung und wähle Java {need} (oder neuer), dann speichern."),
                        String::new(),
                    ));
                    break;
                }
            }
        }
    }

    // ---- Regel 2: zu wenig Arbeitsspeicher ---------------------------------------------
    if all.contains("OutOfMemoryError") {
        let kind = if all.contains("Metaspace") { "Metaspace (Speicher für Klassen)" }
                   else if all.contains("GC overhead") { "GC-Overhead (Java kommt mit dem Aufräumen nicht mehr hinterher)" }
                   else { "Heap (normaler Arbeitsspeicher)" };
        let wish = ((ram_mb + ram_mb / 2) + 511) / 512 * 512; // +50 %, auf 512 MB gerundet
        out.push(cf(
            "Zu wenig Arbeitsspeicher",
            format!("Java ist der Speicher ausgegangen: {kind}. Aktuell sind {ram_mb} MB zugewiesen."),
            format!("Erhöhe den RAM der Instanz (Erweiterte Einstellungen), z.B. auf {wish} MB. Nimm aber nicht mehr als ca. 75 % deines Systemspeichers. Bei Metaspace hilft zusätzlich das JVM-Argument -XX:MaxMetaspaceSize=512m im Misc-Popup."),
            String::new(),
        ));
    }

    // ---- Regel 3+4: fehlende / falsche Abhängigkeiten (Fabric, Forge, NeoForge) ---------
    let mut dep_lines: Vec<String> = Vec::new();
    for l in all.lines() {
        let t = l.trim().trim_start_matches("- ").trim();
        if t.len() < 12 || t.len() > 400 { continue; }
        let tl = t.to_lowercase();
        let fabric = tl.contains(" requires ") && (tl.contains("which is missing") || tl.contains("which is incompatible") || tl.contains("but only"));
        let forge = t.contains("Mod ID:") && t.contains("Requested by");
        let neo = tl.contains(" requires ") && tl.contains("is not installed");
        let old = t.contains("Could not find required mod");
        if (fabric || forge || neo || old) && dep_lines.len() < 8 && !dep_lines.iter().any(|x| x == t) {
            dep_lines.push(t.to_string());
        }
    }
    // Zeilen, die "minecraft" betreffen = Mod passt nicht zur MC-Version, der Rest = fehlende Mods
    let (mc_lines, other_lines): (Vec<&String>, Vec<&String>) =
        dep_lines.iter().partition(|l| l.to_lowercase().contains("minecraft"));
    if !other_lines.is_empty() {
        let text = other_lines.iter().take(3).map(|l| short(l, 150)).collect::<Vec<_>>().join("\n");
        let id = mod_id_from_dep_line(other_lines[0]);
        out.push(cf(
            "Fehlende oder falsche Abhängigkeit",
            text,
            "Installiere den genannten Mod in der passenden Version (Add-Mod-Popup) oder deaktiviere den Mod, der ihn verlangt.".to_string(),
            file_for_mod_id(instance_dir, &id),
        ));
    }
    if !mc_lines.is_empty() {
        let text = mc_lines.iter().take(3).map(|l| short(l, 150)).collect::<Vec<_>>().join("\n");
        let id = mod_id_from_dep_line(mc_lines[0]);
        out.push(cf(
            "Mod passt nicht zur Minecraft-Version",
            text,
            format!("Der Mod ist für eine andere Minecraft-Version oder einen anderen Loader gebaut. Hol dir die Version für {mc} oder entferne ihn."),
            file_for_mod_id(instance_dir, &id),
        ));
    }

    // ---- Regel 5: "Suspected Mods" im Crash-Report (Forge/NeoForge) ------------------------
    if let Some(i) = report.find("Suspected Mod") {
        let mut it = report[i..].lines();
        let first = it.next().unwrap_or("");
        if !first.contains("NONE") {
            let mut n = 0;
            for l in it.take(14) {
                let t = l.trim();
                if t.is_empty() { break; }
                if !(t.contains('(') && t.contains("Version:")) { continue; } // Zeilen wie "Issue tracker URL" überspringen
                let name = t.split('(').next().unwrap_or("").trim().to_string();
                let id = t.split('(').nth(1).and_then(|x| x.split(')').next()).unwrap_or("").to_string();
                out.push(cf(
                    &format!("Verdächtiger Mod: {name}"),
                    format!("Der Crash-Report nennt diesen Mod als wahrscheinliche Ursache: {}", short(t, 120)),
                    "Aktualisiere den Mod oder deaktiviere ihn testweise und starte erneut.".to_string(),
                    file_for_mod_id(instance_dir, &id),
                ));
                n += 1;
                if n >= 3 { break; }
            }
        }
    }

    // ---- Regel 6: Mixin-Fehler -------------------------------------------------------------
    let mixin_markers = ["MixinApplyError", "InvalidInjectionException", "Mixin apply failed", "MixinTransformerError", "Critical injection failure", "MixinPreProcessorException"];
    if mixin_markers.iter().any(|m| all.contains(m)) {
        const FROM: &str = "from mod ";
        let id: String = all.find(FROM)
            .map(|i| all[i + FROM.len()..].chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '-').collect())
            .unwrap_or_default();
        let who = if id.is_empty() { String::new() } else { format!(" Betroffen ist laut Log: {id}.") };
        out.push(cf(
            "Mixin-Fehler (Mod-Konflikt)",
            format!("Ein Mixin (so hängen sich Mods in Minecraft-Code ein) konnte nicht angewendet werden.{who} Meist passen zwei Mods nicht zusammen oder ein Mod nicht zur Minecraft-/Loader-Version."),
            "Aktualisiere den genannten Mod oder deaktiviere ihn. Verdächtig sind oft mehrere Mods, die dasselbe ändern (z.B. mehrere Rendering-/Performance-Mods).".to_string(),
            file_for_mod_id(instance_dir, &id),
        ));
    }

    // ---- Regel 7: doppelte Mods -----------------------------------------------------------
    if lower.contains("duplicate mod") || lower.contains("duplicatemodsfoundexception") || lower.contains("found duplicate mods") {
        out.push(cf(
            "Doppelte Mods",
            "Derselbe Mod liegt in mehreren Versionen im mods-Ordner.".to_string(),
            "Doppelte Mods erscheinen in der Mod-Liste orange. Behalte nur eine Datei (oder nutze 'Doppelte Mods beheben').".to_string(),
            String::new(),
        ));
    }

    // ---- Regel 8: GLFW / OpenGL / Grafiktreiber -------------------------------------------
    if lower.contains("glfw error") || lower.contains("pixel format not accelerated") || lower.contains("failed to create window") {
        out.push(cf(
            "Grafik-/Fensterproblem (GLFW/OpenGL)",
            "Minecraft konnte kein Fenster oder keinen OpenGL-Kontext erzeugen.".to_string(),
            "Im Misc-Popup 'System-GLFW verwenden' umschalten, Grafiktreiber aktualisieren. Bei Laptops mit zwei GPUs den Wrapper prime-run testen.".to_string(),
            String::new(),
        ));
    }

    // ---- Schwacher Hinweis: fehlende Klasse (nur wenn sonst nichts gefunden wurde) -----------
    if out.is_empty() {
        for needle in ["NoClassDefFoundError: ", "ClassNotFoundException: "] {
            if let Some(i) = all.find(needle) {
                let cls: String = all[i + needle.len()..].chars().take_while(|c| !c.is_whitespace()).collect();
                out.push(cf(
                    "Fehlende Klasse",
                    format!("Java findet die Klasse {} nicht.", short(&cls, 100)),
                    "Meist fehlt eine Library-Mod oder ein Mod ist für eine andere Version gebaut. Prüfe die Abhängigkeiten der zuletzt installierten Mods.".to_string(),
                    String::new(),
                ));
                break;
            }
        }
    }

    // ---- Fallback ---------------------------------------------------------------------------
    if out.is_empty() {
        let desc = report.lines().find(|l| l.starts_with("Description:")).unwrap_or("").to_string();
        let caused = all.lines().filter(|l| l.contains("Caused by:")).last().unwrap_or("").trim().to_string();
        out.push(cf(
            "Keine bekannte Ursache erkannt",
            format!("{} {}", short(&desc, 100), short(&caused, 160)).trim().to_string(),
            "Öffne den Report/das Log über den Button unten und suche nach 'Caused by'. Die letzte Mod, die du installiert oder aktualisiert hast, ist meist der Verdächtige.".to_string(),
            String::new(),
        ));
    }
    out
}

// Analyse im Hintergrund starten und das Ergebnis-Popup öffnen.
// since = Some(..): automatischer Aufruf nach einem Crash, None: manueller Button.
async fn show_crash_analysis(ui: Weak<AppWindow>, name: String, since: Option<std::time::SystemTime>) {
    let dir = launcher_dir().join("instances").join(&name);
    let mc = load_instance_config(&dir).map(|c| c.minecraft_version).unwrap_or_default();
    // Effektiver RAM wie beim Start: Instanz-Override (falls aktiv), sonst Settings
    let ov = load_instance_overrides(&dir);
    let ram = if ov.enabled { ov.ram_mb } else { load_settings(&launcher_dir()).default_ram_mb };

    let d2 = dir.clone();
    let findings = tokio::task::spawn_blocking(move || analyze_crash(&d2, &mc, ram, since))
        .await.unwrap_or_default();

    let summary = if since.is_some() {
        format!("Minecraft ist abgestürzt. {} mögliche Ursache(n), die oberste ist meist die wahrscheinlichste.", findings.len())
    } else {
        format!("{} mögliche Ursache(n) gefunden.", findings.len())
    };

    let _ = ui.upgrade_in_event_loop(move |ui| {
        // CrashFindingUi ist nicht Send, deshalb erst hier im UI-Thread bauen
        let items: Vec<CrashFindingUi> = findings.into_iter().map(|f| CrashFindingUi {
            title: f.title.into(),
            explanation: f.explanation.into(),
            suggestion: f.suggestion.into(),
            mod_file: f.mod_file.into(),
        }).collect();
        ui.set_crash_instance(name.into());
        ui.set_crash_findings(ModelRc::new(VecModel::from(items)));
        ui.set_crash_summary(summary.into());
        ui.set_crash_status("".into());
        ui.set_crash_popup_visible(true);
    });
}

// =====================================================================================
// ===================== NEU: Lokale Server ============================================
// =====================================================================================
// Aufbau:  <launcher>/servers/<name>/        = Server-Ordner (Welt, mods, server.properties ...)
//          <launcher>/local_servers.json      = Liste aller Server (Name, MC-Version, Loader, RAM, EULA ...)
// Installiert wird beim ERSTEN Start (Java laden, Server-Jar bzw. Loader-Installer ausführen).

fn local_servers_path() -> PathBuf { launcher_dir().join("local_servers.json") }
fn servers_root() -> PathBuf { launcher_dir().join("servers") }
fn server_dir(name: &str) -> PathBuf { servers_root().join(name) }

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)] // neue Felder brechen alte Dateien nie
struct LocalServer {
    name: String,
    minecraft_version: String,
    loader: String,                 // "vanilla" | "fabric" | "forge" | "neoforge"
    loader_version: Option<String>, // None = beim Installieren die neueste stabile
    ram_mb: i32,
    installed: bool,
    eula_accepted: bool,            // nur true, wenn der Nutzer den Haken selbst gesetzt hat
    source_instance: Option<String>,
}

impl LocalServer {
    fn new(name: String, mc: String, loader: String) -> Self {
        let loader = if loader == "none" || loader.is_empty() { "vanilla".to_string() } else { loader };
        Self { name, minecraft_version: mc, loader, loader_version: None, ram_mb: 2048,
               installed: false, eula_accepted: false, source_instance: None }
    }
}

fn load_local_servers() -> Vec<LocalServer> {
    let Ok(f) = File::open(local_servers_path()) else { return Vec::new(); };
    serde_json::from_reader(BufReader::new(f)).unwrap_or_default()
}

fn save_local_servers(list: &[LocalServer]) {
    let _ = fs::create_dir_all(servers_root());
    if let Ok(json) = serde_json::to_string_pretty(list) {
        if let Err(e) = fs::write(local_servers_path(), json) { println!("Konnte local_servers.json nicht speichern: {e}"); }
    }
}

// Einen Server laden, ändern, speichern
fn update_local_server(name: &str, f: impl FnOnce(&mut LocalServer)) {
    let mut all = load_local_servers();
    if let Some(s) = all.iter_mut().find(|s| s.name == name) { f(s); save_local_servers(&all); }
}

fn unique_server_name(base: &str) -> String {
    let existing: std::collections::HashSet<String> = load_local_servers().into_iter().map(|s| s.name).collect();
    if !existing.contains(base) && !server_dir(base).exists() { return base.to_string(); }
    let mut n = 2;
    loop {
        let c = format!("{base} ({n})");
        if !existing.contains(&c) && !server_dir(&c).exists() { return c; }
        n += 1;
    }
}

// ---- server.properties ----
fn read_props(dir: &Path) -> HashMap<String, String> {
    let mut m = HashMap::new();
    if let Ok(text) = fs::read_to_string(dir.join("server.properties")) {
        for l in text.lines() {
            if l.starts_with('#') { continue; }
            if let Some((k, v)) = l.split_once('=') { m.insert(k.trim().to_string(), v.trim().to_string()); }
        }
    }
    m
}

// Nur die übergebenen Schlüssel ändern/anhängen, Rest der Datei bleibt unangetastet.
// (Existiert die Datei noch nicht, ergänzt Minecraft beim ersten Start alle fehlenden Werte selbst.)
fn write_props(dir: &Path, updates: &[(&str, String)]) {
    let path = dir.join("server.properties");
    let text = fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    for (k, v) in updates {
        let prefix = format!("{k}=");
        if let Some(l) = lines.iter_mut().find(|l| l.starts_with(&prefix)) { *l = format!("{k}={v}"); }
        else { lines.push(format!("{k}={v}")); }
    }
    let _ = fs::write(&path, lines.join("\n") + "\n");
}

fn server_port(dir: &Path) -> i32 {
    read_props(dir).get("server-port").and_then(|p| p.parse().ok()).unwrap_or(25565)
}

// ---- Laufzeit-Zustand (global, weil UI-Callbacks und Tokio-Tasks darauf zugreifen) ----
struct ServerRuntime {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<String>, // Befehle -> stdin des Servers
    kill_tx: Option<tokio::sync::oneshot::Sender<()>>,  // Notbremse (Prozess killen)
}

static SERVER_RUNTIME: OnceLock<Mutex<HashMap<String, ServerRuntime>>> = OnceLock::new();
fn server_runtime() -> &'static Mutex<HashMap<String, ServerRuntime>> {
    SERVER_RUNTIME.get_or_init(|| Mutex::new(HashMap::new()))
}

// Konsolen-Zeilen pro Server (Ringpuffer, Größe = Device-Tier wie bei der Live-Konsole)
static SERVER_LOGS: OnceLock<Mutex<HashMap<String, std::collections::VecDeque<String>>>> = OnceLock::new();
fn server_logs() -> &'static Mutex<HashMap<String, std::collections::VecDeque<String>>> {
    SERVER_LOGS.get_or_init(|| Mutex::new(HashMap::new()))
}

// Server, die gerade installiert/gestartet werden (verhindert doppelten Start durch Doppelklick)
static SERVER_BUSY: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
fn server_busy() -> &'static Mutex<std::collections::HashSet<String>> {
    SERVER_BUSY.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}
struct BusyGuard(String);
impl Drop for BusyGuard {
    fn drop(&mut self) { server_busy().lock().unwrap().remove(&self.0); }
}

fn server_log_push(name: &str, lines: Vec<String>) {
    let cap = tier_log_lines(device_tier());
    let mut g = server_logs().lock().unwrap();
    let d = g.entry(name.to_string()).or_default();
    d.extend(lines);
    while d.len() > cap { d.pop_front(); }
}

// ---- Daten -> UI ----
fn push_servers_to_ui(ui: &AppWindow) {
    let all = load_local_servers();
    let ports: Vec<i32> = all.iter().map(|s| server_port(&server_dir(&s.name))).collect();
    let running = server_runtime().lock().unwrap();
    let items: Vec<LocalServerUi> = all.iter().enumerate().map(|(i, s)| LocalServerUi {
        name: s.name.clone().into(),
        mc_version: s.minecraft_version.clone().into(),
        loader: s.loader.clone().into(),
        port: ports[i],
        running: running.contains_key(&s.name),
        installed: s.installed,
        // Warnung, wenn ein ANDERER Server denselben Port nutzt
        port_conflict: ports.iter().enumerate().any(|(j, p)| j != i && *p == ports[i]),
    }).collect();
    drop(running);
    ui.set_srv_list(ModelRc::new(VecModel::from(items)));
}

fn srv_refresh_console(ui: &AppWindow, name: &str) {
    let lines: Vec<SharedString> = server_logs().lock().unwrap().get(name)
        .map(|d| d.iter().map(|l| SharedString::from(l.as_str())).collect())
        .unwrap_or_default();
    ui.set_srv_console_lines(ModelRc::new(VecModel::from(lines)));
    ui.set_srv_console_tick(ui.get_srv_console_tick().wrapping_add(1)); // Auto-Scroll auslösen
}

// Aus jedem Thread: Konsole aktualisieren, aber nur wenn dieser Server gerade ausgewählt ist
fn srv_refresh_console_post(ui: &Weak<AppWindow>, name: &str) {
    let n = name.to_string();
    let _ = ui.upgrade_in_event_loop(move |ui| {
        if ui.get_srv_selected().as_str() == n { srv_refresh_console(&ui, &n); }
    });
}

fn srv_status_post(ui: &Weak<AppWindow>, text: String) {
    let _ = ui.upgrade_in_event_loop(move |ui| ui.set_srv_status(text.into()));
}

// Server auswählen: alle Detail-Felder (RAM, EULA, Properties, Konsole) in die UI schreiben
fn srv_load_selected(ui: &AppWindow, name: &str) {
    let Some(s) = load_local_servers().into_iter().find(|s| s.name == name) else { return; };
    let props = read_props(&server_dir(name));
    ui.set_srv_selected(name.into());
    ui.set_srv_ram_gb(s.ram_mb as f32 / 1024.0);
    ui.set_srv_eula(s.eula_accepted);
    ui.set_srv_prop_port(props.get("server-port").cloned().unwrap_or_else(|| "25565".to_string()).into());
    ui.set_srv_prop_max(props.get("max-players").cloned().unwrap_or_else(|| "20".to_string()).into());
    ui.set_srv_prop_motd(props.get("motd").cloned().unwrap_or_else(|| "A Minecraft Server".to_string()).into());
    ui.set_srv_prop_difficulty(props.get("difficulty").cloned().unwrap_or_else(|| "normal".to_string()).into());
    ui.set_srv_prop_whitelist(props.get("white-list").map(|v| v == "true").unwrap_or(false));
    ui.set_srv_prop_online(props.get("online-mode").map(|v| v == "true").unwrap_or(true));
    ui.set_srv_confirm_delete(false);
    srv_refresh_console(ui, name);
}

// ---- Download-Helfer (mit HTTP-Statusprüfung, damit eine 404-Seite nicht als Jar gespeichert wird) ----
async fn download_file_checked(url: &str, path: &Path) -> Result<(), String> {
    let resp = http_client().get(url).send().await.map_err(|e| format!("Download fehlgeschlagen: {e}"))?;
    if !resp.status().is_success() { return Err(format!("HTTP {} bei {url}", resp.status())); }
    let bytes = resp.bytes().await.map_err(|e| format!("Konnte Antwort nicht lesen: {e}"))?;
    fs::write(path, &bytes).map_err(|e| format!("Datei schreiben: {e}"))
}

async fn get_json(url: &str) -> Result<serde_json::Value, String> {
    let resp = http_client().get(url).send().await.map_err(|e| format!("Abfrage fehlgeschlagen: {e}"))?;
    if !resp.status().is_success() { return Err(format!("HTTP {} bei {url}", resp.status())); }
    resp.json::<serde_json::Value>().await.map_err(|e| format!("Antwort ungültig: {e}"))
}

// ---- Installation (je Loader anders). Gibt die verwendete Loader-Version zurück. ----
async fn install_local_server(s: &LocalServer, dir: &Path, java: &Path, ui: &Weak<AppWindow>) -> Result<Option<String>, String> {
    let mc = s.minecraft_version.as_str();
    match s.loader.as_str() {
        // Vanilla: Server-Jar steht in der Version-JSON von Mojang
        "vanilla" | "none" | "" => {
            srv_status_post(ui, format!("Lade Vanilla-Server {mc}..."));
            let manifest = get_json("https://piston-meta.mojang.com/mc/game/version_manifest_v2.json").await?;
            let ver_url = manifest["versions"].as_array()
                .and_then(|v| v.iter().find(|x| x["id"].as_str() == Some(mc)))
                .and_then(|x| x["url"].as_str())
                .ok_or("Version nicht im Mojang-Manifest gefunden.")?.to_string();
            let vj = get_json(&ver_url).await?;
            let jar = vj["downloads"]["server"]["url"].as_str()
                .ok_or("Diese Minecraft-Version hat keinen Server-Download.")?.to_string();
            download_file_checked(&jar, &dir.join("server.jar")).await?;
            Ok(None)
        }
        // Fabric: fertiger Server-Launcher direkt von meta.fabricmc.net
        "fabric" => {
            let lv = match s.loader_version.clone() {
                Some(v) => v,
                None => latest_loader_version("fabric", mc).await.ok_or("Fabric-Loader-Version nicht ermittelbar.")?,
            };
            srv_status_post(ui, format!("Lade Fabric-Server ({lv})..."));
            let installers = get_json("https://meta.fabricmc.net/v2/versions/installer").await?;
            let inst = installers.as_array()
                .and_then(|a| a.iter().find(|x| x["stable"].as_bool() == Some(true)))
                .and_then(|x| x["version"].as_str())
                .ok_or("Fabric-Installer-Version nicht ermittelbar.")?.to_string();
            let url = format!("https://meta.fabricmc.net/v2/versions/loader/{mc}/{lv}/{inst}/server/jar");
            download_file_checked(&url, &dir.join("fabric-server-launch.jar")).await?;
            Ok(Some(lv))
        }
        // Forge / NeoForge: Installer laden und mit --installServer ausführen
        "forge" | "neoforge" => {
            let lv = match s.loader_version.clone() {
                Some(v) => v,
                None => latest_loader_version(&s.loader, mc).await.ok_or("Loader-Version nicht ermittelbar.")?,
            };
            let url = if s.loader == "forge" {
                let full = format!("{mc}-{lv}");
                format!("https://maven.minecraftforge.net/net/minecraftforge/forge/{full}/forge-{full}-installer.jar")
            } else {
                format!("https://maven.neoforged.net/releases/net/neoforged/neoforge/{lv}/neoforge-{lv}-installer.jar")
            };
            srv_status_post(ui, format!("Lade {}-Installer ({lv})...", s.loader));
            download_file_checked(&url, &dir.join("installer.jar")).await?;

            srv_status_post(ui, format!("Installiere {} {lv} (lädt Bibliotheken, kann einige Minuten dauern)...", s.loader));
            let out = tokio::process::Command::new(java)
                .arg("-jar").arg("installer.jar").arg("--installServer")
                .current_dir(dir)
                .output().await
                .map_err(|e| format!("Installer konnte nicht starten: {e}"))?;

            // Ausgabe des Installers in die Server-Konsole übernehmen (letzte 150 Zeilen)
            let mut lines: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().map(String::from).collect();
            lines.extend(String::from_utf8_lossy(&out.stderr).lines().map(String::from));
            let skip = lines.len().saturating_sub(150);
            server_log_push(&s.name, lines.into_iter().skip(skip).collect());
            srv_refresh_console_post(ui, &s.name);

            if !out.status.success() {
                return Err(format!("Installer fehlgeschlagen (Exit {:?}). Details stehen in der Konsole.", out.status.code()));
            }
            let _ = fs::remove_file(dir.join("installer.jar"));
            Ok(Some(lv))
        }
        other => Err(format!("Loader '{other}' wird für Server noch nicht unterstützt.")),
    }
}

// Start-Argumente je Loader. Modernes Forge/NeoForge nutzt eine "@args"-Datei,
// altes Forge (<= 1.16) eine forge-*.jar.
fn server_launch_args(dir: &Path, s: &LocalServer) -> Result<Vec<String>, String> {
    let xmx = format!("-Xmx{}M", s.ram_mb.max(512));
    let mc = &s.minecraft_version;
    let lv = s.loader_version.clone().unwrap_or_default();
    match s.loader.as_str() {
        "fabric" => Ok(vec![xmx, "-jar".into(), "fabric-server-launch.jar".into(), "nogui".into()]),
        "forge" | "neoforge" => {
            let sub = if s.loader == "forge" { format!("libraries/net/minecraftforge/forge/{mc}-{lv}") }
                      else { format!("libraries/net/neoforged/neoforge/{lv}") };
            let file = if cfg!(windows) { "win_args.txt" } else { "unix_args.txt" };
            let rel = format!("{sub}/{file}");
            if dir.join(&rel).is_file() {
                // RAM steht in user_jvm_args.txt, die der Installer mitliefert (wir überschreiben sie bei jedem Start)
                fs::write(dir.join("user_jvm_args.txt"), format!("{xmx}\n")).map_err(|e| format!("user_jvm_args.txt: {e}"))?;
                Ok(vec!["@user_jvm_args.txt".into(), format!("@{rel}"), "nogui".into()])
            } else {
                // Altes Forge: forge-<version>.jar im Server-Ordner
                let jar = fs::read_dir(dir).ok().and_then(|rd| {
                    rd.flatten().map(|e| e.file_name().to_string_lossy().to_string())
                        .find(|n| n.starts_with("forge-") && n.ends_with(".jar") && !n.contains("installer"))
                });
                match jar {
                    Some(j) => Ok(vec![xmx, "-jar".into(), j, "nogui".into()]),
                    None => Err("Keine Forge-Startdatei gefunden. Beim nächsten Start wird neu installiert.".to_string()),
                }
            }
        }
        _ => Ok(vec![xmx, "-jar".into(), "server.jar".into(), "nogui".into()]),
    }
}

// Einstiegspunkt für den Start-Button (verhindert Doppelstart, meldet Fehler in Status + Konsole)
async fn run_local_server(name: String, ui: Weak<AppWindow>) {
    if server_runtime().lock().unwrap().contains_key(&name) {
        srv_status_post(&ui, "Der Server läuft bereits.".to_string());
        return;
    }
    {
        let mut busy = server_busy().lock().unwrap();
        if !busy.insert(name.clone()) { return; } // Installation/Start läuft schon
    }
    let _busy = BusyGuard(name.clone()); // gibt den Eintrag wieder frei, egal wie wir enden

    if let Err(e) = spawn_local_server(&name, &ui).await {
        server_log_push(&name, vec![format!("[Launcher] Fehler: {e}")]);
        srv_refresh_console_post(&ui, &name);
        srv_status_post(&ui, format!("Start fehlgeschlagen: {e}"));
    }
}

async fn spawn_local_server(name: &str, ui: &Weak<AppWindow>) -> Result<(), String> {
    let Some(mut s) = load_local_servers().into_iter().find(|s| s.name == name) else {
        return Err("Server nicht gefunden.".to_string());
    };
    // Ohne ausdrückliche Zustimmung des Nutzers wird NIE eine eula.txt geschrieben
    if !s.eula_accepted {
        return Err("Bitte zuerst die Minecraft-EULA bestätigen (Haken setzen) und speichern.".to_string());
    }
    let dir = server_dir(name);
    fs::create_dir_all(&dir).map_err(|e| format!("Ordner anlegen: {e}"))?;

    // Java passend zur MC-Version (1.17 läuft auch mit 17, und Java 16 ist bei Adoptium EOL)
    let mut jv = java_major_for(&s.minecraft_version);
    if jv == 16 { jv = 17; }
    srv_status_post(ui, format!("Prüfe Java {jv}..."));
    let java_bin = ensure_java_runtime(&launcher_dir(), jv, ui).await?;

    // Erstinstallation
    if !s.installed {
        let lv = install_local_server(&s, &dir, &java_bin, ui).await?;
        let lv2 = lv.clone();
        update_local_server(name, move |x| { x.installed = true; if lv2.is_some() { x.loader_version = lv2; } });
        s.installed = true;
        if lv.is_some() { s.loader_version = lv; }
        let _ = ui.upgrade_in_event_loop(|ui| push_servers_to_ui(&ui));
    }

    // Der Nutzer hat den Haken gesetzt -> eula.txt schreiben
    fs::write(dir.join("eula.txt"), "eula=true\n").map_err(|e| format!("eula.txt: {e}"))?;

    let args = match server_launch_args(&dir, &s) {
        Ok(a) => a,
        Err(e) => { update_local_server(name, |x| x.installed = false); return Err(e); }
    };

    srv_status_post(ui, "Starte Server...".to_string());
    server_log_push(name, vec![format!("[Launcher] Starte: java {}", args.join(" "))]);

    let mut child = tokio::process::Command::new(&java_bin)
        .args(&args)
        .current_dir(&dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Prozess konnte nicht gestartet werden: {e}"))?;

    // Alle Ausgabezeilen (stdout + stderr) laufen in EINEN Kanal
    let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    if let Some(out) = child.stdout.take() {
        let tx = line_tx.clone();
        tokio::spawn(async move {
            let mut l = TokioBufReader::new(out).lines();
            while let Ok(Some(x)) = l.next_line().await { let _ = tx.send(x); }
        });
    }
    if let Some(err) = child.stderr.take() {
        let tx = line_tx.clone();
        tokio::spawn(async move {
            let mut l = TokioBufReader::new(err).lines();
            while let Ok(Some(x)) = l.next_line().await { let _ = tx.send(x); }
        });
    }

    // Befehle aus der UI -> stdin des Servers
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    if let Some(mut stdin) = child.stdin.take() {
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            while let Some(cmd) = cmd_rx.recv().await {
                if stdin.write_all(format!("{cmd}\n").as_bytes()).await.is_err() { break; }
                let _ = stdin.flush().await;
            }
        });
    }

    let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<()>();
    server_runtime().lock().unwrap().insert(name.to_string(), ServerRuntime { cmd_tx, kill_tx: Some(kill_tx) });
    let _ = ui.upgrade_in_event_loop(|ui| push_servers_to_ui(&ui));
    srv_status_post(ui, "Server startet... (bereit, sobald 'Done' in der Konsole steht)".to_string());

    // Pump-Task: sammelt Zeilen und schreibt sie höchstens alle 250 ms in die UI
    // (sonst würde jede einzelne Startup-Zeile das Model komplett neu bauen)
    let (ui_p, name_p) = (ui.clone(), name.to_string());
    tokio::spawn(async move {
        let mut buf: Vec<String> = Vec::new();
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
        let flush = |buf: &mut Vec<String>| {
            if buf.is_empty() { return; }
            let lines = std::mem::take(buf);
            // Startmeldung des Servers erkennen -> Status aktualisieren
            if lines.iter().any(|l| l.contains("Done (") && l.contains("For help")) {
                srv_status_post(&ui_p, "Server ist bereit.".to_string());
            }
            server_log_push(&name_p, lines);
            srv_refresh_console_post(&ui_p, &name_p);
        };
        loop {
            tokio::select! {
                msg = line_rx.recv() => match msg {
                    Some(l) => { buf.push(l); if buf.len() < 200 { continue; } }
                    None => { flush(&mut buf); break; } // alle Sender weg = Prozess beendet
                },
                _ = tick.tick() => {}
            }
            flush(&mut buf);
        }
    });

    // Waiter-Task: wartet aufs Ende (oder killt bei Notbremse) und räumt auf
    let (ui_w, name_w) = (ui.clone(), name.to_string());
    tokio::spawn(async move {
        let code = tokio::select! {
            r = child.wait() => r.ok().and_then(|st| st.code()),
            Ok(()) = kill_rx => { let _ = child.kill().await; None }
        };
        server_runtime().lock().unwrap().remove(&name_w);
        let _ = line_tx.send(format!("[Launcher] Server beendet (Exit-Code: {:?})", code));
        drop(line_tx);
        let _ = ui_w.upgrade_in_event_loop(|ui| push_servers_to_ui(&ui));
        srv_status_post(&ui_w, "Server gestoppt.".to_string());
        notify_desktop("Server beendet", &format!("{name_w}: Exit-Code {:?}", code));
    });

    Ok(())
}

// Server-Pack-Zip (aus export_server_pack) in einen neuen Server-Ordner entpacken und eintragen.
// Der Loader wird erst beim ersten Start installiert (Pack enthält nur mods/ und config/).
fn register_server_from_zip(zip_path: &Path, instance_name: &str, server_name: &str) -> Result<String, String> {
    let cfg = load_instance_config(&launcher_dir().join("instances").join(instance_name))
        .ok_or("instance.json fehlt.".to_string())?;
    let base = clean_new_name(server_name).ok_or("Ungültiger Servername.".to_string())?;
    let name = unique_server_name(&base);
    let dir = server_dir(&name);
    fs::create_dir_all(&dir).map_err(|e| format!("Ordner anlegen: {e}"))?;

    let file = File::open(zip_path).map_err(|e| format!("Zip öffnen: {e}"))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| format!("Zip lesen: {e}"))?;
    for i in 0..zip.len() {
        let mut e = zip.by_index(i).map_err(|e| format!("Zip-Fehler: {e}"))?;
        // enclosed_name verhindert Pfade wie "../../etwas"
        let Some(rel) = e.enclosed_name() else { continue; };
        if rel.to_string_lossy() == "SERVER-INFO.txt" { continue; }
        let out = dir.join(&rel);
        if e.is_dir() {
            fs::create_dir_all(&out).map_err(|e| e.to_string())?;
        } else {
            if let Some(p) = out.parent() { fs::create_dir_all(p).map_err(|e| e.to_string())?; }
            let mut f = File::create(&out).map_err(|e| e.to_string())?;
            std::io::copy(&mut e, &mut f).map_err(|e| e.to_string())?;
        }
    }

    let (kind, id_ver) = split_loader_id(&cfg.loader);
    let mut s = LocalServer::new(name.clone(), cfg.minecraft_version.clone(), kind);
    s.loader_version = cfg.loader_version.clone().or(id_ver);
    s.ram_mb = cfg.ram_mb.max(2048);
    s.source_instance = Some(instance_name.to_string());
    let mut all = load_local_servers();
    all.push(s);
    save_local_servers(&all);
    Ok(name)
}

// =====================================================================================
// ===================== NEU: Fixes (CF-Key, Env/Wrapper beim Start, Server beim Beenden) ===
// =====================================================================================

// ---- CurseForge-API-Key: nicht mehr im Quelltext ----
// Reihenfolge: Umgebungsvariable SRUSM_CF_API_KEY, sonst Datei <launcher>/curseforge_key.txt.
// Wird nur einmal gelesen und dann gecacht. Ohne Key schlägt nur die CurseForge-Suche fehl,
// Modrinth läuft normal weiter.
fn cf_api_key() -> &'static str {
    static KEY: OnceLock<String> = OnceLock::new();
    KEY.get_or_init(|| {
        if let Ok(k) = std::env::var("SRUSM_CF_API_KEY") {
            if !k.trim().is_empty() { return k.trim().to_string(); }
        }
        if let Ok(k) = fs::read_to_string(launcher_dir().join("curseforge_key.txt")) {
            if !k.trim().is_empty() { return k.trim().to_string(); }
        }
        println!("⚠️ Kein CurseForge-API-Key: Datei {} anlegen oder SRUSM_CF_API_KEY setzen.",
            launcher_dir().join("curseforge_key.txt").display());
        String::new()
    }).as_str()
}

// ---- Umgebungsvariablen + Wrapper beim Spielstart ----
// lyceris startet den Prozess selbst und kennt weder Env-Variablen noch Wrapper-Befehle.
// Trick: Ein Kindprozess erbt die Umgebung des Launchers im Moment des Starts. Wir setzen die
// Variablen also direkt vor launch() und stellen danach den alten Zustand wieder her.
// Die Sperre verhindert, dass zwei gleichzeitige Starts sich gegenseitig die Variablen überschreiben.
static LAUNCH_ENV_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
fn launch_env_lock() -> &'static tokio::sync::Mutex<()> {
    LAUNCH_ENV_LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn first_existing(paths: &[&str]) -> Option<String> {
    paths.iter().find(|p| Path::new(p).exists()).map(|p| p.to_string())
}

// Übersetzt die aktivierten Wrapper in Umgebungsvariablen.
//  - prime-run: ist im Kern nur ein Satz Variablen -> 1:1 nachgebaut
//  - gamemode / mangohud: die Wrapper-Skripte setzen LD_PRELOAD auf ihre Bibliothek, das machen wir auch
//  - gamescope: braucht einen echten Befehls-Präfix, geht so NICHT (wird übersprungen)
fn wrapper_env(enabled: &[String]) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    let mut preload: Vec<String> = Vec::new();
    for w in enabled {
        match w.as_str() {
            "prime_run" => {
                env.push(("__NV_PRIME_RENDER_OFFLOAD".into(), "1".into()));
                env.push(("__GLX_VENDOR_LIBRARY_NAME".into(), "nvidia".into()));
                env.push(("__VK_LAYER_NV_optimus".into(), "NVIDIA_only".into()));
            }
            "gamemode" => match first_existing(&[
                "/usr/lib/libgamemodeauto.so.0", "/usr/lib64/libgamemodeauto.so.0",
                "/usr/lib/x86_64-linux-gnu/libgamemodeauto.so.0", "/usr/lib/aarch64-linux-gnu/libgamemodeauto.so.0",
            ]) {
                Some(p) => preload.push(p),
                None => println!("⚠️ libgamemodeauto.so.0 nicht gefunden, GameMode wird übersprungen."),
            },
            "mangohud" => match first_existing(&[
                "/usr/lib/mangohud/libMangoHud.so", "/usr/lib64/mangohud/libMangoHud.so",
                "/usr/lib/x86_64-linux-gnu/mangohud/libMangoHud.so",
            ]) {
                Some(p) => preload.push(p),
                None => println!("⚠️ libMangoHud.so nicht gefunden, MangoHud wird übersprungen."),
            },
            "gamescope" => println!("⚠️ Gamescope kann nicht per Umgebungsvariable eingebunden werden und wird übersprungen."),
            _ => {}
        }
    }
    if !preload.is_empty() {
        let existing = std::env::var("LD_PRELOAD").unwrap_or_default();
        if !existing.is_empty() { preload.push(existing); }
        env.push(("LD_PRELOAD".into(), preload.join(":")));
    }
    env
}

// Setzt Wrapper- + Nutzer-Variablen und merkt sich die alten Werte (None = war nicht gesetzt).
// set_var ist ab Edition 2024 "unsafe", in älteren Editionen nicht: der unsafe-Block passt für beide.
#[allow(unused_unsafe)]
fn apply_launch_env(user: &[(String, String)], wrappers: &[String]) -> Vec<(String, Option<std::ffi::OsString>)> {
    let mut all = wrapper_env(wrappers);
    all.extend(user.iter().cloned()); // Nutzer-Variablen kommen zuletzt und gewinnen
    let mut saved = Vec::new();
    for (k, v) in all {
        if k.is_empty() || k.contains('=') { continue; }
        saved.push((k.clone(), std::env::var_os(&k)));
        unsafe { std::env::set_var(&k, &v); }
    }
    saved
}

#[allow(unused_unsafe)]
fn restore_launch_env(saved: Vec<(String, Option<std::ffi::OsString>)>) {
    for (k, old) in saved.into_iter().rev() { // rückwärts, falls ein Name doppelt gesetzt wurde
        unsafe {
            match old {
                Some(v) => std::env::set_var(&k, v),
                None => std::env::remove_var(&k),
            }
        }
    }
}

// ---- Lokale Server sauber beenden, wenn der Launcher geschlossen wird ----
// Sonst bliebe der Server-Prozess ohne steuerbares stdin zurück (Welt wird nicht gespeichert).
// Wird NACH ui.run() aufgerufen: "stop" an alle, bis zu max_wait warten, danach hart beenden.
fn stop_all_local_servers_blocking(max_wait: std::time::Duration) {
    let names: Vec<String> = server_runtime().lock().unwrap().keys().cloned().collect();
    if names.is_empty() { return; }
    println!("🛑 Beende {} laufende(n) Server sauber...", names.len());
    notify_desktop("SRUSM Launcher", &format!("Stoppe {} Server und speichere die Welten...", names.len()));

    for rt in server_runtime().lock().unwrap().values() {
        let _ = rt.cmd_tx.send("stop".to_string());
    }

    let start = std::time::Instant::now();
    while start.elapsed() < max_wait {
        if server_runtime().lock().unwrap().is_empty() { return; } // der Waiter-Task trägt sie beim Ende aus
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    // Zeitüberschreitung: Notbremse
    println!("⚠️ Server reagieren nicht, beende hart.");
    for rt in server_runtime().lock().unwrap().values_mut() {
        if let Some(k) = rt.kill_tx.take() { let _ = k.send(()); }
    }
    std::thread::sleep(std::time::Duration::from_secs(1));
}

#[hotpath::main]
fn main() -> Result<(), Box<dyn Error>> {

    // NVIDIA-Workaround: wird an den Minecraft-Prozess vererbt.
    // Muss vor dem Start weiterer Threads passieren (set_var ist sonst nicht thread-sicher).
    // Ab Rust-Edition 2024 ist set_var "unsafe", in älteren Editionen ohne den unsafe-Block.
    //unsafe { std::env::set_var("__GL_THREADED_OPTIMIZATIONS", "0"); }

    // Wenn eine System-GLFW existiert, zwingen wir LWJGL dazu, sie zu benutzen.
    // JAVA_TOOL_OPTIONS wird an jede JVM vererbt, also auch an den Minecraft-Prozess.
    // Eine schon gesetzte Variable des Nutzers bleibt erhalten, wir hängen nur an.
    //#[cfg(target_os = "linux")]
    //{
    //    if let Some(glfw) = find_system_glfw() {
    //        let flag = format!("-Dorg.lwjgl.glfw.libname={glfw}");
    //        let value = match std::env::var("JAVA_TOOL_OPTIONS") {
    //            Ok(existing) if !existing.is_empty() => format!("{existing} {flag}"),
    //            _ => flag,
    //        };
    //        unsafe { std::env::set_var("JAVA_TOOL_OPTIONS", value); }
    //    }
    //}

    // Device-Tier VOR dem Runtime-Build setzen, weil die Worker-Anzahl davon abhängt.
    // Wir laden die Settings dazu einmalig aus der settings.json (die wird später
    // im Code nochmal geladen — der zweite Load ist trivial und stört nicht).
    {
        let ldir = dirs::data_local_dir().expect("kein Local Data Dir").join("srusm");
        let pref = load_settings(&ldir).device_tier;
        match parse_tier_choice(&pref) {
            Some(t) => {
                set_device_tier(t);
                println!("🖥️  Device-Tier manuell auf {:?} gesetzt", t);
            }
            None => {
                let detected = detect_device_tier();
                set_device_tier(detected);
                let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
                let mut sys = sysinfo::System::new();
                sys.refresh_memory();
                let gb = sys.total_memory() / (1024 * 1024 * 1024);
                println!("🖥️  Device-Tier automatisch: {:?} ({} Kerne, {} GB RAM)", detected, cores, gb);
            }
        }
    }

    // Favoriten-Cache initial befüllen
    {
        let ldir = dirs::data_local_dir().expect("kein Local Data Dir").join("srusm");
        *favorites_cache().lock().unwrap() = load_favorites(&ldir);
        let g = favorites_cache().lock().unwrap();
        println!("⭐ Favoriten geladen: {} Instanzen, {} Modpacks, {} Mods",
            g.instances.len(), g.modpacks.len(), g.mods.len());
    }

    let workers = tier_parallel_workers(device_tier());
    println!("🖥️  Verwende {} Tokio-Worker", workers);
    let rt = Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()?;

    let handle = rt.handle().clone();
    // Separater Clone für Callbacks, die NACH on_modpack_play registriert werden
    // (das verschiebt "handle" komplett in seine Closure).
    let handle_for_tier = rt.handle().clone();

    // Feature 3b: CLI-Argumente parsen, damit Desktop-Verknüpfungen
    // den Launcher mit --launch "<instanzname>" starten können.
    let cli_args: Vec<String> = std::env::args().collect();
    let auto_launch_instance: Option<String> = cli_args.iter().position(|a| a == "--launch")
        .and_then(|i| cli_args.get(i + 1).cloned());


    let ui = AppWindow::new()?;

    // Tray-Icon starten (nur Linux). Schlägt D-Bus fehl, läuft der Launcher normal weiter.
    #[cfg(target_os = "linux")]
    {
        let tray = ksni::TrayService::new(LauncherTray { ui: ui.as_weak() });
        tray.spawn();
    }

    let ui_handle = ui.as_weak();
    let create_and_run_ui_handle = ui_handle.clone();
    let modpack_play_ui_handle = ui_handle.clone();
    let versions_ui_handle = ui_handle.clone();
    let save_settings_ui_handle = ui_handle.clone();
    let browser_search_ui_handle = ui_handle.clone();
    let install_open_ui_handle = ui_handle.clone();
    let install_confirm_ui_handle = ui_handle.clone();
    let open_details_ui_handle = ui_handle.clone();
    let toggle_mod_ui_handle = ui_handle.clone();
    let remove_mod_ui_handle = ui_handle.clone();
    let save_overrides_ui_handle = ui_handle.clone();
    let add_mod_search_ui_handle = ui_handle.clone();
    let add_mod_install_ui_handle = ui_handle.clone();
    let account_add_offline_ui_handle = ui_handle.clone();
    let account_add_microsoft_ui_handle = ui_handle.clone();
    let account_select_ui_handle = ui_handle.clone();
    let account_remove_ui_handle = ui_handle.clone();
    let compat_toggle_ui_handle = ui_handle.clone();
    let compat_apply_ui_handle = ui_handle.clone();
    let compat_disable_ui_handle = ui_handle.clone();
    let compat_cancel_ui_handle = ui_handle.clone();
    let account_ms_open_ui_handle = ui_handle.clone();
    let account_ms_submit_ui_handle = ui_handle.clone();
    let account_ms_submit_handle = handle.clone();
    let import_mrpack_ui_handle = ui_handle.clone();
    let import_mrpack_handle = handle.clone();
    
    // <-- NEU: Eigene Handles für den CurseForge Import
    let import_cf_zip_ui_handle = ui_handle.clone();
    let import_cf_zip_handle = handle.clone();

    // Extra Clones von handle, weil on_modpack_play weiter unten das
    // ursprüngliche "handle" per "move" komplett in seine Closure reinzieht.
    // Ohne diese Clones wäre "handle" danach in den späteren Callbacks nicht mehr nutzbar.
    let browser_search_handle = handle.clone();
    let install_open_handle = handle.clone();
    let install_confirm_handle = handle.clone();
    let add_mod_search_handle = handle.clone();
    let add_mod_install_handle = handle.clone();
    let compat_apply_handle = handle.clone();
    let open_details_bg_handle = handle.clone(); // Für den Icon-Nachlade-Task
    let _open_details_handle = handle.clone(); // Unterstrich unterdrückt die "unused variable" Warnung

    // Speichert Pack-Name + geladene Versionen für das aktuell offene
    // Install-Popup, damit on_browser_confirm_install (bekommt nur einen
    // Index von Slint) weiß was genau gemeint ist.
    let pending_install: Arc<Mutex<Option<PendingInstall>>> = Arc::new(Mutex::new(None));
    let pending_install_open = pending_install.clone();
    let pending_install_confirm = pending_install.clone();
    let pending_resolution: Arc<Mutex<Option<HashMap<String, ResolvedMod>>>> = Arc::new(Mutex::new(None));
    let pending_resolution_install = pending_resolution.clone();
    let pending_resolution_apply = pending_resolution.clone();
    let pending_resolution_disable = pending_resolution.clone();

    // Laufende Instanzen + die Klone für die beiden Closures, die sie brauchen
    let running: RunningMap = Arc::new(Mutex::new(HashMap::new()));
    let running_play = running.clone();
    let running_stop = running.clone();
    ui.set_running_instances(ModelRc::new(VecModel::from(Vec::<RunningInstance>::new())));

    // Force Stop: Sender aus der Map nehmen und feuern -> der wartende Task killt den Prozess.
    // Das Austragen aus der Liste übernimmt run_and_track danach selbst.
    ui.on_force_stop(move |name| {
        if let Some(tx) = running_stop.lock().unwrap().remove(name.as_str()) {
            let _ = tx.send(());
        }
    });

    // ===================== Favoriten-Toggle (Feature 6) =====================
    // Weak-Handle VOR dem Closure erzeugen und ins Closure verschieben.
    // So wandert nur der Weak-Handle hinein, nicht das ganze `ui`.
    let toggle_fav_ui = ui.as_weak();
    ui.on_toggle_favorite(move |kind, key| {
        let kind = kind.to_string();
        let key = key.to_string();
        let launcher_dir = dirs::data_local_dir().expect("kein Local Data Dir").join("srusm");

        let now_starred = toggle_favorite(&launcher_dir, &kind, &key);

        // println HIER, bevor kind/key in das innere Closure verschoben werden
        println!("⭐ Favorit {} / {} -> {}", kind, key, if now_starred { "AN" } else { "AUS" });

        // Model aktualisieren, damit der Stern sofort umschaltet
        let ui_wk = toggle_fav_ui.clone(); // statt ui.as_weak()
        // Model aktualisieren, damit der Stern sofort umschaltet
        //let ui_wk = ui.as_weak();
        let _ = ui_wk.upgrade_in_event_loop(move |ui| {
            match kind.as_str() {
                "instances" => {
                    // Stern in der Kachel setzen ...
                    with_packs_model(&ui, |vm| {
                        for i in 0..vm.row_count() {
                            if let Some(mut row) = vm.row_data(i) {
                                if row.name.as_str() == key.as_str() {
                                    row.starred = now_starred;
                                    vm.set_row_data(i, row);
                                    break;
                                }
                            }
                        }
                    });
                    // ... und danach Favoriten nach vorne sortieren
                    sort_packs_starred_first(&ui);
                }
                "modpacks" => {
                    set_star_and_sort(ui.get_browser_packs(), &key, now_starred);
                }
                "mods" => {
                    // Beide Mod-Listen: Add-Mod-Popup (Instanz) und Preset-Popup.
                    // (Das Preset-Popup wurde vorher gar nicht aktualisiert.)
                    set_star_and_sort(ui.get_add_mod_results(), &key, now_starred);
                    set_star_and_sort(ui.get_preset_add_mod_results(), &key, now_starred);
                }
                _ => {}
            }
        });

        //println!("⭐ Favorit {} / {} -> {}", kind, key, if now_starred { "AN" } else { "AUS" });
    });

    // ===================== Presets (Neue Features 3) =====================
    let launcher_dir_presets_base = dirs::data_local_dir().expect("kein Local Data Dir").join("srusm");

    let pnew_ui = ui.as_weak();
    let launcher_dir_presets = launcher_dir_presets_base.clone();
    ui.on_preset_new_start(move || {
        let ui = pnew_ui.unwrap();
        let ldir = launcher_dir_presets.clone();
        let presets = load_presets(&ldir);
        let items: Vec<PresetUi> = presets.iter().map(|p| PresetUi {
            id: p.id.clone().into(),
            name: p.name.clone().into(),
            mc_version: p.minecraft_version.clone().into(),
            loader: p.loader.clone().into(),
            mod_count: p.mods.len() as i32,
            suggested_loader_version: p.loader_version.clone().unwrap_or_default().into(),
        }).collect();
        ui.set_presets_list(ModelRc::new(VecModel::from(items)));

        let mut names = Vec::new();
        if let Ok(rd) = fs::read_dir(ldir.join("instances")) {
            for e in rd.flatten() {
                if e.path().is_dir() && e.path().join("instance.json").is_file() {
                    names.push(e.file_name().to_string_lossy().to_string());
                }
            }
        }
        names.sort();
        let names_sh: Vec<SharedString> = names.into_iter().map(SharedString::from).collect();
        ui.set_preset_apply_targets_names(ModelRc::new(VecModel::from(names_sh)));

        ui.set_preset_new_name("".into());
        ui.set_preset_new_mods(ModelRc::new(VecModel::from(Vec::<PresetModUi>::new())));
    });

    let psel_ui = ui.as_weak();
    let launcher_dir_presets = launcher_dir_presets_base.clone();
    ui.on_preset_select(move |id| {
        let ui = psel_ui.unwrap();
        let ldir = launcher_dir_presets.clone();
        let presets = load_presets(&ldir);
        if let Some(p) = presets.iter().find(|p| p.id == id.as_str()) {
            ui.set_preset_selected_id(id.clone());
            ui.set_preset_new_name(p.name.clone().into());
            ui.set_preset_new_mc(p.minecraft_version.clone().into());
            ui.set_preset_new_loader(p.loader.clone().into());
            ui.set_preset_new_loader_version(p.loader_version.clone().unwrap_or_default().into());

            let icon_cache = ldir.join("icon_cache");
            let mods: Vec<PresetModUi> = p.mods.iter().map(|m| {
                let key = m.icon_cache_key.clone().unwrap_or_else(|| m.project_id.clone());
                let path = icon_cache.join(format!("{}.png", key));
                let icon = if path.is_file() { Image::load_from_path(&path).unwrap_or_default() } else { Image::default() };
                PresetModUi {
                    project_id: m.project_id.clone().into(),
                    project_name: m.project_name.clone().into(),
                    icon,
                }
            }).collect();
            ui.set_preset_new_mods(ModelRc::new(VecModel::from(mods)));
        }
    });

    ui.on_preset_new_set_mc(move |_mc| { });
    ui.on_preset_new_set_loader(move |_l| { });

        let psearch_ui = ui.as_weak();
    let psearch_handle = rt.handle().clone();
    ui.on_preset_new_add_mod_search(move |query| {
        let q = query.to_string();
        let hdl = psearch_handle.clone();
        let ui_wk = psearch_ui.clone();

        // Der Callback läuft auf dem UI-Thread: hier dürfen wir die aktuell im
        // Preset-Editor gewählte MC-Version und den Loader direkt lesen.
        let ui_now = psearch_ui.unwrap();
        let extra_facets = build_mod_facets(
            ui_now.get_preset_new_mc().as_str(),
            ui_now.get_preset_new_loader().as_str(),
        );
        let limit = ui_now.get_items_per_page();
        println!("🔍 Preset-Mod-Suche '{}' mit Filtern {:?}", q, extra_facets);

        hdl.spawn(async move {
            // NEU: extra_facets statt &[] -> es kommen nur Mods, die es für Version + Loader gibt
            let Some(hits) = search_projects(&q, "mod", limit, "relevance", &extra_facets).await else { return; };
            let icon_cache = dirs::data_local_dir().unwrap().join("srusm").join("icon_cache");

            // slint::Image ist nicht Send: hier nur Strings + PathBuf sammeln
            let mut rows: Vec<(String, String, String, String, Option<PathBuf>)> = Vec::new();
            for hit in hits {
                let icon_path = cache_icon(&icon_cache, &hit.project_id, &hit.icon_url).await;
                rows.push((hit.project_id, hit.title, hit.description, hit.author, icon_path));
            }

            let _ = ui_wk.upgrade_in_event_loop(move |ui| {
                let mut items: Vec<BrowserPackInfo> = rows
                    .into_iter()
                    .map(|(pid, title, desc, author, icon_path)| {
                        let icon = load_icon(&icon_path); // jetzt auf dem UI-Thread
                        let starred = is_favorite("mods", &pid);
                        BrowserPackInfo {
                            project_id: pid.into(),
                            name: title.into(),
                            summary: desc.into(),
                            author: author.into(),
                            icon,
                            source: "Modrinth".into(),
                            starred,
                        }
                    })
                    .collect();

                items.sort_by_key(|i| !i.starred); // Favoriten nach vorne
                ui.set_preset_add_mod_results(ModelRc::new(VecModel::from(items)));
            });
        });
    });

    let padd_ui = ui.as_weak();
    let launcher_dir_presets = launcher_dir_presets_base.clone();
    ui.on_preset_new_add_mod(move |project_id, project_name| {
        let ui = padd_ui.unwrap();
        let pid = project_id.to_string();
        let pname = project_name.to_string();
        let icon_cache = launcher_dir_presets.join("icon_cache");
        let icon_path = icon_cache.join(format!("{}.png", pid));
        let icon = if icon_path.is_file() { Image::load_from_path(&icon_path).unwrap_or_default() } else { Image::default() };

        let model = ui.get_preset_new_mods();
        let mut items: Vec<PresetModUi> = (0..model.row_count()).filter_map(|i| model.row_data(i)).collect();
        if !items.iter().any(|m| m.project_id.as_str() == pid.as_str()) {
            items.push(PresetModUi {
                project_id: pid.into(),
                project_name: pname.into(),
                icon,
            });
        }
        ui.set_preset_new_mods(ModelRc::new(VecModel::from(items)));
    });

    let prm_ui = ui.as_weak();
    ui.on_preset_new_remove_mod(move |project_id| {
        let ui = prm_ui.unwrap();
        let model = ui.get_preset_new_mods();
        let items: Vec<PresetModUi> = (0..model.row_count())
            .filter_map(|i| model.row_data(i))
            .filter(|m| m.project_id != project_id)
            .collect();
        ui.set_preset_new_mods(ModelRc::new(VecModel::from(items)));
    });

    let psave_ui = ui.as_weak();
    let launcher_dir_presets = launcher_dir_presets_base.clone(); // <-- NEU
    ui.on_preset_new_save(move || {
        let ui = psave_ui.unwrap();
        let name = ui.get_preset_new_name().to_string();
        if name.is_empty() {
            ui.set_presets_status("Bitte einen Namen eingeben.".into());
            return;
        }
        let mc = ui.get_preset_new_mc().to_string();
        let loader = ui.get_preset_new_loader().to_string();
        let loader_version = ui.get_preset_new_loader_version().to_string();
        let selected_id = ui.get_preset_selected_id().to_string();

        let model = ui.get_preset_new_mods();
        let mods: Vec<PresetMod> = (0..model.row_count())
            .filter_map(|i| model.row_data(i))
            .map(|m| PresetMod {
                project_id: m.project_id.to_string(),
                project_name: m.project_name.to_string(),
                icon_cache_key: Some(m.project_id.to_string()),
            })
            .collect();

        let mut presets = load_presets(&launcher_dir_presets);

        let id = if !selected_id.is_empty() && presets.iter().any(|p| p.id == selected_id) {
            if let Some(p) = presets.iter_mut().find(|p| p.id == selected_id) {
                p.name = name.clone();
                p.minecraft_version = mc.clone();
                p.loader = loader.clone();
                p.loader_version = if loader_version.is_empty() { None } else { Some(loader_version.clone()) };
                p.mods = mods.clone();
            }
            selected_id
        } else {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let new_id = format!("preset-{}", now);
            presets.push(Preset {
                id: new_id.clone(),
                name: name.clone(),
                minecraft_version: mc,
                loader,
                loader_version: if loader_version.is_empty() { None } else { Some(loader_version) },
                mods,
            });
            new_id
        };

        save_presets(&launcher_dir_presets, &presets);

        let items: Vec<PresetUi> = presets.iter().map(|p| PresetUi {
            id: p.id.clone().into(),
            name: p.name.clone().into(),
            mc_version: p.minecraft_version.clone().into(),
            loader: p.loader.clone().into(),
            mod_count: p.mods.len() as i32,
            suggested_loader_version: p.loader_version.clone().unwrap_or_default().into(),
        }).collect();
        ui.set_presets_list(ModelRc::new(VecModel::from(items)));
        ui.set_preset_selected_id(id.into());
        ui.set_presets_status("Preset gespeichert.".into());
    });

    let pdel_ui = ui.as_weak();
    let launcher_dir_presets = launcher_dir_presets_base.clone();
    ui.on_preset_delete(move |id| {
        let ui = pdel_ui.unwrap();
        let mut presets = load_presets(&launcher_dir_presets);
        presets.retain(|p| p.id != id.as_str());
        save_presets(&launcher_dir_presets, &presets);
        let items: Vec<PresetUi> = presets.iter().map(|p| PresetUi {
            id: p.id.clone().into(),
            name: p.name.clone().into(),
            mc_version: p.minecraft_version.clone().into(),
            loader: p.loader.clone().into(),
            mod_count: p.mods.len() as i32,
            suggested_loader_version: p.loader_version.clone().unwrap_or_default().into(),
        }).collect();
        ui.set_presets_list(ModelRc::new(VecModel::from(items)));
        ui.set_preset_selected_id("".into());
        ui.set_presets_status("Preset gelöscht.".into());
    });

    let papply_ui = ui.as_weak();
    let papply_handle = rt.handle().clone();
    let launcher_dir_presets = launcher_dir_presets_base.clone();
    ui.on_preset_apply(move |preset_id, instance_name| {
        let ui = papply_ui.unwrap();
        let ldir = launcher_dir_presets.clone();
        let presets = load_presets(&ldir);
        let Some(p) = presets.iter().find(|p| p.id == preset_id.as_str()).cloned() else { return; };

        ui.set_presets_status(format!("Wende Preset auf '{}' an...", instance_name).into());

        let hdl = papply_handle.clone();
        let instance_dir = ldir.join("instances").join(instance_name.as_str());
        let icon_cache = ldir.join("icon_cache");
        let ui2 = papply_ui.clone();

        hdl.spawn(async move {
            let _ = fs::create_dir_all(&instance_dir);
            let mods_dir = instance_dir.join("mods");
            let _ = fs::create_dir_all(&mods_dir);
            let mut installed = 0;
            let total = p.mods.len();

            for (i, m) in p.mods.iter().enumerate() {
                let status = format!("Mod {}/{}: {}", i + 1, total, m.project_name);
                let _ = ui2.upgrade_in_event_loop(move |ui| {
                    ui.set_presets_status(status.into());
                });

                let Ok(file) = File::open(instance_dir.join("instance.json")) else { continue; };
                let Ok(cfg) = serde_json::from_reader::<_, ModpackJsonData>(BufReader::new(file)) else { continue; };

                let Some(versions) = fetch_pack_versions(&m.project_id).await else { continue; };
                let Some(v) = pick_best_matching_version(&versions, &cfg.minecraft_version, &cfg.loader) else { continue; };
                let Some(f) = v.files.iter().find(|f| f.primary).or_else(|| v.files.first()) else { continue; };
                let Ok(bytes) = download_bytes(&f.url).await else { continue; };
                if fs::write(mods_dir.join(&f.filename), &bytes).is_ok() {
                    installed += 1;
                    if let Some(info) = fetch_project_info(&m.project_id).await {
                        if let Some(url) = info.icon_url {
                            let _ = cache_icon(&icon_cache, &m.project_id, &Some(url)).await;
                        }
                    }
                    mods_list_upsert(&instance_dir, &m.project_id, None, &f.filename);
                }
            }

            let msg = format!("Preset angewandt: {} von {} Mods installiert.", installed, total);
            let _ = ui2.upgrade_in_event_loop(move |ui| {
                ui.set_presets_status(msg.into());
            });
        });
    });

        // ===================== Welten + Server (Feature 14) =====================
    let wplay_ui = ui.as_weak();
    ui.on_world_play(move |instance_name, folder_name| {
        let ui = wplay_ui.unwrap();
        let _ = ui;
        let launcher_dir = dirs::data_local_dir().expect("kein Local Data Dir").join("srusm");
        let instance_dir = launcher_dir.join("instances").join(instance_name.as_str());

        // Quick-Play in die Instanz-Misc schreiben, damit der Start es nutzt.
        // Der Launcher startet MC dann mit --quickPlaySingleplayer <folder_name>.
        let mut misc = load_misc(&instance_dir);
        misc.quick_enabled = true;
        misc.quick_server = String::new();
        misc.quick_world = folder_name.to_string();
        save_misc(&instance_dir, &misc);

        println!("🎮 Starte Welt '{}' in '{}'", folder_name, instance_name);
        ui.invoke_modpack_play(instance_name);
    });

    let sjoin_ui = ui.as_weak();
    ui.on_server_join(move |instance_name, server_ip| {
        let ui = sjoin_ui.unwrap();
        let _ = ui;
        let launcher_dir = dirs::data_local_dir().expect("kein Local Data Dir").join("srusm");
        let instance_dir = launcher_dir.join("instances").join(instance_name.as_str());

        let mut misc = load_misc(&instance_dir);
        misc.quick_enabled = true;
        misc.quick_server = server_ip.to_string();
        misc.quick_world = String::new();
        save_misc(&instance_dir, &misc);

        println!("🌐 Verbinde zu '{}' in '{}'", server_ip, instance_name);
        ui.invoke_modpack_play(instance_name);
    });

        // ===================== Misc-Werkzeuge: Server-Pack + verwaiste Libraries =====================
    let h = ui.as_weak();
    let hdl = rt.handle().clone();
        // Server-Pack erstellen. NEU: zusätzlich optional als lokalen Server eintragen (Popup in der UI fragt vorher).
    ui.on_server_pack_export(move |name, add_server, server_name| {
        let ui = h.unwrap();
        let target_dir = dirs::download_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from("."));
        let stem = format!("{}-server", sanitize_dir_name(name.as_str()));
        let mut out = target_dir.join(format!("{stem}.zip"));
        let mut n = 2;
        while out.exists() { out = target_dir.join(format!("{stem} ({n}).zip")); n += 1; }
        ui.set_misc_tools_status("Erstelle Server-Pack (Mods werden bei Modrinth geprüft)...".into());

        let (wh, name, server_name) = (h.clone(), name.to_string(), server_name.to_string());
        hdl.spawn(async move {
            let mut msg = match export_server_pack(&name, &out).await {
                Ok(m) => m,
                Err(e) => {
                    let _ = fs::remove_file(&out);
                    let m = format!("Server-Pack fehlgeschlagen: {e}");
                    let _ = wh.upgrade_in_event_loop(move |ui| ui.set_misc_tools_status(m.into()));
                    return;
                }
            };

            // Optional: das fertige Zip in einen neuen Server-Ordner entpacken und in die Liste eintragen
            if add_server {
                let (zp, inst, sn) = (out.clone(), name.clone(), server_name.clone());
                let reg = tokio::task::spawn_blocking(move || register_server_from_zip(&zp, &inst, &sn))
                    .await.unwrap_or_else(|e| Err(format!("Interner Fehler: {e}")));
                match reg {
                    Ok(n) => msg.push_str(&format!(" Als Server '{n}' hinzugefügt (Tab 'Server').")),
                    Err(e) => msg.push_str(&format!(" Server konnte nicht hinzugefügt werden: {e}")),
                }
            }

            let _ = wh.upgrade_in_event_loop(move |ui| {
                ui.set_misc_tools_status(msg.into());
                push_servers_to_ui(&ui); // Server-Tab aktuell halten
            });
        });
    });

    let h = ui.as_weak();
    let hdl = rt.handle().clone();
    ui.on_orphans_scan(move |name| {
        let ui = h.unwrap();
        ui.set_misc_tools_status("Prüfe Abhängigkeiten bei Modrinth...".into());
        let (wh, name) = (h.clone(), name.to_string());
        hdl.spawn(async move {
            let dir = launcher_dir().join("instances").join(&name);
            let text = match find_orphans(&dir).await {
                Ok((orphans, unknown)) => {
                    save_orphans(&dir, &orphans);
                    invalidate_instance_cache(&name);
                    let mut t = if orphans.is_empty() {
                        "Keine verwaisten Libraries gefunden.".to_string()
                    } else {
                        format!("{} mögliche verwaiste Libraries (lila in der Mod-Liste): {}", orphans.len(), orphans.join(", "))
                    };
                    if unknown > 0 {
                        t.push_str(&format!(" Achtung: {unknown} Mods kennt Modrinth nicht, die könnten Libraries trotzdem brauchen."));
                    }
                    t
                }
                Err(e) => format!("Prüfung fehlgeschlagen: {e}"),
            };
            let _ = wh.upgrade_in_event_loop(move |ui| {
                ui.set_misc_tools_status(text.into());
                // Mod-Liste neu aufbauen, falls diese Instanz gerade offen ist
                if ui.get_detail_instance_name().as_str() == name {
                    let d = launcher_dir().join("instances").join(&name);
                    ui.set_detail_mod_files(ModelRc::new(VecModel::from(list_mod_files_cached(&name, &d))));
                }
            });
        });
    });

        let refresh_servers_ui = ui.as_weak();
    let refresh_servers_handle = rt.handle().clone();
    ui.on_refresh_servers(move |instance_name| {
        let ui = refresh_servers_ui.unwrap();
        let launcher_dir = dirs::data_local_dir().expect("kein Local Data Dir").join("srusm");
        let instance_dir = launcher_dir.join("instances").join(instance_name.as_str());
        let servers = read_servers(&instance_dir);

        if servers.is_empty() {
            ui.set_detail_servers_status("Keine Server".into());
            return;
        }
        ui.set_detail_servers_status(format!("Prüfe {} Server...", servers.len()).into());

        // Status sofort auf "checking" setzen (wir sind hier schon auf dem UI-Thread,
        // also direkt statt über den Event-Loop, dann gibt es kein Timing-Problem
        // wenn der Callback direkt nach set_detail_servers aufgerufen wird).
        {
            let model = ui.get_detail_servers();
            if let Some(vm) = model.as_any().downcast_ref::<VecModel<ServerInfoUi>>() {
                for i in 0..vm.row_count() {
                    if let Some(mut row) = vm.row_data(i) {
                        row.status = "checking".into();
                        vm.set_row_data(i, row);
                    }
                }
            }
        }

        let ips: Vec<String> = servers.iter().map(|s| s.ip.clone()).collect();
        let iname = instance_name.to_string();
        let ui_set = ui.as_weak();

        refresh_servers_handle.spawn(async move {
            // Alle Server GLEICHZEITIG pingen (jeder hat 5 s Timeout)
            let mut tasks = Vec::with_capacity(ips.len());
            for ip in ips {
                tasks.push(tokio::spawn(async move {
                    let r = ping_server(&ip).await;
                    (ip, r)
                }));
            }
            let mut results: Vec<(String, Result<(i32, i32, String), String>)> = Vec::new();
            for t in tasks {
                if let Ok(x) = t.await { results.push(x); }
            }

            let _ = ui_set.upgrade_in_event_loop(move |ui| {
                // Hat der Nutzer inzwischen eine andere Instanz geöffnet? Dann nichts überschreiben.
                if ui.get_detail_instance_name().as_str() != iname { return; }

                let model = ui.get_detail_servers();
                if let Some(vm) = model.as_any().downcast_ref::<VecModel<ServerInfoUi>>() {
                    for i in 0..vm.row_count() {
                        if let Some(mut row) = vm.row_data(i) {
                            let ip = row.ip.to_string();
                            if let Some((_ip, res)) = results.iter().find(|(x, _)| x == &ip) {
                                match res {
                                    Ok((online, max, motd)) => {
                                        row.status = "online".into();
                                        row.players_text = format!("{}/{}", online, max).into();
                                        row.motd = motd.clone().into();
                                    }
                                    Err(e) => {
                                        println!("⚠️ Ping {} fehlgeschlagen: {e}", ip);
                                        row.status = "offline".into();
                                        row.players_text = "".into();
                                    }
                                }
                                vm.set_row_data(i, row);
                            }
                        }
                    }
                }
                let online = results.iter().filter(|(_, r)| r.is_ok()).count();
                ui.set_detail_servers_status(format!("{}/{} online", online, results.len()).into());
            });
        });
    });

    // ===================== Desktop-Verknüpfung (Feature 3b) =====================
    let create_shortcut_ui = ui.as_weak();
    ui.on_create_shortcut(move |instance_name| {
        let ui = create_shortcut_ui.unwrap();
        let name = instance_name.to_string();

        match create_desktop_shortcut(&name) {
            Ok(path) => {
                println!("✅ Desktop-Verknüpfung erstellt: {}", path.display());
                ui.set_manage_status(format!("Verknüpfung erstellt: {}", path.display()).into());
            }
            Err(e) => {
                println!("❌ Verknüpfung fehlgeschlagen: {e}");
                ui.set_manage_status(format!("Verknüpfung fehlgeschlagen: {e}").into());
            }
        }
    });

    // ===================== Live Console =====================
    // Öffnet für eine laufende Instanz ein Popup und tailt logs/latest.log.
    // Kein read_to_string der ganzen Datei — wir starten bei "Datei-Ende minus
    // 20 KB" und hängen danach nur noch neue Bytes an. Ringpuffer von 500 Zeilen
    // im UI-Model hält den RAM konstant, egal wie lang die Session läuft.
    let console_open_handle = rt.handle().clone();
    let console_close_handle = rt.handle().clone();
    let console_open_ui = ui.as_weak();
    let console_close_ui = ui.as_weak();

    ui.on_console_open(move |name| {
        let ui = console_open_ui.unwrap();
        let name_str = name.to_string();

        // UI zurücksetzen
        ui.set_console_instance_name(name.clone());
        ui.set_console_lines(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
        ui.set_console_auto_scroll(true);
        ui.set_console_popup_visible(true);

        // Alten Stream stoppen (falls noch einer läuft)
        if let Some(prev) = console_stream_state().lock().unwrap().take() {
            prev.stop_flag.store(true, AtomicOrdering::Relaxed);
        }

        // Neuen Stream starten
        let stop_flag = Arc::new(AtomicBool::new(false));
        *console_stream_state().lock().unwrap() = Some(ConsoleStreamState {
            stop_flag: stop_flag.clone(),
        });

        let log_path = launcher_dir()
            .join("instances")
            .join(&name_str)
            .join("logs")
            .join("latest.log");

        let ui_for_task = console_open_ui.clone();

        console_open_handle.spawn(async move {
            const INITIAL_TAIL_BYTES: u64 = 20_000; // letzte ~150 Zeilen Kontext
            let max_lines: usize = tier_log_lines(device_tier());
            const POLL_INTERVAL_MS: u64 = 300;

            // Startpunkt: Datei-Ende minus 20 KB (falls Datei größer)
            let total_size = fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);
            let mut last_size: u64 = if total_size > INITIAL_TAIL_BYTES {
                total_size - INITIAL_TAIL_BYTES
            } else {
                0
            };

            // Erste (möglicherweise angeschnittene) Zeile verwerfen
            if last_size > 0 {
                if let Ok(file) = File::open(&log_path) {
                    let mut reader = BufReader::new(file);
                    if reader.seek(SeekFrom::Start(last_size)).is_ok() {
                        let mut discard = Vec::new();
                        let _ = reader.read_until(b'\n', &mut discard);
                        last_size += discard.len() as u64;
                    }
                }
            }

            loop {
                if stop_flag.load(AtomicOrdering::Relaxed) {
                    break;
                }

                let Ok(meta) = fs::metadata(&log_path) else {
                    tokio::time::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
                    continue;
                };
                let size = meta.len();

                if size > last_size {
                    let mut new_lines: Vec<String> = Vec::new();
                    if let Ok(file) = File::open(&log_path) {
                        let mut reader = BufReader::new(file);
                        if reader.seek(SeekFrom::Start(last_size)).is_ok() {
                            let to_read = size - last_size;
                            let mut read_bytes: u64 = 0;
                            while read_bytes < to_read {
                                let mut buf: Vec<u8> = Vec::new();
                                match reader.read_until(b'\n', &mut buf) {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        read_bytes += n as u64;
                                        let text = String::from_utf8_lossy(&buf);
                                        let trimmed = text.trim_end_matches('\n').trim_end_matches('\r');
                                        if !trimmed.is_empty() {
                                            new_lines.push(trimmed.to_string());
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                        }
                    }
                    last_size = size;

                    if !new_lines.is_empty() {
                        let lines_sh: Vec<SharedString> =
                            new_lines.into_iter().map(SharedString::from).collect();

                        let _ = ui_for_task.upgrade_in_event_loop(move |ui| {
                            let model = ui.get_console_lines();
                            let mut existing: Vec<SharedString> = (0..model.row_count())
                                .filter_map(|i| model.row_data(i))
                                .collect();
                            existing.extend(lines_sh);
                            if existing.len() > max_lines {
                                let drop = existing.len() - max_lines;
                                existing.drain(0..drop);
                            }
                            ui.set_console_lines(ModelRc::new(VecModel::from(existing)));
                            // Slint springt ans Ende, wenn Auto-Scroll an ist
                            ui.set_console_scroll_tick(ui.get_console_scroll_tick().wrapping_add(1));
                        });
                    }
                }

                tokio::time::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
            }
        });
    });

    // Live Console schließen: stoppt den Tail-Task und leert das Model sofort,
    // damit der RAM wieder freigegeben wird.
    ui.on_console_close(move || {
        let _ = &console_close_handle; // Handle nur am Leben halten
        let ui = console_close_ui.unwrap();

        if let Some(stream) = console_stream_state().lock().unwrap().take() {
            stream.stop_flag.store(true, AtomicOrdering::Relaxed);
        }

        ui.set_console_popup_visible(false);
        ui.set_console_lines(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
        ui.set_console_instance_name("".into());
        ui.set_console_auto_scroll(true);
    });
    // ===================== / Live Console =====================

    // Program Local Dir Setup
    let launcherDir = dirs::data_local_dir()
        .expect("kein Local Data Dir")
        .join("srusm");
    fs::create_dir_all(&launcherDir);

    // Setup Folder Structur
    fs::create_dir_all(&launcherDir.join("shared"));
    fs::create_dir_all(&launcherDir.join("shared/runtime"));
    fs::create_dir_all(&launcherDir.join("instances"));
    fs::create_dir_all(&launcherDir.join("icon_cache"));

    // Testing ModpackTiles
    let modpacktilemodel = Rc::new(VecModel::from(vec![
        //ModpackInfo { name: "Pack A".into()},
        //ModpackInfo { name: "Pack B".into()},
    ]));
    ui.set_packs(ModelRc::from(modpacktilemodel.clone()));
    // model.push(ModpackInfo { name: "Pack B".into(), image: ??? });

    // Modpack Browser startet mit leerem Model, wird über on_browser_search befüllt.
    // Wichtig: Wir halten hier keine Rc<VecModel<...>> fest, die später in
    // handle.spawn() reinwandern müsste (Rc ist nicht Send) – siehe on_browser_search.
    ui.set_browser_packs(ModelRc::new(VecModel::from(Vec::<BrowserPackInfo>::new())));

    // Detail-Ansicht + Add-Mod-Popup starten ebenfalls mit leeren Models.
    ui.set_detail_mod_files(ModelRc::new(VecModel::from(Vec::<ModFileInfo>::new())));
    ui.set_add_mod_results(ModelRc::new(VecModel::from(Vec::<BrowserPackInfo>::new())));
    ui.set_compat_actions(ModelRc::new(VecModel::from(Vec::<CompatibilityActionUi>::new())));
    ui.set_compat_conflicts(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));

    //ModelRc::new(VecModel::from(Vec::<SharedString>::new())));

    // Settings laden (falls schon vorhanden), sonst Defaults nehmen und direkt
    // eine settings.json anlegen, damit's beim nächsten Start schon da ist.
    let settings_path = launcherDir.join("settings.json");
    let settings: LauncherSettings = if settings_path.is_file() {
        let file = File::open(&settings_path).expect("Error during file Reading of settings.json");
        let reader = BufReader::new(file);
        serde_json::from_reader(reader).expect("Error during extracting data out of settings.json")
    } else {
        let defaults = LauncherSettings::default();
        let json = serde_json::to_string_pretty(&defaults).expect("Error Making Settings json");
        fs::write(&settings_path, json).expect("Error Writing Settings File");
        defaults
    };

    // Geladene (oder default) Settings in die UI-Properties schreiben, damit
    // der Settings-Tab beim Öffnen die richtigen Werte anzeigt.
    ui.set_default_ram_mb(settings.default_ram_mb);
    ui.set_java_path(if settings.java_path.is_empty() { "Launcher Standard".into() } else { settings.java_path.clone().into() });
    // NEU: Die Java-Liste auch im Settings-Tab füllen (vorher wurde sie nur beim Öffnen einer Instanz gesetzt)
    ui.set_available_java_paths(ModelRc::new(VecModel::from(
        get_available_java_paths().into_iter().map(SharedString::from).collect::<Vec<_>>(),
    )));
    ui.set_default_username(settings.default_username.clone().into());
    ui.set_close_after_launch(settings.close_after_launch);
    ui.set_open_on_startup(settings.open_on_startup);
    ui.set_show_snapshots(settings.show_snapshots);
    ui.set_items_per_page(settings.items_per_page);
    ui.set_settings_device_tier(settings.device_tier.clone().into());
    //ui.set_debug_ui(settings.debug_ui);

    // Accounts laden und in die UI schreiben.
    let accounts_data = load_accounts(&launcherDir);
    ui.set_accounts(ModelRc::new(VecModel::from(accounts_to_ui_model(&accounts_data))));

    handle.spawn(async move { // Get all avaliable Minecraft Versions
        let manifest: VersionManifest = fetch("https://piston-meta.mojang.com/mc/game/version_manifest_v2.json".to_string(), None).await.expect("WELP HEELLLP MANIFEST IS BROKEN????!!!");
        let versions: Vec<String> = manifest
            .versions
            .into_iter()
            .filter(|v| matches!(v.r#type, Type::Release))
            .map(|v| v.id)
            .collect();

        let _ = versions_ui_handle.upgrade_in_event_loop(move |ui| {
            let items: Vec<SharedString> =
                versions.into_iter().map(SharedString::from).collect();
            ui.set_mcVersions(ModelRc::new(VecModel::from(items)));
        });
    });

    // Search for Installed Instances (async + parallel, damit der UI-Start nicht blockiert)
    {
        let scan_ui = ui_handle.clone();
        let scan_dir = launcherDir.clone();
        let scan_handle = handle.clone();
        scan_handle.spawn(async move {
            // Verzeichnisse einlesen (blocking, kurz)
            let dirs: Vec<PathBuf> = match tokio::task::spawn_blocking({
                let base = scan_dir.clone();
                move || -> Vec<PathBuf> {
                    let mut out = Vec::new();
                    let Ok(rd) = fs::read_dir(base.join("instances")) else { return out; };
                    for entry in rd.flatten() {
                        let p = entry.path();
                        if p.is_dir() && p.join("instance.json").is_file() {
                            out.push(p);
                        }
                    }
                    out
                }
            }).await { Ok(v) => v, Err(_) => Vec::new() };

            // Parallel parsen, UI-Thread baut die Kacheln (weil Image nicht Send)
            let limit = tier_download_concurrency(device_tier());
            let sem = Arc::new(tokio::sync::Semaphore::new(limit));
            let mut tasks = Vec::with_capacity(dirs.len());
            for dir in dirs {
                let sem = sem.clone();
                tasks.push(tokio::spawn(async move {
                    let _permit = sem.acquire().await.ok()?;
                    let cfg: Option<ModpackJsonData> = tokio::task::spawn_blocking(move || -> Option<ModpackJsonData> {
                        let f = File::open(dir.join("instance.json")).ok()?;
                        serde_json::from_reader(BufReader::new(f)).ok()
                    }).await.ok()?;
                    cfg
                }));
            }
            for t in tasks {
                if let Ok(Some(cfg)) = t.await {
                //    let _ = scan_ui.upgrade_in_event_loop(move |ui| {
                //        with_packs_model(&ui, |vm| vm.push(tile_from_config(&cfg)));
                //    });
                    let _ = scan_ui.upgrade_in_event_loop(move |ui| {
                            with_packs_model(&ui, |vm| vm.push(tile_from_config(&cfg)));
                            sort_packs_starred_first(&ui); // NEU: Favoriten nach vorne
                    });
                }

                // Sortieren EINMAL nach dem letzten Push, nicht nach jeder Kachel.
                // Der Event-Loop arbeitet in Reihenfolge, also läuft das nach allen Pushes.
                let _ = scan_ui.upgrade_in_event_loop(move |ui| {
                    sort_packs_starred_first(&ui); // Favoriten nach vorne
                });
            }

            // NEU: Icons, deren Datei fehlt, im Hintergrund neu laden
            repair_missing_icons(scan_ui.clone()).await;
        });
    }

    ui.on_create_and_run(move |name, version, modloader| {
        let ui = create_and_run_ui_handle.unwrap();
        //let handle = handle.clone();
        //let ui_handle = ui_handle.clone();

        //if modloader == "NeoForge" {modloader = "neoforge".into()}

        let data = ModpackJsonData {
            name: name.to_string(),
            minecraft_version: version.to_string(),
            loader: modloader.to_string().to_lowercase(),
            ram_mb: 2048,
            icon_path: None, // manuell angelegte Instanz hat keine Modrinth-Herkunft, also kein Icon
            loader_version: None, // manuell angelegt: immer die neueste stabile Version
            ..Default::default()
        };

        let launcherDir = dirs::data_local_dir()
            .expect("kein Local Data Dir")
            .join("srusm");

        let dir = &launcherDir.join("instances/").join(&data.name);
        fs::create_dir_all(dir);

        let json = serde_json::to_string_pretty(&data).expect("Errer Making json");
        fs::write(dir.join("instance.json"), json).expect("Error Writing File");

        ui.set_popup_new_instance(false);

        ui.invoke_modpack_play(data.name.clone().into());

        modpacktilemodel.push(ModpackInfo {
            name: data.name.clone().into(),
            loader: data.loader.into(),
            minecraft_version: data.minecraft_version.into(),
            ram_mb: data.ram_mb,
            icon: Image::default(),
            starred: is_favorite("instances", &data.name),
        });
    });

        // Play Instance
    ui.on_modpack_play(move |name| {
        let _ui = modpack_play_ui_handle.unwrap();
        let handle = handle.clone();
        let ui_handle = ui_handle.clone();
        let launcherDir = dirs::data_local_dir()
            .expect("kein Local Data Dir")
            .join("srusm");

        // ===== NEU: Start-Popup sofort anzeigen (läuft hier auf dem UI-Thread) =====
        _ui.set_launch_instance(name.clone());
        _ui.set_launch_status("Bereite Start vor...".into());
        _ui.set_launch_progress(0.03);
        _ui.set_launch_popup_visible(true);

        println!("");
        println!("═══════════════════════════════════════════════════════");
        println!("🎮 START: {}", name);
        println!("═══════════════════════════════════════════════════════");

        println!("📂 [1/7] Lese Instanz-Konfiguration...");
        let instance_dir = launcherDir.join("instances/").join(&name);
        let file = File::open(instance_dir.join("instance.json")).expect("Error during file Reading of instance.json");
        let reader = BufReader::new(file);
        let config: ModpackJsonData = serde_json::from_reader(reader).expect("Error during extracting data out of instance.json");

        println!("   ├─ Minecraft Version: {}", config.minecraft_version);
        println!("   ├─ Loader: {}", config.loader);
        println!("   └─ Icon: {}", if config.icon_path.is_some() { "vorhanden" } else { "fehlt" });

        println!("⚙️  [2/7] Lade Instanz-Overrides...");
        let overrides = load_instance_overrides(&instance_dir);
        if overrides.enabled {
            println!("   └─ Overrides aktiviert (RAM: {} MB, Account: {})", overrides.ram_mb, overrides.account_id);
        } else {
            println!("   └─ Overrides deaktiviert, nutze globale Settings");
        }

        println!("💾 [3/7] Berechne effektiven RAM...");
        let settings = load_settings(&launcherDir);
        let effective_ram_mb = if overrides.enabled { overrides.ram_mb } else { settings.default_ram_mb };
        println!("   └─ Effektiver RAM: {} MB", effective_ram_mb);

        println!("👤 [4/7] Bestimme Authentifizierung...");
        let accounts = load_accounts(&launcherDir);

        // Account auswählen: Override hat Vorrang, sonst globaler Account
        let selected_account: Option<AccountEntry> = if overrides.enabled && overrides.account_id != "Launcher Standard" {
            accounts.accounts.iter().find(|a| {
                let kind_str = if a.kind == "microsoft" { " (Microsoft)" } else { " (Offline)" };
                format!("{}{}", a.username, kind_str) == overrides.account_id
            }).cloned()
        } else {
            accounts.selected_id.as_ref()
                .and_then(|id| accounts.accounts.iter().find(|a| &a.id == id))
                .cloned()
        };

        let auth_type = match &selected_account {
            Some(acc) if acc.kind == "microsoft" => "Microsoft",
            Some(_) => "Offline",
            None => "Offline (Fallback: TestUser)",
        };
        println!("   └─ Auth-Typ: {}", auth_type);

        let _launcher_dir = launcherDir.clone();
        let ui_handle = ui_handle.clone();
        let handle = handle.clone();
        let name = name.to_string();

        let running = running_play.clone(); // wandert in den async-Block
        let global_java = settings.java_path.clone(); // NEU: globale Java-Wahl aus den Settings (Zahl oder "Launcher Standard")

        handle.spawn(async move {
            // NEU: schließt das Popup automatisch, egal wie dieser Task endet
            let _guard = LaunchGuard(ui_handle.clone());

            // Start-Zeit-Messung: Zeitpunkt des Klicks merken
            let click_instant = std::time::Instant::now();

            println!("🚀 [5/7] Starte Installation/Setup...");

            // Spielername für die Achievement-Auswertung nach dem Spiel
            let player_name: String = selected_account.as_ref()
                .map(|a| a.username.clone())
                .unwrap_or_else(|| "TestUser".to_string());

            // ---- Auth ----
            launch_ui(&ui_handle, "Melde Account an...", 0.08);
            let auth = match selected_account {
                Some(acc) if acc.kind == "microsoft" => match microsoft_auth_for(&launcherDir, acc).await {
                    Ok(auth) => auth,
                    Err(e) => {
                        launch_error(&ui_handle, format!("Microsoft-Anmeldung fehlgeschlagen: {e}")).await;
                        return;
                    }
                },
                Some(acc) => lyceris::AuthMethod::Offline { username: acc.username, uuid: None },
                None => lyceris::AuthMethod::Offline { username: "TestUser".to_string(), uuid: None },
            };

            // --- Misc-Einstellungen dieser Instanz (misc.json) ---
            let misc = load_misc(&instance_dir);
            let java_major = java_major_for(&config.minecraft_version);
            let extra_jvm_args: Vec<String> = collect_jvm_args(&misc, java_major);
            let mut extra_game_args: Vec<String> = collect_game_args(&misc, &config.minecraft_version);
            // NEU: Fenstergröße/Vollbild aus den Instanz-Overrides als normale Minecraft-Startargumente
            if overrides.enabled {
                if overrides.fullscreen {
                    extra_game_args.push("--fullscreen".to_string());
                } else {
                    extra_game_args.push("--width".to_string());
                    extra_game_args.push(overrides.window_width.to_string());
                    extra_game_args.push("--height".to_string());
                    extra_game_args.push(overrides.window_height.to_string());
                }
            }

            // Quick Play gilt nur für diesen einen Start
            if misc.quick_enabled {
                let mut reset = misc.clone();
                reset.quick_enabled = false;
                save_misc(&instance_dir, &reset);
            }

            let extra_env: Vec<(String, String)> = misc.env_vars.iter().map(|v| (v.name.clone(), v.value.clone())).collect();
            let active_wrappers: Vec<String> = misc.wrappers.clone();

            println!("🧩 Misc: JVM-Args {:?}", extra_jvm_args);
            println!("🧩 Misc: Spiel-Args {:?}", extra_game_args);
            println!("🧩 Misc: Env {:?}, Wrapper {:?}", extra_env, active_wrappers);

            // TODO(lyceris) 3: extra_env und active_wrappers sind weiterhin nicht verdrahtet (siehe alter Kommentar).

            // ---- Java ----
            let java_path_choice = if overrides.enabled {
                overrides.java_path.clone()
            } else {
                global_java.clone() // NEU: vorher war das fest "Launcher Standard"
            };
            let selected_java_version: Option<u32> = java_path_choice.parse::<u32>().ok();

            if let Some(jv) = selected_java_version {
                launch_ui(&ui_handle, format!("Lade Java {}...", jv), 0.18);
                match ensure_java_runtime(&launcherDir, jv, &ui_handle).await {
                    Ok(_) => println!("☕ Java {} heruntergeladen", jv),
                    Err(e) => {
                        println!("⚠️ Java {} Download fehlgeschlagen: {e}", jv);
                        println!("   Fallback auf Launcher-Standard.");
                    }
                }
                // ensure_java_runtime schreibt über report_progress ins Install-Popup, das ist hier egal.
            }

            let builder = ConfigBuilder::new(
                &launcherDir.join("shared"),
                config.minecraft_version.clone(),
                auth,
            ).runtime_dir(launcherDir.join("shared/runtime"))
            .memory(Memory::Megabyte(effective_ram_mb as u64))
            .custom_java_args(extra_jvm_args.clone())
            .custom_args(extra_game_args.clone())
            .profile(Profile::new(name.to_string(), launcherDir.clone().join("instances")));

            let emitter = Emitter::default();

            // ===== Console-Zeilen auswerten =====
            //  1. Fenster erkannt -> Popup schließen
            //  2. Start-Zeit-Messung (wie vorher)
            let window_seen = Arc::new(AtomicBool::new(false));
            let startup_done = Arc::new(AtomicBool::new(false));
            let launch_instant: Arc<Mutex<Option<std::time::Instant>>> = Arc::new(Mutex::new(None));
            {
                let (done, li) = (startup_done.clone(), launch_instant.clone());
                let (seen, ui_c) = (window_seen.clone(), ui_handle.clone());
                let (idir, preset, mods_n, ram) = (
                    instance_dir.clone(), misc.jvm_preset.clone(), count_mods(&instance_dir), effective_ram_mb,
                );
                emitter.on(Event::Console, move |line: String| {
                    println!("[MC] {line}");

                    // NEU: Minecraft-Fenster aufgetaucht? -> Popup weg (nur beim ersten Treffer)
                    if !seen.load(AtomicOrdering::Relaxed) && WINDOW_MARKERS.iter().any(|m| line.contains(m)) {
                        if !seen.swap(true, AtomicOrdering::Relaxed) {
                            launch_ui(&ui_c, "Minecraft ist bereit!", 1.0);
                            launch_ui_hide(&ui_c);
                        }
                    }

                    if !done.load(AtomicOrdering::Relaxed) && line.contains(STARTUP_MARKER) {
                        if !done.swap(true, AtomicOrdering::Relaxed) {
                            let game = li.lock().unwrap().map(|t| t.elapsed().as_secs_f32()).unwrap_or(0.0);
                            let total = click_instant.elapsed().as_secs_f32();
                            println!("⏱️ Start bis Hauptmenü: {game:.1}s (Spiel) / {total:.1}s (ab Klick)");
                            save_startup(&idir, StartupRecord {
                                ts: unix_now(), preset: preset.clone(), game_secs: game,
                                total_secs: total, mods: mods_n, ram_mb: ram,
                            });
                        }
                    }
                }).await;
            }

            if config.loader == "none" {
                println!("📦 [6/7] Installiere Vanilla Minecraft...");
                launch_ui(&ui_handle, format!("Lade Minecraft {}...", config.minecraft_version), 0.30);
                let cfg = builder.build();

                // Download-Fortschritt in die Loading-Bar schreiben
                {
                    let ui_dl = ui_handle.clone();
                    emitter.on(
                        Event::MultipleDownloadProgress,
                        move |(_path, current, total, _kind): (String, u64, u64, String)| {
                            if total == 0 { return; }
                            let frac = current as f32 / total as f32;
                            // Download füllt den Balken zwischen 30 % und 80 %
                            launch_ui(&ui_dl, format!("Lade Dateien... {}/{}", current, total), 0.30 + 0.50 * frac);
                        },
                    ).await;
                }

                if let Err(e) = install(&cfg, Some(&emitter)).await {
                    launch_error(&ui_handle, format!("Installation fehlgeschlagen: {:?}", e)).await;
                    return;
                }

                launch_ui(&ui_handle, "Richte Java-Laufzeit ein...", 0.85);
                if let Some(jv) = selected_java_version {
                    match ensure_java_runtime_for_instance(&launcherDir, &config.minecraft_version, jv) {
                        Ok(()) => println!("☕ Runtime auf Java {} umgestellt", jv),
                        Err(e) => println!("⚠️ Runtime konnte nicht umgestellt werden: {e}"),
                    }
                }

                println!("▶️  [7/7] Starte Minecraft...");
                launch_ui(&ui_handle, "Starte Minecraft...", 0.92);
                *launch_instant.lock().unwrap() = Some(std::time::Instant::now());
                match launch(&cfg, Some(&emitter)).await {

                    Ok(child) => {
                        launch_ui(&ui_handle, "Warte auf das Minecraft-Fenster...", 0.96);

                        // Sicherheitsnetz: falls kein Marker in der Log-Ausgabe kommt,
                        // schließt das Popup spätestens nach 60 s von selbst.
                        let (fb_ui, fb_seen) = (ui_handle.clone(), window_seen.clone());
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                            if !fb_seen.load(AtomicOrdering::Relaxed) { launch_ui_hide(&fb_ui); }
                        });

                        run_and_track(child, name.clone(), running.clone(), ui_handle.clone()).await
                    }
                    Err(e) => {
                        launch_error(&ui_handle, format!("Start fehlgeschlagen: {:?}", e)).await;
                        return;
                    }
                }
            } else {
                let (loader_kind, id_version) = split_loader_id(&config.loader);

                println!("🔧 [6/7] Bestimme Loader-Version...");
                launch_ui(&ui_handle, format!("Suche {}-Version...", loader_kind), 0.22);
                let version = match config.loader_version.clone().or(id_version) {
                    Some(v) => {
                        println!("   └─ Verwende gespeicherte Version: {}", v);
                        v
                    },
                    None => {
                        println!("   └─ Suche neueste stabile Version für {}...", loader_kind);
                        let Some(v) = latest_loader_version(&loader_kind, &config.minecraft_version).await else {
                            launch_error(&ui_handle, "Loader-Version nicht gefunden".to_string()).await;
                            return;
                        };
                        println!("   └─ Gefunden: {}", v);
                        v
                    }
                };

                println!("   └─ Nutze {} {}", loader_kind, version);

                let loader_version_str = version.clone();

                let Some(loader) = make_loader(&loader_kind, version) else {
                    launch_error(&ui_handle, format!("Unbekannter Loader: {}", loader_kind)).await;
                    return;
                };

                let cfg = builder.loader(loader).build();

                println!("📦 Installiere {} + {}...", config.minecraft_version, loader_kind);
                launch_ui(&ui_handle, format!("Installiere Minecraft {} + {}...", config.minecraft_version, loader_kind), 0.30);

                // Download-Fortschritt in die Loading-Bar schreiben
                {
                    let ui_dl = ui_handle.clone();
                    emitter.on(
                        Event::MultipleDownloadProgress,
                        move |(_path, current, total, _kind): (String, u64, u64, String)| {
                            if total == 0 { return; }
                            let frac = current as f32 / total as f32;
                            launch_ui(&ui_dl, format!("Lade Dateien... {}/{}", current, total), 0.30 + 0.50 * frac);
                        },
                    ).await;
                }

                if let Err(e) = install(&cfg, Some(&emitter)).await {
                    println!("⚠️ Install-Fehler: {:?} || Versuche dennoch Fortzufahren", e);

                    if loader_kind == "forge" {
                        println!("🔧 Patche Forge version.json und versuche es erneut...");
                        launch_ui(&ui_handle, "Repariere Forge-Installation...", 0.50);
                        patch_forge_version_json(&launcherDir, &config.minecraft_version, &loader_version_str);

                        if let Err(e2) = install(&cfg, None).await {
                            launch_error(&ui_handle, format!("Installation fehlgeschlagen: {:?}", e2)).await;
                            return;
                        }
                    } else {
                        launch_error(&ui_handle, format!("Installation fehlgeschlagen: {:?}", e)).await;
                        return;
                    }
                }

                launch_ui(&ui_handle, "Konfiguriere Loader...", 0.80);
                if loader_kind == "forge" {
                    patch_forge_launch_args(&launcherDir, &config.minecraft_version, &loader_version_str);
                }

                launch_ui(&ui_handle, "Richte Java-Laufzeit ein...", 0.86);
                if let Some(jv) = selected_java_version {
                    match ensure_java_runtime_for_instance(&launcherDir, &config.minecraft_version, jv) {
                        Ok(()) => println!("☕ Runtime auf Java {} umgestellt", jv),
                        Err(e) => println!("⚠️ Runtime konnte nicht umgestellt werden: {e}"),
                    }
                }

                println!("▶️  [7/7] Starte Minecraft...");
                launch_ui(&ui_handle, "Starte Minecraft...", 0.92);
                *launch_instant.lock().unwrap() = Some(std::time::Instant::now());
                match launch(&cfg, Some(&emitter)).await {
                    Ok(child) => {
                        launch_ui(&ui_handle, "Warte auf das Minecraft-Fenster...", 0.96);

                        let (fb_ui, fb_seen) = (ui_handle.clone(), window_seen.clone());
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                            if !fb_seen.load(AtomicOrdering::Relaxed) { launch_ui_hide(&fb_ui); }
                        });

                        run_and_track(child, name.clone(), running.clone(), ui_handle.clone()).await
                    }
                    Err(e) => {
                        launch_error(&ui_handle, format!("Start fehlgeschlagen: {:?}", e)).await;
                        return;
                    }
                }
            }

            // ===== Achievements aus der latest.log lesen (Minecraft ist jetzt beendet) =====
            {
                let (scan_dir, scan_name, scan_player, scan_mc) = (
                    instance_dir.clone(), name.clone(), player_name.clone(), config.minecraft_version.clone(),
                );
                let found = tokio::task::spawn_blocking(move || {
                    record_session_achievements(&scan_dir, &scan_name, &scan_player, &scan_mc)
                }).await.unwrap_or(0);

                if found > 0 {
                    println!("🏆 {} neue Achievements erfasst", found);
                    notify_desktop("Neue Achievements", &format!("{name}: {found} neue Achievements erfasst."));
                    let _ = ui_handle.upgrade_in_event_loop(|ui| push_achievements_to_ui(&ui));
                }
            }

            println!("═══════════════════════════════════════════════════════");
            println!("🏁 SESSION ENDE");
            println!("═══════════════════════════════════════════════════════");
            println!("");

            let _ = ui_handle.upgrade_in_event_loop(|_ui| {
                println!("Minecraft beendet");
            });
        });
    });

    // Settings speichern: baut aus den vom Slint-Button mitgegebenen Werten
    // eine LauncherSettings-Struct und schreibt sie nach settings.json
    // (exakt gleiches Schreib-Pattern wie bei instance.json oben).
    ui.on_save_settings(move |ram, java_path, username, close_after_launch, open_on_startup, show_snapshots, items_per_page, _debug_ui| {
        let _ui = save_settings_ui_handle.unwrap();

        let launcherDir = dirs::data_local_dir()
            .expect("kein Local Data Dir")
            .join("srusm");

        // device_tier wird über den separaten Apply-Callback verwaltet,
        // hier nur den aktuellen Wert beibehalten.
        let existing_tier = load_settings(&launcherDir).device_tier;

        let settings = LauncherSettings {
            default_ram_mb: ram,
            java_path: java_path.to_string(),
            default_username: username.to_string(),
            close_after_launch,
            open_on_startup,
            show_snapshots,
            items_per_page,
            device_tier: existing_tier,
        };

        let json = serde_json::to_string_pretty(&settings).expect("Error Making Settings json");
        fs::write(launcherDir.join("settings.json"), json).expect("Error Writing Settings File");

        println!("Settings gespeichert: {:?}", settings);
    });

    // Device-Tier ändern + optional Icons neu cachen.
    let hdl_apply_tier = handle_for_tier.clone();
    let ui_apply_tier = ui.as_weak();
    ui.on_apply_device_tier(move |choice, do_recache| {
        let launcher_dir = dirs::data_local_dir().expect("kein Local Data Dir").join("srusm");

        // 1. Tier setzen
        match parse_tier_choice(&choice) {
            Some(t) => { set_device_tier(t); println!("🖥️  Tier manuell auf {:?}", t); }
            None    => { let t = detect_device_tier(); set_device_tier(t); println!("🖥️  Tier auto: {:?}", t); }
        }

        // 2. In Settings speichern
        let mut s = load_settings(&launcher_dir);
        s.device_tier = choice.to_string();
        if let Ok(json) = serde_json::to_string_pretty(&s) {
            let _ = fs::write(launcher_dir.join("settings.json"), json);
        }

        if !do_recache { return; }

        // 3. Recache-Task starten
        let ui = ui_apply_tier.clone();
        let ldir = launcher_dir.clone();
        hdl_apply_tier.spawn(async move {
            let post = |status: &str, p: f32| {
                let status = status.to_string();
                let _ = ui.upgrade_in_event_loop(move |ui| {
                    ui.set_recache_status(status.into());
                    ui.set_recache_progress(p);
                });
            };

            let _ = ui.upgrade_in_event_loop(|ui| {
                ui.set_recache_popup_visible(true);
                ui.set_recache_status("Starte...".into());
                ui.set_recache_progress(0.0);
            });

            // Schritt 1: Icon-Cache auf Platte leeren
            let icon_dir = ldir.join("icon_cache");
            let entries: Vec<_> = fs::read_dir(&icon_dir)
                .map(|rd| rd.flatten().collect())
                .unwrap_or_default();
            let total = entries.len().max(1);
            for (i, e) in entries.into_iter().enumerate() {
                let _ = fs::remove_file(e.path());
                if i % 20 == 0 {
                    let p = (i as f32 / total as f32) * 0.4;
                    post(&format!("Lösche Icon {} / {}", i + 1, total), p);
                }
            }
            post("Icon-Cache geleert", 0.4);

            // Schritt 2: mod_icons.json in allen Instanzen entfernen
            if let Ok(rd) = fs::read_dir(ldir.join("instances")) {
                let insts: Vec<_> = rd.flatten().collect();
                for (i, e) in insts.iter().enumerate() {
                    let _ = fs::remove_file(e.path().join("mod_icons.json"));
                    let p = 0.4 + (i as f32 / insts.len().max(1) as f32) * 0.4;
                    post(&format!("Bereite Instanz {} / {} vor", i + 1, insts.len()), p);
                }
            }
            post("Leere In-Memory-Caches...", 0.85);

            // Schritt 3: Caches leeren (UI-Thread, weil UI_IMAGE_CACHE thread-local)
            let _ = ui.upgrade_in_event_loop(|ui| {
                clear_all_caches();
                refresh_all_tiles(&ui); // meldet alle jetzt fehlenden Icons an repair_missing_icons
            });

            post("Fertig! Icons werden beim nächsten Öffnen neu geladen.", 1.0);

            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let _ = ui.upgrade_in_event_loop(|ui| {
                ui.set_recache_popup_visible(false);
            });

            // NEU: hier ist `ui` noch das Weak<AppWindow> des Tasks, und wir sind im async-Block
            repair_missing_icons(ui.clone()).await;
        });
    });

    // Modpack Browser Suche: fragt Modrinth async ab (auf dem Tokio-Thread,
    // nur Send-Daten wie ModrinthHit/String sind hier unterwegs), cached dabei
    // pro Treffer das Icon (siehe cache_icon) und baut das browser_packs
    // Model erst INNERHALB von upgrade_in_event_loop neu auf, weil sowohl
    // Rc<VecModel<...>> als auch slint::Image nicht garantiert Send sind und
    // den Thread-Wechsel nicht mitmachen können. Gleiches Prinzip wie beim
    // mcVersions-Model oben. Nutzt "browser_search_handle" statt "handle",
    // weil "handle" bereits oben in on_modpack_play per "move" verschoben wurde.
    /*ui.on_browser_search(move |query| {
        let handle = browser_search_handle.clone();
        let ui_handle = browser_search_ui_handle.clone();
        let query = query.to_string();

        let limit = browser_search_ui_handle.unwrap().get_items_per_page();

        handle.spawn(async move {
            //let Some(hits) = search_projects(&query, "modpack").await else {
            let Some(hits) = search_projects(&query, "modpack", limit).await else { // <-- limit ergänzt
                println!("Modrinth-Suche fehlgeschlagen");
                return;
            };

            let launcherDir = dirs::data_local_dir()
                .expect("kein Local Data Dir")
                .join("srusm");
            let icon_cache_dir = launcherDir.join("icon_cache");

            // Icons für alle Treffer laden/cachen (sequentiell, reicht für ~12 Ergebnisse)
            let mut items_raw = Vec::new();
            for hit in hits {
                let icon_path = cache_icon(&icon_cache_dir, &hit.project_id, &hit.icon_url).await;
                items_raw.push((hit, icon_path));
            }

            let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                let items: Vec<BrowserPackInfo> = items_raw
                    .into_iter()
                    .map(|(hit, icon_path)| BrowserPackInfo {
                        project_id: hit.project_id.into(),
                        name: hit.title.into(),
                        summary: hit.description.into(),
                        author: hit.author.into(),
                        icon: load_icon(&icon_path),
                    })
                    .collect();

                ui.set_browser_packs(ModelRc::new(VecModel::from(items)));
            });
        });
    });*/

    // <-- GEÄNDERT: Sucht jetzt GLEICHZEITIG in Modrinth und CurseForge
    ui.on_browser_search(move |query, _source| {
        let handle = browser_search_handle.clone();
        let ui_handle = browser_search_ui_handle.clone();
        let query_str = query.to_string();
        let limit = browser_search_ui_handle.unwrap().get_items_per_page();

        handle.spawn(async move {
            let is_empty = query_str.trim().is_empty();
            let sort = if is_empty { "downloads" } else { "relevance" };
            println!("🔍 Starte Suche nach: '{}' (Limit: {}, Sort: {})", query_str, limit, sort);

            let mr_future = async {
                // &[] = keine Zusatzfilter, Modpack-Suche bleibt wie bisher
                search_projects(&query_str, "modpack", limit, sort, &[]).await
            };
            let cf_future = async {
                let sort_by_downloads = is_empty;
                search_cf_projects(&query_str, limit, sort_by_downloads).await
            };

            let (mr_hits, cf_hits) = tokio::join!(mr_future, cf_future);

            let launcherDir = dirs::data_local_dir().expect("kein Local Data Dir").join("srusm");
            let icon_cache_dir = launcherDir.join("icon_cache");

            struct PackWithUrl {
                project_id: String,
                name: String,
                summary: String,
                author: String,
                icon_url: Option<String>,
                source: String,
                score: u32,
            }
            let mut packs_with_urls: Vec<PackWithUrl> = Vec::new();

            let calculate_score = |name: &str, author: &str, summary: &str, q: &str| -> u32 {
                if q.is_empty() { return 0; }
                let q_lower = q.to_lowercase();
                let mut score = 0u32;

                let name_lower = name.to_lowercase();
                if name_lower == q_lower { score += 10; } 
                else if name_lower.starts_with(&q_lower) { score += 7; } 
                else if name_lower.contains(&q_lower) { score += 3; }

                let author_lower = author.to_lowercase();
                if author_lower == q_lower { score += 15; } 
                else if author_lower.starts_with(&q_lower) { score += 8; } 
                else if author_lower.contains(&q_lower) { score += 2; }

                let summary_lower = summary.to_lowercase();
                let desc_matches = summary_lower.matches(&q_lower).count() as u32;
                score += desc_matches * 2;

                score
            };

            if let Some(hits) = mr_hits {
                println!("✅ Modrinth: {} Treffer gefunden", hits.len());
                for hit in hits {
                    let score = calculate_score(&hit.title, &hit.author, &hit.description, &query_str);
                    packs_with_urls.push(PackWithUrl {
                        project_id: hit.project_id,
                        name: hit.title,
                        summary: hit.description,
                        author: hit.author,
                        icon_url: hit.icon_url,
                        source: "Modrinth".to_string(),
                        score,
                    });
                }
            } else {
                println!("❌ Modrinth: Keine Treffer oder Fehler");
            }

            if let Some(hits) = cf_hits {
                println!("✅ CurseForge: {} Treffer gefunden", hits.len());
                for hit in hits {
                    let logo_url = hit.logo.as_ref().map(|l| l.thumbnailUrl.clone());
                    let id_str = hit.id.to_string();
                    
                    // NEU: Nimm den Namen des ersten Autors, oder "Unknown" falls die Liste leer ist
                    let author_name = hit.authors.first()
                        .map(|a| a.name.clone())
                        .unwrap_or_else(|| "Unknown".to_string());

                    let score = calculate_score(&hit.name, &author_name, &hit.summary, &query_str);
                    
                    packs_with_urls.push(PackWithUrl {
                        project_id: id_str,
                        name: hit.name,
                        summary: hit.summary,
                        author: author_name, // <-- GEÄNDERT: Den extrahierten Namen verwenden
                        icon_url: logo_url,
                        source: "CurseForge".to_string(),
                        score,
                    });
                }
            } else {
                println!("❌ CurseForge: Keine Treffer oder Fehler");
            }

            // Sortieren: Favoriten zuerst, dann nach Score, dann alphabetisch
            packs_with_urls.sort_by(|a, b| {
                let a_star = is_favorite("modpacks", &a.project_id);
                let b_star = is_favorite("modpacks", &b.project_id);
                b_star.cmp(&a_star) // true (Favorit) vor false
                    .then_with(|| b.score.cmp(&a.score))
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });

            // Limit anwenden (z.B. nur Top 48 behalten, um nicht unnötig 100 Icons zu laden)
            if packs_with_urls.len() > limit as usize {
                println!("✂️ Schneide von {} auf Top {} Ergebnisse ab", packs_with_urls.len(), limit);
                packs_with_urls.truncate(limit as usize);
            }

            println!("📥 Lade {} Icons parallel im Hintergrund...", packs_with_urls.len());

            // ── Phase A: Downloads parallel ─────────────────────────────────
            let dl_sem = Arc::new(tokio::sync::Semaphore::new(tier_download_concurrency(device_tier())));
            let mut download_tasks = Vec::with_capacity(packs_with_urls.len());
            for pack in &packs_with_urls {
                let url = pack.icon_url.clone();
                let sem = dl_sem.clone();
                download_tasks.push(tokio::spawn(async move {
                    let url = url?;
                    let _permit = sem.acquire().await.ok()?;
                    let r: Option<Vec<u8>> = download_bytes(&url).await.ok();
                    r
                }));
            }
            let mut downloaded: Vec<Option<Vec<u8>>> = Vec::with_capacity(download_tasks.len());
            for t in download_tasks { downloaded.push(t.await.unwrap_or(None)); }

            // ── Phase B: Decode + PNG-Save parallel (blocking, MC-aware) ────
            let decode_limit = tier_decode_concurrency(device_tier());
            let decode_sem = Arc::new(tokio::sync::Semaphore::new(decode_limit));
            let cache_dir_b = icon_cache_dir.clone();
            let keys: Vec<String> = packs_with_urls.iter().map(|p| p.project_id.clone()).collect();

            let mut decode_tasks = Vec::with_capacity(downloaded.len());
            for (key, bytes_opt) in keys.into_iter().zip(downloaded.into_iter()) {
                let cdir = cache_dir_b.clone();
                let sem = decode_sem.clone();
                decode_tasks.push(tokio::spawn(async move {
                    let bytes = bytes_opt?;
                    let _permit = sem.acquire().await.ok()?;
                    let r: Option<(Vec<u8>, u32, u32)> = tokio::task::spawn_blocking(move || -> Option<(Vec<u8>, u32, u32)> {
                        let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
                        let (w, h) = img.dimensions();
                        let _ = fs::create_dir_all(&cdir);
                        let _ = img.save_with_format(cdir.join(format!("{key}.png")), image::ImageFormat::Png);
                        Some((img.into_raw(), w, h))
                    }).await.ok()?;
                    r
                }));
            }
            let mut decoded: Vec<Option<(Vec<u8>, u32, u32)>> = Vec::with_capacity(decode_tasks.len());
            for t in decode_tasks { decoded.push(t.await.unwrap_or(None)); }

            println!("✅ Alle Icons geladen, aktualisiere UI...");

            // JETZT ERST im UI-Thread die slint::Image Objekte und BrowserPackInfo erstellen!
            // Das umgeht den `Send`-Fehler, da `load_icon` und `BrowserPackInfo` 
            // nur im UI-Thread existieren.
            let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                let items: Vec<BrowserPackInfo> = packs_with_urls.into_iter().zip(decoded.into_iter()).map(|(pack, pixels)| {
                    let icon = match pixels {
                        Some((rgba, w, h)) => rgba_to_image(&rgba, w, h),
                        None => Image::default(),
                    };
                    BrowserPackInfo {
                        project_id: pack.project_id.clone().into(),
                        name: pack.name.into(),
                        summary: pack.summary.into(),
                        author: pack.author.into(),
                        icon,
                        source: pack.source.into(),
                        starred: is_favorite("modpacks", &pack.project_id),
                    }
                }).collect();
                ui.set_browser_packs(ModelRc::new(VecModel::from(items)));
            });
        });
    });

    // Install-Popup öffnen: lädt alle Versionen des gewählten Modrinth-Projekts
    // und baut daraus die Labels fürs ComboBox-Model, z.B. "1.2.0 (1.21.1, 1.21.2)".
    // Rohdaten (inkl. files/loaders + project_id) werden in pending_install
    // gemerkt, damit on_browser_confirm_install später per Index das Richtige
    // findet, tatsächlich herunterladen und das Icon dazu cachen kann.
    /*ui.on_browser_open_install(move |project_id, pack_name| {
        let ui = install_open_ui_handle.unwrap();
        ui.set_install_popup_pack_name(pack_name.clone());
        ui.set_install_popup_visible(true);
        ui.set_install_popup_installing(false);
        ui.set_install_popup_status(SharedString::from(""));
        ui.set_install_popup_versions(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));

        let handle = install_open_handle.clone();
        let ui_handle = install_open_ui_handle.clone();
        let pending_install = pending_install_open.clone();
        let project_id = project_id.to_string();
        let pack_name = pack_name.to_string();

        handle.spawn(async move {
            let Some(versions) = fetch_pack_versions(&project_id).await else {
                report_progress(&ui_handle, "Konnte Versionen nicht laden.".to_string());
                return;
            };

            let labels: Vec<SharedString> = versions
                .iter()
                .map(|v| {
                    let mc_versions = v.game_versions.join(", ");
                    SharedString::from(format!("{} ({})", v.version_number, mc_versions))
                })
                .collect();

            // WICHTIG: Das Mutex wird NICHT im async-Block beschrieben, sondern
            // erst innerhalb von upgrade_in_event_loop, wo wir wieder auf dem
            // UI-Thread sind.
            let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                *pending_install.lock().unwrap() = Some(PendingInstall { project_id, pack_name, versions });

                ui.set_install_popup_versions(ModelRc::new(VecModel::from(labels)));
            });
        });
    });*/
    ui.on_browser_open_install(move |project_id, pack_name, source| {
        let ui = install_open_ui_handle.unwrap();
        ui.set_install_popup_pack_name(pack_name.clone());
        ui.set_install_popup_visible(true);
        ui.set_install_popup_installing(false);
        ui.set_install_popup_status(SharedString::from(""));
        ui.set_install_popup_versions(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));

        let handle = install_open_handle.clone();
        let ui_handle = install_open_ui_handle.clone();
        let pending_install = pending_install_open.clone();
        let project_id = project_id.to_string();
        let pack_name = pack_name.to_string();
        let source = source.to_string();

        handle.spawn(async move {
            let labels: Vec<SharedString>;
            let mut new_pending = PendingInstall {
                project_id: project_id.clone(),
                pack_name: pack_name.clone(),
                source: source.clone(),
                mrpack_versions: Vec::new(),
                cf_files: Vec::new(),
            };

            if source == "CurseForge" {
                if let Some(files) = fetch_cf_files(&project_id).await {
                    new_pending.cf_files = files.clone();
                    labels = files.iter().map(|f| {
                        let mc = f.gameVersions.first().cloned().unwrap_or_else(|| "Unknown".to_string());
                        SharedString::from(format!("{} ({})", f.fileName, mc))
                    }).collect();
                } else {
                    report_progress(&ui_handle, "Konnte CurseForge-Versionen nicht laden.".to_string());
                    return;
                }
            } else {
                if let Some(versions) = fetch_pack_versions(&project_id).await {
                    new_pending.mrpack_versions = versions.clone();
                    labels = versions.iter().map(|v| {
                        let mc_versions = v.game_versions.join(", ");
                        SharedString::from(format!("{} ({})", v.version_number, mc_versions))
                    }).collect();
                } else {
                    report_progress(&ui_handle, "Konnte Modrinth-Versionen nicht laden.".to_string());
                    return;
                }
            }

            let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                *pending_install.lock().unwrap() = Some(new_pending);
                ui.set_install_popup_versions(ModelRc::new(VecModel::from(labels)));
            });
        });
    });

    // Install-Popup bestätigen: lädt jetzt WIRKLICH die Dateien der gewählten
    // Version von Modrinth herunter. Erkennt anhand der primären Datei ob es
    // sich um ein .mrpack (entpacken + referenzierte Mods laden) oder einzelne
    // Jar-Dateien handelt (direkter Download in mods/). Fortschritt und Fehler
    // werden laufend ins Popup geschrieben (install_popup_status). Zusätzlich
    // wird das Projekt-Icon geladen/gecacht und in der instance.json vermerkt.
    // Der Name der Instanz enthält die Pack-Version, damit mehrere Versionen
    // desselben Packs nebeneinander installiert werden können statt sich zu
    // überschreiben.
    /*ui.on_browser_confirm_install(move |index| {
        if index < 0 {
            return; // keine Version ausgewählt
        }

        // Alle Werte die wir später im async-Block brauchen SYNCHRON aus dem
        // Mutex rausziehen (klonen), weil Slint-Typen nicht garantiert Send
        // sind und den Thread-Wechsel in handle.spawn() nicht mitmachen können.
        let Some(pending) = pending_install_confirm.lock().unwrap().take() else {
            println!("Keine ausstehende Installation gefunden");
            return;
        };

        let Some(selected_version) = pending.versions.get(index as usize).cloned() else {
            println!("Ungültiger Versions-Index: {}", index);
            return;
        };

        let project_id = pending.project_id.clone();
        let pack_name = pending.pack_name.clone();
        let minecraft_version = selected_version.game_versions.first().cloned().unwrap_or_default();
        let loader_kind = pick_loader_kind(&selected_version.loaders);
        let instance_name = format!("{} ({})", pack_name, selected_version.version_number);

        let handle = install_confirm_handle.clone();
        let ui_handle = install_confirm_ui_handle.clone();

        handle.spawn(async move {
            let launcherDir = dirs::data_local_dir()
                .expect("kein Local Data Dir")
                .join("srusm");

            let instance_dir = launcherDir.join("instances").join(&instance_name);
            fs::create_dir_all(&instance_dir).expect("Konnte Instanz-Ordner nicht anlegen");

            // Primäre Datei bestimmen (falls markiert, sonst einfach die erste)
            let primary_file = selected_version
                .files
                .iter()
                .find(|f| f.primary)
                .or_else(|| selected_version.files.first())
                .cloned();

            /*let install_result = match &primary_file {
                Some(file) if file.filename.ends_with(".mrpack") => {
                    report_progress(&ui_handle, format!("Lade Modpack-Archiv: {}", file.filename));
                    match download_bytes(&file.url).await {
                        Ok(bytes) => install_mrpack(&instance_dir, bytes, &ui_handle).await,
                        Err(e) => Err(e),
                    }
                }
                _ => {
                    // Kein .mrpack gefunden, also einzelne Dateien direkt laden
                    download_files_direct(&instance_dir, &selected_version.files, &ui_handle).await
                }
            };*/
            let install_result = if pending.source == "CurseForge" {
                // CurseForge Pfad
                if let Some(cf_file) = pending.cf_files.get(index as usize) {
                    report_progress(&ui_handle, format!("Lade CF Modpack: {}", cf_file.fileName));
                    match download_bytes(&cf_file.downloadUrl).await {
                        Ok(bytes) => install_cf_modpack(&instance_dir, bytes, &ui_handle).await.map(|_| ()),
                        Err(e) => Err(e),
                    }
                } else {
                    Err("Ungültiger CurseForge Datei-Index".to_string())
                }
            } else {
                // Modrinth Pfad (Originalcode)
                match &primary_file {
                    Some(file) if file.filename.ends_with(".mrpack") => {
                        report_progress(&ui_handle, format!("Lade Modpack-Archiv: {}", file.filename));
                        match download_bytes(&file.url).await {
                            Ok(bytes) => install_mrpack(&instance_dir, bytes, &ui_handle).await.map(|_| ()),
                            Err(e) => Err(e),
                        }
                    }
                    _ => download_files_direct(&instance_dir, &selected_version.files, &ui_handle).await.map(|_| ()),
                }
            };

            if let Err(e) = install_result {
                println!("Installation fehlgeschlagen: {e}");
                let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                    ui.set_install_popup_status(SharedString::from(format!("Fehler: {e}")));
                    ui.set_install_popup_installing(false);
                    // Popup bewusst offen lassen, damit der Fehler sichtbar bleibt.
                });
                return; // instance.json wird NICHT geschrieben, keine kaputte Instanz in der Liste
            }

            // Icon des Packs laden/cachen (eigener Request, da wir hier nur
            // project_id kennen, nicht mehr die icon_url aus der Suche).
            report_progress(&ui_handle, "Lade Icon...".to_string());
            let icon_cache_dir = launcherDir.join("icon_cache");
            let icon_url = fetch_project_icon_url(&project_id).await;
            let icon_path = cache_icon(&icon_cache_dir, &project_id, &icon_url).await;

            report_progress(&ui_handle, "Fertig!".to_string());

            let data = ModpackJsonData {
                name: instance_name.clone(),
                minecraft_version,
                loader: loader_kind,
                ram_mb: 2048,
                icon_path: icon_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            };

            let json = serde_json::to_string_pretty(&data).expect("Error Making json");
            fs::write(instance_dir.join("instance.json"), json).expect("Error Writing File");

            let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                let model = ui.get_packs();
                // Downcast auf VecModel, damit wir pushen können (gleiches Model
                // das beim Start über set_packs gesetzt wurde).
                if let Some(vec_model) = model.as_any().downcast_ref::<VecModel<ModpackInfo>>() {
                    vec_model.push(ModpackInfo {
                        name: data.name.into(),
                        loader: data.loader.into(),
                        minecraft_version: data.minecraft_version.into(),
                        ram_mb: data.ram_mb,
                        icon: load_icon(&icon_path),
                    });
                }

                ui.set_install_popup_installing(false);
                ui.set_install_popup_visible(false);
            });
        });
    });*/
    // Install-Popup bestätigen: lädt jetzt WIRKLICH die Dateien der gewählten Version
    // Install-Popup bestätigen: lädt jetzt WIRKLICH die Dateien der gewählten Version
    ui.on_browser_confirm_install(move |index| {
        if index < 0 {
            return;
        }

        let Some(pending) = pending_install_confirm.lock().unwrap().take() else {
            println!("Keine ausstehende Installation gefunden");
            return;
        };

        let project_id = pending.project_id.clone();
        let pack_name = pending.pack_name.clone();
        let source = pending.source.clone();

        let handle = install_confirm_handle.clone();
        let ui_handle = install_confirm_ui_handle.clone();

        handle.spawn(async move {
            let launcherDir = dirs::data_local_dir().expect("kein Local Data Dir").join("srusm");

            if source == "CurseForge" {
                // --- CURSEFORGE PFAD ---
                let Some(cf_file) = pending.cf_files.get(index as usize).cloned() else {
                    let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                        ui.set_install_popup_status(SharedString::from("Ungültiger Datei-Index."));
                        ui.set_install_popup_installing(false);
                    });
                    return;
                };

                let instance_name = format!(
                    "{} ({})",
                    pack_name,
                    cf_file.fileName.replace(".zip", "").replace(".mrpack", "")
                );
                let instance_dir = launcherDir.join("instances").join(&instance_name);
                fs::create_dir_all(&instance_dir).expect("Konnte Instanz-Ordner nicht anlegen");

                report_progress(&ui_handle, format!("Lade CF Modpack: {}", cf_file.fileName));

                let install_result = match download_bytes(&cf_file.downloadUrl).await {
                    Ok(bytes) => install_cf_modpack(&instance_dir, bytes, &ui_handle).await,
                    Err(e) => Err(e),
                };

                // FIX: nicht erst per if-let konsumieren, sondern direkt matchen
                let (minecraft_version, full_loader_id) = match install_result {
                    Ok(v) => v,
                    Err(e) => {
                        println!("Installation fehlgeschlagen: {e}");
                        let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                            ui.set_install_popup_status(SharedString::from(format!("Fehler: {e}")));
                            ui.set_install_popup_installing(false);
                        });
                        return;
                    }
                };

                let (loader_kind, loader_version) = split_loader_id(&full_loader_id);

                report_progress(&ui_handle, "Lade Icon...".to_string());
                let icon_cache_dir = launcherDir.join("icon_cache");
                let icon_url = fetch_project_icon_url(&project_id).await;
                let icon_path = cache_icon(&icon_cache_dir, &project_id, &icon_url).await;
                report_progress(&ui_handle, "Fertig!".to_string());

                let data = ModpackJsonData {
                    name: instance_name.clone(),
                    minecraft_version,
                    loader: loader_kind,
                    ram_mb: 2048,
                    icon_path: icon_path.as_ref().map(|p| p.to_string_lossy().to_string()),
                    loader_version,
                    origin_source: Some("CurseForge".to_string()),
                    origin_project_id: Some(project_id.clone()),
                    origin_version_id: Some(cf_file.id.to_string()),
                };

                let json = serde_json::to_string_pretty(&data).expect("Error Making json");
                fs::write(instance_dir.join("instance.json"), json).expect("Error Writing File");

                let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                    let model = ui.get_packs();
                    if let Some(vec_model) = model.as_any().downcast_ref::<VecModel<ModpackInfo>>() {
                        vec_model.push(ModpackInfo {
                            name: data.name.clone().into(),
                            loader: data.loader.into(),
                            minecraft_version: data.minecraft_version.into(),
                            ram_mb: data.ram_mb,
                            icon: load_icon(&icon_path),
                            starred: is_favorite("instances", &data.name),
                        });
                    }
                    ui.set_install_popup_installing(false);
                    ui.set_install_popup_visible(false);
                });
            } else {
                // --- MODRINTH PFAD ---
                let Some(selected_version) = pending.mrpack_versions.get(index as usize).cloned() else {
                    println!("Ungültiger Versions-Index: {}", index);
                    return;
                };

                let minecraft_version = selected_version.game_versions.first().cloned().unwrap_or_default();
                let loader_kind = pick_loader_kind(&selected_version.loaders);
                let instance_name = format!("{} ({})", pack_name, selected_version.version_number);
                let instance_dir = launcherDir.join("instances").join(&instance_name);
                fs::create_dir_all(&instance_dir).expect("Konnte Instanz-Ordner nicht anlegen");

                let primary_file = selected_version
                    .files
                    .iter()
                    .find(|f| f.primary)
                    .or_else(|| selected_version.files.first())
                    .cloned();

                // Loader-Version aus dem .mrpack lesen, BEVOR es installiert wird.
                let mut loader_version: Option<String> = None;

                let install_result = match &primary_file {
                    Some(file) if file.filename.ends_with(".mrpack") => {
                        report_progress(&ui_handle, format!("Lade Modpack-Archiv: {}", file.filename));
                        match download_bytes(&file.url).await {
                            Ok(bytes) => {
                                if let Ok(index) = read_mrpack_index(&bytes) {
                                    loader_version = mrpack_loader_version(&index.dependencies, &loader_kind);
                                }
                                install_mrpack(&instance_dir, bytes, &ui_handle).await.map(|_| ())
                            }
                            Err(e) => Err(e),
                        }
                    }
                    _ => {
                        download_files_direct(&instance_dir, &selected_version.files, &ui_handle)
                            .await
                            .map(|_| ())
                    }
                };

                if let Err(e) = install_result {
                    println!("Installation fehlgeschlagen: {e}");
                    let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                        ui.set_install_popup_status(SharedString::from(format!("Fehler: {e}")));
                        ui.set_install_popup_installing(false);
                    });
                    return;
                }

                report_progress(&ui_handle, "Lade Icon...".to_string());
                let icon_cache_dir = launcherDir.join("icon_cache");
                let icon_url = fetch_project_icon_url(&project_id).await;
                let icon_path = cache_icon(&icon_cache_dir, &project_id, &icon_url).await;
                report_progress(&ui_handle, "Fertig!".to_string());

                let data = ModpackJsonData {
                    name: instance_name.clone(),
                    minecraft_version,
                    loader: loader_kind,
                    ram_mb: 2048,
                    icon_path: icon_path.as_ref().map(|p| p.to_string_lossy().to_string()),
                    loader_version,
                    origin_source: Some("Modrinth".to_string()),
                    origin_project_id: Some(project_id.clone()),
                    origin_version_id: Some(selected_version.id.clone()),
                };

                let json = serde_json::to_string_pretty(&data).expect("Error Making json");
                fs::write(instance_dir.join("instance.json"), json).expect("Error Writing File");

                let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                    let model = ui.get_packs();
                    if let Some(vec_model) = model.as_any().downcast_ref::<VecModel<ModpackInfo>>() {
                        vec_model.push(ModpackInfo {
                            name: data.name.into(),
                            loader: data.loader.into(),
                            minecraft_version: data.minecraft_version.into(),
                            ram_mb: data.ram_mb,
                            icon: load_icon(&icon_path),
                            starred: is_favorite("instances", &instance_name),
                        });
                    }
                    ui.set_install_popup_installing(false);
                    ui.set_install_popup_visible(false);
                });
            }
        });
    });

    // Modrinth-Modpack (.mrpack) von der Festplatte als neue Instanz importieren:
    // Index lesen (Name, Minecraft-Version, Loader) -> Instanz-Ordner anlegen ->
    // install_mrpack (Overrides entpacken + Mods laden) -> instance.json schreiben -> Kachel anzeigen.
    ui.on_import_mrpack(move |path_input| {
        let ui = import_mrpack_ui_handle.unwrap();
        let handle = import_mrpack_handle.clone();
        let ui_handle = import_mrpack_ui_handle.clone();

        let path = clean_path_input(&path_input);

        // Schnelle Prüfung noch im UI-Thread, damit der Fehler sofort erscheint
        if !path.is_file() {
            ui.set_install_popup_status(SharedString::from(format!("Datei nicht gefunden: {}", path.display())));
            ui.set_import_busy(false);
            return;
        }

        handle.spawn(async move {
            let launcherDir = dirs::data_local_dir().expect("kein Local Data Dir").join("srusm");

            // Alles in einem Block, damit "?" für die Fehlerbehandlung genutzt werden kann
            let result: Result<ModpackJsonData, String> = async {
                let bytes = fs::read(&path).map_err(|e| format!("Konnte Datei nicht lesen: {e}"))?;

                let index = read_mrpack_index(&bytes)?;
                let (minecraft_version, loader) = mrpack_mc_and_loader(&index.dependencies);
                if minecraft_version.is_empty() {
                    return Err("Keine Minecraft-Version im Modpack gefunden.".to_string());
                }

                // Instanzname wie beim Browser-Install: "<Pack> (<Version>)", ohne Version nur der Packname
                let base_name = if index.name.trim().is_empty() {
                    path.file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default()
                } else if index.version_id.trim().is_empty() {
                    index.name.clone()
                } else {
                    format!("{} ({})", index.name, index.version_id)
                };
                let base_name = sanitize_dir_name(&base_name);
                let base_name = if base_name.is_empty() { "Importiertes Modpack".to_string() } else { base_name };

                let instances_dir = launcherDir.join("instances");
                let instance_name = unique_instance_name(&instances_dir, &base_name);
                let instance_dir = instances_dir.join(&instance_name);
                fs::create_dir_all(&instance_dir).map_err(|e| format!("Konnte Instanz-Ordner nicht anlegen: {e}"))?;

                report_progress(&ui_handle, format!("Importiere {instance_name}..."));

                if let Err(e) = install_mrpack(&instance_dir, bytes, &ui_handle).await {
                    // Halbfertigen Ordner wieder wegräumen, sonst bleibt eine kaputte Instanz liegen
                    let _ = fs::remove_dir_all(&instance_dir);
                    return Err(e);
                }

                let loader_version = mrpack_loader_version(&index.dependencies, &loader);

                let data = ModpackJsonData {
                    name: instance_name,
                    minecraft_version,
                    loader,
                    ram_mb: 2048,
                    icon_path: None, // .mrpack enthält kein Projekt-Icon
                    loader_version,
                    ..Default::default()
                };

                let json = serde_json::to_string_pretty(&data)
                    .map_err(|e| format!("Konnte instance.json nicht erzeugen: {e}"))?;
                fs::write(instance_dir.join("instance.json"), json)
                    .map_err(|e| format!("Konnte instance.json nicht schreiben: {e}"))?;

                Ok(data)
            }.await;

            match result {
                Ok(data) => {
                    let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                        // Neue Kachel ins bestehende packs-Model (gleiches Muster wie beim Browser-Install)
                        let model = ui.get_packs();
                        if let Some(vec_model) = model.as_any().downcast_ref::<VecModel<ModpackInfo>>() {
                            vec_model.push(ModpackInfo {
                                name: data.name.clone().into(),
                                loader: data.loader.into(),
                                minecraft_version: data.minecraft_version.into(),
                                ram_mb: data.ram_mb,
                                icon: Image::default(),
                                starred: is_favorite("instances", &data.name),
                            });
                        }
                        ui.set_install_popup_status(SharedString::from("Fertig!"));
                        ui.set_import_busy(false);
                        ui.set_import_popup_visible(false);
                    });
                }
                Err(e) => {
                    println!("Import fehlgeschlagen: {e}");
                    let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                        ui.set_install_popup_status(SharedString::from(format!("Fehler: {e}")));
                        ui.set_import_busy(false); // Popup bleibt offen, damit der Fehler sichtbar ist
                    });
                }
            }
        });
    });

    // Instanz-Details öffnen: liest Mods + Overrides + Icon der Instanz vom
    // Datenträger und wechselt das Panel. Startet danach einen Hintergrund-Task,
    // um fehlende Mod-Icons aus der mods-list.json nachzuladen.
    ui.on_open_instance_details(move |name| {
        let ui = open_details_ui_handle.unwrap();
        let launcherDir = dirs::data_local_dir()
            .expect("kein Local Data Dir")
            .join("srusm");
        let instance_dir = launcherDir.join("instances").join(name.to_string());

        // Mod-Suchfeld bei jedem Instanzwechsel zurücksetzen
        *mod_filter().lock().unwrap() = String::new();
        ui.set_mod_filter("".into());
        
        // 1. Synchrones Laden der Basisdaten für sofortige UI-Anzeige
        let overrides = load_instance_overrides(&instance_dir);
        let instance_icon_path: Option<PathBuf> = File::open(instance_dir.join("instance.json"))
            .ok()
            .and_then(|f| serde_json::from_reader::<_, ModpackJsonData>(BufReader::new(f)).ok())
            .and_then(|c| c.icon_path)
            .map(PathBuf::from);

        ui.set_detail_instance_name(name.clone());
        ui.set_detail_instance_icon(load_icon(&instance_icon_path));
        ui.set_detail_mod_files(ModelRc::new(VecModel::from(
            list_mod_files_cached(name.as_str(), &instance_dir),
        )));

        // NEU: Dropdowns und erweiterte Settings befüllen
        ui.set_detail_overrides_enabled(overrides.enabled);
        ui.set_detail_ram_mb(overrides.ram_mb);

        // Java-Versionen: Dropdown mit Zahlen-Strings füllen.
        let java_paths_str = get_available_java_paths();
        let java_paths: Vec<SharedString> = java_paths_str.iter().map(|s| SharedString::from(s.clone())).collect();

        // Auswahl bestimmen:
        // - Override aktiv + gespeicherter Wert gültig  -> diesen nehmen
        // - kein Override                                 -> passende Java-Version
        //                                                    aus der MC-Version ableiten
        let java_sel = if overrides.enabled && java_paths_str.contains(&overrides.java_path) {
            SharedString::from(overrides.java_path.clone())
        } else {
            // Automatische Auswahl anhand der MC-Version
            let mc_version = load_instance_config(&instance_dir)
                .map(|c| c.minecraft_version)
                .unwrap_or_default();
            let auto_version = java_major_for(&mc_version);
            let target = auto_version.to_string();
            if java_paths_str.contains(&target) {
                SharedString::from(target)
            } else {
                SharedString::from("Launcher Standard")
            }
        };
        ui.set_available_java_paths(ModelRc::new(VecModel::from(java_paths)));
        ui.set_detail_java_selection(java_sel);

        // Accounts für Dropdown zusammenbauen
        let accounts_data = load_accounts(&launcherDir);
        let mut acc_options_str = vec!["Launcher Standard".to_string()];
        for acc in &accounts_data.accounts {
            let kind_str = if acc.kind == "microsoft" { " (Microsoft)" } else { " (Offline)" };
            acc_options_str.push(format!("{}{}", acc.username, kind_str));
        }
        let acc_options: Vec<SharedString> = acc_options_str.iter().map(|s| SharedString::from(s.clone())).collect();

        let acc_sel = if overrides.account_id == "Launcher Standard" || acc_options_str.iter().any(|a| a.contains(&overrides.account_id)) {
            SharedString::from(overrides.account_id)
        } else {
            SharedString::from("Launcher Standard")
        };
        ui.set_available_accounts(ModelRc::new(VecModel::from(acc_options)));
        ui.set_detail_account_selection(acc_sel);

        // Fenster-Einstellungen
        ui.set_detail_window_width(overrides.window_width);
        ui.set_detail_window_height(overrides.window_height);
        ui.set_detail_fullscreen(overrides.fullscreen);
        ui.set_selected_panel("Instance Detail".into());

        // NEU: Beim Öffnen der Detailansicht einmal alle Server pingen,
        // genau wie beim "Aktualisieren"-Button (gleicher Callback).
        ui.invoke_refresh_servers(name.clone());

        // Feature 14: Welten + Server laden
        {
            let worlds = read_worlds(&instance_dir);
            let world_items: Vec<WorldInfoUi> = worlds.iter().map(|w| {
                let icon_path = w.icon_path.clone().map(PathBuf::from);
                let game_type_text = match w.game_type {
                    0 => "Survival", 1 => "Creative", 2 => "Adventure", 3 => "Spectator",
                    _ => "?",
                }.to_string();
                let last_played_text = format_relative_time(w.last_played_ms);
                WorldInfoUi {
                    folder_name: w.folder_name.clone().into(),
                    display_name: w.display_name.clone().into(),
                    icon: load_icon(&icon_path),
                    game_type_text: game_type_text.into(),
                    hardcore: w.hardcore,
                    version: w.version.clone().into(),
                    last_played_text: last_played_text.into(),
                }
            }).collect();
            ui.set_detail_worlds(ModelRc::new(VecModel::from(world_items)));

            let servers = read_servers(&instance_dir);
            let server_items: Vec<ServerInfoUi> = servers.iter().map(|s| {
                // base64-Icon dekodieren und als PNG in den Cache legen
                let icon = decode_server_icon(&s.icon_base64, &launcherDir);
                ServerInfoUi {
                    name: s.name.clone().into(),
                    ip: s.ip.clone().into(),
                    icon,
                    status: "unknown".into(),
                    players_text: "".into(),
                    motd: "".into(),
                }
            }).collect();
            ui.set_detail_servers(ModelRc::new(VecModel::from(server_items)));
            ui.set_detail_worlds_status(format!("{} Welten", worlds.len()).into());
            ui.set_detail_servers_status(format!("{} Server", servers.len()).into());
        }
        ui.set_selected_panel("Instance Detail".into());

        println!("📂 Detailansicht für '{}' geöffnet. Prüfe auf fehlende Icons...", name);

        // 2. NEU: Hintergrund-Task zum Nachladen fehlender Icons (nur EIN UI-Update am Ende)
        let bg_ui_handle = open_details_ui_handle.clone();
        let bg_instance_dir = instance_dir.clone();
        let bg_launcher_dir = launcherDir.clone();
        let bg_handle = open_details_bg_handle.clone();

        let bg_instance_name = name.to_string();

                bg_handle.spawn(async move {
            // NEU: fehlende Instanz-Icon-Dateien (Detail-Kopf) reparieren
            repair_missing_icons(bg_ui_handle.clone()).await;

            let cache_dir = bg_launcher_dir.join("icon_cache");
            let mut icons = load_mod_icons(&bg_instance_dir);

            // Aktuelle Mods und Liste einlesen
            let current_mods = list_mod_files_raw(&bg_instance_dir);
            let mods_list = load_mods_list(&bg_instance_dir);

            // Alle Icons sammeln, die fehlen ODER deren Datei nicht mehr existiert
            let mut missing_icons: Vec<(String, String)> = Vec::new(); // (project_id, display_name)
            for entry in &current_mods {
                // NEU: nur überspringen, wenn die Datei wirklich da ist
                if let Some(p) = &entry.icon_path {
                    if Path::new(p).is_file() { continue; }
                }
                if let Some(list_entry) = mods_list.iter().find(|m| m.filename == entry.display_name) {
                    missing_icons.push((list_entry.project_id.clone(), entry.display_name.clone()));
                }
            }

            if !missing_icons.is_empty() {
                println!("📥 Starte Hintergrund-Download von {} fehlenden Icons...", missing_icons.len());

                let mut new_icons_found = 0;

                for (project_id, display_name) in missing_icons {
                    if let Some(project_info) = fetch_project_info(&project_id).await {
                        if let Some(icon_url) = project_info.icon_url {
                            if let Some(cached_path) = cache_icon(&cache_dir, &project_id, &Some(icon_url)).await {
                                println!("   ✅ Icon gecacht für: {}", display_name);
                                icons.insert(display_name, cached_path.to_string_lossy().to_string());
                                new_icons_found += 1;
                            }
                        }
                    }
                }

                if new_icons_found > 0 {
                    save_mod_icons(&bg_instance_dir, &icons);
                    println!("✨ {} Icons erfolgreich geladen. Aktualisiere UI einmalig...", new_icons_found);

                    let _ = bg_ui_handle.upgrade_in_event_loop(move |ui| {
                        invalidate_instance_cache(&bg_instance_name);
                        let new_mods = list_mod_files_cached(&bg_instance_name, &bg_instance_dir);
                        ui.set_detail_mod_files(ModelRc::new(VecModel::from(new_mods)));
                        println!("   🔄 UI für Mods aktualisiert.");
                    });
                } else {
                    println!("✨ Keine neuen Icons gefunden oder alle Downloads fehlgeschlagen.");
                }
            } else {
                println!("✨ Alle Icons sind bereits vorhanden.");
            }
        });
    });

    // Mod aktivieren/deaktivieren (Umbenennen), danach Liste neu laden damit
    // die Anzeige (enabled/disabled, Button-Text) sofort aktuell ist.
    ui.on_detail_toggle_mod(move |instance_name, filename| {
        let ui = toggle_mod_ui_handle.unwrap();
        let launcherDir = dirs::data_local_dir()
            .expect("kein Local Data Dir")
            .join("srusm");
        let instance_dir = launcherDir.join("instances").join(instance_name.to_string());

        toggle_mod_file(&instance_dir, &filename);
        invalidate_instance_cache(instance_name.as_str());
        ui.set_detail_mod_files(ModelRc::new(VecModel::from(
            list_mod_files_cached(instance_name.as_str(), &instance_dir),
        )));
    });

        // Mod entfernen, danach Liste neu laden.
    ui.on_detail_remove_mod(move |instance_name, filename| {
        let ui = remove_mod_ui_handle.unwrap();
        let launcherDir = dirs::data_local_dir()
            .expect("kein Local Data Dir")
            .join("srusm");
        let instance_dir = launcherDir.join("instances").join(instance_name.to_string());
        remove_mod_file(&instance_dir, &filename);
        
        // meta/icons anhand des Dateinamens aufräumen (ggf. ".disabled"-Suffix entfernen)
        let plain_name = filename.strip_suffix(".disabled").unwrap_or(&filename).to_string();
        let mut meta = load_mod_meta(&instance_dir);
        meta.retain(|m| m.filename != plain_name);
        save_mod_meta(&instance_dir, &meta);
        
        // FIX: icons erst laden, dann Eintrag entfernen, dann speichern
        let mut icons = load_mod_icons(&instance_dir);
        icons.remove(&plain_name);
        save_mod_icons(&instance_dir, &icons);
        
        mods_list_remove(&instance_dir, &plain_name); // Pack-Einträge bleiben erhalten
        
        invalidate_instance_cache(instance_name.as_str());
        ui.set_detail_mod_files(ModelRc::new(VecModel::from(
            list_mod_files_cached(instance_name.as_str(), &instance_dir),
        )));
    });

    // Instanz-eigene Overrides speichern (instance_overrides.json).
    ui.on_detail_save_overrides(move |instance_name, enabled, ram_mb, java_path, account_id, win_w, win_h, fullscreen| {
        let launcherDir = dirs::data_local_dir().expect("kein Local Data Dir").join("srusm");
        let instance_dir = launcherDir.join("instances").join(instance_name.to_string());
        
        // Lade bestehende Overrides (falls vorhanden)
        let mut overrides = load_instance_overrides(&instance_dir);
        
        // Aktualisiere nur die Werte, die wir übergeben bekommen
        overrides.enabled = enabled;
        overrides.ram_mb = ram_mb;
        overrides.java_path = java_path.to_string();
        overrides.account_id = account_id.to_string();
        overrides.window_width = win_w;
        overrides.window_height = win_h;
        overrides.fullscreen = fullscreen;
        
        save_instance_overrides(&instance_dir, &overrides);
        println!("✅ Overrides für '{}' gespeichert.", instance_name);
    });

    // Instanz-Ordner im Dateimanager öffnen. Das ist der gameDir der Instanz, dort liegen
    // mods/, config/, saves/ usw. Es braucht keinen UI-Handle, weil wir nichts in die UI schreiben.
    ui.on_detail_open_folder(move |instance_name| {
        let launcherDir = dirs::data_local_dir()
            .expect("kein Local Data Dir")
            .join("srusm");
        let instance_dir = launcherDir.join("instances").join(instance_name.to_string());

        if instance_dir.is_dir() {
            open_in_file_manager(&instance_dir);
        } else {
            println!("Instanz-Ordner nicht gefunden: {}", instance_dir.display());
        }
    });

    // Add-Mod-Popup Suche: identisch zur Modpack-Suche, nur project_type "mod"
    // statt "modpack" und Ziel-Model ist add_mod_results statt browser_packs.
    // Cached ebenfalls das Icon pro Treffer, gleiches Prinzip wie on_browser_search.
    ui.on_detail_add_mod_search(move |query| {
        /*let handle = add_mod_search_handle.clone();
        let ui_handle = add_mod_search_ui_handle.clone();
        let query = query.to_string();

        let limit = add_mod_search_ui_handle.unwrap().get_items_per_page();

        handle.spawn(async move {
            let Some(hits) = search_projects(&query, "mod", limit, "relevance").await else {
                report_mod_progress(&ui_handle, "Suche fehlgeschlagen.".to_string());
                return;
            };

            let launcherDir = dirs::data_local_dir()
                .expect("kein Local Data Dir")
                .join("srusm");
            let icon_cache_dir = launcherDir.join("icon_cache");

            let mut items_raw = Vec::new();
            for hit in hits {
                let icon_path = cache_icon(&icon_cache_dir, &hit.project_id, &hit.icon_url).await;
                items_raw.push((hit, icon_path));
            }

            let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                let items: Vec<BrowserPackInfo> = items_raw
                    .into_iter()
                    .map(|(hit, icon_path)| BrowserPackInfo {
                        project_id: hit.project_id.into(),
                        name: hit.title.into(),
                        summary: hit.description.into(),
                        author: hit.author.into(),
                        icon: load_icon(&icon_path),
                        source: "Modrinth".into(), // <-- NEU: Source hinzufügen
                    })
                    .collect();

                ui.set_add_mod_results(ModelRc::new(VecModel::from(items)));
            });
        });*/

        let handle = add_mod_search_handle.clone();
        let ui_handle = add_mod_search_ui_handle.clone();
        let query = query.to_string();

        let ui = add_mod_search_ui_handle.unwrap();
        let limit = ui.get_items_per_page();

        // Welche Instanz ist gerade offen? Der Name steht schon in der UI-Property
        // (wird in on_open_instance_details gesetzt), wir müssen also nichts in Slint ändern.
        let instance_name = ui.get_detail_instance_name().to_string();

        // instance.json synchron lesen (lokale Datei, geht schnell) und daraus die Filter bauen.
        let mut extra_facets: Vec<String> = Vec::new();
        let launcherDir = dirs::data_local_dir()
            .expect("kein Local Data Dir")
            .join("srusm");
        let instance_json = launcherDir.join("instances").join(&instance_name).join("instance.json");

        if let Ok(file) = File::open(&instance_json) {
            if let Ok(config) = serde_json::from_reader::<_, ModpackJsonData>(BufReader::new(file)) {
                // Minecraft-Version, z.B. "versions:1.21.1"
                if !config.minecraft_version.is_empty() {
                    extra_facets.push(format!("versions:{}", config.minecraft_version));
                }

                // Loader: bei CurseForge-Imports steht die volle ID drin (z.B. "forge-14.23.5.2860"),
                // Modrinth kennt aber nur "forge". Deshalb alles ab dem ersten '-' abschneiden.
                let loader = config.loader.split('-').next().unwrap_or("").to_lowercase();
                if !loader.is_empty() && loader != "none" {
                    extra_facets.push(format!("categories:{}", loader));
                }
            }

        }

        handle.spawn(async move {
            let Some(hits) = search_projects(&query, "mod", limit, "relevance", &extra_facets).await else {
                report_mod_progress(&ui_handle, "Suche fehlgeschlagen.".to_string());
                return;
            };

            let launcherDir = dirs::data_local_dir()
                .expect("kein Local Data Dir")
                .join("srusm");
            let icon_cache_dir = launcherDir.join("icon_cache");

            let mut items_raw = Vec::new();
            for hit in hits {
                let icon_path = cache_icon(&icon_cache_dir, &hit.project_id, &hit.icon_url).await;
                items_raw.push((hit, icon_path));
            }

            let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                let mut items: Vec<BrowserPackInfo> = items_raw
                    .into_iter()
                    .map(|(hit, icon_path)| {
                        let starred = is_favorite("mods", &hit.project_id);
                        BrowserPackInfo {
                            project_id: hit.project_id.into(),
                            name: hit.title.into(),
                            summary: hit.description.into(),
                            author: hit.author.into(),
                            icon: load_icon(&icon_path),
                            source: "Modrinth".into(),
                            starred,
                        }
                    })
                    .collect();

                items.sort_by_key(|i| !i.starred);
                ui.set_add_mod_results(ModelRc::new(VecModel::from(items)));
            });
        });
    });

    // Mod direkt in die aktuell geöffnete Instanz installieren: liest instance.json
    // für minecraft_version + loader (für die Versionsauswahl), lädt die passende
    // Version herunter, cacht anschließend das Icon des Mods und trägt es in
    // mod_icons.json ein, aktualisiert danach die Mod-Liste in der Detailansicht.
    // Löst jetzt NICHT mehr sofort herunter, sondern ermittelt zuerst per Resolver
    // eine widerspruchsfreie Kombination aus neuem Mod + bereits getrackten Mods
    // und zeigt das Ergebnis im Kompatibilitäts-Popup zur Bestätigung an.
    ui.on_detail_add_mod_install(move |instance_name, project_id, _project_name| {
        let handle = add_mod_install_handle.clone();
        let ui_handle = add_mod_install_ui_handle.clone();
        let pending_resolution = pending_resolution_install.clone();
        let instance_name = instance_name.to_string();
        let project_id = project_id.to_string();

        handle.spawn(async move {
            let launcherDir = dirs::data_local_dir().expect("kein Local Data Dir").join("srusm");
            let instance_dir = launcherDir.join("instances").join(&instance_name);

            let Ok(file) = File::open(instance_dir.join("instance.json")) else {
                report_mod_progress(&ui_handle, "Konnte instance.json nicht lesen.".to_string());
                return;
            };
            let Ok(config) = serde_json::from_reader::<_, ModpackJsonData>(BufReader::new(file)) else {
                report_mod_progress(&ui_handle, "Konnte instance.json nicht parsen.".to_string());
                return;
            };

            //push_debug_log(&ui_handle, format!("Suche passende Version für {}...", project_id));
            let Some(versions) = fetch_pack_versions(&project_id).await else {
                report_mod_progress(&ui_handle, "Konnte Versionen nicht laden.".to_string());
                return;
            };
            let Some(new_version) = pick_best_matching_version(&versions, &config.minecraft_version, &config.loader) else {
                report_mod_progress(&ui_handle, "Keine passende Version gefunden.".to_string());
                return;
            };
            let new_project_info = fetch_project_info(&project_id).await;
            let new_project_name = new_project_info.map(|p| p.title).unwrap_or_else(|| project_id.clone());

            // Alle bereits getrackten Mods dieser Instanz mit ihrer aktuellen Version laden.
            let existing_meta = load_mod_meta(&instance_dir);
            let mut state: HashMap<String, ResolvedMod> = HashMap::new();

            for meta in &existing_meta {
                //push_debug_log(&ui_handle, format!("Lade Bestandsdaten für {} ({})", meta.project_name, meta.version_id));
                if let Some(version) = fetch_version_by_id(&meta.version_id).await {
                    state.insert(meta.project_id.clone(), ResolvedMod {
                        project_id: meta.project_id.clone(),
                        project_name: meta.project_name.clone(),
                        version,
                        is_new: false,
                        original_version_id: Some(meta.version_id.clone()),
                    });
                }
            }

            state.insert(project_id.clone(), ResolvedMod {
                project_id: project_id.clone(),
                project_name: new_project_name,
                version: new_version.clone(),
                is_new: true,
                original_version_id: None,
            });

            let result = resolve_compatibility(state, &config.minecraft_version, &config.loader, &ui_handle).await;

            match result {
                Ok(final_state) => {
                    // Nur Zeilen anzeigen, wo sich tatsächlich etwas ändert (neu oder Versionswechsel).
                    let actions: Vec<CompatAction> = final_state.values()
                        .filter(|m| m.is_new || m.original_version_id.as_deref() != Some(m.version.id.as_str()))
                        .map(|m| CompatAction {
                            project_id: m.project_id.clone(),
                            project_name: m.project_name.clone(),
                            description: if m.is_new {
                                format!("Neu installieren: {}", m.version.version_number)
                            } else {
                                format!("Version ändern: {} -> {}",
                                    m.original_version_id.clone().unwrap_or_default(),
                                    m.version.version_number)
                            },
                            confirmed: true,
                        })
                        .collect();

                    *pending_resolution.lock().unwrap() = Some(final_state);

                    let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                        let ui_actions: Vec<CompatibilityActionUi> = actions.iter().map(|a| CompatibilityActionUi {
                            project_id: a.project_id.clone().into(),
                            label: format!("{}: {}", a.project_name, a.description).into(),
                            confirmed: a.confirmed,
                        }).collect();
                        ui.set_compat_actions(ModelRc::new(VecModel::from(ui_actions)));
                        ui.set_compat_conflict_visible(false);
                        ui.set_compat_popup_visible(true);
                    });
                }
                Err(final_state) => {
                    let problems = find_problems(&final_state);
                    let mut conflict_names: Vec<String> = Vec::new();
                    for p in &problems {
                        match p {
                            Problem::Incompatible { a, b } => {
                                let an = final_state.get(a).map(|m| m.project_name.clone()).unwrap_or(a.clone());
                                let bn = final_state.get(b).map(|m| m.project_name.clone()).unwrap_or(b.clone());
                                conflict_names.push(format!("{} ist inkompatibel mit {}", an, bn));
                            }
                            Problem::NeedsVersion { project_id, requested_by, .. } => {
                                let n = final_state.get(project_id).map(|m| m.project_name.clone()).unwrap_or(project_id.clone());
                                conflict_names.push(format!("{} braucht eine andere Version (gefordert von {})", n, requested_by));
                            }
                            Problem::MissingRequired { project_id, requested_by, .. } => {
                                conflict_names.push(format!("{} fehlt (benötigt von {}), konnte nicht geladen werden", project_id, requested_by));
                            }
                        }
                    }

                    *pending_resolution.lock().unwrap() = Some(final_state);

                    let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                        let mut items: Vec<SharedString> = conflict_names.into_iter().map(SharedString::from).collect();
                        ui.set_compat_conflicts(ModelRc::new(VecModel::from(items)));
                        ui.set_compat_conflict_visible(true);
                        ui.set_compat_popup_visible(true);
                    });
                }
            }
        });
    });

    // Nutzer klickt eine einzelne Zeile im Kompatibilitäts-Popup an/aus.
    ui.on_compat_toggle_action(move |project_id| {
        let ui = compat_toggle_ui_handle.unwrap();
        let model = ui.get_compat_actions();
        let items: Vec<CompatibilityActionUi> = (0..model.row_count())
            .filter_map(|i| model.row_data(i))
            .map(|mut a| { if a.project_id == project_id { a.confirmed = !a.confirmed; } a })
            .collect();
        ui.set_compat_actions(ModelRc::new(VecModel::from(items)));
    });

    // Nutzer bestätigt: alle mit confirmed=true markierten Aktionen werden jetzt
    // tatsächlich heruntergeladen, mod_meta.json wird aktualisiert.
    ui.on_compat_apply(move |instance_name| {
        let ui = compat_apply_ui_handle.unwrap();
        let handle = compat_apply_handle.clone();
        let ui_handle = compat_apply_ui_handle.clone();
        let pending_resolution = pending_resolution_apply.clone();
        let instance_name = instance_name.to_string();

        let confirmed_ids: Vec<String> = {
            let model = ui.get_compat_actions();
            (0..model.row_count())
                .filter_map(|i| model.row_data(i))
                .filter(|a| a.confirmed)
                .map(|a| a.project_id.to_string())
                .collect()
        };

        ui.set_compat_popup_visible(false);

        let Some(final_state) = pending_resolution.lock().unwrap().take() else { return; };

        /*handle.spawn(async move {
            let launcherDir = dirs::data_local_dir().expect("kein Local Data Dir").join("srusm");
            let instance_dir = launcherDir.join("instances").join(&instance_name);
            let icon_cache_dir = launcherDir.join("icon_cache");

            let mut meta = load_mod_meta(&instance_dir);

            for (project_id, rmod) in final_state.iter() {
                if !confirmed_ids.contains(project_id) { continue; }

                let Some(file) = rmod.version.files.iter().find(|f| f.primary).or_else(|| rmod.version.files.first()) else { continue; };

                push_debug_log(&ui_handle, format!("Lade {} für {}...", file.filename, rmod.project_name));
                let Ok(bytes) = download_bytes(&file.url).await else {
                    push_debug_log(&ui_handle, format!("Download fehlgeschlagen für {}", rmod.project_name));
                    continue;
                };

                let mods_dir = instance_dir.join("mods");
                let _ = fs::create_dir_all(&mods_dir);

                // Alte Datei dieses Projekts entfernen, falls es ein Versionswechsel ist.
                if let Some(old) = meta.iter().find(|m| &m.project_id == project_id) {
                    let _ = fs::remove_file(mods_dir.join(&old.filename));
                }

                if let Err(e) = fs::write(mods_dir.join(&file.filename), &bytes) {
                    push_debug_log(&ui_handle, format!("Konnte Datei nicht schreiben: {e}"));
                    continue;
                }

                let icon_url = fetch_project_icon_url(project_id).await;
                let icon_path = cache_icon(&icon_cache_dir, project_id, &icon_url).await;
                let mut icons = load_mod_icons(&instance_dir);
                if let Some(path) = &icon_path {
                    icons.insert(file.filename.clone(), path.to_string_lossy().to_string());
                }
                save_mod_icons(&instance_dir, &icons);

                meta.retain(|m| &m.project_id != project_id);
                meta.push(ModEntryMeta {
                    filename: file.filename.clone(),
                    project_id: project_id.clone(),
                    project_name: rmod.project_name.clone(),
                    version_id: rmod.version.id.clone(),
                });
            }

            save_mod_meta(&instance_dir, &meta);

            let mods_raw = list_mod_files_raw(&instance_dir);
            let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                ui.set_add_mod_status("Aktualisiert.".into());
                ui.set_detail_mod_files(ModelRc::new(VecModel::from(
                    mods_raw.into_iter().map(build_mod_file_info).collect::<Vec<_>>(),
                )));
            });
        });*/

        handle.spawn(async move {
            let launcherDir = dirs::data_local_dir().expect("kein Local Data Dir").join("srusm");
            let instance_dir = launcherDir.join("instances").join(&instance_name);
            let icon_cache_dir = launcherDir.join("icon_cache");
            let mods_dir = instance_dir.join("mods");
            let _ = fs::create_dir_all(&mods_dir);

            let mut meta = load_mod_meta(&instance_dir);
            // Icons einmalig vor der Schleife laden, um wiederholte Lese-/Schreibzugriffe zu sparen
            let mut icons = load_mod_icons(&instance_dir);



            for (project_id, rmod) in final_state.iter() {
                if !confirmed_ids.contains(project_id) { continue; }
                let Some(file) = rmod.version.files.iter().find(|f| f.primary).or_else(|| rmod.version.files.first()) else { continue; };
                //push_debug_log(&ui_handle, format!("Lade {} für {}...", file.filename, rmod.project_name));
                let Ok(bytes) = download_bytes(&file.url).await else {
                    //push_debug_log(&ui_handle, format!("Download fehlgeschlagen für {}", rmod.project_name));
                    continue;
                };

                let old_filename: Option<String> = meta.iter().find(|m| &m.project_id == project_id).map(|m| m.filename.clone());

                // Alte Datei dieses Projekts entfernen, falls es ein Versionswechsel ist.
                if let Some(old) = meta.iter().find(|m| &m.project_id == project_id) {
                    let _ = fs::remove_file(mods_dir.join(&old.filename));
                    let _ = fs::remove_file(mods_dir.join(format!("{}.disabled", old.filename)));
                    icons.remove(&old.filename);
                }
                if let Err(e) = fs::write(mods_dir.join(&file.filename), &bytes) {
                    //push_debug_log(&ui_handle, format!("Konnte Datei nicht schreiben: {e}"));
                    continue;
                }
                let icon_url = fetch_project_icon_url(project_id).await;
                let icon_path = cache_icon(&icon_cache_dir, project_id, &icon_url).await;
                if let Some(path) = &icon_path {
                    icons.insert(file.filename.clone(), path.to_string_lossy().to_string());
                }
                meta.retain(|m| &m.project_id != project_id);
                meta.push(ModEntryMeta {
                    filename: file.filename.clone(),
                    project_id: project_id.clone(),
                    project_name: rmod.project_name.clone(),
                    version_id: rmod.version.id.clone(),
                });

                mods_list_upsert(&instance_dir, project_id, old_filename.as_deref(), &file.filename);
            }
            
            save_mod_meta(&instance_dir, &meta);
            save_mod_icons(&instance_dir, &icons);

            // Cache invalidieren + UI einmalig mit frischen Daten aus dem Cache bauen
            invalidate_instance_cache(&instance_name);
            let name_for_ui = instance_name.clone();
            let dir_for_ui = instance_dir.clone();
            let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                ui.set_add_mod_status("Aktualisiert.".into());
                ui.set_detail_mod_files(ModelRc::new(VecModel::from(
                    list_mod_files_cached(&name_for_ui, &dir_for_ui),
                )));
            });
        });
    });

    // Irreconcilable-Fall: Nutzer wählt "deaktivieren" oder "abbrechen".
        // Irreconcilable-Fall: Nutzer wählt "deaktivieren" oder "abbrechen".
    ui.on_compat_disable_conflicts(move |instance_name| {
        let ui = compat_disable_ui_handle.unwrap();
        let pending_resolution = pending_resolution_disable.clone();

        let Some(final_state) = pending_resolution.lock().unwrap().take() else { return; };
        let problems = find_problems(&final_state);

        let launcherDir = dirs::data_local_dir().expect("kein Local Data Dir").join("srusm");
        let iname = instance_name.to_string();
        let instance_dir = launcherDir.join("instances").join(&iname);
        let meta = load_mod_meta(&instance_dir);

        let mut to_disable: std::collections::HashSet<String> = std::collections::HashSet::new();
        for p in &problems {
            match p {
                Problem::Incompatible { a, .. } => { to_disable.insert(a.clone()); }
                Problem::NeedsVersion { project_id, .. } => { to_disable.insert(project_id.clone()); }
                Problem::MissingRequired { project_id, .. } => { to_disable.insert(project_id.clone()); }
            }
        }

        for project_id in &to_disable {
            if let Some(m) = meta.iter().find(|m| &m.project_id == project_id) {
                let mods_dir = instance_dir.join("mods");
                let active_path = mods_dir.join(&m.filename);
                let disabled_path = mods_dir.join(format!("{}.disabled", m.filename));
                if active_path.exists() {
                    if let Err(e) = fs::rename(&active_path, &disabled_path) {
                        println!("Konnte Mod nicht deaktivieren: {e}");
                    }
                }
            }
        }

        ui.set_compat_popup_visible(false);
        invalidate_instance_cache(&iname);
        ui.set_detail_mod_files(ModelRc::new(VecModel::from(
            list_mod_files_cached(&iname, &instance_dir),
        )));
    });

    ui.on_compat_cancel(move || {
        let ui = compat_cancel_ui_handle.unwrap();
        ui.set_compat_mode("mods".into());
        ui.set_compat_popup_visible(false);
    });

    // Offline-Account hinzufügen: legt einen neuen Eintrag in accounts.json an
    // und markiert ihn als ausgewählt, falls noch kein Account existiert.
    ui.on_account_add_offline(move |username| {
        let ui = account_add_offline_ui_handle.unwrap();

        let username = username.trim().to_string();
        if username.is_empty() {
            ui.set_accounts_status(SharedString::from("Bitte einen Namen eingeben."));
            return;
        }

        let launcherDir = dirs::data_local_dir()
            .expect("kein Local Data Dir")
            .join("srusm");
        let mut data = load_accounts(&launcherDir);

        // Einfache, ausreichend eindeutige ID (kein extra uuid-crate nötig):
        // Art + Name + laufende Nummer.
        let id = format!("offline-{}-{}", username, data.accounts.len());
        /*data.accounts.push(AccountEntry {
            id: id.clone(),
            kind: "offline".to_string(),
            username,
        });*/
        data.accounts.push(AccountEntry {
            id: id.clone(),
            kind: "offline".to_string(),
            username,
            ..Default::default() // Microsoft-Felder bleiben None
        });
        if data.selected_id.is_none() {
            data.selected_id = Some(id);
        }

        save_accounts(&launcherDir, &data);

        ui.set_accounts(ModelRc::new(VecModel::from(accounts_to_ui_model(&data))));
        ui.set_accounts_status(SharedString::from(""));
    });

    // Schritt 1: Login-Link erzeugen, im Browser öffnen und das Popup zeigen
    ui.on_account_add_microsoft(move || {
        let ui = account_add_microsoft_ui_handle.unwrap();

        match lyceris::auth::microsoft::create_link() {
            Ok(url) => {
                open_in_browser(&url);
                ui.set_ms_login_url(url.into());
                ui.set_ms_status(SharedString::from(""));
                ui.set_ms_busy(false);
                ui.set_ms_popup_visible(true);
            }
            Err(e) => {
                ui.set_accounts_status(SharedString::from(format!("Konnte Login-Link nicht erzeugen: {e:?}")));
            }
        }
    });

    // "Im Browser öffnen" im Popup nochmal drücken (falls der Browser nicht aufging)
    ui.on_account_microsoft_open_browser(move || {
        let ui = account_ms_open_ui_handle.unwrap();
        open_in_browser(&ui.get_ms_login_url());
    });

    // Schritt 2: eingefügte Adresse -> Code -> Anmeldung bei Microsoft/Xbox/Minecraft
    ui.on_account_microsoft_submit(move |pasted| {
        let ui = account_ms_submit_ui_handle.unwrap();
        let handle = account_ms_submit_handle.clone();
        let ui_handle = account_ms_submit_ui_handle.clone();

        let Some(code) = extract_auth_code(&pasted) else {
            ui.set_ms_status(SharedString::from(
                "Kein Code gefunden - bitte die komplette Adresse der leeren Seite einfügen.",
            ));
            return;
        };

        ui.set_ms_busy(true);
        ui.set_ms_status(SharedString::from("Melde an..."));

        handle.spawn(async move {
            let launcherDir = dirs::data_local_dir().expect("kein Local Data Dir").join("srusm");

            match lyceris::auth::microsoft::authenticate(code, http_client()).await {
                Ok(mc) => {
                    let mut data = load_accounts(&launcherDir);
                    let id = format!("microsoft-{}", mc.uuid);

                    // Gleicher Account nochmal angemeldet: alten Eintrag ersetzen statt doppelt anlegen
                    data.accounts.retain(|a| a.id != id);
                    data.accounts.push(AccountEntry {
                        id: id.clone(),
                        kind: "microsoft".to_string(),
                        username: mc.username.clone(),
                        uuid: Some(mc.uuid.clone()),
                        xuid: Some(mc.xuid.clone()),
                        access_token: Some(mc.access_token.clone()),
                        refresh_token: Some(mc.refresh_token.clone()),
                        token_exp: Some(mc.exp),
                    });
                    if data.selected_id.is_none() {
                        data.selected_id = Some(id);
                    }
                    save_accounts(&launcherDir, &data);

                    let username = mc.username;
                    let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                        ui.set_accounts(ModelRc::new(VecModel::from(accounts_to_ui_model(&data))));
                        ui.set_ms_busy(false);
                        ui.set_ms_popup_visible(false);
                        ui.set_accounts_status(SharedString::from(format!("Angemeldet als {username}.")));
                    });
                }
                Err(e) => {
                    // z.B. abgelaufener/schon benutzter Code, oder der Account besitzt kein Minecraft
                    let msg = format!("Anmeldung fehlgeschlagen: {e:?}");
                    println!("{msg}");
                    let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                        ui.set_ms_status(SharedString::from(msg));
                        ui.set_ms_busy(false);
                    });
                }
            }
        });
    });

    // Account als aktiv auswählen (wird beim nächsten Spielstart als
    // Username verwendet, siehe on_modpack_play).
    ui.on_account_select(move |id| {
        let ui = account_select_ui_handle.unwrap();

        let launcherDir = dirs::data_local_dir()
            .expect("kein Local Data Dir")
            .join("srusm");
        let mut data = load_accounts(&launcherDir);
        data.selected_id = Some(id.to_string());
        save_accounts(&launcherDir, &data);

        ui.set_accounts(ModelRc::new(VecModel::from(accounts_to_ui_model(&data))));
    });

    // Account entfernen. War er ausgewählt, wird automatisch der erste
    // verbleibende Account (falls vorhanden) neu ausgewählt.
    ui.on_account_remove(move |id| {
        let ui = account_remove_ui_handle.unwrap();

        let launcherDir = dirs::data_local_dir()
            .expect("kein Local Data Dir")
            .join("srusm");
        let mut data = load_accounts(&launcherDir);

        data.accounts.retain(|a| a.id != id.to_string());
        if data.selected_id.as_deref() == Some(id.as_str()) {
            data.selected_id = data.accounts.first().map(|a| a.id.clone());
        }

        save_accounts(&launcherDir, &data);

        ui.set_accounts(ModelRc::new(VecModel::from(accounts_to_ui_model(&data))));
    });

        // <-- NEU: Lokale CurseForge .zip importieren
    ui.on_import_cf_zip(move |path_input| {
        let ui = import_cf_zip_ui_handle.unwrap(); // <-- GEÄNDERT
        let handle = import_cf_zip_handle.clone(); // <-- GEÄNDERT
        let ui_handle = import_cf_zip_ui_handle.clone(); // <-- GEÄNDERT

        let path = clean_path_input(&path_input);

        if !path.is_file() {
            ui.set_install_popup_status(SharedString::from(format!("Datei nicht gefunden: {}", path.display())));
            ui.set_import_cf_busy(false);
            return;
        }

        handle.spawn(async move {
            let launcherDir = dirs::data_local_dir().expect("kein Local Data Dir").join("srusm");

            let result: Result<ModpackJsonData, String> = async {
                let bytes = fs::read(&path).map_err(|e| format!("Konnte Datei nicht lesen: {e}"))?;
                
                // WICHTIG: Eigener Block, damit `manifest_file` (nicht Send) vor dem await gedroppt wird!
                let (mc_version, loader, pack_name) = {
                    let cursor = Cursor::new(bytes.clone());
                    let mut archive = zip::ZipArchive::new(cursor).map_err(|e| format!("Ungültige Zip: {e}"))?;
                    let mut manifest_file = archive.by_name("manifest.json").map_err(|_| "Kein manifest.json".to_string())?;
                    let mut manifest_str = String::new();
                    manifest_file.read_to_string(&mut manifest_str).map_err(|e| format!("Manifest Lesefehler: {e}"))?;
                    
                    #[derive(Deserialize)]
                    struct MiniManifest { name: String, minecraft: MiniMinecraft }
                    #[derive(Deserialize)]
                    struct MiniMinecraft { version: String, modLoaders: Vec<MiniLoader> }
                    #[derive(Deserialize)]
                    struct MiniLoader { id: String }
                    
                    let mini: MiniManifest = serde_json::from_str(&manifest_str).map_err(|e| e.to_string())?;
                    let mc_version = mini.minecraft.version;
                    // WICHTIG: Die komplette ID behalten
                    let loader = mini.minecraft.modLoaders.first()
                        .map(|l| l.id.clone())
                        .unwrap_or_else(|| "none".to_string());
                    (mc_version, loader, mini.name)
                }; // <-- `manifest_file` und `archive` werden hier sicher gedroppt!
                
                let base_name = sanitize_dir_name(&pack_name);
                let instances_dir = launcherDir.join("instances");
                let instance_name = unique_instance_name(&instances_dir, &base_name);
                let instance_dir = instances_dir.join(&instance_name);
                fs::create_dir_all(&instance_dir).map_err(|e| format!("Ordner anlegen fehlgeschlagen: {e}"))?;

                report_progress(&ui_handle, format!("Konvertiere und importiere {}...", instance_name));

                // Hier rufen wir die gleiche Funktion auf wie beim Browser-Download!
                match install_cf_modpack(&instance_dir, bytes, &ui_handle).await {
                    Ok(_) => {}
                    Err(e) => {
                        let _ = fs::remove_dir_all(&instance_dir);
                        return Err(e);
                    }
                }

                let (loader_kind, loader_version) = split_loader_id(&loader);

                Ok(ModpackJsonData {
                    name: instance_name,
                    minecraft_version: mc_version,
                    loader: loader_kind,
                    ram_mb: 2048,
                    icon_path: None,
                    loader_version,
                    ..Default::default()
                })
            }.await;

            match result {
                Ok(data) => {
                    let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                        let model = ui.get_packs();
                        if let Some(vec_model) = model.as_any().downcast_ref::<VecModel<ModpackInfo>>() {
                            vec_model.push(ModpackInfo {
                                name: data.name.clone().into(),
                                loader: data.loader.into(),
                                minecraft_version: data.minecraft_version.into(),
                                ram_mb: data.ram_mb,
                                icon: Image::default(),
                                starred: is_favorite("instances", &data.name),
                            });
                        }
                        ui.set_install_popup_status(SharedString::from("Fertig!"));
                        ui.set_import_cf_busy(false);
                        ui.set_import_cf_popup_visible(false);
                    });
                }
                Err(e) => {
                    println!("CF Import fehlgeschlagen: {e}");
                    let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                        ui.set_install_popup_status(SharedString::from(format!("Fehler: {e}")));
                        ui.set_import_cf_busy(false);
                    });
                }
            }
        });
    });

        // ===================== Block 2: Misc, Mod-Suche, Quick-Connect, Spielzeit =====================

    // Spielzeit-Text für Kachel und Detailansicht. Das zweite Argument (Tick) wird nicht gelesen,
    // es sorgt nur dafür, dass Slint die Abfrage nach jeder Session neu ausführt.
    ui.on_instance_playtime(move |name, _tick| {
        let dir = launcher_dir().join("instances").join(name.as_str());
        SharedString::from(format_playtime(load_playtime_secs(&dir)))
    });

        // ===================== Statistik-Dashboard (Feature 5) =====================
    let h = ui.as_weak();
    ui.on_stats_open(move || {
        let ui = h.unwrap();
        let instances = launcher_dir().join("instances");
        let today = unix_now() / 86400; // Tage seit 1970 (UTC, daher kann die Tagesgrenze leicht abweichen)

        let mut per_instance: Vec<(String, u64)> = Vec::new();
        let mut days = [0u64; 7]; // [heute-6 ... heute]
        let mut startups: Vec<(String, Vec<StartupRecord>)> = Vec::new();

        if let Ok(rd) = fs::read_dir(&instances) {
            for e in rd.flatten() {
                let path = e.path();
                if !path.is_dir() { continue; }
                let name = e.file_name().to_string_lossy().to_string();
                let p = load_playtime(&path);
                per_instance.push((name.clone(), p.total_secs));
                for s in &p.sessions {
                    let d = s.start / 86400; // Session zählt für den Tag, an dem sie gestartet wurde
                    if d + 6 >= today && d <= today { days[(d + 6 - today) as usize] += s.secs; }
                }
                startups.push((name, load_startups(&path)));
            }
        }

        let fmt = |s: u64| if s == 0 { "–".to_string() } else { format_playtime(s) };
        let total: u64 = per_instance.iter().map(|(_, s)| *s).sum();
        let week: u64 = days.iter().sum();
        per_instance.sort_by(|a, b| b.1.cmp(&a.1));
        let best = per_instance.first().filter(|x| x.1 > 0).map(|x| x.0.clone()).unwrap_or_else(|| "–".to_string());
        let max_inst = per_instance.first().map(|x| x.1).unwrap_or(0).max(1);

        let inst_rows: Vec<StatRowUi> = per_instance.iter().map(|(n, s)| StatRowUi {
            name: n.clone().into(), text: fmt(*s).into(), fraction: *s as f32 / max_inst as f32,
        }).collect();

        // Wochentag: 1.1.1970 war ein Donnerstag -> (Tage + 4) % 7 mit So=0
        const WD: [&str; 7] = ["So", "Mo", "Di", "Mi", "Do", "Fr", "Sa"];
        let max_day = days.iter().copied().max().unwrap_or(0).max(1);
        let day_rows: Vec<StatRowUi> = (0..7usize).map(|i| {
            let d = today + i as u64 - 6;
            StatRowUi {
                name: format!("{}{}", WD[((d + 4) % 7) as usize], if i == 6 { " (heute)" } else { "" }).into(),
                text: fmt(days[i]).into(),
                fraction: days[i] as f32 / max_day as f32,
            }
        }).collect();

        // Startzeiten: Durchschnitt pro (Instanz, JVM-Preset). Verschiedene Packs nicht mischen!
        let mut start_rows_raw: Vec<(String, f32, usize, f32)> = Vec::new(); // (Label, Ø, Anzahl, letzte)
        for (name, recs) in &startups {
            let mut by_preset: HashMap<String, Vec<f32>> = HashMap::new();
            for r in recs { by_preset.entry(r.preset.clone()).or_default().push(r.game_secs); }
            for (preset, v) in by_preset {
                let avg = v.iter().sum::<f32>() / v.len() as f32;
                start_rows_raw.push((format!("{name} · {preset}"), avg, v.len(), *v.last().unwrap_or(&0.0)));
            }
        }
        start_rows_raw.sort_by(|a, b| a.0.cmp(&b.0));
        let max_start = start_rows_raw.iter().map(|r| r.1).fold(1.0f32, f32::max);
        let start_rows: Vec<StatRowUi> = start_rows_raw.into_iter().map(|(label, avg, n, last)| StatRowUi {
            name: label.into(),
            text: format!("Ø {avg:.1} s (n={n}, zuletzt {last:.1} s)").into(),
            fraction: avg / max_start,
        }).collect();

        ui.set_stats_summary(format!("Gesamt: {}  ·  Letzte 7 Tage: {}  ·  Meistgespielt: {}", fmt(total), fmt(week), best).into());
        ui.set_stats_instances(ModelRc::new(VecModel::from(inst_rows)));
        ui.set_stats_days(ModelRc::new(VecModel::from(day_rows)));
        ui.set_stats_startups(ModelRc::new(VecModel::from(start_rows)));
    });

    // Misc-Popup öffnen: alles laden, dann anzeigen.
    let h = ui.as_weak();
    ui.on_misc_open(move |name| {
        let ui = h.unwrap();
        push_misc_to_ui(&ui, name.as_str());
        ui.set_misc_popup_visible(true);
    });

    // Speichern-Button des Misc-Popups: Textfeld, Preset und die beiden Schalter.
    let h = ui.as_weak();
    ui.on_misc_save(move |name, jvm_args, preset_id, use_glfw, auto_fix| {
        let ui = h.unwrap();
        let dir = launcher_dir().join("instances").join(name.as_str());
        let mut misc = load_misc(&dir); // frisch laden: Env/Wrapper wurden evtl. schon separat gespeichert
        misc.jvm_args = jvm_args.trim().to_string();
        misc.jvm_preset = preset_id.to_string();
        misc.use_system_glfw = use_glfw;
        misc.auto_fix_duplicates = auto_fix;
        save_misc(&dir, &misc);
        ui.set_misc_status("Gespeichert.".into());
    });

    // Umgebungsvariable hinzufügen (mit einfacher Prüfung des Namens).
    let h = ui.as_weak();
    ui.on_misc_env_add(move |name, var_name, var_value| {
        let ui = h.unwrap();
        let var_name = var_name.trim().to_string();
        if var_name.is_empty() || var_name.contains('=') || var_name.contains(char::is_whitespace) {
            ui.set_misc_status("Ungültiger Name (leer, '=' oder Leerzeichen sind nicht erlaubt).".into());
            return;
        }
        let dir = launcher_dir().join("instances").join(name.as_str());
        let mut misc = load_misc(&dir);
        misc.env_vars.retain(|v| v.name != var_name); // gleicher Name überschreibt
        misc.env_vars.push(EnvVar { name: var_name, value: var_value.to_string() });
        save_misc(&dir, &misc);
        ui.set_misc_status("".into());
        push_misc_lists(&ui, name.as_str());
    });

    let h = ui.as_weak();
    ui.on_misc_env_remove(move |name, index| {
        let ui = h.unwrap();
        let dir = launcher_dir().join("instances").join(name.as_str());
        let mut misc = load_misc(&dir);
        if index >= 0 && (index as usize) < misc.env_vars.len() {
            misc.env_vars.remove(index as usize);
            save_misc(&dir, &misc);
        }
        push_misc_lists(&ui, name.as_str());
    });

    let h = ui.as_weak();
    ui.on_misc_wrapper_toggle(move |name, id, enabled| {
        let ui = h.unwrap();
        let dir = launcher_dir().join("instances").join(name.as_str());
        let mut misc = load_misc(&dir);
        misc.wrappers.retain(|w| w != id.as_str());
        if enabled { misc.wrappers.push(id.to_string()); }
        save_misc(&dir, &misc);
        push_misc_lists(&ui, name.as_str()); // aktualisiert auch die Warnung
    });

    // Mod-Suchfeld: Filtertext merken, Liste neu aufbauen (list_mod_files_raw filtert).
    let h = ui.as_weak();
    ui.on_detail_filter_mods(move |name, text| {
        let ui = h.unwrap();
        *mod_filter().lock().unwrap() = text.to_string();
        let dir = launcher_dir().join("instances").join(name.as_str());
        // Cache wird nicht invalidiert — nur aus dem Speicher gelesen!
        ui.set_detail_mod_files(ModelRc::new(VecModel::from(
            list_mod_files_cached(name.as_str(), &dir),
        )));
    });

        // ===================== Block 3: Manage Instanz =====================

    // Popup öffnen: aktuellen Loader anzeigen und die Versionsliste im Hintergrund laden.
    let h = ui.as_weak();
    let hdl = rt.handle().clone();
    ui.on_manage_open(move |name| {
        let ui = h.unwrap();
        let dir = launcher_dir().join("instances").join(name.as_str());
        ui.set_manage_confirm_delete(false);
        ui.set_manage_status("".into());
        ui.set_manage_loader_report("".into());
        ui.set_manage_loader_pending("".into());
        ui.set_manage_loader_versions(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));

        let Some(cfg) = load_instance_config(&dir) else {
            ui.set_manage_status("instance.json fehlt.".into());
            ui.set_manage_popup_visible(true);
            return;
        };
        let (kind, id_ver) = split_loader_id(&cfg.loader);
        let shown = if kind == "none" {
            "Vanilla (kein Loader)".to_string()
        } else {
            format!("{} {}", kind, cfg.loader_version.clone().or(id_ver).unwrap_or_else(|| "(neueste stabile)".to_string()))
        };
        ui.set_manage_loader_current(shown.into());
        ui.set_manage_popup_visible(true);
        if kind == "none" { return; }

        let (mc, wh) = (cfg.minecraft_version.clone(), h.clone());
        hdl.spawn(async move {
            let versions = list_loader_versions(&kind, &mc).await.unwrap_or_default();
            let _ = wh.upgrade_in_event_loop(move |ui| {
                let items: Vec<SharedString> = versions.into_iter().map(SharedString::from).collect();
                ui.set_manage_loader_versions(ModelRc::new(VecModel::from(items)));
            });
        });
    });

    // Duplizieren: Ordner im Hintergrund kopieren, dann Kachel anhängen.
    let h = ui.as_weak();
    let hdl = rt.handle().clone();
    let run = running.clone();
    ui.on_manage_duplicate(move |name, new_name| {
        let ui = h.unwrap();
        if is_running(&run, name.as_str()) { ui.set_manage_status("Die Instanz läuft: bitte erst beenden.".into()); return; }
        let Some(base) = clean_new_name(new_name.as_str()) else { ui.set_manage_status("Bitte einen gültigen Namen eingeben.".into()); return; };
        let instances = launcher_dir().join("instances");
        let target = unique_instance_name(&instances, &base);
        let (src, dst) = (instances.join(name.as_str()), instances.join(&target));
        ui.set_manage_status("Kopiere Instanz...".into());

        let wh = h.clone();
        hdl.spawn(async move {
            let res = tokio::task::spawn_blocking(move || -> Result<ModpackJsonData, String> {
                let r = (|| -> Result<ModpackJsonData, String> {
                    copy_dir_recursive(&src, &dst).map_err(|e| format!("Kopieren fehlgeschlagen: {e}"))?;
                    let mut cfg = load_instance_config(&dst).ok_or("instance.json fehlt in der Kopie.".to_string())?;
                    cfg.name = target.clone();
                    save_instance_config(&dst, &cfg)?;
                    Ok(cfg)
                })();
                if r.is_err() { let _ = fs::remove_dir_all(&dst); } // halbe Kopie wegräumen
                r
            }).await.unwrap_or_else(|e| Err(format!("Interner Fehler: {e}")));

            let _ = wh.upgrade_in_event_loop(move |ui| match res {
                Ok(cfg) => {
                    with_packs_model(&ui, |vm| vm.push(tile_from_config(&cfg)));
                    ui.set_manage_status(format!("Dupliziert als '{}'.", cfg.name).into());
                }
                Err(e) => ui.set_manage_status(e.into()),
            });
        });
    });

    // Umbenennen: Ordner verschieben, instance.json und Kachel anpassen.
    let h = ui.as_weak();
    let run = running.clone();
    ui.on_manage_rename(move |name, new_name| {
        let ui = h.unwrap();
        if is_running(&run, name.as_str()) { ui.set_manage_status("Die Instanz läuft: bitte erst beenden.".into()); return; }
        let Some(base) = clean_new_name(new_name.as_str()) else { ui.set_manage_status("Bitte einen gültigen Namen eingeben.".into()); return; };
        if base == name.as_str() { ui.set_manage_status("Der Name ist unverändert.".into()); return; }
        let instances = launcher_dir().join("instances");
        let (src, dst) = (instances.join(name.as_str()), instances.join(&base));
        if dst.exists() { ui.set_manage_status("Eine Instanz mit diesem Namen existiert schon.".into()); return; }
        if let Err(e) = fs::rename(&src, &dst) { ui.set_manage_status(format!("Umbenennen fehlgeschlagen: {e}").into()); return; }

        if let Some(mut cfg) = load_instance_config(&dst) {
            cfg.name = base.clone();
            let _ = save_instance_config(&dst, &cfg);
            with_packs_model(&ui, |vm| {
                if let Some(i) = find_tile_row(vm, name.as_str()) { vm.set_row_data(i, tile_from_config(&cfg)); }
            });
        }
        ui.set_detail_instance_name(base.clone().into()); // die Detailansicht zeigt jetzt den neuen Namen
        ui.set_manage_status(format!("Umbenannt in '{base}'.").into());
    });

    // Export als .zip in den Downloads-Ordner.
    let h = ui.as_weak();
    let hdl = rt.handle().clone();
    ui.on_manage_export(move |name| {
        let ui = h.unwrap();
        let src = launcher_dir().join("instances").join(name.as_str());
        let target_dir = dirs::download_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from("."));
        let stem = sanitize_dir_name(name.as_str());
        let mut out = target_dir.join(format!("{stem}.zip"));
        let mut n = 2;
        while out.exists() { out = target_dir.join(format!("{stem} ({n}).zip")); n += 1; }
        ui.set_manage_status("Exportiere... (bei großen Instanzen kann das dauern)".into());

        let wh = h.clone();
        hdl.spawn(async move {
            let out2 = out.clone();
            let res = tokio::task::spawn_blocking(move || export_instance_zip(&src, &out2))
                .await.unwrap_or_else(|e| Err(format!("Interner Fehler: {e}")));
            let _ = wh.upgrade_in_event_loop(move |ui| match res {
                Ok(bytes) => ui.set_manage_status(format!("Exportiert nach {} ({} MB unkomprimiert).", out.display(), bytes / 1_048_576).into()),
                Err(e) => ui.set_manage_status(format!("Export fehlgeschlagen: {e}").into()),
            });
        });
    });

    // Export als .mrpack in den Downloads-Ordner (Mods per Modrinth-Link, Rest in overrides/).
    let h = ui.as_weak();
    let hdl = rt.handle().clone();
    ui.on_manage_export_mrpack(move |name| {
        let ui = h.unwrap();
        let target_dir = dirs::download_dir().or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from("."));
        let stem = sanitize_dir_name(name.as_str());
        let mut out = target_dir.join(format!("{stem}.mrpack"));
        let mut n = 2;
        while out.exists() { out = target_dir.join(format!("{stem} ({n}).mrpack")); n += 1; }
        ui.set_manage_status("Erstelle .mrpack (Mods werden bei Modrinth nachgeschlagen)...".into());

        let (wh, name) = (h.clone(), name.to_string());
        hdl.spawn(async move {
            let msg = match export_mrpack(&name, &out).await {
                Ok(m) => m,
                Err(e) => { let _ = fs::remove_file(&out); format!("Export fehlgeschlagen: {e}") }
            };
            manage_status_post(&wh, msg);
        });
    });

    // Löschen (die Bestätigung passiert schon in der UI).
    let h = ui.as_weak();
    let run = running.clone();
    ui.on_manage_delete(move |name| {
        let ui = h.unwrap();
        ui.set_manage_confirm_delete(false);
        if is_running(&run, name.as_str()) { ui.set_manage_status("Die Instanz läuft: bitte erst beenden.".into()); return; }
        let dir = launcher_dir().join("instances").join(name.as_str());
        if let Err(e) = fs::remove_dir_all(&dir) { ui.set_manage_status(format!("Löschen fehlgeschlagen: {e}").into()); return; }
        with_packs_model(&ui, |vm| { if let Some(i) = find_tile_row(vm, name.as_str()) { vm.remove(i); } });
        ui.set_manage_popup_visible(false);
        ui.set_selected_panel("Instanzen".into()); // die Detailansicht der gelöschten Instanz verlassen
    });

    // Loader: Check mit gewählter Version bzw. mit der neuesten. Das Ergebnis erscheint im Popup,
    // erst danach kann der Nutzer bestätigen.
    let h = ui.as_weak();
    let hdl = rt.handle().clone();
    let run = running.clone();
    ui.on_manage_loader_check(move |name, target| {
        let ui = h.unwrap();
        if is_running(&run, name.as_str()) { ui.set_manage_status("Die Instanz läuft: bitte erst beenden.".into()); return; }
        ui.set_manage_loader_pending("".into());
        ui.set_manage_loader_report("Prüfe Mods...".into());
        start_loader_check(h.clone(), hdl.clone(), name.to_string(), Some(target.to_string()));
    });

    let h = ui.as_weak();
    let hdl = rt.handle().clone();
    let run = running.clone();
    ui.on_manage_loader_latest(move |name| {
        let ui = h.unwrap();
        if is_running(&run, name.as_str()) { ui.set_manage_status("Die Instanz läuft: bitte erst beenden.".into()); return; }
        ui.set_manage_loader_pending("".into());
        ui.set_manage_loader_report("Ermittle neueste Version und prüfe Mods...".into());
        start_loader_check(h.clone(), hdl.clone(), name.to_string(), None);
    });

    // Bestätigt: Version in die instance.json schreiben. Installiert wird beim nächsten Start.
    let h = ui.as_weak();
    let run = running.clone();
    ui.on_manage_loader_apply(move |name, version| {
        let ui = h.unwrap();
        if is_running(&run, name.as_str()) { ui.set_manage_status("Die Instanz läuft: bitte erst beenden.".into()); return; }
        let dir = launcher_dir().join("instances").join(name.as_str());
        let Some(mut cfg) = load_instance_config(&dir) else { ui.set_manage_status("instance.json fehlt.".into()); return; };
        let (kind, _) = split_loader_id(&cfg.loader);
        cfg.loader = kind.clone(); // alte CurseForge-IDs ("forge-14.23...") dabei bereinigen
        cfg.loader_version = Some(version.to_string());
        if let Err(e) = save_instance_config(&dir, &cfg) { ui.set_manage_status(e.into()); return; }
        with_packs_model(&ui, |vm| {
            if let Some(i) = find_tile_row(vm, name.as_str()) { vm.set_row_data(i, tile_from_config(&cfg)); }
        });
        ui.set_manage_loader_current(format!("{kind} {version}").into());
        ui.set_manage_loader_pending("".into());
        ui.set_manage_loader_report("".into());
        ui.set_manage_status(format!("Loader auf {version} gestellt. Beim nächsten Start wird er installiert.").into());
    });

        // ===================== Modpack-Update (Feature 1) =====================

    // Beim Öffnen des Manage-Popups: Versionsliste laden, falls die Instanz eine Modrinth-Herkunft hat.
    let h = ui.as_weak();
    let hdl = rt.handle().clone();
    ui.on_manage_update_load(move |name| {
        let ui = h.unwrap();
        ui.set_manage_update_versions(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
        ui.set_manage_update_report("".into());
        ui.set_manage_update_pending(false);
        ui.set_manage_update_supported(false);

        let dir = launcher_dir().join("instances").join(name.as_str());
        let Some(cfg) = load_instance_config(&dir) else { return; };
        if cfg.origin_source.as_deref() != Some("Modrinth") { return; }
        let Some(pid) = cfg.origin_project_id.clone() else { return; };
        ui.set_manage_update_supported(true);

        let current = cfg.origin_version_id.clone();
        let wh = h.clone();
        hdl.spawn(async move {
            let Some(versions) = fetch_pack_versions(&pid).await else { return; };
            let labels: Vec<SharedString> = versions.iter().map(|v| {
                let mark = if Some(&v.id) == current.as_ref() { " [installiert]" } else { "" };
                SharedString::from(format!("{}{} ({})", v.version_number, mark, v.game_versions.join(", ")))
            }).collect();
            update_state().lock().unwrap().versions = versions;
            let _ = wh.upgrade_in_event_loop(move |ui| {
                ui.set_manage_update_versions(ModelRc::new(VecModel::from(labels)));
            });
        });
    });

    // "Prüfen": Plan berechnen und als Zusammenfassung anzeigen.
    let h = ui.as_weak();
    let hdl = rt.handle().clone();
    let run = running.clone();
    ui.on_manage_update_check(move |name, index| {
        let ui = h.unwrap();
        if is_running(&run, name.as_str()) { ui.set_manage_status("Die Instanz läuft: bitte erst beenden.".into()); return; }
        let Some(version) = update_state().lock().unwrap().versions.get(index.max(0) as usize).cloned() else {
            ui.set_manage_update_report("Bitte zuerst eine Version wählen.".into());
            return;
        };
        ui.set_manage_update_pending(false);
        ui.set_manage_update_report("Lade Modpack und vergleiche Dateien...".into());

        let (wh, name) = (h.clone(), name.to_string());
        hdl.spawn(async move {
            let post = |text: String, pending: bool| {
                let _ = wh.upgrade_in_event_loop(move |ui| {
                    ui.set_manage_update_report(text.into());
                    ui.set_manage_update_pending(pending);
                });
            };
            match build_update_plan(&name, &version).await {
                Ok(plan) => {
                    let mc_note = if !plan.minecraft_version.is_empty() && plan.minecraft_version != plan.old_mc {
                        format!(" ACHTUNG: Minecraft wechselt {} -> {}. Welten können inkompatibel werden, vorher unter 'Duplizieren' sichern!", plan.old_mc, plan.minecraft_version)
                    } else { String::new() };
                    let snap_note = if plan.had_snapshot { "" } else {
                        " Hinweis: Für diese Instanz gibt es keinen Snapshot der alten Version. Jede abweichende Datei gilt als Konflikt, alte Pack-Mods werden nicht entfernt (Duplikate erscheinen orange)."
                    };
                    let report = format!(
                        "Version {}: {} Config-Dateien automatisch, {} Konflikte (du entscheidest), {} neue Mods, {} entfernte Pack-Mods. saves/ bleibt unberührt.{}{}",
                        version.version_number, plan.auto_new.len(), plan.conflicts.len(),
                        plan.to_download.len(), plan.to_remove.len(), mc_note, snap_note
                    );
                    update_state().lock().unwrap().plan = Some(plan);
                    post(report, true);
                }
                Err(e) => post(format!("Prüfung fehlgeschlagen: {e}"), false),
            }
        });
    });

        // "Update Instanz using .mrpack": lokale .mrpack-Datei einlesen, Plan berechnen und anzeigen.
    // Danach läuft alles über den bestehenden "Update durchführen"-Button (manage_update_apply).
    let h = ui.as_weak();
    let hdl = rt.handle().clone();
    let run = running.clone();
    ui.on_manage_update_from_file(move |name, path_input| {
        let ui = h.unwrap();
        if is_running(&run, name.as_str()) { ui.set_manage_status("Die Instanz läuft: bitte erst beenden.".into()); return; }

        let path = clean_path_input(&path_input);
        if !path.is_file() {
            ui.set_manage_update_pending(false);
            ui.set_manage_update_report(format!("Datei nicht gefunden: {}", path.display()).into());
            return;
        }
        ui.set_manage_update_pending(false);
        ui.set_manage_update_report("Lese .mrpack und vergleiche Dateien...".into());

        let (wh, name) = (h.clone(), name.to_string());
        hdl.spawn(async move {
            let post = |text: String, pending: bool| {
                let _ = wh.upgrade_in_event_loop(move |ui| {
                    ui.set_manage_update_report(text.into());
                    ui.set_manage_update_pending(pending);
                });
            };

            let bytes = match tokio::fs::read(&path).await {
                Ok(b) => b,
                Err(e) => { post(format!("Konnte Datei nicht lesen: {e}"), false); return; }
            };

            // Herkunfts-Version der Instanz beibehalten (leer = wird beim Update nicht überschrieben)
            let version_id = String::new();

            match build_update_plan_from_bytes(&name, bytes, version_id).await {
                Ok(plan) => {
                    let file_label = path.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                    let mc_note = if !plan.minecraft_version.is_empty() && plan.minecraft_version != plan.old_mc {
                        format!(" ACHTUNG: Minecraft wechselt {} -> {}. Welten können inkompatibel werden, vorher unter 'Duplizieren' sichern!", plan.old_mc, plan.minecraft_version)
                    } else { String::new() };
                    let snap_note = if plan.had_snapshot { "" } else {
                        " Hinweis: Für diese Instanz gibt es keinen Snapshot der alten Version. Jede abweichende Datei gilt als Konflikt, alte Pack-Mods werden nicht entfernt (Duplikate erscheinen orange)."
                    };
                    let report = format!(
                        "Lokale Datei {}: {} Config-Dateien automatisch, {} Konflikte (du entscheidest), {} neue Mods, {} entfernte Pack-Mods. saves/ bleibt unberührt.{}{}",
                        file_label, plan.auto_new.len(), plan.conflicts.len(),
                        plan.to_download.len(), plan.to_remove.len(), mc_note, snap_note
                    );
                    update_state().lock().unwrap().plan = Some(plan);
                    post(report, true);
                }
                Err(e) => post(format!("Prüfung fehlgeschlagen: {e}"), false),
            }
        });
    });

    // "Update durchführen": bei Konflikten erst das Popup zeigen, sonst direkt anwenden.
    let h = ui.as_weak();
    let hdl = rt.handle().clone();
    let run = running.clone();
    ui.on_manage_update_apply(move |name| {
        let ui = h.unwrap();
        if is_running(&run, name.as_str()) { ui.set_manage_status("Die Instanz läuft: bitte erst beenden.".into()); return; }
        let conflicts = update_state().lock().unwrap().plan.as_ref().map(|p| p.conflicts.clone());
        let Some(conflicts) = conflicts else { ui.set_manage_update_report("Erst auf 'Prüfen' klicken.".into()); return; };

        ui.set_manage_update_pending(false);
        if conflicts.is_empty() {
            start_update(h.clone(), hdl.clone(), std::collections::HashSet::new());
            return;
        }
        // Konflikt-Popup: das Kompatibilitäts-Popup wird im Modus "update" wiederverwendet.
        // confirmed = true  -> NEUE Datei aus dem Pack nehmen
        // confirmed = false -> MEINE Datei behalten (Standard, damit nichts versehentlich überschrieben wird)
        let actions: Vec<CompatibilityActionUi> = conflicts.into_iter().map(|p| CompatibilityActionUi {
            project_id: p.clone().into(),
            label: p.into(),
            confirmed: false,
        }).collect();
        ui.set_compat_actions(ModelRc::new(VecModel::from(actions)));
        ui.set_compat_conflict_visible(false);
        ui.set_compat_mode("update".into());
        ui.set_compat_popup_visible(true);
    });

    // "Übernehmen" im Konflikt-Popup (Modus "update"): Auswahl auslesen und Update starten.
    let h = ui.as_weak();
    let hdl = rt.handle().clone();
    ui.on_update_confirm(move |_name| {
        let ui = h.unwrap();
        let model = ui.get_compat_actions();
        let keep: std::collections::HashSet<String> = (0..model.row_count())
            .filter_map(|i| model.row_data(i))
            .filter(|a| !a.confirmed) // nicht "neu" = meine behalten
            .map(|a| a.project_id.to_string())
            .collect();
        ui.set_compat_popup_visible(false);
        ui.set_compat_mode("mods".into());
        start_update(h.clone(), hdl.clone(), keep);
    });

    // ===================== Block 4: Doppelte Mods beheben =====================

    // Schritt 1: Vorschläge berechnen und im Kompatibilitäts-Popup zur Bestätigung zeigen.
    // Nichts wird geändert, bevor der Nutzer "Übernehmen" drückt. Ohne Erlaubnis (Misc) passiert gar nichts.
    let h = ui.as_weak();
    ui.on_duplicates_fix(move |name| {
        let ui = h.unwrap();
        let dir = launcher_dir().join("instances").join(name.as_str());
        if !load_misc(&dir).auto_fix_duplicates {
            ui.set_dup_status("Gesperrt: im Misc-Popup erlauben und speichern.".into());
            return;
        }
        let proposals = duplicate_proposals(&dir);
        if proposals.is_empty() {
            ui.set_dup_status("Keine behebbaren Duplikate gefunden (nur Mods aus mods-list.json werden erkannt).".into());
            return;
        }
        // project_id dient hier als Schlüssel = Dateiname (eindeutig); das Umschalten läuft über compat_toggle_action.
        let actions: Vec<CompatibilityActionUi> = proposals.into_iter()
            .map(|(file, label)| CompatibilityActionUi { project_id: file.into(), label: label.into(), confirmed: true })
            .collect();
        ui.set_compat_actions(ModelRc::new(VecModel::from(actions)));
        ui.set_compat_conflict_visible(false);
        ui.set_compat_mode("duplicates".into());
        ui.set_compat_popup_visible(true);
    });

        // Schritt 2: nur die bestätigten Zeilen anwenden (Dateien deaktivieren).
    let h = ui.as_weak();
    ui.on_duplicates_apply(move |name| {
        let ui = h.unwrap();
        let dir = launcher_dir().join("instances").join(name.as_str());
        let model = ui.get_compat_actions();
        let files: Vec<String> = (0..model.row_count())
            .filter_map(|i| model.row_data(i))
            .filter(|a| a.confirmed)
            .map(|a| a.project_id.to_string())
            .collect();
        for f in &files {
            if dir.join("mods").join(f).exists() { toggle_mod_file(&dir, f); }
        }
        ui.set_compat_popup_visible(false);
        ui.set_compat_mode("mods".into());
        ui.set_dup_status(format!("{} Mods deaktiviert.", files.len()).into());
        invalidate_instance_cache(name.as_str());
        ui.set_detail_mod_files(ModelRc::new(VecModel::from(
            list_mod_files_cached(name.as_str(), &dir),
        )));
    });

    // Achievements-Tab: Liste beim Öffnen neu aufbauen
    let h = ui.as_weak();
    ui.on_achievements_open(move || {
        let ui = h.unwrap();
        push_achievements_to_ui(&ui);
    });

    // Gruppe im Achievements-Tab ein-/ausklappen
    let h = ui.as_weak();
    ui.on_achievements_toggle(move |inst| {
        let ui = h.unwrap();
        {
            let mut g = ach_collapsed().lock().unwrap();
            if !g.remove(inst.as_str()) { g.insert(inst.to_string()); }
        }
        push_achievements_to_ui(&ui);
    });

        // =============== NEU: Absturz-Analyse ===============
    // Manueller Button in der Detailansicht (ohne since-Filter: nimmt den neuesten Report)
    let h = ui.as_weak();
    let hdl = rt.handle().clone();
    ui.on_crash_analyze(move |name| {
        hdl.spawn(show_crash_analysis(h.clone(), name.to_string(), None));
    });

    // Verdächtigen Mod deaktivieren (.disabled, also umkehrbar)
    let h = ui.as_weak();
    ui.on_crash_disable_mod(move |instance, file| {
        let ui = h.unwrap();
        let dir = launcher_dir().join("instances").join(instance.as_str());
        if file.ends_with(".disabled") || !dir.join("mods").join(file.as_str()).is_file() {
            ui.set_crash_status("Der Mod ist schon deaktiviert oder nicht mehr vorhanden.".into());
            return;
        }
        toggle_mod_file(&dir, file.as_str());
        invalidate_instance_cache(instance.as_str());
        if ui.get_detail_instance_name() == instance {
            ui.set_detail_mod_files(ModelRc::new(VecModel::from(list_mod_files_cached(instance.as_str(), &dir))));
        }
        ui.set_crash_status(format!("'{file}' wurde deaktiviert. Starte die Instanz erneut.").into());
    });

    // Report (sonst latest.log, sonst Ordner) im System öffnen
    ui.on_crash_open_report(move |instance| {
        let dir = launcher_dir().join("instances").join(instance.as_str());
        let log = dir.join("logs").join("latest.log");
        let target = newest_crash_report_path(&dir, None)
            .or_else(|| if log.is_file() { Some(log.clone()) } else { None })
            .unwrap_or_else(|| dir.clone());
        open_in_file_manager(&target);
    });

    // =============== NEU: Server-Tab ===============
    let h = ui.as_weak();
    ui.on_srv_open(move || {
        let ui = h.unwrap();
        push_servers_to_ui(&ui);
        let sel = ui.get_srv_selected().to_string();
        if !sel.is_empty() { srv_load_selected(&ui, &sel); }
    });

    let h = ui.as_weak();
    ui.on_srv_select(move |name| {
        let ui = h.unwrap();
        ui.set_srv_status("".into());
        srv_load_selected(&ui, name.as_str());
    });

    // Neuen (leeren) Server anlegen. Installiert wird beim ersten Start.
    let h = ui.as_weak();
    ui.on_srv_create(move |name, mc, loader| {
        let ui = h.unwrap();
        let Some(base) = clean_new_name(name.as_str()) else {
            ui.set_srv_status("Bitte einen gültigen Namen eingeben.".into());
            return;
        };
        if mc.is_empty() { ui.set_srv_status("Bitte eine Minecraft-Version wählen.".into()); return; }
        let unique = unique_server_name(&base);
        let _ = fs::create_dir_all(server_dir(&unique));
        let mut all = load_local_servers();
        all.push(LocalServer::new(unique.clone(), mc.to_string(), loader.to_string().to_lowercase()));
        save_local_servers(&all);
        push_servers_to_ui(&ui);
        srv_load_selected(&ui, &unique);
        ui.set_srv_status(format!("Server '{unique}' angelegt. EULA-Haken setzen, speichern, dann Start (installiert beim ersten Start).").into());
    });

    let h = ui.as_weak();
    let hdl = rt.handle().clone();
    ui.on_srv_start(move |name| {
        hdl.spawn(run_local_server(name.to_string(), h.clone()));
    });

    // Sauber stoppen: der Befehl "stop" speichert die Welt (Kill kann Daten kosten)
    let h = ui.as_weak();
    ui.on_srv_stop(move |name| {
        let ui = h.unwrap();
        let sent = server_runtime().lock().unwrap().get(name.as_str())
            .map(|rt| rt.cmd_tx.send("stop".to_string()).is_ok()).unwrap_or(false);
        let msg = if sent { "Stoppe Server (speichert die Welt)..." } else { "Der Server läuft nicht." };
        ui.set_srv_status(msg.into());
    });

    // Notbremse: Prozess hart beenden
    let h = ui.as_weak();
    ui.on_srv_kill(move |name| {
        let ui = h.unwrap();
        let mut g = server_runtime().lock().unwrap();
        match g.get_mut(name.as_str()).and_then(|rt| rt.kill_tx.take()) {
            Some(tx) => { let _ = tx.send(()); ui.set_srv_status("Server wird hart beendet...".into()); }
            None => ui.set_srv_status("Der Server läuft nicht.".into()),
        }
    });

    // Befehl an die Server-Konsole senden (ein führendes "/" wird entfernt)
    let h = ui.as_weak();
    ui.on_srv_command(move |name, text| {
        let ui = h.unwrap();
        let cmd = text.trim().trim_start_matches('/').to_string();
        if cmd.is_empty() { return; }
        let sent = server_runtime().lock().unwrap().get(name.as_str())
            .map(|rt| rt.cmd_tx.send(cmd.clone()).is_ok()).unwrap_or(false);
        if sent {
            server_log_push(name.as_str(), vec![format!("> {cmd}")]);
            srv_refresh_console(&ui, name.as_str());
        } else {
            ui.set_srv_status("Der Server läuft nicht.".into());
        }
    });

    // RAM, EULA und server.properties speichern (wirkt beim nächsten Start)
    let h = ui.as_weak();
    ui.on_srv_save(move |name, ram_mb, eula, port, max, motd, diff, wl, online| {
        let ui = h.unwrap();
        let Ok(port_n) = port.trim().parse::<u16>() else { ui.set_srv_status("Ungültiger Port (1-65535).".into()); return; };
        if port_n == 0 { ui.set_srv_status("Ungültiger Port (1-65535).".into()); return; }
        let Ok(max_n) = max.trim().parse::<u32>() else { ui.set_srv_status("Ungültige Spielerzahl.".into()); return; };

        update_local_server(name.as_str(), |s| { s.ram_mb = ram_mb; s.eula_accepted = eula; });
        let dir = server_dir(name.as_str());
        let _ = fs::create_dir_all(&dir);
        write_props(&dir, &[
            ("server-port", port_n.to_string()),
            ("max-players", max_n.to_string()),
            ("motd", motd.to_string()),
            ("difficulty", diff.to_string()),
            ("white-list", wl.to_string()),
            ("online-mode", online.to_string()),
        ]);
        push_servers_to_ui(&ui); // Port-Konflikt-Warnung neu berechnen
        let running = server_runtime().lock().unwrap().contains_key(name.as_str());
        ui.set_srv_status(if running { "Gespeichert. Gilt nach einem Neustart des Servers.".into() } else { "Gespeichert.".into() });
    });

    let h = ui.as_weak();
    ui.on_srv_delete(move |name| {
        let ui = h.unwrap();
        ui.set_srv_confirm_delete(false);
        if server_runtime().lock().unwrap().contains_key(name.as_str()) {
            ui.set_srv_status("Der Server läuft: bitte erst stoppen.".into());
            return;
        }
        let dir = server_dir(name.as_str());
        if dir.exists() {
            if let Err(e) = fs::remove_dir_all(&dir) { ui.set_srv_status(format!("Löschen fehlgeschlagen: {e}").into()); return; }
        }
        let mut all = load_local_servers();
        all.retain(|s| s.name != name.as_str());
        save_local_servers(&all);
        server_logs().lock().unwrap().remove(name.as_str());
        ui.set_srv_selected("".into());
        push_servers_to_ui(&ui);
        ui.set_srv_status(format!("Server '{name}' gelöscht.").into());
    });

    ui.on_srv_open_folder(move |name| {
        let dir = server_dir(name.as_str());
        if dir.is_dir() { open_in_file_manager(&dir); }
    });

    // Feature 3b: Bei --launch-Argument die Instanz direkt starten
    if let Some(inst) = auto_launch_instance.clone() {
        let ui_wk = ui.as_weak();
        let _ = ui_wk.upgrade_in_event_loop(move |ui| {
            println!("🚀 Auto-Launch via Shortcut: {}", inst);
            ui.invoke_modpack_play(inst.into());
        });
    }

    //ui.on_request_increase_value(move || {
    //    let ui = ui_handle.unwrap();
    //    ui.set_counter(ui.get_counter() + 1);
    //});

    ui.run()?;

    // NEU: Fenster ist zu -> laufende lokale Server sauber stoppen (Welt speichern), max. 45 s warten
    stop_all_local_servers_blocking(std::time::Duration::from_secs(45));

    Ok(())
}