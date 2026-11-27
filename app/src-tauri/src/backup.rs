//! The backup engine: one encrypted file per run in the user's backup folder (format in
//! `backup_format`), the schedule, retention and the history the Settings page lists.
//!
//! Consistency, "the safe way" of the plan: apps are archived one at a time, each stopped
//! (`docker-compose stop`) while its folder and named volumes stream out of the distro, then started
//! again if it was running. Every database is thus copied at rest. The Windows Photos and Media
//! folders are plain files, copied after the apps' parts while the apps run, so an app's database
//! never refers to a file the archive misses. An app that cannot be stopped or is busy for too long
//! is left out with a note, and the run goes on (status `partial`).
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
pub(crate) const SPACE_MARGIN: u64 = 64 << 20;
/// How long an app's turn waits for another operation on it to end.
const BUSY_WAIT: Duration = Duration::from_secs(10 * 60);
const HISTORY_MAX: usize = 30;
/// Named volumes that are not backed up (re-downloadable).
pub(crate) const SKIP_VOLUMES: &[(&str, &str)] = &[("immich", "model-cache")];

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

/// The op of the run in progress (`RunMarker`), for the panel and the tray's Quit.
static CURRENT_OP: Mutex<Option<String>> = Mutex::new(None);

/// What holds the run guard: `backup`, `restore`, or `recover` (the startup recovery of an
/// unfinished run, or the moment before a run's marker is written); `None` when nothing runs.
pub fn running_op() -> Option<String> {
    if !running() {
        return None;
    }
    Some(CURRENT_OP.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_else(|| "recover".into()))
}

/// While a backup or restore runs, `op-running.json` in the Konak home names this process, so an
/// uninstall (another process) waits for it instead of stopping Konak in the middle of it.
pub struct RunMarker(PathBuf);

fn run_marker_file(paths: &Paths) -> PathBuf {
    paths.home.join("op-running.json")
}

impl RunMarker {
    pub fn new(paths: &Paths, op: &str) -> RunMarker {
        let file = run_marker_file(paths);
        if let Err(e) = settings::write_atomic(&file, serde_json::json!({ "pid": std::process::id(), "op": op }).to_string().as_bytes()) {
            crate::log(paths, &format!("{op}: cannot write {}: {e}", file.display()));
        }
        *CURRENT_OP.lock().unwrap_or_else(|e| e.into_inner()) = Some(op.to_string());
        RunMarker(file)
    }
}

impl Drop for RunMarker {
    fn drop(&mut self) {
        *CURRENT_OP.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let _ = std::fs::remove_file(&self.0);
    }
}

// ---- Cancel and keep-awake ---------------------------------------------------------------------

static CANCEL: AtomicBool = AtomicBool::new(false);
/// The error (and history/event code) of a backup the user cancelled.
pub const CANCELLED: &str = "backup_cancelled";

/// Asks the running backup to stop: it ends at the next file or chunk, the app being archived is
/// started again, and the partial file is deleted. `backup_not_running` when no backup runs (a
/// restore cannot be cancelled).
pub fn cancel() -> Result<(), String> {
    if running_op().as_deref() != Some("backup") {
        return Err("backup_not_running".into());
    }
    CANCEL.store(true, Ordering::SeqCst);
    Ok(())
}

fn cancelled() -> bool {
    CANCEL.load(Ordering::SeqCst)
}

/// A reader that fails with `CANCELLED` once a cancel was asked for.
struct Cancellable<R>(R);

impl<R: std::io::Read> std::io::Read for Cancellable<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if cancelled() {
            return Err(std::io::Error::other(CANCELLED));
        }
        self.0.read(buf)
    }
}

/// Keeps the PC from going to sleep while a backup or restore runs on this thread
/// (`SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED)`), and lets it sleep again when
/// dropped. The display may still turn off; no power setting is changed.
pub struct KeepAwake(());

impl KeepAwake {
    pub fn new() -> KeepAwake {
        use windows_sys::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED};
        // SAFETY: plain call; it only changes this thread's execution state.
        unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
        KeepAwake(())
    }
}

impl Default for KeepAwake {
    fn default() -> Self {
        KeepAwake::new()
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS};
        // SAFETY: as in `new`.
        unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
    }
}

/// The backup or restore another live process of `exe` runs: its pid and op (`backup`, `restore`).
pub fn running_elsewhere(paths: &Paths, exe: &Path) -> Option<(u32, String)> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(run_marker_file(paths)).ok()?).ok()?;
    let pid = u32::try_from(v["pid"].as_u64()?).ok()?;
    let image = crate::diag::process_image(pid)?;
    let same = |a: &Path, b: &Path| a.to_string_lossy().replace('/', "\\").eq_ignore_ascii_case(&b.to_string_lossy().replace('/', "\\"));
    (pid != std::process::id() && same(Path::new(&image), exe)).then(|| (pid, v["op"].as_str().unwrap_or("backup").to_string()))
}

// ---- What the engine needs from its caller ---------------------------------------------------------

/// `backup-progress` event payload (contract).
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Progress {
    /// running | done | failed | cancelled
    pub state: &'static str,
    /// prepare | konak | app | photos | media | verify | finish | done
    pub step: &'static str,
    pub app: Option<String>,
    pub done_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    /// A locale code (`errors.<code>`): `backup_app_failed` and `backup_failed` stand for any
    /// error without a code of its own; `backup_cancelled` with state `cancelled`.
    pub error: Option<String>,
    /// The technical text behind `error`, redacted; for details on demand.
    pub detail: Option<String>,
    /// With state `done`: how many files or apps were left out, or changed while they were read
    /// (0 for a complete backup; the history lists it as `partial` then).
    pub skipped: Option<u64>,
    /// With state `done`: the first of those, e.g. `photos/2026/a.jpg (changed while it was read)`
    /// or `apps/jellyfin (left out: busy)`, redacted.
    pub notes: Option<Vec<String>>,
}

/// The running Konak: progress events and the busy set the panel's app operations use.
pub trait Host {
    fn progress(&self, p: &Progress);
    /// Marks the app busy (op `backup`); false when another operation holds it.
    fn try_busy(&self, id: &str) -> bool;
    fn release(&self, id: &str, result: &Result<(), String>);
}

