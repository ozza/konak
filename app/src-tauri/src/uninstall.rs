//! `konak.exe --uninstall [--purge]`, run by the NSIS uninstaller (`windows/hooks.nsh`) before it
//! removes Konak's files. It undoes what Konak set up outside its install folder:
//! - stops a running Konak instance and the caddy / mdns-responder / wsl keepalive it started,
//!   matched by exact executable path and command line, never by name alone;
//! - stops the containers in Konak's distro, so netavark removes its rules from the shared WSL
//!   network namespace;
//! - `wsl --terminate` of Konak's distro, the autostart Run value, the root certificate in the
//!   current user's store and the `Konak` firewall rule group (one UAC prompt).
//!
//! `--purge` also unregisters the distro and deletes the Konak home. The visible Media and Photos
//! folders are never deleted. Every action goes to a log file in %TEMP%.

use crate::paths::{skip_elevation, Paths};
use crate::shell::{self, tail};
use serde::Deserialize;
use std::io::Write;
use std::path::{Path, PathBuf};

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

#[derive(Debug, PartialEq)]
pub struct Opts {
    pub purge: bool,
    /// The installer runs silently (/S): skip the steps that show a dialog or a UAC prompt, which
    /// nobody would answer, and log them as left behind.
    pub silent: bool,
}

/// `Some` when the command line asks for an uninstall.
pub fn parse(args: &[String]) -> Option<Opts> {
    let has = |flag: &str| args.iter().skip(1).any(|a| a == flag);
    has("--uninstall").then(|| Opts { purge: has("--purge"), silent: has("--silent") })
}

struct Report {
    file: Option<std::fs::File>,
    log_path: PathBuf,
    summary: Vec<String>,
    failed: bool,
}

impl Report {
    fn log(&mut self, msg: &str) {
        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(f, "{} {msg}", crate::installer::now());
        }
    }
    /// One line of the final summary (also logged).
    fn step(&mut self, name: &str, outcome: Result<String, String>) {
        let line = match outcome {
            Ok(s) => format!("{name}: {s}"),
            Err(e) => {
                self.failed = true;
                format!("{name}: FAILED: {e}")
            }
        };
        self.log(&line);
        self.summary.push(line);
    }
}

/// Lower-case, backslash-separated, no trailing separator: Windows paths compare case-insensitively.
fn norm(p: &Path) -> String {
    p.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

fn same_path(a: &Path, b: &Path) -> bool {
    norm(a) == norm(b)
}

/// `outer` is `inner` or one of its parent folders.
fn contains(outer: &Path, inner: &Path) -> bool {
    let (o, i) = (norm(outer), norm(inner));
    i == o || i.starts_with(&format!("{o}\\"))
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct Proc {
    pub process_id: u32,
    pub executable_path: Option<String>,
    pub command_line: Option<String>,
}

fn args_of(cmdline: &str) -> Vec<String> {
    cmdline.split_whitespace().map(|t| t.trim_matches('"').to_lowercase()).collect()
}

/// Processes this install owns, the Konak instances first so they cannot restart the others.
pub fn owned_processes(procs: &[Proc], me: u32, exe: &Path, paths: &Paths, wsl: &Path) -> Vec<(u32, &'static str)> {
    let caddyfile = norm(&paths.caddyfile());
    let distro = paths.distro.to_lowercase();
    let mut konak = vec![];
    let mut others = vec![];
    for p in procs.iter().filter(|p| p.process_id != me) {
        let (Some(path), Some(cmd)) = (p.executable_path.as_deref().map(Path::new), p.command_line.as_deref()) else { continue };
        let args = args_of(cmd);
        let has = |s: &str| args.iter().any(|a| a == s);
        if same_path(path, exe) {
            konak.push((p.process_id, "konak"));
        } else if same_path(path, &paths.caddy()) && has("run") && norm(Path::new(&cmd.replace('"', ""))).contains(&caddyfile) {
            others.push((p.process_id, "caddy"));
        } else if same_path(path, &paths.mdns()) && args.last().is_some_and(|names| names.split(',').any(|n| n == crate::services::HOME_NAME)) {
            others.push((p.process_id, "mdns-responder"));
        } else if same_path(path, wsl) && args.windows(2).any(|w| w[0] == "-d" && w[1] == distro) && args.ends_with(&["sleep".into(), "infinity".into()]) {
            others.push((p.process_id, "wsl keepalive"));
        }
    }
    konak.extend(others);
    konak
}

/// Stops the processes this install owns (see `owned_processes`). With `with_konak` false, only the
/// children (caddy, mdns-responder, keepalive): a starting Konak clears leftovers of an earlier run
/// that way, without touching `--mcp` servers started from the same exe.
fn stop_processes_of(paths: &Paths, exe: &Path, with_konak: bool) -> Result<String, String> {
    let out = shell::run(
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-Command", "@(Get-CimInstance Win32_Process | Select-Object ProcessId, ExecutablePath, CommandLine) | ConvertTo-Json -Compress"],
    );
    let procs: Vec<Proc> = serde_json::from_str(out.text.trim()).map_err(|e| format!("cannot list processes: {e}: {}", tail(&out.text, 2)))?;
    let wsl = PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into())).join(r"System32\wsl.exe");
    let mut targets = owned_processes(&procs, std::process::id(), exe, paths, &wsl);
    targets.retain(|(_, what)| with_konak || *what != "konak");
    if targets.is_empty() {
        return Ok("nothing running".into());
    }
    let mut stopped = vec![];
    let mut gone = vec![];
    let mut errors = vec![];
    for (pid, what) in targets {
        match kill(pid) {
            Ok(true) => stopped.push(format!("{what} ({pid})")),
            Ok(false) => gone.push(format!("{what} ({pid})")),
            Err(e) => errors.push(format!("{what} ({pid}): {e}")),
        }
    }
    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    let mut parts = vec![];
    if !stopped.is_empty() {
        parts.push(format!("stopped {}", stopped.join(", ")));
    }
    if !gone.is_empty() {
        parts.push(format!("already gone {}", gone.join(", ")));
    }
    Ok(parts.join("; "))
}

