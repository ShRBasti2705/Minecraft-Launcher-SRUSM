// Prevent console window in addition to Slint window in Windows release builds when, e.g., starting the app via file manager. Ignored on other platforms.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{error::Error, fs, io::BufReader};
use std::sync::{Arc, Mutex, OnceLock};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use lyceris::{http::fetch::fetch, install, json::version::manifest::{Type, VersionManifest}, launch, minecraft::{config::{ConfigBuilder, Profile}, loader::Loader}};
use lyceris::minecraft::config::Memory;
use slint::{Image, ModelRc, VecModel, Weak};
use std::rc::Rc;
use fs::File;
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

#[derive(Debug, Serialize, Deserialize)]
struct ModpackJsonData {
    name: String,
    minecraft_version: String,
    loader: String,
    ram_mb: i32,
    // Pfad zur gecachten Icon-Datei (icon_cache/<project_id>.png), falls vorhanden.
    // #[serde(default)] damit bestehende, ältere instance.json-Dateien ohne dieses
    // Feld weiterhin problemlos eingelesen werden können.
    #[serde(default)]
    icon_path: Option<String>,
    // Vom Modpack vorgegebene Loader-Version (z.B. "0.16.9" bei Fabric). None = neueste stabile.
    // #[serde(default)] damit ältere instance.json-Dateien ohne dieses Feld weiter laden.
    #[serde(default)]
    loader_version: Option<String>,
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
}

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
        }
    }
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
#[derive(Deserialize)]
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

