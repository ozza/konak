//! The backup engine: one encrypted file per run in the user's backup folder (format in
//! `backup_format`), the schedule, retention and the history the Settings page lists.
//!
//! Consistency, "the safe way" of the plan: apps are archived one at a time, each stopped
//! (`docker-compose stop`) while its folder and named volumes stream out of the distro, then started
//! again if it was running. Every database is thus copied at rest. The Windows Photos and Media
//! folders are plain files, copied after every app's part while the apps run.
//!
//! A run writes `<location>\konak-<YYYYMMDD-HHMMSS>.konakbackup.partial`, re-reads all of it
//! (decrypt and checksums), and only then renames it to `.konakbackup`. A failure removes the
//! partial file. Errors the panel explains are short codes (`backup_no_location`, ...).

use crate::backup_format::{self as fmt, AppManifest, Manifest, VolumeManifest, Writer};
use crate::installer::{now, InstalledApp, State};
use crate::paths::Paths;
use crate::settings::{self, Schedule};
use crate::shell;
use age::secrecy::SecretString;
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use time::{Date, Month, PrimitiveDateTime, Time};

const EXT: &str = ".konakbackup";
const PARTIAL: &str = ".partial";
/// Room kept free beyond the estimate.
const SPACE_MARGIN: u64 = 64 << 20;
/// How long an app's turn waits for another operation on it to end.
const BUSY_WAIT: Duration = Duration::from_secs(10 * 60);
const HISTORY_MAX: usize = 30;
/// Named volumes that are not backed up (re-downloadable).
const SKIP_VOLUMES: &[(&str, &str)] = &[("immich", "model-cache")];

// ---- One run at a time ---------------------------------------------------------------------------

static RUNNING: AtomicBool = AtomicBool::new(false);

/// Held for a backup's (and later a restore's) whole run; never two at once.
pub struct Guard(());

impl Guard {
    pub fn acquire() -> Result<Guard, String> {
        RUNNING.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).map(|_| Guard(())).map_err(|_| "backup_running".to_string())
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

pub fn running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

// ---- What the engine needs from its caller ---------------------------------------------------------

/// `backup-progress` event payload (contract).
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Progress {
    /// running | done | failed
    pub state: &'static str,
    /// prepare | app | konak | photos | media | verify | finish | done
    pub step: &'static str,
    pub app: Option<String>,
    pub done_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub error: Option<String>,
}

/// The running Konak: progress events and the busy set the panel's app operations use.
pub trait Host {
    fn progress(&self, p: &Progress);
    /// Marks the app busy (op `backup`); false when another operation holds it.
    fn try_busy(&self, id: &str) -> bool;
    fn release(&self, id: &str, result: &Result<(), String>);
}

/// Stopping and starting an app's containers.
pub trait Apps {
    /// Whether any of the app's containers runs.
    fn running(&self, id: &str) -> Result<bool, String>;
    fn stop(&self, id: &str) -> Result<(), String>;
    fn start(&self, id: &str) -> Result<(), String>;
}

/// `Apps` through docker-compose in Konak's distro.
pub struct DistroApps<'a>(pub &'a Paths);

const RUNNING_SH: &str = "set -euo pipefail\npodman ps -q --filter \"label=com.docker.compose.project=$1\" --filter status=running </dev/null\n";

impl Apps for DistroApps<'_> {
    fn running(&self, id: &str) -> Result<bool, String> {
        let out = shell::distro_script(&self.0.distro, RUNNING_SH, &[id]);
        if !out.ok {
            return Err(format!("podman ps for {id} failed: {}", shell::tail(&out.text, 3)));
        }
        Ok(out.text.lines().any(|l| !l.trim().is_empty() && l.trim().bytes().all(|b| b.is_ascii_hexdigit())))
    }
    fn stop(&self, id: &str) -> Result<(), String> {
        // Up to a minute per container for a clean database shutdown.
        let out = crate::apps::compose(self.0, id, "stop -t 60");
        out.ok.then_some(()).ok_or_else(|| format!("stop of {id} failed: {}", shell::tail(&out.text, 4)))
    }
    fn start(&self, id: &str) -> Result<(), String> {
        let out = crate::apps::compose(self.0, id, "start");
        out.ok.then_some(()).ok_or_else(|| format!("start of {id} failed: {}", shell::tail(&out.text, 4)))
    }
}

/// Runs `f` with app `id` stopped, and starts it again afterwards only if it was running, whether
/// `f` succeeded or not. A stop that fails is undone with a start.
pub fn with_stopped<T>(apps: &dyn Apps, id: &str, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let was_running = apps.running(id)?;
    if was_running {
        if let Err(e) = apps.stop(id) {
            let back = apps.start(id).err().map(|b| format!("; starting it again failed too: {b}")).unwrap_or_default();
            return Err(format!("{e}{back}"));
        }
    }
    let result = f();
    if was_running {
        if let Err(e) = apps.start(id) {
            return Err(match result {
                Ok(_) => format!("{id} was backed up, but did not start again: {e}"),
                Err(r) => format!("{r}; {id} did not start again: {e}"),
            });
        }
    }
    result
}

// ---- File names and local time ---------------------------------------------------------------------

pub fn file_name(t: PrimitiveDateTime) -> String {
    format!("konak-{:04}{:02}{:02}-{:02}{:02}{:02}{EXT}", t.year(), t.month() as u8, t.day(), t.hour(), t.minute(), t.second())
}

/// `konak-YYYYMMDD-HHMMSS.konakbackup`, exactly.
pub fn is_backup_name(name: &str) -> bool {
    let Some(stamp) = name.strip_prefix("konak-").and_then(|n| n.strip_suffix(EXT)) else { return false };
    let b = stamp.as_bytes();
    b.len() == 15 && b[8] == b'-' && b.iter().enumerate().all(|(i, c)| i == 8 || c.is_ascii_digit())
}

fn is_partial_name(name: &str) -> bool {
    name.strip_suffix(PARTIAL).is_some_and(is_backup_name)
}

/// Local civil time of a Unix time, with the time zone rules of that moment (DST included).
pub fn local_of(unix: u64) -> Option<PrimitiveDateTime> {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    let t = (unix + 11_644_473_600) * 10_000_000;
    let ft = FILETIME { dwLowDateTime: t as u32, dwHighDateTime: (t >> 32) as u32 };
    // SAFETY: plain conversions between stack structs.
    let local = unsafe {
        let (mut utc, mut local): (SYSTEMTIME, SYSTEMTIME) = (std::mem::zeroed(), std::mem::zeroed());
        if FileTimeToSystemTime(&ft, &mut utc) == 0 || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) == 0 {
            return None;
        }
        local
    };
    let date = Date::from_calendar_date(local.wYear as i32, Month::try_from(local.wMonth as u8).ok()?, local.wDay as u8).ok()?;
    let time = Time::from_hms(local.wHour as u8, local.wMinute as u8, local.wSecond as u8).ok()?;
    Some(PrimitiveDateTime::new(date, time))
}

// ---- Schedule ------------------------------------------------------------------------------------