/// `Ok(true)` when taskkill stopped the process, `Ok(false)` when it was already gone. Killing
/// Konak first lets its job object take the children down, so their own taskkill then finds
/// nothing; that is success. Checked by asking whether the pid is still alive, not by parsing
/// taskkill's (localized) message.
fn kill(pid: u32) -> Result<bool, String> {
    let out = shell::run("taskkill.exe", &["/PID", &pid.to_string(), "/F"]);
    if out.ok {
        Ok(true)
    } else if crate::diag::process_image(pid).is_none() {
        Ok(false)
    } else {
        Err(tail(&out.text, 1))
    }
}

fn stop_processes(paths: &Paths, exe: &Path) -> Result<String, String> {
    stop_processes_of(paths, exe, true)
}

/// At startup, before the supervisor runs: children of an earlier Konak run of this install that
/// are still alive (from a build without the job object, or a run killed before it existed).
pub fn stop_leftovers(paths: &Paths) -> Result<String, String> {
    stop_processes_of(paths, &std::env::current_exe().map_err(|e| e.to_string())?, false)
}

/// `konak.exe --stop [<installed konak.exe>]`: stops that install's running Konak and its children,
/// and nothing else. Used by the NSIS installer before it overwrites the files of a running install;
/// it runs a copy of the new exe, so the old one never has to understand this flag.
pub fn parse_stop(args: &[String]) -> Option<Option<PathBuf>> {
    let i = args.iter().skip(1).position(|a| a == "--stop")? + 1;
    Some(args.get(i + 1).filter(|a| !a.starts_with("--")).map(PathBuf::from))
}

pub fn run_stop(target: Option<PathBuf>) -> i32 {
    let exe = target.or_else(|| std::env::current_exe().ok()).unwrap_or_default();
    let paths = Paths::for_uninstall(exe.parent().unwrap_or(Path::new(".")).to_path_buf());
    match stop_processes(&paths, &exe) {
        Ok(s) => {
            println!("Konak stop: {s}");
            0
        }
        Err(e) => {
            println!("Konak stop: FAILED: {e}");
            1
        }
    }
}

/// Konak only ever terminates or unregisters its own distros.
pub fn own_distro(name: &str) -> bool {
    name == "konak" || name.starts_with("konak-")
}

