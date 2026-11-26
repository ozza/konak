//! The restore engine: brings apps, the local CA and the Photos/Media folders back from a backup
//! file (`backup_format`) into this Konak, a fresh install or one that runs already.
//!
//! Order of a run:
//! 1. `verify`: the whole file is read and checked (age authentication, every entry against
//!    `checksums.json`). Nothing is changed before this passes. The CA files are kept in memory.
//! 2. `konak`: the CA rule (`ca_action`). `settings.json` and the backup password are never restored.
//! 3. A second pass over the file. Each chosen app, one at a time and in the busy set (op `restore`):
//!    its existing data (only with `replace`) is set aside in the distro, its folder is unpacked,
//!    its volumes are imported, it is rendered for this PC's active web ports under its backed-up
//!    name (or `-2`, `-3`, ... when that name is taken), started with the install's health checks
//!    unless it was stopped, and added to `state.json`. Only then is the set-aside copy deleted; a
//!    failure puts it back and starts the old app again. One app's failure never stops the others.
//! 4. `photos` / `media` (only when asked): merged into the configured folders, never deleting or
//!    overwriting a file (`merge_file`).
//!
//! Errors the panel explains are short codes (`restore_app_exists`, ...); `Summary` lists the result.

use crate::apps;
use crate::backup::Guard;
use crate::backup_format::{self as fmt, AppManifest, Entry, Manifest, VolumeManifest};
use crate::catalog;
use crate::installer::{now, InstalledApp, State};
use crate::paths::Paths;
use crate::services::HOME_NAME;
use crate::settings;
use crate::shell;
use age::secrecy::SecretString;
use serde::Serialize;
use std::cell::Cell;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const RESTORE_SH: &str = include_str!("../scripts/app-restore.sh");
/// Names tried for an app: its backed-up name, then `-2` up to this.
const MAX_SUFFIX: u32 = 9;

// ---- Request, progress, result ---------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Request {
    pub path: String,
    /// Empty: the stored backup password.
    pub password: String,
    pub apps: Vec<String>,
    pub replace: bool,
    pub photos: bool,
    pub media: bool,
}

/// `restore-progress` event payload: the contract's fields, plus `summary` on the final event.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Progress {
    /// running | done | failed
    pub state: &'static str,
    /// verify | konak | app | photos | media | done
    pub step: &'static str,
    pub app: Option<String>,
    pub done_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub error: Option<String>,
    pub summary: Option<Summary>,
}

#[derive(Serialize, Clone, Debug, PartialEq, Default)]
pub struct Summary {
    pub restored: Vec<Restored>,
    pub failed: Vec<AppFailure>,
    pub photos: Option<FolderResult>,
    pub media: Option<FolderResult>,
    /// Codes: `restore_ca_restored`, `restore_ca_kept`, `restore_ca_invalid`, `restore_ca_failed`,
    /// `restore_settings_kept`.
    pub notes: Vec<&'static str>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Restored {
    pub id: String,
    pub name: String,
    /// The backed-up name, when it was taken here and the app got `name` instead.
    pub renamed_from: Option<String>,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct AppFailure {
    pub id: String,
    pub error: String,
}

#[derive(Serialize, Clone, Debug, PartialEq, Default)]
pub struct FolderResult {
    /// New files written.
    pub written: u64,
    /// Files already there with the same content (left alone).
    pub identical: u64,
    /// Files already there with other content: kept, and the backed-up one written next to it as
    /// `name (restored).ext`.
    pub kept_both: u64,
    pub failed: u64,
    /// The first failure's reason.
    pub error: Option<String>,
}

/// Where a run failed as a whole (an app's failure is in `Summary::failed` instead).
#[derive(Debug, PartialEq)]
pub struct Failed {
    pub step: &'static str,
    pub error: String,
}

fn fail(step: &'static str, error: impl Into<String>) -> Failed {
    Failed { step, error: error.into() }
}

// ---- What the engine needs -------------------------------------------------------------------------

/// The running Konak: events, the busy set, and the supervisor.
pub trait Host {
    fn progress(&self, p: &Progress);
    /// Marks the app busy (op `restore`); false when another operation holds it.
    fn try_busy(&self, id: &str) -> bool;
    fn release(&self, id: &str, result: &Result<(), String>);
    /// `state.json` changed: apply the app list to Caddy and mDNS now.
    fn apps_changed(&self);
    /// The root CA files were replaced: restart Caddy so it loads them.
    fn ca_changed(&self);
}

#[derive(Debug, PartialEq, Default, Clone)]
pub struct Probe {
    /// `/var/lib/konak/apps/<id>` exists.
    pub folder: bool,
    /// A set-aside copy of an earlier restore that did not finish.
    pub aside: bool,
    /// Volumes labelled with the app's compose project, or named as the backup's volumes.
    pub volumes: bool,
}

/// A stream into the distro.
pub trait Sink: Write {
    fn finish(self: Box<Self>) -> Result<(), String>;
    /// Ends it as failed; returns what the other side said, if anything.
    fn abort(self: Box<Self>) -> String;
}

impl Sink for shell::DistroWriter {
    fn finish(self: Box<Self>) -> Result<(), String> {
        shell::DistroWriter::finish(*self).map(|_| ())
    }
    fn abort(self: Box<Self>) -> String {
        shell::DistroWriter::abort(*self)
    }
}

/// What a restore changes for one app; `DistroTarget` in Konak's distro.
pub trait Target {
    fn probe(&self, id: &str, volumes: &[&str]) -> Result<Probe, String>;
    /// Removes the app's containers and moves its folder and volume content aside.
    fn set_aside(&self, id: &str) -> Result<(), String>;
    /// Removes what the restore made; with `had_old`, puts the set-aside data back.
    fn roll_back(&self, id: &str, had_old: bool) -> Result<(), String>;
    /// Deletes the set-aside data.
    fn commit(&self, id: &str) -> Result<(), String>;
    fn folder(&self, id: &str) -> Result<Box<dyn Sink>, String>;
    fn volume(&self, id: &str, v: &VolumeManifest) -> Result<Box<dyn Sink>, String>;
    /// Renders the app for the active ports; with `up`, starts it and runs the health checks.
    fn bring_up(&self, id: &str, name: &str, claimed: bool, up: bool) -> Result<(), String>;
    /// Starts the app's own containers again after a rollback (it was running before).
    fn start_old(&self, id: &str) -> Result<(), String>;
    fn name_free_on_lan(&self, name: &str) -> Result<bool, String>;
}

pub struct DistroTarget<'a>(pub &'a Paths);

impl DistroTarget<'_> {
    fn script(&self, args: &[&str]) -> Result<String, String> {
        let out = shell::distro_script(&self.0.distro, RESTORE_SH, args);
        crate::log(self.0, &format!("restore {} {}: {}", args[0], args[1], shell::tail(&out.text, 2)));
        if out.ok {
            Ok(out.text)
        } else {
            Err(format!("{} of {} failed: {}", args[0], args[1], shell::tail(&out.text, 4)))
        }
    }
}

pub fn parse_probe(text: &str) -> Result<Probe, String> {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.last() != Some(&"end") {
        return Err(format!("probe failed: {}", shell::tail(text, 3)));
    }
    let mut p = Probe::default();
    for l in &lines {
        match l.split_once(' ') {
            Some(("folder", v)) => p.folder = v == "1",
            Some(("aside", v)) => p.aside = v == "1",
            Some(("volumes", n)) => p.volumes |= n.parse::<u32>().unwrap_or(1) > 0,
            Some(("named", _)) => p.volumes = true,
            _ => {}
        }
    }
    Ok(p)
}

