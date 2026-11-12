//! The clean-install flow (spike step 5). Steps run in order; finished steps are persisted so the
//! flow resumes after the reboot that installing WSL needs. Every step logs its duration to
//! `logs/install.log` so the run can be measured (UAC prompts, reboots, time, download size).

use crate::paths::{skip_elevation, Paths};
use crate::services::{lan_ip, Supervisor, APP_PORT};
use crate::shell::{self, tail};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const STEPS: [&str; 8] = ["preflight", "admin", "reboot", "distro", "systemd", "engine", "app", "network"];

const PREFLIGHT_PS1: &str = include_str!("../scripts/preflight.ps1");
const ELEVATED_PS1: &str = include_str!("../scripts/elevated.ps1");
const BASE_SH: &str = include_str!("../scripts/base.sh");
const ENGINE_SH: &str = include_str!("../scripts/engine.sh");
const APP_SH: &str = include_str!("../scripts/app-jellyfin.sh");

/// Persisted install progress and measurements.
#[derive(Serialize, Deserialize, Default, Clone)]
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
    pub app_name: Option<String>,
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
    pub fn load(paths: &Paths) -> State {
        std::fs::read_to_string(paths.state_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }
    pub fn save(&self, paths: &Paths) {
        let _ = std::fs::create_dir_all(&paths.home);
        let _ = std::fs::write(paths.state_file(), serde_json::to_string_pretty(self).unwrap());
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

fn distro_exists(distro: &str) -> bool {
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
fn elevate(paths: &Paths, state: &mut State, enable_wsl: bool, set_private_interface: i64) -> Result<bool, String> {
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
    state.uac_prompts += 1;
    state.save(paths);
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

fn http_status(ip: &str, host: &str) -> Option<u16> {
    let addr = format!("{ip}:80").parse().ok()?;
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(s, "GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").ok()?;
    let mut buf = [0u8; 64];
    let n = s.read(&mut buf).ok()?;
    String::from_utf8_lossy(&buf[..n]).split_whitespace().nth(1)?.parse().ok()
}

/// First free `.local` name from `base`, `base-2`, ... (asks the LAN through the mDNS responder).
fn pick_name(paths: &Paths, ip: &str, base: &str) -> Result<String, String> {
    for i in 1..=5 {
        let name = if i == 1 { base.to_string() } else { format!("{base}-{i}") };
        let out = shell::run(&paths.mdns().to_string_lossy(), &[ip, "1", &name, "--check-only"]);
        crate::log(paths, &format!("name check {name}: {}", tail(&out.text, 1)));
        if out.ok {
            return Ok(name);
        }
    }
    Err(format!("names {base} to {base}-5 are all taken on this network"))
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
            if elevate(paths, state, enable_wsl, iface)? {
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
        "app" => {
            std::fs::create_dir_all(&paths.media).map_err(|e| format!("cannot create media folder: {e}"))?;
            let out = shell::distro_script(distro, APP_SH, &[&paths.media.to_string_lossy()]);
            if !out.ok {
                return Err(format!("app start failed: {}", tail(&out.text, 6)));
            }
        }
        "network" => {
            let ip = lan_ip().ok_or("no LAN IP address")?;
            sup.stop_mdns();
            let name = match state.app_name.clone() {
                Some(n) => n,
                None => pick_name(paths, &ip, "jellyfin")?,
            };
            state.app_name = Some(name.clone());
            state.save(paths);
            sup.set_name(&name);
            sup.start();
            sup.tick();
            let host = format!("{name}.local");
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                match http_status(&ip, &host) {
                    Some(200) | Some(302) => break,
                    other if Instant::now() > deadline => {
                        return Err(format!("http://{host} did not answer through Caddy (last status {other:?})"));
                    }
                    _ => std::thread::sleep(Duration::from_secs(2)),
                }
            }
            crate::log(paths, &format!("reachable: http://{host} via {ip}:80 (app port {APP_PORT})"));
        }
        _ => return Err(format!("unknown step {step}")),
    }
    state.completed.push(step.into());
    Ok(Flow::Next)
}