/// The latest scheduled moment at or before `now`; `None` when the schedule is off or invalid.
pub fn latest_slot(s: &Schedule, now: PrimitiveDateTime) -> Option<PrimitiveDateTime> {
    let (h, m) = settings::parse_time(&s.time)?;
    let at = Time::from_hms(h, m, 0).ok()?;
    let mut day = now.date();
    let max_back = match s.kind.as_str() {
        "daily" => 1,
        "weekly" if s.weekday <= 6 => 7,
        _ => return None,
    };
    for _ in 0..=max_back {
        let slot = PrimitiveDateTime::new(day, at);
        let weekday_ok = s.kind == "daily" || day.weekday().number_days_from_sunday() == s.weekday;
        if weekday_ok && slot <= now {
            return Some(slot);
        }
        day = day.previous_day()?;
    }
    None
}

/// Whether a scheduled run is due: a slot has come since `last` (the last scheduled run, or when the
/// schedule was set). A run missed while the PC was off is due at the next check.
pub fn due(s: &Schedule, last: PrimitiveDateTime, now: PrimitiveDateTime) -> bool {
    latest_slot(s, now).is_some_and(|slot| slot > last)
}

#[derive(Serialize, Deserialize)]
struct ScheduleState {
    /// The last scheduled run, or when the schedule was set or changed.
    baseline_unix: u64,
}

fn schedule_file(paths: &Paths) -> PathBuf {
    paths.home.join("backup-schedule.json")
}

fn write_baseline(paths: &Paths, unix: u64) {
    let body = serde_json::to_vec(&ScheduleState { baseline_unix: unix }).unwrap();
    if let Err(e) = settings::write_atomic(&schedule_file(paths), &body) {
        crate::log(paths, &format!("backup schedule: cannot save its state: {e}"));
    }
}

/// A new or changed schedule starts counting now.
pub fn reset_schedule(paths: &Paths) {
    write_baseline(paths, now());
}

/// The slot whose skip was last logged, so a skipped slot is logged once, not every tick.
static SKIP_LOGGED: Mutex<Option<PrimitiveDateTime>> = Mutex::new(None);

/// Called every supervisor tick: true when a scheduled backup should start now (the caller starts
/// it). A due run with no password or no location is skipped with one log line, and runs once they
/// are set (the slot is still due then).
pub fn scheduled_due(paths: &Paths) -> bool {
    let s = settings::load(paths);
    let sched = &s.backup.schedule;
    if sched.kind == "off" || running() {
        return false;
    }
    let t = now();
    let baseline = match std::fs::read(schedule_file(paths)).ok().and_then(|b| serde_json::from_slice::<ScheduleState>(&b).ok()) {
        Some(b) => b.baseline_unix,
        None => {
            write_baseline(paths, t);
            return false;
        }
    };
    let (Some(last), Some(local)) = (local_of(baseline), local_of(t)) else { return false };
    if !due(sched, last, local) {
        return false;
    }
    let reason = if s.backup.location.is_none() {
        Some("no backup location is set")
    } else if !s.backup.password_set {
        Some("no backup password is set")
    } else {
        None
    };
    if let Some(reason) = reason {
        let slot = latest_slot(sched, local);
        let mut logged = SKIP_LOGGED.lock().unwrap_or_else(|e| e.into_inner());
        if *logged != slot {
            *logged = slot;
            crate::log(paths, &format!("scheduled backup skipped: {reason}"));
        }
        return false;
    }
    write_baseline(paths, t);
    true
}

// ---- History and listing ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct HistoryEntry {
    file: String,
    created_unix: u64,
    size_bytes: u64,
    apps: Vec<String>,
    /// ok | failed
    status: String,
    error: Option<String>,
    step: Option<String>,
}

fn history_file(paths: &Paths) -> PathBuf {
    paths.home.join("backup-history.json")
}

fn load_history(paths: &Paths) -> Vec<HistoryEntry> {
    std::fs::read(history_file(paths)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

static HISTORY_LOCK: Mutex<()> = Mutex::new(());

fn record(paths: &Paths, e: HistoryEntry) {
    let _guard = HISTORY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut h = load_history(paths);
    h.insert(0, e);
    h.truncate(HISTORY_MAX);
    if let Err(e) = settings::write_atomic(&history_file(paths), &serde_json::to_vec_pretty(&h).unwrap()) {
        crate::log(paths, &format!("backup history not saved: {e}"));
    }
}

/// `list_backups` row (contract).
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct BackupEntry {
    pub file: String,
    pub created_unix: u64,
    pub size_bytes: u64,
    pub apps: Vec<String>,
    /// ok (verified when written) | failed (no file) | unverified (a Konak file this PC has no
    /// record of, e.g. copied from another PC)
    pub status: &'static str,
    pub error: Option<String>,
}

fn same_file(a: &str, b: &str) -> bool {
    a.replace('/', "\\").eq_ignore_ascii_case(&b.replace('/', "\\"))
}

/// Konak's backup files in `dir`: the name pattern and the magic line.
fn konak_files(dir: &Path) -> Vec<(String, std::fs::Metadata)> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    rd.flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            let meta = e.metadata().ok()?;
            (meta.is_file() && is_backup_name(&name) && fmt::has_magic(&e.path())).then_some((name, meta))
        })
        .collect()
}

/// The backup files in the location, plus the recent failed runs, newest first.
pub fn list(paths: &Paths) -> Vec<BackupEntry> {
    let history = load_history(paths);
    let mut out = vec![];
    if let Some(loc) = settings::load(paths).backup.location {
        for (name, meta) in konak_files(Path::new(&loc)) {
            let file = Path::new(&loc).join(&name).to_string_lossy().to_string();
            let known = history.iter().find(|h| h.status == "ok" && same_file(&h.file, &file));
            let mtime = meta.modified().ok().and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
            out.push(BackupEntry {
                file,
                created_unix: known.map_or(mtime, |h| h.created_unix),
                size_bytes: meta.len(),
                apps: known.map(|h| h.apps.clone()).unwrap_or_default(),
                status: if known.is_some() { "ok" } else { "unverified" },
                error: None,
            });
        }
    }
    for h in history.iter().filter(|h| h.status == "failed") {
        out.push(BackupEntry { file: h.file.clone(), created_unix: h.created_unix, size_bytes: 0, apps: h.apps.clone(), status: "failed", error: h.error.clone() });
    }
    out.sort_by_key(|e| std::cmp::Reverse(e.created_unix));
    out
}

/// `inspect_backup`'s answer (contract `BackupManifest`): the manifest, plus the backup's root CA.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Inspected {
    #[serde(flatten)]
    pub manifest: Manifest,
    /// SHA-256 of the backup's root certificate (as `ca_info` shows it), when it has one.
    pub ca_fingerprint: Option<String>,
    /// The root passes the restore's check (`ca::check_restored_root`).
    pub ca_valid: bool,
}