impl Target for DistroTarget<'_> {
    fn probe(&self, id: &str, volumes: &[&str]) -> Result<Probe, String> {
        let mut args = vec!["probe", id];
        args.extend_from_slice(volumes);
        parse_probe(&self.script(&args)?)
    }
    fn set_aside(&self, id: &str) -> Result<(), String> {
        self.script(&["aside", id]).map(|_| ())
    }
    fn roll_back(&self, id: &str, had_old: bool) -> Result<(), String> {
        self.script(&["rollback", id, if had_old { "1" } else { "0" }]).map(|_| ())
    }
    fn commit(&self, id: &str) -> Result<(), String> {
        self.script(&["commit", id]).map(|_| ())
    }
    fn folder(&self, id: &str) -> Result<Box<dyn Sink>, String> {
        Ok(Box::new(shell::DistroWriter::start(&self.0.distro, RESTORE_SH, &["extract", id])?))
    }
    fn volume(&self, id: &str, v: &VolumeManifest) -> Result<Box<dyn Sink>, String> {
        Ok(Box::new(shell::DistroWriter::start(&self.0.distro, RESTORE_SH, &["import", id, &v.name, &v.podman_name])?))
    }
    fn bring_up(&self, id: &str, name: &str, claimed: bool, up: bool) -> Result<(), String> {
        apps::render_restored(self.0, id, name, claimed, up)
    }
    fn start_old(&self, id: &str) -> Result<(), String> {
        let out = apps::compose(self.0, id, "up -d --remove-orphans");
        out.ok.then_some(()).ok_or_else(|| format!("start of {id} failed: {}", shell::tail(&out.text, 4)))
    }
    fn name_free_on_lan(&self, name: &str) -> Result<bool, String> {
        apps::name_free_on_lan(self.0, name)
    }
}

// ---- Rules ---------------------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
pub enum CaAction {
    Nothing,
    Install,
    Keep,
}

/// The CA rule: the backup's root is installed when this Konak has none, or when it is a fresh
/// install (no apps yet, so no device was asked to trust its root for one). A different root that
/// is in use is kept: swapping it would silently break every device that trusts it.
pub fn ca_action(current: Option<(&str, &str)>, backup: Option<(&str, &str)>, fresh: bool) -> CaAction {
    match (current, backup) {
        (_, None) => CaAction::Nothing,
        (None, Some(_)) => CaAction::Install,
        (Some(c), Some(b)) if c == b => CaAction::Nothing,
        (Some(_), Some(_)) if fresh => CaAction::Install,
        _ => CaAction::Keep,
    }
}

/// A path component that Windows stores as written: no reserved device name (`CON`, `NUL`,
/// `COM1`, ...), no characters Windows refuses, no trailing dot or space (Windows drops those, so
/// `a.` would land on `a`).
pub fn component_ok(c: &str) -> bool {
    const RESERVED: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];
    let base = c.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    let device = RESERVED.contains(&base.as_str()) || ((base.starts_with("COM") || base.starts_with("LPT")) && base.len() == 4 && base.as_bytes()[3].is_ascii_digit());
    !c.is_empty()
        && c != "."
        && c != ".."
        && c.len() <= 255
        && !device
        && !c.ends_with('.')
        && !c.ends_with(' ')
        && !c.chars().any(|ch| ch < ' ' || "<>:\"/\\|?*".contains(ch))
}

/// The file `rel` (`/`-separated, from the format's strict parser) under `root`, with its parent
/// folders created. Refused (`restore_path_refused`) when a component is not a plain name, when a
/// folder on the way is a link or junction, or when the parent's real path is not under the
/// root's real path.
pub fn safe_target(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let parts: Vec<&str> = rel.split('/').collect();
    if !parts.iter().all(|c| component_ok(c)) {
        return Err(format!("restore_path_refused: {rel}"));
    }
    std::fs::create_dir_all(root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    let real_root = std::fs::canonicalize(root).map_err(|e| format!("cannot resolve {}: {e}", root.display()))?;
    let mut cur = root.to_path_buf();
    for c in &parts[..parts.len() - 1] {
        cur.push(c);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() || !m.is_dir() => return Err(format!("restore_path_refused: {rel}")),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => std::fs::create_dir(&cur).map_err(|e| format!("cannot create {}: {e}", cur.display()))?,
            Err(e) => return Err(format!("cannot read {}: {e}", cur.display())),
        }
    }
    let real_parent = std::fs::canonicalize(&cur).map_err(|e| format!("cannot resolve {}: {e}", cur.display()))?;
    if !real_parent.starts_with(&real_root) {
        return Err(format!("restore_path_refused: {rel}"));
    }
    Ok(cur.join(parts[parts.len() - 1]))
}

/// `name (restored).ext`, `name (restored 2).ext`, ...
pub fn restored_name(dest: &Path, n: u32) -> PathBuf {
    let stem = dest.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let tag = if n == 1 { " (restored)".to_string() } else { format!(" (restored {n})") };
    let name = match dest.extension() {
        Some(ext) => format!("{stem}{tag}.{}", ext.to_string_lossy()),
        None => format!("{stem}{tag}"),
    };
    dest.with_file_name(name)
}

