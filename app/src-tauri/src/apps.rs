//! Installed apps and their operations, shared by the UI commands and the MCP server. Functions take
//! plain `&Paths` and never need Tauri. The supervisor follows changes on its own (it re-reads
//! `state.json` every tick); callers that run one can call `Supervisor::tick` to apply them at once.
//!
//! Errors that the UI can explain are short codes (`invalid_name`, `name_in_use`, ...; see the
//! `errors.*` locale keys). Other errors are technical text, already passed through the redactor.

use crate::catalog::{self, Manifest};
use crate::installer::{now, InstalledApp, State};
use crate::paths::Paths;
use crate::redact::Redactor;
use crate::services::{active_ports, lan_ip, render_ports, WebPorts, HOME_NAME};
use crate::shell::{self, tail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

const INSTALL_SH: &str = include_str!("../scripts/app-install.sh");
const REMOVE_SH: &str = include_str!("../scripts/app-remove.sh");
pub const MAX_LOG_LINES: u32 = 1000;

/// Serializes read-modify-write of `state.json` between app operations in this process.
static STATE_LOCK: Mutex<()> = Mutex::new(());

/// A `state.json` that cannot be read is never overwritten: the change fails with `state_unreadable`.
pub(crate) fn update_state(paths: &Paths, f: impl FnOnce(&mut State)) -> Result<(), String> {
    let _guard = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut s = State::try_load(paths)?;
    f(&mut s);
    s.try_save(paths).map_err(|e| format!("cannot save state.json: {e}"))
}

#[derive(Serialize, Clone, Debug)]
pub struct Container {
    pub name: String,
    /// Podman state: running, exited, created, paused, ...
    pub state: String,
    /// healthy, unhealthy or starting, when the image defines a health check.
    pub health: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct AppInfo {
    pub id: String,
    pub name: String,
    /// The `.local` URL on the web ports Caddy serves on (with the port on a fallback pair); served
    /// on the LAN only once the app is claimed.
    pub url: String,
    /// `http://127.0.0.1:<port>`, reachable from this PC only. Used to finish an unclaimed app's
    /// first-run setup (a browser secure context, so Vaultwarden's web vault works there too).
    pub local_url: String,
    pub https: bool,
    pub port: u16,
    pub installed_unix: u64,
    pub claimed: bool,
    /// Stopped by the user; it stays stopped across distro restarts and reboots until started.
    pub stopped: bool,
    /// running | starting | unhealthy | stopped | missing (no containers) | unknown (distro unreadable)
    pub status: &'static str,
    pub containers: Vec<Container>,
}

/// The fields of `podman ps -a --format json` that Konak reads.
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PsEntry {
    #[serde(default)]
    names: Vec<String>,
    #[serde(default)]
    state: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    labels: Option<HashMap<String, String>>,
}

/// Containers grouped by compose project (= app id), from `podman ps -a --format json` output.
pub fn parse_ps(json: &str) -> Result<HashMap<String, Vec<Container>>, String> {
    let entries: Vec<PsEntry> = serde_json::from_str(json.trim()).map_err(|e| format!("podman ps output unreadable: {e}"))?;
    Ok(group_ps(entries))
}

fn group_ps(entries: Vec<PsEntry>) -> HashMap<String, Vec<Container>> {
    let mut by_project: HashMap<String, Vec<Container>> = HashMap::new();
    for e in entries {
        let Some(project) = e.labels.as_ref().and_then(|l| l.get("com.docker.compose.project")) else { continue };
        let health = ["unhealthy", "healthy", "starting"].into_iter().find(|h| e.status.contains(&format!("({h})"))).map(String::from);
        by_project.entry(project.clone()).or_default().push(Container { name: e.names.join(","), state: e.state, health });
    }
    by_project
}

pub fn status_of(containers: &[Container]) -> &'static str {
    let health = |h: &str| containers.iter().any(|c| c.health.as_deref() == Some(h));
    if containers.is_empty() {
        "missing"
    } else if health("unhealthy") {
        "unhealthy"
    } else if containers.iter().any(|c| c.state != "running") {
        "stopped"
    } else if health("starting") {
        "starting"
    } else {
        "running"
    }
}

/// CPU and memory of one app: its running containers summed.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct AppStats {
    pub id: String,
    pub cpu_percent: f64,
    pub mem_bytes: u64,
}

/// One row of `podman stats --format json`. Podman 5 prints strings ("0.17%", "4.76MB / 67.3GB");
/// other versions print numbers (percent, bytes) under the Go field names.
#[derive(Deserialize)]
struct StatsEntry {
    #[serde(default, alias = "Name")]
    name: String,
    #[serde(default, alias = "CPU")]
    cpu_percent: serde_json::Value,
    #[serde(default, alias = "MemUsage")]
    mem_usage: serde_json::Value,
}

/// A size as podman or docker prints it: `4.76MB`, `512kB`, `1.5GiB`, `0B`.
fn parse_size(s: &str) -> Option<f64> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let n: f64 = s[..split].parse().ok()?;
    let unit = match s[split..].trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "kb" | "k" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some(n * unit)
}

fn cpu_of(v: &serde_json::Value) -> f64 {
    match v {
        serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0),
        serde_json::Value::String(s) => s.trim().trim_end_matches('%').trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Used memory in bytes: a number, or the part before ` / ` of `"used / limit"`.
fn mem_of(v: &serde_json::Value) -> u64 {
    match v {
        serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0) as u64,
        serde_json::Value::String(s) => parse_size(s.split('/').next().unwrap_or("")).unwrap_or(0.0).round() as u64,
        _ => 0,
    }
}

/// The first JSON value of `text`; anything after it (stderr warnings appended by `distro_script`)
/// is ignored. Empty text is `None`.
fn first_json<T: serde::de::DeserializeOwned>(text: &str) -> Result<Option<T>, String> {
    serde_json::Deserializer::from_str(text).into_iter::<T>().next().transpose().map_err(|e| e.to_string())
}