/// The manifest of a backup file and its root CA. An empty `password` uses the stored one. The CA
/// files come after the apps in the file, so this reads (and decrypts) up to them; the rest of the
/// file is not checked here, a restore verifies all of it first.
pub fn inspect(paths: &Paths, path: &str, password: &str) -> Result<Inspected, String> {
    let pw = if password.is_empty() { settings::password(paths)?.ok_or("backup_no_password")? } else { SecretString::from(password.to_string()) };
    let b = fmt::open(Path::new(path), &pw).map_err(|e| {
        crate::log(paths, &format!("inspect backup: {e}"));
        e.code.to_string()
    })?;
    let manifest = b.manifest.clone();
    let (mut crt, mut key) = (None, None);
    if manifest.konak_files.iter().any(|f| f == "ca/root.crt") {
        // Stopped with an error once both are read, or at the first Photos/Media entry.
        let _ = b.for_each_entry(|e, _, r| {
            let slot = match e {
                fmt::Entry::Konak(p) if p == "ca/root.crt" => &mut crt,
                fmt::Entry::Konak(p) if p == "ca/root.key" => &mut key,
                fmt::Entry::Photos(_) | fmt::Entry::Media(_) => return Err("past the CA".into()),
                _ => return Ok(()),
            };
            let mut body = String::new();
            std::io::Read::read_to_string(&mut std::io::Read::take(r, 1 << 20), &mut body).map_err(|e| e.to_string())?;
            *slot = Some(body);
            if crt.is_some() && key.is_some() {
                return Err("both read".into());
            }
            Ok(())
        });
    }
    let ca_fingerprint = crt.as_deref().and_then(crate::ca::restored_fingerprint);
    let ca_valid = matches!((&crt, &key), (Some(c), Some(k)) if crate::ca::check_restored_root(c, k).is_ok());
    Ok(Inspected { manifest, ca_fingerprint, ca_valid })
}

// ---- Retention -------------------------------------------------------------------------------------

/// How far ahead of this PC's clock a backup's time may be and still count (clocks of two PCs).
const FUTURE_SLACK: u64 = 5 * 60;

/// Of `(name, created_unix)` pairs, those beyond the newest `keep`, newest by the authenticated
/// creation time (the name breaks ties).
pub fn prune_candidates(mut found: Vec<(String, u64)>, keep: usize) -> Vec<String> {
    found.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
    found.into_iter().skip(keep).map(|(n, _)| n).collect()
}

/// Deletes Konak's backups beyond the newest `keep` in `dir`. `written` is the file this run just
/// wrote and verified: it takes one of the `keep` places and is never deleted. Of the other files
/// with Konak's name pattern and magic line, only those that open with `password` (age header,
/// manifest entry and its tar checksum: a quick check, not a full verify, which would read every
/// file end to end) and whose manifest time is not in the future are counted, ranked by that time,
/// and may be deleted. A file that does not open (another password, planted, damaged) or has a
/// future time is neither counted nor deleted, with a log line. Returns the deleted names.
pub fn prune(paths: &Paths, dir: &Path, keep: usize, password: &SecretString, written: &Path) -> Vec<String> {
    let written_name = written.file_name().map(|n| n.to_string_lossy().to_string());
    let limit = now() + FUTURE_SLACK;
    let mut counted = vec![];
    for (name, _) in konak_files(dir) {
        if Some(&name) == written_name.as_ref() {
            continue;
        }
        match fmt::open(&dir.join(&name), password) {
            Ok(b) if b.manifest.created_unix > limit => crate::log(paths, &format!("backup retention: {name} not counted, its time is in the future")),
            Ok(b) => counted.push((name, b.manifest.created_unix)),
            Err(e) => crate::log(paths, &format!("backup retention: {name} not counted, it does not open with the current password ({})", e.code)),
        }
    }
    let mut deleted = vec![];
    for name in prune_candidates(counted, keep.saturating_sub(1)) {
        match std::fs::remove_file(dir.join(&name)) {
            Ok(()) => deleted.push(name),
            Err(e) => crate::log(paths, &format!("backup retention: cannot delete {name}: {e}")),
        }
    }
    if !deleted.is_empty() {
        crate::log(paths, &format!("backup retention: deleted {}", deleted.join(", ")));
    }
    deleted
}

/// Partial files of an earlier run that did not finish (Konak was closed or crashed).
fn remove_stale_partials(paths: &Paths, dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Some(name) = e.file_name().to_str().map(String::from) else { continue };
        let empty = e.metadata().is_ok_and(|m| m.is_file() && m.len() == 0);
        if is_partial_name(&name) && (empty || fmt::has_magic(&e.path())) {
            match std::fs::remove_file(e.path()) {
                Ok(()) => crate::log(paths, &format!("backup: removed {name}, left by a run that did not finish")),
                Err(err) => crate::log(paths, &format!("backup: cannot remove {name}: {err}")),
            }
        }
    }
}

// ---- Sizes ---------------------------------------------------------------------------------------

#[derive(Debug, PartialEq, Clone)]
pub struct AppSize {
    pub id: String,
    pub folder: u64,
    pub volumes: Vec<VolumeSize>,
}

#[derive(Debug, PartialEq, Clone)]
pub struct VolumeSize {
    pub podman_name: String,
    pub name: String,
    pub bytes: u64,
}

/// Folder and named-volume sizes of the given apps, and which volumes they have.
const SIZES_SH: &str = r#"set -uo pipefail
for id in "$@"; do
  [[ "$id" =~ ^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$ ]] || { echo "bad app id: $id" >&2; exit 2; }
  d=/var/lib/konak/apps/$id
  [ -d "$d" ] || { echo "no folder for $id" >&2; exit 3; }
  echo "F $id $(du -sb --one-file-system "$d" </dev/null | cut -f1)"
  vols=$(podman volume ls -q --filter "label=com.docker.compose.project=$id" </dev/null) || { echo "podman volume ls failed for $id" >&2; exit 4; }
  for v in $vols; do
    name=$(podman volume inspect "$v" --format '{{index .Labels "com.docker.compose.volume"}}' </dev/null) || exit 4
    mp=$(podman volume inspect "$v" --format '{{.Mountpoint}}' </dev/null) || exit 4
    echo "V $id $v ${name:-$v} $(du -sb "$mp" </dev/null | cut -f1)"
  done
done
echo end
"#;

pub fn parse_sizes(text: &str, ids: &[&str]) -> Result<Vec<AppSize>, String> {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.last() != Some(&"end") {
        return Err(format!("size check failed: {}", shell::tail(text, 3)));
    }
    let mut out: Vec<AppSize> = ids.iter().map(|id| AppSize { id: id.to_string(), folder: 0, volumes: vec![] }).collect();
    for l in &lines[..lines.len() - 1] {
        let f: Vec<&str> = l.split(' ').collect();
        fn app<'a>(out: &'a mut [AppSize], id: &str) -> Result<&'a mut AppSize, String> {
            out.iter_mut().find(|a| a.id == id).ok_or_else(|| "size check: unknown app".to_string())
        }
        match f[..] {
            ["F", id, n] => app(&mut out, id)?.folder = n.parse().unwrap_or(0),
            ["V", id, podman, name, n] => {
                if SKIP_VOLUMES.contains(&(id, name)) {
                    continue;
                }
                app(&mut out, id)?.volumes.push(VolumeSize { podman_name: podman.into(), name: name.into(), bytes: n.parse().unwrap_or(0) });
            }
            // Tool warnings (podman prints some on stderr, which follows stdout here).
            _ => {}
        }
    }
    for a in &out {
        if let Some(v) = a.volumes.iter().find(|v| fmt::parse_entry(&format!("apps/{}/volumes/{}/000000", a.id, v.name)).is_none()) {
            return Err(format!("volume name {} of {} cannot be backed up", v.name, a.id));
        }
    }
    Ok(out)
}

/// Bytes free for this user on the volume holding `dir` (a drive or a network share).
fn free_space(dir: &Path) -> Result<u64, String> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = dir.as_os_str().to_string_lossy().encode_utf16().chain(Some(0)).collect();
    let mut avail = 0u64;
    // SAFETY: NUL-terminated path; the other outputs are optional.
    if unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, std::ptr::null_mut(), std::ptr::null_mut()) } == 0 {
        return Err(format!("cannot read the free space of {}: {}", dir.display(), std::io::Error::last_os_error()));
    }
    Ok(avail)
}

