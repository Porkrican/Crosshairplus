//! Best-effort discovery of installed games for the Slots auto-launch picker.
//!
//! Only well-known launcher folders are read: Steam libraries listed in
//! `libraryfolders.vdf`, Epic's `LauncherInstalled.dat`, and `XboxGames` on
//! fixed drives. Nothing is launched or modified. When a folder gives no clue
//! which executable is the game, it is skipped rather than guessed, and Browse
//! `.exe` still covers it.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::slots::icon_data_url;

pub const MAX_GAMES: usize = 200;
/// Total executables inspected across every game folder, so a pathological
/// install tree cannot turn a scan into a full-disk walk.
const MAX_EXE_CANDIDATES: usize = 4000;
const MAX_DEPTH: usize = 4;
const MAX_ICONS: usize = 64;
/// Steam installs support content into `steamapps`; these appids are not games.
const NON_GAME_APPIDS: [&str; 4] = ["228980", "250820", "1070560", "1493710"];
const TOOL_NAME_MARKERS: [&str; 6] = [
    "redistributable",
    "steamworks",
    "steam linux runtime",
    "proton",
    "steamvr",
    "dedicated server",
];
const HELPER_EXE_MARKERS: [&str; 14] = [
    "unins",
    "vcredist",
    "dxsetup",
    "dxwebsetup",
    "unitycrashhandler",
    "crashhandler",
    "crashreport",
    "easyanticheat",
    "battleye",
    "prereq",
    "redist",
    "installer",
    "setup",
    "helper",
];
/// Launchers, updaters and config tools that carry the game's name but are not
/// the game itself. They stay candidates, but must lose to a name-matching game.
const DEPRIORITIZED_EXE_MARKERS: [&str; 5] = [
    "prelauncher",
    "launcher",
    "bootstrapper",
    "updater",
    "config",
];
const SKIP_DIRS: [&str; 8] = [
    "redist",
    "_commonredist",
    "redistributables",
    "support",
    "docs",
    "installers",
    "directx",
    "dotnet",
];

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InstalledGame {
    pub name: String,
    pub exe: String,
    pub source: String,
    pub icon: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct SteamApp {
    pub appid: String,
    pub name: String,
    pub installdir: String,
}

pub fn scan() -> Vec<InstalledGame> {
    let mut games: Vec<InstalledGame> = Vec::new();
    let mut budget = MAX_EXE_CANDIDATES;
    for library in steam_libraries() {
        steam_games_in(&library, &mut games, &mut budget);
        if games.len() >= MAX_GAMES {
            break;
        }
    }
    games.extend(epic_games_installed(&mut budget));
    games.extend(xbox_games_installed(&mut budget));
    dedupe(&mut games);
    for (index, game) in games.iter_mut().enumerate() {
        if index < MAX_ICONS {
            game.icon = icon_data_url(Path::new(&game.exe));
        }
    }
    games.sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()));
    games.truncate(MAX_GAMES);
    games
}

/// Reads the scan cached next to the config. A missing file is an empty list,
/// because a cache miss and an empty cache lead to the same scan; a damaged
/// file is an error so the caller can say why it is scanning again.
pub fn load_cache(path: &Path) -> Result<Vec<InstalledGame>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(format!("cannot read the cached game list {}: {err}", path.display())),
    };
    let mut games: Vec<InstalledGame> = serde_json::from_str(&text)
        .map_err(|err| format!("cannot parse the cached game list {}: {err}", path.display()))?;
    games.truncate(MAX_GAMES);
    Ok(games)
}

/// Writes then renames, so a crash mid-write cannot leave a half file for the
/// next launch to reject.
pub fn save_cache(path: &Path, games: &[InstalledGame]) {
    let Ok(text) = serde_json::to_string(games) else {
        crate::logging::log("games: cannot encode the scan cache");
        return;
    };
    let tmp = path.with_extension("json.tmp");
    if let Err(err) = fs::write(&tmp, text).and_then(|_| fs::rename(&tmp, path)) {
        let _ = fs::remove_file(&tmp);
        crate::logging::log(&format!("games: cannot write the scan cache {}: {err}", path.display()));
    }
}

