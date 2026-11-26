//! The user's settings (`settings.json` in the Konak home) and the backup password.
//!
//! Settings are read and written whole by the panel's Settings page. Validation errors are short
//! codes the panel shows through the `errors.*` locale keys (`settings_time`, ...). The backup
//! password is never part of `Settings`: it is stored apart, protected with DPAPI for the current
//! Windows user (`backup-password.dpapi`), and is never returned, logged or put in a backup.

use crate::paths::{skip_elevation, Paths};
use age::secrecy::SecretString;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// `system` (the Windows display language), `tr` or `en`. Stored only; the panel applies it.
    pub language: String,
    /// Start Konak with Windows (the per-user Run value).
    pub autostart: bool,
    pub backup: BackupSettings,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct BackupSettings {
    /// A Windows folder (drive path or UNC share); `None` until the user picks one.
    pub location: Option<String>,
    pub schedule: Schedule,
    /// How many of Konak's backup files to keep in the location, 1 to 100.
    pub keep: u32,
    pub include: Include,
    /// Read-only: whether a backup password is stored. Filled in by `load`; a value sent by the
    /// panel is ignored.
    #[serde(skip_deserializing)]
    pub password_set: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Schedule {
    /// `off`, `daily` or `weekly`.
    pub kind: String,
    /// Local time, `HH:MM` (24 hours).
    pub time: String,
    /// For `weekly`: 0 = Sunday ... 6 = Saturday (JavaScript's `getDay`, Windows' `wDayOfWeek`).
    pub weekday: u8,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Include {
    /// App ids to back up; empty means every installed app.
    pub apps: Vec<String>,
    pub photos: bool,
    pub media: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { language: "system".into(), autostart: true, backup: BackupSettings::default() }
    }
}

impl Default for BackupSettings {
    fn default() -> Self {
        BackupSettings { location: None, schedule: Schedule::default(), keep: 7, include: Include::default(), password_set: false }
    }
}

impl Default for Schedule {
    fn default() -> Self {
        Schedule { kind: "daily".into(), time: "03:00".into(), weekday: 0 }
    }
}

impl Default for Include {
    /// Photos are in by default because Immich's database refers to them; media is often large and
    /// replaceable.
    fn default() -> Self {
        Include { apps: vec![], photos: true, media: false }
    }
}

pub const MAX_KEEP: u32 = 100;

fn file(paths: &Paths) -> PathBuf {
    paths.home.join("settings.json")
}

/// `HH:MM` as hours and minutes.
pub fn parse_time(t: &str) -> Option<(u8, u8)> {
    let (h, m) = t.split_once(':')?;
    if h.len() != 2 || m.len() != 2 || !h.bytes().chain(m.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some((h, m))
}

/// The settings, with `password_set` filled in. A missing file gives the defaults; a damaged one too,
/// with a line in the log (saving from the panel then replaces it).
pub fn load(paths: &Paths) -> Settings {
    let mut s = match std::fs::read_to_string(file(paths)) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            crate::log(paths, &format!("settings.json unreadable, using the defaults: {e}"));
            Settings::default()
        }),
        Err(_) => Settings::default(),
    };
    s.backup.password_set = password_set(paths);
    s
}