/// The apps' side of a run: stopping and starting an app's containers, and reading its data.
pub trait Apps {
    /// Whether any of the app's containers runs.
    fn running(&self, id: &str) -> Result<bool, String>;
    fn stop(&self, id: &str) -> Result<(), String>;
    fn start(&self, id: &str) -> Result<(), String>;
    /// Folder and named-volume sizes of the given apps.
    fn sizes(&self, ids: &[&str]) -> Result<Vec<AppSize>, String>;
    /// Hands the tar stream of the app's folder to `read`.
    fn read_folder(&self, id: &str, read: &mut dyn FnMut(&mut dyn std::io::Read) -> Result<(), String>) -> Result<(), String>;
    /// Hands `podman volume export <podman_name>` to `read`.
    fn read_volume(&self, podman_name: &str, read: &mut dyn FnMut(&mut dyn std::io::Read) -> Result<(), String>) -> Result<(), String>;
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
    fn sizes(&self, ids: &[&str]) -> Result<Vec<AppSize>, String> {
        let out = shell::distro_script(&self.0.distro, SIZES_SH, ids);
        parse_sizes(&out.text, ids)
    }
    fn read_folder(&self, id: &str, read: &mut dyn FnMut(&mut dyn std::io::Read) -> Result<(), String>) -> Result<(), String> {
        shell::distro_read(&self.0.distro, FOLDER_SH, &[id], |out| read(out))
    }
    fn read_volume(&self, podman_name: &str, read: &mut dyn FnMut(&mut dyn std::io::Read) -> Result<(), String>) -> Result<(), String> {
        shell::distro_read(&self.0.distro, VOLUME_SH, &[podman_name], |out| read(out))
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

/// The slot a skip was last logged for, so a slot kept due is logged once, not every tick.
static SKIP_LOGGED: Mutex<Option<PrimitiveDateTime>> = Mutex::new(None);

fn read_baseline(paths: &Paths) -> Option<u64> {
    std::fs::read(schedule_file(paths)).ok().and_then(|b| serde_json::from_slice::<ScheduleState>(&b).ok()).map(|b| b.baseline_unix)
}

/// Logs `msg` once for the current scheduled slot.
pub fn log_once_per_slot(paths: &Paths, msg: &str) {
    let sched = settings::load(paths).backup.schedule;
    let slot = local_of(now()).and_then(|local| latest_slot(&sched, local));
    let mut logged = SKIP_LOGGED.lock().unwrap_or_else(|e| e.into_inner());
    if *logged != slot {
        *logged = slot;
        crate::log(paths, msg);
    }
}

/// A drive letter that is not there at all (a USB disk that is unplugged). Instant, no disk or
/// network access; UNC paths and present drives are not checked here (the run checks them).
fn drive_missing(location: &str) -> bool {
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    let b = location.as_bytes();
    if !(b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':') {
        return false;
    }
    let root: Vec<u16> = format!("{}:\\", b[0] as char).encode_utf16().chain(Some(0)).collect();
    // SAFETY: NUL-terminated string. 1 is DRIVE_NO_ROOT_DIR.
    unsafe { GetDriveTypeW(root.as_ptr()) == 1 }
}

/// Called every supervisor tick: true when a scheduled backup should start now. The caller starts
/// it and only then marks the slot as done (`consume_slot`), so a run that cannot start keeps it
/// due. While no location or no password is set, the slot is passed over (the starting point moves
/// on), so the first scheduled run after setting them is the next slot, not one at once. While the
/// location's drive is missing, the slot stays due (logged once) and runs when the drive is back.
pub fn scheduled_due(paths: &Paths) -> bool {
    !running() && schedule_check(paths, &settings::load(paths), now())
}

fn schedule_check(paths: &Paths, s: &settings::Settings, t: u64) -> bool {
    let sched = &s.backup.schedule;
    if sched.kind == "off" {
        return false;
    }
    let Some(baseline) = read_baseline(paths) else {
        write_baseline(paths, t);
        return false;
    };
    // A run made while the clock was wrong in the future would hold every later slot back.
    if baseline > t + FUTURE_SLACK {
        crate::log(paths, &format!("backup schedule: its last run ({baseline}) is in the future; counting from now"));
        write_baseline(paths, t);
        return false;
    }
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
        crate::log(paths, &format!("scheduled backup skipped: {reason}"));
        write_baseline(paths, t);
        return false;
    }
    if s.backup.location.as_deref().is_some_and(drive_missing) {
        log_once_per_slot(paths, "scheduled backup waits: the backup folder's drive is not connected");
        return false;
    }
    true
}

/// Marks the current slot as done; called once a scheduled run has started.
pub fn consume_slot(paths: &Paths) {
    write_baseline(paths, now());
}

// ---- History and listing ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct HistoryEntry {
    file: String,
    created_unix: u64,
    size_bytes: u64,
    apps: Vec<String>,
    /// ok | partial (a file with left-out entries) | failed | cancelled
    status: String,
    /// A locale code, as in `Progress`.
    error: Option<String>,
    #[serde(default)]
    detail: Option<String>,
    step: Option<String>,
    /// As in `Progress`.
    #[serde(default)]
    skipped: u64,
    #[serde(default)]
    notes: Vec<String>,
    /// Started by the schedule, not by "Back up now".
    #[serde(default)]
    scheduled: bool,
}

/// How many notes a history entry or a `done` event keeps.
const NOTES_MAX: usize = 10;

fn has_file(status: &str) -> bool {
    status == "ok" || status == "partial"
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
    /// ok (verified when written) | partial (verified, but some files or apps were left out:
    /// `skipped`, `notes`) | failed (no file) | unverified (a Konak file this PC has no record of,
    /// e.g. copied from another PC)
    pub status: &'static str,
    /// A failed run's locale code, and its technical text.
    pub error: Option<String>,
    pub detail: Option<String>,
    pub skipped: u64,
    pub notes: Vec<String>,
}

/// `last_scheduled_backup` (contract): the newest run the schedule started.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct ScheduledRun {
    pub created_unix: u64,
    /// ok | partial | failed | cancelled
    pub status: String,
    pub error: Option<String>,
    pub detail: Option<String>,
    pub skipped: u64,
}