/// Drops cached entries whose executable no longer exists. This touches the
/// filesystem, so callers keep it on the scan worker: a disconnected library
/// path can block for seconds.
pub fn retain_existing(games: Vec<InstalledGame>) -> Vec<InstalledGame> {
    games.into_iter().filter(|game| Path::new(&game.exe).is_file()).collect()
}

fn dedupe(games: &mut Vec<InstalledGame>) {
    let mut seen: Vec<String> = Vec::new();
    games.retain(|game| {
        let key = game.exe.to_ascii_lowercase();
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
}

fn steam_libraries() -> Vec<PathBuf> {
    let Some(root) = steam_root() else {
        return Vec::new();
    };
    let mut libraries = vec![root.clone()];
    let text = fs::read_to_string(root.join("steamapps").join("libraryfolders.vdf")).unwrap_or_default();
    for path in vdf_library_paths(&text) {
        let library = PathBuf::from(unescape_vdf_path(&path));
        if library.join("steamapps").is_dir() && !libraries.contains(&library) {
            libraries.push(library);
        }
    }
    libraries
}

fn steam_root() -> Option<PathBuf> {
    if let Some(path) = registry_string("Software\\Valve\\Steam", "SteamPath") {
        let root = PathBuf::from(unescape_vdf_path(&path));
        if root.is_dir() {
            return Some(root);
        }
    }
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Ok(base) = std::env::var(var) {
            let root = PathBuf::from(base).join("Steam");
            if root.is_dir() {
                return Some(root);
            }
        }
    }
    None
}

fn registry_string(subkey: &str, name: &str) -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};

    let subkey = wide(subkey);
    let name = wide(name);
    let mut size = 0u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS || size == 0 {
        return None;
    }
    let mut data = vec![0u16; (size as usize / 2).max(1)];
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(data.as_mut_ptr() as *mut core::ffi::c_void),
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let end = data.iter().position(|ch| *ch == 0).unwrap_or(data.len());
    Some(String::from_utf16_lossy(&data[..end]))
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn steam_games_in(library: &Path, out: &mut Vec<InstalledGame>, budget: &mut usize) {
    let apps = library.join("steamapps");
    let Ok(entries) = fs::read_dir(&apps) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if !file_name.starts_with("appmanifest_") || !file_name.ends_with(".acf") {
            continue;
        }
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let Some(app) = parse_acf(&text) else {
            continue;
        };
        if !is_steam_game(&app) {
            continue;
        }
        let dir = apps.join("common").join(&app.installdir);
        if !dir.is_dir() {
            continue;
        }
        if let Some(exe) = pick_exe(&app.name, &dir, budget) {
            out.push(InstalledGame {
                name: app.name,
                exe: exe.to_string_lossy().to_string(),
                source: "Steam".into(),
                icon: None,
            });
        }
        if out.len() >= MAX_GAMES {
            return;
        }
    }
}

fn epic_games_installed(budget: &mut usize) -> Vec<InstalledGame> {
    let Ok(program_data) = std::env::var("ProgramData") else {
        return Vec::new();
    };
    let file = PathBuf::from(program_data)
        .join("Epic")
        .join("UnrealEngineLauncher")
        .join("LauncherInstalled.dat");
    let Ok(text) = fs::read_to_string(file) else {
        return Vec::new();
    };
    epic_games(&text)
        .into_iter()
        .filter_map(|(name, location)| {
            let exe = pick_exe(&name, Path::new(&location), budget)?;
            Some(InstalledGame {
                name,
                exe: exe.to_string_lossy().to_string(),
                source: "Epic".into(),
                icon: None,
            })
        })
        .collect()
}

fn xbox_games_installed(budget: &mut usize) -> Vec<InstalledGame> {
    let mut out = Vec::new();
    for drive in fixed_drives() {
        let root = PathBuf::from(format!("{drive}:\\XboxGames"));
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let content = entry.path().join("Content");
            let dir = if content.is_dir() { content } else { entry.path() };
            if let Some(exe) = pick_exe(&name, &dir, budget) {
                out.push(InstalledGame {
                    name,
                    exe: exe.to_string_lossy().to_string(),
                    source: "Xbox".into(),
                    icon: None,
                });
            }
        }
    }
    out
}

