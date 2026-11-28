//! The Peko folder: where the engine keeps pekos, conversations, models and
//! keys, and the first-run setup that chooses it.
//!
//! Every path the app derives from the folder comes from [`peko_home`]:
//! `PEKO_HOME`, else `PEKO_CONFIG_DIR`, else the folder named in the
//! pointer file, else `~/.peko`. The pointer file is the runtime's
//! (`peko_core::common::paths::home_pointer_path`):
//! `<local config dir>/peko/home.toml`, one key, `home = "<absolute path>"`.
//! The `peko` command line and `peko-daemon` read the same file, so the
//! terminal reaches the pekos the app shows. It exists only when the owner
//! chose somewhere other than `~/.peko`.
//!
//! On a computer with no Peko folder yet (or one whose chosen folder is
//! gone, say on a drive that is not connected), the engine is held: the
//! supervisor refuses to start it until the setup wizard has chosen a
//! folder and called [`setup_finish`].

use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::RwLock;

use serde::Serialize;

/// The longest folder path whose `run/daemon.sock` still fits a Unix
/// socket address. The runtime warns past 100 bytes for the whole socket
/// path (`sun_path` is 104 bytes on macOS, 108 on Linux).
const MAX_SOCKET_PATH: usize = 100;
const SOCKET_SUFFIX: &str = "/run/daemon.sock";

/// The name of the folder made inside a chosen folder that has other
/// things in it.
const SUBFOLDER: &str = "Peko";

// ─── Resolution ──────────────────────────────────────────────────────

/// The pointer file's folder, read once and kept until setup rewrites it.
static POINTER: RwLock<Option<Option<PathBuf>>> = RwLock::new(None);

/// Whether the engine waits for setup to finish.
static HELD: AtomicBool = AtomicBool::new(false);

/// Where the pointer file lives (the runtime's `home_pointer_path`).
pub fn pointer_path() -> Option<PathBuf> {
    dirs::config_local_dir().map(|d| d.join("peko").join("home.toml"))
}

/// The folder a pointer file names: an absolute `home`, else `None`.
/// Mirrors the runtime's `read_home_pointer`.
fn read_pointer_file(path: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(path).ok()?;
    let table = text.parse::<toml::Table>().ok()?;
    let home = PathBuf::from(table.get("home")?.as_str()?.trim());
    home.is_absolute().then_some(home)
}

/// The folder the pointer file names, if any.
pub fn pointer_home() -> Option<PathBuf> {
    if let Some(cached) = POINTER.read().unwrap().as_ref() {
        return cached.clone();
    }
    let read = pointer_path().and_then(|p| read_pointer_file(&p));
    *POINTER.write().unwrap() = Some(read.clone());
    read
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// `~/.peko`, the folder Peko uses unless told otherwise.
pub fn default_home() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".peko"))
}

/// The Peko folder. See the module docs for the order.
pub fn peko_home() -> Option<PathBuf> {
    resolve(
        env_nonempty("PEKO_HOME"),
        env_nonempty("PEKO_CONFIG_DIR"),
        pointer_home(),
        default_home(),
    )
}

fn resolve(
    env_home: Option<String>,
    env_config: Option<String>,
    pointer: Option<PathBuf>,
    default: Option<PathBuf>,
) -> Option<PathBuf> {
    env_home
        .or(env_config)
        .map(PathBuf::from)
        .or(pointer)
        .or(default)
}

/// The engine's run folder, where its socket is: the runtime's
/// `default_run_dir` (`PEKO_HOME`, which the runtime takes from the
/// pointer, else `~/.peko`). `PEKO_CONFIG_DIR` does not move it.
pub fn run_dir() -> PathBuf {
    resolve(
        env_nonempty("PEKO_HOME"),
        None,
        pointer_home(),
        default_home(),
    )
    .unwrap_or_else(|| PathBuf::from(".peko"))
    .join("run")
}

/// The folder to hand the engine as `PEKO_HOME`, when the owner chose one
/// and the app's own environment does not already name one.
pub fn home_for_engine() -> Option<PathBuf> {
    if env_nonempty("PEKO_HOME").is_some() || env_nonempty("PEKO_CONFIG_DIR").is_some() {
        return None;
    }
    pointer_home()
}

/// True while setup has not chosen a folder; the supervisor does not start
/// the engine then.
pub fn engine_held() -> bool {
    HELD.load(Ordering::SeqCst)
}

// ─── Setup status ────────────────────────────────────────────────────