pub fn last_scheduled(paths: &Paths) -> Option<ScheduledRun> {
    load_history(paths).into_iter().find(|h| h.scheduled).map(|h| ScheduledRun { created_unix: h.created_unix, status: h.status, error: h.error, detail: h.detail, skipped: h.skipped })
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

/// The backup files in the location, plus the recent failed runs, newest first. A file this PC has
/// no record of (another install's, copied here) shows its manifest's time and apps when the stored
/// password opens it (one scrypt and the manifest, not a full read: it stays `unverified`), and
/// otherwise its write time and no apps.
pub fn list(paths: &Paths) -> Vec<BackupEntry> {
    let history = load_history(paths);
    let mut out = vec![];
    if let Some(loc) = settings::load(paths).backup.location {
        let mut pw: Option<Option<SecretString>> = None;
        for (name, meta) in konak_files(Path::new(&loc)) {
            let file = Path::new(&loc).join(&name).to_string_lossy().to_string();
            let known = history.iter().find(|h| has_file(&h.status) && same_file(&h.file, &file));
            let (created_unix, apps) = match known {
                Some(h) => (h.created_unix, h.apps.clone()),
                None => {
                    let pw = pw.get_or_insert_with(|| settings::password(paths).ok().flatten());
                    match pw.as_ref().and_then(|pw| fmt::open(Path::new(&file), pw).ok()) {
                        Some(b) => (b.manifest.created_unix, b.manifest.apps.iter().map(|a| a.id.clone()).collect()),
                        None => (meta.modified().ok().and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs()), vec![]),
                    }
                }
            };
            let status = match known {
                Some(h) if h.status == "partial" => "partial",
                Some(_) => "ok",
                None => "unverified",
            };
            let (skipped, notes) = known.map(|h| (h.skipped, h.notes.clone())).unwrap_or_default();
            out.push(BackupEntry { file, created_unix, size_bytes: meta.len(), apps, status, error: None, detail: None, skipped, notes });
        }
    }
    for h in history.iter().filter(|h| h.status == "failed") {
        out.push(BackupEntry { file: h.file.clone(), created_unix: h.created_unix, size_bytes: 0, apps: h.apps.clone(), status: "failed", error: h.error.clone(), detail: h.detail.clone(), skipped: 0, notes: vec![] });
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
/// files come right after the manifest (after the apps in files written before Photos and Media
/// came first), so this reads (and decrypts) up to them; the rest of the file is not checked here,
/// a restore verifies all of it first.
pub fn inspect(paths: &Paths, path: &str, password: &str) -> Result<Inspected, String> {
    if settings::wsl_path(path) {
        return Err("backup_path_refused".into());
    }
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

fn install_id_file(paths: &Paths) -> PathBuf {
    paths.home.join("install-id")
}

/// This Konak install's id (32 hex digits), made once and kept in the Konak home. Backups carry it
/// in their manifest, so retention deletes only this install's files (a shared NAS folder, or a
/// file copied in by hand, is left alone).
pub fn install_id(paths: &Paths) -> String {
    let file = install_id_file(paths);
    if let Ok(s) = std::fs::read_to_string(&file) {
        let s = s.trim();
        if s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return s.to_string();
        }
    }
    let mut bytes = [0u8; 16];
    if let Err(e) = getrandom::fill(&mut bytes) {
        crate::log(paths, &format!("backup: no random install id ({e}); using the time and pid"));
        bytes[..8].copy_from_slice(&std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos().to_le_bytes()[..8]);
        bytes[8..12].copy_from_slice(&std::process::id().to_le_bytes());
    }
    let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    if let Err(e) = settings::write_atomic(&file, id.as_bytes()) {
        crate::log(paths, &format!("backup: cannot save the install id: {e}"));
    }
    id
}

/// Deletes Konak's backups beyond the newest `keep` in `dir`. `written` is the file this run just
/// wrote and verified: it takes one of the `keep` places and is never deleted. Of the other files
/// with Konak's name pattern and magic line, only those that open with `password` (age header,
/// manifest entry and its tar checksum: a quick check, not a full verify, which would read every
/// file end to end), were made by this install (its id in the manifest, or, for files of earlier
/// builds without an id, listed in this install's history) and whose manifest time is not in the
/// future are counted, ranked by that time, and may be deleted. Any other file (another password,
/// another install, planted, damaged, a future time) is neither counted nor deleted, with a log
/// line. Returns the deleted names.
pub fn prune(paths: &Paths, dir: &Path, keep: usize, password: &SecretString, written: &Path) -> Vec<String> {
    let written_name = written.file_name().map(|n| n.to_string_lossy().to_string());
    let limit = now() + FUTURE_SLACK;
    let ours = install_id(paths);
    let history = load_history(paths);
    let mut counted = vec![];
    for (name, _) in konak_files(dir) {
        if Some(&name) == written_name.as_ref() {
            continue;
        }
        let path = dir.join(&name);
        let in_history = || history.iter().any(|h| has_file(&h.status) && same_file(&h.file, &path.to_string_lossy()));
        match fmt::open(&path, password) {
            Ok(b) if b.manifest.created_unix > limit => crate::log(paths, &format!("backup retention: {name} not counted, its time is in the future")),
            Ok(b) if !(b.manifest.install_id.as_deref() == Some(ours.as_str()) || (b.manifest.install_id.is_none() && in_history())) => {
                crate::log(paths, &format!("backup retention: {name} not counted, another Konak install made it"))
            }
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
pub(crate) fn free_space(dir: &Path) -> Result<u64, String> {
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
        self.host.progress(&Progress { state: "running", step, app: app.map(String::from), done_bytes: Some(done.min(self.total.get())), total_bytes: Some(self.total.get()), error: None, detail: None, skipped: None, notes: None });
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

/// The app an unfinished backup left stopped, if any (what `recover` will start again).
pub fn inflight_app(paths: &Paths) -> Option<String> {
    let body = std::fs::read(inflight_file(paths)).ok()?;
    serde_json::from_slice::<serde_json::Value>(&body).ok().and_then(|v| v["app"].as_str().map(String::from))
}

/// At start: an app left stopped by a backup that did not finish is started again (unless the user
/// had stopped it). Returns its id.
pub fn recover(paths: &Paths, apps: &dyn Apps) -> Option<String> {
    std::fs::metadata(inflight_file(paths)).ok()?;
    let id = inflight_app(paths);
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

/// One backup run, start to end. `_guard` proves no other run is going on; `scheduled` says who
/// started it (the history keeps it for `last_scheduled`). On failure or cancel the partial file is
/// gone and every app is back in its previous running state. The result is recorded in the history
/// and reported through `host` (`done`, `failed` or `cancelled`).
pub fn run(paths: &Paths, host: &dyn Host, apps: &dyn Apps, _guard: &Guard, log_n: u8, scheduled: bool) -> Result<PathBuf, Failed> {
    let t0 = Instant::now();
    CANCEL.store(false, Ordering::SeqCst);
    let _marker = RunMarker::new(paths, "backup");
    let created = now();
    let ctx = Ctx { host, total: Cell::new(0), last: Cell::new(None) };
    let mut partial = None;
    let mut ids = vec![];
    let result = run_steps(paths, &ctx, apps, created, log_n, &mut partial, &mut ids);
    let was_cancelled = result.is_err() && cancelled();
    CANCEL.store(false, Ordering::SeqCst);
    let file_for_history = match (&result, &partial) {
        (Ok((p, _)), _) => p.to_string_lossy().to_string(),
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
    let entry = |status: &str, size_bytes: u64, error: Option<String>, detail: Option<String>, step: Option<String>, skipped: u64, notes: Vec<String>| HistoryEntry {
        file: file_for_history.clone(),
        created_unix: created,
        size_bytes,
        apps: ids.clone(),
        status: status.into(),
        error,
        detail,
        step,
        skipped,
        notes,
        scheduled,
    };
    match &result {
        Ok((p, left_out)) => {
            let size = std::fs::metadata(p).map_or(0, |m| m.len());
            let notes: Vec<String> = left_out.iter().take(NOTES_MAX).map(|n| redact(n)).collect();
            let skipped = left_out.len() as u64;
            crate::log(paths, &format!("backup done: {} ({size} bytes, {:.0} s, {skipped} left out)", p.display(), t0.elapsed().as_secs_f64()));
            record(paths, entry(if skipped > 0 { "partial" } else { "ok" }, size, None, None, None, skipped, notes.clone()));
            host.progress(&Progress { state: "done", step: "done", app: None, done_bytes: Some(ctx.total.get()), total_bytes: Some(ctx.total.get()), error: None, detail: None, skipped: Some(skipped), notes: Some(notes) });
        }
        Err(f) if was_cancelled => {
            crate::log(paths, &format!("backup cancelled at {}{}", f.step, f.app.as_deref().map(|a| format!(" ({a})")).unwrap_or_default()));
            record(paths, entry("cancelled", 0, Some(CANCELLED.into()), None, Some(f.step.into()), 0, vec![]));
            host.progress(&Progress { state: "cancelled", step: f.step, app: f.app.clone(), done_bytes: None, total_bytes: None, error: Some(CANCELLED.into()), detail: None, skipped: None, notes: None });
        }
        Err(f) => {
            crate::log(paths, &format!("backup failed at {}{}: {}", f.step, f.app.as_deref().map(|a| format!(" ({a})")).unwrap_or_default(), redact(&f.error)));
            let (error, detail) = crate::redact::code_and_detail(&f.error, if f.step == "app" { "backup_app_failed" } else { "backup_failed" });
            record(paths, entry("failed", 0, Some(error.clone()), detail.clone(), Some(f.step.into()), 0, vec![]));
            host.progress(&Progress { state: "failed", step: f.step, app: f.app.clone(), done_bytes: None, total_bytes: None, error: Some(error), detail, skipped: None, notes: None });
        }
    }
    result.map(|(p, _)| p)
}

/// Whether an error came from writing the backup file itself (then the file is unusable and the
/// run fails), not from the app or the distro.
fn is_write_error(e: &str) -> bool {
    e.contains("backup_no_space") || e.contains("cannot write the backup file")
}

/// The run; on success the backup file and what it left out (from `checksums.json`, plus apps that
/// were archived but did not start again).
#[allow(clippy::too_many_arguments)]
fn run_steps(paths: &Paths, ctx: &Ctx, apps: &dyn Apps, created: u64, log_n: u8, partial_out: &mut Option<PathBuf>, ids_out: &mut Vec<String>) -> Result<(PathBuf, Vec<String>), Failed> {
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
    let sizes = if id_refs.is_empty() { vec![] } else { apps.sizes(&id_refs).map_err(|e| fail("prepare", e))? };
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
        install_id: Some(install_id(paths)),
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

    // The apps first, each at rest; Photos and Media after them (see the module comment).
    let mut not_started = vec![];
    'apps: for z in &sizes {
        let id = z.id.as_str();
        let app_fail = |e: String| Failed { step: "app", app: Some(id.to_string()), error: e };
        let left_out = |w: &mut Writer<_>, why: &str| {
            crate::log(paths, &format!("backup: {id} left out: {why}"));
            w.skip(format!("apps/{id} (left out: {})", crate::redact::Redactor::new(vec![]).apply(why)));
        };
        if cancelled() {
            return Err(app_fail(CANCELLED.into()));
        }
        ctx.step("app", Some(id), w.bytes());
        let deadline = Instant::now() + BUSY_WAIT;
        while !ctx.host.try_busy(id) {
            if cancelled() {
                return Err(app_fail(CANCELLED.into()));
            }
            if Instant::now() >= deadline {
                left_out(&mut w, "busy");
                continue 'apps;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
        // Removed since the sizes were taken (the panel or an MCP remove): left out with a note,
        // the other apps are still backed up.
        if !State::try_load(paths).map_err(|e| fail("app", e))?.apps.iter().any(|a| a.id == id) {
            ctx.host.release(id, &Ok(()));
            crate::log(paths, &format!("backup: {id} was removed during the backup; left out"));
            w.skip(format!("apps/{id} (removed during the backup)"));
            continue;
        }
        // Without the marker, a Konak that ended now would leave the app stopped: it is not stopped then.
        if let Err(e) = settings::write_atomic(&inflight_file(paths), serde_json::json!({ "app": id }).to_string().as_bytes()) {
            let e = format!("cannot record the backup in progress: {e}");
            ctx.host.release(id, &Err(e.clone()));
            left_out(&mut w, &e);
            continue;
        }
        let before = w.bytes();
        let archived = Cell::new(false);
        let result = with_stopped(apps, id, || {
            let r = archive_app(apps, ctx, &mut w, z, created);
            archived.set(r.is_ok());
            r
        });
        let _ = std::fs::remove_file(inflight_file(paths));
        ctx.host.release(id, &result);
        match result {
            Ok(()) => {}
            Err(e) if cancelled() => return Err(app_fail(e)),
            // Its data is all in the file; the app is reported failed through `release`.
            Err(e) if archived.get() => {
                crate::log(paths, &format!("backup: {id} was backed up but did not start again: {e}"));
                not_started.push(format!("apps/{id} (backed up, but did not start again: {e})"));
            }
            // Nothing of it was written (it could not be stopped, the distro could not list it):
            // the file stays whole without it.
            Err(e) if w.bytes() == before && !is_write_error(&e) => left_out(&mut w, &e),
            Err(e) => return Err(app_fail(e)),
        }
    }

    for (step, root, on) in [("photos", &paths.photos, photos.is_some()), ("media", &paths.media, media.is_some())] {
        if !on {
            continue;
        }
        ctx.step(step, None, w.bytes());
        let mut skipped = vec![];
        walk(root, &mut skipped, step, &mut |rel, abs, meta| {
            if cancelled() {
                return Err(CANCELLED.into());
            }
            let mtime = meta.modified().ok().and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
            match std::fs::File::open(abs) {
                Ok(f) => {
                    let r = std::io::BufReader::with_capacity(1 << 20, Cancellable(f));
                    let whole = w.add_reader(&format!("{step}/{rel}"), meta.len(), r, 0o644, mtime).map_err(write_error)?;
                    // A file still being written (an upload) grew or changed after its size was
                    // taken: what was archived is its start only. (One that shrank is flagged by
                    // the writer.)
                    if whole && changed_since(abs, meta) {
                        w.skip(format!("{step}/{rel} (changed while it was read)"));
                    }
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

    // The whole file read back (decrypted, every checksum) before it gets its final name.
    ctx.step("verify", None, ctx.total.get());
    let b = fmt::open(&partial, &pw).map_err(|e| fail("verify", e.to_string()))?;
    let read_back = b.manifest.clone();
    b.for_each_entry(|_, _, _| if cancelled() { Err(CANCELLED.into()) } else { Ok(()) }).map_err(|e| fail("verify", e.to_string()))?;
    if read_back != manifest {
        return Err(fail("verify", "backup_damaged: the manifest read back differs"));
    }

    ctx.step("finish", None, ctx.total.get());
    std::fs::rename(&partial, &final_path).map_err(|e| fail("finish", format!("cannot rename the finished backup: {e}")))?;
    *partial_out = None;
    prune(paths, &dir, s.backup.keep as usize, &pw, &final_path);
    let mut left_out = sums.skipped;
    left_out.extend(not_started);
    Ok((final_path, left_out))
}

/// The file's size or write time differs from `meta` (taken before it was read).
fn changed_since(path: &Path, meta: &std::fs::Metadata) -> bool {
    match std::fs::metadata(path) {
        Ok(now) => now.len() != meta.len() || now.modified().ok() != meta.modified().ok(),
        Err(_) => true,
    }
}

/// Streams one app's folder and named volumes out of the distro into the archive.
fn archive_app<W: Write>(apps: &dyn Apps, ctx: &Ctx, w: &mut Writer<W>, z: &AppSize, mtime: u64) -> Result<(), String> {
    let id = z.id.as_str();
    apps.read_folder(id, &mut |out| w.add_stream(&format!("apps/{id}/folder"), Cancellable(out), mtime, &mut |n| ctx.bytes("app", Some(id), n)).map(|_| ()).map_err(write_error))
        .map_err(|e| format!("archiving the folder of {id} failed: {e}"))?;
    for v in &z.volumes {
        apps.read_volume(&v.podman_name, &mut |out| w.add_stream(&format!("apps/{id}/volumes/{}", v.name), Cancellable(out), mtime, &mut |n| ctx.bytes("app", Some(id), n)).map(|_| ()).map_err(write_error))
            .map_err(|e| format!("exporting volume {} of {id} failed: {e}", v.podman_name))?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;
    use time::macros::datetime;

    /// Apps without a distro: `sizes` as given, and every folder and volume stream is `data`.
    /// `on_read` hears `folder <id>` / `volume <podman name>` before each stream is handed over.
    struct FakeApps {
        running: bool,
        fail_stop: bool,
        /// Only this app's stop fails.
        fail_stop_of: Option<&'static str>,
        fail_start: bool,
        calls: RefCell<Vec<String>>,
        sizes: Vec<AppSize>,
        data: Vec<u8>,
        on_read: Option<ReadHook>,
    }

    type ReadHook = Box<dyn Fn(&str)>;
    /// What was read, and the inflight marker's app at that moment.
    type Seen = std::rc::Rc<RefCell<Vec<(String, Option<String>)>>>;

    impl FakeApps {
        fn new(running: bool) -> Self {
            FakeApps { running, fail_stop: false, fail_stop_of: None, fail_start: false, calls: RefCell::new(vec![]), sizes: vec![], data: b"tar".to_vec(), on_read: None }
        }
        fn read(&self, what: String, read: &mut dyn FnMut(&mut dyn std::io::Read) -> Result<(), String>) -> Result<(), String> {
            if let Some(f) = &self.on_read {
                f(&what);
            }
            self.calls.borrow_mut().push(format!("read {what}"));
            read(&mut self.data.as_slice())
        }
    }

    impl Apps for FakeApps {
        fn running(&self, id: &str) -> Result<bool, String> {
            self.calls.borrow_mut().push(format!("running {id}"));
            Ok(self.running)
        }
        fn stop(&self, id: &str) -> Result<(), String> {
            self.calls.borrow_mut().push(format!("stop {id}"));
            if self.fail_stop || self.fail_stop_of == Some(id) { Err("stop failed".into()) } else { Ok(()) }
        }
        fn start(&self, id: &str) -> Result<(), String> {
            self.calls.borrow_mut().push(format!("start {id}"));
            if self.fail_start { Err("start failed".into()) } else { Ok(()) }
        }
        fn sizes(&self, ids: &[&str]) -> Result<Vec<AppSize>, String> {
            Ok(self.sizes.iter().filter(|s| ids.contains(&s.id.as_str())).cloned().collect())
        }
        fn read_folder(&self, id: &str, read: &mut dyn FnMut(&mut dyn std::io::Read) -> Result<(), String>) -> Result<(), String> {
            self.read(format!("folder {id}"), read)
        }
        fn read_volume(&self, podman_name: &str, read: &mut dyn FnMut(&mut dyn std::io::Read) -> Result<(), String>) -> Result<(), String> {
            self.read(format!("volume {podman_name}"), read)
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
        let ours = |created: u64| Manifest { install_id: Some(install_id(&paths)), ..manifest(created) };
        // Real backups of this install, made with the current password: (name, created).
        let real = [
            ("konak-20261120-030000.konakbackup", t - 4 * day),
            ("konak-20261121-030000.konakbackup", t - 3 * day),
            ("konak-20261122-030000.konakbackup", t - 2 * day),
            ("konak-20261123-030000.konakbackup", t - day),
        ];
        for (n, created) in real {
            write_sample(&dir.join(n), pw_now, &ours(created), b"x");
        }
        // The one this run wrote, older by its manifest than the others (a clock that went back):
        // still never deleted.
        let written = dir.join("konak-20261124-030000.konakbackup");
        write_sample(&written, pw_now, &ours(t - 10 * day), b"x");
        // Old, but made with another password: not deleted, not counted.
        write_sample(&dir.join("konak-20261101-030000.konakbackup"), "an earlier password", &ours(t - 30 * day), b"x");
        // Old, same password, but another install's (a shared NAS folder), or without an id and
        // unknown to this install's history (copied in by hand): not deleted, not counted.
        write_sample(&dir.join("konak-20261104-030000.konakbackup"), pw_now, &Manifest { install_id: Some("0".repeat(32)), ..manifest(t - 20 * day) }, b"x");
        write_sample(&dir.join("konak-20261105-030000.konakbackup"), pw_now, &manifest(t - 20 * day), b"x");
        // Planted: names far in the future with only the magic line, and one that opens but claims
        // a future time. None of them takes a place or is deleted.
        for i in 1..=7 {
            std::fs::write(dir.join(format!("konak-9999010{i}-000000.konakbackup")), b"KONAK-BACKUP\n").unwrap();
        }
        write_sample(&dir.join("konak-20991231-000000.konakbackup"), pw_now, &ours(t + 365 * day), b"x");
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
        assert_eq!(left.len(), 19 - 2, "only the two oldest real ones are gone: {left:?}");
        assert!(left.contains(&"konak-20261104-030000.konakbackup".to_string()) && left.contains(&"konak-20261105-030000.konakbackup".to_string()));
        assert_eq!(prune_candidates(vec![("a".into(), 1), ("b".into(), 3), ("c".into(), 2)], 1), ["c", "a"]);
        assert!(prune_candidates(vec![("a".into(), 1)], 5).is_empty());
    }

    #[test]
    fn retention_with_keep_one_keeps_only_the_written_file() {
        use crate::backup_format::tests::{manifest, pw, write_sample};
        let paths = crate::settings::tests::temp_paths("retention-one");
        let dir = temp_dir("retention-one");
        let t = now();
        // A file of an earlier build (no install id) that this install's history lists: its own.
        let old = dir.join("konak-20261120-030000.konakbackup");
        write_sample(&old, "the backup password", &manifest(t - 100), b"x");
        record(&paths, HistoryEntry { file: old.to_string_lossy().to_string(), created_unix: t - 100, size_bytes: 1, apps: vec![], status: "ok".into(), error: None, detail: None, step: None, skipped: 0, notes: vec![], scheduled: false });
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
        let r = run(&paths, &host, &FakeApps::new(true), &guard, 10, true);
        drop(guard);
        let listed = list(&paths);
        let history = load_history(&paths);
        let last = last_scheduled(&paths);
        let _ = std::fs::remove_dir_all(&paths.home);
        let last = last.unwrap();
        assert_eq!((last.status.as_str(), last.error.as_deref()), ("failed", Some("backup_no_location")), "the scheduled run's result for the panel's notice");
        assert_eq!(r.unwrap_err(), fail("prepare", "backup_no_location"));
        let last = host.events.borrow().last().cloned().unwrap();
        assert_eq!((last.state, last.step, last.error.as_deref(), last.detail.as_deref()), ("failed", "prepare", Some("backup_no_location"), None));
        assert_eq!((history[0].status.as_str(), history[0].error.as_deref()), ("failed", Some("backup_no_location")));
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
        let file = run(&paths, &Print, &apps, &guard, fmt::SCRYPT_LOG_N, false).unwrap();
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
        let r = run(&paths, &host, &FakeApps::new(true), &guard, 10, false);
        drop(guard);
        let file = r.unwrap();
        let (m, sums) = fmt::verify(&file, &SecretString::from("a long enough secret".to_string())).unwrap();
        let listed = list(&paths);
        let inspected = inspect(&paths, &file.to_string_lossy(), "").unwrap();
        let wrong = inspect(&paths, &file.to_string_lossy(), "wrong password!");
        let names: Vec<String> = std::fs::read_dir(&out).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        // Files this PC has no record of: one the stored password opens (copied from another
        // install), one it does not.
        let mut other = m.clone();
        other.created_unix = 5;
        other.apps = vec![AppManifest { id: "forgejo".into(), name: "git".into(), claimed: true, stopped: false, size_bytes: 1, volumes: vec![] }];
        for (name, password) in [("konak-20250101-000000.konakbackup", "a long enough secret"), ("konak-20240101-000000.konakbackup", "another password!")] {
            let w = std::io::BufWriter::new(std::fs::File::create(out.join(name)).unwrap());
            let (mut f, _) = Writer::new(w, &SecretString::from(password.to_string()), &other, 10).unwrap().finish(5).unwrap();
            f.flush().unwrap();
        }
        let unknown = list(&paths);
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
        let row = |name: &str| unknown.iter().find(|e| e.file.ends_with(name)).cloned().unwrap();
        let (copied, foreign) = (row("konak-20250101-000000.konakbackup"), row("konak-20240101-000000.konakbackup"));
        assert_eq!((copied.status, copied.created_unix, copied.apps.as_slice()), ("unverified", 5, &["forgejo".to_string()][..]), "the manifest's time and apps");
        assert_eq!(foreign.status, "unverified");
        assert!(foreign.created_unix > 1_000_000_000 && foreign.apps.is_empty(), "the file's write time: {foreign:?}");
        let steps: Vec<&str> = host.events.borrow().iter().map(|e| e.step).collect();
        assert!(steps.starts_with(&["prepare", "konak", "photos"]) && steps.ends_with(&["verify", "finish", "done"]), "{steps:?}");
    }

    const PW: &str = "a long enough secret";

    fn fake_host() -> FakeHost {
        FakeHost { events: RefCell::new(vec![]), busy: RefCell::new(vec![]) }
    }

    fn size(id: &str, volumes: &[&str]) -> AppSize {
        AppSize { id: id.into(), folder: 3, volumes: volumes.iter().map(|v| VolumeSize { podman_name: format!("{id}_{v}"), name: v.to_string(), bytes: 3 }).collect() }
    }

    /// A set-up Konak with these apps installed (running unless listed as stopped), a backup folder,
    /// a password and one photo. Returns the paths and the backup folder.
    fn konak_with_apps(tag: &str, ids: &[&str]) -> (Paths, PathBuf) {
        let paths = crate::settings::tests::temp_paths(tag);
        let out = temp_dir(&format!("{tag}-out"));
        std::fs::create_dir_all(&paths.photos).unwrap();
        std::fs::write(paths.photos.join("p.jpg"), b"jpeg").unwrap();
        let mut st = State::default();
        st.done = true;
        st.apps = ids.iter().map(|id| InstalledApp { id: id.to_string(), name: id.to_string(), installed_unix: 1, claimed: true, stopped: false }).collect();
        st.save(&paths);
        let mut s = settings::Settings::default();
        s.backup.location = Some(out.to_string_lossy().to_string());
        s.backup.schedule.kind = "off".into();
        settings::save(&paths, &s).unwrap();
        settings::set_password(&paths, None, PW).unwrap();
        (paths, out)
    }

    fn cleanup(paths: &Paths, out: &Path) {
        let _ = std::fs::remove_dir_all(out);
        let _ = std::fs::remove_dir_all(&paths.home);
    }

    fn files_in(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect()
    }

    #[test]
    fn a_run_archives_each_app_at_rest_then_photos() {
        let (paths, out) = konak_with_apps("run-apps", &["nextcloud", "jellyfin"]);
        let seen: Seen = Default::default();
        let mut apps = FakeApps::new(true);
        apps.sizes = vec![size("nextcloud", &["db"]), size("jellyfin", &[])];
        let (log, p2) = (seen.clone(), paths.clone());
        apps.on_read = Some(Box::new(move |what| log.borrow_mut().push((what.to_string(), inflight_app(&p2)))));
        let host = fake_host();
        let guard = test_guard();
        let r = run(&paths, &host, &apps, &guard, 10, false);
        drop(guard);
        let file = r.unwrap();
        let mut settings_entry = String::new();
        let b = fmt::open(&file, &SecretString::from(PW.to_string())).unwrap();
        let manifest = b.manifest.clone();
        let sums = b
            .for_each_entry(|_, p, r| {
                if p == "konak/settings.json" {
                    r.read_to_string(&mut settings_entry).map_err(|e| e.to_string())?;
                }
                Ok(())
            })
            .unwrap();
        let history = load_history(&paths);
        let (left_inflight, id) = (inflight_app(&paths), install_id(&paths));
        cleanup(&paths, &out);
        let order: Vec<&str> = sums.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(order, ["manifest.json", "konak/state.json", "konak/settings.json", "apps/nextcloud/folder/000000", "apps/nextcloud/volumes/db/000000", "apps/jellyfin/folder/000000", "photos/p.jpg"], "apps first, Photos after them");
        assert_eq!(
            *apps.calls.borrow(),
            ["running nextcloud", "stop nextcloud", "read folder nextcloud", "read volume nextcloud_db", "start nextcloud", "running jellyfin", "stop jellyfin", "read folder jellyfin", "start jellyfin"],
            "each app stopped while it is read"
        );
        for (what, marker) in seen.borrow().iter() {
            let app = what.split(' ').nth(1).unwrap().split('_').next().unwrap();
            assert_eq!(marker.as_deref(), Some(app), "the inflight marker names the app while it is read ({what})");
        }
        assert_eq!(left_inflight, None);
        assert!(settings_entry.contains("\"password_set\": false"), "{settings_entry}");
        assert_eq!(manifest.install_id.as_deref(), Some(id.as_str()));
        assert_eq!(*host.busy.borrow(), ["nextcloud", "jellyfin"]);
        assert_eq!((history[0].status.as_str(), history[0].skipped), ("ok", 0));
    }

    #[test]
    fn an_app_that_cannot_be_stopped_is_left_out_and_the_run_goes_on() {
        let (paths, out) = konak_with_apps("run-partial", &["nextcloud", "jellyfin"]);
        let mut apps = FakeApps::new(true);
        apps.sizes = vec![size("nextcloud", &["db"]), size("jellyfin", &[])];
        apps.fail_stop_of = Some("nextcloud");
        let host = fake_host();
        let guard = test_guard();
        let r = run(&paths, &host, &apps, &guard, 10, false);
        drop(guard);
        let file = r.unwrap();
        let (_, sums) = fmt::verify(&file, &SecretString::from(PW.to_string())).unwrap();
        let listed = list(&paths);
        let history = load_history(&paths);
        cleanup(&paths, &out);
        assert!(sums.entries.iter().all(|e| !e.path.starts_with("apps/nextcloud/")));
        assert!(sums.entries.iter().any(|e| e.path == "apps/jellyfin/folder/000000"));
        assert_eq!(sums.skipped, ["apps/nextcloud (left out: stop failed)"]);
        assert_eq!((history[0].status.as_str(), history[0].skipped, history[0].notes.as_slice()), ("partial", 1, &["apps/nextcloud (left out: stop failed)".to_string()][..]));
        assert_eq!((listed[0].status, listed[0].skipped), ("partial", 1));
        let done = host.events.borrow().last().cloned().unwrap();
        assert_eq!((done.state, done.skipped), ("done", Some(1)));
        // Its stop was undone.
        assert!(apps.calls.borrow().starts_with(&["running nextcloud".to_string(), "stop nextcloud".to_string(), "start nextcloud".to_string()]));
    }

    #[test]
    fn a_file_damaged_after_writing_is_never_published() {
        let (paths, out) = konak_with_apps("run-verify", &["nextcloud"]);
        let mut apps = FakeApps::new(true);
        apps.sizes = vec![size("nextcloud", &["db"])];
        // Incompressible, so most of it is on disk when the volume is read; then one byte of what
        // is already written changes (a bad disk, a sync tool).
        let mut x = 0x2545_f491_u32;
        apps.data = (0..3 << 20)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        let dir = out.clone();
        apps.on_read = Some(Box::new(move |what| {
            if what.starts_with("volume") {
                let name = files_in(&dir).into_iter().find(|n| n.ends_with(PARTIAL)).unwrap();
                let mut bytes = std::fs::read(dir.join(&name)).unwrap();
                assert!(bytes.len() > 4096);
                bytes[2000] ^= 1;
                let mut f = std::fs::OpenOptions::new().write(true).open(dir.join(&name)).unwrap();
                f.write_all(&bytes).unwrap();
            }
        }));
        let host = fake_host();
        let guard = test_guard();
        let r = run(&paths, &host, &apps, &guard, 10, false);
        drop(guard);
        let left = files_in(&out);
        cleanup(&paths, &out);
        assert_eq!(r.unwrap_err().step, "verify");
        assert!(left.is_empty(), "no backup file, no partial file: {left:?}");
    }

    #[test]
    fn a_cancelled_run_restarts_the_app_and_leaves_no_file() {
        {
            let _g = test_guard();
            assert_eq!(cancel().unwrap_err(), "backup_not_running", "nothing to cancel while no backup runs");
        }
        let (paths, out) = konak_with_apps("run-cancel", &["nextcloud"]);
        let mut apps = FakeApps::new(true);
        apps.sizes = vec![size("nextcloud", &["db"])];
        apps.on_read = Some(Box::new(|what| {
            if what.starts_with("folder") {
                assert_eq!(running_op().as_deref(), Some("backup"));
                cancel().unwrap();
            }
        }));
        let host = fake_host();
        let guard = test_guard();
        let r = run(&paths, &host, &apps, &guard, 10, false);
        let after = cancelled();
        drop(guard);
        let left = files_in(&out);
        let history = load_history(&paths);
        let inflight = inflight_app(&paths);
        cleanup(&paths, &out);
        assert!(r.is_err());
        assert!(!after, "the cancel request is cleared for the next run");
        assert!(left.is_empty(), "{left:?}");
        assert_eq!(inflight, None);
        assert_eq!((history[0].status.as_str(), history[0].error.as_deref()), ("cancelled", Some(CANCELLED)));
        let last = host.events.borrow().last().cloned().unwrap();
        assert_eq!((last.state, last.error.as_deref()), ("cancelled", Some(CANCELLED)));
        assert_eq!(apps.calls.borrow().last().map(String::as_str), Some("start nextcloud"), "started again");
    }

    #[test]
    fn recover_starts_only_an_app_the_user_had_running() {
        let paths = crate::settings::tests::temp_paths("recover");
        let mut st = State::default();
        st.done = true;
        st.apps = vec![
            InstalledApp { id: "jellyfin".into(), name: "jellyfin".into(), installed_unix: 1, claimed: true, stopped: true },
            InstalledApp { id: "immich".into(), name: "immich".into(), installed_unix: 1, claimed: true, stopped: false },
        ];
        st.save(&paths);
        let apps = FakeApps::new(false);
        let mark = |id: &str| settings::write_atomic(&inflight_file(&paths), serde_json::json!({ "app": id }).to_string().as_bytes()).unwrap();
        let none = recover(&paths, &apps);
        mark("jellyfin");
        let stopped = recover(&paths, &apps);
        let calls_after_stopped = apps.calls.borrow().clone();
        mark("immich");
        let running = recover(&paths, &apps);
        let left = inflight_app(&paths);
        let _ = std::fs::remove_dir_all(&paths.home);
        assert_eq!(none, None);
        assert_eq!(stopped.as_deref(), Some("jellyfin"));
        assert!(calls_after_stopped.is_empty(), "a user-stopped app stays stopped: {calls_after_stopped:?}");
        assert_eq!(running.as_deref(), Some("immich"));
        assert_eq!(*apps.calls.borrow(), ["start immich"]);
        assert_eq!(left, None);
    }

    #[test]
    fn schedule_checks_move_the_starting_point_only_when_they_should() {
        let paths = crate::settings::tests::temp_paths("schedule-check");
        let out = temp_dir("schedule-check-out");
        let (t, day) = (now(), 86_400);
        let mut s = settings::Settings::default();
        assert_eq!(s.backup.schedule.kind, "daily");
        // Not set up (no folder, no password): the slot is passed over, so setting them up later
        // does not start a backup at once.
        write_baseline(&paths, t - 3 * day);
        let not_ready = schedule_check(&paths, &s, t);
        let after_not_ready = read_baseline(&paths);
        s.backup.location = Some(out.to_string_lossy().to_string());
        s.backup.password_set = true;
        let just_ready = schedule_check(&paths, &s, t);
        // A due slot is consumed by the run that starts, not by the check.
        write_baseline(&paths, t - 3 * day);
        let due_now = schedule_check(&paths, &s, t);
        let after_due = read_baseline(&paths);
        consume_slot(&paths);
        let consumed = read_baseline(&paths);
        // A starting point in the future (a clock that was wrong) is reset to now.
        write_baseline(&paths, t + 365 * day);
        let future = schedule_check(&paths, &s, t);
        let after_future = read_baseline(&paths);
        // The folder's drive is not connected: kept due.
        let free = (b'D'..=b'Z').rev().map(|l| format!("{}:\\konak-backups", l as char)).find(|l| drive_missing(l));
        let missing = free.map(|loc| {
            s.backup.location = Some(loc);
            write_baseline(&paths, t - 3 * day);
            (schedule_check(&paths, &s, t), read_baseline(&paths))
        });
        cleanup(&paths, &out);
        assert!(!not_ready);
        assert_eq!(after_not_ready, Some(t));
        assert!(!just_ready, "the first scheduled run is the next slot");
        assert!(due_now);
        assert_eq!(after_due, Some(t - 3 * day));
        assert!(consumed.is_some_and(|b| b >= t));
        assert!(!future);
        assert_eq!(after_future, Some(t));
        if let Some((due, baseline)) = missing {
            assert!(!due);
            assert_eq!(baseline, Some(t - 3 * day), "still due once the drive is back");
        }
        assert!(!drive_missing(r"\\nas\share") && !drive_missing(r"C:\x"));
    }

    #[test]
    fn a_file_that_grows_while_read_is_noticed() {
        let dir = temp_dir("grow");
        let f = dir.join("upload.jpg");
        std::fs::write(&f, b"first part").unwrap();
        let meta = std::fs::metadata(&f).unwrap();
        let same = changed_since(&f, &meta);
        std::fs::OpenOptions::new().append(true).open(&f).unwrap().write_all(b" and more").unwrap();
        let grown = changed_since(&f, &meta);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!same && grown);
    }

    #[test]
    fn keep_awake_holds_the_pc_up_for_its_thread_only_while_it_lives() {
        use windows_sys::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED};
        // SAFETY: this test thread's own execution state only.
        let state = || unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
        state();
        let during = {
            let _awake = KeepAwake::new();
            let s = state();
            // Put it back for the rest of the block, as `state` cleared it.
            unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
            s
        };
        let after = state();
        assert_eq!(during & (ES_CONTINUOUS | ES_SYSTEM_REQUIRED), ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
        assert_eq!(after & ES_SYSTEM_REQUIRED, 0);
    }

    #[test]
    fn the_install_id_is_made_once() {
        let paths = crate::settings::tests::temp_paths("install-id");
        let (a, b) = (install_id(&paths), install_id(&paths));
        let _ = std::fs::remove_dir_all(&paths.home);
        assert_eq!(a, b);
        assert!(a.len() == 32 && a.bytes().all(|c| c.is_ascii_hexdigit()));
    }
}
