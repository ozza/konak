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

/// `distro_script`, but stdout is handed to `read` as it arrives (binary-safe: wsl.exe passes the
/// Linux process's stdout through unchanged), for streams too large to hold. stderr is collected
/// meanwhile. When `read` fails, the process is killed. A non-zero exit is an error carrying the
/// end of stderr, even when `read` succeeded (a stream cut short by a failing command).
pub fn distro_read<T>(distro: &str, script: &str, args: &[&str], read: impl FnOnce(&mut dyn std::io::Read) -> Result<T, String>) -> Result<T, String> {
    use std::io::Read;
    let mut full = vec!["-d", distro, "-u", "root", "--exec", "bash", "-s", "--"];
    full.extend_from_slice(args);
    let mut child = hidden("wsl.exe")
        .args(&full)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start wsl.exe: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(script.replace("\r\n", "\n").as_bytes());
    }
    let mut stderr = child.stderr.take();
    let errors = std::thread::spawn(move || {
        let mut buf = vec![];
        if let Some(e) = stderr.as_mut() {
            let _ = e.read_to_end(&mut buf);
        }
        decode(&buf)
    });
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let result = read(&mut stdout);
    if result.is_err() {
        let _ = child.kill();
    }
    drop(stdout);
    let status = child.wait().map_err(|e| e.to_string())?;
    let err_text = errors.join().unwrap_or_default();
    let value = result?;
    if !status.success() {
        return Err(format!("exit {:?}: {}", status.code(), tail(&err_text, 4)));
    }
    Ok(value)
}

/// Runs a base64-encoded script given as `$1`, with the arguments after it as `$1...`, so the
/// script's stdin stays free for data.
const RUN_B64: &str = r#"s=$(printf %s "$1" | base64 -d) || exit 2; shift; eval "$s""#;

/// A bash script run as root inside the distro whose stdin is fed by the caller through `Write`
/// (binary-safe), for streams too large to hold: the counterpart of `distro_read`. The script travels
/// as an argument, so it must not read stdin for anything but the data. stdout and stderr are
/// collected meanwhile. End it with `finish` (closes stdin, waits, fails on a non-zero exit) or
/// `abort` (kills it).
pub struct DistroWriter {
    child: Child,
    stdin: Option<std::process::ChildStdin>,
    output: Vec<std::thread::JoinHandle<String>>,
}

impl DistroWriter {
    pub fn start(distro: &str, script: &str, args: &[&str]) -> Result<Self, String> {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(script.replace("\r\n", "\n"));
        let mut full = vec!["-d", distro, "-u", "root", "--exec", "bash", "-c", RUN_B64, "konak", &b64];
        full.extend_from_slice(args);
        let mut child = hidden("wsl.exe")
            .args(&full)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("cannot start wsl.exe: {e}"))?;
        let collect = |mut r: Box<dyn std::io::Read + Send>| {
            std::thread::spawn(move || {
                let mut buf = vec![];
                let _ = r.read_to_end(&mut buf);
                decode(&buf)
            })
        };
        let mut output = vec![];
        if let Some(o) = child.stdout.take() {
            output.push(collect(Box::new(o)));
        }
        if let Some(e) = child.stderr.take() {
            output.push(collect(Box::new(e)));
        }
        Ok(DistroWriter { stdin: child.stdin.take(), child, output })
    }

    fn wait(&mut self) -> (Option<std::process::ExitStatus>, String) {
        self.stdin = None;
        let status = self.child.wait().ok();
        let text = self.output.drain(..).map(|h| h.join().unwrap_or_default()).collect::<Vec<_>>().join("");
        (status, text)
    }

    /// Closes stdin and waits; the script's output, or an error with its last lines.
    pub fn finish(mut self) -> Result<String, String> {
        match self.wait() {
            (Some(s), text) if s.success() => Ok(text),
            (s, text) => Err(format!("exit {:?}: {}", s.and_then(|s| s.code()), tail(&text, 4))),
        }
    }

    /// Kills the script and returns the end of what it printed (why a write may have failed).
    pub fn abort(mut self) -> String {
        let _ = self.child.kill();
        tail(&self.wait().1, 4)
    }
}

impl Drop for DistroWriter {
    /// Dropped without `finish` or `abort` (an early return, a panic): the script is killed, so it
    /// never treats a cut stream as complete.
    fn drop(&mut self) {
        if !self.output.is_empty() {
            let _ = self.child.kill();
            self.wait();
        }
    }
}

impl std::io::Write for DistroWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stdin.as_mut().ok_or_else(|| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.stdin.as_mut().map_or(Ok(()), |s| s.flush())
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