/// Renames `from` to `to` only if `to` does not exist (no replace, unlike `std::fs::rename`).
fn move_new(from: &Path, to: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    let wide = |p: &Path| p.as_os_str().to_string_lossy().encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let (a, b) = (wide(from), wide(to));
    // SAFETY: two NUL-terminated paths; flags 0 = fail if the target exists.
    if unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn exists_error(e: &io::Error) -> bool {
    // ERROR_FILE_EXISTS, ERROR_ALREADY_EXISTS
    e.kind() == io::ErrorKind::AlreadyExists || matches!(e.raw_os_error(), Some(80 | 183))
}

/// Fills `buf` as far as the reader goes.
fn read_full(r: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

fn same_content(a: &Path, b: &Path) -> io::Result<bool> {
    if std::fs::metadata(a)?.len() != std::fs::metadata(b)?.len() {
        return Ok(false);
    }
    let (mut fa, mut fb) = (io::BufReader::new(std::fs::File::open(a)?), io::BufReader::new(std::fs::File::open(b)?));
    let (mut ba, mut bb) = (vec![0u8; 1 << 16], vec![0u8; 1 << 16]);
    loop {
        let (na, nb) = (read_full(&mut fa, &mut ba)?, read_full(&mut fb, &mut bb)?);
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Merged {
    Written,
    Identical,
    KeptBoth,
}

/// Temporary name next to `dest`; `.partial` at the end, so a backup running meanwhile skips it.
fn temp_for(dest: &Path) -> PathBuf {
    let name = dest.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    dest.with_file_name(format!(".{name}.{}.konak-restore.partial", std::process::id()))
}

/// Writes one backed-up file into `root` at `rel` without ever deleting or overwriting a file:
/// - nothing there: written;
/// - a file with the same content: left alone (`Identical`), nothing is written;
/// - anything else: the existing one stays and the backed-up one is written as
///   `name (restored).ext` (or `name (restored 2).ext`, ...; one of those with the same content
///   counts as `Identical`).
///
/// The existing file is compared while the entry is read, so an identical file costs no write.
pub fn merge_file(root: &Path, rel: &str, data: &mut dyn Read) -> Result<Merged, String> {
    let dest = safe_target(root, rel)?;
    let temp = temp_for(&dest);
    let result = merge_into(&dest, &temp, data);
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn merge_into(dest: &Path, temp: &Path, data: &mut dyn Read) -> Result<Merged, String> {
    let io_err = |e: io::Error| format!("{}: {e}", dest.display());
    let existing = match std::fs::symlink_metadata(dest) {
        Ok(m) => Some(m),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(io_err(e)),
    };
    match existing {
        None => {
            let mut out = std::fs::File::create_new(temp).map_err(io_err)?;
            io::copy(data, &mut out).map_err(io_err)?;
            out.sync_all().map_err(io_err)?;
            drop(out);
            match move_new(temp, dest) {
                Ok(()) => return Ok(Merged::Written),
                // Created meanwhile: treat it like any existing file.
                Err(e) if exists_error(&e) => {}
                Err(e) => return Err(io_err(e)),
            }
        }
        Some(m) if m.is_file() && !m.file_type().is_symlink() => {
            if write_if_different(dest, temp, data).map_err(io_err)? {
                return Ok(Merged::Identical);
            }
        }
        // A folder, a link: never written through; the backed-up file goes next to it.
        Some(_) => {
            let mut out = std::fs::File::create_new(temp).map_err(io_err)?;
            io::copy(data, &mut out).map_err(io_err)?;
        }
    }
    for n in 1..=1000 {
        let cand = restored_name(dest, n);
        match std::fs::symlink_metadata(&cand) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => match move_new(temp, &cand) {
                Ok(()) => return Ok(Merged::KeptBoth),
                Err(e) if exists_error(&e) => continue,
                Err(e) => return Err(io_err(e)),
            },
            Ok(m) if m.is_file() && !m.file_type().is_symlink() && same_content(temp, &cand).map_err(io_err)? => {
                let _ = std::fs::remove_file(temp);
                return Ok(Merged::Identical);
            }
            _ => continue,
        }
    }
    Err(format!("{}: too many restored copies", dest.display()))
}

/// Compares `data` with the file at `dest` as it is read. Returns true when they are the same;
/// otherwise `temp` holds all of `data` (the prefix that matched is copied from `dest`).
fn write_if_different(dest: &Path, temp: &Path, data: &mut dyn Read) -> io::Result<bool> {
    let mut ex = io::BufReader::new(std::fs::File::open(dest)?);
    let (mut buf, mut exbuf) = (vec![0u8; 1 << 16], vec![0u8; 1 << 16]);
    let mut matched = 0u64;
    let pending = loop {
        let n = read_full(data, &mut buf)?;
        let m = read_full(&mut ex, &mut exbuf[..n.max(1)])?;
        if n == 0 {
            if m == 0 {
                return Ok(true);
            }
            // The existing file is longer.
            break 0;
        }
        if m != n || buf[..n] != exbuf[..n] {
            break n;
        }
        matched += n as u64;
    };
    let mut out = std::fs::File::create_new(temp)?;
    io::copy(&mut std::fs::File::open(dest)?.take(matched), &mut out)?;
    out.write_all(&buf[..pending])?;
    io::copy(data, &mut out)?;
    out.sync_all()?;
    Ok(false)
}

// ---- Reading the file ------------------------------------------------------------------------------

/// Counts what is read through it, and remembers a read error, so a failed copy tells the
/// archive's errors (the file is damaged) from the target's.
struct Tracked<'a> {
    inner: &'a mut dyn Read,
    count: &'a Cell<u64>,
    error: Option<String>,
}

impl Read for Tracked<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.inner.read(buf) {
            Ok(n) => {
                self.count.set(self.count.get() + n as u64);
                Ok(n)
            }
            Err(e) => {
                self.error = Some(e.to_string());
                Err(e)
            }
        }
    }
}

/// The file as it was verified: a different size or time before the second pass means it changed.
fn stamp(path: &Path) -> Option<(u64, Option<std::time::SystemTime>)> {
    std::fs::metadata(path).ok().map(|m| (m.len(), m.modified().ok()))
}

struct Verified {
    manifest: Manifest,
    total: u64,
    ca: Option<(String, String)>,
}

fn verify(path: &Path, pw: &SecretString, ctx: &Ctx) -> Result<Verified, Failed> {
    let read_err = |e: fmt::ReadError| {
        crate::log(ctx.paths, &format!("restore: {e}"));
        fail("verify", e.code)
    };
    let backup = fmt::open(path, pw).map_err(read_err)?;
    let manifest = backup.manifest.clone();
    let estimate = manifest.apps.iter().map(|a| a.size_bytes).sum::<u64>() + manifest.photos_bytes.unwrap_or(0) + manifest.media_bytes.unwrap_or(0);
    ctx.total.set(estimate);
    let count = Cell::new(0u64);
    let (mut crt, mut key) = (None, None);
    let sums = backup
        .for_each_entry(|e, _, r| {
            let mut body = vec![];
            let mut t = Tracked { inner: r, count: &count, error: None };
            match e {
                Entry::Konak(p) if p == "ca/root.crt" || p == "ca/root.key" => {
                    (&mut t).take(1 << 20).read_to_end(&mut body).map_err(|e| e.to_string())?;
                }
                _ => {
                    io::copy(&mut t, &mut io::sink()).map_err(|e| e.to_string())?;
                }
            }
            match e {
                Entry::Konak(p) if p == "ca/root.crt" => crt = String::from_utf8(body).ok(),
                Entry::Konak(p) if p == "ca/root.key" => key = String::from_utf8(body).ok(),
                _ => {}
            }
            ctx.bytes("verify", None, count.get());
            Ok(())
        })
        .map_err(|e| {
            // A read error inside the callback is the archive's: report it as damaged.
            let e = if e.code == "backup_restore_failed" { fmt::ReadError { code: "backup_damaged", detail: e.detail } } else { e };
            read_err(e)
        })?;
    let total = sums.entries.iter().skip(1).map(|e| e.size).sum();
    Ok(Verified { manifest, total, ca: crt.zip(key) })
}

// ---- The run ---------------------------------------------------------------------------------------

/// Progress events, at most twice a second for byte counts.
struct Ctx<'a> {
    paths: &'a Paths,
    host: &'a dyn Host,
    total: Cell<u64>,
    last: Cell<Option<Instant>>,
}

impl Ctx<'_> {
    fn step(&self, step: &'static str, app: Option<&str>, done: u64) {
        self.last.set(Some(Instant::now()));
        let total = self.total.get();
        self.host.progress(&Progress { state: "running", step, app: app.map(String::from), done_bytes: Some(done.min(total)), total_bytes: Some(total), error: None, summary: None });
    }
    fn bytes(&self, step: &'static str, app: Option<&str>, done: u64) {
        if self.last.get().is_none_or(|t| t.elapsed() >= Duration::from_millis(500)) {
            self.step(step, app, done);
        }
    }
}

/// The quick checks `restore_backup` answers at once.
pub fn precheck(paths: &Paths, req: &Request) -> Result<(), String> {
    if crate::backup::running() {
        return Err("backup_running".into());
    }
    if !State::try_load(paths)?.done {
        return Err("backup_not_set_up".into());
    }
    if req.apps.is_empty() && !req.photos && !req.media {
        return Err("restore_nothing_chosen".into());
    }
    if !fmt::has_magic(Path::new(&req.path)) {
        return Err(if Path::new(&req.path).is_file() { "backup_not_konak" } else { "backup_unreadable" }.into());
    }
    Ok(())
}

fn password(paths: &Paths, given: &str) -> Result<SecretString, String> {
    if given.is_empty() {
        settings::password(paths)?.ok_or_else(|| "backup_no_password".to_string())
    } else {
        Ok(SecretString::from(given.to_string()))
    }
}

/// One restore, start to end. `_guard` proves no backup or other restore runs. The result goes to
/// `host` as the final `done` (everything chosen was restored) or `failed` event, with the summary.
pub fn run(paths: &Paths, host: &dyn Host, target: &dyn Target, _guard: &Guard, req: &Request) -> Result<Summary, Failed> {
    let t0 = Instant::now();
    let ctx = Ctx { paths, host, total: Cell::new(0), last: Cell::new(None) };
    let mut summary = Summary::default();
    let result = run_steps(&ctx, target, req, &mut summary);
    let redact = |e: &str| crate::redact::Redactor::new(vec![]).apply(e);
    for f in &mut summary.failed {
        f.error = redact(&f.error);
    }
    let error = match &result {
        Err(f) => Some(redact(&f.error)),
        Ok(()) if !summary.failed.is_empty() || [&summary.photos, &summary.media].iter().any(|f| f.as_ref().is_some_and(|f| f.failed > 0)) => Some("restore_some_failed".to_string()),
        Ok(()) => None,
    };
    crate::log(paths, &format!("restore finished in {:.0} s: {}", t0.elapsed().as_secs_f64(), serde_json::to_string(&summary).unwrap_or_default()));
    let (state, step) = match (&result, &error) {
        (Err(f), _) => ("failed", f.step),
        (Ok(()), Some(_)) => ("failed", "done"),
        (Ok(()), None) => ("done", "done"),
    };
    if let Some(e) = &error {
        crate::log(paths, &format!("restore failed at {step}: {e}"));
    }
    host.progress(&Progress { state, step, app: None, done_bytes: Some(ctx.total.get()), total_bytes: Some(ctx.total.get()), error, summary: Some(summary.clone()) });
    result.map(|()| summary)
}

