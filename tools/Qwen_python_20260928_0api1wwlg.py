import zipfile
import json
import os
import sys
import tempfile
import urllib.request
import time

# Der offizielle API-Key für Launcher wie Prism (kostenlos nutzbar)
CF_API_KEY = "$2a$10$bL4bIL5pUWqfcO7KQtnMReakwtfHbNKh6v1uTpKlzhwoueEJQnPnm"

def get_cf_download_url(project_id, file_id, api_key):
    """Fragt die CurseForge API nach der Download-URL für eine Mod."""
    url = f"https://api.curseforge.com/v1/mods/{project_id}/files/{file_id}"
    req = urllib.request.Request(url, headers={"x-api-key": api_key, "Accept": "application/json"})
    try:
        with urllib.request.urlopen(req) as response:
            data = json.loads(response.read().decode('utf-8'))
            return data['data'].get('downloadUrl')
    except Exception as e:
        print(f"    ⚠️  API-Fehler für Projekt {project_id}: {e}")
        return None

def convert_cf_to_mrpack(cf_zip_path):
    if not os.path.exists(cf_zip_path):
        print(f"❌ Fehler: Datei '{cf_zip_path}' nicht gefunden!")
        return

    print(f"📦 Starte Konvertierung für: {cf_zip_path}")

    with tempfile.TemporaryDirectory() as temp_dir:
        # 1. CF Zip entpacken
        print("📂 Entpacke CurseForge-Archiv...")
        with zipfile.ZipFile(cf_zip_path, 'r') as zip_ref:
            zip_ref.extractall(temp_dir)

        # 2. manifest.json lesen
        manifest_path = os.path.join(temp_dir, 'manifest.json')
        if not os.path.exists(manifest_path):
            print("❌ Kein manifest.json gefunden. Ist das eine echte CurseForge Zip?")
            return

        with open(manifest_path, 'r', encoding='utf-8') as f:
            manifest = json.load(f)

        mc_version = manifest.get('minecraft', {}).get('version', '1.20.1')
        loaders = manifest.get('minecraft', {}).get('modLoaders', [])
        
        if not loaders:
            loader_name, loader_version = "forge", "0.0.0"
        else:
            loader_name, loader_version = loaders[0]['id'].split('-', 1)

        pack_name = manifest.get('name', 'Converted Modpack')
        
        # 3. Modrinth Index aufbauen
        mr_index = {
            "formatVersion": 1,
            "game": "minecraft",
            "versionId": "1.0.0",
            "name": pack_name,
            "summary": manifest.get('synopsis', 'Konvertiert von CurseForge'),
            "files": [],
            "dependencies": {
                "minecraft": mc_version,
                loader_name: loader_version
            }
        }

        # 4. Overrides-Ordner vorbereiten
        overrides_dir = os.path.join(temp_dir, 'overrides')
        mods_dir = os.path.join(overrides_dir, 'mods')
        os.makedirs(mods_dir, exist_ok=True)

        # 5. Mods aus der manifest.json herunterladen
        cf_files = manifest.get('files', [])
        if cf_files:
            print(f"🔄 Lade {len(cf_files)} Mods von CurseForge herunter...")
            success_count = 0
            for i, mod in enumerate(cf_files):
                pid, fid = mod.get('projectID'), mod.get('fileID')
                url = get_cf_download_url(pid, fid, CF_API_KEY)
                
                if url:
                    try:
                        filename = url.split('/')[-1]
                        filepath = os.path.join(mods_dir, filename)
                        urllib.request.urlretrieve(url, filepath)
                        print(f"  [{i+1}/{len(cf_files)}] ✅ {filename}")
                        success_count += 1
                        time.sleep(0.1)
                    except Exception as e:
                        print(f"  [{i+1}/{len(cf_files)}] ❌ Download fehlgeschlagen: {e}")
                else:
                    print(f"  [{i+1}/{len(cf_files)}] ⚠️ URL für Projekt {pid} nicht gefunden.")
            
            print(f"\n📊 {success_count}/{len(cf_files)} Mods erfolgreich heruntergeladen.")
        else:
            print("ℹ️  Keine Mods in der manifest.json gefunden.")

        # 6. .mrpack erstellen
        output_path = cf_zip_path.rsplit('.', 1)[0] + '.mrpack'
        print(f"\n📦 Erstelle Modrinth-Pack: {output_path}")
        
        with zipfile.ZipFile(output_path, 'w', zipfile.ZIP_DEFLATED) as mrpack:
            mrpack.writestr('modrinth.index.json', json.dumps(mr_index, indent=2))
            
            if os.path.exists(overrides_dir):
                file_count = 0
                for root, dirs, files in os.walk(overrides_dir):
                    for file in files:
                        file_path = os.path.join(root, file)
                        arcname = os.path.relpath(file_path, temp_dir)
                        mrpack.write(file_path, arcname)
                        file_count += 1
                print(f"✅ {file_count} Dateien in das Modrinth-Pack gepackt.")
            else:
                print("⚠️  Kein 'overrides' Ordner gefunden.")

        print(f"\n🎉 Erfolg! Dein Modrinth Pack liegt hier:\n   {output_path}")
        print("   Jetzt im Modrinth Launcher unter 'Import from file' laden.")

if __name__ == "__main__":
    print("=== CF zu Modrinth Converter ===\n")
    
    if len(sys.argv) < 2:
        print("Nutzung: python3 cf2mrpack.py <pfad_zum_curseforge_pack.zip>")
        print("Beispiel: python3 cf2mrpack.py ~/Downloads/SkyFactory\\ 4-4.2.4.zip")
    else:
        convert_cf_to_mrpack(sys.argv[1])
