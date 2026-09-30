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
    #[serde(default)]
    debug_ui: bool,
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
            debug_ui: false,
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
    java_path: String,
    username: String,
}

impl Default for InstanceOverrides {
    fn default() -> Self {
        Self {
            enabled: false,
            ram_mb: 2048,
            java_path: String::new(),
            username: "TestUser".to_string(),
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
async fn cache_icon(icon_cache_dir: &Path, key: &str, icon_url: &Option<String>) -> Option<PathBuf> {
    let url = icon_url.as_ref()?;
    let cache_path = icon_cache_dir.join(format!("{key}.png"));

    if !cache_path.is_file() {
        let bytes = download_bytes(url).await.ok()?;

        // WICHTIG: image:: (die Crate), nicht slint::Image!
        let decoded = image::load_from_memory(&bytes).ok()?;
        fs::create_dir_all(icon_cache_dir).ok()?;
        decoded
            .save_with_format(&cache_path, image::ImageFormat::Png)
            .ok()?;
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
fn push_debug_log(ui_handle: &Weak<AppWindow>, message: String) {
    let _ = ui_handle.upgrade_in_event_loop(move |ui| {
        if !ui.get_debug_ui() { return; }
        let model = ui.get_debug_log();
        let mut items: Vec<SharedString> = (0..model.row_count())
            .filter_map(|i| model.row_data(i))
            .collect();
        items.push(message.into());
        if items.len() > 200 { items.remove(0); } // Ringpuffer, kein unbegrenztes Wachstum
        ui.set_debug_log(ModelRc::new(VecModel::from(items)));
    });
}

// Entpackt ein .mrpack-Archiv (Zip) in den Instanz-Ordner:
// - "overrides/" bzw. "client-overrides/" werden 1:1 in den Instanz-Ordner kopiert
//   (Configs, Resourcepacks, Shader etc., die im Pack mitgeliefert werden)
// - "modrinth.index.json" listet die eigentlichen Mod-Jars, die wir einzeln
//   nachladen und unter ihrem "path" (meist "mods/xyz.jar") ablegen
async fn install_mrpack(
    instance_dir: &PathBuf,
    mrpack_bytes: Vec<u8>,
    ui_handle: &Weak<AppWindow>,
) -> Result<(), String> {
    let cursor = Cursor::new(mrpack_bytes);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| format!("Konnte .mrpack nicht als Zip öffnen: {e}"))?;

    // 1. modrinth.index.json einlesen, dort steht drin welche Mods geladen werden müssen
    let index: MrpackIndex = {
        let mut index_file = archive
            .by_name("modrinth.index.json")
            .map_err(|e| format!("modrinth.index.json fehlt im .mrpack: {e}"))?;
        let mut contents = String::new();
        index_file
            .read_to_string(&mut contents)
            .map_err(|e| format!("Konnte modrinth.index.json nicht lesen: {e}"))?;
        serde_json::from_str(&contents)
            .map_err(|e| format!("Konnte modrinth.index.json nicht parsen: {e}"))?
    };

    // 2. overrides/ direkt aus dem Zip-Archiv in den Instanz-Ordner entpacken
    report_progress(ui_handle, "Entpacke Overrides...".to_string());
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| format!("Zip-Fehler: {e}"))?;
        let name = entry.name().to_string();

        let Some(rel_path) = name
            .strip_prefix("overrides/")
            .or_else(|| name.strip_prefix("client-overrides/"))
        else {
            continue; // kein Override-Eintrag, ignorieren (z.B. modrinth.index.json selbst)
        };

        if rel_path.is_empty() {
            continue; // der Ordner-Eintrag "overrides/" selbst
        }

        let out_path = instance_dir.join(rel_path);

        if entry.is_dir() {
            fs::create_dir_all(&out_path).map_err(|e| format!("Konnte Ordner nicht anlegen: {e}"))?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("Konnte Ordner nicht anlegen: {e}"))?;
            }
            let mut out_file =
                File::create(&out_path).map_err(|e| format!("Konnte Datei nicht anlegen: {e}"))?;
            std::io::copy(&mut entry, &mut out_file)
                .map_err(|e| format!("Konnte Datei nicht schreiben: {e}"))?;
        }
    }