/// Lower-case, backslash-separated, without a trailing separator or a verbatim prefix.
fn norm(p: &Path) -> String {
    let p = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let s = p.to_string_lossy().replace('/', "\\");
    let s = s.strip_prefix(r"\\?\UNC\").map(|r| format!(r"\\{r}")).unwrap_or_else(|| s.strip_prefix(r"\\?\").unwrap_or(&s).to_string());
    s.trim_end_matches('\\').to_lowercase()
}

/// `inner` is `outer` or inside it.
pub fn inside(inner: &Path, outer: &Path) -> bool {
    let (i, o) = (norm(inner), norm(outer));
    i == o || i.starts_with(&format!("{o}\\"))
}

/// A drive path (`D:\...`) or a UNC share (`\\server\share\...`).
fn absolute_windows_path(p: &str) -> bool {
    let b = p.as_bytes();
    let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/');
    let unc = p.starts_with(r"\\") && !p.starts_with(r"\\?\") && !p.starts_with(r"\\.\") && p[2..].split('\\').filter(|s| !s.is_empty()).count() >= 2;
    drive || unc
}

/// A path into a WSL distro (`\\wsl$\...`, `\\wsl.localhost\...`), as written or once resolved
/// (a mapped drive, a link or a junction that leads there).
pub fn wsl_path(p: &str) -> bool {
    let wsl = |s: &str| {
        let l = s.replace('/', "\\").to_lowercase();
        ["\\\\wsl$", "\\\\wsl.localhost"].iter().any(|h| l == *h || l.starts_with(&format!("{h}\\")))
    };
    wsl(p) || wsl(&norm(Path::new(p)))
}

/// Checks a backup location: absolute, not inside a WSL distro, an existing folder, writable, and
/// outside the folders a backup reads from or a purge deletes. Error codes as `validate`.
pub fn check_location(paths: &Paths, location: &str) -> Result<(), String> {
    if !absolute_windows_path(location) {
        return Err("settings_location_relative".into());
    }
    // Backups inside the distro they protect would be lost with it, and could sit in a folder an
    // app's container mounts.
    if wsl_path(location) {
        return Err("settings_location_wsl".into());
    }
    let dir = Path::new(location);
    if !dir.is_dir() {
        return Err("settings_location_missing".into());
    }
    if inside(dir, &paths.photos) || inside(dir, &paths.media) {
        return Err("settings_location_in_library".into());
    }
    if inside(dir, &paths.home) {
        return Err("settings_location_in_konak".into());
    }
    let probe = dir.join(format!(".konak-write-test-{}", std::process::id()));
    std::fs::write(&probe, b"konak").map_err(|_| "settings_location_not_writable".to_string())?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Validation of what the panel sends. Each error is a locale key under `errors.`.
pub fn validate(paths: &Paths, s: &Settings) -> Result<(), String> {
    if !["system", "tr", "en"].contains(&s.language.as_str()) {
        return Err("settings_language".into());
    }
    let b = &s.backup;
    if !["off", "daily", "weekly"].contains(&b.schedule.kind.as_str()) {
        return Err("settings_schedule_kind".into());
    }
    if parse_time(&b.schedule.time).is_none() {
        return Err("settings_time".into());
    }
    if b.schedule.weekday > 6 {
        return Err("settings_weekday".into());
    }
    if !(1..=MAX_KEEP).contains(&b.keep) {
        return Err("settings_keep".into());
    }
    if b.include.apps.iter().any(|id| crate::catalog::get(id).is_none()) {
        return Err("settings_unknown_app".into());
    }
    if let Some(loc) = &b.location {
        check_location(paths, loc)?;
    }
    Ok(())
}

/// Serializes writes of `settings.json` in this process.
static SAVE_LOCK: Mutex<()> = Mutex::new(());

/// Validates and stores the settings, applies the autostart choice when it changed, and resets the
/// backup schedule's starting point when the schedule changed (so a new schedule starts with its
/// next slot, not with one that passed before it was set). Returns the settings as now stored.
pub fn save(paths: &Paths, new: &Settings) -> Result<Settings, String> {
    let mut new = new.clone();
    new.backup.location = new.backup.location.map(|l| l.trim().to_string()).filter(|l| !l.is_empty());
    new.backup.include.apps.sort();
    new.backup.include.apps.dedup();
    validate(paths, &new)?;
    let _guard = SAVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let old = load(paths);
    if old.autostart != new.autostart {
        apply_autostart(new.autostart)?;
    }
    new.backup.password_set = false;
    write_atomic(&file(paths), serde_json::to_string_pretty(&new).unwrap().as_bytes()).map_err(|e| format!("cannot save settings.json: {e}"))?;
    if old.backup.schedule != new.backup.schedule {
        crate::backup::reset_schedule(paths);
    }
    Ok(load(paths))
}

/// Adds or removes Konak's Run value. Dev runs (`KONAK_SKIP_ELEVATION=1`) never touch it, as
/// `installer::register_autostart`.
fn apply_autostart(on: bool) -> Result<(), String> {
    if skip_elevation() {
        return Ok(());
    }
    if on {
        crate::installer::register_autostart();
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    crate::uninstall::remove_run_value(&exe).map(|_| ()).map_err(|_| "autostart_failed".to_string())
}

/// Writes a temporary file, flushes it and renames it over `path`.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Unique per call, not only per process: two threads writing the same file at once must not
    // share a temp file (the second rename would find it gone: "os error 2").
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp{}-{seq}", std::process::id()));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

// ---- Backup password ---------------------------------------------------------------------------

pub const MIN_PASSWORD_CHARS: usize = 12;
pub const MAX_PASSWORD_CHARS: usize = 1024;

fn password_file(paths: &Paths) -> PathBuf {
    paths.home.join("backup-password.dpapi")
}

pub fn password_set(paths: &Paths) -> bool {
    password_file(paths).is_file()
}

/// The stored backup password, or `None` when none is set. An error when the file exists and cannot
/// be unprotected (another Windows user, a profile restored without its DPAPI keys).
pub fn password(paths: &Paths) -> Result<Option<SecretString>, String> {
    let blob = match std::fs::read(password_file(paths)) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read the backup password: {e}")),
    };
    let plain = dpapi::unprotect(&blob).map_err(|e| format!("cannot unprotect the backup password: {e}"))?;
    Ok(Some(SecretString::from(plain)))
}

static PASSWORD_LOCK: Mutex<()> = Mutex::new(());

/// Sets or changes the backup password. Once one is set, `current` must match it. Backups made
/// before a change keep needing the password they were made with.
pub fn set_password(paths: &Paths, current: Option<&str>, new: &str) -> Result<(), String> {
    use age::secrecy::ExposeSecret;
    let _guard = PASSWORD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(stored) = password(paths)? {
        let Some(current) = current else { return Err("password_current_required".into()) };
        if !constant_time_eq(stored.expose_secret().as_bytes(), current.as_bytes()) {
            return Err("password_current_wrong".into());
        }
    }
    let n = new.chars().count();
    if n < MIN_PASSWORD_CHARS {
        return Err("password_too_short".into());
    }
    if n > MAX_PASSWORD_CHARS {
        return Err("password_too_long".into());
    }
    let blob = dpapi::protect(new.as_bytes()).map_err(|e| format!("cannot protect the backup password: {e}"))?;
    write_atomic(&password_file(paths), &blob).map_err(|e| format!("cannot save the backup password: {e}"))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// DPAPI for the current Windows user (`CryptProtectData` without `LOCAL_MACHINE`), with an entropy
/// value of Konak's own so another program's blob is never accepted as Konak's.
pub mod dpapi {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB};

    const ENTROPY: &[u8] = b"konak-backup-password-v1";

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
    }

    /// Copies the output blob, wipes it and frees it.
    fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        // SAFETY: DPAPI returned `cbData` bytes at `pbData`, allocated with LocalAlloc; they are
        // copied, overwritten and freed once.
        unsafe {
            let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            std::ptr::write_bytes(out.pbData, 0, out.cbData as usize);
            LocalFree(out.pbData as _);
            v
        }
    }

    pub fn protect(data: &[u8]) -> Result<Vec<u8>, String> {
        let (input, entropy) = (blob(data), blob(ENTROPY));
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: the input blobs point at live slices for the call; `out` receives a LocalAlloc buffer.
        let ok = unsafe { CryptProtectData(&input, std::ptr::null(), &entropy, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        Ok(take(out))
    }

    pub fn unprotect(data: &[u8]) -> Result<String, String> {
        let (input, entropy) = (blob(data), blob(ENTROPY));
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: as in `protect`.
        let ok = unsafe { CryptUnprotectData(&input, std::ptr::null_mut(), &entropy, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        String::from_utf8(take(out)).map_err(|_| "not text".to_string())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn concurrent_atomic_writes_of_one_file_all_succeed() {
        let dir = std::env::temp_dir().join(format!("konak-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("state.json");
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let file = file.clone();
                std::thread::spawn(move || (0..50).map(|j| write_atomic(&file, format!("{i}-{j}").as_bytes())).collect::<Vec<_>>())
            })
            .collect();
        let results: Vec<_> = handles.into_iter().flat_map(|h| h.join().unwrap()).collect();
        let _ = std::fs::remove_dir_all(&dir);
        // Rename onto a file another thread holds open can still fail on Windows; a vanished temp
        // file (os error 2) must not happen any more.
        assert!(results.iter().all(|r| r.as_ref().map_err(|e| e.kind()) != Err(std::io::ErrorKind::NotFound)), "{results:?}");
    }

    pub fn temp_paths(tag: &str) -> Paths {
        let home = std::env::temp_dir().join(format!("konak-settings-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        Paths { home: home.clone(), distro: "konak-nonexistent".into(), bin: home.join("bin"), media: home.join("Media"), photos: home.join("Photos") }
    }

    #[test]
    fn defaults_match_the_plan() {
        let paths = temp_paths("defaults");
        let s = load(&paths);
        let _ = std::fs::remove_dir_all(&paths.home);
        assert_eq!(s.language, "system");
        assert!(s.autostart);
        let b = &s.backup;
        assert_eq!((b.location.as_deref(), b.keep, b.password_set), (None, 7, false));
        assert_eq!(b.schedule, Schedule { kind: "daily".into(), time: "03:00".into(), weekday: 0 });
        assert_eq!(b.include, Include { apps: vec![], photos: true, media: false });
        // Fields missing from an older file take their defaults.
        let partial: Settings = serde_json::from_str(r#"{"language":"tr","backup":{"keep":3}}"#).unwrap();
        assert_eq!((partial.language.as_str(), partial.autostart, partial.backup.keep), ("tr", true, 3));
        assert_eq!(partial.backup.schedule.time, "03:00");
        // password_set from the panel is ignored.
        let sent: Settings = serde_json::from_str(r#"{"backup":{"password_set":true}}"#).unwrap();
        assert!(!sent.backup.password_set);
    }

    #[test]
    fn validation_codes() {
        let paths = temp_paths("validate");
        let out = std::env::temp_dir().join(format!("konak-settings-out-{}", std::process::id()));
        std::fs::create_dir_all(&out).unwrap();
        std::fs::create_dir_all(paths.photos.join("sub")).unwrap();
        let ok = Settings::default();
        let with = |f: &dyn Fn(&mut Settings)| {
            let mut s = ok.clone();
            f(&mut s);
            validate(&paths, &s).err()
        };
        let results = [
            (with(&|_| {}), None),
            (with(&|s| s.language = "de".into()), Some("settings_language")),
            (with(&|s| s.backup.schedule.kind = "hourly".into()), Some("settings_schedule_kind")),
            (with(&|s| s.backup.schedule.time = "24:00".into()), Some("settings_time")),
            (with(&|s| s.backup.schedule.time = "3:00".into()), Some("settings_time")),
            (with(&|s| s.backup.schedule.time = "03:60".into()), Some("settings_time")),
            (with(&|s| s.backup.schedule.weekday = 7), Some("settings_weekday")),
            (with(&|s| s.backup.keep = 0), Some("settings_keep")),
            (with(&|s| s.backup.keep = 101), Some("settings_keep")),
            (with(&|s| s.backup.keep = 100), None),
            (with(&|s| s.backup.include.apps = vec!["jellyfin".into(), "nosuch".into()]), Some("settings_unknown_app")),
            (with(&|s| s.backup.location = Some("backups".into())), Some("settings_location_relative")),
            (with(&|s| s.backup.location = Some(r"\\?\C:\x".into())), Some("settings_location_relative")),
            (with(&|s| s.backup.location = Some(r"\\wsl.localhost\konak\var\lib\konak\apps\jellyfin\config".into())), Some("settings_location_wsl")),
            (with(&|s| s.backup.location = Some(r"\\WSL$\konak\tmp".into())), Some("settings_location_wsl")),
            (with(&|s| s.backup.location = Some("//wsl.localhost/konak/tmp".into())), Some("settings_location_relative")),
            (with(&|s| s.backup.location = Some(out.join("missing").to_string_lossy().into())), Some("settings_location_missing")),
            (with(&|s| s.backup.location = Some(paths.home.to_string_lossy().into())), Some("settings_location_in_konak")),
            (with(&|s| s.backup.location = Some(paths.photos.join("sub").to_string_lossy().into())), Some("settings_location_in_library")),
            (with(&|s| s.backup.location = Some(out.to_string_lossy().into())), None),
        ];
        let leftovers = std::fs::read_dir(&out).unwrap().count();
        let _ = std::fs::remove_dir_all(&paths.home);
        let _ = std::fs::remove_dir_all(&out);
        for (i, (got, want)) in results.iter().enumerate() {
            assert_eq!(got.as_deref(), *want, "case {i}");
        }
        assert_eq!(leftovers, 0, "the write probe is removed");
        assert!(absolute_windows_path(r"\\nas\share\konak") && !absolute_windows_path(r"\\nas"));
        assert!(wsl_path(r"\\wsl$\konak\x") && wsl_path("//wsl.localhost/konak/x") && wsl_path(r"\\wsl.localhost"));
        // Local paths only here: resolving a UNC path would ask that host.
        assert!(!wsl_path(r"C:\wsl.localhost\x") && !wsl_path(r"D:\wsl$\x"));
        assert_eq!(parse_time("23:59"), Some((23, 59)));
    }

    #[test]
    fn save_round_trips_and_never_stores_password_set() {
        let paths = temp_paths("save");
        let mut s = Settings { language: "en".into(), autostart: true, ..Settings::default() };
        s.backup.include.apps = vec!["nextcloud".into(), "jellyfin".into(), "nextcloud".into()];
        s.backup.password_set = true;
        let saved = save(&paths, &s).unwrap();
        let text = std::fs::read_to_string(file(&paths)).unwrap();
        let _ = std::fs::remove_dir_all(&paths.home);
        assert_eq!(saved.backup.include.apps, ["jellyfin", "nextcloud"]);
        assert!(!saved.backup.password_set, "no password stored");
        assert_eq!(saved.language, "en");
        assert!(text.contains("\"password_set\": false"));
        assert!(save(&paths, &Settings { language: "xx".into(), ..Settings::default() }).is_err());
    }

    #[test]
    fn dpapi_round_trip() {
        let blob = dpapi::protect("correct horse battery".as_bytes()).unwrap();
        assert!(!blob.windows(7).any(|w| w == b"correct"), "the blob is not plain text");
        assert_eq!(dpapi::unprotect(&blob).unwrap(), "correct horse battery");
        let mut bad = blob.clone();
        let mid = bad.len() / 2;
        bad[mid] ^= 1;
        assert!(dpapi::unprotect(&bad).is_err());
    }

    #[test]
    fn the_password_needs_the_current_one_to_change() {
        use age::secrecy::ExposeSecret;
        let paths = temp_paths("password");
        let first = set_password(&paths, None, "short");
        let set = set_password(&paths, None, "a long enough secret");
        let is_set = password_set(&paths);
        let no_current = set_password(&paths, None, "another long secret");
        let wrong = set_password(&paths, Some("not it at all"), "another long secret");
        let changed = set_password(&paths, Some("a long enough secret"), "another long secret");
        let stored = password(&paths).unwrap().unwrap().expose_secret().to_string();
        let on_disk = std::fs::read(password_file(&paths)).unwrap();
        let _ = std::fs::remove_dir_all(&paths.home);
        assert_eq!(first.unwrap_err(), "password_too_short");
        assert!(set.is_ok() && is_set);
        assert_eq!(no_current.unwrap_err(), "password_current_required");
        assert_eq!(wrong.unwrap_err(), "password_current_wrong");
        assert!(changed.is_ok());
        assert_eq!(stored, "another long secret");
        assert!(!on_disk.windows(7).any(|w| w == b"another"));
        assert_eq!(set_password(&temp_paths("pw-long"), None, &"x".repeat(1025)).unwrap_err(), "password_too_long");
        let _ = std::fs::remove_dir_all(temp_paths("pw-long").home);
    }
}