fn run_steps(ctx: &Ctx, target: &dyn Target, req: &Request, summary: &mut Summary) -> Result<(), Failed> {
    let paths = ctx.paths;
    ctx.step("verify", None, 0);
    let state = State::try_load(paths).map_err(|e| fail("verify", e))?;
    if !state.done {
        return Err(fail("verify", "backup_not_set_up"));
    }
    let pw = password(paths, &req.password).map_err(|e| fail("verify", e))?;
    let file = Path::new(&req.path);
    let before = stamp(file);
    let v = verify(file, &pw, ctx)?;
    crate::log(paths, &format!("restore: {} verified ({} apps in it; chosen {:?}, replace {}, photos {}, media {})", file.display(), v.manifest.apps.len(), req.apps, req.replace, req.photos, req.media));

    ctx.step("konak", None, 0);
    restore_ca(paths, ctx.host, &v, state.apps.is_empty(), summary);
    if v.manifest.konak_files.iter().any(|f| f == "settings.json") {
        summary.notes.push("restore_settings_kept");
    }

    if stamp(file) != before {
        return Err(fail("verify", "restore_file_changed"));
    }
    ctx.total.set(v.total);
    let backup = fmt::open(file, &pw).map_err(|e| fail("verify", e.code))?;
    let count = Cell::new(0u64);
    let mut walk = Walk { ctx, target, req, manifest: &v.manifest, count: &count, current: Current::None, seen: vec![], summary };
    let walked = backup.for_each_entry(|e, _, r| walk.entry(e, r)).map(|_| ());
    match &walked {
        Ok(()) => walk.end_current(),
        Err(e) => {
            walk.fail_current("backup_damaged".into());
            crate::log(paths, &format!("restore: the second read failed: {e}"));
        }
    }
    let seen = std::mem::take(&mut walk.seen);
    drop(walk);
    for id in &req.apps {
        if !seen.contains(id) && !summary.failed.iter().any(|f| &f.id == id) {
            summary.failed.push(AppFailure { id: id.clone(), error: "restore_app_not_in_backup".into() });
        }
    }
    walked.map_err(|e| fail("verify", if e.code == "backup_restore_failed" { "backup_damaged" } else { e.code }))
}

fn restore_ca(paths: &Paths, host: &dyn Host, v: &Verified, fresh: bool, summary: &mut Summary) {
    let read = |p: PathBuf| std::fs::read_to_string(p).ok();
    let current = read(paths.ca_cert()).zip(read(paths.ca_key()));
    let action = ca_action(current.as_ref().map(|(c, k)| (c.as_str(), k.as_str())), v.ca.as_ref().map(|(c, k)| (c.as_str(), k.as_str())), fresh);
    let Some((crt, key)) = &v.ca else { return };
    match action {
        CaAction::Nothing => {}
        CaAction::Keep => {
            crate::log(paths, "restore: this Konak's root CA differs from the backup's and is in use; kept");
            summary.notes.push("restore_ca_kept");
        }
        CaAction::Install if !crate::ca::pair_matches(crt, key) => {
            crate::log(paths, "restore: the backup's root CA certificate and key do not match; not restored");
            summary.notes.push("restore_ca_invalid");
        }
        CaAction::Install => match crate::ca::install_root(paths, crt, key) {
            Ok(()) => {
                summary.notes.push("restore_ca_restored");
                host.ca_changed();
            }
            Err(e) => {
                crate::log(paths, &format!("restore: the root CA was not restored: {e}"));
                summary.notes.push("restore_ca_failed");
            }
        },
    }
}

#[derive(PartialEq, Clone, Debug)]
enum Stage {
    Folder,
    Volume(String),
}

/// One app's restore in progress.
struct Turn {
    am: AppManifest,
    name: String,
    renamed_from: Option<String>,
    _reservation: apps::Reservation,
    old: Option<InstalledApp>,
    had_old: bool,
    sink: Option<(Stage, Box<dyn Sink>)>,
    next_index: u32,
    folder_done: bool,
    volumes_done: Vec<String>,
}

enum Current {
    None,
    /// Entries of an app not chosen, or one that failed: read past.
    Skip(String),
    Turn(Box<Turn>),
}

struct Walk<'a> {
    ctx: &'a Ctx<'a>,
    target: &'a dyn Target,
    req: &'a Request,
    manifest: &'a Manifest,
    count: &'a Cell<u64>,
    current: Current,
    seen: Vec<String>,
    summary: &'a mut Summary,
}