fn fixed_drives() -> Vec<char> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    /// `DRIVE_FIXED` from the Win32 header; not re-exported by the windows crate.
    const DRIVE_FIXED: u32 = 3;

    let mut drives = Vec::new();
    let mask = unsafe { GetLogicalDrives() };
    for index in 0..26u32 {
        if mask & (1 << index) == 0 {
            continue;
        }
        let letter = (b'A' + index as u8) as char;
        let root: [u16; 4] = [letter as u16, ':' as u16, '\\' as u16, 0];
        if unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) } == DRIVE_FIXED {
            drives.push(letter);
        }
    }
    drives
}

/// Library paths from a `libraryfolders.vdf`, old (`"1" "D:\\Steam"`) or new
/// (`"path" "D:\\Steam"`) layout, still escaped.
pub fn vdf_library_paths(text: &str) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for line in text.lines() {
        let tokens = quoted_tokens(line);
        if tokens.len() < 2 {
            continue;
        }
        let key = tokens[0].to_ascii_lowercase();
        let is_library = key == "path" || (!key.is_empty() && key.chars().all(|ch| ch.is_ascii_digit()));
        let value = &tokens[1];
        // App ids inside an "apps" block use the same numeric-key shape, so a
        // value must look like a filesystem path before it counts as a library.
        let looks_like_path = value.contains(':') || value.contains('\\') || value.contains('/');
        if is_library && looks_like_path && !paths.iter().any(|path| path.eq_ignore_ascii_case(value)) {
            paths.push(value.clone());
        }
    }
    paths
}

pub fn parse_acf(text: &str) -> Option<SteamApp> {
    let mut app = SteamApp { appid: String::new(), name: String::new(), installdir: String::new() };
    for line in text.lines() {
        let tokens = quoted_tokens(line);
        if tokens.len() < 2 {
            continue;
        }
        let value = tokens[1].clone();
        match tokens[0].to_ascii_lowercase().as_str() {
            "appid" => app.appid = value,
            "name" => app.name = value,
            "installdir" => app.installdir = value,
            _ => {}
        }
    }
    if app.appid.is_empty() || app.installdir.is_empty() {
        None
    } else {
        Some(app)
    }
}

pub fn is_steam_game(app: &SteamApp) -> bool {
    if NON_GAME_APPIDS.contains(&app.appid.as_str()) {
        return false;
    }
    let name = app.name.to_ascii_lowercase();
    !TOOL_NAME_MARKERS.iter().any(|marker| name.contains(marker))
}

/// Epic's launcher data, as `(display name, install folder)`.
pub fn epic_games(text: &str) -> Vec<(String, String)> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let Some(list) = value.get("InstallationList").and_then(|list| list.as_array()) else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|entry| {
            let location = entry.get("InstallLocation")?.as_str()?.to_string();
            let app_name = entry.get("AppName").and_then(|value| value.as_str()).unwrap_or("");
            let folder = Path::new(&location)
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            let name = if folder.is_empty() { app_name.to_string() } else { folder };
            if name.is_empty() {
                None
            } else {
                Some((name, location))
            }
        })
        .collect()
}

fn quoted_tokens(line: &str) -> Vec<String> {
    line.split('"')
        .enumerate()
        .filter_map(|(index, part)| (index % 2 == 1).then(|| part.to_string()))
        .collect()
}

fn unescape_vdf_path(path: &str) -> String {
    path.replace("\\\\", "\\").replace('/', "\\")
}

/// The executable that most plausibly is the game, or `None` when the folder
/// does not make that clear enough to write into a slot.
pub fn pick_exe(game_name: &str, dir: &Path, budget: &mut usize) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    collect_exes(dir, 0, &mut candidates, budget);
    candidates.retain(|exe| !is_helper_exe(exe));
    if candidates.is_empty() {
        return None;
    }
    if candidates.len() == 1 {
        return candidates.pop();
    }
    candidates.sort();
    let mut best: Option<(i32, u64, PathBuf)> = None;
    for exe in candidates {
        let Some(score) = exe_score(game_name, &exe) else {
            continue;
        };
        let size = fs::metadata(&exe).map(|meta| meta.len()).unwrap_or(0);
        let better = match &best {
            None => true,
            Some((best_score, best_size, _)) => score > *best_score || (score == *best_score && size > *best_size),
        };
        if better {
            best = Some((score, size, exe));
        }
    }
    let (score, _, exe) = best?;
    // A weak match is more likely a bundled tool than the game itself.
    if score >= 20 {
        Some(exe)
    } else {
        None
    }
}