/// What the setup wizard needs to know at start.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SetupStatus {
    /// Whether the wizard shows.
    pub needed: bool,
    /// `"fresh"`: no Peko folder yet. `"missing"`: the chosen folder is
    /// not there (a drive not connected, a folder moved).
    pub reason: Option<&'static str>,
    /// The folder Peko uses now (or would, when `needed`).
    pub home: String,
    /// `~/.peko`.
    pub default_home: String,
    /// `macos`, `windows` or `linux`.
    pub platform: &'static str,
}

fn platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    }
}

/// Decide whether setup is needed, from what is on disk.
fn evaluate(
    env_set: bool,
    pointer: Option<&Path>,
    default: Option<&Path>,
) -> (bool, Option<&'static str>) {
    if env_set {
        return (false, None);
    }
    match pointer {
        Some(p) if p.is_dir() => (false, None),
        Some(_) => (true, Some("missing")),
        None => match default {
            Some(d) if d.exists() => (false, None),
            _ => (true, Some("fresh")),
        },
    }
}

fn status() -> SetupStatus {
    // The hold decides, not the disk: a folder that appears while setup is
    // open (the command line made `~/.peko`, a drive came back) must not
    // hide the wizard from an engine that is still waiting for it.
    let held = engine_held();
    let missing = pointer_home().is_some_and(|p| !p.is_dir());
    let default = default_home();
    let show = |p: Option<PathBuf>| p.map(|p| p.display().to_string()).unwrap_or_default();
    SetupStatus {
        needed: held,
        reason: held.then_some(if missing { "missing" } else { "fresh" }),
        home: show(peko_home()),
        default_home: show(default),
        platform: platform(),
    }
}

/// Called once at start, before the supervisor's first spawn: holds the
/// engine when there is no Peko folder to start it in.
pub fn evaluate_at_boot() {
    let env_set = env_nonempty("PEKO_HOME").is_some() || env_nonempty("PEKO_CONFIG_DIR").is_some();
    let (needed, _) = evaluate(
        env_set,
        pointer_home().as_deref(),
        default_home().as_deref(),
    );
    HELD.store(needed, Ordering::SeqCst);
}

// ─── Checking a folder ───────────────────────────────────────────────

/// What choosing a folder would do. `home` is where Peko would live: the
/// folder itself, or a `Peko` folder inside it when it holds other things.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FolderCheck {
    pub home: String,
    /// `"new"` (made on finish), `"empty"`, `"peko"` (a Peko folder
    /// already: its pekos come along), `"default"` (`~/.peko`).
    pub kind: &'static str,
    /// How many pekos a Peko folder already has.
    pub pekos: Option<u32>,
    /// Free space on the folder's drive, where the system says.
    pub free_bytes: Option<u64>,
    /// Things to know, none of them a reason to refuse: `"synced"` (a
    /// cloud-synced folder), `"removable"` (another drive, which must be
    /// connected when Peko starts), `"protected"` (macOS asks first).
    pub warnings: Vec<&'static str>,
    /// Why the folder cannot be used: `"notAbsolute"`, `"notFolder"`,
    /// `"notWritable"`, `"tooLong"`, `"system"`, `"insideApp"`.
    pub problem: Option<&'static str>,
}

/// Files a system leaves in an otherwise empty folder.
fn is_clutter(name: &str) -> bool {
    matches!(
        name,
        ".DS_Store" | "desktop.ini" | "Thumbs.db" | ".localized"
    )
}

/// Whether a folder already holds a Peko home.
fn looks_like_peko(dir: &Path) -> bool {
    [
        "vault.enc",
        "pekos",
        "principals",
        "models.toml",
        "providers.toml",
    ]
    .iter()
    .any(|m| dir.join(m).exists())
}

fn count_pekos(dir: &Path) -> u32 {
    std::fs::read_dir(dir.join("pekos"))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .count() as u32
        })
        .unwrap_or(0)
}

fn is_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .all(|e| is_clutter(&e.file_name().to_string_lossy()))
        })
        .unwrap_or(false)
}

/// The nearest folder at or above `path` that exists.
fn existing_ancestor(path: &Path) -> Option<&Path> {
    path.ancestors().find(|a| a.is_dir())
}

/// Whether a file can be made in `dir`: makes and removes one.
fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".peko-write-check-{}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

#[cfg(unix)]
fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a NUL-terminated path and `s` a zeroed out-struct.
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)]
    Some(s.f_bavail as u64 * s.f_frsize as u64)
}

#[cfg(not(unix))]
fn free_bytes(_dir: &Path) -> Option<u64> {
    None
}

fn lower(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/").to_lowercase()
}