impl Walk<'_> {
    fn current_id(&self) -> Option<&str> {
        match &self.current {
            Current::None => None,
            Current::Skip(id) => Some(id),
            Current::Turn(t) => Some(&t.am.id),
        }
    }

    /// One archive entry. An error is the archive's (it cannot be read further); an app's own
    /// failure is recorded and the walk goes on.
    fn entry(&mut self, e: &Entry, r: &mut dyn Read) -> Result<(), String> {
        let mut t = Tracked { inner: r, count: self.count, error: None };
        match e {
            Entry::AppFolder { app, index } | Entry::AppVolume { app, index, .. } => {
                if self.current_id() != Some(app.as_str()) {
                    self.end_current();
                    self.current = self.begin(app);
                }
                if matches!(self.current, Current::Turn(_)) {
                    let volume = if let Entry::AppVolume { volume, .. } = e { Some(volume.as_str()) } else { None };
                    if let Err(err) = self.feed(volume, *index, &mut t) {
                        if let Some(read) = t.error.take() {
                            return Err(read);
                        }
                        self.fail_current(err);
                    }
                }
            }
            Entry::Photos(rel) | Entry::Media(rel) => {
                self.end_current();
                let photos = matches!(e, Entry::Photos(_));
                let (wanted, root, step) = if photos { (self.req.photos, &self.ctx.paths.photos, "photos") } else { (self.req.media, &self.ctx.paths.media, "media") };
                if wanted {
                    let result = merge_file(root, rel, &mut t);
                    if let Some(read) = t.error.take() {
                        return Err(read);
                    }
                    let slot = if photos { &mut self.summary.photos } else { &mut self.summary.media };
                    let f = slot.get_or_insert_with(FolderResult::default);
                    match result {
                        Ok(Merged::Written) => f.written += 1,
                        Ok(Merged::Identical) => f.identical += 1,
                        Ok(Merged::KeptBoth) => f.kept_both += 1,
                        Err(err) => {
                            f.failed += 1;
                            if f.error.is_none() {
                                crate::log(self.ctx.paths, &format!("restore {step}: {err}"));
                                f.error = Some(err);
                            }
                        }
                    }
                    self.ctx.bytes(step, None, self.count.get());
                }
            }
            _ => self.end_current(),
        }
        io::copy(&mut t, &mut io::sink()).map(|_| ()).map_err(|e| e.to_string())
    }

    /// Starts app `id`'s turn: checks, name, and its old data set aside (with `replace`).
    fn begin(&mut self, id: &str) -> Current {
        self.seen.push(id.to_string());
        let skip = Current::Skip(id.to_string());
        if !self.req.apps.iter().any(|a| a == id) {
            return skip;
        }
        let Some(am) = self.manifest.apps.iter().find(|a| a.id == id).cloned() else {
            self.record_failure(id, "backup_damaged: the app is not in the manifest".into());
            return skip;
        };
        if catalog::get(id).is_none() {
            self.record_failure(id, "unknown_app".into());
            return skip;
        }
        if !self.ctx.host.try_busy(id) {
            self.record_failure(id, "busy".into());
            return skip;
        }
        self.ctx.step("app", Some(id), self.count.get());
        let prepared = (|| {
            let state = State::try_load(self.ctx.paths)?;
            let old = state.apps.iter().find(|a| a.id == id).cloned();
            let volumes: Vec<&str> = am.volumes.iter().map(|v| v.podman_name.as_str()).collect();
            let probe = self.target.probe(id, &volumes)?;
            if probe.aside {
                return Err("restore_aside_exists".to_string());
            }
            let had_old = old.is_some() || probe.folder || probe.volumes;
            if had_old && !self.req.replace {
                return Err("restore_app_exists".to_string());
            }
            let (name, reservation) = pick_name(self.ctx.paths, &state, &am, old.as_ref().map(|a| a.name.as_str()), self.target)?;
            Ok((old, had_old, name, reservation))
        })();
        let (old, had_old, name, reservation) = match prepared {
            Ok(p) => p,
            Err(e) => {
                self.ctx.host.release(id, &Err(e.clone()));
                self.record_failure(id, e);
                return skip;
            }
        };
        let renamed_from = (name != am.name).then(|| am.name.clone());
        if let Some(from) = &renamed_from {
            crate::log(self.ctx.paths, &format!("restore {id}: {from}.local is taken; restoring it as {name}.local"));
        }
        self.current = Current::Turn(Box::new(Turn { am, name, renamed_from, _reservation: reservation, old, had_old, sink: None, next_index: 0, folder_done: false, volumes_done: vec![] }));
        if had_old {
            if let Err(e) = self.target.set_aside(id) {
                self.fail_current(e);
                return std::mem::replace(&mut self.current, Current::None);
            }
        }
        std::mem::replace(&mut self.current, Current::None)
    }

    /// Streams one chunk into the app's folder or one of its volumes.
    fn feed(&mut self, volume: Option<&str>, index: u32, data: &mut Tracked) -> Result<(), String> {
        let Current::Turn(t) = &mut self.current else { return Ok(()) };
        let id = t.am.id.clone();
        let stage = match volume {
            None => Stage::Folder,
            Some(v) => Stage::Volume(v.to_string()),
        };
        if t.sink.as_ref().map(|(s, _)| s) != Some(&stage) {
            if let Some((s, sink)) = t.sink.take() {
                sink.finish().map_err(|e| format!("{} of {id}: {e}", stage_name(&s)))?;
                match s {
                    Stage::Folder => t.folder_done = true,
                    Stage::Volume(v) => t.volumes_done.push(v),
                }
            }
            let sink = match &stage {
                Stage::Folder if t.folder_done || !t.volumes_done.is_empty() => return Err("backup_damaged: folder chunks out of order".into()),
                Stage::Folder => self.target.folder(&id)?,
                Stage::Volume(_) if !t.folder_done => return Err("backup_damaged: a volume before the folder".into()),
                Stage::Volume(v) if t.volumes_done.contains(v) => return Err("backup_damaged: volume chunks out of order".into()),
                Stage::Volume(v) => {
                    let vm = t.am.volumes.iter().find(|m| &m.name == v).ok_or_else(|| format!("backup_damaged: volume {v} is not in the manifest"))?;
                    self.target.volume(&id, vm)?
                }
            };
            t.sink = Some((stage.clone(), sink));
            t.next_index = 0;
        }
        if index != t.next_index {
            return Err("backup_damaged: a missing chunk".into());
        }
        t.next_index += 1;
        let (_, sink) = t.sink.as_mut().expect("set above");
        let ctx = self.ctx;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = read_full(data, &mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                return Ok(());
            }
            sink.write_all(&buf[..n]).map_err(|e| format!("writing {} of {id}: {e}", stage_name(&stage)))?;
            ctx.bytes("app", Some(&id), data.count.get());
        }
    }

    /// Ends the current app: an open stream is finished and a turn completed.
    fn end_current(&mut self) {
        let Current::Turn(mut t) = std::mem::replace(&mut self.current, Current::None) else { return };
        let id = t.am.id.clone();
        let result = complete(self.ctx, self.target, &mut t);
        match result {
            Ok(()) => {
                if t.had_old {
                    if let Err(e) = self.target.commit(&id) {
                        crate::log(self.ctx.paths, &format!("restore {id}: the set-aside copy was not deleted: {e}"));
                    }
                }
                self.ctx.host.release(&id, &Ok(()));
                self.ctx.host.apps_changed();
                crate::log(self.ctx.paths, &format!("restore {id}: restored as {}.local", t.name));
                self.summary.restored.push(Restored { id, name: t.name.clone(), renamed_from: t.renamed_from.clone() });
            }
            Err(e) => {
                self.current = Current::Turn(t);
                self.fail_current(e);
                self.current = Current::None;
            }
        }
    }

    /// The current app failed: what it changed is undone, its old data put back and started again.
    fn fail_current(&mut self, error: String) {
        let Current::Turn(mut t) = std::mem::replace(&mut self.current, Current::None) else { return };
        let id = t.am.id.clone();
        self.current = Current::Skip(id.clone());
        let mut error = error;
        if let Some((_, sink)) = t.sink.take() {
            let said = sink.abort();
            if !said.is_empty() {
                error = format!("{error} ({said})");
            }
        }
        if let Err(e) = self.target.roll_back(&id, t.had_old) {
            crate::log(self.ctx.paths, &format!("restore {id}: rolling back failed, the set-aside data stays in the distro: {e}"));
            error = format!("{error}; rolling back failed: {e}");
        } else if t.old.as_ref().is_some_and(|a| !a.stopped) {
            if let Err(e) = self.target.start_old(&id) {
                error = format!("{error}; the previous {id} did not start again: {e}");
            }
        }
        crate::log(self.ctx.paths, &format!("restore {id} failed: {error}"));
        self.ctx.host.release(&id, &Err(error.clone()));
        self.record_failure(&id, error);
    }

    fn record_failure(&mut self, id: &str, error: String) {
        self.summary.failed.push(AppFailure { id: id.to_string(), error });
    }
}

fn stage_name(s: &Stage) -> String {
    match s {
        Stage::Folder => "the folder".into(),
        Stage::Volume(v) => format!("volume {v}"),
    }
}

/// The app's data is all in: render, start, check, and record it.
fn complete(ctx: &Ctx, target: &dyn Target, t: &mut Turn) -> Result<(), String> {
    let id = t.am.id.clone();
    if let Some((s, sink)) = t.sink.take() {
        sink.finish().map_err(|e| format!("{} of {id}: {e}", stage_name(&s)))?;
        match s {
            Stage::Folder => t.folder_done = true,
            Stage::Volume(v) => t.volumes_done.push(v),
        }
    }
    if !t.folder_done {
        return Err("backup_damaged: the app's folder is missing".into());
    }
    if let Some(v) = t.am.volumes.iter().find(|v| !t.volumes_done.contains(&v.name)) {
        return Err(format!("backup_damaged: volume {} is missing", v.name));
    }
    target.bring_up(&id, &t.name, t.am.claimed, !t.am.stopped)?;
    let (name, claimed, stopped) = (t.name.clone(), t.am.claimed, t.am.stopped);
    apps::update_state(ctx.paths, |s| {
        s.rerender.retain(|r| *r != id);
        match s.apps.iter_mut().find(|a| a.id == id) {
            Some(a) => {
                a.name = name;
                a.claimed = claimed;
                a.stopped = stopped;
            }
            None => s.apps.push(InstalledApp { id: id.clone(), name, installed_unix: now(), claimed, stopped }),
        }
    })
}