fn collect_exes(dir: &Path, depth: usize, out: &mut Vec<PathBuf>, budget: &mut usize) {
    if depth > MAX_DEPTH || *budget == 0 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut children = Vec::new();
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            children.push(path);
        } else if kind.is_file() && path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("exe")) {
            out.push(path);
            *budget = budget.saturating_sub(1);
            if *budget == 0 {
                return;
            }
        }
    }
    children.sort();
    for child in children {
        collect_exes(&child, depth + 1, out, budget);
        if *budget == 0 {
            return;
        }
    }
}

fn is_helper_exe(exe: &Path) -> bool {
    let Some(stem) = exe.file_stem() else {
        return true;
    };
    let name = normalize(&stem.to_string_lossy());
    name.len() < 3 || HELPER_EXE_MARKERS.iter().any(|marker| name.contains(marker))
}

fn exe_score(game_name: &str, exe: &Path) -> Option<i32> {
    let stem = exe.file_stem()?;
    let name = normalize(&stem.to_string_lossy());
    if name.len() < 3 {
        return None;
    }
    let game = normalize(game_name);
    let mut score = 0;
    if !game.is_empty() && name == game {
        score += 100;
    } else if !game.is_empty() && (name.starts_with(&game) || game.starts_with(&name)) {
        score += 60;
    } else {
        let shared = tokens(&game)
            .into_iter()
            .filter(|token| token.len() >= 3 && name.contains(token.as_str()))
            .count();
        score += 20 * shared as i32;
    }
    // Unreal titles ship a stub at the install root and the real game as
    // `*-Win64-Shipping.exe`; the shipping binary must win that tie-break.
    if name.contains("shipping") {
        score += 40;
    }
    if exe
        .components()
        .any(|part| part.as_os_str().to_string_lossy().eq_ignore_ascii_case("binaries"))
    {
        score += 10;
    }
    if DEPRIORITIZED_EXE_MARKERS.iter().any(|marker| name.contains(marker)) {
        // A pre-launcher or updater often carries the game name and can otherwise
        // outrank the real executable. Keep the candidate (the single-candidate
        // path still accepts it) but put it behind a name-matching game binary.
        score -= 40;
    }
    Some(score)
}

fn normalize(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect()
}