/// Folders the system owns, or that an uninstaller removes.
fn system_folder(path: &Path, app_dir: Option<&Path>) -> Option<&'static str> {
    if let Some(app) = app_dir {
        if path.starts_with(app) {
            return Some("insideApp");
        }
    }
    let p = lower(path);
    if p.contains(".app/") || p.ends_with(".app") {
        return Some("insideApp");
    }
    let system: &[&str] = if cfg!(windows) {
        &[
            "c:/windows",
            "c:/program files",
            "c:/program files (x86)",
            "c:/programdata",
        ]
    } else {
        &[
            "/system",
            "/usr",
            "/bin",
            "/sbin",
            "/etc",
            "/var",
            "/private",
            "/library",
            "/applications",
            "/opt",
            "/boot",
            "/proc",
            "/sys",
            "/dev",
        ]
    };
    if cfg!(unix) && path.parent().is_none() {
        return Some("system");
    }
    if system
        .iter()
        .any(|s| p == *s || p.starts_with(&format!("{s}/")))
    {
        return Some("system");
    }
    // Temp folders too: the system empties them.
    let tmp = lower(&std::env::temp_dir());
    let tmp = tmp.trim_end_matches('/');
    if !tmp.is_empty() && (p == tmp || p.starts_with(&format!("{tmp}/"))) {
        return Some("system");
    }
    None
}

fn warnings_for(path: &Path, home: Option<&Path>) -> Vec<&'static str> {
    let p = lower(path);
    let mut out = Vec::new();
    let synced = [
        "/library/mobile documents/",
        "/library/cloudstorage/",
        "/icloud drive/",
        "/iclouddrive/",
        "/onedrive",
        "/dropbox",
        "/google drive",
        "/googledrive",
        "/my drive/",
    ];
    if synced.iter().any(|s| p.contains(s)) {
        out.push("synced");
    }
    let removable = if cfg!(target_os = "macos") {
        p.starts_with("/volumes/")
    } else if cfg!(windows) {
        let system_drive = std::env::var("SystemDrive")
            .unwrap_or_else(|_| "C:".into())
            .to_lowercase();
        p.starts_with("//") || !p.starts_with(&system_drive)
    } else {
        p.starts_with("/media/") || p.starts_with("/run/media/") || p.starts_with("/mnt/")
    };
    if removable {
        out.push("removable");
    }
    // macOS asks before an app reads Desktop, Documents or Downloads.
    if cfg!(target_os = "macos") {
        if let Some(h) = home {
            for f in ["Desktop", "Documents", "Downloads"] {
                if path.starts_with(h.join(f)) {
                    out.push("protected");
                    break;
                }
            }
        }
    }
    out
}

/// What choosing `path` would do.
pub fn check_folder(path: &Path) -> FolderCheck {
    let app_dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf));
    check_folder_with(path, default_home().as_deref(), app_dir.as_deref())
}

fn check_folder_with(path: &Path, default: Option<&Path>, app_dir: Option<&Path>) -> FolderCheck {
    let refuse = |problem| FolderCheck {
        home: path.display().to_string(),
        kind: "new",
        pekos: None,
        free_bytes: None,
        warnings: Vec::new(),
        problem: Some(problem),
    };
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        return refuse("notAbsolute");
    }
    if path.exists() && !path.is_dir() {
        return refuse("notFolder");
    }

    // The folder itself, or `Peko` inside it when it holds other things.
    let (home, kind) = if Some(path) == default {
        (path.to_path_buf(), "default")
    } else if !path.exists() {
        (path.to_path_buf(), "new")
    } else if looks_like_peko(path) {
        (path.to_path_buf(), "peko")
    } else if is_empty_dir(path) {
        (path.to_path_buf(), "empty")
    } else {
        let inner = path.join(SUBFOLDER);
        if inner.is_dir() && looks_like_peko(&inner) {
            (inner, "peko")
        } else if inner.exists() && !(inner.is_dir() && is_empty_dir(&inner)) {
            return refuse("notFolder");
        } else {
            (inner, "new")
        }
    };

    let mut check = FolderCheck {
        home: home.display().to_string(),
        kind,
        pekos: (kind == "peko").then(|| count_pekos(&home)),
        free_bytes: existing_ancestor(&home).and_then(free_bytes),
        warnings: warnings_for(&home, dirs::home_dir().as_deref()),
        problem: None,
    };
    if kind == "default" {
        if home.exists() {
            check.pekos = Some(count_pekos(&home));
        }
        return check;
    }
    check.problem = system_folder(&home, app_dir)
        .or_else(|| {
            (cfg!(unix) && home.as_os_str().len() + SOCKET_SUFFIX.len() > MAX_SOCKET_PATH)
                .then_some("tooLong")
        })
        .or_else(|| {
            let at = existing_ancestor(&home)?;
            (!writable(at)).then_some("notWritable")
        });
    check
}