fn distro_cmd(paths: &Paths, verb: &str) -> Result<String, String> {
    let d = &paths.distro;
    if !own_distro(d) {
        return Err(format!("refusing to touch distro '{d}', it is not a Konak distro"));
    }
    if !crate::installer::distro_exists(d)? {
        return Ok(format!("distro '{d}' not found, nothing to do"));
    }
    let out = shell::run("wsl.exe", &[verb, d]);
    if out.ok {
        Ok(format!("wsl {verb} {d}: done"))
    } else {
        Err(format!("wsl {verb} {d}: {}", tail(&out.text, 2)))
    }
}

/// Run as root in Konak's distro before it is terminated. Waits for boot to finish first (a stopped
/// distro is started by this very call, and konak-boot.service then starts the containers), so no
/// container comes up after the stop.
const STOP_CONTAINERS_SH: &str = r#"set -uo pipefail
timeout 120 systemctl is-system-running --wait >/dev/null 2>&1 || true
command -v podman >/dev/null || { echo 'podman not installed, nothing to stop'; exit 0; }
err=$(mktemp)
# stdout has one line per stopped container; warnings (a container killed after the timeout) go to stderr.
if out=$(podman stop --all --time 15 2>"$err"); then echo "podman stop --all: $(printf '%s' "$out" | grep -c .) containers stopped"; else echo "podman stop --all failed: $out $(tr '\n' ' ' <"$err")"; rm -f "$err"; exit 1; fi
[ -s "$err" ] && echo "podman stop warnings: $(tr '\n' ' ' <"$err")"
rm -f "$err"
if out=$(podman network prune -f 2>&1); then echo "podman network prune: $(printf '%s' "$out" | grep -c .) networks removed"; else echo "podman network prune failed: $out"; exit 1; fi
"#;

/// Stops every container in Konak's distro so netavark removes its own port forwards, chains and
/// bridges from the WSL network namespace, which all distros share and which outlives the distro.
/// Terminating or unregistering it with containers running leaves them behind, and a dead
/// network's DNAT jump then black-holes the same port for a later install (spike finding N1).
fn stop_containers(paths: &Paths) -> Result<String, String> {
    let d = &paths.distro;
    if !own_distro(d) {
        return Err(format!("refusing to touch distro '{d}', it is not a Konak distro"));
    }
    if !crate::installer::distro_exists(d)? {
        return Ok(format!("distro '{d}' not found, nothing to do"));
    }
    let out = shell::distro_script(d, STOP_CONTAINERS_SH, &[]);
    let text = out.text.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("; ");
    if out.ok {
        Ok(text)
    } else {
        Err(text)
    }
}

/// The Run value is Konak's when it is `"<this exe>" --autostart`, as `register_autostart` writes it.
pub fn run_value_owned(reg_query: &str, exe: &Path) -> bool {
    reg_query
        .lines()
        .filter_map(|l| l.trim().strip_prefix("Konak")?.trim().strip_prefix("REG_SZ"))
        .any(|v| v.trim().strip_prefix('"').and_then(|v| v.split_once('"')).is_some_and(|(p, rest)| same_path(Path::new(p), exe) && rest.trim() == "--autostart"))
}

fn remove_run_value(exe: &Path) -> Result<String, String> {
    let out = shell::run("reg.exe", &["query", RUN_KEY, "/v", "Konak"]);
    if !out.ok {
        return Ok("no Run value".into());
    }
    if !run_value_owned(&out.text, exe) {
        return Ok(format!("kept, it points elsewhere: {}", tail(&out.text, 1).trim()));
    }
    let out = shell::run("reg.exe", &["delete", RUN_KEY, "/v", "Konak", "/f"]);
    if out.ok {
        Ok("removed".into())
    } else {
        Err(tail(&out.text, 2))
    }
}

/// Whether the root's thumbprint is in the current user's Root store (no dialog for reading).
fn root_trusted(paths: &Paths) -> Result<bool, String> {
    let cert = paths.ca_cert().to_string_lossy().replace('\'', "''");
    let script = format!("$c = [Security.Cryptography.X509Certificates.X509Certificate2]::new('{cert}'); Test-Path ('Cert:\\CurrentUser\\Root\\' + $c.Thumbprint)");
    let out = shell::run("powershell.exe", &["-NoProfile", "-NonInteractive", "-Command", &script]);
    match out.text.trim() {
        "True" => Ok(true),
        "False" => Ok(false),
        _ => Err(format!("cannot read the certificate store: {}", tail(&out.text, 2))),
    }
}

