//! The clean-install flow (spike step 5). Steps run in order; finished steps are persisted so the
//! flow resumes after the reboot that installing WSL needs. Every step logs its duration to
//! `logs/install.log` so the run can be measured (UAC prompts, reboots, time, download size).

use crate::paths::{skip_elevation, Paths};
use crate::services::{lan_ip, Supervisor, HOME_NAME};
use crate::shell::{self, tail};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The first run installs the platform only; apps are installed from the panel (`apps.rs`).
pub const STEPS: [&str; 7] = ["preflight", "admin", "reboot", "distro", "systemd", "engine", "network"];

const PREFLIGHT_PS1: &str = include_str!("../scripts/preflight.ps1");
const ELEVATED_PS1: &str = include_str!("../scripts/elevated.ps1");
const BASE_SH: &str = include_str!("../scripts/base.sh");
const ENGINE_SH: &str = include_str!("../scripts/engine.sh");

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct InstalledApp {
    /// Catalog id.
    pub id: String,
    /// `.local` name without the suffix, chosen by the user.
    pub name: String,
    pub installed_unix: u64,
    /// False until the user has created the admin account on this PC. Unclaimed apps get no `.local`
    /// name and no Caddy route, so nobody on the LAN can take over the first-run admin setup. State
    /// entries from before this field were already exposed, so they load as claimed.
    #[serde(default = "claimed_by_default")]
    pub claimed: bool,
}

fn claimed_by_default() -> bool {
    true
}

/// Persisted install progress, measurements and the installed apps. Missing fields default, so
/// older state files keep loading.
#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub struct State {
    pub completed: Vec<String>,
    pub skipped: Vec<String>,
    pub reboot_pending: bool,
    pub boot_time: u64,
    pub uac_prompts: u32,
    pub reboots: u32,
    pub started_unix: u64,
    pub finished_unix: u64,
    pub rx_total: i64,
    pub rx_baseline: Option<i64>,
    /// Before the catalog, Konak installed only Jellyfin and stored its name here. Read once and
    /// migrated into `apps`.
    #[serde(skip_serializing)]
    app_name: Option<String>,
    pub apps: Vec<InstalledApp>,
    /// Apps whose containers must be re-rendered by the install script at the next start (the
    /// migrated Jellyfin still listens on its old port 8096).
    pub rerender: Vec<String>,
    pub done: bool,
}

#[derive(Deserialize, Default, Clone)]
pub struct Preflight {
    pub interface_index: i64,
    pub network_category: String,
    pub virtualization_fw: bool,
    pub hypervisor_present: bool,
    pub rx_bytes: i64,
}