/// The app's backed-up name, or with `-2`, `-3`, ... when that one is taken by another installed
/// app, held by another operation, or answers on the LAN. `own` is the app's current name when it
/// is being replaced: Konak itself may announce it, so it is not asked on the LAN.
fn pick_name(paths: &Paths, state: &State, am: &AppManifest, own: Option<&str>, target: &dyn Target) -> Result<(String, apps::Reservation), String> {
    let base = if catalog::valid_label(&am.name) && am.name != HOME_NAME { am.name.clone() } else { catalog::get(&am.id).map(|m| m.default_name.clone()).ok_or("unknown_app")? };
    for i in 1..=MAX_SUFFIX {
        let cand = if i == 1 { base.clone() } else { format!("{base}-{i}") };
        let held = apps::reserved_by_other(&apps::RESERVED.lock().unwrap_or_else(|e| e.into_inner()), &am.id, &cand);
        if held || apps::validate_name(state, &am.id, &cand).is_err() {
            continue;
        }
        if own == Some(cand.as_str()) || target.name_free_on_lan(&cand)? {
            if let Ok(r) = apps::reserve(paths, &am.id, &cand) {
                return Ok((cand, r));
            }
        }
    }
    Err("restore_no_free_name".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup_format::tests::{pw, TEST_LOG_N};
    use std::cell::RefCell;
    use std::collections::{BTreeMap, HashMap};
    use std::rc::Rc;

    type Data = Rc<RefCell<BTreeMap<String, Vec<u8>>>>;

    #[derive(Default)]
    struct FakeTarget {
        calls: RefCell<Vec<String>>,
        probes: HashMap<String, Probe>,
        lan_taken: Vec<String>,
        fail_up: Vec<String>,
        data: Data,
    }

    struct FakeSink {
        key: String,
        buf: Vec<u8>,
        data: Data,
    }

    impl Write for FakeSink {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.buf.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Sink for FakeSink {
        fn finish(self: Box<Self>) -> Result<(), String> {
            self.data.borrow_mut().insert(self.key, self.buf);
            Ok(())
        }
        fn abort(self: Box<Self>) -> String {
            String::new()
        }
    }

    impl FakeTarget {
        fn call(&self, c: String) {
            self.calls.borrow_mut().push(c);
        }
        fn sink(&self, key: String) -> Box<dyn Sink> {
            Box::new(FakeSink { key, buf: vec![], data: self.data.clone() })
        }
    }

    impl Target for FakeTarget {
        fn probe(&self, id: &str, _: &[&str]) -> Result<Probe, String> {
            self.call(format!("probe {id}"));
            Ok(self.probes.get(id).cloned().unwrap_or_default())
        }
        fn set_aside(&self, id: &str) -> Result<(), String> {
            self.call(format!("aside {id}"));
            Ok(())
        }
        fn roll_back(&self, id: &str, had_old: bool) -> Result<(), String> {
            self.call(format!("rollback {id} {had_old}"));
            Ok(())
        }
        fn commit(&self, id: &str) -> Result<(), String> {
            self.call(format!("commit {id}"));
            Ok(())
        }
        fn folder(&self, id: &str) -> Result<Box<dyn Sink>, String> {
            self.call(format!("folder {id}"));
            Ok(self.sink(format!("{id}/folder")))
        }
        fn volume(&self, id: &str, v: &VolumeManifest) -> Result<Box<dyn Sink>, String> {
            self.call(format!("volume {id} {}", v.podman_name));
            Ok(self.sink(format!("{id}/{}", v.podman_name)))
        }
        fn bring_up(&self, id: &str, name: &str, claimed: bool, up: bool) -> Result<(), String> {
            self.call(format!("up {id} {name} claimed={claimed} up={up}"));
            if self.fail_up.iter().any(|f| f == id) {
                return Err(format!("{id} did not answer on /health within 5 minutes"));
            }
            Ok(())
        }
        fn start_old(&self, id: &str) -> Result<(), String> {
            self.call(format!("start_old {id}"));
            Ok(())
        }
        fn name_free_on_lan(&self, name: &str) -> Result<bool, String> {
            self.call(format!("lan {name}"));
            Ok(!self.lan_taken.iter().any(|t| t == name))
        }
    }

    #[derive(Default)]
    struct FakeHost {
        events: RefCell<Vec<Progress>>,
        released: RefCell<Vec<(String, bool)>>,
        busy_refused: Vec<String>,
        ca_changes: Cell<u32>,
    }

    impl Host for FakeHost {
        fn progress(&self, p: &Progress) {
            self.events.borrow_mut().push(p.clone());
        }
        fn try_busy(&self, id: &str) -> bool {
            !self.busy_refused.iter().any(|b| b == id)
        }
        fn release(&self, id: &str, r: &Result<(), String>) {
            self.released.borrow_mut().push((id.to_string(), r.is_ok()));
        }
        fn apps_changed(&self) {}
        fn ca_changed(&self) {
            self.ca_changes.set(self.ca_changes.get() + 1);
        }
    }

    fn app(id: &str, name: &str, stopped: bool, volumes: &[&str]) -> AppManifest {
        AppManifest {
            id: id.into(),
            name: name.into(),
            claimed: true,
            stopped,
            size_bytes: 100,
            volumes: volumes.iter().map(|v| VolumeManifest { name: v.to_string(), podman_name: format!("{id}_{v}") }).collect(),
        }
    }

    fn stream(tag: &str, len: usize) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(31) ^ tag.as_bytes()[i % tag.len()]).collect()
    }

    /// A backup with the given apps (folder and volume streams from `stream`), a CA when given, and
    /// photos `(rel, content)`.
    fn write_backup(path: &Path, apps: &[AppManifest], ca: Option<(&str, &str)>, photos: &[(&str, &[u8])]) {
        let m = Manifest {
            format: fmt::FORMAT,
            konak_version: "0.1.0".into(),
            created_unix: 1,
            apps: apps.to_vec(),
            photos_bytes: (!photos.is_empty()).then_some(photos.iter().map(|p| p.1.len() as u64).sum()),
            media_bytes: None,
            konak_files: vec!["state.json".into(), "settings.json".into()],
        };
        let file = io::BufWriter::new(std::fs::File::create(path).unwrap());
        let mut w = fmt::Writer::new(file, &pw("the backup password"), &m, TEST_LOG_N).unwrap();
        for a in apps {
            w.add_stream(&format!("apps/{}/folder", a.id), stream(&a.id, 3000).as_slice(), 1, &mut |_| {}).unwrap();
            for v in &a.volumes {
                w.add_stream(&format!("apps/{}/volumes/{}", a.id, v.name), stream(&v.podman_name, 5000).as_slice(), 1, &mut |_| {}).unwrap();
            }
        }
        if let Some((crt, key)) = ca {
            w.add_bytes("konak/ca/root.crt", crt.as_bytes(), 0o600, 1).unwrap();
            w.add_bytes("konak/ca/root.key", key.as_bytes(), 0o600, 1).unwrap();
        }
        w.add_bytes("konak/state.json", b"{}", 0o600, 1).unwrap();
        w.add_bytes("konak/settings.json", b"{}", 0o600, 1).unwrap();
        for (rel, body) in photos {
            w.add_reader(&format!("photos/{rel}"), body.len() as u64, *body, 0o644, 1).unwrap();
        }
        let (mut f, _) = w.finish(1).unwrap();
        f.flush().unwrap();
    }

    fn setup(tag: &str, installed: &[(&str, &str, bool)]) -> (Paths, PathBuf) {
        let paths = crate::settings::tests::temp_paths(&format!("restore-{tag}"));
        let mut st = State::default();
        st.done = true;
        st.apps = installed.iter().map(|(id, name, stopped)| InstalledApp { id: id.to_string(), name: name.to_string(), installed_unix: 7, claimed: true, stopped: *stopped }).collect();
        st.save(&paths);
        let file = paths.home.join("b.konakbackup");
        (paths, file)
    }

    fn request(file: &Path, apps: &[&str], replace: bool, photos: bool) -> Request {
        Request { path: file.to_string_lossy().to_string(), password: "the backup password".into(), apps: apps.iter().map(|a| a.to_string()).collect(), replace, photos, media: false }
    }

    fn restore(paths: &Paths, host: &FakeHost, target: &FakeTarget, req: &Request) -> Result<Summary, Failed> {
        let guard = loop {
            // Other tests (backup's) hold the guard for a moment.
            match Guard::acquire() {
                Ok(g) => break g,
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        };
        run(paths, host, target, &guard, req)
    }

    #[test]
    fn nothing_changes_before_the_whole_file_is_verified() {
        let (paths, file) = setup("verify", &[]);
        write_backup(&file, &[app("vaultwarden", "vault-v", false, &[])], None, &[("a.jpg", b"jpeg")]);
        // A byte changed near the end: the apps come first in the file, so a restore that did not
        // verify first would already have unpacked them.
        let mut raw = std::fs::read(&file).unwrap();
        let n = raw.len();
        raw[n - 40] ^= 1;
        std::fs::write(&file, &raw).unwrap();
        let (host, target) = (FakeHost::default(), FakeTarget::default());
        let r = restore(&paths, &host, &target, &request(&file, &["vaultwarden"], true, true));
        let wrong = restore(&paths, &host, &target, &Request { password: "not the password".into(), ..request(&file, &["vaultwarden"], true, true) });
        let state = State::load(&paths);
        let photos_exist = paths.photos.exists();
        let _ = std::fs::remove_dir_all(&paths.home);
        assert_eq!(r.unwrap_err(), fail("verify", "backup_damaged"));
        assert_eq!(wrong.unwrap_err(), fail("verify", "backup_wrong_password"));
        assert!(target.calls.borrow().is_empty(), "{:?}", target.calls.borrow());
        assert!(state.apps.is_empty() && !photos_exist && host.released.borrow().is_empty());
        let last = host.events.borrow().last().cloned().unwrap();
        assert_eq!((last.state, last.step, last.error.as_deref()), ("failed", "verify", Some("backup_wrong_password")));
    }

    #[test]
    fn new_apps_are_restored_and_renamed_when_their_name_is_taken() {
        // Jellyfin is installed as "vault-n" here; "cloud-n" answers on the LAN.
        let (paths, file) = setup("rename", &[("jellyfin", "vault-n", false)]);
        let apps = [app("vaultwarden", "vault-n", false, &[]), app("nextcloud", "cloud-n", true, &["db", "nextcloud"])];
        write_backup(&file, &apps, None, &[]);
        let host = FakeHost::default();
        let target = FakeTarget { lan_taken: vec!["cloud-n".into()], ..Default::default() };
        let summary = restore(&paths, &host, &target, &request(&file, &["vaultwarden", "nextcloud"], false, false)).unwrap();
        let state = State::load(&paths);
        let _ = std::fs::remove_dir_all(&paths.home);
        assert_eq!(
            summary.restored,
            [
                Restored { id: "vaultwarden".into(), name: "vault-n-2".into(), renamed_from: Some("vault-n".into()) },
                Restored { id: "nextcloud".into(), name: "cloud-n-2".into(), renamed_from: Some("cloud-n".into()) }
            ]
        );
        assert!(summary.failed.is_empty());
        assert_eq!(summary.notes, ["restore_settings_kept"]);
        assert_eq!(
            *target.calls.borrow(),
            [
                "probe vaultwarden",
                "lan vault-n-2",
                "folder vaultwarden",
                "up vaultwarden vault-n-2 claimed=true up=true",
                "probe nextcloud",
                "lan cloud-n",
                "lan cloud-n-2",
                "folder nextcloud",
                "volume nextcloud nextcloud_db",
                "volume nextcloud nextcloud_nextcloud",
                // Stopped in the backup: rendered, not started.
                "up nextcloud cloud-n-2 claimed=true up=false",
            ]
        );
        let data = target.data.borrow();
        assert_eq!(data["vaultwarden/folder"], stream("vaultwarden", 3000));
        assert_eq!(data["nextcloud/nextcloud_db"], stream("nextcloud_db", 5000));
        let names: Vec<(&str, &str, bool)> = state.apps.iter().map(|a| (a.id.as_str(), a.name.as_str(), a.stopped)).collect();
        assert_eq!(names, [("jellyfin", "vault-n", false), ("vaultwarden", "vault-n-2", false), ("nextcloud", "cloud-n-2", true)]);
        let last = host.events.borrow().last().cloned().unwrap();
        assert_eq!((last.state, last.summary.is_some()), ("done", true));
        assert_eq!(last.done_bytes, last.total_bytes);
        assert_eq!(*host.released.borrow(), [("vaultwarden".to_string(), true), ("nextcloud".to_string(), true)]);
    }

    #[test]
    fn an_installed_app_is_refused_without_replace_and_the_others_go_on() {
        let (paths, file) = setup("noreplace", &[("vaultwarden", "vault-r", false)]);
        let apps = [app("vaultwarden", "vault-r", false, &[]), app("forgejo", "git-r", false, &[]), app("jellyfin", "films-r", false, &[])];
        write_backup(&file, &apps, None, &[]);
        let host = FakeHost::default();
        // Jellyfin is not in state.json, but its folder was kept by an earlier remove: existing data too.
        let target = FakeTarget { probes: HashMap::from([("jellyfin".to_string(), Probe { folder: true, ..Default::default() })]), ..Default::default() };
        let summary = restore(&paths, &host, &target, &request(&file, &["vaultwarden", "forgejo", "jellyfin", "immich"], false, false)).unwrap();
        let state = State::load(&paths);
        let _ = std::fs::remove_dir_all(&paths.home);
        let failed: Vec<(&str, &str)> = summary.failed.iter().map(|f| (f.id.as_str(), f.error.as_str())).collect();
        assert_eq!(failed, [("vaultwarden", "restore_app_exists"), ("jellyfin", "restore_app_exists"), ("immich", "restore_app_not_in_backup")]);
        assert_eq!(summary.restored.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["forgejo"]);
        assert!(!target.calls.borrow().iter().any(|c| c.starts_with("aside") || c == "folder vaultwarden" || c == "folder jellyfin"), "{:?}", target.calls.borrow());
        assert_eq!(state.apps.len(), 2);
        let last = host.events.borrow().last().cloned().unwrap();
        assert_eq!((last.state, last.error.as_deref()), ("failed", Some("restore_some_failed")));
    }

    #[test]
    fn a_replaced_app_is_moved_aside_and_put_back_when_the_restore_fails() {
        let (paths, file) = setup("aside", &[("vaultwarden", "vault-a", false), ("nextcloud", "cloud-a", false)]);
        let apps = [app("vaultwarden", "vault-a", false, &[]), app("nextcloud", "files-a", false, &["db"])];
        write_backup(&file, &apps, None, &[]);
        let host = FakeHost::default();
        let target = FakeTarget { fail_up: vec!["vaultwarden".into()], ..Default::default() };
        let summary = restore(&paths, &host, &target, &request(&file, &["vaultwarden", "nextcloud"], true, false)).unwrap();
        let state = State::load(&paths);
        let _ = std::fs::remove_dir_all(&paths.home);
        assert_eq!(
            *target.calls.borrow(),
            [
                // Its own name is not asked on the LAN (Konak announces it itself).
                "probe vaultwarden",
                "aside vaultwarden",
                "folder vaultwarden",
                "up vaultwarden vault-a claimed=true up=true",
                "rollback vaultwarden true",
                "start_old vaultwarden",
                "probe nextcloud",
                "lan files-a",
                "aside nextcloud",
                "folder nextcloud",
                "volume nextcloud nextcloud_db",
                "up nextcloud files-a claimed=true up=true",
                "commit nextcloud",
            ]
        );
        assert_eq!(summary.failed.len(), 1);
        assert!(summary.failed[0].error.contains("did not answer"), "{:?}", summary.failed);
        let names: Vec<(&str, &str, u64)> = state.apps.iter().map(|a| (a.id.as_str(), a.name.as_str(), a.installed_unix)).collect();
        assert_eq!(names, [("vaultwarden", "vault-a", 7), ("nextcloud", "files-a", 7)], "the failed one unchanged, the replaced one renamed");
        assert_eq!(*host.released.borrow(), [("vaultwarden".to_string(), false), ("nextcloud".to_string(), true)]);
    }

    #[test]
    fn a_busy_app_is_skipped() {
        let (paths, file) = setup("busy", &[]);
        write_backup(&file, &[app("forgejo", "git-b", false, &[])], None, &[]);
        let host = FakeHost { busy_refused: vec!["forgejo".into()], ..Default::default() };
        let target = FakeTarget::default();
        let summary = restore(&paths, &host, &target, &request(&file, &["forgejo"], false, false)).unwrap();
        let _ = std::fs::remove_dir_all(&paths.home);
        assert_eq!(summary.failed, [AppFailure { id: "forgejo".into(), error: "busy".into() }]);
        assert!(target.calls.borrow().is_empty());
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("konak-restore-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        v.sort();
        v
    }

    #[test]
    fn photos_are_merged_without_deleting_or_overwriting() {
        let root = temp_dir("merge");
        std::fs::create_dir_all(root.join("2026")).unwrap();
        std::fs::write(root.join("2026").join("same.jpg"), b"same bytes").unwrap();
        std::fs::write(root.join("edited.jpg"), b"edited here").unwrap();
        std::fs::write(root.join("longer.jpg"), b"backup bytes plus more").unwrap();
        std::fs::write(root.join("shorter.jpg"), b"backup").unwrap();
        std::fs::write(root.join("only-here.jpg"), b"x").unwrap();
        std::fs::create_dir(root.join("a-folder.jpg")).unwrap();
        let big: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
        let mut big_changed = big.clone();
        big_changed[200_000] ^= 1;
        std::fs::write(root.join("big.raw"), &big_changed).unwrap();
        let m = |rel: &str, data: &[u8]| merge_file(&root, rel, &mut &data[..]).unwrap();
        assert_eq!(m("2026/same.jpg", b"same bytes"), Merged::Identical);
        assert_eq!(m("2026/new.jpg", b"new"), Merged::Written);
        assert_eq!(m("deep/er/new.png", b"png"), Merged::Written);
        assert_eq!(m("edited.jpg", b"original"), Merged::KeptBoth);
        // A second restore of the same file finds its restored copy.
        assert_eq!(m("edited.jpg", b"original"), Merged::Identical);
        assert_eq!(m("edited.jpg", b"another version"), Merged::KeptBoth);
        assert_eq!(m("longer.jpg", b"backup bytes"), Merged::KeptBoth);
        assert_eq!(m("shorter.jpg", b"backup bytes"), Merged::KeptBoth);
        assert_eq!(m("a-folder.jpg", b"file"), Merged::KeptBoth);
        assert_eq!(m("big.raw", &big), Merged::KeptBoth);
        let read = |p: &str| std::fs::read(root.join(p)).unwrap();
        let top = names_in(&root);
        let (edited, edited2, longer, shorter, big_restored) = (read("edited (restored).jpg"), read("edited (restored 2).jpg"), read("longer (restored).jpg"), read("shorter (restored).jpg"), read("big (restored).raw"));
        let (kept, short_kept, big_kept) = (read("edited.jpg"), read("shorter.jpg"), read("big.raw"));
        let new_png = read("deep/er/new.png");
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!((edited.as_slice(), edited2.as_slice()), (&b"original"[..], &b"another version"[..]));
        assert_eq!((longer.as_slice(), shorter.as_slice()), (&b"backup bytes"[..], &b"backup bytes"[..]));
        assert_eq!((kept.as_slice(), short_kept.as_slice()), (&b"edited here"[..], &b"backup"[..]), "existing files are never overwritten");
        assert_eq!((big_restored, big_kept), (big, big_changed), "a difference after the first chunk keeps the matched prefix");
        assert_eq!(new_png, b"png");
        assert!(top.contains(&"only-here.jpg".to_string()), "nothing is deleted");
        assert!(!top.iter().any(|n| n.ends_with(".partial")), "{top:?}");
        assert_eq!(restored_name(Path::new("a/.hidden"), 1), Path::new("a/.hidden (restored)"));
        assert_eq!(restored_name(Path::new("a/x.tar.gz"), 3), Path::new("a/x.tar (restored 3).gz"));
    }

    #[test]
    fn paths_cannot_escape_the_target_folder() {
        let base = temp_dir("escape");
        let (root, outside) = (base.join("Photos"), base.join("outside"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        // A junction inside the target pointing out of it (no admin needed).
        let out = shell::run("cmd.exe", &["/c", "mklink", "/J", &root.join("link").to_string_lossy(), &outside.to_string_lossy()]);
        assert!(out.ok, "{}", out.text);
        let refused = |rel: &str| merge_file(&root, rel, &mut &b"x"[..]).err().unwrap_or_default();
        let results: Vec<String> = ["link/evil.jpg", "link/sub/evil.jpg", "NUL", "con.txt", "dir/COM1.jpg", "trailing.", "space ", "a:b", "a|b", "../x", "a/../../x"].iter().map(|r| refused(r)).collect();
        let ok = merge_file(&root, "fine/ok.jpg", &mut &b"x"[..]);
        let leaked = names_in(&outside);
        let _ = std::fs::remove_dir(root.join("link"));
        let _ = std::fs::remove_dir_all(&base);
        for r in &results {
            assert!(r.starts_with("restore_path_refused"), "{results:?}");
        }
        assert_eq!(ok, Ok(Merged::Written));
        assert!(leaked.is_empty(), "{leaked:?}");
        assert!(component_ok("IMG_0001.JPG") && component_ok("Çiçek ağacı.jpg") && component_ok("COM10.txt") && component_ok(".hidden"));
    }

    #[test]
    fn photos_are_restored_only_when_asked_into_the_configured_folder() {
        let (paths, file) = setup("photos", &[]);
        write_backup(&file, &[], None, &[("2026/a.jpg", b"aaa"), ("b.jpg", b"bbb")]);
        std::fs::create_dir_all(&paths.photos).unwrap();
        std::fs::write(paths.photos.join("b.jpg"), b"changed").unwrap();
        let (host, target) = (FakeHost::default(), FakeTarget::default());
        let skipped = restore(&paths, &host, &target, &request(&file, &[], false, false)).unwrap();
        let before = names_in(&paths.photos);
        let done = restore(&paths, &host, &target, &request(&file, &[], false, true)).unwrap();
        let a = std::fs::read(paths.photos.join("2026").join("a.jpg")).unwrap();
        let after = names_in(&paths.photos);
        let _ = std::fs::remove_dir_all(&paths.home);
        assert_eq!((skipped.photos, before), (None, vec!["b.jpg".to_string()]));
        assert_eq!(done.photos, Some(FolderResult { written: 1, identical: 0, kept_both: 1, failed: 0, error: None }));
        assert_eq!(a, b"aaa");
        assert_eq!(after, ["2026", "b (restored).jpg", "b.jpg"]);
    }

    #[test]
    fn the_ca_rule() {
        let (a, b) = (("crt-a", "key-a"), ("crt-b", "key-b"));
        assert_eq!(ca_action(None, Some(b), false), CaAction::Install);
        assert_eq!(ca_action(Some(a), Some(b), true), CaAction::Install);
        assert_eq!(ca_action(Some(a), Some(b), false), CaAction::Keep);
        assert_eq!(ca_action(Some(b), Some(b), false), CaAction::Nothing);
        assert_eq!(ca_action(Some(a), None, true), CaAction::Nothing);
    }

    #[test]
    fn a_different_ca_in_use_is_kept_and_a_fresh_install_takes_the_backups() {
        let old = crate::ca::generate_root("Konak Test Old").unwrap();
        let here = crate::ca::generate_root("Konak Test Here").unwrap();
        for (tag, installed, want_note, want) in [("ca-keep", true, "restore_ca_kept", &here), ("ca-fresh", false, "restore_ca_restored", &old)] {
            let apps: &[(&str, &str, bool)] = if installed { &[("forgejo", "git-c", false)] } else { &[] };
            let (paths, file) = setup(tag, apps);
            std::fs::create_dir_all(paths.ca_dir()).unwrap();
            std::fs::write(paths.ca_cert(), &here.cert_pem).unwrap();
            std::fs::write(paths.ca_key(), &here.key_pem).unwrap();
            std::fs::create_dir_all(paths.caddy_storage().join("pki").join("authorities").join("local")).unwrap();
            write_backup(&file, &[], Some((&old.cert_pem, &old.key_pem)), &[("p.jpg", b"p")]);
            let host = FakeHost::default();
            let summary = restore(&paths, &host, &FakeTarget::default(), &request(&file, &[], false, true)).unwrap();
            let (crt, key) = (std::fs::read_to_string(paths.ca_cert()).unwrap(), std::fs::read_to_string(paths.ca_key()).unwrap());
            let intermediate_left = paths.caddy_storage().join("pki").join("authorities").join("local").exists();
            let _ = std::fs::remove_dir_all(&paths.home);
            assert_eq!(summary.notes, [want_note, "restore_settings_kept"], "{tag}");
            assert_eq!((crt.as_str(), key.as_str()), (want.cert_pem.as_str(), want.key_pem.as_str()), "{tag}");
            assert_eq!(intermediate_left, installed, "{tag}: Caddy's intermediate under the old root is cleared on a swap");
            assert_eq!(host.ca_changes.get(), u32::from(!installed));
        }
        // A pair that does not belong together is never installed.
        assert!(!crate::ca::pair_matches(&old.cert_pem, &here.key_pem));
        assert!(crate::ca::pair_matches(&old.cert_pem, &old.key_pem));
    }

    #[test]
    fn probe_output_is_parsed() {
        assert_eq!(parse_probe("folder 1\naside 0\nvolumes 0\nend\n").unwrap(), Probe { folder: true, aside: false, volumes: false });
        assert_eq!(parse_probe("folder 0\naside 1\nvolumes 2\nend\n").unwrap(), Probe { folder: false, aside: true, volumes: true });
        assert!(parse_probe("folder 0\naside 0\nvolumes 0\nnamed nextcloud_db\nend\n").unwrap().volumes);
        assert!(parse_probe("folder 0\n").is_err());
    }
}