fn get_available_java_paths() -> Vec<String> {
    let mut paths = vec!["Launcher Standard".to_string()];
    // Füge hier bekannte Pfade hinzu, falls vorhanden
    if std::path::Path::new("/usr/bin/java").exists() {
        paths.push("/usr/bin/java".to_string());
    }
    paths
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

// Lädt alle verfügbaren Versionen eines Modrinth-Projekts (Modpack oder Mod).
// Neueste zuerst laut Modrinth-API-Reihenfolge.

async fn fetch_pack_versions(project_id: &str) -> Option<Vec<ModrinthVersionInfo>> {
    let url = format!("https://api.modrinth.com/v2/project/{}/version", project_id);
    fetch(url, None).await.ok()
}

// Fragt die icon_url eines Modrinth-Projekts ab (falls nicht schon aus einem
// Suchtreffer bekannt, z.B. beim Bestätigen einer Installation).

async fn fetch_project_icon_url(project_id: &str) -> Option<String> {
    let url = format!("https://api.modrinth.com/v2/project/{}", project_id);
    let project: ModrinthProjectInfo = fetch(url, None).await.ok()?;
    project.icon_url
}


async fn fetch_project_info(project_id: &str) -> Option<ModrinthProjectInfo> {
    let url = format!("https://api.modrinth.com/v2/project/{}", project_id);
    fetch(url, None).await.ok()
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

        // Decode + Resize in spawn_blocking, damit der Tokio-Worker frei bleibt
        // und andere Icon-Downloads parallel laufen koennen.
        let cdir = icon_cache_dir.to_path_buf();
        let cpath = cache_path.clone();
        tokio::task::spawn_blocking(move || -> Option<()> {
            let decoded = image::load_from_memory(&bytes).ok()?.to_rgba8();
            let resized = image::imageops::resize(
                &decoded, 96, 96,
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

// Lädt ein Icon von Platte als slint::Image. Muss auf dem UI-Thread aufgerufen
// werden (analog zu allen anderen Stellen, an denen wir erst nach dem
// Thread-Wechsel Slint-Typen bauen). Fehlt der Pfad oder schlägt das Laden
// fehl, kommt einfach ein leeres Bild zurück statt eines Absturzes.

fn load_icon(path: &Option<PathBuf>) -> Image {
    match path {
        Some(p) => Image::load_from_path(p).unwrap_or_default(),
        None => Image::default(),
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

    let index = tokio::task::spawn_blocking(move || -> Result<MrpackIndex, String> {
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
                let mut out_file = File::create(&out_path)
                    .map_err(|e| format!("Datei anlegen: {e}"))?;
                std::io::copy(&mut entry, &mut out_file)
                    .map_err(|e| format!("Datei schreiben: {e}"))?;
            }
        }

        Ok(index)
    })
    .await
    .map_err(|e| format!("Interner Fehler beim Entpacken: {e}"))??;

    // 3. Mod-Dateien einzeln nachladen (async, wie bisher)
    let total = index.files.len();
    let mut mods_list: Vec<ModListEntry> = Vec::new();

    for (i, file) in index.files.iter().enumerate() {
        let Some(url) = file.downloads.first() else { continue; };
        report_progress(ui_handle, format!("Lade Mod {}/{}: {}", i + 1, total, file.path));
        let bytes = download_bytes(url).await?;
        let out_path = instance_dir.join(&file.path);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("Ordner anlegen: {e}"))?;
        }
        fs::write(&out_path, &bytes)
            .map_err(|e| format!("Datei schreiben ({}): {e}", file.path))?;

        if let Some(project_id) = extract_modrinth_project_id(url) {
            let filename = file.path.split('/').last().unwrap_or(&file.path).to_string();
            mods_list.push(ModListEntry { filename, project_id });
        }
    }

    save_mods_list(instance_dir, &mods_list);
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

    let total = files.len();
    for (i, file) in files.iter().enumerate() {
        report_progress(
            ui_handle,
            format!("Lade Datei {}/{}: {}", i + 1, total, file.filename),
        );

        let bytes = download_bytes(&file.url).await?;
        fs::write(mods_dir.join(&file.filename), &bytes)
            .map_err(|e| format!("Konnte Datei nicht schreiben ({}): {e}", file.filename))?;
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
            ModFileEntry { filename, display_name, enabled, icon_path, duplicate: false, dup_total: 0 }
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
    let mut mods_list: Vec<ModListEntry> = Vec::new();

    for (i, file) in cf_files.iter().enumerate() {
        report_progress(ui_handle, format!("Lade Mod {}/{} herunter...", i + 1, total));
        match download_cf_file(file.fileID).await {
            Ok((filename, bytes)) => {
                fs::write(mods_dir.join(&filename), &bytes)
                    .map_err(|e| format!("Mod speichern fehlgeschlagen: {e}"))?;
                mods_list.push(ModListEntry {
                    filename: filename.clone(),
                    project_id: file.projectID.to_string(),
                });
            }
            Err(e) => {
                println!("⚠️ Mod-Download übersprungen (Projekt-ID: {}, Datei-ID: {}): {}",
                    file.projectID, file.fileID, e);
            }
        }
    }

    save_mods_list(instance_dir, &mods_list);
    Ok((mc_version, loader))
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

    // NEU: Flag setzen, damit parallel_limit() sich drosselt
    MC_RUNNING.store(true, AtomicOrdering::Relaxed);

    tokio::select! {
        result = child.wait() => match result {
            Ok(status) => println!("✅ Minecraft beendet: {status}"),
            Err(e) => println!("❌ Fehler beim Warten auf Minecraft: {e}"),
        },
        Ok(()) = rx => {
            println!("🛑 Force Stop: {name}");
            let _ = child.kill().await;
        }
    }

    // NEU: Flag wieder runter
    MC_RUNNING.store(false, AtomicOrdering::Relaxed);

    add_playtime_secs(&launcher_dir().join("instances").join(&name), started.elapsed().as_secs());
    let _ = ui_handle.upgrade_in_event_loop(|ui| { ui.set_playtime_tick(ui.get_playtime_tick() + 1); });

    running.lock().unwrap().remove(&name);
    refresh_running_ui(&ui_handle, &running);
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
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Playtime { total_secs: u64 }

fn load_playtime_secs(instance_dir: &Path) -> u64 {
    let Ok(file) = File::open(instance_dir.join("playtime.json")) else { return 0; };
    serde_json::from_reader::<_, Playtime>(BufReader::new(file)).map(|p| p.total_secs).unwrap_or(0)
}

fn add_playtime_secs(instance_dir: &Path, add: u64) {
    let p = Playtime { total_secs: load_playtime_secs(instance_dir) + add };
    if let Ok(json) = serde_json::to_string_pretty(&p) {
        let _ = fs::write(instance_dir.join("playtime.json"), json);
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
            if java_major == 21 { vec!["-XX:+UseZGC", "-XX:+ZGenerational"] } else { vec!["-XX:+UseZGC"] }
        }
        _ => vec![],
    };
    flags.into_iter().map(String::from).collect()
}

// Auto-Vorschlag: geht bekannte Fälle von oben nach unten durch; der erste passende gewinnt.
// Liefert (Preset-id, Begründungstext).

fn suggest_jvm_preset(java_major: u32, mods: usize, ram_mb: i32, cores: usize) -> (&'static str, String) {
    if ram_mb < 3072 {
        return ("none", format!("Vorschlag: Standard. Nur {ram_mb} MB RAM sind zugewiesen, bei so wenig Speicher bringen GC-Flags kaum etwas. Erhöhe lieber zuerst den RAM."));
    }
    if java_major <= 8 {
        if mods >= 100 && ram_mb >= 4096 {
            return ("aikar_client", format!("Vorschlag: Aikar's Flags. Diese Instanz läuft mit Java 8 (Minecraft bis 1.16), hat {mods} Mods und {ram_mb} MB RAM. Java 8 startet sonst mit dem Parallel-GC, was bei großen Packs zu längeren Pausen führen kann. Ob es bei dir spürbar hilft, hängt vom Rechner ab: einfach testen."));
        }
        return ("mojang_default", format!("Vorschlag: Mojang-Standard. Java 8 (Minecraft bis 1.16) nutzt ohne Flags den Parallel-GC. G1 mit kurzer Ziel-Pause ist bei {mods} Mods und {ram_mb} MB RAM meist die ruhigere Wahl. Der Unterschied ist bei kleinen Packs gering."));
    }
    if mods < 30 && ram_mb <= 4096 {
        return ("none", format!("Vorschlag: Standard. Mit {mods} Mods und {ram_mb} MB RAM ist das Pack klein; zusätzliche Flags bringen hier selten etwas."));
    }
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
    let (suggested_id, suggestion) = suggest_jvm_preset(java_major, count_mods(&instance_dir), ram_mb, cores);

    let presets: Vec<PresetUi> = JVM_PRESETS.iter().map(|p| PresetUi {
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

// Quick-Connect-Zeile der Detailansicht.

fn push_quick_to_ui(ui: &AppWindow, instance_name: &str) {
    let instance_dir = launcher_dir().join("instances").join(instance_name);
    let misc = load_misc(&instance_dir);
    let mc = load_instance_config(&instance_dir).map(|c| c.minecraft_version).unwrap_or_default();
    ui.set_detail_quick_enabled(misc.quick_enabled);
    ui.set_detail_quick_server(misc.quick_server.into());
    ui.set_detail_quick_world(misc.quick_world.into());
    ui.set_detail_quick_supported(supports_quick_play(&mc));
    ui.set_misc_auto_fix_dupes(misc.auto_fix_duplicates);
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

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dst.join(entry.file_name()));
        if entry.file_type()?.is_dir() { copy_dir_recursive(&from, &to)?; } else { fs::copy(&from, &to)?; }
    }
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

static MC_RUNNING: AtomicBool = AtomicBool::new(false);

/// Wie viele parallele Aufgaben dürfen wir starten?
/// Während MC läuft deutlich weniger, um dessen Ressourcen nicht zu stehlen.
fn parallel_limit(cap: usize) -> usize {
    if MC_RUNNING.load(AtomicOrdering::Relaxed) {
        cap.min(2)
    } else {
        cap
    }
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
    #[cfg(target_os = "linux")]
    {
        if let Some(glfw) = find_system_glfw() {
            let flag = format!("-Dorg.lwjgl.glfw.libname={glfw}");
            let value = match std::env::var("JAVA_TOOL_OPTIONS") {
                Ok(existing) if !existing.is_empty() => format!("{existing} {flag}"),
                _ => flag,
            };
            unsafe { std::env::set_var("JAVA_TOOL_OPTIONS", value); }
        }
    }

    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
    let workers = cores.clamp(2, 4);
    println!("🖥️  Verwende {} Tokio-Worker ({} logische Kerne erkannt)", workers, cores);
    let rt = Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()?;

    let handle = rt.handle().clone();

    let ui = AppWindow::new()?;

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
    ui.set_java_path(settings.java_path.clone().into());
    ui.set_default_username(settings.default_username.clone().into());
    ui.set_close_after_launch(settings.close_after_launch);
    ui.set_open_on_startup(settings.open_on_startup);
    ui.set_show_snapshots(settings.show_snapshots);
    ui.set_items_per_page(settings.items_per_page);
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
            let limit = parallel_limit(8);
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
                    let _ = scan_ui.upgrade_in_event_loop(move |ui| {
                        with_packs_model(&ui, |vm| vm.push(tile_from_config(&cfg)));
                    });
                }
            }
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
            name: data.name.into(),
            loader: data.loader.into(),
            minecraft_version: data.minecraft_version.into(),
            ram_mb: data.ram_mb,
            icon: Image::default(),
        });
    });

    // Play Instance
        // Play Instance
    ui.on_modpack_play(move |name| {
        let _ui = modpack_play_ui_handle.unwrap();
        let handle = handle.clone();
        let ui_handle = ui_handle.clone();
        let launcherDir = dirs::data_local_dir()
            .expect("kein Local Data Dir")
            .join("srusm");
        
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
            // FIX 1: overrides.username durch overrides.account_id ersetzt
            println!("   └─ Overrides aktiviert (RAM: {} MB, Account: {})", overrides.ram_mb, overrides.account_id);
        } else {
            println!("   └─ Overrides deaktiviert, nutze globale Settings");
        }

        println!("💾 [3/7] Berechne effektiven RAM...");
        let settings = load_settings(&launcherDir);
        let effective_ram_mb = if overrides.enabled { overrides.ram_mb } else { settings.default_ram_mb };
        println!("   └─ Effektiver RAM: {} MB", effective_ram_mb);

        println!("👤 [4/7] Bestimme Authentifizierung...");
        // FIX 2: Die Accounts müssen hier explizit geladen werden, bevor wir sie nutzen!
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
            Some(acc) => "Offline",
            None => "Offline (Fallback: TestUser)",
        };
        println!("   └─ Auth-Typ: {}", auth_type);
        
        let _launcher_dir = launcherDir.clone();
        let ui_handle = ui_handle.clone();
        let handle = handle.clone();
        let name = name.to_string();

        let running = running_play.clone(); // wandert in den async-Block
        
        handle.spawn(async move {
            println!("🚀 [5/7] Starte Installation/Setup...");
            
            // Auth bestimmen
            let auth = match selected_account {
                Some(acc) if acc.kind == "microsoft" => match microsoft_auth_for(&launcherDir, acc).await {
                    Ok(auth) => auth,
                    Err(e) => {
                        println!("❌ Microsoft-Anmeldung fehlgeschlagen: {e}");
                        //push_debug_log(&ui_handle, format!("Microsoft-Anmeldung fehlgeschlagen: {e}"));
                        return;
                    }
                },
                Some(acc) => lyceris::AuthMethod::Offline { username: acc.username, uuid: None },
                None => lyceris::AuthMethod::Offline { username: "TestUser".to_string(), uuid: None },
            };

                        // --- Misc-Einstellungen dieser Instanz (misc.json) ---
            let misc = load_misc(&instance_dir);
            let java_major = java_major_for(&config.minecraft_version);
            let extra_jvm_args: Vec<String> = collect_jvm_args(&misc, java_major);   // System-GLFW + Preset + freie Args
            let extra_game_args: Vec<String> = collect_game_args(&misc, &config.minecraft_version); // Quick Play
            let extra_env: Vec<(String, String)> = misc.env_vars.iter().map(|v| (v.name.clone(), v.value.clone())).collect();
            let active_wrappers: Vec<String> = misc.wrappers.clone();

            // Kontrollausgabe, damit du siehst, was übergeben WERDEN soll (und die Variablen genutzt sind).
            println!("🧩 Misc: JVM-Args {:?}", extra_jvm_args);
            println!("🧩 Misc: Spiel-Args {:?}", extra_game_args);
            println!("🧩 Misc: Env {:?}, Wrapper {:?}", extra_env, active_wrappers);

            // TODO(lyceris) 1: extra_jvm_args an den ConfigBuilder übergeben. Er hat dafür eine
            //   Builder-Methode `custom_java_args(Vec<String>)`, die launch() an den Java-Befehl hängt.
            //   Wenn das läuft: den JAVA_TOOL_OPTIONS-Block in main() löschen (die GLFW-Flag kommt
            //   dann pro Instanz aus collect_jvm_args, erst dann wirkt der Schalter "System-GLFW aus").
            // TODO(lyceris) 2: extra_game_args analog über `custom_args(Vec<String>)` (Quick Play).
            // TODO(lyceris) 3: extra_env und active_wrappers: in launch() ist kein Hook dafür sichtbar. Möglichkeiten:
            //   (a) Env vor launch() im Launcher-Prozess setzen: wird vererbt, gilt aber global und kann
            //       sich bei parallel laufenden Instanzen überschneiden (set_var ist in Multithread-Code unsafe).
            //   (b) Falls Config/launch() den Java-Pfad überschreiben lässt: ein kleines Start-Skript als
            //       "java" eintragen, das Env setzt und Wrapper (gamemoderun ...) vor das echte java stellt.
            //   (c) den Prozess-Start von launch() nachbauen (größerer Umbau, vorher mit mir besprechen).
            
            let builder = ConfigBuilder::new(
                &launcherDir.join("shared"),
                config.minecraft_version.clone(),
                auth,
            ).runtime_dir(launcherDir.join("shared/runtime"))
            .memory(Memory::Megabyte(effective_ram_mb as u64))
            .profile(Profile::new(name.to_string(), launcherDir.clone().join("instances")));
            
            let emitter = Emitter::default();
            emitter
                .on(Event::Console, |line: String| {
                    println!("[MC] {line}");
                })
                .await;
            
            if config.loader == "none" {
                println!("📦 [6/7] Installiere Vanilla Minecraft...");
                let cfg = builder.build();
                if let Err(e) = install(&cfg, None).await {
                    println!("❌ Install-Fehler: {:?}", e);
                    return;
                }
                
                println!("▶️  [7/7] Starte Minecraft...");
                match launch(&cfg, Some(&emitter)).await {
                    // Prozess läuft: in die Leiste eintragen und warten (oder killen)
                    Ok(child) => run_and_track(child, name.clone(), running.clone(), ui_handle.clone()).await,
                    Err(e) => {
                        println!("❌ Start-Fehler: {:?}", e);
                        return;
                    }
                }
            } else {
                let (loader_kind, id_version) = split_loader_id(&config.loader);
                
                println!("🔧 [6/7] Bestimme Loader-Version...");
                let version = match config.loader_version.clone().or(id_version) {
                    Some(v) => {
                        println!("   └─ Verwende gespeicherte Version: {}", v);
                        v
                    },
                    None => {
                        println!("   └─ Suche neueste stabile Version für {}...", loader_kind);
                        let Some(v) = latest_loader_version(&loader_kind, &config.minecraft_version).await else {
                            println!("❌ Loader-Version nicht gefunden");
                            return;
                        };
                        println!("   └─ Gefunden: {}", v);
                        v
                    }
                };
                
                println!("   └─ Nutze {} {}", loader_kind, version);

                let loader_version_str = version.clone();
                
                let Some(loader) = make_loader(&loader_kind, version) else {
                    println!("❌ Unbekannter Loader: {}", loader_kind);
                    return;
                };
                
                let cfg = builder.loader(loader).build();
                
                println!("📦 Installiere {} + {}...", config.minecraft_version, loader_kind);
                if let Err(e) = install(&cfg, None).await {
                    println!("⚠️ Install-Fehler: {:?} || Versuche dennoch Fortzufahren", e);

                    // Forge < 1.13 liefert "minecraftArguments" statt "arguments" und bringt lyceris
                    // beim Parsen zum Absturz. Die Datei liegt jetzt (nach dem Download) im Cache,
                    // also konvertieren wir sie und versuchen es genau EINMAL erneut.
                    if loader_kind == "forge" {
                        println!("🔧 Patche Forge version.json und versuche es erneut...");
                        patch_forge_version_json(&launcherDir, &config.minecraft_version, &loader_version_str);

                        if let Err(e2) = install(&cfg, None).await {
                            println!("❌ Install-Fehler nach Patch: {:?}", e2);
                            return;
                        }
                    } else {
                        return;
                    }
                }

                // Forge <= 1.12.2: Tweaker-Argumente in die Start-JSON schreiben (siehe Funktion).
                // Bei jedem Start, weil install() die Datei neu schreiben kann.
                if loader_kind == "forge" {
                    patch_forge_launch_args(&launcherDir, &config.minecraft_version, &loader_version_str);
                }
                
                println!("▶️  [7/7] Starte Minecraft...");
                match launch(&cfg, Some(&emitter)).await {
                    // Prozess läuft: in die Leiste eintragen und warten (oder killen)
                    Ok(child) => run_and_track(child, name.clone(), running.clone(), ui_handle.clone()).await,
                    Err(e) => {
                        println!("❌ Start-Fehler: {:?}", e);
                        return;
                    }
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
    ui.on_save_settings(move |ram, java_path, username, close_after_launch, open_on_startup, show_snapshots, items_per_page, debug_ui| {
        let _ui = save_settings_ui_handle.unwrap();

        let settings = LauncherSettings {
            default_ram_mb: ram,
            java_path: java_path.to_string(),
            default_username: username.to_string(),
            close_after_launch,
            open_on_startup,
            show_snapshots,
            items_per_page,
        };

        let launcherDir = dirs::data_local_dir()
            .expect("kein Local Data Dir")
            .join("srusm");

        let json = serde_json::to_string_pretty(&settings).expect("Error Making Settings json");
        fs::write(launcherDir.join("settings.json"), json).expect("Error Writing Settings File");

        println!("Settings gespeichert: {:?}", settings);
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

            // Sortieren: Zuerst nach Score, dann alphabetisch
            packs_with_urls.sort_by(|a, b| {
                b.score.cmp(&a.score)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });

            // Limit anwenden (z.B. nur Top 48 behalten, um nicht unnötig 100 Icons zu laden)
            if packs_with_urls.len() > limit as usize {
                println!("✂️ Schneide von {} auf Top {} Ergebnisse ab", packs_with_urls.len(), limit);
                packs_with_urls.truncate(limit as usize);
            }

            println!("📥 Lade {} Icons parallel im Hintergrund...", packs_with_urls.len());

            // ── Phase A: Downloads parallel ─────────────────────────────────
            let dl_sem = Arc::new(tokio::sync::Semaphore::new(32));
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
            let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2);
            let decode_limit = parallel_limit(cores);
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
                        project_id: pack.project_id.into(),
                        name: pack.name.into(),
                        summary: pack.summary.into(),
                        author: pack.author.into(),
                        icon,
                        source: pack.source.into(),
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

                let instance_name = format!("{} ({})", pack_name, cf_file.fileName.replace(".zip", "").replace(".mrpack", ""));
                let instance_dir = launcherDir.join("instances").join(&instance_name);
                fs::create_dir_all(&instance_dir).expect("Konnte Instanz-Ordner nicht anlegen");

                report_progress(&ui_handle, format!("Lade CF Modpack: {}", cf_file.fileName));
                
                let install_result = match download_bytes(&cf_file.downloadUrl).await {
                    Ok(bytes) => install_cf_modpack(&instance_dir, bytes, &ui_handle).await,
                    Err(e) => Err(e),
                };

                if let Err(e) = install_result {
                    println!("Installation fehlgeschlagen: {e}");
                    let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                        ui.set_install_popup_status(SharedString::from(format!("Fehler: {e}")));
                        ui.set_install_popup_installing(false);
                    });
                    return;
                }

                // install_cf_modpack gibt (mc_version, loader) zurück!
                // install_cf_modpack gibt (mc_version, "fabric-0.16.9") zurück -> zerlegen
                let (minecraft_version, full_loader_id) = install_result.unwrap();
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

                let primary_file = selected_version.files.iter().find(|f| f.primary).or_else(|| selected_version.files.first()).cloned();

                // Loader-Version aus dem .mrpack lesen, BEVOR es installiert wird.
                // Bleibt None, wenn kein .mrpack vorliegt oder die Version nicht drinsteht.
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
                        download_files_direct(&instance_dir, &selected_version.files, &ui_handle).await.map(|_| ())
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
                                name: data.name.into(),
                                loader: data.loader.into(),
                                minecraft_version: data.minecraft_version.into(),
                                ram_mb: data.ram_mb,
                                icon: Image::default(),
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

        // Setze die Auswahl im Dropdown. Falls der gespeicherte Pfad nicht mehr in der Liste ist, fallback auf Standard.
        let java_paths_str = get_available_java_paths();
        let java_paths: Vec<SharedString> = java_paths_str.iter().map(|s| SharedString::from(s.clone())).collect();
        let java_sel = if java_paths_str.contains(&overrides.java_path) {
            SharedString::from(overrides.java_path)
        } else {
            SharedString::from("Launcher Standard")
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
        push_quick_to_ui(&ui, name.as_str()); // Quick-Connect-Zeile füllen
        ui.set_selected_panel("Instance Detail".into());

        println!("📂 Detailansicht für '{}' geöffnet. Prüfe auf fehlende Icons...", name);

        // 2. NEU: Hintergrund-Task zum Nachladen fehlender Icons (nur EIN UI-Update am Ende)
        let bg_ui_handle = open_details_ui_handle.clone();
        let bg_instance_dir = instance_dir.clone();
        let bg_launcher_dir = launcherDir.clone();
        let bg_handle = open_details_bg_handle.clone();

        let bg_instance_name = name.to_string();

        bg_handle.spawn(async move {
            let cache_dir = bg_launcher_dir.join("icon_cache");
            let mut icons = load_mod_icons(&bg_instance_dir);
            
            // Aktuelle Mods und Liste einlesen
            let current_mods = list_mod_files_raw(&bg_instance_dir);
            let mods_list = load_mods_list(&bg_instance_dir);

            // Sammle alle fehlenden Icons, die wir herunterladen müssen
            let mut missing_icons: Vec<(String, String)> = Vec::new(); // (project_id, display_name)
            for entry in &current_mods {
                if entry.icon_path.is_some() {
                    continue; // Bereits vorhanden
                }
                if let Some(list_entry) = mods_list.iter().find(|m| m.filename == entry.display_name) {
                    missing_icons.push((list_entry.project_id.clone(), entry.display_name.clone()));
                }
            }

            if !missing_icons.is_empty() {
                println!("📥 Starte Hintergrund-Download von {} fehlenden Icons...", missing_icons.len());
                
                let mut new_icons_found = 0;

                // Lade Icons nacheinander im Hintergrund ab.
                // Das frisst weniger Leistung (keine Netzwerk/CPU-Spikes durch Parallel-Downloads),
                // blockiert aber NICHT die UI, da es in einem eigenen Task läuft.
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

                // Wenn wir neue Icons haben, speichern und die UI *einmal* aktualisieren
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
                        let items: Vec<SharedString> = conflict_names.into_iter().map(SharedString::from).collect();
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
                if let Err(e) = install_cf_modpack(&instance_dir, bytes, &ui_handle).await {
                    let _ = fs::remove_dir_all(&instance_dir);
                    return Err(e);
                }

                let (loader_kind, loader_version) = split_loader_id(&loader);

                Ok(ModpackJsonData {
                    name: instance_name,
                    minecraft_version: mc_version,
                    loader: loader_kind,
                    ram_mb: 2048,
                    icon_path: None,
                    loader_version,
                })
            }.await;

            match result {
                Ok(data) => {
                    let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                        let model = ui.get_packs();
                        if let Some(vec_model) = model.as_any().downcast_ref::<VecModel<ModpackInfo>>() {
                            vec_model.push(ModpackInfo {
                                name: data.name.into(),
                                loader: data.loader.into(),
                                minecraft_version: data.minecraft_version.into(),
                                ram_mb: data.ram_mb,
                                icon: Image::default(),
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

    // Quick-Connect (Detailansicht).
    let h = ui.as_weak();
    ui.on_detail_save_quick(move |name, enabled, server, world| {
        let _ui = h.unwrap();
        let dir = launcher_dir().join("instances").join(name.as_str());
        let mut misc = load_misc(&dir);
        misc.quick_enabled = enabled;
        misc.quick_server = server.trim().to_string();
        misc.quick_world = world.trim().to_string();
        save_misc(&dir, &misc);
        println!("Quick-Connect für '{}' gespeichert.", name);
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

    //ui.on_request_increase_value(move || {
    //    let ui = ui_handle.unwrap();
    //    ui.set_counter(ui.get_counter() + 1);
    //});

    ui.run()?;

    Ok(())
}