pub enum Flow {
    Next,
    /// Stop here and wait for the user (reboot).
    Pause,
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

extern "system" {
    fn GetTickCount64() -> u64;
}

/// Approximate boot time, used to tell whether a reboot happened while paused.
pub fn boot_time() -> u64 {
    now() - unsafe { GetTickCount64() } / 1000
}

impl State {
    /// The UI may restart the PC only while the install waits for the reboot WSL needs.
    pub fn may_restart_pc(&self) -> Result<(), String> {
        if self.reboot_pending {
            Ok(())
        } else {
            Err("no_reboot_pending".into())
        }
    }
    /// For readers that can live with a fresh state when the file is unreadable. Writers use
    /// `try_load`, so a damaged file is never overwritten with defaults.
    pub fn load(paths: &Paths) -> State {
        State::try_load(paths).unwrap_or_default()
    }
    /// A missing file is a fresh install. A file that cannot be read or parsed is an error
    /// (`state_unreadable`, details in the log); it is retried briefly first, in case another
    /// process holds it open for a moment.
    pub fn try_load(paths: &Paths) -> Result<State, String> {
        let mut last = String::new();
        for attempt in 0..3 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(100));
            }
            match std::fs::read_to_string(paths.state_file()) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::default()),
                Err(e) => last = e.to_string(),
                Ok(text) => match State::from_json(&text) {
                    Ok(s) => return Ok(s),
                    Err(e) => last = e,
                },
            }
        }
        crate::log(paths, &format!("state.json unreadable, left as it is: {last}"));
        Err("state_unreadable".into())
    }
    pub fn from_json(text: &str) -> Result<State, String> {
        let mut s: State = serde_json::from_str(text).map_err(|e| e.to_string())?;
        if let Some(name) = s.app_name.take() {
            if !s.apps.iter().any(|a| a.id == "jellyfin") {
                s.apps.push(InstalledApp { id: "jellyfin".into(), name, installed_unix: s.finished_unix, claimed: true });
                s.rerender.push("jellyfin".into());
            }
        }
        Ok(s)
    }
    /// Writes a temporary file, flushes it and renames it over `state.json`, so a reader never sees
    /// a half-written file and a power loss leaves either the old or the new state.
    pub fn save(&self, paths: &Paths) {
        if let Err(e) = self.try_save(paths) {
            crate::log(paths, &format!("state.json not saved: {e}"));
        }
    }
    pub fn try_save(&self, paths: &Paths) -> std::io::Result<()> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::fs::create_dir_all(&paths.home)?;
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = paths.home.join(format!("state.json.{}-{n}.tmp", std::process::id()));
        let result = (|| {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(serde_json::to_string_pretty(self).unwrap().as_bytes())?;
            f.sync_all()?;
            drop(f);
            // A reader in another process can hold the target open for a moment; retry briefly.
            let mut tries = 0;
            loop {
                match std::fs::rename(&tmp, paths.state_file()) {
                    Ok(()) => return Ok(()),
                    Err(_) if tries < 5 => {
                        tries += 1;
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(e) => return Err(e),
                }
            }
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }
    pub fn finished(&self, step: &str) -> bool {
        self.completed.iter().any(|s| s == step) || self.skipped.iter().any(|s| s == step)
    }
}

fn powershell_file(paths: &Paths, name: &str, body: &str, args: &[&str]) -> shell::Out {
    let dir = paths.home.join("scripts");
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join(name);
    let _ = std::fs::write(&file, body);
    let f = file.to_string_lossy().to_string();
    let mut full = vec!["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", &f];
    full.extend_from_slice(args);
    shell::run("powershell.exe", &full)
}

pub fn preflight(paths: &Paths) -> Result<Preflight, String> {
    let out = powershell_file(paths, "preflight.ps1", PREFLIGHT_PS1, &[]);
    let json = out.text.lines().rev().find(|l| l.trim_start().starts_with('{')).unwrap_or("");
    serde_json::from_str(json).map_err(|e| format!("preflight output unreadable: {e}: {}", tail(&out.text, 5)))
}

pub fn wsl_installed() -> bool {
    shell::run("wsl.exe", &["--version"]).ok
}

pub fn distro_exists(distro: &str) -> bool {
    let out = shell::run("wsl.exe", &["-l", "-q"]);
    out.ok && out.text.lines().any(|l| l.trim() == distro)
}

/// Registers Konak to start at logon (per user, no admin). Used to resume after the reboot and,
/// once installed, to run the supervisor from the tray.
pub fn register_autostart() {
    // Dev runs (see app/README.md) must not add the dev build to the developer's logon programs.
    if skip_elevation() {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        let value = format!("\"{}\" --autostart", exe.display());
        shell::run("reg.exe", &["add", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run", "/v", "Konak", "/t", "REG_SZ", "/d", &value, "/f"]);
    }
}

/// One UAC prompt for everything that needs admin. Returns whether a reboot is required.
fn elevate(paths: &Paths, enable_wsl: bool, set_private_interface: i64) -> Result<bool, String> {
    let dir = paths.home.join("scripts");
    let _ = std::fs::create_dir_all(&dir);
    let script = dir.join("elevated.ps1");
    let params = dir.join("elevated-params.json");
    let result = dir.join("elevated-result.json");
    let _ = std::fs::remove_file(&result);
    std::fs::write(&script, ELEVATED_PS1).map_err(|e| e.to_string())?;
    let p = serde_json::json!({
        "enable_wsl": enable_wsl,
        "programs": [paths.caddy(), paths.mdns()],
        "set_private_interface": set_private_interface,
        "result_file": result,
    });
    std::fs::write(&params, p.to_string()).map_err(|e| e.to_string())?;

    let q = |p: &std::path::Path| p.to_string_lossy().replace('\'', "''");
    let cmd = format!(
        "try {{ $p = Start-Process -FilePath powershell.exe -Verb RunAs -WindowStyle Hidden -Wait -PassThru -ArgumentList '-NoProfile -ExecutionPolicy Bypass -File \"{}\" -ParamFile \"{}\"'; exit $p.ExitCode }} catch {{ exit 1223 }}",
        q(&script),
        q(&params)
    );
    let out = shell::run("powershell.exe", &["-NoProfile", "-NonInteractive", "-Command", &cmd]);
    if out.code == Some(1223) {
        return Err("uac_declined".into());
    }
    let text = std::fs::read_to_string(&result).map_err(|_| format!("elevated step left no result: {}", tail(&out.text, 5)))?;
    let v: serde_json::Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| e.to_string())?;
    if let Some(errors) = v["errors"].as_array().filter(|a| !a.is_empty()) {
        return Err(format!("elevated step failed: {}", errors.iter().map(|e| e.as_str().unwrap_or("")).collect::<Vec<_>>().join("; ")));
    }
    crate::log(paths, &format!("wsl --install output: {}", tail(v["wsl_output"].as_str().unwrap_or(""), 4)));
    Ok(v["reboot_required"].as_bool().unwrap_or(false))
}

/// Whether Konak's firewall rules for the current caddy.exe and mdns-responder.exe are missing
/// (read-only; the same facts as the MCP network check). A reinstall that kept the data or a moved
/// install loses them, and nothing re-adds them on its own. `false` in dev runs, which never have
/// rules, and when the rules cannot be read.
pub fn firewall_missing(paths: &Paths) -> bool {
    if skip_elevation() {
        return false;
    }
    match crate::diag::net_facts(paths, None) {
        Ok(f) => rules_missing(&f.firewall),
        Err(e) => {
            crate::log(paths, &format!("firewall check failed: {e}"));
            false
        }
    }
}

pub fn rules_missing(firewall: &[crate::diag::FwProgram]) -> bool {
    firewall.is_empty() || firewall.iter().any(|p| !p.private_allow())
}

/// Re-adds the firewall rules through the same elevated step as the setup (one UAC prompt). Only
/// ever run when the user clicks "Repair network access" in the panel.
pub fn repair_network(paths: &Paths) -> Result<(), String> {
    crate::log(paths, "repairing the firewall rules");
    elevate(paths, false, 0).map(|_| ())
}

fn http_status(ip: &str, host: &str) -> Option<u16> {
    let addr = format!("{ip}:80").parse().ok()?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(s, "GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").ok()?;
    let mut buf = [0u8; 64];
    let n = s.read(&mut buf).ok()?;
    String::from_utf8_lossy(&buf[..n]).split_whitespace().nth(1)?.parse().ok()
}

/// Runs one step. `set_private` is the user's choice on the Public-network notice.
pub fn run_step(step: &str, paths: &Paths, state: &mut State, pre: &Preflight, set_private: bool, sup: &Arc<Supervisor>) -> Result<Flow, String> {
    let distro = paths.distro.as_str();
    match step {
        "preflight" => {
            if !pre.virtualization_fw && !pre.hypervisor_present {
                return Err("virtualization_off".into());
            }
        }
        "admin" => {
            let enable_wsl = !wsl_installed();
            let iface = if set_private && pre.network_category == "Public" { pre.interface_index } else { 0 };
            if skip_elevation() {
                if enable_wsl {
                    return Err("KONAK_SKIP_ELEVATION is set but WSL is not installed".into());
                }
                state.skipped.push(step.into());
                return Ok(Flow::Next);
            }
            state.uac_prompts += 1;
            state.save(paths);
            if elevate(paths, enable_wsl, iface)? {
                state.reboot_pending = true;
                state.boot_time = boot_time();
            }
        }
        "reboot" => {
            if !state.reboot_pending {
                state.skipped.push(step.into());
                return Ok(Flow::Next);
            }
            // Adapter counters reset on reboot, so bank this boot's download bytes now.
            if let (Some(base), Ok(p)) = (state.rx_baseline, preflight(paths)) {
                state.rx_total += p.rx_bytes - base;
            }
            state.rx_baseline = None;
            state.save(paths);
            return Ok(Flow::Pause);
        }
        "distro" => {
            if !distro_exists(distro) {
                let dir = paths.distro_dir();
                let _ = std::fs::create_dir_all(&dir);
                let out = shell::run("wsl.exe", &["--install", "Debian", "--name", distro, "--location", &dir.to_string_lossy(), "--no-launch"]);
                crate::log(paths, &format!("wsl --install Debian: {}", tail(&out.text, 3)));
                if !distro_exists(distro) {
                    return Err(format!("distro install failed: {}", tail(&out.text, 5)));
                }
            }
        }
        "systemd" => {
            let out = shell::distro_script(distro, BASE_SH, &[]);
            if !out.ok {
                return Err(format!("base config failed: {}", tail(&out.text, 5)));
            }
            // Only Konak's own distro is restarted; other distros and the WSL VM are untouched.
            shell::run("wsl.exe", &["--terminate", distro]);
            let out = shell::run("wsl.exe", &["-d", distro, "-u", "root", "--", "systemctl", "is-system-running", "--wait"]);
            let st = out.text.trim().to_string();
            if !(st.contains("running") || st.contains("degraded")) {
                return Err(format!("systemd did not start: {st}"));
            }
        }
        "engine" => {
            let out = shell::distro_script(distro, ENGINE_SH, &[]);
            crate::log(paths, &format!("engine: {}", tail(&out.text, 3)));
            if !out.ok {
                return Err(format!("container engine install failed: {}", tail(&out.text, 6)));
            }
        }
        "network" => {
            let ip = lan_ip().ok_or("no LAN IP address")?;
            crate::ca::ensure(paths)?;
            let _ = crate::uninstall::stop_leftovers(paths);
            sup.start();
            sup.tick();
            let host = format!("{HOME_NAME}.local");
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                match http_status(&ip, &host) {
                    Some(200) => break,
                    other if Instant::now() > deadline => {
                        return Err(format!("http://{host} did not answer through Caddy (last status {other:?})"));
                    }
                    _ => std::thread::sleep(Duration::from_secs(2)),
                }
            }
            crate::log(paths, &format!("reachable: http://{host} via {ip}:80"));
        }
        _ => return Err(format!("unknown step {step}")),
    }
    state.completed.push(step.into());
    Ok(Flow::Next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_only_while_a_reboot_is_pending() {
        let mut s = State::default();
        assert_eq!(s.may_restart_pc().unwrap_err(), "no_reboot_pending");
        s.reboot_pending = true;
        assert!(s.may_restart_pc().is_ok());
    }

    #[test]
    fn migrates_the_old_app_name() {
        let old = r#"{"completed":["preflight","app","network"],"skipped":[],"reboot_pending":false,"boot_time":1,
            "uac_prompts":1,"reboots":0,"started_unix":10,"finished_unix":20,"rx_total":5,"rx_baseline":null,
            "app_name":"jellyfin-2","done":true}"#;
        let s = State::from_json(old).unwrap();
        assert!(s.done);
        assert_eq!(s.apps, vec![InstalledApp { id: "jellyfin".into(), name: "jellyfin-2".into(), installed_unix: 20, claimed: true }]);
        assert_eq!(s.rerender, vec!["jellyfin".to_string()]);
        let saved = serde_json::to_string(&s).unwrap();
        assert!(!saved.contains("app_name"));
        // Loading the saved form again does not migrate twice.
        let again = State::from_json(&saved).unwrap();
        assert_eq!(again.apps.len(), 1);
        assert_eq!(again.rerender.len(), 1);
    }

    #[test]
    fn apps_saved_before_the_claim_step_load_as_claimed() {
        let s = State::from_json(r#"{"apps":[{"id":"immich","name":"photos","installed_unix":5}]}"#).unwrap();
        assert!(s.apps[0].claimed);
        let unclaimed = InstalledApp { id: "forgejo".into(), name: "git".into(), installed_unix: 6, claimed: false };
        let back: InstalledApp = serde_json::from_str(&serde_json::to_string(&unclaimed).unwrap()).unwrap();
        assert_eq!(back, unclaimed);
    }

    #[test]
    fn fresh_state_has_no_apps() {
        let s = State::from_json(r#"{"completed":["preflight"]}"#).unwrap();
        assert!(s.apps.is_empty() && s.rerender.is_empty() && !s.done);
    }

    fn temp_paths(tag: &str) -> Paths {
        let home = std::env::temp_dir().join(format!("konak-state-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        Paths { home: home.clone(), distro: "konak-nonexistent".into(), bin: home.join("bin"), media: home.join("m"), photos: home.join("p") }
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_files() {
        let paths = temp_paths("save");
        assert!(State::try_load(&paths).unwrap().apps.is_empty(), "missing file is a fresh state");
        let mut s = State { done: true, apps: vec![InstalledApp { id: "forgejo".into(), name: "git".into(), installed_unix: 1, claimed: true }], ..State::default() };
        s.save(&paths);
        s.apps[0].name = "code".into();
        s.save(&paths);
        let back = State::try_load(&paths).unwrap();
        let files: Vec<_> = std::fs::read_dir(&paths.home).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        let _ = std::fs::remove_dir_all(&paths.home);
        assert!(back.done && back.apps[0].name == "code");
        assert_eq!(files, vec!["state.json".to_string()], "no temporary files left behind");
    }

    #[test]
    fn a_damaged_state_file_is_an_error_not_an_empty_state() {
        let paths = temp_paths("damaged");
        std::fs::create_dir_all(&paths.home).unwrap();
        std::fs::write(paths.state_file(), r#"{"done":true,"apps":[{"id":"forg"#).unwrap();
        let r = State::try_load(&paths);
        let text = std::fs::read_to_string(paths.state_file()).unwrap();
        let _ = std::fs::remove_dir_all(&paths.home);
        assert_eq!(r.err().as_deref(), Some("state_unreadable"));
        assert!(text.contains("forg"), "the damaged file is left as it is");
        assert!(State::from_json("").is_err());
    }

    #[test]
    fn firewall_rules_count_only_when_private_allow_exists() {
        use crate::diag::{FwProgram, FwRule};
        let rule = |action: &str, profile: &str| FwRule { name: "r".into(), enabled: true, action: action.into(), profile: profile.into(), direction: "Inbound".into() };
        let prog = |rules| FwProgram { program: "caddy.exe".into(), rules };
        assert!(rules_missing(&[]));
        assert!(rules_missing(&[prog(vec![rule("Allow", "Private")]), prog(vec![rule("Block", "Public")])]));
        assert!(!rules_missing(&[prog(vec![rule("Allow", "Private"), rule("Block", "Public")]), prog(vec![rule("Allow", "Private")])]));
    }
}