/// Every regular file under `root` in name order, with its `/`-separated relative path. Links and
/// junctions are not followed. Entries that cannot be read go to `skipped`.
fn walk(root: &Path, skipped: &mut Vec<String>, label: &str, f: &mut dyn FnMut(&str, &Path, &std::fs::Metadata) -> Result<(), String>) -> Result<(), String> {
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let mut entries: Vec<_> = match std::fs::read_dir(&dir) {
            Ok(rd) => rd.flatten().collect(),
            Err(e) => {
                skipped.push(format!("{label}/{rel} (unreadable: {e})"));
                continue;
            }
        };
        entries.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
        for e in entries {
            let Some(name) = e.file_name().to_str().map(String::from) else {
                skipped.push(format!("{label}/{rel}/{} (name is not Unicode)", e.file_name().to_string_lossy()));
                continue;
            };
            let child = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            let Ok(meta) = e.metadata() else {
                skipped.push(format!("{label}/{child} (unreadable)"));
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            } else if meta.is_dir() {
                stack.push((e.path(), child));
            } else if meta.is_file() && !name.ends_with(PARTIAL) {
                f(&child, &e.path(), &meta)?;
            }
        }
    }
    Ok(())
}

fn folder_size(root: &Path) -> u64 {
    let mut total = 0;
    let _ = walk(root, &mut vec![], "", &mut |_, _, m| {
        total += m.len();
        Ok(())
    });
    total
}

// ---- The run ---------------------------------------------------------------------------------------

/// Where a run failed.
#[derive(Debug, PartialEq)]
pub struct Failed {
    pub step: &'static str,
    pub app: Option<String>,
    pub error: String,
}

fn fail(step: &'static str, error: impl Into<String>) -> Failed {
    Failed { step, app: None, error: error.into() }
}

/// Progress events, at most twice a second for byte counts.
struct Ctx<'a> {
    host: &'a dyn Host,
    total: Cell<u64>,
    last: Cell<Option<Instant>>,
}

impl Ctx<'_> {
    fn step(&self, step: &'static str, app: Option<&str>, done: u64) {
        self.last.set(Some(Instant::now()));
        self.host.progress(&Progress { state: "running", step, app: app.map(String::from), done_bytes: Some(done.min(self.total.get())), total_bytes: Some(self.total.get()), error: None });
    }
    fn bytes(&self, step: &'static str, app: Option<&str>, done: u64) {
        if self.last.get().is_none_or(|t| t.elapsed() >= Duration::from_millis(500)) {
            self.step(step, app, done);
        }
    }
}

const FOLDER_SH: &str = r#"set -euo pipefail
id="$1"
[[ "$id" =~ ^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$ ]] || { echo "bad app id: $id" >&2; exit 2; }
cd /var/lib/konak/apps
[ -d "$id" ] || { echo "no folder for $id" >&2; exit 3; }
exec tar --numeric-owner --xattrs --xattrs-include='*' --acls --sparse --one-file-system -cpf - "$id" </dev/null
"#;

const VOLUME_SH: &str = r#"set -euo pipefail
[[ "$1" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]*$ ]] || { echo "bad volume name: $1" >&2; exit 2; }
exec podman volume export "$1" </dev/null
"#;

fn write_error(e: std::io::Error) -> String {
    // ERROR_HANDLE_DISK_FULL, ERROR_DISK_FULL
    if matches!(e.raw_os_error(), Some(39 | 112)) {
        "backup_no_space".into()
    } else {
        format!("cannot write the backup file: {e}")
    }
}

/// The apps a run includes: installed, in the catalog, and selected (none selected = all).
fn included<'a>(state: &'a State, selected: &[String]) -> Vec<&'a InstalledApp> {
    state.apps.iter().filter(|a| crate::catalog::get(&a.id).is_some() && (selected.is_empty() || selected.contains(&a.id))).collect()
}

/// The quick checks `backup_now` answers at once.
pub fn precheck(paths: &Paths) -> Result<(), String> {
    if running() {
        return Err("backup_running".into());
    }
    if !State::try_load(paths)?.done {
        return Err("backup_not_set_up".into());
    }
    let s = settings::load(paths);
    if s.backup.location.is_none() {
        return Err("backup_no_location".into());
    }
    if !s.backup.password_set {
        return Err("backup_no_password".into());
    }
    Ok(())
}

/// The app whose containers a run has stopped, so a Konak that ended mid-backup starts it again at
/// its next start (`recover`).
fn inflight_file(paths: &Paths) -> PathBuf {
    paths.home.join("backup-inflight.json")
}

/// At start: an app left stopped by a backup that did not finish is started again (unless the user
/// had stopped it). Returns its id.
pub fn recover(paths: &Paths, apps: &dyn Apps) -> Option<String> {
    let body = std::fs::read(inflight_file(paths)).ok()?;
    let id = serde_json::from_slice::<serde_json::Value>(&body).ok().and_then(|v| v["app"].as_str().map(String::from));
    if let Some(id) = &id {
        let user_stopped = State::load(paths).apps.iter().find(|a| &a.id == id).is_none_or(|a| a.stopped);
        if !user_stopped {
            let r = apps.start(id);
            crate::log(paths, &format!("backup: {id} was left stopped by an unfinished backup; started again: {r:?}"));
        }
    }
    let _ = std::fs::remove_file(inflight_file(paths));
    id
}