/// Sums `podman stats --format json` rows per compose project, using the container names of
/// `parse_ps` output to find each row's project. Rows of containers outside any project are left
/// out. Sorted by app id.
pub fn aggregate_stats(stats_json: &str, ps: &HashMap<String, Vec<Container>>) -> Result<Vec<AppStats>, String> {
    let rows: Vec<StatsEntry> = first_json(stats_json).map_err(|e| format!("podman stats output unreadable: {e}"))?.unwrap_or_default();
    let project_of: HashMap<&str, &str> = ps.iter().flat_map(|(p, cs)| cs.iter().flat_map(|c| c.name.split(',')).map(move |n| (n, p.as_str()))).collect();
    let mut sums: HashMap<&str, AppStats> = HashMap::new();
    for r in &rows {
        let Some(&project) = project_of.get(r.name.as_str()) else { continue };
        let s = sums.entry(project).or_insert_with(|| AppStats { id: project.to_string(), cpu_percent: 0.0, mem_bytes: 0 });
        s.cpu_percent += cpu_of(&r.cpu_percent);
        s.mem_bytes += mem_of(&r.mem_usage);
    }
    let mut out: Vec<AppStats> = sums.into_values().collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// `podman stats` is skipped when nothing runs, so an empty engine is never an error.
const STATS_SH: &str = "set -e\nif [ -n \"$(podman ps -q)\" ]; then podman stats --no-stream --format json; fi\necho '## ps'\npodman ps --format json\n";

/// CPU and memory per installed app, from one `podman stats` call. Empty when the distro is not
/// running: it is never started for this.
pub fn stats(paths: &Paths) -> Result<Vec<AppStats>, String> {
    if !crate::diag::distro(paths).running {
        return Ok(vec![]);
    }
    let out = shell::distro_script(&paths.distro, STATS_SH, &[]);
    if !out.ok {
        return Err(format!("podman stats failed: {}", tail(&out.text, 3)));
    }
    let (stats, ps) = out.text.split_once("## ps").ok_or("podman stats output unreadable")?;
    let entries: Vec<PsEntry> = first_json(ps).map_err(|e| format!("podman ps output unreadable: {e}"))?.unwrap_or_default();
    let ps = group_ps(entries);
    let installed = State::load(paths).apps;
    Ok(aggregate_stats(stats, &ps)?.into_iter().filter(|s| installed.iter().any(|a| a.id == s.id)).collect())
}

fn podman_ps(paths: &Paths) -> Result<HashMap<String, Vec<Container>>, String> {
    let out = shell::run("wsl.exe", &["-d", &paths.distro, "-u", "root", "--", "podman", "ps", "-a", "--format", "json"]);
    if !out.ok {
        return Err(format!("podman ps failed: {}", tail(&out.text, 3)));
    }
    parse_ps(&out.text)
}

/// The app's `.local` URL on `ports`: with the port on a fallback pair, no port on 80/443.
pub fn url(m: &Manifest, name: &str, ports: WebPorts) -> String {
    ports.url(m.https, name)
}

/// Installed apps with live container status. When the distro cannot be read, status is `unknown`.
pub fn list(paths: &Paths) -> Vec<AppInfo> {
    let state = State::load(paths);
    let ports = active_ports(paths);
    let ps = podman_ps(paths).map_err(|e| crate::log(paths, &e)).ok();
    state
        .apps
        .iter()
        .filter_map(|a| {
            let m = catalog::get(&a.id)?;
            let containers = ps.as_ref().map(|p| p.get(&a.id).cloned().unwrap_or_default());
            // A user-stopped app has no containers (`stop`), which would otherwise read as missing.
            let status = match containers.as_deref() {
                Some(c) if a.stopped && !c.iter().any(|c| c.state == "running") => "stopped",
                Some(c) => status_of(c),
                None => "unknown",
            };
            Some(AppInfo {
                id: a.id.clone(),
                name: a.name.clone(),
                url: url(m, &a.name, ports),
                local_url: format!("http://127.0.0.1:{}", m.port),
                https: m.https,
                port: m.port,
                installed_unix: a.installed_unix,
                claimed: a.claimed,
                stopped: a.stopped,
                status,
                containers: containers.unwrap_or_default(),
            })
        })
        .collect()
}

/// Local checks for a new name of app `id`: syntax, reserved, unique among installed apps.
pub fn validate_name(state: &State, id: &str, name: &str) -> Result<(), String> {
    if !catalog::valid_label(name) {
        return Err("invalid_name".into());
    }
    if name == HOME_NAME {
        return Err("name_reserved".into());
    }
    if state.apps.iter().any(|a| a.id != id && a.name == name) {
        return Err("name_in_use".into());
    }
    Ok(())
}

/// Asks the LAN whether another device already answers to `<name>.local`.
pub fn name_free_on_lan(paths: &Paths, name: &str) -> Result<bool, String> {
    let ip = lan_ip().ok_or("no_lan_ip")?;
    let out = shell::run(&paths.mdns().to_string_lossy(), &[&ip, "1", name, "--check-only"]);
    crate::log(paths, &format!("name check {name}: {}", Redactor::new(vec![]).apply(&tail(&out.text, 1))));
    Ok(out.ok)
}

/// Names held by install and rename operations in progress (app id, name), so two of them cannot
/// pick the same name before either one is saved in `state.json`.
pub(crate) static RESERVED: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Holds a reserved name until the operation ends.
pub(crate) struct Reservation(String);

impl Drop for Reservation {
    fn drop(&mut self) {
        RESERVED.lock().unwrap_or_else(|e| e.into_inner()).retain(|(id, _)| *id != self.0);
    }
}

pub(crate) fn reserved_by_other(reserved: &[(String, String)], id: &str, name: &str) -> bool {
    reserved.iter().any(|(i, n)| i != id && n == name)
}

/// Checks `name` for app `id` against the saved apps and the names other operations hold, and
/// reserves it, all under the state lock.
pub(crate) fn reserve(paths: &Paths, id: &str, name: &str) -> Result<Reservation, String> {
    let _guard = STATE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    validate_name(&State::try_load(paths)?, id, name)?;
    let mut reserved = RESERVED.lock().unwrap_or_else(|e| e.into_inner());
    if reserved_by_other(&reserved, id, name) {
        return Err("name_in_use".into());
    }
    reserved.retain(|(i, _)| i != id);
    reserved.push((id.to_string(), name.to_string()));
    Ok(Reservation(id.to_string()))
}

/// First free name from `base`, `base-2`, ... `base-5`, reserved for `id`.
fn pick_name(paths: &Paths, state: &State, id: &str, base: &str) -> Result<(String, Reservation), String> {
    for i in 1..=5 {
        let name = if i == 1 { base.to_string() } else { format!("{base}-{i}") };
        let held = reserved_by_other(&RESERVED.lock().unwrap_or_else(|e| e.into_inner()), id, &name);
        if validate_name(state, id, &name).is_ok() && !held && name_free_on_lan(paths, &name)? {
            // Another operation may have taken it during the LAN check; then try the next one.
            if let Ok(r) = reserve(paths, id, &name) {
                return Ok((name, r));
            }
        }
    }
    Err(format!("names {base} to {base}-5 are all taken on this network"))
}

fn b64(s: &str) -> String {
    if s.is_empty() {
        "-".into()
    } else {
        base64::engine::general_purpose::STANDARD.encode(s)
    }
}

/// Where Caddy's requests come from, as the apps see them: Caddy connects to 127.0.0.1:<port> on
/// Windows, WSL relays that to the distro's loopback, and Podman's port forward then masquerades it
/// to the gateway of the app's compose network. Compose networks get /24s from Konak's own subnet
/// pool (`default_subnet_pools` in engine.sh, kept off Podman's default 10.89.0.0/16 so it never
/// collides with another Podman distro in the shared WSL network namespace); the exact /24 varies,
/// so the pool is the narrowest stable choice. Apps trust X-Forwarded-For only from here, and Caddy
/// overwrites that header with the real client address.
pub const PROXY_NET: &str = "10.234.0.0/16";

/// Values written to the app's `.env` by Konak (secrets and mounts are added by the script).
/// `claimed_env` is added only once the app is claimed. `ports` are the web ports Caddy serves on:
/// `KONAK_URL` and `KONAK_HOSTPORT` carry the port on the fallback and drop it on 80/443, while
/// `KONAK_HOST` is always the bare name (trusted domains, Forgejo's DOMAIN).
pub fn env_lines(m: &Manifest, name: &str, claimed: bool, ports: WebPorts) -> String {
    let (host, url) = (format!("{name}.local"), url(m, name, ports));
    let hostport = format!("{host}{}", ports.suffix(m.https));
    let mut lines = vec![
        format!("KONAK_HOST={host}"),
        format!("KONAK_HOSTPORT={hostport}"),
        format!("KONAK_URL={url}"),
        format!("KONAK_PORT={}", m.port),
        format!("KONAK_PROXY_NET={PROXY_NET}"),
    ];
    let claimed_env = m.claimed_env.iter().filter(|_| claimed);
    for (k, v) in m.env.iter().chain(claimed_env) {
        let v = v.replace("${KONAK_HOSTPORT}", &hostport).replace("${KONAK_HOST}", &host).replace("${KONAK_URL}", &url).replace("${KONAK_PROXY_NET}", PROXY_NET);
        lines.push(format!("{k}={v}"));
    }
    lines.join("\n")
}

fn redacted(paths: &Paths, text: &str) -> String {
    Redactor::load(paths).apply(text)
}

/// Renders the app under `name` for the web `ports` and, with `up`, (re)creates and starts it; with
/// `hook`, runs it after the app is healthy. Without `up` only the files are written (a stopped app
/// stays stopped) and `hook` is not run.
fn run_install(paths: &Paths, m: &Manifest, name: &str, claimed: bool, hook: Option<&str>, ports: WebPorts, up: bool) -> Result<(), String> {
    let mut mounts = String::new();
    for (var, kind) in &m.mounts {
        let dir = if kind == "photos" { &paths.photos } else { &paths.media };
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        mounts += &format!("{var}\t{}\n", dir.display());
    }
    let port = m.port.to_string();
    let secrets = if m.secrets.is_empty() { "-".to_string() } else { m.secrets.join(" ") };
    let hook = if up { hook.unwrap_or("") } else { "" };
    let mode = if up { "up" } else { "render" };
    let args = [&m.id, port.as_str(), &m.health_path, &b64(m.compose), &b64(&env_lines(m, name, claimed, ports)), &secrets, &b64(&mounts), &b64(hook), mode];
    let out = shell::distro_script(&paths.distro, INSTALL_SH, &args);
    crate::log(paths, &format!("install {} as {name}: {}", m.id, redacted(paths, &tail(&out.text, 2))));
    if !out.ok {
        return Err(redacted(paths, &format!("install of {} failed: {}", m.id, tail(&out.text, 6))));
    }
    if up {
        windows_side_check(&m.id, m.port, PORT_WINDOW)?;
        crate::log(paths, &format!("install {}: 127.0.0.1:{} answers from Windows", m.id, m.port));
    }
    Ok(())
}

/// Renders a restored app (its folder and volumes are in place) under `name` for the web ports Caddy
/// serves on now, so the settings that embed the app's own address follow this PC. With `up` it is
/// started with the install's health checks and the `post_rename` hook, which points settings the
/// app stored itself (Nextcloud's trusted domain) at `name`; without, only the files are written.
pub(crate) fn render_restored(paths: &Paths, id: &str, name: &str, claimed: bool, up: bool) -> Result<(), String> {
    let m = manifest(id)?;
    run_install(paths, m, name, claimed, m.post_rename.as_deref(), render_ports(paths)?, up)
}

/// How long the published port may take to answer from Windows once the app itself answered.
const PORT_WINDOW: std::time::Duration = std::time::Duration::from_secs(30);

/// The install script's health probe goes to the container's own address, which proves the app is
/// up but not that Caddy can reach it. Caddy connects to `127.0.0.1:<port>` on Windows, which reaches
/// the published port through WSL (localhost forwarding in NAT mode, the mirrored loopback in
/// mirrored mode) and netavark's port forward; a stale forward rule or a FORWARD DROP set by another
/// distro breaks only that leg. So the same path is probed from here, for up to `window`.
fn windows_side_check(id: &str, port: u16, window: std::time::Duration) -> Result<(), String> {
    let deadline = std::time::Instant::now() + window;
    loop {
        if crate::diag::port_answers(port, std::time::Duration::from_secs(3)) {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "{id} runs, but its published port 127.0.0.1:{port} does not answer from Windows, so Caddy cannot reach it (stale port forward rules, or another WSL distro's FORWARD policy; run diagnostics)"
            ));
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// Whether an install or render error says the app was started but did not pass its health checks:
/// app-install.sh's probe ("did not answer on <path> within 5 minutes") or `windows_side_check`.
pub(crate) fn health_failed(error: &str) -> bool {
    error.contains(" did not answer on ") || error.contains("does not answer from Windows")
}

fn manifest(id: &str) -> Result<&'static Manifest, String> {
    catalog::get(id).ok_or_else(|| "unknown_app".into())
}

fn installed(paths: &Paths, id: &str) -> Result<InstalledApp, String> {
    State::try_load(paths)?.apps.into_iter().find(|a| a.id == id).ok_or_else(|| "not_installed".into())
}

/// Installs an app, or re-renders and restarts an installed one under its current name (repair).
pub fn install(paths: &Paths, id: &str) -> Result<InstalledApp, String> {
    let m = manifest(id)?;
    let state = State::try_load(paths)?;
    let (name, claimed, reservation) = match state.apps.iter().find(|a| a.id == id) {
        Some(a) => (a.name.clone(), a.claimed, None),
        None => {
            let (name, r) = pick_name(paths, &state, id, &m.default_name)?;
            (name, false, Some(r))
        }
    };
    let repair = reservation.is_none();
    if let Err(e) = run_install(paths, m, &name, claimed, None, render_ports(paths)?, true) {
        if !repair {
            // The containers may already run, holding the port and RAM, while the panel shows the app
            // as not installed. Stop them; the data stays, so a retry reuses its secrets.
            let out = shell::distro_script(&paths.distro, REMOVE_SH, &[id, "keep"]);
            crate::log(paths, &format!("install of {id} failed; containers removed, data kept: {}", redacted(paths, &tail(&out.text, 1))));
        }
        return Err(e);
    }
    let mut result = None;
    update_state(paths, |s| {
        s.rerender.retain(|r| r != id);
        result = match s.apps.iter_mut().find(|a| a.id == id) {
            // `up -d` has started it, so a repaired app is no longer stopped.
            Some(a) => {
                a.stopped = false;
                Some(a.clone())
            }
            // Removed while it was being repaired: a repair never brings an app back.
            None if repair => None,
            None => {
                // New apps stay off the LAN until the user has created the admin account (`claim`).
                let a = InstalledApp { id: id.into(), name: name.clone(), installed_unix: now(), claimed: false, stopped: false };
                s.apps.push(a.clone());
                Some(a)
            }
        };
    })?;
    result.ok_or_else(|| "not_installed".into())
}

/// Removes the app's containers; `delete_data` also deletes its data in the distro. Windows folders
/// (media, photos) are never deleted.
pub fn remove(paths: &Paths, id: &str, delete_data: bool) -> Result<(), String> {
    manifest(id)?;
    let mode = if delete_data { "--delete-data" } else { "keep" };
    let out = shell::distro_script(&paths.distro, REMOVE_SH, &[id, mode]);
    crate::log(paths, &format!("remove {id} ({mode}): {}", redacted(paths, &tail(&out.text, 2))));
    if !out.ok {
        return Err(redacted(paths, &format!("remove of {id} failed: {}", tail(&out.text, 6))));
    }
    update_state(paths, |s| {
        s.apps.retain(|a| a.id != id);
        s.rerender.retain(|r| r != id);
    })?;
    Ok(())
}

/// Renames an installed app: local checks, LAN conflict check, then re-renders its env (and runs the
/// manifest's `post_rename` hook). On failure the old name is rendered back.
pub fn rename(paths: &Paths, id: &str, new_name: &str) -> Result<(), String> {
    let m = manifest(id)?;
    let app = installed(paths, id)?;
    validate_name(&State::try_load(paths)?, id, new_name)?;
    if app.name == new_name {
        return Ok(());
    }
    let _reservation = reserve(paths, id, new_name)?;
    if !name_free_on_lan(paths, new_name)? {
        return Err("name_taken_on_lan".into());
    }
    let ports = render_ports(paths)?;
    if let Err(e) = run_install(paths, m, new_name, app.claimed, m.post_rename.as_deref(), ports, true) {
        if let Err(back) = run_install(paths, m, &app.name, app.claimed, m.post_rename.as_deref(), ports, true) {
            // The containers may still carry the new name: render the saved one again at next start.
            crate::log(paths, &format!("rename of {id}: restoring {} failed: {back}", app.name));
            let _ = update_state(paths, |s| {
                if !s.rerender.iter().any(|r| r == id) {
                    s.rerender.push(id.to_string());
                }
            });
            return Err(format!("{e} (could not restore the old name)"));
        }
        return Err(e);
    }
    update_state(paths, |s| {
        if let Some(a) = s.apps.iter_mut().find(|a| a.id == id) {
            a.name = new_name.to_string();
            // The re-render (`up -d`) has started it.
            a.stopped = false;
        }
    })?;
    Ok(())
}

/// Marks an app as claimed (its admin account exists), which gives it its `.local` name and Caddy
/// route at the supervisor's next tick. The app is first re-rendered with its `claimed_env` (e.g.
/// registration closed) and is marked claimed only when that succeeds, so it never reaches the LAN
/// with its pre-claim settings.
///
/// A web-port switch during the render (which can take minutes) re-renders only claimed apps, so it
/// skips this one; the claim then renders it again for the ports Caddy serves on by now.
pub fn claim(paths: &Paths, id: &str) -> Result<(), String> {
    let m = manifest(id)?;
    let app = installed(paths, id)?;
    // The first render runs no hook; a follow-up for new ports is a re-render like a port switch's.
    claim_with(paths, id, &|ports, again| run_install(paths, m, &app.name, true, if again { m.post_rename.as_deref() } else { None }, ports, true))
}

/// `claim` with the render passed in (`again` is false for the first render), so the port-switch
/// handling can be tested without a distro.
fn claim_with(paths: &Paths, id: &str, render: &dyn Fn(WebPorts, bool) -> Result<(), String>) -> Result<(), String> {
    let ports = render_ports(paths)?;
    render(ports, false)?;
    update_state(paths, |s| {
        if let Some(a) = s.apps.iter_mut().find(|a| a.id == id) {
            a.claimed = true;
            // The re-render (`up -d`) has started it.
            a.stopped = false;
        }
    })?;
    let now = match render_ports(paths) {
        Ok(now) if now == ports => return Ok(()),
        Ok(now) => now,
        Err(e) => return claim_follow_up_failed(paths, id, ports, &e),
    };
    crate::log(paths, &format!("claim of {id}: the web ports changed to {}/{} during the render; rendering it again", now.http, now.https));
    match render(now, true) {
        Ok(()) => Ok(()),
        Err(e) => claim_follow_up_failed(paths, id, ports, &e),
    }
}

/// The claim stands (the app answers, with its claimed settings), but carries the settings of
/// `rendered_for`. Recording those ports as the apps' rendered ports makes the supervisor re-render
/// every claimed app, this one included, and retry that until it succeeds.
fn claim_follow_up_failed(paths: &Paths, id: &str, rendered_for: WebPorts, e: &str) -> Result<(), String> {
    crate::log(paths, &format!("claim of {id}: rendered for ports {}/{}, the supervisor re-renders it: {e}", rendered_for.http, rendered_for.https));
    update_state(paths, |s| s.rendered_ports = rendered_for)
}

/// Re-renders every claimed app for the web ports Caddy now serves on, so the settings that embed
/// the app's own address (Nextcloud's OVERWRITEHOST and OVERWRITECLIURL, Vaultwarden's DOMAIN,
/// Forgejo's ROOT_URL) carry the port on the fallback and drop it on 80/443. The same render as a
/// rename, `post_rename` hook included: a running app's changed containers are recreated. A stopped
/// app only gets its files rewritten and stays stopped; `start` brings it up with them.
///
/// Idempotent: nothing happens when the apps were last rendered for `ports`, and `ports` is
/// recorded only once every app succeeded, so a failure is retried as a whole. Unclaimed apps are
/// left alone; `claim` renders them for the ports of that moment. `on_app` hears each app's result.
pub fn rerender_for_ports(paths: &Paths, ports: WebPorts, on_app: &dyn Fn(&str, &Result<(), String>)) -> Result<(), String> {
    let state = State::try_load(paths)?;
    if state.rendered_ports == ports {
        return Ok(());
    }
    let mut failed = vec![];
    for a in state.apps.iter().filter(|a| a.claimed) {
        let Some(m) = catalog::get(&a.id) else { continue };
        let result = run_install(paths, m, &a.name, true, m.post_rename.as_deref(), ports, !a.stopped);
        on_app(&a.id, &result);
        if let Err(e) = result {
            failed.push(e);
        }
    }
    if !failed.is_empty() {
        return Err(failed.join("; "));
    }
    update_state(paths, |s| s.rendered_ports = ports)
}

/// Runs a docker-compose command for an installed app in its directory.
pub(crate) fn compose(paths: &Paths, id: &str, args: &str) -> shell::Out {
    shell::distro_script(&paths.distro, &compose_script(args), &[id])
}

/// The script behind `compose`: only the compose.yml Konak rendered (`-f`), never a compose.yaml or
/// compose.override.yml next to it, nor a file the environment names.
pub(crate) fn compose_script(args: &str) -> String {
    format!("set -e\ncd /var/lib/konak/apps/\"$1\"\nexport DOCKER_HOST=unix:///run/podman/podman.sock\nunset COMPOSE_FILE COMPOSE_PATH_SEPARATOR\ndocker-compose -f compose.yml -p \"$1\" {args}\n")
}

fn set_stopped(paths: &Paths, id: &str, stopped: bool) -> Result<(), String> {
    update_state(paths, |s| {
        if let Some(a) = s.apps.iter_mut().find(|a| a.id == id) {
            a.stopped = stopped;
        }
    })
}

/// Stops an app so that it stays stopped. Its containers are removed (`down`; data, volumes and the
/// rendered files are kept): konak-boot.service starts every container with a restart policy at each
/// boot of the distro, and Podman does not remember a manual stop across boots, so a container that
/// is merely stopped would come back after a distro restart or a reboot. With none left there is
/// nothing to start, and this needs no change to the boot unit already written into existing distros.
pub fn stop(paths: &Paths, id: &str) -> Result<(), String> {
    installed(paths, id)?;
    let out = shell::distro_script(&paths.distro, REMOVE_SH, &[id, "keep"]);
    crate::log(paths, &format!("stop {id}: {}", redacted(paths, &tail(&out.text, 2))));
    if !out.ok {
        return Err(redacted(paths, &format!("stop of {id} failed: {}", tail(&out.text, 6))));
    }
    set_stopped(paths, id, true)
}

/// Recreates a stopped app's containers from its rendered compose file and `.env`.
pub fn start(paths: &Paths, id: &str) -> Result<(), String> {
    installed(paths, id)?;
    let out = compose(paths, id, "up -d --remove-orphans");
    if !out.ok {
        return Err(redacted(paths, &format!("start of {id} failed: {}", tail(&out.text, 6))));
    }
    set_stopped(paths, id, false)
}

/// A stopped app has no containers to restart, so restarting it starts it: the user asked for it to
/// run, and a silent no-op would leave it stopped.
pub fn restart(paths: &Paths, id: &str) -> Result<(), String> {
    if installed(paths, id)?.stopped {
        return start(paths, id);
    }
    let out = compose(paths, id, "restart");
    if !out.ok {
        return Err(redacted(paths, &format!("restart of {id} failed: {}", tail(&out.text, 6))));
    }
    Ok(())
}

/// Last `lines` log lines of all the app's containers together (at most `MAX_LOG_LINES`), redacted.
pub fn logs(paths: &Paths, id: &str, lines: u32) -> Result<String, String> {
    installed(paths, id)?;
    let n = lines.clamp(1, MAX_LOG_LINES);
    // `--tail` counts per container, so the merged output is cut to `n` here.
    let out = compose(paths, id, &format!("logs --no-color --timestamps --tail {n}"));
    let text = redacted(paths, &out.text);
    if !out.ok {
        return Err(format!("logs of {id} failed: {}", tail(&text, 6)));
    }
    Ok(last_lines(&text, n as usize))
}

/// Sort key of a `docker-compose logs --timestamps` line (`svc-1  | 2026-11-22T10:00:00.5Z msg`):
/// the time with its fraction padded to nanoseconds, so keys compare as text. `None` without one.
fn time_key(line: &str) -> Option<String> {
    let ts = line.split_once("| ")?.1.split_whitespace().next()?;
    let b = ts.as_bytes();
    if b.len() < 19 || !b[0].is_ascii_digit() || b[10] != b'T' {
        return None;
    }
    let frac: String = ts[19..].strip_prefix('.').unwrap_or("").chars().take_while(char::is_ascii_digit).collect();
    Some(format!("{}{frac:0<9}", &ts[..19]))
}

/// Merges the containers' lines by time and keeps the last `n`. A line without a timestamp stays
/// with the line before it; without any timestamps, this only keeps the last `n` lines.
pub fn last_lines(text: &str, n: usize) -> String {
    let mut key = String::new();
    let mut lines: Vec<(String, &str)> = text
        .lines()
        .map(|l| {
            if let Some(k) = time_key(l) {
                key = k;
            }
            (key.clone(), l)
        })
        .collect();
    lines.sort_by(|a, b| a.0.cmp(&b.0));
    lines[lines.len().saturating_sub(n)..].iter().map(|(_, l)| *l).collect::<Vec<_>>().join("
")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(apps: &[(&str, &str)]) -> State {
        let mut s = State::default();
        s.apps = apps.iter().map(|(id, name)| InstalledApp { id: id.to_string(), name: name.to_string(), installed_unix: 0, claimed: true, stopped: false }).collect();
        s
    }

    #[test]
    fn health_failures_are_told_from_other_install_errors() {
        assert!(INSTALL_SH.contains(r#"echo "$id did not answer on $health within 5 minutes""#), "the message health_failed looks for");
        assert!(health_failed("install of immich failed: curl: (7) ...\nimmich did not answer on /api/server/ping within 5 minutes"));
        assert!(health_failed(&windows_side_check("immich", 1, std::time::Duration::ZERO).unwrap_err()));
        assert!(!health_failed("install of immich failed: Error response from daemon: pull access denied"));
    }

    #[test]
    fn name_validation() {
        let s = state_with(&[("jellyfin", "movies"), ("immich", "photos")]);
        assert!(validate_name(&s, "jellyfin", "films").is_ok());
        assert!(validate_name(&s, "jellyfin", "movies").is_ok(), "keeping its own name");
        assert_eq!(validate_name(&s, "jellyfin", "photos").unwrap_err(), "name_in_use");
        assert_eq!(validate_name(&s, "jellyfin", "konak").unwrap_err(), "name_reserved");
        for bad in ["", "Movies", "-a", "a-", "a.b", "a b", &"x".repeat(64)] {
            assert_eq!(validate_name(&s, "jellyfin", bad).unwrap_err(), "invalid_name", "{bad}");
        }
    }

    #[test]
    fn claim_is_not_recorded_when_the_rerender_fails() {
        let home = std::env::temp_dir().join(format!("konak-claim-test-{}", std::process::id()));
        let paths = Paths { home: home.clone(), distro: "konak-nonexistent".into(), bin: home.join("bin"), media: home.join("m"), photos: home.join("p") };
        let mut s = State::default();
        s.apps.push(InstalledApp { id: "forgejo".into(), name: "git".into(), installed_unix: 1, claimed: false, stopped: false });
        s.save(&paths);
        let not_installed = claim(&paths, "immich").unwrap_err();
        // The distro does not exist, so the re-render with claimed_env fails.
        let failed = claim(&paths, "forgejo");
        let after = State::load(&paths);
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(not_installed, "not_installed");
        assert!(failed.is_err());
        assert!(!after.apps[0].claimed, "claimed only after a successful re-render");
    }

    fn heartbeat_on(paths: &Paths, ports: WebPorts) {
        let hb = serde_json::json!({"last_tick_unix": now(), "lan_ip": null, "web_ports": ports});
        std::fs::write(paths.supervisor_file(), hb.to_string()).unwrap();
    }

    #[test]
    fn a_claim_that_overlaps_a_port_switch_ends_with_the_active_ports() {
        // 02 finding 2: rendered for 18080, the supervisor switches to 80/443 meanwhile and skips
        // the still unclaimed app.
        let home = std::env::temp_dir().join(format!("konak-claim-switch-{}", std::process::id()));
        let paths = Paths { home: home.clone(), distro: "konak-nonexistent".into(), bin: home.join("bin"), media: home.join("m"), photos: home.join("p") };
        let mut s = State::default();
        s.rendered_ports = WebPorts::FALLBACKS[0];
        s.apps.push(InstalledApp { id: "nextcloud".into(), name: "cloud".into(), installed_unix: 1, claimed: false, stopped: false });
        s.save(&paths);
        heartbeat_on(&paths, WebPorts::FALLBACKS[0]);
        let renders = std::cell::RefCell::new(vec![]);
        let switching = |ports: WebPorts, again: bool| {
            renders.borrow_mut().push((ports, again));
            if !again {
                heartbeat_on(&paths, WebPorts::STANDARD);
                update_state(&paths, |s| s.rendered_ports = WebPorts::STANDARD).unwrap();
            }
            Ok(())
        };
        let r = claim_with(&paths, "nextcloud", &switching);
        let after = State::load(&paths);
        assert_eq!(r, Ok(()));
        assert_eq!(*renders.borrow(), vec![(WebPorts::FALLBACKS[0], false), (WebPorts::STANDARD, true)]);
        assert!(after.apps[0].claimed);

        // No switch: one render only.
        renders.borrow_mut().clear();
        assert_eq!(claim_with(&paths, "nextcloud", &|p, a| { renders.borrow_mut().push((p, a)); Ok(()) }), Ok(()));
        assert_eq!(renders.borrow().len(), 1);

        // The follow-up render fails: the claim stands, and the old ports are recorded so the
        // supervisor re-renders every claimed app for the active ones.
        heartbeat_on(&paths, WebPorts::FALLBACKS[1]);
        let failing = |_: WebPorts, again: bool| {
            if again {
                return Err("busy".to_string());
            }
            heartbeat_on(&paths, WebPorts::STANDARD);
            update_state(&paths, |s| s.rendered_ports = WebPorts::STANDARD).unwrap();
            Ok(())
        };
        let r = claim_with(&paths, "nextcloud", &failing);
        let after = State::load(&paths);
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(r, Ok(()));
        assert_eq!(after.rendered_ports, WebPorts::FALLBACKS[1]);
    }

    #[test]
    fn an_install_needs_its_published_port_to_answer_from_windows() {
        use std::io::{Read, Write};
        // Loopback only: nothing on the LAN, no firewall prompt.
        let answering = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = answering.local_addr().unwrap().port();
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut c, _) = answering.accept().unwrap();
            let _ = c.read(&mut [0u8; 64]);
            c.write_all(b"HTTP/1.0 200 OK\r\n\r\n").unwrap();
        });
        assert_eq!(windows_side_check("jellyfin", port, std::time::Duration::from_secs(5)), Ok(()));
        server.join().unwrap();
        let e = windows_side_check("jellyfin", closed, std::time::Duration::from_millis(1)).unwrap_err();
        assert!(e.contains("runs, but its published port") && e.contains(&format!("127.0.0.1:{closed}")) && e.contains("from Windows"), "{e}");
    }

    #[test]
    fn an_unreadable_heartbeat_never_renders_as_80_and_443() {
        let home = std::env::temp_dir().join(format!("konak-torn-hb-{}", std::process::id()));
        let paths = Paths { home: home.clone(), distro: "konak-nonexistent".into(), bin: home.join("bin"), media: home.join("m"), photos: home.join("p") };
        let mut s = State::default();
        s.apps.push(InstalledApp { id: "nextcloud".into(), name: "cloud".into(), installed_unix: 1, claimed: false, stopped: false });
        s.save(&paths);
        // No heartbeat yet: 80/443, as before the first tick.
        assert_eq!(render_ports(&paths), Ok(WebPorts::STANDARD));
        // A truncated file (a write from a build without the atomic write, caught midway).
        std::fs::write(paths.supervisor_file(), r#"{"last_tick_unix": 5, "web_po"#).unwrap();
        let rendered = std::cell::Cell::new(false);
        let r = claim_with(&paths, "nextcloud", &|_, _| {
            rendered.set(true);
            Ok(())
        });
        let after = State::load(&paths);
        let display = active_ports(&paths);
        let _ = std::fs::remove_dir_all(&home);
        assert!(r.unwrap_err().contains("supervisor.json unreadable"));
        assert!(!rendered.get() && !after.apps[0].claimed);
        assert_eq!(display, WebPorts::STANDARD, "display falls back; rendering does not");
    }

    #[test]
    fn stop_and_start_change_the_flag_only_when_they_succeed() {
        let home = std::env::temp_dir().join(format!("konak-stop-test-{}", std::process::id()));
        let paths = Paths { home: home.clone(), distro: "konak-nonexistent".into(), bin: home.join("bin"), media: home.join("m"), photos: home.join("p") };
        let mut s = State::default();
        s.apps.push(InstalledApp { id: "forgejo".into(), name: "git".into(), installed_unix: 1, claimed: true, stopped: false });
        s.apps.push(InstalledApp { id: "jellyfin".into(), name: "movies".into(), installed_unix: 1, claimed: true, stopped: true });
        s.save(&paths);
        let not_installed = stop(&paths, "immich").unwrap_err();
        // The distro does not exist, so `down` and `up -d` fail.
        let stop_failed = stop(&paths, "forgejo").is_err();
        let start_failed = start(&paths, "jellyfin").is_err();
        let restart_failed = restart(&paths, "jellyfin").unwrap_err();
        let after = State::load(&paths);
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(not_installed, "not_installed");
        assert!(stop_failed && start_failed);
        assert!(restart_failed.starts_with("start of jellyfin failed"), "restart of a stopped app starts it: {restart_failed}");
        assert!(!after.apps[0].stopped && after.apps[1].stopped);
    }

    #[test]
    fn a_reserved_name_cannot_be_taken_by_another_app() {
        let home = std::env::temp_dir().join(format!("konak-reserve-test-{}", std::process::id()));
        let paths = Paths { home: home.clone(), distro: "konak-nonexistent".into(), bin: home.join("bin"), media: home.join("m"), photos: home.join("p") };
        let mut s = State::default();
        s.apps.push(InstalledApp { id: "jellyfin".into(), name: "movies".into(), installed_unix: 1, claimed: true, stopped: false });
        s.save(&paths);
        let r = reserve(&paths, "forgejo", "code").unwrap();
        let clash = reserve(&paths, "immich", "code").err();
        let saved_clash = reserve(&paths, "immich", "movies").err();
        // The same app may reserve again (a retry); once released, the name is free for others.
        let again = reserve(&paths, "forgejo", "code").map(std::mem::forget).is_ok();
        drop(r);
        let after = reserve(&paths, "immich", "code").is_ok();
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(clash.as_deref(), Some("name_in_use"));
        assert_eq!(saved_clash.as_deref(), Some("name_in_use"));
        assert!(again && after);
    }

    /// Needs a throwaway distro provisioned with base.sh and engine.sh:
    /// `KONAK_TEST_DISTRO=konak-<name> cargo test live_install -- --ignored --nocapture`.
    /// Installs Forgejo through the real scripts, then checks that its network comes from Konak's
    /// subnet pool and that Windows reaches it on 127.0.0.1.
    #[test]
    #[ignore]
    fn live_install_uses_the_konak_subnet_pool() {
        use std::io::{Read, Write};
        let distro = std::env::var("KONAK_TEST_DISTRO").expect("set KONAK_TEST_DISTRO");
        assert!(distro.starts_with("konak-") && distro != "konak-podman", "only a throwaway Konak distro");
        let home = std::env::temp_dir().join(format!("konak-live-{distro}"));
        let res = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources");
        let paths = Paths { home: home.clone(), distro: distro.clone(), bin: res.join("bin"), media: home.join("m"), photos: home.join("p") };
        let app = install(&paths, "forgejo").unwrap();
        println!("installed as {}", app.name);
        let script = "for n in $(podman inspect forgejo --format '{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{end}}'); do\n  podman network inspect \"$n\" --format '{{range .Subnets}}{{.Subnet}} gw {{.Gateway}}{{end}}'\ndone\n";
        let net = shell::distro_script(&distro, script, &[]);
        println!("forgejo networks: {}", net.text.trim());
        assert!(net.ok && net.text.trim().starts_with("10.234."), "{}", net.text);
        let mut s = std::net::TcpStream::connect("127.0.0.1:47105").unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        write!(s, "GET /api/healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").unwrap();
        let mut reply = String::new();
        let _ = s.read_to_string(&mut reply);
        println!("{}", reply.lines().next().unwrap_or(""));
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    }

    #[test]
    fn logs_are_merged_by_time_and_capped_in_total() {
        let text = "db-1      | 2026-11-22T10:00:01Z ready
db-1      | 2026-11-22T10:00:03.5Z checkpoint
server-1  | 2026-11-22T10:00:00.25Z start
server-1  |   continued line
server-1  | 2026-11-22T10:00:01.000000001Z listening
server-1  | 2026-11-22T10:00:04Z request";
        assert_eq!(
            last_lines(text, 10).lines().map(|l| l.rsplit(' ').next().unwrap()).collect::<Vec<_>>(),
            ["start", "line", "ready", "listening", "checkpoint", "request"]
        );
        assert_eq!(last_lines(text, 2), "db-1      | 2026-11-22T10:00:03.5Z checkpoint
server-1  | 2026-11-22T10:00:04Z request");
        // No timestamps (an older engine): still at most n lines.
        assert_eq!(last_lines("a
b
c", 2), "b
c");
        assert_eq!(last_lines("", 5), "");
    }

    #[test]
    fn container_status() {
        let json = r#"[
          {"Names":["jellyfin"],"State":"running","Status":"Up 2 minutes (healthy)","Labels":{"com.docker.compose.project":"jellyfin"}},
          {"Names":["immich_server"],"State":"running","Status":"Up 1 minute (unhealthy)","Labels":{"com.docker.compose.project":"immich"}},
          {"Names":["immich_db"],"State":"running","Status":"Up 1 minute","Labels":{"com.docker.compose.project":"immich"}},
          {"Names":["vw"],"State":"exited","Status":"Exited (0) 1 minute ago","Labels":{"com.docker.compose.project":"vault"}},
          {"Names":["fj"],"State":"running","Status":"Up 3 seconds (starting)","Labels":{"com.docker.compose.project":"forgejo"}},
          {"Names":["other"],"State":"running","Status":"Up","Labels":null}
        ]"#;
        let p = parse_ps(json).unwrap();
        assert_eq!(p.len(), 4);
        assert_eq!(status_of(&p["jellyfin"]), "running");
        assert_eq!(status_of(&p["immich"]), "unhealthy");
        assert_eq!(status_of(&p["vault"]), "stopped");
        assert_eq!(status_of(&p["forgejo"]), "starting");
        assert_eq!(status_of(&[]), "missing");
        assert_eq!(p["jellyfin"][0].health.as_deref(), Some("healthy"));
        assert!(parse_ps("[]").unwrap().is_empty());
    }

    #[test]
    fn stats_are_summed_per_app() {
        let ps = parse_ps(
            r#"[
          {"Names":["immich_server"],"State":"running","Labels":{"com.docker.compose.project":"immich"}},
          {"Names":["immich_postgres"],"State":"running","Labels":{"com.docker.compose.project":"immich"}},
          {"Names":["jellyfin"],"State":"running","Labels":{"com.docker.compose.project":"jellyfin"}},
          {"Names":["loose"],"State":"running","Labels":null}
        ]"#,
        )
        .unwrap();
        // Podman 5.4: strings with units, decimal sizes. Trailing stderr text is ignored.
        let podman5 = r#"[
         {"id": "3c816f05c85d", "name": "immich_server", "cpu_percent": "0.13%", "mem_usage": "715.9MB / 67.3GB", "pids": "81"},
         {"id": "0fe1d646cde1", "name": "immich_postgres", "cpu_percent": "1.5%", "mem_usage": "6.009MB / 67.3GB"},
         {"id": "5b8d97b9206d", "name": "jellyfin", "cpu_percent": "0.07%", "mem_usage": "512kB / 67.3GB"},
         {"id": "aaaaaaaaaaaa", "name": "loose", "cpu_percent": "50%", "mem_usage": "1GB / 67.3GB"}
        ]
time="2026-11-24" level=warning msg="something""#;
        let s = aggregate_stats(podman5, &ps).unwrap();
        assert_eq!(s.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["immich", "jellyfin"]);
        assert!((s[0].cpu_percent - 1.63).abs() < 1e-9, "{s:?}");
        assert_eq!(s[0].mem_bytes, 715_900_000 + 6_009_000);
        assert_eq!(s[1].mem_bytes, 512_000);

        // Numbers (percent and bytes) under the Go field names, and binary units.
        let numbers = r#"[{"Name": "jellyfin", "CPU": 2.25, "MemUsage": 1048576}, {"Name": "immich_server", "CPU": 0, "MemUsage": "1.5GiB / 16GiB"}]"#;
        let s = aggregate_stats(numbers, &ps).unwrap();
        assert_eq!(s, vec![AppStats { id: "immich".into(), cpu_percent: 0.0, mem_bytes: 1_610_612_736 }, AppStats { id: "jellyfin".into(), cpu_percent: 2.25, mem_bytes: 1_048_576 }]);

        // Nothing running: no stats output at all.
        assert!(aggregate_stats("", &ps).unwrap().is_empty());
        assert!(aggregate_stats("[]", &ps).unwrap().is_empty());
        assert!(aggregate_stats("not json", &ps).is_err());
        assert_eq!(parse_size("0B"), Some(0.0));
        assert_eq!(parse_size("12XB"), None);
    }

    #[test]
    fn env_is_templated() {
        let mut m = catalog::get("jellyfin").unwrap().clone();
        m.env.insert("PUBLIC_URL".into(), "${KONAK_URL}/x".into());
        m.env.insert("HOST".into(), "${KONAK_HOST}".into());
        let env = env_lines(&m, "movies", true, WebPorts::STANDARD);
        assert!(env.contains("KONAK_HOST=movies.local\nKONAK_HOSTPORT=movies.local\nKONAK_URL=http://movies.local\nKONAK_PORT=47101\nKONAK_PROXY_NET=10.234.0.0/16"));
        assert!(env.contains("PUBLIC_URL=http://movies.local/x"));
        assert!(env.contains("HOST=movies.local"));
    }

    #[test]
    fn env_carries_the_fallback_port() {
        let line = |id: &str, ports: WebPorts, key: &str| {
            let env = env_lines(catalog::get(id).unwrap(), "x", true, ports);
            env.lines().find_map(|l| l.strip_prefix(&format!("{key}="))).map(String::from)
        };
        let fb = WebPorts::FALLBACKS[0];
        assert_eq!(line("nextcloud", fb, "OVERWRITEHOST").as_deref(), Some("x.local:18080"));
        assert_eq!(line("nextcloud", fb, "OVERWRITECLIURL").as_deref(), Some("http://x.local:18080"));
        // Nextcloud checks trusted domains without the port; Forgejo's DOMAIN is a bare host too.
        assert_eq!(line("nextcloud", fb, "NEXTCLOUD_TRUSTED_DOMAINS").as_deref(), Some("x.local"));
        assert_eq!(line("forgejo", fb, "KONAK_HOST").as_deref(), Some("x.local"));
        assert_eq!(line("forgejo", fb, "KONAK_URL").as_deref(), Some("http://x.local:18080"));
        // Vaultwarden is HTTPS: its DOMAIN (KONAK_URL in compose.yml) gets the HTTPS port.
        assert_eq!(line("vaultwarden", fb, "KONAK_URL").as_deref(), Some("https://x.local:18443"));
        assert_eq!(line("vaultwarden", fb, "KONAK_HOSTPORT").as_deref(), Some("x.local:18443"));
        assert!(catalog::get("vaultwarden").unwrap().compose.contains("DOMAIN: ${KONAK_URL}"));
        assert!(catalog::get("forgejo").unwrap().compose.contains("ROOT_URL=${KONAK_URL}/"));
        // Back on 80/443 the port is dropped again.
        let std = WebPorts::STANDARD;
        assert_eq!(line("nextcloud", std, "OVERWRITEHOST").as_deref(), Some("x.local"));
        assert_eq!(line("vaultwarden", std, "KONAK_URL").as_deref(), Some("https://x.local"));
        assert_eq!(url(catalog::get("jellyfin").unwrap(), "movies", fb), "http://movies.local:18080");
        assert_eq!(url(catalog::get("jellyfin").unwrap(), "movies", std), "http://movies.local");
        for m in catalog::all() {
            let env = env_lines(m, "x", true, fb);
            assert!(!env.contains(".local:80") && !env.contains(".local:443") && !env.contains("${"), "{}: {env}", m.id);
        }
    }

    #[test]
    fn rerender_for_ports_is_a_no_op_when_already_rendered_and_records_only_on_success() {
        let home = std::env::temp_dir().join(format!("konak-rerender-test-{}", std::process::id()));
        let paths = Paths { home: home.clone(), distro: "konak-nonexistent".into(), bin: home.join("bin"), media: home.join("m"), photos: home.join("p") };
        let mut s = State::default();
        s.apps.push(InstalledApp { id: "forgejo".into(), name: "git".into(), installed_unix: 1, claimed: false, stopped: false });
        s.save(&paths);
        let seen = std::cell::RefCell::new(vec![]);
        let on_app = |id: &str, _: &Result<(), String>| seen.borrow_mut().push(id.to_string());
        // Already rendered for 80/443: nothing runs.
        let same = rerender_for_ports(&paths, WebPorts::STANDARD, &on_app);
        // Only an unclaimed app: nothing to render, the new ports are recorded.
        let none_claimed = rerender_for_ports(&paths, WebPorts::FALLBACKS[0], &on_app);
        let recorded = State::load(&paths).rendered_ports;
        // A claimed app whose render fails (no such distro): the ports are not recorded.
        let mut s = State::load(&paths);
        s.apps[0].claimed = true;
        s.apps[0].stopped = true;
        s.save(&paths);
        let failed = rerender_for_ports(&paths, WebPorts::STANDARD, &on_app);
        let after = State::load(&paths);
        let _ = std::fs::remove_dir_all(&home);
        assert!(same.is_ok() && none_claimed.is_ok());
        assert_eq!(recorded, WebPorts::FALLBACKS[0]);
        assert!(failed.is_err());
        assert_eq!(after.rendered_ports, WebPorts::FALLBACKS[0], "retried as a whole later");
        assert!(after.apps[0].stopped, "a stopped app stays stopped");
        assert_eq!(*seen.borrow(), ["forgejo"]);
    }

    #[test]
    fn apps_trust_only_the_proxy_net() {
        let env = |id: &str| env_lines(catalog::get(id).unwrap(), id, true, WebPorts::STANDARD);
        assert!(env("nextcloud").lines().any(|l| l == "TRUSTED_PROXIES=10.234.0.0/16"));
        assert!(env("nextcloud").lines().any(|l| l == "FORWARDED_FOR_HEADERS=HTTP_X_FORWARDED_FOR"));
        assert!(env("immich").lines().any(|l| l == "IMMICH_TRUSTED_PROXIES=10.234.0.0/16"));
        // The trusted range is the subnet pool engine.sh gives Konak's Podman.
        assert!(include_str!("../scripts/engine.sh").contains(&format!("{{\"base\" = \"{PROXY_NET}\", \"size\" = 24}}")));
        let compose = |id: &str| catalog::get(id).unwrap().compose;
        assert!(compose("forgejo").contains("FORGEJO__security__REVERSE_PROXY_TRUSTED_PROXIES=${KONAK_PROXY_NET}"));
        assert!(compose("forgejo").contains("FORGEJO__security__REVERSE_PROXY_LIMIT=1"));
        assert!(compose("vaultwarden").contains("IP_HEADER: X-Forwarded-For"));
        for m in catalog::all() {
            assert!(!m.compose.contains("0.0.0.0/0") && !m.env.values().any(|v| v.contains("0.0.0.0/0")), "{}", m.id);
        }
    }

    #[test]
    fn claimed_env_only_after_claim() {
        let m = catalog::get("forgejo").unwrap();
        assert_eq!(m.claimed_env["FORGEJO_DISABLE_REGISTRATION"], "true");
        assert!(!env_lines(m, "git", false, WebPorts::STANDARD).contains("FORGEJO_DISABLE_REGISTRATION"));
        assert!(env_lines(m, "git", true, WebPorts::STANDARD).lines().any(|l| l == "FORGEJO_DISABLE_REGISTRATION=true"));
        // The compose file falls back to open registration while the app is unclaimed.
        assert!(m.compose.contains("FORGEJO__service__DISABLE_REGISTRATION=${FORGEJO_DISABLE_REGISTRATION:-false}"));
    }

    #[test]
    fn nextcloud_points_at_its_local_name_only_once_claimed() {
        // Before the claim, the web installer on 127.0.0.1 must not redirect to the .local name,
        // which nothing announces yet. The image applies OVERWRITEHOST per request, and Nextcloud
        // trusts every host once overwritehost is set, so the claimed .local name is trusted.
        let m = catalog::get("nextcloud").unwrap();
        let before = env_lines(m, "cloud", false, WebPorts::STANDARD);
        assert!(!before.contains("OVERWRITEHOST") && !before.contains("OVERWRITECLIURL"));
        let after = env_lines(m, "cloud", true, WebPorts::STANDARD);
        assert!(after.lines().any(|l| l == "OVERWRITEHOST=cloud.local"));
        assert!(after.lines().any(|l| l == "OVERWRITECLIURL=http://cloud.local"));
    }
}