fn tokens(value: &str) -> Vec<String> {
    value
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| token.to_ascii_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir().join(format!("crosshairplus-games-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("temp tree");
            Self(root)
        }

        fn file(&self, relative: &str) -> PathBuf {
            let path = self.0.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("temp parent");
            }
            fs::write(&path, b"stub").expect("temp file");
            path
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn dead_cached_entries_are_pruned() {
        let tree = TempTree::new("cache-prune");
        let alive = tree.file("alive/game.exe");
        let games = vec![
            InstalledGame { name: "Alive".into(), exe: alive.display().to_string(), source: "Steam".into(), icon: None },
            InstalledGame {
                name: "Gone".into(),
                exe: tree.0.join("gone/game.exe").display().to_string(),
                source: "Steam".into(),
                icon: None,
            },
        ];
        let kept = retain_existing(games);
        assert_eq!(kept.len(), 1, "only the entry that still exists survives");
        assert_eq!(kept[0].name, "Alive");
    }

    #[test]
    fn scan_cache_round_trips_with_icons() {
        let tree = TempTree::new("cache");
        let path = tree.0.join("games.json");
        let games = vec![
            InstalledGame {
                name: "Alpha".into(),
                exe: "C:\\Games\\alpha.exe".into(),
                source: "Steam".into(),
                icon: Some("data:image/png;base64,AA".into()),
            },
            InstalledGame { name: "Beta".into(), exe: "D:\\Games\\beta.exe".into(), source: "Epic".into(), icon: None },
        ];
        save_cache(&path, &games);
        assert_eq!(load_cache(&path).unwrap(), games);
    }

    #[test]
    fn cache_reads_the_on_disk_shape() {
        let tree = TempTree::new("cache-shape");
        let path = tree.0.join("games.json");
        fs::write(
            &path,
            r#"[{"name":"Cached Alpha","exe":"C:\\Fake\\alpha.exe","source":"Steam","icon":null}]"#,
        )
        .expect("shape cache");
        assert_eq!(load_cache(&path).unwrap().len(), 1);
    }

    #[test]
    fn a_missing_or_corrupt_cache_is_only_a_cache_miss() {
        let tree = TempTree::new("cache-bad");
        let path = tree.0.join("games.json");
        assert!(load_cache(&path).unwrap().is_empty(), "no cache has been written yet");
        fs::write(&path, "{not json").expect("corrupt cache");
        assert!(load_cache(&path).is_err(), "a half-written cache must not pass as a list");
    }

    #[test]
    fn cache_load_caps_the_list() {
        let tree = TempTree::new("cache-cap");
        let path = tree.0.join("games.json");
        let games: Vec<InstalledGame> = (0..MAX_GAMES + 4)
            .map(|index| InstalledGame {
                name: format!("Game {index}"),
                exe: format!("C:\\Games\\{index}.exe"),
                source: "Steam".into(),
                icon: None,
            })
            .collect();
        save_cache(&path, &games);
        assert_eq!(load_cache(&path).unwrap().len(), MAX_GAMES);
    }

    #[test]
    fn library_paths_cover_old_and_new_layouts() {
        let old = r#"
"LibraryFolders"
{
	"TimeNextStatsReport"		"1234"
	"1"		"D:\\SteamLibrary"
	"2"		"E:\\Games\\Steam"
}
"#;
        assert_eq!(vdf_library_paths(old), vec!["D:\\\\SteamLibrary", "E:\\\\Games\\\\Steam"]);

        let new = r#"
"libraryfolders"
{
	"0"
	{
		"path"		"C:\\Program Files (x86)\\Steam"
		"apps"		{ "440"		"100" }
	}
	"1"
	{
		"path"		"D:\\SteamLibrary"
	}
}
"#;
        let paths = vdf_library_paths(new);
        assert_eq!(paths.len(), 2, "{paths:?}");
        assert_eq!(paths[0], "C:\\\\Program Files (x86)\\\\Steam");
        assert_eq!(paths[1], "D:\\\\SteamLibrary");
        assert_eq!(unescape_vdf_path(&paths[1]), "D:\\SteamLibrary");

        // App ids and sizes inside an "apps" block share the numeric-key shape.
        let apps = "\"apps\"\n{\n\t\"440\"\t\t\"100\"\n\t\"570\"\t\t\"200\"\n}";
        assert!(vdf_library_paths(apps).is_empty(), "app ids are not libraries");
    }

    #[test]
    fn app_manifests_expose_name_and_install_folder() {
        let acf = r#"
"AppState"
{
	"appid"		"730"
	"name"		"Counter-Strike 2"
	"installdir"		"Counter-Strike Global Offensive"
	"StateFlags"		"4"
}
"#;
        let app = parse_acf(acf).expect("manifest parsed");
        assert_eq!(app.appid, "730");
        assert_eq!(app.name, "Counter-Strike 2");
        assert_eq!(app.installdir, "Counter-Strike Global Offensive");
        assert!(is_steam_game(&app));
    }

    #[test]
    fn redistributables_and_tools_are_not_games() {
        let redist = SteamApp {
            appid: "228980".into(),
            name: "Steamworks Common Redistributables".into(),
            installdir: "Steamworks Shared".into(),
        };
        assert!(!is_steam_game(&redist));
        let proton = SteamApp { appid: "1493710".into(), name: "Proton Experimental".into(), installdir: "Proton - Experimental".into() };
        assert!(!is_steam_game(&proton));
        let runtime = SteamApp { appid: "1234".into(), name: "Steam Linux Runtime 3.0".into(), installdir: "SteamLinuxRuntime".into() };
        assert!(!is_steam_game(&runtime));
    }

    #[test]
    fn epic_installations_use_the_install_folder_name() {
        let data = r#"{
  "InstallationList": [
    { "InstallLocation": "C:\\Program Files\\Epic Games\\Fortnite", "AppName": "Fortnite", "AppVersion": "1.0" },
    { "InstallLocation": "D:\\Epic\\RocketLeague", "AppName": "Sugar" }
  ]
}"#;
        assert_eq!(
            epic_games(data),
            vec![
                ("Fortnite".to_string(), "C:\\Program Files\\Epic Games\\Fortnite".to_string()),
                ("RocketLeague".to_string(), "D:\\Epic\\RocketLeague".to_string()),
            ]
        );
        assert!(epic_games("not json").is_empty());
    }

    #[test]
    fn the_game_executable_beats_bundled_helpers() {
        let tree = TempTree::new("pick");
        tree.file("Game.exe");
        tree.file("Uninstall.exe");
        tree.file("vcredist_x64.exe");
        tree.file("tools\\dxsetup.exe");
        let mut budget = MAX_EXE_CANDIDATES;
        let exe = pick_exe("Game", &tree.0, &mut budget).expect("game exe");
        assert_eq!(exe.file_name().unwrap(), "Game.exe");
    }

    #[test]
    fn the_real_game_beats_prelaunchers_and_updaters() {
        let tree = TempTree::new("prelauncher");
        tree.file("MyGame.exe");
        tree.file("MyGameLauncher.exe");
        tree.file("MyGamePrelauncher.exe");
        tree.file("MyGameBootstrapper.exe");
        tree.file("MyGameUpdater.exe");
        tree.file("MyGameConfig.exe");
        let mut budget = MAX_EXE_CANDIDATES;
        let exe = pick_exe("MyGame", &tree.0, &mut budget).expect("real game exe");
        assert_eq!(exe.file_name().unwrap(), "MyGame.exe");
    }

    #[test]
    fn cyberpunk_game_exe_beats_redprelauncher() {
        let tree = TempTree::new("cyberpunk");
        tree.file("Cyberpunk2077.exe");
        tree.file("REDprelauncher.exe");
        let mut budget = MAX_EXE_CANDIDATES;
        let exe = pick_exe("Cyberpunk 2077", &tree.0, &mut budget).expect("game exe");
        assert_eq!(exe.file_name().unwrap(), "Cyberpunk2077.exe");
    }

    #[test]
    fn a_lone_prelauncher_is_still_selectable() {
        let tree = TempTree::new("only-prelauncher");
        tree.file("REDprelauncher.exe");
        let mut budget = MAX_EXE_CANDIDATES;
        let exe = pick_exe("Cyberpunk 2077", &tree.0, &mut budget).expect("lone prelauncher fallback");
        assert_eq!(exe.file_name().unwrap(), "REDprelauncher.exe");
    }

    #[test]
    fn a_shipping_binary_wins_inside_binaries() {
        let tree = TempTree::new("shipping");
        tree.file("Binaries\\Win64\\MyGame-Win64-Shipping.exe");
        tree.file("MyGame.exe");
        let mut budget = MAX_EXE_CANDIDATES;
        let exe = pick_exe("MyGame", &tree.0, &mut budget).expect("shipping exe");
        assert_eq!(exe.file_name().unwrap(), "MyGame-Win64-Shipping.exe");
    }

    #[test]
    fn a_folder_with_only_helpers_yields_nothing() {
        let tree = TempTree::new("helpers");
        tree.file("Uninstall.exe");
        tree.file("UnityCrashHandler64.exe");
        let mut budget = MAX_EXE_CANDIDATES;
        assert_eq!(pick_exe("Whatever", &tree.0, &mut budget), None);
    }

    #[test]
    fn an_unrelated_binary_set_is_not_guessed() {
        let tree = TempTree::new("ambiguous");
        tree.file("alpha.exe");
        tree.file("beta.exe");
        let mut budget = MAX_EXE_CANDIDATES;
        assert_eq!(pick_exe("Completely Different", &tree.0, &mut budget), None);
        // A single leftover candidate is still a fair answer.
        let single = TempTree::new("single");
        single.file("whatever.exe");
        let mut budget = MAX_EXE_CANDIDATES;
        assert!(pick_exe("Completely Different", &single.0, &mut budget).is_some());
    }

    #[test]
    fn the_walk_stops_at_the_depth_limit() {
        let tree = TempTree::new("depth");
        tree.file("a\\b\\c\\d\\e\\deep.exe");
        let mut budget = MAX_EXE_CANDIDATES;
        assert_eq!(pick_exe("Deep", &tree.0, &mut budget), None);
    }

    #[test]
    fn the_candidate_budget_is_respected() {
        let tree = TempTree::new("budget");
        for index in 0..8 {
            tree.file(&format!("tool{index}.exe"));
        }
        let mut budget = 3;
        let _ = pick_exe("Tool", &tree.0, &mut budget);
        assert_eq!(budget, 0, "the walk must stop once the budget is spent");
    }
}
