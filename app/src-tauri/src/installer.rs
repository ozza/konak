//! The clean-install flow (spike step 5). Steps run in order; finished steps are persisted so the
//! flow resumes after the reboot that installing WSL needs. Every step logs its duration to
//! `logs/install.log` so the run can be measured (UAC prompts, reboots, time, download size).

use crate::paths::{skip_elevation, Paths};
use crate::services::{lan_ip, Supervisor, WebPorts, HOME_NAME};
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
    /// Stopped by the user (`apps::stop`): its containers are removed, data kept, so nothing starts
    /// it again until `apps::start`. Older state files have no such field and load as running.
    #[serde(default)]
    pub stopped: bool,
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
    /// The web ports the claimed apps' settings were last rendered for (`apps::rerender_for_ports`).
    /// State files from before the port fallback were rendered for 80/443.
    pub rendered_ports: WebPorts,
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
                s.apps.push(InstalledApp { id: "jellyfin".into(), name, installed_unix: s.finished_unix, claimed: true, stopped: false });
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

/// Whether WSL has `distro` registered. `Ok(false)` only when that is certain: `wsl -l -q` listed the
/// distros without it, or, when `wsl -l` fails (no distros at all, or a WSL service that is hung,
/// updating or broken), WSL's own registry list (`HKCU\...\Lxss`) has no such name. `Err` when
/// neither could be read, so a caller never treats "cannot tell" as "not there".
pub fn distro_exists(distro: &str) -> Result<bool, String> {
    let out = shell::run("wsl.exe", &["-l", "-q"]);
    distro_listed(distro, out.ok, &out.text, registered_distros)
}

/// The decision behind `distro_exists`, apart from the commands so it can be tested.
pub fn distro_listed(distro: &str, wsl_ok: bool, wsl_text: &str, registry: impl FnOnce() -> Result<Vec<String>, String>) -> Result<bool, String> {
    if wsl_ok {
        return Ok(wsl_text.lines().any(|l| l.trim().eq_ignore_ascii_case(distro)));
    }
    match registry() {
        Ok(names) => Ok(names.iter().any(|n| n.trim().eq_ignore_ascii_case(distro))),
        Err(e) => Err(format!("cannot tell whether WSL distro '{distro}' is registered: `wsl -l -q` failed ({}) and {e}", tail(wsl_text, 1).trim())),
    }
}

/// Read-only: the `DistributionName` of every subkey of WSL's per-user registry list. A missing key
/// is an empty list (WSL never registered a distro for this user).
const LXSS_PS1: &str = "$k = 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Lxss'\n\
if (Test-Path -LiteralPath $k) { Get-ChildItem -LiteralPath $k -ErrorAction Stop | ForEach-Object { 'name ' + $_.GetValue('DistributionName') } }\n\
'end'\n";

fn registered_distros() -> Result<Vec<String>, String> {
    let out = shell::run("powershell.exe", &["-NoProfile", "-NonInteractive", "-Command", LXSS_PS1]);
    parse_registered(out.ok, &out.text)
}

/// `LXSS_PS1` output; the closing `end` line proves the whole list was read.
pub fn parse_registered(ok: bool, text: &str) -> Result<Vec<String>, String> {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if !ok || lines.last() != Some(&"end") {
        return Err(format!("the WSL registry list could not be read ({})", tail(text, 1).trim()));
    }
    Ok(lines.iter().filter_map(|l| l.strip_prefix("name ")).map(str::trim).filter(|n| !n.is_empty()).map(String::from).collect())
}

