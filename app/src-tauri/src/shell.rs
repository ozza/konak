//! Running Windows and WSL commands without flashing console windows.

use std::io::Write;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// A command that never opens a console window. `WSL_UTF8` makes wsl.exe print UTF-8 instead of UTF-16.
pub fn hidden(program: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.creation_flags(CREATE_NO_WINDOW).env("WSL_UTF8", "1");
    cmd
}

pub struct Out {
    pub ok: bool,
    pub code: Option<i32>,
    pub text: String,
}

fn decode(bytes: &[u8]) -> String {
    // Some wsl.exe builds ignore WSL_UTF8 for their own messages and print UTF-16LE.
    String::from_utf8_lossy(bytes).replace('\0', "")
}

pub fn run(program: &str, args: &[&str]) -> Out {
    match hidden(program).args(args).output() {
        Ok(o) => Out {
            ok: o.status.success(),
            code: o.status.code(),
            text: format!("{}{}", decode(&o.stdout), decode(&o.stderr)),
        },
        Err(e) => Out { ok: false, code: None, text: format!("cannot start {program}: {e}") },
    }
}

/// Runs a bash script as root inside the distro, fed through stdin. `--exec` starts bash directly:
/// with `--`, wsl.exe joins the arguments into one line for the login shell, which re-parses them.
pub fn distro_script(distro: &str, script: &str, args: &[&str]) -> Out {
    let mut full = vec!["-d", distro, "-u", "root", "--exec", "bash", "-s", "--"];
    full.extend_from_slice(args);
    let child = hidden("wsl.exe")
        .args(&full)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return Out { ok: false, code: None, text: format!("cannot start wsl.exe: {e}") },
    };
    // Scripts are authored with LF endings; strip any CR that git or an editor may have added.
    let body = script.replace("\r\n", "\n");
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(body.as_bytes());
    }
    match child.wait_with_output() {
        Ok(o) => Out {
            ok: o.status.success(),
            code: o.status.code(),
            text: format!("{}{}", decode(&o.stdout), decode(&o.stderr)),
        },
        Err(e) => Out { ok: false, code: None, text: e.to_string() },
    }
}

/// A Windows job object that kills every process in it when its last handle closes.
pub struct Job(HANDLE);

// SAFETY: a job handle may be used and closed from any thread.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

impl Job {
    pub fn new() -> std::io::Result<Job> {
        // SAFETY: plain Win32 calls; the handle is owned by `Job` and closed in Drop, and the info
        // struct is passed with its own size.
        unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if h.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let job = Job(h);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let size = std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
            if SetInformationJobObject(h, JobObjectExtendedLimitInformation, (&raw const info).cast(), size) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(job)
        }
    }

    pub fn assign(&self, child: &Child) -> std::io::Result<()> {
        // SAFETY: both handles are valid for the duration of the call.
        if unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle()) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateJobObjectW and is closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// The app's job, held until the process exits. Windows closes the handle however konak.exe ends
/// (Quit, a crash, Task Manager, the installer), which kills caddy, mdns-responder and the
/// keepalive with it, so none of them can outlive the app and keep port 80 or a file locked.
fn app_job() -> Option<&'static Job> {
    static JOB: OnceLock<Option<Job>> = OnceLock::new();
    JOB.get_or_init(|| Job::new().ok()).as_ref()
}

/// A long-running child, placed in the app's job (see `app_job`).
pub fn spawn_hidden(program: &str, args: &[&str], log: Option<&std::path::Path>) -> std::io::Result<Child> {
    let mut cmd = hidden(program);
    cmd.args(args).stdin(Stdio::null());
    match log.and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok()) {
        Some(f) => {
            let f2 = f.try_clone()?;
            cmd.stdout(f).stderr(f2);
        }
        None => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    let child = cmd.spawn()?;
    if let Some(job) = app_job() {
        // Without the job the child still runs; it just would not die with the app.
        let _ = job.assign(&child);
    }
    Ok(child)
}

/// Last few lines of command output, for error messages.
pub fn tail(text: &str, lines: usize) -> String {
    let v: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    v[v.len().saturating_sub(lines)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn children_die_when_the_job_handle_closes() {
        let job = Job::new().unwrap();
        let mut child = hidden("ping.exe").args(["-n", "30", "127.0.0.1"]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        job.assign(&child).unwrap();
        assert!(child.try_wait().unwrap().is_none(), "ping runs for about 30 s");
        drop(job);
        let deadline = Instant::now() + Duration::from_secs(5);
        let exited = loop {
            if child.try_wait().unwrap().is_some() {
                break true;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                break false;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(exited, "the child outlived its job");
    }

    /// Needs a throwaway distro: `KONAK_TEST_DISTRO=konak-<name> cargo test distro_script -- --ignored`.
    #[test]
    #[ignore]
    fn distro_script_passes_arguments_verbatim() {
        let distro = std::env::var("KONAK_TEST_DISTRO").expect("set KONAK_TEST_DISTRO");
        assert!(distro.starts_with("konak-"), "only Konak's own distros");
        let args = ["a b", "$(echo INJECTED)", "x;echo SEMI", "/p?q=1&r", "'\"", "-"];
        let out = super::distro_script(&distro, "for a in \"$@\"; do printf '[%s]\\n' \"$a\"; done\n", &args);
        let want: String = args.iter().map(|a| format!("[{a}]\n")).collect();
        assert!(out.ok, "{}", out.text);
        assert_eq!(out.text, want);
    }
}