        // 3. Die in modrinth.index.json referenzierten Mod-Dateien einzeln nachladen
    // und gleichzeitig die mods-list.json aufbauen
    let total = index.files.len();
    let mut mods_list: Vec<ModListEntry> = Vec::new();

    for (i, file) in index.files.iter().enumerate() {
        let Some(url) = file.downloads.first() else {
            continue;
        };
        report_progress(
            ui_handle,
            format!("Lade Mod {}/{}: {}", i + 1, total, file.path),
        );
        let bytes = download_bytes(url).await?;
        let out_path = instance_dir.join(&file.path);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("Konnte Ordner nicht anlegen: {e}"))?;
        }
        fs::write(&out_path, &bytes)
            .map_err(|e| format!("Konnte Datei nicht schreiben ({}): {e}", file.path))?;

        // NEU: Versuche, die Project ID aus der URL zu extrahieren
        if let Some(project_id) = extract_modrinth_project_id(url) {
            // file.path ist z.B. "mods/sodium.jar". Wir wollen nur den Dateinamen.
            let filename = file.path.split('/').last().unwrap_or(&file.path).to_string();
            mods_list.push(ModListEntry {
                filename,
                project_id,
            });
        }
    }
    
    // NEU: mods-list.json für diese Instanz speichern
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
            push_debug_log(ui_handle, "Resolver: Zustand bereits versucht, breche ab.".to_string());
            return Err(state);
        }
        tried.insert(signature);

        let problems = find_problems(&state);
        if problems.is_empty() {
            push_debug_log(ui_handle, format!("Resolver: Lösung gefunden nach {} Durchlauf/Durchläufen.", iteration + 1));
            return Ok(state);
        }

        push_debug_log(ui_handle, format!("Resolver Durchlauf {}: {} Problem(e) gefunden.", iteration + 1, problems.len()));
        
        // Erst versuchen, ein "braucht andere Version"-Problem zu fixen (billiger als Konflikt-Suche).
        let mut fixed = false;
        let mut added_missing = false;
        for problem in &problems {
            if let Problem::MissingRequired { project_id, version_id, requested_by } = problem {
                push_debug_log(ui_handle, format!("Fehlende Abhängigkeit: {} (benötigt von {}).", project_id, requested_by));

                let version = if let Some(vid) = version_id {
                    fetch_version_by_id(vid).await
                } else {
                    fetch_pack_versions(project_id).await
                        .and_then(|vs| pick_best_matching_version(&vs, mc_version, loader_kind).cloned())
                };

                let Some(version) = version else {
                    push_debug_log(ui_handle, format!("Konnte keine Version für {} laden.", project_id));
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
                push_debug_log(ui_handle, format!("Versionskonflikt: {} benötigt {} in Version {}.", requested_by, project_id, required_version_id));
                
                // SCHRITT 1: Zuerst versuchen, den anfordernden Mod (z.B. Iris) herunterzustufen,
                // damit er mit der AKTUELLEN Version von project_id (z.B. Sodium) kompatibel ist.
                // Das verhindert, dass wir Sodium upgraden und dadurch Konflikte mit Voxy erzeugen.
                if let Some(alternate) = find_alternate_compatible_version(requested_by, project_id, &state, mc_version, loader_kind).await {
                    push_debug_log(ui_handle, format!("Lösung: Downgrade von {} auf {}, um mit {} kompatibel zu sein.", requested_by, alternate.version_number, project_id));
                    if let Some(existing) = state.get_mut(requested_by) {
                        existing.version = alternate;
                        fixed = true;
                        break; // Erfolgreich gefixt, nächste Resolver-Runde starten
                    }
                } else {
                    // SCHRITT 2 (Fallback): Wenn kein kompatibles Downgrade für requested_by gefunden wurde,
                    // versuchen wir immer noch, project_id auf die benötigte Version zu setzen.
                    push_debug_log(ui_handle, format!("Kein kompatibles Downgrade für {} gefunden. Versuche Upgrade von {}.", requested_by, project_id));
                    if let Some(version) = fetch_version_by_id(required_version_id).await {
                        if let Some(existing) = state.get_mut(project_id) {
                            existing.version = version;
                            fixed = true;
                            break;
                        }
                    } else {
                        push_debug_log(ui_handle, format!("Konnte Version {} nicht laden.", required_version_id));
                    }
                }
            }
        }
        if fixed { continue; }
        if added_missing { continue; }

        push_debug_log(ui_handle, format!("Resolver Durchlauf {}: {} Problem(e) gefunden.", iteration + 1, problems.len()));

        let mut resolved = false;
        for problem in &problems {
            if let Problem::Incompatible { a, b } = problem {
                for to_change in [a, b] {
                    let other = if to_change == a { b } else { a };
                    push_debug_log(ui_handle, format!(
                        "Inkompatibilität: {} <-> {}. Suche Alternative für {}.",
                        a, b, to_change
                    ));
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
                push_debug_log(ui_handle, format!(
                    "Resolver: versuche Downgrade von {} ({})...", trouble_name, trouble_id
                ));
                if let Some(older) =
                    downgrade_project(&trouble_id, &state, mc_version, loader_kind).await
                {
                    if let Some(existing) = state.get_mut(&trouble_id) {
                        existing.version = older;
                        continue; // neuen Durchlauf mit der älteren Version starten
                    }
                }
                push_debug_log(ui_handle, format!("Resolver: {} hat keine ältere Version mehr.", trouble_name));
            }
        }

        push_debug_log(ui_handle, "Resolver: keine automatische Lösung mehr möglich.".to_string());
        return Err(state);
    }

    push_debug_log(ui_handle, "Resolver: Iterationslimit erreicht.".to_string());
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

// Listet alle Dateien im mods-Ordner einer Instanz für die Detailansicht auf,
// als reine (Send-sichere) Daten - ohne slint::Image, das wird erst über
// build_mod_file_info auf dem UI-Thread ergänzt. Deaktivierte Mods liegen als
// "xyz.jar.disabled" vor (Konvention vieler Launcher), display_name zeigt den
// Namen ohne dieses Suffix.
fn list_mod_files_raw(instance_dir: &Path) -> Vec<ModFileEntry> {
    let mods_dir = instance_dir.join("mods");
    let Ok(entries) = fs::read_dir(&mods_dir) else {
        return Vec::new(); // noch kein mods-Ordner vorhanden -> leere Liste
    };

    let icons = load_mod_icons(instance_dir);

    let mut result: Vec<ModFileEntry> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_file())
        .map(|entry| {
            let filename = entry.file_name().to_string_lossy().to_string();
            let enabled = !filename.ends_with(".disabled");
            let display_name = filename.strip_suffix(".disabled").unwrap_or(&filename).to_string();
            let icon_path = icons.get(&display_name).cloned();

            ModFileEntry { filename, display_name, enabled, icon_path }
        })
        .collect();

    result.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    result
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

async fn install_cf_modpack(
    instance_dir: &PathBuf,
    zip_bytes: Vec<u8>,
    ui_handle: &Weak<AppWindow>,
) -> Result<(String, String), String> {
    let cursor = Cursor::new(zip_bytes);
    let mut archive = zip::ZipArchive::new(cursor).map_err(|e| format!("Ungültige Zip-Datei: {e}"))?;

    // 1. manifest.json lesen (in einem Block, um den Borrow von `archive` sofort zu beenden)
    let manifest: CfManifest = {
        let mut manifest_file = archive.by_name("manifest.json").map_err(|_| "Kein manifest.json gefunden. Ist das ein CF-Modpack?".to_string())?;
        let mut manifest_str = String::new();
        manifest_file.read_to_string(&mut manifest_str).map_err(|e| format!("Manifest Lesefehler: {e}"))?;
        serde_json::from_str(&manifest_str).map_err(|e| format!("Manifest Parse Fehler: {e}"))?
    }; // <-- Hier endet der Block, `manifest_file` wird gedroppt und gibt `archive` frei!

    let mc_version = manifest.minecraft.version.clone();
    // WICHTIG: Die komplette ID behalten (z.B. "forge-14.23.5.2860"), damit lyceris die richtige Version findet!
    let loader = manifest.minecraft.modLoaders.first()
        .map(|l| l.id.clone()) 
        .unwrap_or_else(|| "none".to_string());
    
    // 2. Overrides entpacken
    report_progress(ui_handle, "Entpacke CurseForge Overrides...".to_string());
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| format!("Zip-Fehler: {e}"))?;
        let name = entry.name().to_string();
        if let Some(rel_path) = name.strip_prefix("overrides/") {
            if rel_path.is_empty() { continue; }
            let out_path = instance_dir.join(rel_path);
            if entry.is_dir() {
                fs::create_dir_all(&out_path).ok();
            } else {
                if let Some(parent) = out_path.parent() { fs::create_dir_all(parent).ok(); }
                let mut out_file = File::create(&out_path).map_err(|e| format!("Datei erstellen fehlgeschlagen: {e}"))?;
                std::io::copy(&mut entry, &mut out_file).ok();
            }
        }
    }

    // 2.5. Stelle sicher, dass der mods-Ordner existiert (auch wenn das Manifest keine Downloads hat)
    let mods_dir = instance_dir.join("mods");
    fs::create_dir_all(&mods_dir).map_err(|e| format!("Konnte mods-Ordner nicht anlegen: {e}"))?;

    // 3. Mods aus dem Manifest herunterladen und mods-list.json erstellen
    let total = manifest.files.len();
    let mut mods_list: Vec<ModListEntry> = Vec::new();

    for (i, file) in manifest.files.iter().enumerate() {
        report_progress(ui_handle, format!("Lade Mod {}/{} herunter...", i + 1, total));
        match download_cf_file(file.fileID).await {
            Ok((filename, bytes)) => {
                let mods_dir = instance_dir.join("mods");
                fs::create_dir_all(&mods_dir).ok();
                fs::write(mods_dir.join(&filename), &bytes).map_err(|e| format!("Mod speichern fehlgeschlagen: {e}"))?;
                
                // NEU: Zur Liste hinzufügen
                mods_list.push(ModListEntry {
                    filename: filename.clone(),
                    project_id: file.projectID.to_string(),
                });
            }
            Err(e) => {
                println!("⚠️ Mod-Download übersprungen (Projekt-ID: {}, Datei-ID: {}): {}", file.projectID, file.fileID, e);
            }
        }
    }
    
    // NEU: mods-list.json für diese Instanz speichern
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

    let rt = Builder::new_multi_thread()
        .worker_threads(1)
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
    let open_details_bg_handle = handle.clone(); // <-- NEU: Für den Icon-Nachlade-Task
    let open_details_handle = handle.clone(); // <-- NEU: Für den Hintergrund-Task im Detail-Tab

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

    ui.set_debug_log(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));

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
    ui.set_debug_ui(settings.debug_ui);

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

    // Search for Installed Instances
    for instance in fs::read_dir(&launcherDir.join("instances"))? {
        //if fs::exists(&launcherDir.join("instances/").join(instance).join("instance.json")) {
        //    modpacktilemodel.push(ModpackInfo { name: "Test...".into() });
        //}

        let Ok(instance) = instance else {
            continue;
        };

        let path = instance.path();

        if !path.is_dir() {
            continue;
        }

        if !path.join("instance.json").is_file() {
            continue;
        }

        let Some(modpackname) = path.file_name() else {
            continue; // Pfad ohne Namen überspringen
        };

        let file = File::open(&launcherDir.join("instances/").join(&modpackname).join("instance.json")).expect("Error during file Reading of instance.json");
        let reader = BufReader::new(file);
        
        let config: ModpackJsonData = serde_json::from_reader(reader).expect("Error during extracting data out of instance.json");

        // Icon-Pfad (falls vorhanden) klonen bevor die restlichen Felder unten
        // per .into() verschoben werden, und direkt zu einem slint::Image laden.
        let icon_path = config.icon_path.clone().map(PathBuf::from);

        modpacktilemodel.push(ModpackInfo {
            //name: modpackname.to_string_lossy().to_string().into(),
            name: config.name.into(),
            minecraft_version: config.minecraft_version.into(),
            loader: config.loader.into(),
            ram_mb: config.ram_mb,
            icon: load_icon(&icon_path),
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
    ui.on_modpack_play(move |name| {
        let _ui = modpack_play_ui_handle.unwrap();
        let handle = handle.clone();
        let ui_handle = ui_handle.clone();

        let launcherDir = dirs::data_local_dir()
            .expect("kein Local Data Dir")
            .join("srusm");

        println!("Modpack Name: {}", name);

        println!("{:?}", &launcherDir.join("instances/").join(&name).join("instance.json"));
        
        let instance_dir = launcherDir.join("instances/").join(&name);

        let file = File::open(instance_dir.join("instance.json")).expect("Error during file Reading of instance.json");
        let reader = BufReader::new(file);
        
        let config: ModpackJsonData = serde_json::from_reader(reader).expect("Error during extracting data out of instance.json");

        println!("Modpack Name: {}\nMinecraft Version: {}\nModLoader: {}\n Ram MB: {}mb", name, config.minecraft_version, config.loader, config.ram_mb);

        // Instanz-eigene Overrides laden: wenn aktiviert, überschreiben RAM und
        // Username die Werte aus der instance.json bzw. dem aktuell
        // ausgewählten Account weiter unten.
        let overrides = load_instance_overrides(&instance_dir);
        // RAM-Reihenfolge: Instanz-Override (wenn aktiviert) > globaler Wert aus dem Settings-Tab.
        // config.ram_mb aus der instance.json wird bewusst NICHT mehr genutzt, weil dort bei
        // jeder neuen Instanz fest 2048 steht und die Settings sonst nie greifen würden.
        let settings = load_settings(&launcherDir);
        let effective_ram_mb = if overrides.enabled { overrides.ram_mb } else { settings.default_ram_mb };
        println!("RAM für den Start: {} MB", effective_ram_mb);

        // Ausgewählten Account merken; die eigentliche Auth-Methode (ggf. mit Token-Erneuerung,
        // das braucht Netzwerk) wird erst im async-Block gebaut.
        let accounts = load_accounts(&launcherDir);
        let selected_account: Option<AccountEntry> = accounts
            .selected_id
            .as_ref()
            .and_then(|id| accounts.accounts.iter().find(|a| &a.id == id))
            .cloned();

        
        // Instanz-Override hat Vorrang und startet immer als Offline-Name
        let override_username: Option<String> = if overrides.enabled { Some(overrides.username.clone()) } else { None };

        let _launcher_dir = launcherDir.clone();
        let ui_handle = ui_handle.clone();
        let handle = handle.clone();
        let name = name.to_string();

        handle.spawn(async move {
            println!("Starte {}", name);

            // Auth bestimmen: Override > Microsoft-Account > Offline-Account > "TestUser"
            let auth = if let Some(name) = override_username {
                lyceris::AuthMethod::Offline { username: name, uuid: None }
            } else {
                match selected_account {
                    Some(acc) if acc.kind == "microsoft" => match microsoft_auth_for(&launcherDir, acc).await {
                        Ok(auth) => auth,
                        Err(e) => {
                            println!("Microsoft-Anmeldung fehlgeschlagen: {e}");
                            push_debug_log(&ui_handle, format!("Microsoft-Anmeldung fehlgeschlagen: {e}"));
                            return;
                        }
                    },
                    Some(acc) => lyceris::AuthMethod::Offline { username: acc.username, uuid: None },
                    None => lyceris::AuthMethod::Offline { username: "TestUser".to_string(), uuid: None },
                }
            };

            let builder = ConfigBuilder::new(
                &launcherDir.join("shared"),
                config.minecraft_version.clone(),
                auth,
            ).runtime_dir(launcherDir.join("shared/runtime"))
            .memory(Memory::Megabyte(effective_ram_mb as u64))
            .profile(Profile::new(name.to_string(), launcherDir.clone().join("instances")));
            
            // lyceris liest stdout/stderr des Spiels selbst und schickt jede Zeile als
            // Event::Console an den Emitter. Ohne Emitter (None) geht die Ausgabe verloren.
            let emitter = Emitter::default();
            emitter
                .on(Event::Console, |line: String| {
                    println!("[MC] {line}");
                })
                .await;

            if config.loader == "none" {
                let cfg = builder.build();

                if let Err(e) = install(&cfg, None).await {
                    println!("Install-Fehler: {:?}", e);
                    return;
                }
                //if let Err(e) = launch(&cfg, None).await {
                //    println!("Start-Fehler: {:?}", e);
                //    return;
                //}
                match launch(&cfg, Some(&emitter)).await {
                    // Wir müssen nur noch auf das Spielende warten, das Log kommt über den Emitter
                    Ok(mut child) => {
                        match child.wait().await {
                            Ok(status) => println!("Minecraft beendet: {status}"),
                            Err(e) => println!("Fehler beim Warten auf Minecraft: {e}"),
                        }
                    }
                    Err(e) => {
                        println!("Start-Fehler: {:?}", e);
                        return;
                    }
                }
            } else {
                // Alte CurseForge-Instanzen haben noch die volle ID im loader-Feld
                // ("fabric-0.16.9"), deshalb hier immer zerlegen.
                let (loader_kind, id_version) = split_loader_id(&config.loader);

                // Vorrang: gespeicherte Pack-Version > Version aus der ID > neueste stabile.
                let version = match config.loader_version.clone().or(id_version) {
                    Some(v) => v,
                    None => {
                        let Some(v) = latest_loader_version(&loader_kind, &config.minecraft_version).await else {
                            println!("Loader-Version nicht gefunden");
                            return;
                        };
                        v
                    }
                };
                println!("Nutze {} {}", loader_kind, version);

                let Some(loader) = make_loader(&loader_kind, version) else {
                    println!("Unbekannter Loader: {}", loader_kind);
                    return;
                };

                let cfg = builder.loader(loader).build();

                if let Err(e) = install(&cfg, None).await {
                    println!("Install-Fehler: {:?}", e);
                    return;
                }
                //if let Err(e) = launch(&cfg, None).await {
                //    println!("Start-Fehler: {:?}", e);
                //    return;
                //}

                match launch(&cfg, Some(&emitter)).await {
                    // Wir müssen nur noch auf das Spielende warten, das Log kommt über den Emitter
                    Ok(mut child) => {
                        match child.wait().await {
                            Ok(status) => println!("Minecraft beendet: {status}"),
                            Err(e) => println!("Fehler beim Warten auf Minecraft: {e}"),
                        }
                    }
                    Err(e) => {
                        println!("Start-Fehler: {:?}", e);
                        return;
                    }
                }
            }

            let _ = ui_handle.upgrade_in_event_loop(|_ui| {
                println!("Minecraft beendet");
            });
        });

        /*let builded_config = minecraft_config.build();
        let result = if config.loader == "none" {
            builded_config = minecraft_config.build();
        } else {
            match latest_loader_version(&config.loader, &config.minecraft_version).await
                .and_then(|v| make_loader(&config.loader, v))
            {
                Some(loader) => builded_config.loader(loader).build(),//run(builded_config.loader(loader).build()).await,
                None => Err("Loader-Version nicht gefunden".to_string()),
            }
        };

        //let builded_config = minecraft_config.build();

        handle.spawn(async move {
            // Testphase: später ersetzt du das durch JSON lesen, Config bauen, install() ...
            println!("Starte {}", name);
            //tokio::time::sleep(Duration::from_secs(3)).await;

            install(&builded_config, None).await;
            launch(&builded_config, None).await;

            let _ = ui_handle.upgrade_in_event_loop(|ui| {
                println!("Minecraft Gestarted");
            });
        });*/

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
            debug_ui,
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

            // PARALLELER Icon-Download
            let mut handles = Vec::new();
            for pack in &packs_with_urls {
                let cache_dir = icon_cache_dir.clone();
                let key = pack.project_id.clone();
                let url = pack.icon_url.clone();
                let handle = tokio::spawn(async move {
                    cache_icon(&cache_dir, &key, &url).await
                });
                handles.push(handle);
            }

            // Warte auf alle Icon-Downloads
            let mut icon_paths = Vec::new();
            for handle in handles {
                icon_paths.push(handle.await.unwrap_or(None));
            }

            println!("✅ Alle Icons geladen, aktualisiere UI...");

            // JETZT ERST im UI-Thread die slint::Image Objekte und BrowserPackInfo erstellen!
            // Das umgeht den `Send`-Fehler, da `load_icon` und `BrowserPackInfo` 
            // nur im UI-Thread existieren.
            let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                let items: Vec<BrowserPackInfo> = packs_with_urls.into_iter().zip(icon_paths.into_iter()).map(|(pack, icon_path)| {
                    BrowserPackInfo {
                        project_id: pack.project_id.into(),
                        name: pack.name.into(),
                        summary: pack.summary.into(),
                        author: pack.author.into(),
                        icon: load_icon(&icon_path), // <--- Hier ist es erlaubt!
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
        
        // 1. Synchrones Laden der Basisdaten für sofortige UI-Anzeige
        let mods_raw = list_mod_files_raw(&instance_dir);
        let overrides = load_instance_overrides(&instance_dir);
        let instance_icon_path: Option<PathBuf> = File::open(instance_dir.join("instance.json"))
            .ok()
            .and_then(|f| serde_json::from_reader::<_, ModpackJsonData>(BufReader::new(f)).ok())
            .and_then(|c| c.icon_path)
            .map(PathBuf::from);

        ui.set_detail_instance_name(name.clone());
        ui.set_detail_instance_icon(load_icon(&instance_icon_path));
        ui.set_detail_mod_files(ModelRc::new(VecModel::from(
            mods_raw.iter().cloned().map(build_mod_file_info).collect::<Vec<_>>(),
        )));
        ui.set_detail_overrides_enabled(overrides.enabled);
        ui.set_detail_ram_mb(overrides.ram_mb);
        ui.set_detail_java_path(overrides.java_path.into());
        ui.set_detail_username(overrides.username.into());
        ui.set_selected_panel("Instance Detail".into());

        println!("📂 Detailansicht für '{}' geöffnet. Prüfe auf fehlende Icons...", name);

        // 2. NEU: Hintergrund-Task zum Nachladen fehlender Icons (nur EIN UI-Update am Ende)
        let bg_ui_handle = open_details_ui_handle.clone();
        let bg_instance_dir = instance_dir.clone();
        let bg_launcher_dir = launcherDir.clone();
        let bg_handle = open_details_bg_handle.clone();

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
                        let new_mods = list_mod_files_raw(&bg_instance_dir);
                        ui.set_detail_mod_files(ModelRc::new(VecModel::from(
                            new_mods.into_iter().map(build_mod_file_info).collect::<Vec<_>>()
                        )));
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

        let mods_raw = list_mod_files_raw(&instance_dir);
        ui.set_detail_mod_files(ModelRc::new(VecModel::from(
            mods_raw.into_iter().map(build_mod_file_info).collect::<Vec<_>>(),
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
        
        // NEU: mods-list.json aus dem aktualisierten meta-Array regenerieren
        let mods_list: Vec<ModListEntry> = meta.iter().map(|m| ModListEntry {
            filename: m.filename.clone(),
            project_id: m.project_id.clone(),
        }).collect();
        save_mods_list(&instance_dir, &mods_list);
        
        let mods_raw = list_mod_files_raw(&instance_dir);
        ui.set_detail_mod_files(ModelRc::new(VecModel::from(
            mods_raw.into_iter().map(build_mod_file_info).collect::<Vec<_>>(),
        )));
    });

    // Instanz-eigene Overrides speichern (instance_overrides.json).
    ui.on_detail_save_overrides(move |instance_name, enabled, ram, java_path, username| {
        let _ui = save_overrides_ui_handle.unwrap();

        let launcherDir = dirs::data_local_dir()
            .expect("kein Local Data Dir")
            .join("srusm");
        let instance_dir = launcherDir.join("instances").join(instance_name.to_string());

        let overrides = InstanceOverrides {
            enabled,
            ram_mb: ram,
            java_path: java_path.to_string(),
            username: username.to_string(),
        };

        save_instance_overrides(&instance_dir, &overrides);

        println!("Overrides für {} gespeichert: {:?}", instance_name, overrides);
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

            push_debug_log(&ui_handle, format!("Suche passende Version für {}...", project_id));
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
                push_debug_log(&ui_handle, format!("Lade Bestandsdaten für {} ({})", meta.project_name, meta.version_id));
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
                push_debug_log(&ui_handle, format!("Lade {} für {}...", file.filename, rmod.project_name));
                let Ok(bytes) = download_bytes(&file.url).await else {
                    push_debug_log(&ui_handle, format!("Download fehlgeschlagen für {}", rmod.project_name));
                    continue;
                };
                // Alte Datei dieses Projekts entfernen, falls es ein Versionswechsel ist.
                if let Some(old) = meta.iter().find(|m| &m.project_id == project_id) {
                    let _ = fs::remove_file(mods_dir.join(&old.filename));
                    let _ = fs::remove_file(mods_dir.join(format!("{}.disabled", old.filename)));
                    icons.remove(&old.filename);
                }
                if let Err(e) = fs::write(mods_dir.join(&file.filename), &bytes) {
                    push_debug_log(&ui_handle, format!("Konnte Datei nicht schreiben: {e}"));
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
            }
            
            save_mod_meta(&instance_dir, &meta);
            save_mod_icons(&instance_dir, &icons); // Einmaliges Speichern am Ende
            
            // FIX: mods-list.json einfach aus dem aktualisierten meta regenerieren
            // (Das ersetzt den fehlerhaften 'current_list' Code)
            let mods_list: Vec<ModListEntry> = meta.iter().map(|m| ModListEntry {
                filename: m.filename.clone(),
                project_id: m.project_id.clone(),
            }).collect();
            save_mods_list(&instance_dir, &mods_list);

            let mods_raw = list_mod_files_raw(&instance_dir);
            let _ = ui_handle.upgrade_in_event_loop(move |ui| {
                ui.set_add_mod_status("Aktualisiert.".into());
                ui.set_detail_mod_files(ModelRc::new(VecModel::from(
                    mods_raw.into_iter().map(build_mod_file_info).collect::<Vec<_>>(),
                )));
            });
        });
    });

    // Irreconcilable-Fall: Nutzer wählt "deaktivieren" oder "abbrechen".
    ui.on_compat_disable_conflicts(move |instance_name| {
        let ui = compat_disable_ui_handle.unwrap();
        let pending_resolution = pending_resolution_disable.clone();

        let Some(final_state) = pending_resolution.lock().unwrap().take() else { return; };
        let problems = find_problems(&final_state);

        let launcherDir = dirs::data_local_dir().expect("kein Local Data Dir").join("srusm");
        let instance_dir = launcherDir.join("instances").join(instance_name.to_string());
        let meta = load_mod_meta(&instance_dir);

        let mut to_disable: std::collections::HashSet<String> = std::collections::HashSet::new();
        for p in &problems {
            match p {
                Problem::Incompatible { a, .. } => { to_disable.insert(a.clone()); }
                Problem::NeedsVersion { project_id, .. } => { to_disable.insert(project_id.clone()); }
                Problem::MissingRequired { project_id, .. } => { to_disable.insert(project_id.clone()); } // NEU
            }
        }

        for project_id in &to_disable {
            if let Some(m) = meta.iter().find(|m| &m.project_id == project_id) {
                let mods_dir = instance_dir.join("mods");
                let active_path = mods_dir.join(&m.filename);
                let disabled_path = mods_dir.join(format!("{}.disabled", m.filename));
                
                // Nur umbenennen, wenn die aktive (nicht deaktivierte) Datei tatsächlich existiert
                if active_path.exists() {
                    if let Err(e) = fs::rename(&active_path, &disabled_path) {
                        println!("Konnte Mod nicht deaktivieren: {e}");
                    }
                }
            }
        }

        ui.set_compat_popup_visible(false);
        let mods_raw = list_mod_files_raw(&instance_dir);
        ui.set_detail_mod_files(ModelRc::new(VecModel::from(
            mods_raw.into_iter().map(build_mod_file_info).collect::<Vec<_>>(),
        )));
    });

    ui.on_compat_cancel(move || {
        let ui = compat_cancel_ui_handle.unwrap();
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

    //ui.on_request_increase_value(move || {
    //    let ui = ui_handle.unwrap();
    //    ui.set_counter(ui.get_counter() + 1);
    //});

    ui.run()?;

    Ok(())
}