/// Registers Konak to start at logon (per user, no admin). Used to resume after the reboot and,
/// once installed, to run the supervisor from the tray.
pub fn register_autostart() -> Result<(), String> {
    // Dev runs (see app/README.md) must not add the dev build to the developer's logon programs.
    if skip_elevation() {
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let value = format!("\"{}\" --autostart", exe.display());
    let out = shell::run("reg.exe", &["add", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run", "/v", "Konak", "/t", "REG_SZ", "/d", &value, "/f"]);
    out.ok.then_some(()).ok_or_else(|| format!("reg add failed: {}", shell::tail(&out.text, 2)))
}

/// `-EncodedCommand` argument: base64 of the UTF-16LE text, as powershell.exe expects it.
fn encode_command(text: &str) -> String {
    use base64::Engine;
    let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The elevated process's `-EncodedCommand`: `script` as a script block, called with `params` as an
/// inline base64 JSON argument. The elevated process reads no file a same-user process could swap
/// while the UAC prompt is open (base64 has no quote characters, so the argument needs no escaping).
fn elevated_command(script: &str, params: &serde_json::Value) -> String {
    use base64::Engine;
    let p = base64::engine::general_purpose::STANDARD.encode(params.to_string());
    encode_command(&format!("& {{\n{script}\n}} -ParamsB64 '{p}'"))
}

/// The non-elevated launcher: one UAC prompt, waits, and exits with the elevated exit code (1223
/// when the prompt was declined).
fn elevation_launcher(encoded: &str) -> String {
    format!("try {{ $p = Start-Process -FilePath powershell.exe -Verb RunAs -WindowStyle Hidden -Wait -PassThru -ArgumentList '-NoProfile -NonInteractive -ExecutionPolicy Bypass -EncodedCommand {encoded}'; exit $p.ExitCode }} catch {{ exit 1223 }}")
}

/// One UAC prompt for everything that needs admin. Returns whether a reboot is required.
fn elevate(paths: &Paths, enable_wsl: bool, set_private_interface: i64) -> Result<bool, String> {
    // Files earlier builds left in the scripts folder for the elevated step; nothing uses them now.
    let dir = paths.home.join("scripts");
    for f in ["elevated.ps1", "elevated-params.json", "elevated-result.json"] {
        let _ = std::fs::remove_file(dir.join(f));
    }
    let pipe = result_pipe::ResultPipe::create(result_pipe::ELEVATED_ONLY)?;
    let p = serde_json::json!({
        "enable_wsl": enable_wsl,
        "pipe": pipe.name,
        "bin": paths.bin,
        "programs": [paths.caddy(), paths.mdns()],
        "set_private_interface": set_private_interface,
    });
    let out = shell::run("powershell.exe", &["-NoProfile", "-NonInteractive", "-Command", &elevation_launcher(&elevated_command(ELEVATED_PS1, &p))]);
    if out.code == Some(1223) {
        return Err("uac_declined".into());
    }
    let reply = pipe.collect();
    let r = elevated_outcome(reply.as_deref(), out.code).map_err(|e| format!("{e}: {}", tail(&out.text, 5)))?;
    if !r.errors.is_empty() {
        return Err(format!("elevated step failed: {}", r.errors.join("; ")));
    }
    crate::log(paths, &format!("wsl --install output: {}", tail(&r.wsl_output, 4)));
    Ok(r.reboot_required)
}

/// What the elevated step reports: through the pipe when that worked, else from its exit code.
#[derive(Debug, PartialEq)]
pub struct Elevated {
    pub reboot_required: bool,
    pub errors: Vec<String>,
    pub wsl_output: String,
}

/// The exit code's flags (see `elevated.ps1`), each with the error prefix it stands for.
const EXIT_BASE: i32 = 4096;
const ERROR_FLAGS: [(i32, &str); 3] = [(2, "wsl:"), (4, "firewall:"), (8, "network profile:")];

/// Flags of a finished elevated run; `None` when it did not get to the end (refused parameters,
/// PowerShell failed, killed).
fn exit_flags(code: Option<i32>) -> Option<i32> {
    code.filter(|c| (EXIT_BASE..EXIT_BASE + 16).contains(c)).map(|c| c - EXIT_BASE)
}

/// The pipe reply, accepted only when it is well formed and agrees with the exit code's flags: a
/// process that got hold of the pipe can at most deny a result, never change it.
fn parse_reply(reply: &[u8], flags: i32) -> Option<Elevated> {
    let v: serde_json::Value = serde_json::from_slice(reply).ok()?;
    let errors: Vec<String> = v.get("errors")?.as_array()?.iter().map(|e| e.as_str().map(String::from)).collect::<Option<_>>()?;
    let r = Elevated { reboot_required: v.get("reboot_required")?.as_bool()?, errors, wsl_output: v.get("wsl_output")?.as_str()?.to_string() };
    let mut got = i32::from(r.reboot_required);
    for (bit, prefix) in ERROR_FLAGS {
        if r.errors.iter().any(|e| e.starts_with(prefix)) {
            got |= bit;
        }
    }
    let known = r.errors.iter().all(|e| ERROR_FLAGS.iter().any(|(_, p)| e.starts_with(p)));
    (got == flags && known).then_some(r)
}

/// The outcome of the elevated run from its pipe reply and exit code.
pub fn elevated_outcome(reply: Option<&[u8]>, code: Option<i32>) -> Result<Elevated, String> {
    let Some(flags) = exit_flags(code) else {
        return Err(match code {
            Some(3) => "elevated step refused its parameters".into(),
            c => format!("elevated step did not finish (exit {c:?})"),
        });
    };
    if let Some(r) = reply.and_then(|b| parse_reply(b, flags)) {
        return Ok(r);
    }
    // The pipe failed: the flags still say what happened, without the details.
    let errors = ERROR_FLAGS.iter().filter(|(bit, _)| flags & bit != 0).map(|(_, p)| format!("{p} failed (details lost: no result from the elevated step)")).collect();
    Ok(Elevated { reboot_required: flags & 1 != 0, errors, wsl_output: String::new() })
}

/// The channel the elevated step reports through: a named pipe this (unelevated) process creates
/// before the UAC prompt, with a random name passed inline. The elevated process writes nothing by
/// path, so no user-writable folder can redirect an admin write.
mod result_pipe {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, LocalFree, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::Storage::FileSystem::{ReadFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_INBOUND};
    use windows_sys::Win32::System::Pipes::{ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT};
    use windows_sys::Win32::System::Threading::CreateEventW;
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED};

    /// Only an elevated administrator token may open it (a filtered, unelevated token carries the
    /// Administrators group as deny-only, which an allow ACE does not match).
    pub const ELEVATED_ONLY: &str = "D:P(A;;GA;;;BA)";
    const MAX_REPLY: usize = 64 * 1024;

    pub struct ResultPipe {
        pub name: String,
        pipe: HANDLE,
        event: HANDLE,
        /// Boxed: the pending connect writes to it until it completes or is cancelled.
        ov: Box<OVERLAPPED>,
        pending: bool,
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    impl ResultPipe {
        /// Creates the pipe and starts waiting for the one client.
        pub fn create(sddl: &str) -> Result<Self, String> {
            let mut id = [0u8; 16];
            getrandom::fill(&mut id).map_err(|e| format!("no random pipe name: {e}"))?;
            let name = format!("konak-elevated-{}", id.iter().map(|b| format!("{b:02x}")).collect::<String>());
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            // SAFETY: NUL-terminated SDDL; `sd` is freed with LocalFree below.
            if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(wide(sddl).as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) } == 0 {
                return Err(format!("result pipe security descriptor: error {}", unsafe { GetLastError() }));
            }
            let sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd, bInheritHandle: 0 };
            let path = wide(&format!(r"\\.\pipe\{name}"));
            // SAFETY: valid name and attributes; one instance, so nobody can add a second server.
            let pipe = unsafe {
                CreateNamedPipeW(
                    path.as_ptr(),
                    PIPE_ACCESS_INBOUND | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                    1,
                    0,
                    MAX_REPLY as u32,
                    0,
                    &sa,
                )
            };
            let err = unsafe { GetLastError() };
            unsafe { LocalFree(sd as _) };
            if pipe == INVALID_HANDLE_VALUE {
                return Err(format!("result pipe not created: error {err}"));
            }
            // SAFETY: a manual-reset event for the overlapped calls.
            let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
            if event.is_null() {
                unsafe { CloseHandle(pipe) };
                return Err("result pipe event not created".into());
            }
            let mut me = ResultPipe { name, pipe, event, ov: Box::new(unsafe { std::mem::zeroed() }), pending: false };
            me.ov.hEvent = event;
            // SAFETY: `ov` outlives the call (boxed, cancelled and waited for in Drop).
            if unsafe { ConnectNamedPipe(me.pipe, &mut *me.ov) } == 0 {
                match unsafe { GetLastError() } {
                    ERROR_IO_PENDING => me.pending = true,
                    ERROR_PIPE_CONNECTED => {}
                    e => return Err(format!("result pipe does not wait for a client: error {e}")),
                }
            }
            Ok(me)
        }

        /// After the elevated process has exited: its whole reply, or `None` when it never
        /// connected, wrote more than the cap, or the read failed.
        pub fn collect(mut self) -> Option<Vec<u8>> {
            let mut n = 0u32;
            if self.pending {
                // SAFETY: the overlapped connect started in `create`; no wait.
                if unsafe { GetOverlappedResult(self.pipe, &*self.ov, &mut n, 0) } == 0 {
                    return None;
                }
                self.pending = false;
            }
            let mut reply = vec![];
            let mut buf = vec![0u8; 4096];
            loop {
                let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
                ov.hEvent = self.event;
                // SAFETY: `buf` and `ov` live until the read completes below (or is cancelled).
                let ok = unsafe { ReadFile(self.pipe, buf.as_mut_ptr(), buf.len() as u32, std::ptr::null_mut(), &mut ov) } != 0;
                let done = ok || unsafe { GetLastError() } == ERROR_IO_PENDING;
                // The writer has exited, so the data is buffered; 5 s only guards against a stuck read.
                if !done || unsafe { GetOverlappedResultEx(self.pipe, &ov, &mut n, 5000, 0) } == 0 {
                    let e = unsafe { GetLastError() };
                    if e != ERROR_BROKEN_PIPE {
                        unsafe {
                            CancelIoEx(self.pipe, &ov);
                            GetOverlappedResult(self.pipe, &ov, &mut n, 1);
                        }
                        return None;
                    }
                    return Some(reply);
                }
                reply.extend_from_slice(&buf[..n as usize]);
                if reply.len() > MAX_REPLY {
                    return None;
                }
            }
        }
    }

    impl Drop for ResultPipe {
        fn drop(&mut self) {
            // SAFETY: cancel the pending connect and wait for it, so `ov` is no longer written to.
            unsafe {
                if self.pending {
                    let mut n = 0;
                    CancelIoEx(self.pipe, &*self.ov);
                    GetOverlappedResult(self.pipe, &*self.ov, &mut n, 1);
                }
                CloseHandle(self.pipe);
                CloseHandle(self.event);
            }
        }
    }
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

fn http_status(ip: &str, port: u16, host: &str) -> Option<u16> {
    let addr = format!("{ip}:{port}").parse().ok()?;
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
            if !distro_exists(distro)? {
                let dir = paths.distro_dir();
                let _ = std::fs::create_dir_all(&dir);
                let out = shell::run("wsl.exe", &["--install", "Debian", "--name", distro, "--location", &dir.to_string_lossy(), "--no-launch"]);
                crate::log(paths, &format!("wsl --install Debian: {}", tail(&out.text, 3)));
                if !distro_exists(distro).unwrap_or(false) {
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
            // The supervisor may serve on the fallback ports (another program holds port 80); read
            // them each try, as a later tick can switch.
            let ports = loop {
                let ports = sup.web_status().ports();
                match http_status(&ip, ports.http, &host) {
                    Some(200) => break ports,
                    other if Instant::now() > deadline => {
                        return Err(format!("{} did not answer through Caddy (last status {other:?})", ports.url(false, HOME_NAME)));
                    }
                    _ => std::thread::sleep(Duration::from_secs(2)),
                }
            };
            crate::log(paths, &format!("reachable: {} via {ip}:{}", ports.url(false, HOME_NAME), ports.http));
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
        assert_eq!(s.apps, vec![InstalledApp { id: "jellyfin".into(), name: "jellyfin-2".into(), installed_unix: 20, claimed: true, stopped: false }]);
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
        assert!(s.apps[0].claimed && !s.apps[0].stopped);
        let unclaimed = InstalledApp { id: "forgejo".into(), name: "git".into(), installed_unix: 6, claimed: false, stopped: false };
        let back: InstalledApp = serde_json::from_str(&serde_json::to_string(&unclaimed).unwrap()).unwrap();
        assert_eq!(back, unclaimed);
    }

    #[test]
    fn fresh_state_has_no_apps() {
        let s = State::from_json(r#"{"completed":["preflight"]}"#).unwrap();
        assert!(s.apps.is_empty() && s.rerender.is_empty() && !s.done);
    }

    #[test]
    fn state_from_before_the_port_fallback_was_rendered_for_80_and_443() {
        let old = r#"{"completed":["preflight","network"],"apps":[{"id":"nextcloud","name":"cloud","installed_unix":5,"claimed":true,"stopped":true}],"rerender":[],"done":true}"#;
        let s = State::from_json(old).unwrap();
        assert_eq!(s.rendered_ports, WebPorts::STANDARD);
        assert!(s.done && s.apps[0].stopped);
        let fallback = State { rendered_ports: WebPorts::FALLBACKS[0], ..s };
        let back = State::from_json(&serde_json::to_string(&fallback).unwrap()).unwrap();
        assert_eq!(back.rendered_ports, WebPorts { http: 18080, https: 18443 });
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
        let mut s = State { done: true, apps: vec![InstalledApp { id: "forgejo".into(), name: "git".into(), installed_unix: 1, claimed: true, stopped: false }], ..State::default() };
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

    fn decode_command(encoded: &str) -> String {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).unwrap();
        String::from_utf16(&bytes.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()).unwrap()
    }

    fn params(pipe: &str, bin: &std::path::Path, programs: &[std::path::PathBuf]) -> serde_json::Value {
        serde_json::json!({"enable_wsl": false, "pipe": pipe, "bin": bin, "programs": programs, "set_private_interface": 0})
    }

    #[test]
    fn the_elevated_step_gets_script_and_parameters_inline() {
        use base64::Engine;
        let bin = std::path::PathBuf::from(r"C:\Users\u\AppData\Local\Konak\bin");
        let p = params("konak-elevated-00112233445566778899aabbccddeeff", &bin, &[bin.join("caddy.exe"), bin.join("mdns-responder.exe")]);
        let encoded = elevated_command(ELEVATED_PS1, &p);
        let text = decode_command(&encoded);
        assert!(text.starts_with("& {\n") && text.contains(ELEVATED_PS1));
        let arg = text.rsplit_once("-ParamsB64 '").unwrap().1.strip_suffix('\'').unwrap();
        let back: serde_json::Value = serde_json::from_slice(&base64::engine::general_purpose::STANDARD.decode(arg).unwrap()).unwrap();
        assert_eq!(back, p);
        let launcher = elevation_launcher(&encoded);
        assert!(launcher.contains("-Verb RunAs") && launcher.contains(&format!("-EncodedCommand {encoded}'")) && !launcher.contains("-File "));
        // Both command lines stay far below Windows' 32767-character limit.
        assert!(launcher.len() < 24_000, "{}", launcher.len());
        // The script writes no file at all.
        assert!(!ELEVATED_PS1.contains("Set-Content") && !ELEVATED_PS1.contains("IO.File]") && !ELEVATED_PS1.contains("Out-File"));
    }

    #[test]
    fn exit_codes_map_to_outcomes() {
        assert_eq!(exit_flags(Some(4096)), Some(0));
        assert_eq!(exit_flags(Some(4096 + 15)), Some(15));
        for c in [Some(0), Some(1), Some(3), Some(1223), Some(4096 + 16), Some(4095), None] {
            assert_eq!(exit_flags(c), None, "{c:?}");
        }
        assert_eq!(elevated_outcome(None, Some(3)).unwrap_err(), "elevated step refused its parameters");
        assert!(elevated_outcome(Some(br#"{"reboot_required":false,"errors":[],"wsl_output":""}"#), Some(1)).unwrap_err().contains("did not finish"));
        // No pipe reply: the flags alone.
        assert_eq!(elevated_outcome(None, Some(4096)), Ok(Elevated { reboot_required: false, errors: vec![], wsl_output: String::new() }));
        let r = elevated_outcome(None, Some(4096 + 1 + 4)).unwrap();
        assert!(r.reboot_required && r.errors.len() == 1 && r.errors[0].starts_with("firewall:"), "{r:?}");
        let r = elevated_outcome(Some(b"not json"), Some(4096 + 2 + 8)).unwrap();
        assert_eq!(r.errors.iter().map(|e| e.split(':').next().unwrap()).collect::<Vec<_>>(), ["wsl", "network profile"]);
    }

    #[test]
    fn a_pipe_reply_counts_only_when_it_agrees_with_the_exit_code() {
        let ok = br#"{"reboot_required":true,"errors":["firewall: access denied"],"wsl_output":"done"}"#;
        assert_eq!(elevated_outcome(Some(ok), Some(4096 + 1 + 4)), Ok(Elevated { reboot_required: true, errors: vec!["firewall: access denied".into()], wsl_output: "done".into() }));
        // A forged "all fine" while the exit code says the firewall failed: the flags win.
        let forged = br#"{"reboot_required":false,"errors":[],"wsl_output":""}"#;
        let r = elevated_outcome(Some(forged), Some(4096 + 4)).unwrap();
        assert!(r.errors.len() == 1 && r.errors[0].contains("details lost"), "{r:?}");
        // Missing or mistyped fields, unknown error kinds: rejected.
        for bad in [&br#"{"errors":[],"wsl_output":""}"#[..], br#"{"reboot_required":"no","errors":[],"wsl_output":""}"#, br#"{"reboot_required":false,"errors":[1],"wsl_output":""}"#, br#"{"reboot_required":false,"errors":["other: x"],"wsl_output":""}"#] {
            assert_eq!(parse_reply(bad, 0), None, "{}", String::from_utf8_lossy(bad));
        }
    }

    /// Runs the real elevated script, not elevated, on inputs it must refuse before it changes
    /// anything: no firewall rule is touched, no network profile, no WSL install.
    fn run_elevated_unprivileged(p: &serde_json::Value) -> shell::Out {
        shell::run("powershell.exe", &["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand", &elevated_command(ELEVATED_PS1, p)])
    }

    /// For the tests, a pipe the unelevated test process itself may write to.
    const ANY_USER: &str = "D:P(A;;GA;;;AU)";

    #[test]
    fn the_elevated_script_reports_through_the_pipe_and_refuses_programs_outside_bin() {
        let pipe = result_pipe::ResultPipe::create(ANY_USER).unwrap();
        let bin = std::env::temp_dir().join("konak-elevated-test").join("bin");
        let out = run_elevated_unprivileged(&params(&pipe.name, &bin, &[bin.join(r"..\evil.exe"), std::path::PathBuf::from(r"C:\Windows\System32\cmd.exe")]));
        let reply = pipe.collect();
        assert_eq!(out.code, Some(4096 + 4), "{}", out.text);
        let r = elevated_outcome(reply.as_deref(), out.code).unwrap();
        assert!(!r.reboot_required && r.errors.len() == 1, "{r:?}");
        assert!(r.errors[0].starts_with("firewall: refused") && r.errors[0].contains("cmd.exe") && r.errors[0].contains("evil.exe"), "{r:?}");
    }

    #[test]
    fn the_elevated_script_refuses_a_bad_pipe_name() {
        let out = run_elevated_unprivileged(&params(r"..\..\somewhere", std::path::Path::new(r"C:\k\bin"), &[]));
        assert_eq!(out.code, Some(3), "{}", out.text);
    }

    #[test]
    fn an_unelevated_process_cannot_open_the_result_pipe() {
        let elevated = shell::run("whoami.exe", &["/groups"]).text.contains("S-1-16-12288");
        let pipe = result_pipe::ResultPipe::create(result_pipe::ELEVATED_ONLY).unwrap();
        let open = std::fs::OpenOptions::new().write(true).open(format!(r"\\.\pipe\{}", pipe.name));
        if !elevated {
            assert_eq!(open.as_ref().unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
        }
        // Nobody wrote: no reply.
        drop(open);
        if !elevated {
            assert_eq!(pipe.collect(), None);
        }
    }
    #[test]
    fn cannot_list_is_not_the_same_as_not_listed() {
        let no_registry = || -> Result<Vec<String>, String> { panic!("the registry is read only when wsl -l fails") };
        assert_eq!(distro_listed("konak", true, "Ubuntu-24.04\r\nkonak\r\n", no_registry), Ok(true));
        assert_eq!(distro_listed("konak", true, "Ubuntu-24.04\r\nkonak-auditfix\r\n", no_registry), Ok(false));
        assert_eq!(distro_listed("konak", true, "KONAK\r\n", no_registry), Ok(true), "WSL names are case-insensitive");
        // `wsl -l` failed (no distros at all, or a broken service): the registry decides.
        let failed = "Windows Subsystem for Linux has no installed distributions.";
        assert_eq!(distro_listed("konak", false, failed, || Ok(vec![])), Ok(false));
        assert_eq!(distro_listed("konak", false, failed, || Ok(vec!["konak".into()])), Ok(true));
        let e = distro_listed("konak", false, "The service is being updated", || Err("the WSL registry list could not be read ()".into())).unwrap_err();
        assert!(e.contains("cannot tell") && e.contains("being updated"), "{e}");

        assert_eq!(parse_registered(true, "name Ubuntu-24.04\r\nname konak\r\nend\r\n"), Ok(vec!["Ubuntu-24.04".to_string(), "konak".to_string()]));
        assert_eq!(parse_registered(true, "end\r\n"), Ok(vec![]), "no Lxss key: nothing registered");
        assert!(parse_registered(true, "name konak\r\n").is_err(), "cut off before the end line");
        assert!(parse_registered(false, "name konak\r\nend\r\n").is_err());
    }

    #[test]
    #[ignore = "reads this PC's WSL registry list"]
    fn reads_the_wsl_registry_list() {
        assert!(registered_distros().is_ok());
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