fn remove_root(paths: &Paths) -> Result<String, String> {
    if !paths.ca_cert().exists() {
        return Ok("no root certificate in the Konak home".into());
    }
    if !root_trusted(paths)? {
        return Ok("not in the CurrentUser Root store".into());
    }
    // Windows asks the user to confirm; the call returns after the answer.
    crate::ca::untrust_on_this_pc(paths)?;
    if root_trusted(paths)? {
        return Err("still trusted (removal declined?)".into());
    }
    Ok("removed from the CurrentUser Root store".into())
}

fn firewall_rule_count() -> Result<usize, String> {
    let out = shell::run("powershell.exe", &["-NoProfile", "-NonInteractive", "-Command", "@(Get-NetFirewallRule -Group Konak -ErrorAction SilentlyContinue).Count"]);
    out.text.trim().parse().map_err(|_| format!("cannot read firewall rules: {}", tail(&out.text, 2)))
}

fn remove_firewall_rules() -> Result<String, String> {
    let n = firewall_rule_count()?;
    if n == 0 {
        return Ok("no Konak rules".into());
    }
    // One UAC prompt; the elevated child only removes the Konak group.
    let cmd = "try { $p = Start-Process -FilePath powershell.exe -Verb RunAs -WindowStyle Hidden -Wait -PassThru -ArgumentList '-NoProfile -NonInteractive -Command Remove-NetFirewallRule -Group Konak'; exit $p.ExitCode } catch { exit 1223 }";
    let out = shell::run("powershell.exe", &["-NoProfile", "-NonInteractive", "-Command", cmd]);
    if out.code == Some(1223) {
        return Err("UAC prompt declined".into());
    }
    match firewall_rule_count()? {
        0 => Ok(format!("removed {n} rules")),
        left => Err(format!("{left} rules left: {}", tail(&out.text, 2))),
    }
}

/// Refuses to purge anything that is not the expected Konak home: never a drive root, never a folder
/// that holds the visible Media/Photos folders, the user profile, AppData or Temp, never a parent
/// of the install folder; and only the default home or a folder that has Konak's `state.json`.
pub fn purge_guard(home: &Path, default_home: &Path, has_state: bool, install_dir: &Path, keep: &[PathBuf]) -> Result<(), String> {
    let h = norm(home);
    if !home.is_absolute() || !h.contains('\\') || h.ends_with(':') {
        return Err(format!("{} is not a safe folder to delete", home.display()));
    }
    if let Some(k) = keep.iter().find(|k| !k.as_os_str().is_empty() && contains(home, k)) {
        return Err(format!("{} contains {}, which must be kept", home.display(), k.display()));
    }
    if !same_path(home, install_dir) && contains(home, install_dir) {
        return Err(format!("{} contains the install folder", home.display()));
    }
    if !same_path(home, default_home) && !has_state {
        return Err(format!("{} does not look like a Konak home (no state.json)", home.display()));
    }
    Ok(())
}

/// A registered distro's disk lives in the home, so the home is deleted only once the unregister
/// succeeded and WSL then certainly no longer lists the distro (`listed_after`). Any doubt skips it.
pub fn purge_allowed(unregister: &Result<String, String>, listed_after: Result<bool, String>) -> Result<(), String> {
    if unregister.is_err() {
        return Err("skipped, the distro may still be registered".into());
    }
    match listed_after {
        Ok(false) => Ok(()),
        Ok(true) => Err("skipped, the distro is still registered".into()),
        Err(e) => Err(format!("skipped, could not confirm the distro is unregistered: {e}")),
    }
}