// ─── Using a folder ──────────────────────────────────────────────────

fn write_pointer(home: &Path) -> std::io::Result<()> {
    let path = pointer_path().ok_or_else(|| std::io::Error::other("no config folder"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let quoted = toml::Value::String(home.display().to_string());
    let text = format!(
        "# Where Peko keeps its pekos, models and keys. Written by Peko Desktop's\n\
         # setup; `peko` and the engine read it when PEKO_HOME is not set.\n\
         home = {quoted}\n"
    );
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

fn remove_pointer() -> std::io::Result<()> {
    match pointer_path() {
        Some(p) if p.exists() => std::fs::remove_file(p),
        _ => Ok(()),
    }
}

/// Make the folder, private to this user on Unix.
fn create_home(home: &Path) -> std::io::Result<()> {
    if home.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(home)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Use `path` (or `~/.peko` for `None`) as the Peko folder. Only while
/// setup holds the engine: a running engine keeps the folder it has.
pub fn use_folder(path: Option<&Path>) -> Result<String, String> {
    if !engine_held() {
        return Err("Peko is already set up; its folder can't change while it runs.".into());
    }
    let default = default_home().ok_or("could not find your home folder")?;
    let check = check_folder(path.unwrap_or(&default));
    if let Some(problem) = check.problem {
        return Err(problem.to_string());
    }
    let home = PathBuf::from(&check.home);
    create_home(&home).map_err(|e| format!("could not make {}: {e}", home.display()))?;
    let written = if home == default {
        remove_pointer()
    } else {
        write_pointer(&home)
    };
    written.map_err(|e| format!("could not save where Peko lives: {e}"))?;
    *POINTER.write().unwrap() = Some((home != default).then_some(home.clone()));
    Ok(home.display().to_string())
}

// ─── Commands ────────────────────────────────────────────────────────

#[tauri::command]
pub fn setup_status() -> SetupStatus {
    status()
}

#[tauri::command]
pub fn setup_check_folder(path: String) -> FolderCheck {
    check_folder(Path::new(path.trim()))
}

#[tauri::command]
pub fn setup_use_folder(path: Option<String>) -> Result<String, String> {
    use_folder(path.as_deref().map(str::trim).map(Path::new))
}

/// Setup is done: let the engine start, and start it.
#[tauri::command]
pub fn setup_finish(app: tauri::AppHandle) -> Result<(), String> {
    let home = peko_home().ok_or("could not find the Peko folder")?;
    if !home.is_dir() {
        return Err(format!("{} is not there", home.display()));
    }
    HELD.store(false, Ordering::SeqCst);
    crate::allow_attachment_dirs(&app);
    let sup = crate::sidecar::get(&app);
    sup.start().map(|_| ()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_beats_the_pointer_which_beats_the_default() {
        let p = Some(PathBuf::from("/data/Peko"));
        let d = Some(PathBuf::from("/home/ada/.peko"));
        assert_eq!(
            resolve(
                Some("/env".into()),
                Some("/cfg".into()),
                p.clone(),
                d.clone()
            ),
            Some(PathBuf::from("/env"))
        );
        assert_eq!(
            resolve(None, Some("/cfg".into()), p.clone(), d.clone()),
            Some(PathBuf::from("/cfg"))
        );
        assert_eq!(resolve(None, None, p.clone(), d.clone()), p);
        assert_eq!(resolve(None, None, None, d.clone()), d);
    }

    #[test]
    fn setup_shows_only_without_a_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let there = tmp.path().join("there");
        std::fs::create_dir(&there).unwrap();
        let gone = tmp.path().join("gone");

        assert_eq!(evaluate(true, None, Some(&gone)), (false, None), "env set");
        assert_eq!(evaluate(false, Some(&there), None), (false, None));
        assert_eq!(
            evaluate(false, Some(&gone), Some(&there)),
            (true, Some("missing"))
        );
        assert_eq!(
            evaluate(false, None, Some(&there)),
            (false, None),
            "upgrade"
        );
        assert_eq!(evaluate(false, None, Some(&gone)), (true, Some("fresh")));
    }

    #[test]
    fn the_pointer_round_trips_through_the_runtime_format() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("home.toml");
        let home = tmp.path().join("My \"Peko\" folder");
        let quoted = toml::Value::String(home.display().to_string());
        std::fs::write(&file, format!("home = {quoted}\n")).unwrap();
        assert_eq!(read_pointer_file(&file), Some(home));
        std::fs::write(&file, "home = \"relative\"\n").unwrap();
        assert_eq!(read_pointer_file(&file), None);
    }

    #[test]
    fn a_folder_is_used_as_is_or_given_a_peko_folder_inside() {
        let tmp = tempfile::tempdir().unwrap();
        // Not under the temp dir rule: check through a fake "home".
        let root = tmp.path();
        let check = |p: &Path| {
            let mut c = check_folder_with(p, Some(&root.join(".peko")), None);
            // The temp folder itself is refused as `system`; the shape is
            // what these cases are about.
            if c.problem == Some("system") {
                c.problem = None;
            }
            c
        };

        let new = root.join("fresh");
        let c = check(&new);
        assert_eq!((c.kind, c.home.clone()), ("new", new.display().to_string()));

        let empty = root.join("empty");
        std::fs::create_dir(&empty).unwrap();
        std::fs::write(empty.join(".DS_Store"), "").unwrap();
        assert_eq!(check(&empty).kind, "empty");

        let busy = root.join("busy");
        std::fs::create_dir(&busy).unwrap();
        std::fs::write(busy.join("notes.txt"), "hi").unwrap();
        let c = check(&busy);
        assert_eq!(c.kind, "new");
        assert_eq!(c.home, busy.join("Peko").display().to_string());

        let old = root.join("old");
        std::fs::create_dir_all(old.join("pekos").join("mochi")).unwrap();
        std::fs::create_dir_all(old.join("pekos").join("tofu")).unwrap();
        let c = check(&old);
        assert_eq!((c.kind, c.pekos), ("peko", Some(2)));

        let file = root.join("a-file");
        std::fs::write(&file, "").unwrap();
        assert_eq!(check(&file).problem, Some("notFolder"));
        assert_eq!(
            check(Path::new("relative/peko")).problem,
            Some("notAbsolute")
        );
        assert_eq!(
            check(&root.join("..").join("x")).problem,
            Some("notAbsolute")
        );
    }

    #[test]
    fn system_and_app_folders_are_refused() {
        let app = Path::new("/opt/peko-desktop");
        if cfg!(unix) {
            assert_eq!(
                system_folder(Path::new("/usr/local/peko"), None),
                Some("system")
            );
            assert_eq!(system_folder(Path::new("/"), None), Some("system"));
            assert_eq!(
                system_folder(Path::new("/Applications/Peko Desktop.app/Contents/x"), None),
                Some("insideApp")
            );
            assert_eq!(
                system_folder(&app.join("data"), Some(app)),
                Some("insideApp")
            );
            assert_eq!(system_folder(Path::new("/Users/ada/Peko"), None), None);
            assert_eq!(
                system_folder(&std::env::temp_dir().join("p"), None),
                Some("system")
            );
        }
    }

    #[test]
    fn a_long_folder_does_not_fit_the_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let long = tmp.path().join("a".repeat(MAX_SOCKET_PATH));
        let c = check_folder_with(&long, None, None);
        if cfg!(unix) {
            assert!(
                matches!(c.problem, Some("tooLong") | Some("system")),
                "{c:?}"
            );
        }
    }

    #[test]
    fn synced_and_other_drive_folders_are_flagged() {
        let home = Path::new("/Users/ada");
        let w = |p: &str| warnings_for(Path::new(p), Some(home));
        assert!(
            w("/Users/ada/Library/Mobile Documents/com~apple~CloudDocs/Peko").contains(&"synced")
        );
        assert!(w("/Users/ada/Library/CloudStorage/Dropbox/Peko").contains(&"synced"));
        // A folder on the system drive needs no warning. Windows names
        // the drive: a path without one counts as another drive.
        let plain = if cfg!(windows) {
            r"C:\Users\ada\Peko"
        } else {
            "/Users/ada/Peko"
        };
        assert!(w(plain).is_empty(), "{:?}", w(plain));
        if cfg!(windows) {
            assert!(w(r"D:\Peko").contains(&"removable"));
            assert!(w(r"C:\Users\ada\OneDrive\Peko").contains(&"synced"));
        }
        if cfg!(target_os = "macos") {
            assert!(w("/Volumes/T7/Peko").contains(&"removable"));
            assert!(w("/Users/ada/Documents/Peko").contains(&"protected"));
        }
    }
}