/// One backup run, start to end. `_guard` proves no other run is going on. On failure the partial
/// file is gone and every app is back in its previous running state. The result is recorded in the
/// history and reported through `host` (`done` or `failed`).
pub fn run(paths: &Paths, host: &dyn Host, apps: &dyn Apps, _guard: &Guard, log_n: u8) -> Result<PathBuf, Failed> {
    let t0 = Instant::now();
    let created = now();
    let ctx = Ctx { host, total: Cell::new(0), last: Cell::new(None) };
    let mut partial = None;
    let mut ids = vec![];
    let result = run_steps(paths, &ctx, apps, created, log_n, &mut partial, &mut ids);
    let file_for_history = match (&result, &partial) {
        (Ok(p), _) => p.to_string_lossy().to_string(),
        (Err(_), Some(p)) => p.to_string_lossy().trim_end_matches(PARTIAL).to_string(),
        (Err(_), None) => String::new(),
    };
    if let (Err(_), Some(p)) = (&result, &partial) {
        if let Err(e) = std::fs::remove_file(p) {
            if e.kind() != std::io::ErrorKind::NotFound {
                crate::log(paths, &format!("backup: cannot remove {}: {e}", p.display()));
            }
        }
    }
    let redact = |e: &str| crate::redact::Redactor::new(vec![]).apply(e);
    match &result {
        Ok(p) => {
            let size = std::fs::metadata(p).map_or(0, |m| m.len());
            crate::log(paths, &format!("backup done: {} ({size} bytes, {:.0} s)", p.display(), t0.elapsed().as_secs_f64()));
            record(paths, HistoryEntry { file: file_for_history, created_unix: created, size_bytes: size, apps: ids, status: "ok".into(), error: None, step: None });
            host.progress(&Progress { state: "done", step: "done", app: None, done_bytes: Some(ctx.total.get()), total_bytes: Some(ctx.total.get()), error: None });
        }
        Err(f) => {
            let error = redact(&f.error);
            crate::log(paths, &format!("backup failed at {}{}: {error}", f.step, f.app.as_deref().map(|a| format!(" ({a})")).unwrap_or_default()));
            record(paths, HistoryEntry { file: file_for_history, created_unix: created, size_bytes: 0, apps: ids, status: "failed".into(), error: Some(error.clone()), step: Some(f.step.into()) });
            host.progress(&Progress { state: "failed", step: f.step, app: f.app.clone(), done_bytes: None, total_bytes: None, error: Some(error) });
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn run_steps(paths: &Paths, ctx: &Ctx, apps: &dyn Apps, created: u64, log_n: u8, partial_out: &mut Option<PathBuf>, ids_out: &mut Vec<String>) -> Result<PathBuf, Failed> {
    ctx.step("prepare", None, 0);
    let state = State::try_load(paths).map_err(|e| fail("prepare", e))?;
    if !state.done {
        return Err(fail("prepare", "backup_not_set_up"));
    }
    let s = settings::load(paths);
    let loc = s.backup.location.clone().ok_or_else(|| fail("prepare", "backup_no_location"))?;
    let pw = settings::password(paths).map_err(|e| fail("prepare", e))?.ok_or_else(|| fail("prepare", "backup_no_password"))?;
    settings::check_location(paths, &loc).map_err(|e| fail("prepare", e))?;
    let dir = PathBuf::from(&loc);
    remove_stale_partials(paths, &dir);

    let selected = included(&state, &s.backup.include.apps);
    *ids_out = selected.iter().map(|a| a.id.clone()).collect();
    let id_refs: Vec<&str> = ids_out.iter().map(String::as_str).collect();
    let sizes = if id_refs.is_empty() {
        vec![]
    } else {
        let out = shell::distro_script(&paths.distro, SIZES_SH, &id_refs);
        parse_sizes(&out.text, &id_refs).map_err(|e| fail("prepare", e))?
    };
    let photos = (s.backup.include.photos && paths.photos.is_dir()).then(|| folder_size(&paths.photos));
    let media = (s.backup.include.media && paths.media.is_dir()).then(|| folder_size(&paths.media));
    let app_bytes = |a: &AppSize| a.folder + a.volumes.iter().map(|v| v.bytes).sum::<u64>();
    let total = sizes.iter().map(app_bytes).sum::<u64>() + photos.unwrap_or(0) + media.unwrap_or(0);
    ctx.total.set(total);
    let free = free_space(&dir).map_err(|e| fail("prepare", e))?;
    if free < total + SPACE_MARGIN {
        crate::log(paths, &format!("backup: {free} bytes free in {loc}, about {total} needed"));
        return Err(fail("prepare", "backup_no_space"));
    }

    let local = local_of(created).ok_or_else(|| fail("prepare", "cannot read the local time"))?;
    let name = file_name(local);
    let final_path = dir.join(&name);
    let partial = dir.join(format!("{name}{PARTIAL}"));
    if final_path.exists() {
        return Err(fail("prepare", "backup_exists"));
    }
    let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&partial).map_err(|_| fail("prepare", "settings_location_not_writable"))?;
    *partial_out = Some(partial.clone());

    let ca = [("ca/root.crt", paths.ca_cert()), ("ca/root.key", paths.ca_key())];
    let mut konak_files: Vec<String> = ca.iter().filter(|(_, p)| p.is_file()).map(|(n, _)| n.to_string()).collect();
    konak_files.extend(["state.json".to_string(), "settings.json".to_string()]);
    let manifest = Manifest {
        format: fmt::FORMAT,
        konak_version: env!("CARGO_PKG_VERSION").into(),
        created_unix: created,
        apps: selected
            .iter()
            .zip(&sizes)
            .map(|(a, z)| AppManifest {
                id: a.id.clone(),
                name: a.name.clone(),
                claimed: a.claimed,
                stopped: a.stopped,
                size_bytes: app_bytes(z),
                volumes: z.volumes.iter().map(|v| VolumeManifest { name: v.name.clone(), podman_name: v.podman_name.clone() }).collect(),
            })
            .collect(),
        photos_bytes: photos,
        media_bytes: media,
        konak_files,
    };
    crate::log(paths, &format!("backup started: {} ({} apps, about {total} bytes)", partial.display(), manifest.apps.len()));
    let mut w = Writer::new(BufWriter::with_capacity(1 << 20, file), &pw, &manifest, log_n).map_err(|e| fail("prepare", write_error(e)))?;

    for z in &sizes {
        let id = z.id.as_str();
        let app_fail = |e: String| Failed { step: "app", app: Some(id.to_string()), error: e };
        ctx.step("app", Some(id), w.bytes());
        let deadline = Instant::now() + BUSY_WAIT;
        while !ctx.host.try_busy(id) {
            if Instant::now() >= deadline {
                return Err(app_fail("busy".into()));
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        let _ = settings::write_atomic(&inflight_file(paths), serde_json::json!({ "app": id }).to_string().as_bytes());
        let result = with_stopped(apps, id, || archive_app(paths, ctx, &mut w, z, created));
        let _ = std::fs::remove_file(inflight_file(paths));
        ctx.host.release(id, &result);
        result.map_err(app_fail)?;
    }

    ctx.step("konak", None, w.bytes());
    let konak = |e| fail("konak", write_error(e));
    for (rel, path) in &ca {
        if path.is_file() {
            let body = std::fs::read(path).map_err(|e| fail("konak", format!("cannot read {}: {e}", path.display())))?;
            w.add_bytes(&format!("konak/{rel}"), &body, 0o600, created).map_err(konak)?;
        }
    }
    let essentials = serde_json::json!({ "apps": state.apps, "rendered_ports": state.rendered_ports });
    w.add_bytes("konak/state.json", &serde_json::to_vec_pretty(&essentials).unwrap(), 0o600, created).map_err(konak)?;
    let mut stored = s.clone();
    stored.backup.password_set = false;
    w.add_bytes("konak/settings.json", &serde_json::to_vec_pretty(&stored).unwrap(), 0o600, created).map_err(konak)?;

    for (step, root, on) in [("photos", &paths.photos, photos.is_some()), ("media", &paths.media, media.is_some())] {
        if !on {
            continue;
        }
        ctx.step(step, None, w.bytes());
        let mut skipped = vec![];
        walk(root, &mut skipped, step, &mut |rel, abs, meta| {
            let mtime = meta.modified().ok().and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
            match std::fs::File::open(abs) {
                Ok(f) => {
                    let r = std::io::BufReader::with_capacity(1 << 20, f);
                    w.add_reader(&format!("{step}/{rel}"), meta.len(), r, 0o644, mtime).map_err(write_error)?;
                }
                Err(e) => w.skip(format!("{step}/{rel} (unreadable: {e})")),
            }
            ctx.bytes(step, None, w.bytes());
            Ok(())
        })
        .map_err(|e| fail(step, e))?;
        if !skipped.is_empty() {
            crate::log(paths, &format!("backup: {} entries of {step} left out, e.g. {}", skipped.len(), skipped[0]));
        }
        for s in skipped {
            w.skip(s);
        }
    }

    let (buf, sums) = w.finish(created).map_err(|e| fail("finish", write_error(e)))?;
    let file = buf.into_inner().map_err(|e| fail("finish", write_error(e.into_error())))?;
    file.sync_all().map_err(|e| fail("finish", write_error(e)))?;
    drop(file);
    if !sums.skipped.is_empty() {
        crate::log(paths, &format!("backup: {} files left out or changed while read", sums.skipped.len()));
    }

    ctx.step("verify", None, ctx.total.get());
    let (read_back, _) = fmt::verify(&partial, &pw).map_err(|e| fail("verify", e.to_string()))?;
    if read_back != manifest {
        return Err(fail("verify", "backup_damaged: the manifest read back differs"));
    }

    ctx.step("finish", None, ctx.total.get());
    std::fs::rename(&partial, &final_path).map_err(|e| fail("finish", format!("cannot rename the finished backup: {e}")))?;
    *partial_out = None;
    prune(paths, &dir, s.backup.keep as usize, &pw, &final_path);
    Ok(final_path)
}

/// Streams one app's folder and named volumes out of the distro into the archive.
fn archive_app<W: Write>(paths: &Paths, ctx: &Ctx, w: &mut Writer<W>, z: &AppSize, mtime: u64) -> Result<(), String> {
    let id = z.id.as_str();
    shell::distro_read(&paths.distro, FOLDER_SH, &[id], |out| w.add_stream(&format!("apps/{id}/folder"), out, mtime, &mut |n| ctx.bytes("app", Some(id), n)).map_err(write_error))
        .map_err(|e| format!("archiving the folder of {id} failed: {e}"))?;
    for v in &z.volumes {
        shell::distro_read(&paths.distro, VOLUME_SH, &[&v.podman_name], |out| {
            w.add_stream(&format!("apps/{id}/volumes/{}", v.name), out, mtime, &mut |n| ctx.bytes("app", Some(id), n)).map_err(write_error)
        })
        .map_err(|e| format!("exporting volume {} of {id} failed: {e}", v.podman_name))?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;
    use time::macros::datetime;

    struct FakeApps {
        running: bool,
        fail_stop: bool,
        fail_start: bool,
        calls: RefCell<Vec<String>>,
    }

    impl FakeApps {
        fn new(running: bool) -> Self {
            FakeApps { running, fail_stop: false, fail_start: false, calls: RefCell::new(vec![]) }
        }
    }

    impl Apps for FakeApps {
        fn running(&self, id: &str) -> Result<bool, String> {
            self.calls.borrow_mut().push(format!("running {id}"));
            Ok(self.running)
        }
        fn stop(&self, id: &str) -> Result<(), String> {
            self.calls.borrow_mut().push(format!("stop {id}"));
            if self.fail_stop { Err("stop failed".into()) } else { Ok(()) }
        }
        fn start(&self, id: &str) -> Result<(), String> {
            self.calls.borrow_mut().push(format!("start {id}"));
            if self.fail_start { Err("start failed".into()) } else { Ok(()) }
        }
    }

    #[test]
    fn apps_come_back_to_their_previous_state() {
        // Running: stopped for the archive, started again, also when the archive fails.
        let a = FakeApps::new(true);
        assert_eq!(with_stopped(&a, "nextcloud", || Ok(5)), Ok(5));
        assert_eq!(*a.calls.borrow(), ["running nextcloud", "stop nextcloud", "start nextcloud"]);
        let a = FakeApps::new(true);
        let r: Result<(), String> = with_stopped(&a, "nextcloud", || Err("disk full".into()));
        assert_eq!(r.unwrap_err(), "disk full");
        assert_eq!(*a.calls.borrow(), ["running nextcloud", "stop nextcloud", "start nextcloud"]);
        // Stopped by the user: never started.
        let a = FakeApps::new(false);
        let r: Result<(), String> = with_stopped(&a, "jellyfin", || Err("x".into()));
        assert!(r.is_err());
        assert_eq!(*a.calls.borrow(), ["running jellyfin"]);
        // A failed stop is undone; the archive does not run.
        let a = FakeApps { fail_stop: true, ..FakeApps::new(true) };
        let ran = Cell::new(false);
        let r = with_stopped(&a, "immich", || {
            ran.set(true);
            Ok(())
        });
        assert!(r.is_err() && !ran.get());
        assert_eq!(*a.calls.borrow(), ["running immich", "stop immich", "start immich"]);
        // A start that fails after a good archive is still a failure, and says so.
        let a = FakeApps { fail_start: true, ..FakeApps::new(true) };
        let e = with_stopped(&a, "forgejo", || Ok(())).unwrap_err();
        assert!(e.contains("did not start again") && e.contains("start failed"), "{e}");
    }

    fn sched(kind: &str, time: &str, weekday: u8) -> Schedule {
        Schedule { kind: kind.into(), time: time.into(), weekday }
    }

    #[test]
    fn schedule_decisions() {
        let daily = sched("daily", "03:00", 0);
        // 2026-11-26 is a Thursday.
        let yesterday_run = datetime!(2026-11-25 03:00:05);
        assert!(!due(&daily, yesterday_run, datetime!(2026-11-26 02:59:59)));
        assert!(due(&daily, yesterday_run, datetime!(2026-11-26 03:00:00)));
        assert!(!due(&daily, datetime!(2026-11-26 03:00:10), datetime!(2026-11-26 23:00)), "once per slot");
        // The PC was off for three days: one run at the next start, even before today's slot.
        assert!(due(&daily, datetime!(2026-11-22 03:00:30), datetime!(2026-11-26 01:00)));
        // Set at 14:00: the first run is the next 03:00.
        assert!(!due(&daily, datetime!(2026-11-26 14:00), datetime!(2026-11-26 23:59)));
        assert!(due(&daily, datetime!(2026-11-26 14:00), datetime!(2026-11-27 03:00)));

        let sunday = sched("weekly", "22:30", 0);
        assert_eq!(latest_slot(&sunday, datetime!(2026-11-26 12:00)), Some(datetime!(2026-11-22 22:30)));
        assert_eq!(latest_slot(&sunday, datetime!(2026-11-22 22:29)), Some(datetime!(2026-11-15 22:30)));
        assert_eq!(latest_slot(&sunday, datetime!(2026-11-22 22:30)), Some(datetime!(2026-11-22 22:30)));
        assert!(!due(&sunday, datetime!(2026-11-23 09:00), datetime!(2026-11-29 22:29)));
        assert!(due(&sunday, datetime!(2026-11-23 09:00), datetime!(2026-11-29 22:30)));
        // Thursday is 4.
        assert_eq!(latest_slot(&sched("weekly", "00:00", 4), datetime!(2026-11-26 00:00)), Some(datetime!(2026-11-26 00:00)));

        assert!(!due(&sched("off", "03:00", 0), datetime!(2020-01-01 00:00), datetime!(2026-11-26 12:00)));
        assert!(!due(&sched("daily", "bad", 0), datetime!(2020-01-01 00:00), datetime!(2026-11-26 12:00)));
        assert!(!due(&sched("weekly", "03:00", 9), datetime!(2020-01-01 00:00), datetime!(2026-11-26 12:00)));
    }

    #[test]
    fn local_time_and_names() {
        let t = local_of(1_795_000_000).unwrap();
        assert!(t.year() == 2026 || t.year() == 2027);
        assert_eq!(file_name(datetime!(2026-11-26 03:04:05)), "konak-20261126-030405.konakbackup");
        assert!(is_backup_name("konak-20261126-030405.konakbackup"));
        for bad in ["konak-20261126-030405.konakbackup.partial", "konak-2026112-030405.konakbackup", "Konak-20261126-030405.konakbackup", "konak-20261126x030405.konakbackup", "my.konakbackup"] {
            assert!(!is_backup_name(bad), "{bad}");
        }
        assert!(is_partial_name("konak-20261126-030405.konakbackup.partial"));
    }

    #[test]
    fn sizes_are_parsed_and_the_model_cache_is_skipped() {
        let text = "F nextcloud 4096\nV nextcloud nextcloud_db db 1000\nV nextcloud nextcloud_nextcloud nextcloud 2000\nF immich 50\nV immich immich_model-cache model-cache 999999\ntime=\"x\" level=warning msg=\"y\"\nend\n";
        let s = parse_sizes(text, &["nextcloud", "immich"]).unwrap();
        assert_eq!(s[0].folder, 4096);
        assert_eq!(s[0].volumes.iter().map(|v| (v.podman_name.as_str(), v.name.as_str(), v.bytes)).collect::<Vec<_>>(), [("nextcloud_db", "db", 1000), ("nextcloud_nextcloud", "nextcloud", 2000)]);
        assert_eq!((s[1].folder, s[1].volumes.len()), (50, 0));
        assert!(parse_sizes("F nextcloud 1\n", &["nextcloud"]).is_err(), "no end line");
        assert!(parse_sizes("V nextcloud x Bad/Name 1\nend\n", &["nextcloud"]).is_err());
    }

    /// The run guard, once another test (backup's or restore's) has let it go.
    pub(crate) fn test_guard() -> Guard {
        loop {
            match Guard::acquire() {
                Ok(g) => return g,
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("konak-backup-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn retention_prunes_only_konak_files() {
        use crate::backup_format::tests::{manifest, pw, write_sample};
        let paths = crate::settings::tests::temp_paths("retention");
        let dir = temp_dir("retention");
        let (pw_now, t) = ("the backup password", now());
        let day = 86_400;
        // Real backups, made with the current password: (name, created).
        let real = [
            ("konak-20261120-030000.konakbackup", t - 4 * day),
            ("konak-20261121-030000.konakbackup", t - 3 * day),
            ("konak-20261122-030000.konakbackup", t - 2 * day),
            ("konak-20261123-030000.konakbackup", t - day),
        ];
        for (n, created) in real {
            write_sample(&dir.join(n), pw_now, &manifest(created), b"x");
        }
        // The one this run wrote, older by its manifest than the others (a clock that went back):
        // still never deleted.
        let written = dir.join("konak-20261124-030000.konakbackup");
        write_sample(&written, pw_now, &manifest(t - 10 * day), b"x");
        // Old, but made with another password: not deleted, not counted.
        write_sample(&dir.join("konak-20261101-030000.konakbackup"), "an earlier password", &manifest(t - 30 * day), b"x");
        // Planted: names far in the future with only the magic line, and one that opens but claims
        // a future time. None of them takes a place or is deleted.
        for i in 1..=7 {
            std::fs::write(dir.join(format!("konak-9999010{i}-000000.konakbackup")), b"KONAK-BACKUP\n").unwrap();
        }
        write_sample(&dir.join("konak-20991231-000000.konakbackup"), pw_now, &manifest(t + 365 * day), b"x");
        // Konak's name without Konak's content, and other files: never touched.
        std::fs::write(dir.join("konak-20261102-030000.konakbackup"), b"not a backup").unwrap();
        std::fs::write(dir.join("holiday.jpg"), b"jpg").unwrap();
        std::fs::write(dir.join("konak-20261103-030000.konakbackup.old"), b"KONAK-BACKUP\n").unwrap();
        let deleted = prune(&paths, &dir, 3, &pw(pw_now), &written);
        let left: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&paths.home);
        // keep 3: the written one plus the two newest real ones.
        assert_eq!(deleted, ["konak-20261121-030000.konakbackup", "konak-20261120-030000.konakbackup"]);
        assert!(left.contains(&"konak-20261124-030000.konakbackup".to_string()));
        assert!(left.contains(&"konak-20261123-030000.konakbackup".to_string()) && left.contains(&"konak-20261122-030000.konakbackup".to_string()));
        assert_eq!(left.len(), 17 - 2, "only the two oldest real ones are gone: {left:?}");
        assert_eq!(prune_candidates(vec![("a".into(), 1), ("b".into(), 3), ("c".into(), 2)], 1), ["c", "a"]);
        assert!(prune_candidates(vec![("a".into(), 1)], 5).is_empty());
    }

    #[test]
    fn retention_with_keep_one_keeps_only_the_written_file() {
        use crate::backup_format::tests::{manifest, pw, write_sample};
        let paths = crate::settings::tests::temp_paths("retention-one");
        let dir = temp_dir("retention-one");
        let t = now();
        write_sample(&dir.join("konak-20261120-030000.konakbackup"), "the backup password", &manifest(t - 100), b"x");
        let written = dir.join("konak-20261121-030000.konakbackup");
        write_sample(&written, "the backup password", &manifest(t), b"x");
        let deleted = prune(&paths, &dir, 1, &pw("the backup password"), &written);
        let written_left = written.is_file();
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&paths.home);
        assert_eq!(deleted, ["konak-20261120-030000.konakbackup"]);
        assert!(written_left);
    }

    struct FakeHost {
        events: RefCell<Vec<Progress>>,
        busy: RefCell<Vec<String>>,
    }

    impl Host for FakeHost {
        fn progress(&self, p: &Progress) {
            self.events.borrow_mut().push(p.clone());
        }
        fn try_busy(&self, id: &str) -> bool {
            self.busy.borrow_mut().push(id.into());
            true
        }
        fn release(&self, _: &str, _: &Result<(), String>) {}
    }

    #[test]
    fn a_run_without_a_location_fails_cleanly_and_is_recorded() {
        let paths = crate::settings::tests::temp_paths("run-noloc");
        let mut st = State::default();
        st.done = true;
        st.save(&paths);
        let host = FakeHost { events: RefCell::new(vec![]), busy: RefCell::new(vec![]) };
        let guard = test_guard();
        assert_eq!(Guard::acquire().err().as_deref(), Some("backup_running"));
        let r = run(&paths, &host, &FakeApps::new(true), &guard, 10);
        drop(guard);
        let listed = list(&paths);
        let history = load_history(&paths);
        let _ = std::fs::remove_dir_all(&paths.home);
        assert_eq!(r.unwrap_err(), fail("prepare", "backup_no_location"));
        let last = host.events.borrow().last().cloned().unwrap();
        assert_eq!((last.state, last.step, last.error.as_deref()), ("failed", "prepare", Some("backup_no_location")));
        assert_eq!(history[0].status, "failed");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].status, "failed");
        assert!(host.busy.borrow().is_empty());
    }

    /// A real backup in a throwaway distro. Env: `KONAK_TEST_DISTRO=konak-<name>`, `KONAK_TEST_HOME`,
    /// `KONAK_TEST_OUT` (backup folder), `KONAK_TEST_PHOTOS`. Provisions the distro with the setup's
    /// distro/systemd/engine steps when it is missing, installs Vaultwarden and Nextcloud (already
    /// listed in state.json, so no LAN name check runs), backs up, verifies and lists the archive.
    #[test]
    #[ignore]
    fn live_backup() {
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
        let distro = env("KONAK_TEST_DISTRO");
        assert!(distro.starts_with("konak-") && distro != "konak", "only a throwaway Konak distro");
        let home = PathBuf::from(env("KONAK_TEST_HOME"));
        let out = PathBuf::from(env("KONAK_TEST_OUT"));
        let photos = PathBuf::from(env("KONAK_TEST_PHOTOS"));
        let res = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources");
        let paths = Paths { home: home.clone(), distro: distro.clone(), bin: res.join("bin"), media: photos.with_file_name("media"), photos: photos.clone() };
        std::fs::create_dir_all(&out).unwrap();
        std::fs::create_dir_all(&photos).unwrap();
        std::fs::write(photos.join("p1.jpg"), vec![7u8; 300_000]).unwrap();
        let t = Instant::now();
        if !crate::installer::distro_exists(&distro).unwrap() {
            let sup = crate::services::Supervisor::new(paths.clone());
            let mut st = State::default();
            for step in ["distro", "systemd", "engine"] {
                crate::installer::run_step(step, &paths, &mut st, &crate::installer::Preflight::default(), false, &sup).unwrap();
                println!("{step} done at {:.0} s", t.elapsed().as_secs_f64());
            }
        }
        let mut st = State::try_load(&paths).unwrap();
        st.done = true;
        for id in ["vaultwarden", "nextcloud"] {
            if !st.apps.iter().any(|a| a.id == id) {
                st.apps.push(InstalledApp { id: id.into(), name: id.into(), installed_unix: now(), claimed: false, stopped: false });
            }
        }
        st.save(&paths);
        for id in ["vaultwarden", "nextcloud"] {
            crate::apps::install(&paths, id).unwrap();
            println!("{id} installed at {:.0} s", t.elapsed().as_secs_f64());
        }
        let mut s = settings::load(&paths);
        s.backup.location = Some(out.to_string_lossy().to_string());
        s.backup.schedule.kind = "off".into();
        settings::save(&paths, &s).unwrap();
        if !settings::password_set(&paths) {
            settings::set_password(&paths, None, "live test password 1").unwrap();
        }
        struct Print;
        impl Host for Print {
            fn progress(&self, p: &Progress) {
                println!("progress {} {} {:?} {:?}/{:?} {:?}", p.state, p.step, p.app, p.done_bytes, p.total_bytes, p.error);
            }
            fn try_busy(&self, _: &str) -> bool {
                true
            }
            fn release(&self, id: &str, r: &Result<(), String>) {
                println!("release {id}: {r:?}");
            }
        }
        let apps = DistroApps(&paths);
        let before: Vec<bool> = ["vaultwarden", "nextcloud"].iter().map(|id| apps.running(id).unwrap()).collect();
        let t1 = Instant::now();
        let guard = Guard::acquire().unwrap();
        let file = run(&paths, &Print, &apps, &guard, fmt::SCRYPT_LOG_N).unwrap();
        println!("backup took {:.1} s, {} bytes: {}", t1.elapsed().as_secs_f64(), std::fs::metadata(&file).unwrap().len(), file.display());
        let after: Vec<bool> = ["vaultwarden", "nextcloud"].iter().map(|id| apps.running(id).unwrap()).collect();
        assert_eq!(before, after, "apps back in their previous state");
        // The folder chunks concatenate to a tar of the app folder, .env included.
        let pw = SecretString::from("live test password 1".to_string());
        let b = fmt::open(&file, &pw).unwrap();
        println!("manifest: {}", serde_json::to_string(&b.manifest).unwrap());
        let mut streams: std::collections::BTreeMap<String, Vec<u8>> = Default::default();
        let sums = b
            .for_each_entry(|e, p, r| {
                if let fmt::Entry::AppFolder { .. } | fmt::Entry::AppVolume { .. } = e {
                    let key = p.rsplit_once('/').unwrap().0.to_string();
                    r.read_to_end(streams.entry(key).or_default()).map_err(|e| e.to_string())?;
                }
                Ok(())
            })
            .unwrap();
        println!("{} entries, skipped {:?}", sums.entries.len(), sums.skipped);
        for (k, v) in &streams {
            let mut a = tar::Archive::new(v.as_slice());
            let names: Vec<String> = a.entries().unwrap().map(|e| e.unwrap().path().unwrap().to_string_lossy().to_string()).collect();
            println!("{k}: {} bytes, {} members, e.g. {:?}", v.len(), names.len(), &names[..names.len().min(3)]);
            if k.ends_with("/folder") {
                assert!(names.iter().any(|n| n.ends_with("/.env")), "{k}");
            }
        }
        assert!(streams.contains_key("apps/nextcloud/volumes/db") && streams.contains_key("apps/nextcloud/volumes/nextcloud"));
        println!("list: {:?}", list(&paths));
    }

    /// The whole run without a distro: no apps, the Konak files and a Photos folder, into a real
    /// folder; then verified, listed and pruned.
    #[test]
    fn a_run_with_photos_only_writes_one_verified_file() {
        let paths = crate::settings::tests::temp_paths("run-photos");
        let out = temp_dir("run-photos-out");
        std::fs::create_dir_all(paths.photos.join("2026")).unwrap();
        std::fs::write(paths.photos.join("2026").join("a.jpg"), b"jpeg data").unwrap();
        std::fs::write(paths.photos.join("b.png"), b"png").unwrap();
        let mut st = State::default();
        st.done = true;
        st.save(&paths);
        let mut s = settings::Settings::default();
        s.backup.location = Some(out.to_string_lossy().to_string());
        s.backup.schedule.kind = "off".into();
        settings::save(&paths, &s).unwrap();
        settings::set_password(&paths, None, "a long enough secret").unwrap();
        // A partial left by an earlier run is cleared.
        std::fs::write(out.join("konak-20200101-000000.konakbackup.partial"), b"").unwrap();
        let host = FakeHost { events: RefCell::new(vec![]), busy: RefCell::new(vec![]) };
        let guard = test_guard();
        let r = run(&paths, &host, &FakeApps::new(true), &guard, 10);
        drop(guard);
        let file = r.unwrap();
        let (m, sums) = fmt::verify(&file, &SecretString::from("a long enough secret".to_string())).unwrap();
        let listed = list(&paths);
        let inspected = inspect(&paths, &file.to_string_lossy(), "").unwrap();
        let wrong = inspect(&paths, &file.to_string_lossy(), "wrong password!");
        let names: Vec<String> = std::fs::read_dir(&out).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        let _ = std::fs::remove_dir_all(&out);
        let _ = std::fs::remove_dir_all(&paths.home);
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(is_backup_name(&names[0]));
        assert_eq!(m.photos_bytes, Some(12));
        assert!(m.apps.is_empty() && m.media_bytes.is_none());
        let paths_in: Vec<&str> = sums.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths_in, ["manifest.json", "konak/state.json", "konak/settings.json", "photos/b.png", "photos/2026/a.jpg"]);
        assert_eq!(inspected.manifest, m);
        assert_eq!((inspected.ca_fingerprint, inspected.ca_valid), (None, false));
        assert_eq!(wrong.unwrap_err(), "backup_wrong_password");
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].status, listed[0].error.as_deref()), ("ok", None));
        let steps: Vec<&str> = host.events.borrow().iter().map(|e| e.step).collect();
        assert!(steps.starts_with(&["prepare", "konak", "photos"]) && steps.ends_with(&["verify", "finish", "done"]), "{steps:?}");
    }
}