/// Deletes the Konak home. When the home is also the install folder, the installed files
/// (this exe, the uninstaller, bundled binaries) stay for the NSIS uninstaller to remove.
fn purge_home(paths: &Paths, exe: &Path) -> Result<String, String> {
    let home = &paths.home;
    if !home.exists() {
        return Ok(format!("{} not found, nothing to do", home.display()));
    }
    let env = |k: &str| std::env::var(k).map(PathBuf::from).unwrap_or_default();
    let install_dir = exe.parent().unwrap_or(Path::new("")).to_path_buf();
    let default_home = env("LOCALAPPDATA").join("Konak");
    let keep = [paths.media.clone(), paths.photos.clone(), env("USERPROFILE"), env("LOCALAPPDATA"), env("APPDATA"), env("TEMP"), env("SystemRoot")];
    purge_guard(home, &default_home, paths.state_file().exists(), &install_dir, &keep)?;
    let installed: Vec<PathBuf> = if same_path(home, &install_dir) {
        [exe.to_path_buf(), install_dir.join("uninstall.exe"), paths.bin.clone()].into()
    } else {
        vec![]
    };
    let mut errors = vec![];
    for entry in std::fs::read_dir(home).map_err(|e| e.to_string())?.flatten() {
        let p = entry.path();
        if installed.iter().any(|i| same_path(i, &p)) {
            continue;
        }
        let r = if entry.file_type().is_ok_and(|t| t.is_dir()) { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
        if let Err(e) = r {
            errors.push(format!("{}: {e}", p.display()));
        }
    }
    if installed.is_empty() {
        if let Err(e) = std::fs::remove_dir(home) {
            errors.push(format!("{}: {e}", home.display()));
        }
    }
    if !errors.is_empty() {
        Err(errors.join("; "))
    } else if installed.is_empty() {
        Ok(format!("deleted {}", home.display()))
    } else {
        Ok(format!("deleted the data in {}, installed files left to the uninstaller", home.display()))
    }
}

/// Runs the uninstall and returns the process exit code.
pub fn run(opts: Opts) -> i32 {
    let exe = std::env::current_exe().unwrap_or_default();
    let paths = Paths::for_uninstall(exe.parent().unwrap_or(Path::new(".")).to_path_buf());
    let log_path = std::env::temp_dir().join(format!("konak-uninstall-{}.log", crate::installer::now()));
    let file = std::fs::File::create(&log_path).ok();
    let mut r = Report { file, log_path, summary: vec![], failed: false };
    r.log(&format!("uninstall: purge={} silent={} exe={} home={} distro={}", opts.purge, opts.silent, exe.display(), paths.home.display(), paths.distro));

    let o = stop_processes(&paths, &exe);
    r.step("processes", o);
    let o = stop_containers(&paths);
    r.step("containers", o);
    let o = distro_cmd(&paths, "--terminate");
    r.step("distro terminate", o);
    if skip_elevation() {
        for s in ["autostart", "root certificate", "firewall"] {
            r.step(s, Ok("skipped (KONAK_SKIP_ELEVATION=1)".into()));
        }
    } else {
        let o = remove_run_value(&exe);
        r.step("autostart", o);
        if opts.silent {
            for s in ["root certificate", "firewall"] {
                r.step(s, Ok("left behind (silent uninstall: it needs a Windows dialog or a UAC prompt)".into()));
            }
        } else {
            let o = remove_root(&paths);
            r.step("root certificate", o);
            let o = remove_firewall_rules();
            r.step("firewall", o);
        }
    }
    if opts.purge {
        let o = distro_cmd(&paths, "--unregister");
        let listed_after = if o.is_ok() { crate::installer::distro_exists(&paths.distro) } else { Ok(true) };
        let allowed = purge_allowed(&o, listed_after);
        r.step("distro unregister", o);
        let o = allowed.and_then(|()| purge_home(&paths, &exe));
        r.step("konak home", o);
    } else {
        r.step("konak home", Ok(format!("kept {}", paths.home.display())));
    }
    r.step("media and photos", Ok(format!("kept {} and {}", paths.media.display(), paths.photos.display())));

    let summary = format!("Konak uninstall {}\n{}\nlog: {}", if r.failed { "finished with errors" } else { "done" }, r.summary.join("\n"), r.log_path.display());
    r.log("done");
    println!("{summary}");
    i32::from(r.failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_uninstall_flags() {
        assert_eq!(parse(&args(&["konak.exe"])), None);
        assert_eq!(parse(&args(&["konak.exe", "--autostart"])), None);
        assert_eq!(parse(&args(&["konak.exe", "--uninstall"])), Some(Opts { purge: false, silent: false }));
        assert_eq!(parse(&args(&["konak.exe", "--uninstall", "--purge"])), Some(Opts { purge: true, silent: false }));
        assert_eq!(parse(&args(&["konak.exe", "--purge", "--uninstall"])), Some(Opts { purge: true, silent: false }));
        assert_eq!(parse(&args(&["konak.exe", "--uninstall", "--silent"])), Some(Opts { purge: false, silent: true }));
        // --purge alone does nothing; argv[0] is never a flag.
        assert_eq!(parse(&args(&["konak.exe", "--purge"])), None);
        assert_eq!(parse(&args(&["--uninstall"])), None);
    }

    #[test]
    fn parses_stop_flag() {
        assert_eq!(parse_stop(&args(&["konak.exe"])), None);
        assert_eq!(parse_stop(&args(&["konak.exe", "--uninstall"])), None);
        assert_eq!(parse_stop(&args(&["konak.exe", "--stop"])), Some(None));
        assert_eq!(parse_stop(&args(&["konak-stop.exe", "--stop", r"C:\K\konak.exe"])), Some(Some(PathBuf::from(r"C:\K\konak.exe"))));
        assert_eq!(parse_stop(&args(&["--stop"])), None);
    }

    #[test]
    fn run_value_ownership() {
        let exe = Path::new(r"C:\Users\u\AppData\Local\Konak\konak.exe");
        let q = |v: &str| format!("\r\nHKEY_CURRENT_USER\\Software\\Microsoft\\Windows\\CurrentVersion\\Run\r\n    Konak    REG_SZ    {v}\r\n\r\n");
        assert!(run_value_owned(&q(r#""C:\Users\u\AppData\Local\Konak\konak.exe" --autostart"#), exe));
        assert!(run_value_owned(&q(r#""c:\users\u\appdata\local\konak\KONAK.EXE" --autostart"#), exe));
        assert!(!run_value_owned(&q(r#""D:\dev\target\release\konak.exe" --autostart"#), exe));
        assert!(!run_value_owned(&q(r#""C:\Users\u\AppData\Local\Konak\konak.exe" --other"#), exe));
        assert!(!run_value_owned(&q(r#"C:\Users\u\AppData\Local\Konak\konak.exe --autostart"#), exe));
        assert!(!run_value_owned("ERROR: The system was unable to find the specified registry key or value.", exe));
        // Another value whose name merely starts with Konak.
        let helper = q(r#""C:\Users\u\AppData\Local\Konak\konak.exe" --autostart"#).replace("Konak    REG_SZ", "KonakHelper    REG_SZ");
        assert!(!run_value_owned(&helper, exe));
    }

    #[test]
    fn purge_guard_refuses_unsafe_folders() {
        let p = PathBuf::from;
        let default = p(r"C:\Users\u\AppData\Local\Konak");
        let install = p(r"C:\Users\u\AppData\Local\Konak");
        let keep = [p(r"C:\Users\u\Konak\Media"), p(r"C:\Users\u\Konak\Photos"), p(r"C:\Users\u"), p(r"C:\Users\u\AppData\Local"), p(r"C:\Users\u\AppData\Local\Temp")];
        let g = |home: &str, has_state: bool| purge_guard(Path::new(home), &default, has_state, &install, &keep);
        // The default home, which is also the install folder.
        assert!(g(r"C:\Users\u\AppData\Local\Konak", false).is_ok());
        assert!(g(r"c:/users/u/appdata/local/konak/", false).is_ok());
        // A dev home only with Konak's state file.
        assert!(g(r"C:\Users\u\AppData\Local\Temp\konak-dev", true).is_ok());
        assert!(g(r"C:\Users\u\AppData\Local\Temp\konak-dev", false).is_err());
        // Never a root, the profile, AppData, Temp, or anything holding the visible folders.
        for bad in [r"C:\", r"C:", r"C:\Users", r"C:\Users\u", r"C:\Users\u\Konak", r"C:\Users\u\Konak\Media", r"C:\Users\u\AppData\Local", r"C:\Users\u\AppData\Local\Temp", r"relative\konak"] {
            assert!(g(bad, true).is_err(), "{bad}");
        }
        // Never a parent of the install folder.
        let other_install = p(r"D:\Apps\Konak\bin");
        assert!(purge_guard(Path::new(r"D:\Apps\Konak"), &default, true, &other_install, &keep).is_err());
    }

    #[test]
    fn owned_processes_match_path_and_command_line() {
        let base = PathBuf::from(r"C:\Users\u\AppData\Local\Konak");
        let paths = Paths { home: base.clone(), distro: "konak".into(), bin: base.join("bin"), media: base.join("m"), photos: base.join("p") };
        let exe = base.join("konak.exe");
        let wsl = Path::new(r"C:\Windows\System32\wsl.exe");
        let proc_ = |pid, path: &str, cmd: &str| Proc { process_id: pid, executable_path: Some(path.into()), command_line: Some(cmd.into()) };
        let procs = [
            proc_(1, r"C:\Users\u\AppData\Local\Konak\konak.exe", r#""C:\Users\u\AppData\Local\Konak\konak.exe" --uninstall"#),
            proc_(2, r"C:\Users\u\AppData\Local\Konak\konak.exe", r#""C:\Users\u\AppData\Local\Konak\konak.exe" --autostart"#),
            proc_(3, r"C:\Users\u\AppData\Local\Konak\bin\caddy.exe", r#"C:\Users\u\AppData\Local\Konak\bin\caddy.exe run --config C:\Users\u\AppData\Local\Konak\Caddyfile --adapter caddyfile"#),
            proc_(4, r"C:\Users\u\AppData\Local\Konak\bin\mdns-responder.exe", r#"mdns-responder.exe 192.168.1.5 86400 konak,jellyfin"#),
            proc_(5, r"C:\Windows\System32\wsl.exe", "wsl.exe -d konak -u root -- sleep infinity"),
            // Same names, not ours: another Caddy, the spike's distro keepalive, a Konak dev build.
            proc_(6, r"C:\tools\caddy.exe", r#"caddy.exe run --config C:\Users\u\AppData\Local\Konak\Caddyfile"#),
            proc_(7, r"C:\Windows\System32\wsl.exe", "wsl.exe -d konak-podman -u root -- sleep infinity"),
            proc_(8, r"J:\dev\target\release\konak.exe", "konak.exe"),
            proc_(9, r"C:\Users\u\AppData\Local\Konak\bin\caddy.exe", r#"caddy.exe run --config D:\elsewhere\Caddyfile"#),
            proc_(10, r"C:\Windows\System32\wsl.exe", "wsl.exe -d konak"),
            Proc { process_id: 11, executable_path: None, command_line: None },
        ];
        let got = owned_processes(&procs, 1, &exe, &paths, wsl);
        assert_eq!(got, vec![(2, "konak"), (3, "caddy"), (4, "mdns-responder"), (5, "wsl keepalive")]);
    }

    #[test]
    fn killing_a_process_that_is_already_gone_succeeds() {
        // A live process this test started: stopped.
        let mut live = shell::hidden("ping.exe").args(["-n", "30", "127.0.0.1"]).spawn().unwrap();
        assert_eq!(kill(live.id()), Ok(true));
        let _ = live.wait();
        // One that has exited (as the job object leaves Konak's children): already gone, not an error.
        let mut done = shell::hidden("cmd.exe").args(["/c", "exit 0"]).spawn().unwrap();
        let pid = done.id();
        done.wait().unwrap();
        drop(done);
        assert_eq!(kill(pid), Ok(false));
    }

    #[test]
    fn the_home_is_purged_only_when_the_distro_is_certainly_gone() {
        let done: Result<String, String> = Ok("wsl --unregister konak: done".into());
        let not_found: Result<String, String> = Ok("distro 'konak' not found, nothing to do".into());
        assert_eq!(purge_allowed(&done, Ok(false)), Ok(()));
        assert_eq!(purge_allowed(&not_found, Ok(false)), Ok(()));
        assert_eq!(purge_allowed(&done, Ok(true)).unwrap_err(), "skipped, the distro is still registered");
        // WSL could not be asked (hung or updating service): never delete the disk on a guess.
        let doubt = purge_allowed(&done, Err("cannot tell".into())).unwrap_err();
        assert!(doubt.starts_with("skipped, could not confirm the distro is unregistered") && doubt.contains("cannot tell"), "{doubt}");
        assert!(purge_allowed(&Err("cannot tell whether WSL distro 'konak' is registered".into()), Ok(false)).is_err());
    }

    #[test]
    fn only_konak_distros() {
        assert!(own_distro("konak") && own_distro("konak-nonexistent"));
        assert!(!own_distro("Ubuntu-24.04") && !own_distro("docker-desktop") && !own_distro("konakx") && !own_distro(""));
        // Refused before any wsl.exe call.
        let p = |d: &str| Paths { home: PathBuf::from(r"C:\k"), distro: d.into(), bin: PathBuf::from(r"C:\k\bin"), media: PathBuf::new(), photos: PathBuf::new() };
        assert!(stop_containers(&p("Ubuntu-24.04")).unwrap_err().contains("not a Konak distro"));
    }
}
